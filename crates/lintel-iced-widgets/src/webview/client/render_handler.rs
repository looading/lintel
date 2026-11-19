use crate::webview::{
  BrowserId,
  instance::{DragData, webview_states},
  widget::wake_render_waker,
};
use arc_swap::ArcSwapOption;
use cef::{rc::*, sys, *};
use lintel_lib_iced::iced_wgpu::wgpu;
use lintel_lib_iced::iced_wgpu::window::compositor::gpu_context;
use lintel_lib_iced::{self as iced, window::ScreenInfo};
use std::{
  cell::{OnceCell, Ref, RefCell},
  collections::HashSet,
  fmt::Debug,
  ops::Deref,
  ptr::null_mut,
  sync::{LazyLock, Mutex},
  time::{Duration, Instant},
};
use tokio::sync::mpsc::{Receiver, Sender};
#[cfg(target_os = "windows")]
use windows::Win32::{
  Foundation::{HWND, POINT, RECT},
  Graphics::Gdi::ClientToScreen,
  UI::WindowsAndMessaging::GetWindowRect,
};

static GPU_RENDER_LOGGED_BROWSERS: LazyLock<Mutex<HashSet<BrowserId>>> = LazyLock::new(Default::default);
fn log_gpu_render_once(browser_id: BrowserId) {
  if let Ok(mut logged_browsers) = GPU_RENDER_LOGGED_BROWSERS.lock()
    && logged_browsers.insert(browser_id)
  {
    tracing::info!(?browser_id, "render with gpu");
  }
}

fn record_cef_frame_arrival(state: &IcyRenderState, browser_id: BrowserId) {
  if !lintel_lib_iced::ui_perf::is_enabled() {
    return;
  }

  let now = Instant::now();
  let mut inner = state.inner.borrow_mut();
  if let Some(last_arrival_at) = inner.last_frame_arrival_at.replace(now) {
    let elapsed = now.saturating_duration_since(last_arrival_at);
    lintel_lib_iced::ui_perf::record_cef_frame_arrival_for_browser(browser_id.inner(), elapsed);
    if crate::webview::widget::has_render_waker(browser_id) {
      lintel_lib_iced::ui_perf::record_foreground_cef_frame_arrival_for_browser(browser_id.inner(), elapsed);
    }
  }
}

/// 除独立 frame pump 外，仅对首批被过滤帧做有限次快速补绘。
fn request_resize_retry(state: &IcyRenderState, browser_id: BrowserId) {
  match state.plan_resize_retry(Instant::now()) {
    ResizeRetryPlan::Immediate => send_resize_retry(browser_id),
    ResizeRetryPlan::Schedule { delay, generation } => {
      let state = state.clone();
      let scheduled_state = state.clone();
      let mut task = crate::webview::task::IcyTask::build(move || {
        if scheduled_state.complete_scheduled_resize_retry(generation, Instant::now()) {
          send_resize_retry(browser_id);
        }
      });
      let delay_ms = delay.as_millis().max(1) as i64;
      let posted = cef::post_delayed_task(cef::sys::cef_thread_id_t::TID_UI.into(), Some(&mut task), delay_ms);
      if posted <= 0 && state.complete_scheduled_resize_retry(generation, Instant::now()) {
        send_resize_retry(browser_id);
      }
    }
    ResizeRetryPlan::Coalesced | ResizeRetryPlan::Suppressed => {}
  }
}

fn send_resize_retry(browser_id: BrowserId) {
  if let Some(browser) = cef::browser_host_get_browser_by_identifier(browser_id.inner())
    && let Some(host) = browser.host()
  {
    host.invalidate(cef::PaintElementType::VIEW.into());
    tracing::trace!(?browser_id, "webview resize retry requested after rejected paint");
  }
}

const RESIZE_RETRY_MIN_INTERVAL: Duration = Duration::from_millis(50);
const RESIZE_RETRY_BURST_LIMIT: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResizeRetryPlan {
  Immediate,
  Schedule { delay: Duration, generation: u64 },
  Coalesced,
  Suppressed,
}

/// 判断 CEF dirty rect 的并集是否覆盖指定区域。
fn dirty_rects_cover_area(rects: Option<&[cef::Rect]>, target: &cef::Rect) -> (usize, bool) {
  let Some(rects) = rects else {
    return (0, false);
  };
  if rects.is_empty() {
    return (0, false);
  }
  if target.width <= 0 || target.height <= 0 {
    return (rects.len(), false);
  }

  let target_left = target.x;
  let target_top = target.y;
  let target_right = target.x.saturating_add(target.width);
  let target_bottom = target.y.saturating_add(target.height);
  if rects.iter().any(|rect| {
    rect.x <= target_left
      && rect.y <= target_top
      && rect.x.saturating_add(rect.width) >= target_right
      && rect.y.saturating_add(rect.height) >= target_bottom
  }) {
    return (rects.len(), true);
  }

  let clipped_rects = rects
    .iter()
    .filter_map(|rect| {
      let left = rect.x.clamp(target_left, target_right);
      let top = rect.y.clamp(target_top, target_bottom);
      let right = rect.x.saturating_add(rect.width).clamp(target_left, target_right);
      let bottom = rect.y.saturating_add(rect.height).clamp(target_top, target_bottom);
      (right > left && bottom > top).then_some((left, top, right, bottom))
    })
    .collect::<Vec<_>>();
  if clipped_rects.is_empty() {
    return (rects.len(), false);
  }

  let mut x_edges = Vec::with_capacity(clipped_rects.len() * 2 + 2);
  x_edges.extend([target_left, target_right]);
  for &(left, _, right, _) in &clipped_rects {
    x_edges.extend([left, right]);
  }
  x_edges.sort_unstable();
  x_edges.dedup();

  let mut covered_area = 0_i64;
  for x_pair in x_edges.windows(2) {
    let (x_start, x_end) = (x_pair[0], x_pair[1]);
    if x_end <= x_start {
      continue;
    }
    let mut y_intervals = clipped_rects
      .iter()
      .filter(|&&(left, _, right, _)| left < x_end && right > x_start)
      .map(|&(_, top, _, bottom)| (top, bottom))
      .collect::<Vec<_>>();
    y_intervals.sort_unstable();

    let mut covered_height = 0_i64;
    let mut merged: Option<(i32, i32)> = None;
    for (top, bottom) in y_intervals {
      match merged {
        Some((merged_top, merged_bottom)) if top <= merged_bottom => {
          merged = Some((merged_top, merged_bottom.max(bottom)));
        }
        Some((merged_top, merged_bottom)) => {
          covered_height += i64::from(merged_bottom - merged_top);
          merged = Some((top, bottom));
        }
        None => merged = Some((top, bottom)),
      }
    }
    if let Some((top, bottom)) = merged {
      covered_height += i64::from(bottom - top);
    }
    covered_area += i64::from(x_end - x_start) * covered_height;
  }

  (
    rects.len(),
    covered_area >= i64::from(target.width) * i64::from(target.height),
  )
}

/// 判断 CEF dirty rect 的并集是否覆盖整张 view。
fn dirty_rects_cover_frame(rects: Option<&[cef::Rect]>, width: i32, height: i32) -> (usize, bool) {
  dirty_rects_cover_area(
    rects,
    &cef::Rect {
      x: 0,
      y: 0,
      width,
      height,
    },
  )
}

/// Cached GPU resources that are created once and reused across frames.
struct CachedGpuResources {
  sampler: wgpu::Sampler,
  bind_group_layout: wgpu::BindGroupLayout,
  full_texture_region_buffer: wgpu::Buffer,
}

/// Cached texture for on_paint rendering path.
/// Reused when dimensions haven't changed.
struct CachedPaintTexture {
  texture: wgpu::Texture,
  width: u32,
  height: u32,
  format: wgpu::TextureFormat,
}

#[derive(Clone)]
pub struct IcyRenderHandler {
  state: IcyRenderState,
  drag_tx: Sender<(BrowserId, DragData)>,
  ime_composition_tx: Sender<(BrowserId, f32, f32)>,
}

#[derive(Clone)]
enum CefViewContent {
  Hardware {
    bind_group: wgpu::BindGroup,
    dirty_rect: Option<iced::Rectangle>,
    capture_counter: u64,
    swap_rb: bool,
    is_popup: bool,
  },
  Software {
    handle: iced::widget::image::Handle,
    allocation: Option<anyhow::Result<iced::widget::image::Allocation, ()>>,
  },
}

#[derive(Clone, Debug)]
pub struct CefViewWrapper {
  view: std::sync::Arc<CefView>,
  resize_enabled: bool,
}

impl CefViewWrapper {
  pub fn new(view: std::sync::Arc<CefView>) -> Self {
    Self {
      view,
      resize_enabled: true,
    }
  }

  pub fn with_resize(view: std::sync::Arc<CefView>, resize_enabled: bool) -> Self {
    Self { view, resize_enabled }
  }

  fn should_resize(&self) -> bool {
    self.resize_enabled && self.view.should_resize
  }
}

impl Deref for CefViewWrapper {
  type Target = CefView;
  fn deref(&self) -> &Self::Target {
    &self.view
  }
}

#[derive(Clone)]
pub struct CefView {
  pub(crate) browser_id: BrowserId,
  pub(crate) bounds: Option<iced::Rectangle>,
  content_size: iced::Size,
  display_size: iced::Size,
  pub(crate) popup_offset: Option<iced::Vector>,
  pub(crate) timestamp: std::time::Instant,
  pub(crate) should_resize: bool,
  content: CefViewContent,
}

impl std::fmt::Debug for CefView {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("CefView")
      .field("browser_id", &self.browser_id)
      .field("mode", &self.mode())
      .field("capture_counter", &self.capture_counter())
      .finish()
  }
}

impl CefView {
  pub fn new_hardware(
    browser_id: BrowserId,
    bind_group: wgpu::BindGroup,
    dirty_rect: Option<iced::Rectangle>,
    bounds: Option<iced::Rectangle>,
    content_size: iced::Size,
    display_size: iced::Size,
    popup_offset: Option<iced::Vector>,
    should_resize: bool,
    capture_counter: u64,
    swap_rb: bool,
    is_popup: bool,
  ) -> Self {
    Self {
      browser_id,
      bounds,
      content_size,
      display_size,
      popup_offset,
      timestamp: std::time::Instant::now(),
      should_resize,
      content: CefViewContent::Hardware {
        bind_group,
        dirty_rect,
        capture_counter,
        swap_rb,
        is_popup,
      },
    }
  }

  pub fn new_software(
    browser_id: BrowserId,
    handle: iced::widget::image::Handle,
    content_size: iced::Size,
    display_size: iced::Size,
  ) -> Self {
    Self {
      browser_id,
      bounds: None,
      content_size,
      display_size,
      popup_offset: None,
      timestamp: std::time::Instant::now(),
      should_resize: false,
      content: CefViewContent::Software {
        handle,
        allocation: None,
      },
    }
  }

  pub fn software_handle(&self) -> Option<iced::widget::image::Handle> {
    match &self.content {
      CefViewContent::Software { handle, .. } => Some(handle.clone()),
      CefViewContent::Hardware { .. } => None,
    }
  }

  pub fn display_size(&self) -> iced::Size {
    self.display_size
  }

  pub fn content_size(&self) -> iced::Size {
    self.content_size
  }

  /// 当前帧是否已按目标尺寸完成绘制。
  pub fn matches_content_size(&self, target_size: iced::Size, device_scale_factor: f32) -> bool {
    matches_frame_dimension(self.content_size.width, target_size.width, device_scale_factor)
      && matches_frame_dimension(self.content_size.height, target_size.height, device_scale_factor)
  }

  pub fn software_allocation(&self) -> Option<anyhow::Result<iced::widget::image::Allocation, ()>> {
    match &self.content {
      CefViewContent::Software { allocation, .. } => allocation.clone(),
      CefViewContent::Hardware { .. } => None,
    }
  }

  pub fn with_software_allocation(
    &self,
    allocation: anyhow::Result<iced::widget::image::Allocation, ()>,
  ) -> Option<Self> {
    match &self.content {
      CefViewContent::Software { handle, .. } => Some(Self {
        browser_id: self.browser_id,
        bounds: self.bounds,
        content_size: self.content_size,
        display_size: self.display_size,
        popup_offset: self.popup_offset,
        timestamp: self.timestamp,
        should_resize: self.should_resize,
        content: CefViewContent::Software {
          handle: handle.clone(),
          allocation: Some(allocation),
        },
      }),
      CefViewContent::Hardware { .. } => None,
    }
  }

  fn bind_group(&self) -> Option<&wgpu::BindGroup> {
    match &self.content {
      CefViewContent::Hardware { bind_group, .. } => Some(bind_group),
      CefViewContent::Software { .. } => None,
    }
  }

  fn dirty_rect(&self) -> Option<iced::Rectangle> {
    match &self.content {
      CefViewContent::Hardware { dirty_rect, .. } => dirty_rect.clone(),
      CefViewContent::Software { .. } => None,
    }
  }

  fn capture_counter(&self) -> Option<u64> {
    match &self.content {
      CefViewContent::Hardware { capture_counter, .. } => Some(*capture_counter),
      CefViewContent::Software { .. } => None,
    }
  }

  fn mode(&self) -> &'static str {
    match &self.content {
      CefViewContent::Hardware { .. } => "hardware",
      CefViewContent::Software { .. } => "software",
    }
  }

  fn swap_rb(&self) -> bool {
    match &self.content {
      CefViewContent::Hardware { swap_rb, .. } => *swap_rb,
      CefViewContent::Software { .. } => false,
    }
  }

  fn is_popup(&self) -> bool {
    match &self.content {
      CefViewContent::Hardware { is_popup, .. } => *is_popup,
      CefViewContent::Software { .. } => false,
    }
  }
}

fn matches_frame_dimension(frame_size: f32, target_size: f32, device_scale_factor: f32) -> bool {
  if !frame_size.is_finite() || !target_size.is_finite() || frame_size <= 0.0 || target_size <= 0.0 {
    return false;
  }

  let frame_pixel = frame_size.round();
  if (frame_size - frame_pixel).abs() > f32::EPSILON {
    return false;
  }

  let matches_discrete_rounding = |expected: f32| {
    let lower = expected.floor();
    let upper = expected.ceil();
    frame_pixel == lower || frame_pixel == upper
  };
  let scale_factor = if device_scale_factor.is_finite() && device_scale_factor > 0.0 {
    device_scale_factor
  } else {
    1.0
  };

  matches_discrete_rounding(target_size) || matches_discrete_rounding(target_size * scale_factor)
}

fn view_rect_size(rect: cef::Rect) -> iced::Size {
  iced::Size::new(rect.width.max(1) as f32, rect.height.max(1) as f32)
}

fn accelerated_frame_metadata_is_complete(
  coded_size: &cef::Size,
  visible_rect: &cef::Rect,
  content_rect: &cef::Rect,
) -> bool {
  if coded_size.width <= 0 || coded_size.height <= 0 {
    return false;
  }

  let rect_is_inside_coded_size = |rect: &cef::Rect| {
    rect.x >= 0
      && rect.y >= 0
      && rect.width > 0
      && rect.height > 0
      && rect.x.saturating_add(rect.width) <= coded_size.width
      && rect.y.saturating_add(rect.height) <= coded_size.height
  };
  let content_covers_visible = content_rect.x <= visible_rect.x
    && content_rect.y <= visible_rect.y
    && content_rect.x.saturating_add(content_rect.width) >= visible_rect.x.saturating_add(visible_rect.width)
    && content_rect.y.saturating_add(content_rect.height) >= visible_rect.y.saturating_add(visible_rect.height);

  rect_is_inside_coded_size(visible_rect) && rect_is_inside_coded_size(content_rect) && content_covers_visible
}

fn normalized_texture_region(coded_size: &cef::Size, visible_rect: &cef::Rect) -> [f32; 4] {
  if coded_size.width <= 0 || coded_size.height <= 0 {
    return [0.0, 0.0, 1.0, 1.0];
  }

  let coded_width = coded_size.width as f32;
  let coded_height = coded_size.height as f32;
  [
    visible_rect.x as f32 / coded_width,
    visible_rect.y as f32 / coded_height,
    visible_rect.width as f32 / coded_width,
    visible_rect.height as f32 / coded_height,
  ]
}

#[cfg(test)]
mod tests {
  use std::{sync::Arc, time::Duration};

  use arc_swap::ArcSwapOption;

  use super::{
    CefView, IcyRenderHandler, ResizeRetryPlan, accelerated_frame_metadata_is_complete, dirty_rects_cover_frame,
    matches_frame_dimension, normalized_texture_region,
  };
  use crate::webview::BrowserId;
  use lintel_lib_iced::iced;

  #[test]
  fn frame_size_match_requires_exact_logical_pixel_size_at_scale_one() {
    assert!(matches_frame_dimension(625.0, 625.0, 1.0));
    assert!(!matches_frame_dimension(624.0, 625.0, 1.0));
    assert!(!matches_frame_dimension(626.0, 625.0, 1.0));
  }

  #[test]
  fn frame_size_match_accepts_only_discrete_fractional_scale_rounding() {
    assert!(matches_frame_dimension(1406.0, 625.0, 2.25));
    assert!(matches_frame_dimension(1407.0, 625.0, 2.25));
    assert!(!matches_frame_dimension(1408.0, 625.0, 2.25));
    assert!(matches_frame_dimension(1125.0, 500.0, 2.25));
    assert!(!matches_frame_dimension(1127.0, 500.0, 2.25));
  }

  #[test]
  fn accelerated_resize_frame_requires_fully_populated_metadata() {
    let coded_size = cef::Size {
      width: 2504,
      height: 1264,
    };
    let full_rect = cef::Rect {
      x: 0,
      y: 0,
      width: coded_size.width,
      height: coded_size.height,
    };

    assert!(accelerated_frame_metadata_is_complete(
      &coded_size,
      &full_rect,
      &full_rect,
    ));

    let padded_coded_size = cef::Size {
      width: 2512,
      height: 1272,
    };
    let padded_visible_rect = cef::Rect {
      x: 4,
      y: 4,
      width: coded_size.width,
      height: coded_size.height,
    };
    let padded_content_rect = cef::Rect {
      x: 0,
      y: 0,
      width: padded_coded_size.width,
      height: padded_coded_size.height,
    };
    assert!(
      accelerated_frame_metadata_is_complete(&padded_coded_size, &padded_visible_rect, &padded_content_rect,),
      "valid coded padding must be cropped instead of leaving resize permanently pending"
    );
    assert_eq!(
      normalized_texture_region(&padded_coded_size, &padded_visible_rect),
      [4.0 / 2512.0, 4.0 / 1272.0, 2504.0 / 2512.0, 1264.0 / 1272.0]
    );

    let transitional_content_rect = cef::Rect {
      x: 0,
      y: 2,
      width: coded_size.width,
      height: coded_size.height - 5,
    };
    assert!(
      !accelerated_frame_metadata_is_complete(&coded_size, &full_rect, &transitional_content_rect,),
      "a resize transition that has not populated the complete visible region must not replace the cached frame"
    );
  }

  #[test]
  fn cef_view_exposes_original_iced_display_size_for_drawing() {
    let content_size = iced::Size::new(1250.0, 1000.0);
    let display_size = iced::Size::new(625.0, 500.0);

    let view = CefView::new_software(
      BrowserId::new(42),
      iced::widget::image::Handle::from_rgba(1, 1, vec![0, 0, 0, 0]),
      content_size,
      display_size,
    );

    assert_eq!(view.display_size(), display_size);
  }

  #[test]
  fn pending_resize_waits_for_matching_frame_commit_before_clearing_target() {
    let (_handler, state, _drag_rx, _ime_rx) = IcyRenderHandler::new(
      1.0,
      cef::Rect {
        x: 0,
        y: 0,
        width: 880,
        height: 596,
      },
      None,
      Arc::new(ArcSwapOption::const_empty()),
      Arc::new(ArcSwapOption::const_empty()),
      None,
    );
    let browser_id = BrowserId::new(7);
    let pending_size = iced::Size::new(880.0, 596.0);

    state.set_pending_resize_target(pending_size);

    assert!(!state.should_store_resize_paint_frame(
      browser_id,
      "test",
      false,
      iced::Size::new(2.0, 2.0),
      pending_size,
      0,
      true,
      true,
    ));
    assert!(state.has_pending_resize_target(pending_size));

    assert!(
      state.should_store_resize_paint_frame(browser_id, "test", false, pending_size, pending_size, 0, true, true),
      "the first matching, fully populated frame should replace the stretched cached frame immediately"
    );
    assert!(state.has_pending_resize_target(pending_size));

    state.clear_pending_resize_target();
    assert!(!state.has_pending_resize_target(pending_size));
  }

  #[test]
  fn non_pending_mismatched_frame_is_rejected_against_current_view_size() {
    let (_handler, state, _drag_rx, _ime_rx) = IcyRenderHandler::new(
      1.0,
      cef::Rect {
        x: 0,
        y: 0,
        width: 880,
        height: 596,
      },
      None,
      Arc::new(ArcSwapOption::const_empty()),
      Arc::new(ArcSwapOption::const_empty()),
      None,
    );
    let browser_id = BrowserId::new(8);
    let current_size = iced::Size::new(880.0, 596.0);

    assert!(!state.should_store_resize_paint_frame(
      browser_id,
      "test",
      false,
      iced::Size::new(2.0, 2.0),
      current_size,
      0,
      true,
      true,
    ));
    assert!(state.should_store_resize_paint_frame(
      browser_id,
      "test",
      false,
      current_size,
      current_size,
      0,
      true,
      true,
    ));
  }

  #[test]
  fn matching_resize_frame_with_partial_dirty_rects_stays_on_cached_frame() {
    let (_handler, state, _drag_rx, _ime_rx) = IcyRenderHandler::new(
      1.0,
      cef::Rect {
        x: 0,
        y: 0,
        width: 880,
        height: 596,
      },
      None,
      Arc::new(ArcSwapOption::const_empty()),
      Arc::new(ArcSwapOption::const_empty()),
      None,
    );
    let browser_id = BrowserId::new(9);
    let target_size = iced::Size::new(880.0, 596.0);
    state.set_pending_resize_target(target_size);

    assert!(!state.should_store_resize_paint_frame(
      browser_id,
      "test",
      false,
      target_size,
      target_size,
      1,
      false,
      true,
    ));
    assert!(state.has_pending_resize_target(target_size));
    assert!(!state.should_store_resize_paint_frame(
      browser_id,
      "test",
      false,
      target_size,
      target_size,
      1,
      true,
      false,
    ));
    assert!(state.has_pending_resize_target(target_size));
    assert!(state.should_store_resize_paint_frame(browser_id, "test", false, target_size, target_size, 1, true, true,));
    assert!(state.has_pending_resize_target(target_size));
  }

  #[test]
  fn clearing_an_already_cleared_resize_target_keeps_paint_log_deduplication() {
    let (_handler, state, _drag_rx, _ime_rx) = IcyRenderHandler::new(
      1.0,
      cef::Rect {
        x: 0,
        y: 0,
        width: 880,
        height: 596,
      },
      None,
      Arc::new(ArcSwapOption::const_empty()),
      Arc::new(ArcSwapOption::const_empty()),
      None,
    );
    let browser_id = BrowserId::new(10);
    let target_size = iced::Size::new(880.0, 596.0);
    state.set_pending_resize_target(target_size);
    assert!(!state.should_store_resize_paint_frame(
      browser_id,
      "test",
      false,
      target_size,
      target_size,
      1,
      true,
      false,
    ));
    assert!(state.should_store_resize_paint_frame(browser_id, "test", false, target_size, target_size, 1, true, true,));
    state.clear_pending_resize_target();
    assert!(
      state.should_store_resize_paint_frame(browser_id, "test", false, target_size, target_size, 1, false, true,)
    );

    let log_key = state.inner.borrow().last_resize_paint_log_key;
    state.clear_pending_resize_target();

    assert_eq!(state.inner.borrow().last_resize_paint_log_key, log_key);
  }

  #[test]
  fn resize_retry_coalescing_keeps_a_trailing_request() {
    let (_handler, state, _drag_rx, _ime_rx) = IcyRenderHandler::new(
      1.0,
      cef::Rect {
        x: 0,
        y: 0,
        width: 880,
        height: 596,
      },
      None,
      Arc::new(ArcSwapOption::const_empty()),
      Arc::new(ArcSwapOption::const_empty()),
      None,
    );
    let now = std::time::Instant::now();

    assert_eq!(state.plan_resize_retry(now), ResizeRetryPlan::Immediate);
    let ResizeRetryPlan::Schedule { delay, generation } = state.plan_resize_retry(now + Duration::from_millis(20))
    else {
      panic!("a rejected frame inside the interval must schedule a trailing retry");
    };
    assert_eq!(delay, Duration::from_millis(30));
    assert_eq!(
      state.plan_resize_retry(now + Duration::from_millis(30)),
      ResizeRetryPlan::Coalesced
    );
    assert!(state.complete_scheduled_resize_retry(generation, now + Duration::from_millis(50)));
    assert!(!state.complete_scheduled_resize_retry(generation, now + Duration::from_millis(51)));
    assert_eq!(
      state.plan_resize_retry(now + Duration::from_millis(100)),
      ResizeRetryPlan::Suppressed
    );

    state.set_pending_resize_target(iced::Size::new(900.0, 600.0));
    assert_eq!(
      state.plan_resize_retry(now + Duration::from_millis(101)),
      ResizeRetryPlan::Immediate
    );
  }

  #[test]
  fn dirty_rect_summary_requires_full_frame_coverage() {
    assert_eq!(dirty_rects_cover_frame(None, 100, 80), (0, false));
    assert_eq!(
      dirty_rects_cover_frame(
        Some(&[cef::Rect {
          x: 0,
          y: 0,
          width: 40,
          height: 80,
        }]),
        100,
        80,
      ),
      (1, false)
    );
    assert_eq!(
      dirty_rects_cover_frame(
        Some(&[
          cef::Rect {
            x: 0,
            y: 0,
            width: 50,
            height: 80,
          },
          cef::Rect {
            x: 50,
            y: 0,
            width: 50,
            height: 80,
          },
        ]),
        100,
        80,
      ),
      (2, true)
    );
    assert_eq!(
      dirty_rects_cover_frame(
        Some(&[
          cef::Rect {
            x: 0,
            y: 0,
            width: 50,
            height: 40,
          },
          cef::Rect {
            x: 50,
            y: 40,
            width: 50,
            height: 40,
          },
        ]),
        100,
        80,
      ),
      (2, false),
      "disjoint corner rects must not be mistaken for full-frame coverage"
    );
  }
}

fn software_paint_buffer_len(width: i32, height: i32) -> Option<usize> {
  let (width, height) = (u32::try_from(width).ok()?, u32::try_from(height).ok()?);
  if width == 0 || height == 0 {
    return None;
  }
  // The existing upload uses a u32 bytes_per_row, and Rust slices cannot exceed
  // isize::MAX. Validate both before constructing the callback's borrowed slice.
  let row = width.checked_mul(4)?;
  let length = usize::try_from(row).ok()?.checked_mul(usize::try_from(height).ok()?)?;
  (length <= isize::MAX as usize).then_some(length)
}

fn convert_bgra_to_rgba(src: &[u8], dst: &mut Vec<u8>) {
  debug_assert_eq!(src.len() % 4, 0);

  dst.resize(src.len(), 0);

  let pixels = src.len() / 4;
  let src_ptr = src.as_ptr();
  let dst_ptr = dst.as_mut_ptr();

  for index in 0..pixels {
    // SAFETY: Each iteration reads/writes exactly one complete four-byte pixel
    // within the source slice and equally sized destination Vec. The buffers
    // are separate borrows; unaligned accesses impose no extra alignment rule.
    unsafe {
      let bgra = u32::from_le(std::ptr::read_unaligned(src_ptr.add(index * 4).cast::<u32>()));
      let rgba = (bgra & 0xFF00_FF00) | ((bgra & 0x00FF_0000) >> 16) | ((bgra & 0x0000_00FF) << 16);

      std::ptr::write_unaligned(dst_ptr.add(index * 4).cast::<u32>(), rgba.to_le());
    }
  }
}

#[cfg(test)]
mod cpu_paint_tests {
  use super::*;

  #[test]
  fn cpu_paint_lengths_reject_invalid_dimensions_and_upload_stride_overflow() {
    assert_eq!(software_paint_buffer_len(1, 1), Some(4));
    assert_eq!(software_paint_buffer_len(3, 2), Some(24));
    for (width, height) in [(0, 1), (1, 0), (-1, 1), (1, -1), (i32::MAX, 1), (i32::MAX, i32::MAX)] {
      assert_eq!(software_paint_buffer_len(width, height), None);
    }
  }

  #[test]
  fn cpu_bgra_conversion_preserves_red_blue_channels_alpha_and_owned_snapshot_size() {
    let source = [255, 0, 0, 255, 0, 0, 255, 128, 0, 255, 0, 64, 1, 2, 3, 0];
    let mut rgba = vec![0xee; 100];
    convert_bgra_to_rgba(&source, &mut rgba);
    assert_eq!(rgba, [0, 0, 255, 255, 255, 0, 0, 128, 0, 255, 0, 64, 3, 2, 1, 0]);
    convert_bgra_to_rgba(&source[..4], &mut rgba);
    assert_eq!(rgba, [0, 0, 255, 255]);
  }

  #[test]
  fn cpu_software_popup_stays_separate_from_main_view_through_hide_and_resize() {
    use std::sync::Arc;
    let (_, state, _, _) = IcyRenderHandler::new(
      1.0,
      cef::Rect {
        x: 0,
        y: 0,
        width: 800,
        height: 600,
      },
      None,
      Arc::new(ArcSwapOption::const_empty()),
      Arc::new(ArcSwapOption::const_empty()),
      None,
    );
    let browser = BrowserId::new(50);
    let handle = || iced::widget::image::Handle::from_rgba(1, 1, vec![255, 0, 0, 255]);
    assert!(state.store_software_paint(browser, handle(), iced::Size::new(800.0, 600.0), false));
    let main = state.hw_view.load_full().unwrap();
    assert_eq!(main.bounds, None);
    assert_eq!(main.display_size(), iced::Size::new(800.0, 600.0));

    state.set_popup_rect(Some(iced::Rectangle::new(
      iced::Point::new(760.0, 570.0),
      iced::Size::new(100.0, 60.0),
    )));
    assert!(state.store_software_paint(browser, handle(), iced::Size::new(200.0, 120.0), true));
    let popup = state.popup_view.load_full().unwrap();
    assert!(Arc::ptr_eq(&state.hw_view.load_full().unwrap(), &main));
    assert_eq!(
      popup.bounds.unwrap(),
      iced::Rectangle {
        x: 700.0,
        y: 540.0,
        width: 100.0,
        height: 60.0
      }
    );
    assert_eq!(popup.display_size(), iced::Size::new(100.0, 60.0));
    assert_eq!(popup.content_size(), iced::Size::new(200.0, 120.0));
    assert_eq!(popup.popup_offset, Some(iced::Vector::new(60.0, 30.0)));
    assert!(popup.software_handle().is_some());

    state.set_view_rect(cef::Rect {
      x: 0,
      y: 0,
      width: 1000,
      height: 700,
    });
    let resized = iced::Size::new(1000.0, 700.0);
    state.set_pending_resize_target(resized);
    assert!(state.store_software_paint(browser, handle(), resized, false));
    assert!(!state.has_pending_resize_target(resized));
    assert_eq!(state.hw_view.load_full().unwrap().display_size(), resized);
    assert!(Arc::ptr_eq(&state.popup_view.load_full().unwrap(), &popup));

    let resized_main = state.hw_view.load_full().unwrap();
    state.clear_popup_view();
    assert!(state.popup_view.load_full().is_none());
    assert!(!state.store_software_paint(browser, handle(), iced::Size::new(200.0, 120.0), true));
    assert!(state.popup_view.load_full().is_none());
    assert!(Arc::ptr_eq(&state.hw_view.load_full().unwrap(), &resized_main));
  }
}

impl IcyRenderHandler {
  pub fn new(
    device_scale_factor: f32,
    view_rect: cef::Rect,
    screen: Option<iced::window::ScreenInfo>,
    hw_view: std::sync::Arc<ArcSwapOption<CefView>>,
    popup_view: std::sync::Arc<ArcSwapOption<CefView>>,
    native_window: Option<isize>,
  ) -> (
    Self,
    IcyRenderState,
    Receiver<(BrowserId, DragData)>,
    Receiver<(BrowserId, f32, f32)>,
  ) {
    let (drag_tx, drag_rx) = tokio::sync::mpsc::channel(4);
    let (ime_composition_tx, ime_composition_rx) = tokio::sync::mpsc::channel(10);
    let state = IcyRenderState {
      inner: std::rc::Rc::new(RefCell::new(RenderStateInner {
        pixels: Vec::new(),
        view_rect,
        screen,
        view_origin: iced::Point::ORIGIN,
        native_window,
        composition_position: (0.0, 0.0),
        cached_paint_texture: None,
        popup_rect: None,
        popup_original_rect: None,
        last_frame_arrival_at: None,
        pending_resize: None,
        last_resize_paint_log_key: None,
        last_resize_retry_at: None,
        resize_retry_attempts: 0,
        scheduled_resize_retry_generation: None,
        next_resize_retry_generation: 0,
      })),
      hw_view,
      popup_view,
      cached_gpu: std::rc::Rc::new(OnceCell::new()),
    };
    (
      Self {
        state: state.clone(),
        drag_tx,
        ime_composition_tx,
      },
      state,
      drag_rx,
      ime_composition_rx,
    )
  }
}

pub struct RenderHandlerBuilder {
  object: *mut RcImpl<sys::cef_render_handler_t, Self>,
  handler: IcyRenderHandler,
}

/// Mutable value fields of `IcyRenderState`, grouped under a single `RefCell`.
struct RenderStateInner {
  pixels: Vec<u8>,
  view_rect: cef::Rect,
  screen: Option<iced::window::ScreenInfo>,
  view_origin: iced::Point,
  native_window: Option<isize>,
  composition_position: (f32, f32),
  cached_paint_texture: Option<CachedPaintTexture>,
  popup_rect: Option<iced::Rectangle>,
  popup_original_rect: Option<iced::Rectangle>,
  last_frame_arrival_at: Option<Instant>,
  pending_resize: Option<PendingResizeDiagnostic>,
  last_resize_paint_log_key: Option<ResizePaintLogKey>,
  last_resize_retry_at: Option<Instant>,
  resize_retry_attempts: usize,
  scheduled_resize_retry_generation: Option<u64>,
  next_resize_retry_generation: u64,
}

#[derive(Clone, Copy, Debug)]
struct PendingResizeDiagnostic {
  size: iced::Size,
  updated_at: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ResizePaintLogKey {
  mode: &'static str,
  pending_size: iced::Size,
  content_size: iced::Size,
  display_size: iced::Size,
  matches_pending_size: bool,
  has_pending_resize: bool,
  full_repaint: bool,
  frame_metadata_complete: bool,
}

#[derive(Clone)]
pub struct IcyRenderState {
  inner: std::rc::Rc<RefCell<RenderStateInner>>,
  pub(crate) hw_view: std::sync::Arc<ArcSwapOption<CefView>>,
  pub(crate) popup_view: std::sync::Arc<ArcSwapOption<CefView>>,
  /// Lazily-initialized GPU resources shared across paint callbacks.
  cached_gpu: std::rc::Rc<OnceCell<CachedGpuResources>>,
}

impl Debug for IcyRenderState {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let inner = self.inner.borrow();
    f.debug_struct("RenderState")
      .field("pixels", &inner.pixels.len())
      .finish()
  }
}

impl IcyRenderState {
  pub fn view_rect(&self) -> cef::Rect {
    self.inner.borrow().view_rect.clone()
  }

  pub fn screen(&self) -> Option<ScreenInfo> {
    self.inner.borrow().screen.clone()
  }

  pub fn device_scale_factor(&self) -> f32 {
    self
      .inner
      .borrow()
      .screen
      .as_ref()
      .map(|screen| screen.device_scale_factor as f32)
      .unwrap_or(1.0)
  }

  pub fn view_origin(&self) -> iced::Point {
    self.inner.borrow().view_origin
  }

  pub fn native_window(&self) -> Option<isize> {
    self.inner.borrow().native_window
  }

  pub fn popup_rect(&self) -> Option<iced::Rectangle> {
    self.inner.borrow().popup_rect
  }

  pub fn popup_offset(&self) -> Option<iced::Vector> {
    let inner = self.inner.borrow();
    match (inner.popup_rect, inner.popup_original_rect) {
      (Some(rect), Some(original_rect)) => Some(iced::Vector::new(original_rect.x - rect.x, original_rect.y - rect.y)),
      _ => None,
    }
  }

  pub fn set_device_scale_factor(&self, device_scale_factor: f32) {
    if let Some(screen) = self.inner.borrow_mut().screen.as_mut() {
      screen.device_scale_factor = device_scale_factor as _;
    } else {
      self.inner.borrow_mut().screen = Some(ScreenInfo {
        size: (1, 1).into(),
        depth: 1.into(),
        device_scale_factor: device_scale_factor as _,
        refresh_rate: 30.into(),
      });
    }
  }

  pub fn set_view_rect(&self, view_rect: cef::Rect) {
    if view_rect.width > 0 && view_rect.height > 0 {
      self.inner.borrow_mut().view_rect = view_rect;
    }
  }

  pub fn set_pending_resize_target(&self, size: iced::Size) {
    let mut inner = self.inner.borrow_mut();
    if inner.pending_resize.is_none_or(|pending| pending.size != size) {
      inner.pending_resize = Some(PendingResizeDiagnostic {
        size,
        updated_at: Instant::now(),
      });
      inner.last_resize_paint_log_key = None;
      inner.last_resize_retry_at = None;
      inner.resize_retry_attempts = 0;
      inner.scheduled_resize_retry_generation = None;
      inner.next_resize_retry_generation = inner.next_resize_retry_generation.wrapping_add(1);
    }
  }

  pub fn has_pending_resize_target(&self, size: iced::Size) -> bool {
    self
      .inner
      .borrow()
      .pending_resize
      .is_some_and(|pending| pending.size == size)
  }

  fn has_pending_resize(&self) -> bool {
    self.inner.borrow().pending_resize.is_some()
  }

  pub fn clear_pending_resize_target(&self) {
    let mut inner = self.inner.borrow_mut();
    if inner.pending_resize.take().is_some() {
      inner.last_resize_paint_log_key = None;
    }
    inner.last_resize_retry_at = None;
    inner.resize_retry_attempts = 0;
    inner.scheduled_resize_retry_generation = None;
    inner.next_resize_retry_generation = inner.next_resize_retry_generation.wrapping_add(1);
  }

  fn plan_resize_retry(&self, now: Instant) -> ResizeRetryPlan {
    let mut inner = self.inner.borrow_mut();
    if inner.scheduled_resize_retry_generation.is_some() {
      return ResizeRetryPlan::Coalesced;
    }
    if inner.resize_retry_attempts >= RESIZE_RETRY_BURST_LIMIT {
      return ResizeRetryPlan::Suppressed;
    }

    let elapsed = inner
      .last_resize_retry_at
      .map(|last_retry_at| now.saturating_duration_since(last_retry_at));
    if elapsed.is_none_or(|elapsed| elapsed >= RESIZE_RETRY_MIN_INTERVAL) {
      inner.last_resize_retry_at = Some(now);
      inner.resize_retry_attempts += 1;
      inner.scheduled_resize_retry_generation = None;
      inner.next_resize_retry_generation = inner.next_resize_retry_generation.wrapping_add(1);
      return ResizeRetryPlan::Immediate;
    }

    inner.next_resize_retry_generation = inner.next_resize_retry_generation.wrapping_add(1);
    let generation = inner.next_resize_retry_generation;
    inner.resize_retry_attempts += 1;
    inner.scheduled_resize_retry_generation = Some(generation);
    ResizeRetryPlan::Schedule {
      delay: RESIZE_RETRY_MIN_INTERVAL.saturating_sub(elapsed.unwrap_or_default()),
      generation,
    }
  }

  fn complete_scheduled_resize_retry(&self, generation: u64, now: Instant) -> bool {
    let mut inner = self.inner.borrow_mut();
    if inner.scheduled_resize_retry_generation != Some(generation) {
      return false;
    }
    inner.scheduled_resize_retry_generation = None;
    inner.last_resize_retry_at = Some(now);
    true
  }

  fn valid_resize_size(size: iced::Size) -> bool {
    size.width.is_finite() && size.height.is_finite() && size.width > 0.0 && size.height > 0.0
  }

  fn should_store_resize_paint_frame(
    &self,
    browser_id: BrowserId,
    mode: &'static str,
    is_popup: bool,
    content_size: iced::Size,
    display_size: iced::Size,
    dirty_rect_count: usize,
    full_repaint: bool,
    frame_metadata_complete: bool,
  ) -> bool {
    if is_popup {
      return true;
    }

    let mut inner = self.inner.borrow_mut();
    let pending_resize_snapshot = inner.pending_resize;
    let has_pending_resize = pending_resize_snapshot.is_some();
    if pending_resize_snapshot.is_none() && !Self::valid_resize_size(display_size) {
      return true;
    }
    let target_size = pending_resize_snapshot
      .map(|pending| pending.size)
      .unwrap_or(display_size);
    let device_scale_factor = inner
      .screen
      .as_ref()
      .map(|screen| screen.device_scale_factor as f32)
      .unwrap_or(1.0);
    let matches_pending_size = matches_frame_dimension(content_size.width, target_size.width, device_scale_factor)
      && matches_frame_dimension(content_size.height, target_size.height, device_scale_factor);
    let trusted_frame = matches_pending_size && frame_metadata_complete && (!has_pending_resize || full_repaint);
    let log_key = ResizePaintLogKey {
      mode,
      pending_size: target_size,
      content_size,
      display_size,
      matches_pending_size,
      has_pending_resize,
      full_repaint,
      frame_metadata_complete,
    };
    if inner.last_resize_paint_log_key == Some(log_key) {
      return trusted_frame;
    }
    inner.last_resize_paint_log_key = Some(log_key);
    let elapsed_ms_since_pending_resize = pending_resize_snapshot
      .map(|pending| pending.updated_at.elapsed().as_millis())
      .unwrap_or_default();
    drop(inner);

    if matches_pending_size {
      tracing::debug!(
        ?browser_id,
        mode,
        pending_size = ?target_size,
        ?content_size,
        ?display_size,
        elapsed_ms_since_pending_resize,
        device_scale_factor,
        has_pending_resize,
        dirty_rect_count,
        full_repaint,
        frame_metadata_complete,
        matches_pending_size,
        trusted_frame,
        "webview cef paint frame for pending resize"
      );
    } else {
      tracing::warn!(
        ?browser_id,
        mode,
        pending_size = ?target_size,
        ?content_size,
        ?display_size,
        elapsed_ms_since_pending_resize,
        device_scale_factor,
        has_pending_resize,
        dirty_rect_count,
        full_repaint,
        frame_metadata_complete,
        matches_pending_size,
        trusted_frame,
        "webview cef paint frame for pending resize"
      );
    }

    trusted_frame
  }

  pub fn set_view_origin(&self, view_origin: iced::Point) {
    self.inner.borrow_mut().view_origin = view_origin;
  }

  pub fn set_popup_rect(&self, popup_rect: Option<iced::Rectangle>) {
    let mut inner = self.inner.borrow_mut();
    inner.popup_original_rect = popup_rect;
    inner.popup_rect = popup_rect.map(|mut popup_rect| {
      let view_width = inner.view_rect.width.max(0) as f32;
      let view_height = inner.view_rect.height.max(0) as f32;

      if popup_rect.x < 0.0 {
        popup_rect.x = 0.0;
      }
      if popup_rect.y < 0.0 {
        popup_rect.y = 0.0;
      }
      if popup_rect.x + popup_rect.width > view_width {
        popup_rect.x = view_width - popup_rect.width;
      }
      if popup_rect.y + popup_rect.height > view_height {
        popup_rect.y = view_height - popup_rect.height;
      }
      if popup_rect.x < 0.0 {
        popup_rect.x = 0.0;
      }
      if popup_rect.y < 0.0 {
        popup_rect.y = 0.0;
      }
      popup_rect
    });
  }

  pub fn clear_popup_view(&self) {
    let mut inner = self.inner.borrow_mut();
    inner.popup_rect = None;
    inner.popup_original_rect = None;
    self.popup_view.store(None);
  }

  fn store_software_paint(
    &self,
    browser_id: BrowserId,
    handle: iced::widget::image::Handle,
    content_size: iced::Size,
    is_popup: bool,
  ) -> bool {
    let bounds = if is_popup {
      // OnPopupShow(false) clears the rectangle. A late paint must not
      // resurrect its old popup or replace the main browser frame.
      let Some(bounds) = self.popup_rect().filter(|rect| rect.width > 0.0 && rect.height > 0.0) else {
        return false;
      };
      Some(bounds)
    } else {
      None
    };
    let display_size = bounds.map_or_else(|| view_rect_size(self.view_rect()), |rect| rect.size());
    let mut view = CefView::new_software(browser_id, handle, content_size, display_size);
    view.bounds = bounds;
    view.popup_offset = if is_popup { self.popup_offset() } else { None };
    let view = std::sync::Arc::new(view);
    if is_popup {
      self.popup_view.store(Some(view));
    } else {
      self.hw_view.store(Some(view));
      self.clear_pending_resize_target();
    }
    true
  }

  #[allow(dead_code)]
  pub fn pixels(&self) -> Ref<'_, Vec<u8>> {
    Ref::map(self.inner.borrow(), |inner| &inner.pixels)
  }

  /// Get or lazily create the shared sampler + bind_group_layout.
  fn get_or_init_gpu(&self, device: &wgpu::Device) -> &CachedGpuResources {
    self.cached_gpu.get_or_init(|| {
      use wgpu::util::DeviceExt as _;

      let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
      });

      let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        entries: &[
          wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
              multisampled: false,
              view_dimension: wgpu::TextureViewDimension::D2,
              sample_type: wgpu::TextureSampleType::Float { filterable: true },
            },
            count: None,
          },
          wgpu::BindGroupLayoutEntry {
            binding: 1,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
          },
          wgpu::BindGroupLayoutEntry {
            binding: 2,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
              ty: wgpu::BufferBindingType::Uniform,
              has_dynamic_offset: false,
              min_binding_size: None,
            },
            count: None,
          },
        ],
        label: Some("cef_bind_group_layout"),
      });
      let full_texture_region_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("CEF Full Texture Region Uniform"),
        contents: bytemuck::cast_slice(&[[0.0_f32, 0.0, 1.0, 1.0]]),
        usage: wgpu::BufferUsages::UNIFORM,
      });

      CachedGpuResources {
        sampler,
        bind_group_layout,
        full_texture_region_buffer,
      }
    })
  }

  fn get_or_create_paint_texture(
    &self,
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
  ) -> Ref<'_, CachedPaintTexture> {
    {
      let inner = self.inner.borrow();
      if let Some(ref texture) = inner.cached_paint_texture {
        if texture.width == width && texture.height == height && texture.format == format {
          return Ref::map(inner, |inner| inner.cached_paint_texture.as_ref().unwrap());
        }
      }
    }

    let texture = device.create_texture(&wgpu::TextureDescriptor {
      size: wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
      },
      mip_level_count: 1,
      sample_count: 1,
      dimension: wgpu::TextureDimension::D2,
      format,
      usage: wgpu::TextureUsages::TEXTURE_BINDING
        | wgpu::TextureUsages::COPY_DST
        | wgpu::TextureUsages::RENDER_ATTACHMENT,
      label: Some("cef_paint_texture"),
      view_formats: &[],
    });

    self.inner.borrow_mut().cached_paint_texture = Some(CachedPaintTexture {
      texture,
      width,
      height,
      format,
    });
    Ref::map(self.inner.borrow(), |inner| {
      inner.cached_paint_texture.as_ref().unwrap()
    })
  }
}

impl RenderHandlerBuilder {
  pub fn build(handler: IcyRenderHandler) -> RenderHandler {
    RenderHandler::new(Self {
      object: null_mut(),
      handler,
    })
  }
}

impl Rc for RenderHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}
impl WrapRenderHandler for RenderHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_render_handler_t, Self>) {
    self.object = object;
  }
}
impl Clone for RenderHandlerBuilder {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc_impl = &mut *self.object;
      rc_impl.interface.add_ref();
      rc_impl
    };

    Self {
      object,
      handler: self.handler.clone(),
    }
  }
}

impl ImplRenderHandler for RenderHandlerBuilder {
  fn get_raw(&self) -> *mut sys::_cef_render_handler_t {
    self.object.cast()
  }

  fn root_screen_rect(&self, browser: Option<&mut Browser>, rect: Option<&mut Rect>) -> ::std::os::raw::c_int {
    false as _
  }

  fn view_rect(&self, browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
    let Some(rect) = rect else {
      return;
    };
    if let Some(browser_id) = browser.map(|b| b.identifier().into())
      && webview_states().with_borrow(|states| {
        if let Some(state) = states.get(&browser_id) {
          let size = state.render.view_rect();
          *rect = Rect {
            width: if size.width > 0 { size.width } else { 1 },
            height: if size.height > 0 { size.height } else { 1 },
            x: size.x,
            y: size.y,
          };
          return true;
        }
        false
      })
    {
      return;
    }

    *rect = Rect {
      width: 1,
      height: 1,
      ..Default::default()
    };
  }

  fn screen_point(
    &self,
    browser: Option<&mut Browser>,
    view_x: ::std::os::raw::c_int,
    view_y: ::std::os::raw::c_int,
    screen_x: Option<&mut ::std::os::raw::c_int>,
    screen_y: Option<&mut ::std::os::raw::c_int>,
  ) -> ::std::os::raw::c_int {
    let (Some(screen_x), Some(screen_y)) = (screen_x, screen_y) else {
      return false as _;
    };
    #[cfg(target_os = "windows")]
    {
      if let Some(browser_id) = browser.map(|b| b.identifier().into()) {
        return webview_states().with_borrow(|states| {
          if let Some(state) = states.get(&browser_id)
            && let Some(hwnd) = state.render.native_window()
          {
            let scale_factor = state.render.device_scale_factor().max(1.0);
            let origin = state.render.view_origin();
            let mut point = POINT {
              x: ((origin.x + view_x as f32) * scale_factor).round() as i32,
              y: ((origin.y + view_y as f32) * scale_factor).round() as i32,
            };
            unsafe {
              _ = ClientToScreen(HWND(hwnd as *mut _), &mut point);
            }
            *screen_x = point.x;
            *screen_y = point.y;
            return true;
          }
          false
        }) as _;
      }
    }
    false as _
  }

  fn screen_info(
    &self,
    browser: Option<&mut Browser>,
    screen_info: Option<&mut cef::ScreenInfo>,
  ) -> ::std::os::raw::c_int {
    if let Some(screen_info) = screen_info
      && let Some(browser_id) = browser.map(|b| b.identifier().into())
    {
      return webview_states().with_borrow(|states| {
        if let Some(state) = states.get(&browser_id)
          && let Some(screen) = state.render.screen()
        {
          screen_info.device_scale_factor = screen.device_scale_factor as _;
          screen_info.rect.x = 0;
          screen_info.rect.y = 0;
          screen_info.rect.width = screen.size.width as _;
          screen_info.rect.height = screen.size.height as _;
          screen_info.available_rect.x = 0;
          screen_info.available_rect.y = 0;
          screen_info.available_rect.width = screen.size.width as _;
          screen_info.available_rect.height = screen.size.height as _;
          screen_info.depth = screen.depth.unwrap_or(0) as _;
          return true;
        }
        false
      }) as _;
    }
    false as _
  }

  fn start_dragging(
    &self,
    browser: Option<&mut Browser>,
    drag_data: Option<&mut cef::DragData>,
    allowed_ops: DragOperationsMask,
    x: ::std::os::raw::c_int,
    y: ::std::os::raw::c_int,
  ) -> ::std::os::raw::c_int {
    let Some(browser) = browser else {
      return false as _;
    };
    let Some(_) = browser.host() else {
      return false as _;
    };
    let Some(drag_data) = drag_data else {
      return false as _;
    };

    let browser_id = browser.identifier();
    let Some(drag_data) = <_ as ImplDragData>::clone(drag_data) else {
      return false as _;
    };
    tracing::debug!(browser_id, x, y, "request dragging at",);

    _ = self
      .handler
      .drag_tx
      .try_send((
        browser_id.into(),
        DragData {
          data: drag_data,
          operation: allowed_ops,
          x: x as _,
          y: y as _,
        },
      ))
      .inspect_err(|err| {
        tracing::warn!(?err, "cannot send drag data");
      });

    true as _
  }

  fn update_drag_cursor(&self, browser: Option<&mut Browser>, operation: DragOperationsMask) {
    //_ = self.handler.drag_tx.send(operation);
  }

  fn on_popup_show(&self, browser: Option<&mut Browser>, show: ::std::os::raw::c_int) {
    let Some(browser) = browser else {
      return;
    };

    if show != 0 {
      return;
    }
    self.handler.state.clear_popup_view();
  }

  fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
    self.handler.state.set_popup_rect(rect.map(|rect| {
      iced::Rectangle::new(
        iced::Point::new(rect.x as f32, rect.y as f32),
        iced::Size::new(rect.width.max(0) as f32, rect.height.max(0) as f32),
      )
    }));
  }

  fn on_ime_composition_range_changed(
    &self,
    browser: Option<&mut Browser>,
    selected_range: Option<&Range>,
    character_bounds: Option<&[Rect]>,
  ) {
    let Some(browser) = browser else {
      return;
    };
    let Some(bounds) = character_bounds else {
      return;
    };
    let (prev_x, prev_y) = self.handler.state.inner.borrow().composition_position;
    if let Some(bounds) = bounds.last() {
      let x = bounds.x;
      let y = bounds.y;
      self.handler.state.inner.borrow_mut().composition_position = (x as f32, y as f32);
      // input line changed
      if prev_y != y as f32 {
        _ = self
          .handler
          .ime_composition_tx
          .try_send((browser.identifier().into(), x as f32, y as f32));
      }
    }
  }

  #[cfg(target_os = "macos")]
  fn on_accelerated_paint(
    &self,
    browser: Option<&mut Browser>,
    type_: PaintElementType,
    dirty_rects: Option<&[Rect]>,
    info: Option<&AcceleratedPaintInfo>,
  ) {
    let paint_start = lintel_lib_iced::ui_perf::is_enabled().then(Instant::now);
    let Some(browser) = browser else {
      return;
    };
    let Some(_) = browser.host() else {
      return;
    };

    let Some(info) = info else {
      return;
    };

    let browser_id = browser.identifier().into();
    record_cef_frame_arrival(&self.handler.state, browser_id);
    let interaction_frame_start = lintel_lib_iced::ui_perf::is_webview_interaction_active(browser_id.inner())
      .then_some(paint_start)
      .flatten();
    let is_view = type_ == cef::sys::cef_paint_element_type_t::PET_VIEW.into();
    let is_popup = type_ == cef::sys::cef_paint_element_type_t::PET_POPUP.into();
    if !is_view && !is_popup {
      tracing::error!(?browser_id, ?type_, "unsupported paint element type");
      debug_assert!(false, "unsupported paint element type");
      return;
    }
    let frame_metadata_complete = accelerated_frame_metadata_is_complete(
      &info.extra.coded_size,
      &info.extra.visible_rect,
      &info.extra.content_rect,
    );
    let sampled_rect = if frame_metadata_complete {
      info.extra.visible_rect.clone()
    } else {
      cef::Rect {
        x: 0,
        y: 0,
        width: info.extra.coded_size.width,
        height: info.extra.coded_size.height,
      }
    };
    let dirty_rect_count = dirty_rects.map_or(0, <[cef::Rect]>::len);
    let full_repaint = if is_view && self.handler.state.has_pending_resize() {
      let covers_coded_coordinates = dirty_rects_cover_area(dirty_rects, &sampled_rect).1;
      let covers_visible_coordinates = (sampled_rect.x != 0 || sampled_rect.y != 0)
        && dirty_rects_cover_frame(dirty_rects, sampled_rect.width, sampled_rect.height).1;
      covers_coded_coordinates || covers_visible_coordinates
    } else {
      true
    };
    let dirty_rects = dirty_rects.and_then(|rects| {
      rects.first().map(|rect| {
        iced::Rectangle::new(
          iced::Point::new(rect.x as f32, rect.y as f32),
          iced::Size::new(rect.width as f32, rect.height as f32),
        )
      })
    });

    let bounds = if is_popup {
      Some(self.handler.state.popup_rect().unwrap_or_else(|| {
        iced::Rectangle::new(
          iced::Point::ORIGIN,
          iced::Size::new(sampled_rect.width as f32, sampled_rect.height as f32),
        )
      }))
    } else {
      None
    };
    let display_size = bounds
      .map(|bounds| bounds.size())
      .unwrap_or_else(|| view_rect_size(self.handler.state.view_rect()));
    let content_size = iced::Size::new(sampled_rect.width as f32, sampled_rect.height as f32);
    if is_view && self.handler.state.has_pending_resize_target(display_size) {
      tracing::trace!(
        ?browser_id,
        coded_width = info.extra.coded_size.width,
        coded_height = info.extra.coded_size.height,
        visible_x = info.extra.visible_rect.x,
        visible_y = info.extra.visible_rect.y,
        visible_width = info.extra.visible_rect.width,
        visible_height = info.extra.visible_rect.height,
        content_x = info.extra.content_rect.x,
        content_y = info.extra.content_rect.y,
        content_width = info.extra.content_rect.width,
        content_height = info.extra.content_rect.height,
        source_width = info.extra.source_size.width,
        source_height = info.extra.source_size.height,
        has_source_size = info.extra.has_source_size,
        frame_metadata_complete,
        capture_counter = info.extra.capture_counter,
        "webview resize accelerated frame metadata"
      );
    }
    let should_store_paint_frame = self.handler.state.should_store_resize_paint_frame(
      browser_id,
      "accelerated",
      is_popup,
      content_size,
      display_size,
      dirty_rect_count,
      full_repaint,
      frame_metadata_complete,
    );
    if !should_store_paint_frame {
      request_resize_retry(&self.handler.state, browser_id);
      wake_render_waker(browser_id, is_view.then_some(interaction_frame_start).flatten());
      if let Some(paint_start) = paint_start {
        lintel_lib_iced::ui_perf::record_webview_paint_for_browser(
          browser_id.inner(),
          "accelerated_paint_rejected",
          paint_start.elapsed(),
        );
      }
      return;
    }

    let Some(context) = gpu_context() else {
      tracing::error!(?browser_id, "no active iced compositor GPU context");
      return;
    };
    let (device, queue) = (&context.device, &context.queue);

    let src_texture = {
      use cef::osr_texture_import::shared_texture_handle::SharedTextureHandle;

      if info.shared_texture_io_surface.is_null()
        || info.extra.coded_size.width <= 0
        || info.extra.coded_size.height <= 0
        || !matches!(
          *info.format.as_ref(),
          cef::sys::cef_color_type_t::CEF_COLOR_TYPE_BGRA_8888 | cef::sys::cef_color_type_t::CEF_COLOR_TYPE_RGBA_8888
        )
      {
        tracing::warn!(?browser_id, "invalid CEF IOSurface handle or format");
        return;
      }

      match SharedTextureHandle::new(info).import_texture(device) {
        Ok(texture) if texture.usage().contains(wgpu::TextureUsages::COPY_SRC) => texture,
        Ok(_) => {
          // Stock CEF returns an empty fallback texture without COPY_SRC if Metal import fails.
          tracing::warn!(?browser_id, "CEF IOSurface import returned an empty fallback texture");
          return;
        }
        Err(err) => {
          tracing::error!(?browser_id, ?err, "cannot import texture");
          return;
        }
      }
    };
    let texture_desc = wgpu::TextureDescriptor {
      label: Some("Cef Texture"),
      size: wgpu::Extent3d {
        width: info.extra.coded_size.width as _,
        height: info.extra.coded_size.height as _,
        depth_or_array_layers: 1,
      },
      mip_level_count: 1,
      sample_count: 1,
      dimension: wgpu::TextureDimension::D2,
      format: wgpu::TextureFormat::Bgra8Unorm,
      usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
      view_formats: &[],
    };
    // CEF can recycle the shared texture when this callback returns. Finish the blit
    // here and publish only the destination, retaining the imported texture until the wait completes.
    let cached_dst = self.handler.state.get_or_create_paint_texture(
      device,
      texture_desc.size.width,
      texture_desc.size.height,
      texture_desc.format,
    );
    let dst_texture = &cached_dst.texture;
    let dst_view = dst_texture.create_view(&wgpu::TextureViewDescriptor {
      label: Some("CEF Copy Texture View"),
      ..Default::default()
    });
    {
      let mut copy = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("CEF Copy Texture Command"),
      });

      // Use a blit-based copy so the source texture only needs TEXTURE_BINDING usage.
      wgpu::util::TextureBlitter::new(device, texture_desc.format).copy(
        device,
        &mut copy,
        &src_texture.create_view(&Default::default()),
        &dst_view,
      );

      let cmd = copy.finish();
      let wait = lintel_lib_iced::ui_perf::measure(
        || {
          let index = queue.submit(Some(cmd));
          device.poll(wgpu::wgt::PollType::Wait {
            submission_index: index.into(),
            timeout: None,
          })
        },
        |elapsed| lintel_lib_iced::ui_perf::record_webview_gpu_poll_for_browser(browser_id.inner(), elapsed),
      );
      if let Err(err) = wait {
        tracing::error!(?browser_id, ?err, "cannot wait for CEF IOSurface blit");
        return;
      }
    }
    let cached = self.handler.state.get_or_init_gpu(device);
    let texture_region = normalized_texture_region(&info.extra.coded_size, &sampled_rect);
    let texture_region_buffer = (texture_region != [0.0, 0.0, 1.0, 1.0]).then(|| {
      use wgpu::util::DeviceExt as _;

      device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("CEF Cropped Texture Region Uniform"),
        contents: bytemuck::cast_slice(&[texture_region]),
        usage: wgpu::BufferUsages::UNIFORM,
      })
    });
    let texture_region_buffer = texture_region_buffer
      .as_ref()
      .unwrap_or(&cached.full_texture_region_buffer);

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
      label: Some("Cef Texture Bind Group"),
      layout: &cached.bind_group_layout,
      entries: &[
        wgpu::BindGroupEntry {
          binding: 0,
          resource: wgpu::BindingResource::TextureView(&dst_view),
        },
        wgpu::BindGroupEntry {
          binding: 1,
          resource: wgpu::BindingResource::Sampler(&cached.sampler),
        },
        wgpu::BindGroupEntry {
          binding: 2,
          resource: texture_region_buffer.as_entire_binding(),
        },
      ],
    });

    // 诊断日志会更新 RenderStateInner，先释放缓存纹理的 RefCell 只读借用。
    drop(cached_dst);

    /*
    self
      .handler
      .state
      .hw_view
      .store(Some(std::sync::Arc::new(CefView::new(browser_id, bind_group, dirty_rects, info.extra.capture_counter))));
      */
    let popup_offset = if is_popup {
      self.handler.state.popup_offset()
    } else {
      None
    };
    let view = std::sync::Arc::new(CefView::new_hardware(
      browser_id,
      bind_group,
      dirty_rects,
      bounds,
      content_size,
      display_size,
      popup_offset,
      !is_popup,
      info.extra.capture_counter,
      false,
      is_popup,
    ));
    if is_popup {
      self.handler.state.popup_view.store(Some(view));
    } else {
      self.handler.state.hw_view.store(Some(view));
      self.handler.state.clear_pending_resize_target();
      if self.handler.state.popup_rect().is_some()
        && let Some(host) = browser.host()
      {
        host.invalidate(cef::PaintElementType::POPUP);
      }
    }
    wake_render_waker(browser_id, is_view.then_some(interaction_frame_start).flatten());
    if let Some(paint_start) = paint_start {
      lintel_lib_iced::ui_perf::record_webview_paint_for_browser(
        browser_id.inner(),
        "accelerated_paint",
        paint_start.elapsed(),
      );
    }

    if should_store_paint_frame {
      log_gpu_render_once(browser_id);
    }

    /*
    tracing::info!(
      counter = info.extra.capture_counter,
      elapsed = ?std::time::Duration::from_micros(info.extra.timestamp),
      "frame sent"
    );
    */
  }

  fn on_paint(
    &self,
    browser: Option<&mut Browser>,
    type_: PaintElementType,
    dirty_rects: Option<&[Rect]>,
    buffer: *const u8,
    width: ::std::os::raw::c_int,
    height: ::std::os::raw::c_int,
  ) {
    let paint_start = lintel_lib_iced::ui_perf::is_enabled().then(Instant::now);
    let Some(browser) = browser else {
      return;
    };
    let browser_id: BrowserId = browser.identifier().into();
    record_cef_frame_arrival(&self.handler.state, browser_id);
    let interaction_frame_start = lintel_lib_iced::ui_perf::is_webview_interaction_active(browser_id.inner())
      .then_some(paint_start)
      .flatten();
    let is_view = type_ == cef::sys::cef_paint_element_type_t::PET_VIEW.into();
    let is_popup = type_ == cef::sys::cef_paint_element_type_t::PET_POPUP.into();
    if !is_view && !is_popup {
      tracing::error!("invalid paint element type");
      return;
    }

    if buffer.is_null() || width <= 0 || height <= 0 {
      tracing::error!("no pixels");
      return;
    }

    // CEF software paint always provides the complete backing buffer; dirty rects only identify changed pixels.
    let dirty_rect_count = dirty_rects.map_or(0, |rects| rects.len());
    let full_repaint = true;

    let Some(buffer_size) = software_paint_buffer_len(width, height) else {
      tracing::error!(
        ?browser_id,
        width,
        height,
        "CEF CPU paint dimensions exceed supported buffer limits"
      );
      return;
    };
    // SAFETY: CEF OnPaint owns a complete width*height*4 BGRA buffer until this
    // callback returns. Null/positive dimensions and checked row/slice lengths
    // were validated above. The borrow does not escape: queue.write_texture
    // copies its input, and the software fallback copies into an owned image.
    let buffer_slice = unsafe { std::slice::from_raw_parts(buffer, buffer_size) };

    let context = gpu_context();
    let gpu = context
      .as_ref()
      .map(|context| (&context.device, &context.queue, context.adapter.name.contains("ANGLE ")));
    if let Some((device, queue, is_angle)) = gpu {
      let limit = device.limits().max_texture_dimension_2d;
      if width as u32 > limit || height as u32 > limit {
        tracing::error!(
          ?browser_id,
          width,
          height,
          limit,
          "CEF CPU frame exceeds the renderer texture limit"
        );
        return;
      }
      let cached = self.handler.state.get_or_init_gpu(device);
      let texture_format = if is_angle {
        wgpu::TextureFormat::Rgba8Unorm
      } else {
        wgpu::TextureFormat::Bgra8Unorm
      };
      let bounds =
        if is_popup {
          Some(
            self.handler.state.popup_rect().unwrap_or_else(|| {
              iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(width as f32, height as f32))
            }),
          )
        } else {
          None
        };
      let display_size = bounds
        .map(|bounds| bounds.size())
        .unwrap_or_else(|| view_rect_size(self.handler.state.view_rect()));
      let content_size = iced::Size::new(width as f32, height as f32);
      let should_store_paint_frame = self.handler.state.should_store_resize_paint_frame(
        browser_id,
        "software-gpu-upload",
        is_popup,
        content_size,
        display_size,
        dirty_rect_count,
        full_repaint,
        true,
      );
      if !should_store_paint_frame {
        request_resize_retry(&self.handler.state, browser_id);
        wake_render_waker(browser_id, is_view.then_some(interaction_frame_start).flatten());
        if let Some(paint_start) = paint_start {
          lintel_lib_iced::ui_perf::record_webview_paint_for_browser(
            browser_id.inner(),
            "software_upload_rejected",
            paint_start.elapsed(),
          );
        }
        return;
      }
      let cached_tex =
        self
          .handler
          .state
          .get_or_create_paint_texture(device, width as u32, height as u32, texture_format);

      lintel_lib_iced::ui_perf::measure(
        || {
          queue.write_texture(
            wgpu::TexelCopyTextureInfo {
              texture: &cached_tex.texture,
              mip_level: 0,
              origin: wgpu::Origin3d::ZERO,
              aspect: wgpu::TextureAspect::All,
            },
            buffer_slice,
            wgpu::TexelCopyBufferLayout {
              offset: 0,
              bytes_per_row: Some(4 * width as u32),
              rows_per_image: Some(height as u32),
            },
            wgpu::Extent3d {
              width: width as u32,
              height: height as u32,
              depth_or_array_layers: 1,
            },
          );
        },
        |elapsed| {
          lintel_lib_iced::ui_perf::record_webview_paint_for_browser(browser_id.inner(), "software_upload", elapsed)
        },
      );

      let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("Cef Texture Bind Group"),
        layout: &cached.bind_group_layout,
        entries: &[
          wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(&cached_tex.texture.create_view(
              &wgpu::TextureViewDescriptor {
                label: Some("Cef Texture View"),
                ..Default::default()
              },
            )),
          },
          wgpu::BindGroupEntry {
            binding: 1,
            resource: wgpu::BindingResource::Sampler(&cached.sampler),
          },
          wgpu::BindGroupEntry {
            binding: 2,
            resource: cached.full_texture_region_buffer.as_entire_binding(),
          },
        ],
      });

      // 诊断日志会更新 RenderStateInner，先释放缓存纹理的 RefCell 只读借用。
      drop(cached_tex);
      let popup_offset = if is_popup {
        self.handler.state.popup_offset()
      } else {
        None
      };
      let view = std::sync::Arc::new(CefView::new_hardware(
        browser_id,
        bind_group,
        None,
        bounds,
        content_size,
        display_size,
        popup_offset,
        !is_popup,
        0,
        is_angle,
        is_popup,
      ));
      if is_popup {
        self.handler.state.popup_view.store(Some(view));
      } else {
        self.handler.state.hw_view.store(Some(view));
        self.handler.state.clear_pending_resize_target();
      }

      log_gpu_render_once(browser_id);
    } else {
      let content_size = iced::Size::new(width as f32, height as f32);
      let display_size = if is_popup {
        self.handler.state.popup_rect().map_or(content_size, |rect| rect.size())
      } else {
        view_rect_size(self.handler.state.view_rect())
      };
      let should_store_paint_frame = self.handler.state.should_store_resize_paint_frame(
        browser_id,
        "software",
        is_popup,
        content_size,
        display_size,
        dirty_rect_count,
        full_repaint,
        true,
      );
      if should_store_paint_frame {
        let handle = {
          let mut inner = self.handler.state.inner.borrow_mut();
          lintel_lib_iced::ui_perf::measure(
            || {
              convert_bgra_to_rgba(buffer_slice, &mut inner.pixels);
              iced::widget::image::Handle::from_rgba(width as u32, height as u32, inner.pixels.clone())
            },
            |elapsed| {
              lintel_lib_iced::ui_perf::record_webview_paint_for_browser(
                browser_id.inner(),
                "software_convert",
                elapsed,
              )
            },
          )
        };

        self
          .handler
          .state
          .store_software_paint(browser_id, handle, content_size, is_popup);
      } else {
        request_resize_retry(&self.handler.state, browser_id);
      }
    }
    wake_render_waker(browser_id, is_view.then_some(interaction_frame_start).flatten());
    if let Some(paint_start) = paint_start {
      lintel_lib_iced::ui_perf::record_webview_paint_for_browser(
        browser_id.inner(),
        "software_paint",
        paint_start.elapsed(),
      );
    }
  }
}

pub(crate) mod hw_renderer {
  use std::collections::BTreeMap;

  use lintel_lib_iced::iced;
  use lintel_lib_iced::iced_wgpu::wgpu;

  use crate::webview::{BrowserId, client::render_handler::CefViewWrapper, instance::resize};

  #[derive(Default)]
  struct ResizeBoundsTracker {
    by_browser: BTreeMap<BrowserId, iced::Rectangle<f32>>,
  }

  impl ResizeBoundsTracker {
    fn needs_resize(&mut self, browser_id: BrowserId, bounds: iced::Rectangle<f32>) -> bool {
      if self.by_browser.get(&browser_id) == Some(&bounds) {
        return false;
      }
      self.by_browser.insert(browser_id, bounds);
      true
    }
  }

  pub struct CefPipeline {
    pipeline: wgpu::RenderPipeline,
    swizzle_pipeline: wgpu::RenderPipeline,
    quad: Geometry,
    resize_bounds: ResizeBoundsTracker,
  }

  impl iced::widget::shader::Pipeline for CefPipeline {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self
    where
      Self: Sized,
    {
      let _ = queue;
      CefPipeline::new(device, format)
    }
  }

  impl CefPipeline {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
      let texture_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("Cef Texture Bind Group Layout"),
        entries: &[
          wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
              multisampled: false,
              view_dimension: wgpu::TextureViewDimension::D2,
              sample_type: wgpu::TextureSampleType::Float { filterable: true },
            },
            count: None,
          },
          wgpu::BindGroupLayoutEntry {
            binding: 1,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
          },
          wgpu::BindGroupLayoutEntry {
            binding: 2,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
              ty: wgpu::BufferBindingType::Uniform,
              has_dynamic_offset: false,
              min_binding_size: None,
            },
            count: None,
          },
        ],
      });
      let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("Cef Shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
      });
      let swizzle_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("Cef Swizzle Shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shader_win7.wgsl").into()),
      });

      let render_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("Cef Pipeline Layout"),
        bind_group_layouts: &[&texture_bind_group_layout],
        immediate_size: 0,
      });
      let create_pipeline = |label: &'static str, shader: &wgpu::ShaderModule, blend| {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
          label: Some(label),
          layout: Some(&render_pipeline_layout),
          vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[Vertex::desc()],
            compilation_options: Default::default(),
          },
          fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            targets: &[Some(wgpu::ColorTargetState {
              format,
              blend,
              write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
          }),
          primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Cw,
            cull_mode: Some(wgpu::Face::Back),
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
          },
          depth_stencil: None,
          multisample: wgpu::MultisampleState {
            count: 1,
            mask: !0,
            alpha_to_coverage_enabled: false,
          },
          multiview_mask: None,
          cache: None,
        })
      };
      let alpha_blend = Some(wgpu::BlendState {
        color: wgpu::BlendComponent::OVER,
        alpha: wgpu::BlendComponent::OVER,
      });
      let pipeline = create_pipeline("Cef Alpha Render Pipeline", &shader, alpha_blend);
      let swizzle_pipeline = create_pipeline("Cef Alpha Swizzle Render Pipeline", &swizzle_shader, alpha_blend);
      Self {
        pipeline,
        swizzle_pipeline,
        quad: Geometry::new(device),
        resize_bounds: ResizeBoundsTracker::default(),
      }
    }

    fn render(
      &self,
      bind_group: &wgpu::BindGroup,
      render_pass: &mut wgpu::RenderPass,
      bounds: iced::Rectangle<f32>,
      dirty_rect: Option<iced::Rectangle<f32>>,
      swap_rb: bool,
      is_popup: bool,
    ) {
      /*
      if let Some(dirty_rect) = dirty_rect {
        render_pass.set_scissor_rect(
          bounds.x as u32 + dirty_rect.x as u32,
          bounds.y as u32 + dirty_rect.y as u32,
          dirty_rect.width as u32,
          dirty_rect.height as u32,
        );
      }
      */
      render_pass.set_pipeline(if swap_rb {
        &self.swizzle_pipeline
      } else {
        &self.pipeline
      });
      render_pass.set_bind_group(0, bind_group, &[]);
      render_pass.set_vertex_buffer(0, self.quad.vertex_buffer.slice(..));
      render_pass.draw(0..self.quad.vertex_count, 0..1);
    }
  }

  #[derive(Debug)]
  pub struct Primitive {
    bind_group: wgpu::BindGroup,
    browser_id: BrowserId,
    bounds: iced::Rectangle,
    frame_content_size: iced::Size,
    frame_display_size: iced::Size,
    dirty_rect: Option<iced::Rectangle>,
    should_resize: bool,
    swap_rb: bool,
    is_popup: bool,
  }

  impl Primitive {
    fn new(
      bind_group: wgpu::BindGroup,
      browser_id: BrowserId,
      bounds: iced::Rectangle,
      frame_content_size: iced::Size,
      frame_display_size: iced::Size,
      dirty_rect: Option<iced::Rectangle>,
      should_resize: bool,
      swap_rb: bool,
      is_popup: bool,
    ) -> Self {
      Self {
        bind_group,
        browser_id,
        bounds,
        frame_content_size,
        frame_display_size,
        dirty_rect,
        should_resize,
        swap_rb,
        is_popup,
      }
    }
  }

  impl<Message> iced::widget::shader::Program<Message> for CefViewWrapper {
    type State = Option<BrowserId>;
    type Primitive = Primitive;

    fn draw(&self, state: &Self::State, _cursor: iced::mouse::Cursor, bounds: iced::Rectangle) -> Self::Primitive {
      Primitive::new(
        self
          .bind_group()
          .cloned()
          .expect("shader webview frame must come from accelerated paint"),
        self.browser_id,
        bounds,
        self.content_size(),
        self.display_size(),
        self.dirty_rect(),
        self.should_resize(),
        self.swap_rb(),
        self.is_popup(),
      )
    }

    fn update(
      &self,
      state: &mut Self::State,
      _event: &iced::Event,
      _bounds: iced::Rectangle,
      _cursor: iced::advanced::mouse::Cursor,
    ) -> Option<iced::widget::Action<Message>> {
      if Some(self.view.browser_id) != *state {
        state.replace(self.view.browser_id);
      }
      None
    }
  }

  impl iced::widget::shader::Primitive for Primitive {
    type Pipeline = CefPipeline;

    fn prepare(
      &self,
      pipeline: &mut Self::Pipeline,
      _device: &wgpu::Device,
      _queue: &wgpu::Queue,
      bounds: &iced::Rectangle,
      viewport: &iced::widget::shader::Viewport,
    ) {
      if self.should_resize && pipeline.resize_bounds.needs_resize(self.browser_id, *bounds) {
        tracing::trace!(
          browser_id = ?self.browser_id,
          shader_bounds_width = bounds.width,
          shader_bounds_height = bounds.height,
          frame_content_width = self.frame_content_size.width,
          frame_content_height = self.frame_content_size.height,
          frame_display_width = self.frame_display_size.width,
          frame_display_height = self.frame_display_size.height,
          "webview shader bounds changed"
        );
        resize(self.browser_id, *bounds);
      }
    }

    fn draw(&self, renderer: &Self::Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
      renderer.render(
        &self.bind_group,
        render_pass,
        self.bounds,
        self.dirty_rect,
        self.swap_rb,
        self.is_popup,
      );
      true
      //false
    }

    fn render(
      &self,
      renderer: &Self::Pipeline,
      encoder: &mut wgpu::CommandEncoder,
      target: &wgpu::TextureView,
      clip_bounds: &iced::Rectangle<u32>,
    ) {
      return;
      /*
      use cef::ImplBrowserHost;

      let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("Cef Render Pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
          view: target,
          resolve_target: None,
          ops: wgpu::Operations {
            load: wgpu::LoadOp::Load,
            store: wgpu::StoreOp::Store,
          },
          depth_slice: None,
        })],
        ..Default::default()
      });

      render_pass.set_viewport(
        clip_bounds.x as f32,
        clip_bounds.y as f32,
        clip_bounds.width as f32,
        clip_bounds.height as f32,
        0.0,
        1.0,
      );
      renderer.render(
        &self.bind_group,
        &mut render_pass,
        self.bounds,
        self.dirty_rect,
        self.swap_rb,
      );
      */
    }
  }

  #[cfg(test)]
  mod tests {
    use super::ResizeBoundsTracker;
    use crate::webview::BrowserId;
    use lintel_lib_iced::iced;

    #[test]
    fn resize_bounds_are_deduplicated_per_browser() {
      let mut tracker = ResizeBoundsTracker::default();
      let first_id = BrowserId::new(1);
      let second_id = BrowserId::new(2);
      let first_bounds = iced::Rectangle::with_size(iced::Size::new(880.0, 596.0));
      let second_bounds = iced::Rectangle::with_size(iced::Size::new(210.0, 28.0));

      assert!(tracker.needs_resize(first_id, first_bounds));
      assert!(!tracker.needs_resize(first_id, first_bounds));
      assert!(tracker.needs_resize(second_id, second_bounds));
      assert!(
        !tracker.needs_resize(first_id, first_bounds),
        "another WebView must not invalidate this browser's stable bounds"
      );
    }
  }

  #[repr(C)]
  #[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
  struct Vertex {
    position: [f32; 3],
    tex_coords: [f32; 2],
  }

  impl Vertex {
    const ATTRIBS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2];

    fn desc<'a>() -> wgpu::VertexBufferLayout<'a> {
      wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &Self::ATTRIBS,
      }
    }
  }

  struct Geometry {
    vertex_buffer: wgpu::Buffer,
    vertex_count: u32,
  }

  impl Geometry {
    fn new(device: &wgpu::Device) -> Self {
      use wgpu::util::DeviceExt as _;

      let x = -1.0;
      let y = 1.0;
      let width = 2.0;
      let height = 2.0;
      let z = 1.0;

      let vertices = [
        Vertex {
          position: [x, y, z],
          tex_coords: [0.0, 0.0],
        },
        Vertex {
          position: [x + width, y, z],
          tex_coords: [1.0, 0.0],
        },
        Vertex {
          position: [x, y - height, z],
          tex_coords: [0.0, 1.0],
        },
        Vertex {
          position: [x + width, y - height, z],
          tex_coords: [1.0, 1.0],
        },
      ];

      let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("Quad Vertex Buffer"),
        contents: bytemuck::cast_slice(&vertices),
        usage: wgpu::BufferUsages::VERTEX,
      });

      Self {
        vertex_buffer,
        vertex_count: vertices.len() as u32,
      }
    }
  }
}
