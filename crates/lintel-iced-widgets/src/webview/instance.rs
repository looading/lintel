use super::{
  client::{
    ClientBuilder, ClientEventSubscriber, IcyClient, IcyClientState, NativeResponsePolicy, SubscriberDepositor,
  },
  request::{IcyRequestContextHandler, RequestContextHandlerBuilder},
  widget::{WebviewUiCommandOperation, WebviewUiStateOperation},
};
#[cfg(target_os = "macos")]
use crate::webview::drag_drop::drop_target_mac;
#[cfg(target_os = "windows")]
use crate::webview::drag_drop::{register_browser_drop_target, start_dragging, unregister_browser_drop_target};
use crate::webview::{
  WebviewFocusReason, WebviewKeyReason, WebviewSyntheticKey, WebviewUiCommand, WebviewUiStateSnapshot,
};
use crate::{
  circular::Circular,
  crash_view::{self, AppInfo, CrashInfo, CrashView},
  webview::{
    DownloadEvent, PreloadType, Webview,
    client::{CefView, CefViewWrapper, IcyClientConfig, RequestEvent},
    data::{BrowerCreateExtraInfo, CHANNEL_INVOKE_JS, RustInvokeResponse},
    lifecycle::ClientLifecyeleImpl,
    request::cookie::{
      self, CefCookie, IcyCompletionCallback, IcyCookieVisitor, IcyDeleteCookiesCallback, IcySetCookieCallback,
    },
  },
};
use crate::{post_task_sync_ui, webview::BrowserId};
use anyhow::anyhow;
use arc_swap::ArcSwapOption;
use atomic_take::AtomicTake;
use bytes::Bytes;
use camino::Utf8PathBuf;
use cef::{ImplBrowserHost, RequestContextSettings, sys::cef_termination_status_t, *};
use iced::{Element, Subscription, Task, widget::Image, window};
use lintel_lib_iced::iced_wgpu::wgpu::rwh::RawWindowHandle;
use lintel_lib_iced::iced_wgpu::window::compositor::gpu_context;
use lintel_lib_iced::{self as iced, Length::Fill, advanced::mouse::Click, widget::container};
use lintel_shared_data::TransferData;
use std::{
  cell::RefCell,
  collections::BTreeMap,
  fmt::Debug,
  sync::{
    Arc, LazyLock, Mutex,
    atomic::{AtomicUsize, Ordering},
  },
  time::{Duration, Instant},
};
use tokio::{
  sync::{
    broadcast,
    mpsc::UnboundedReceiver,
    oneshot::{self, Sender},
  },
  time::timeout,
};
use tracing::{debug, error, trace, warn};
use url::Url;
#[cfg(target_os = "windows")]
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

pub(crate) const DEFAULT_MAX_WINDOWLESS_FRAME_RATE: i32 = 60;
pub(crate) const DEFAULT_MIN_WINDOWLESS_FRAME_RATE: i32 = 30;
/// CEF UI 线程上的 resize 补帧退避；用完后每 5 秒重试，直到目标帧落地或 generation 失效。
const RESIZE_FRAME_PUMP_DELAYS: [Duration; 11] = [
  Duration::from_millis(16),
  Duration::from_millis(33),
  Duration::from_millis(66),
  Duration::from_millis(100),
  Duration::from_millis(250),
  Duration::from_millis(500),
  Duration::from_secs(1),
  Duration::from_secs(2),
  Duration::from_secs(4),
  Duration::from_secs(5),
  Duration::from_secs(5),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WebviewRenderMode {
  /// 使用 CEF 当前帧原始尺寸绘制，超出组件区域的部分被裁剪。
  #[default]
  Crop,
  /// 忽略宽高比，直接拉伸到组件尺寸。
  Stretch,
  /// 保持宽高比完整显示在组件内。
  Fit,
  /// 保持宽高比填满组件，超出部分被裁剪。
  Fill,
}

impl WebviewRenderMode {
  pub const ALL: [Self; 4] = [Self::Crop, Self::Stretch, Self::Fit, Self::Fill];

  pub fn status_label(self) -> &'static str {
    match self {
      Self::Crop => "crop",
      Self::Stretch => "stretch",
      Self::Fit => "fit",
      Self::Fill => "fill",
    }
  }

  fn center_content(self) -> bool {
    !matches!(self, Self::Crop)
  }
}

impl std::fmt::Display for WebviewRenderMode {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(match self {
      Self::Crop => "裁剪",
      Self::Stretch => "拉伸",
      Self::Fit => "适应",
      Self::Fill => "填充",
    })
  }
}

static RESIZE_FRAME_PUMP_GENERATIONS: LazyLock<Mutex<BTreeMap<BrowserId, u64>>> =
  LazyLock::new(|| Mutex::new(BTreeMap::new()));

fn valid_render_size(size: iced::Size) -> bool {
  size.width.is_finite() && size.height.is_finite() && size.width > 0.0 && size.height > 0.0
}

fn render_mode_size(render_mode: WebviewRenderMode, frame_size: iced::Size, bounds_size: iced::Size) -> iced::Size {
  if !valid_render_size(frame_size) || !valid_render_size(bounds_size) {
    return frame_size;
  }

  match render_mode {
    WebviewRenderMode::Crop => frame_size,
    WebviewRenderMode::Stretch => bounds_size,
    WebviewRenderMode::Fit | WebviewRenderMode::Fill => {
      let width_scale = bounds_size.width / frame_size.width;
      let height_scale = bounds_size.height / frame_size.height;
      let scale = if matches!(render_mode, WebviewRenderMode::Fit) {
        width_scale.min(height_scale)
      } else {
        width_scale.max(height_scale)
      };
      iced::Size::new(frame_size.width * scale, frame_size.height * scale)
    }
  }
}

fn cef_rect_size_from_size(size: iced::Size) -> Option<iced::Size> {
  if !size.width.is_finite() || !size.height.is_finite() || size.width <= 0.0 || size.height <= 0.0 {
    return None;
  }

  let width = size.width as i32;
  let height = size.height as i32;
  if width <= 0 || height <= 0 {
    return None;
  }

  Some(iced::Size::new(width as f32, height as f32))
}

fn cef_rect_size_from_bounds(bounds: iced::Rectangle) -> Option<iced::Size> {
  cef_rect_size_from_size(bounds.size())
}

fn next_resize_frame_pump_generation(browser_id: BrowserId) -> u64 {
  let Ok(mut generations) = RESIZE_FRAME_PUMP_GENERATIONS.lock() else {
    return 0;
  };

  let generation = generations.entry(browser_id).or_insert(0);
  *generation = (*generation).wrapping_add(1).max(1);
  *generation
}

fn current_resize_frame_pump_generation(browser_id: BrowserId) -> Option<u64> {
  RESIZE_FRAME_PUMP_GENERATIONS
    .lock()
    .ok()
    .and_then(|generations| generations.get(&browser_id).copied())
}

fn clear_resize_frame_pump_generation(browser_id: BrowserId) {
  if let Ok(mut generations) = RESIZE_FRAME_PUMP_GENERATIONS.lock() {
    generations.remove(&browser_id);
  }
}

fn resize_frame_pump_delay(attempt_index: usize) -> Option<Duration> {
  RESIZE_FRAME_PUMP_DELAYS
    .get(attempt_index)
    .or_else(|| RESIZE_FRAME_PUMP_DELAYS.last())
    .copied()
}

fn schedule_resize_frame_pump(browser_id: BrowserId, target_size: iced::Size, generation: u64) {
  fn schedule_step(browser_id: BrowserId, target_size: iced::Size, generation: u64, attempt_index: usize) {
    let Some(delay) = resize_frame_pump_delay(attempt_index) else {
      trace!(
        ?browser_id,
        ?target_size,
        generation,
        "webview resize frame pump exhausted"
      );
      return;
    };

    let attempt = attempt_index.saturating_add(1);
    let mut task = crate::webview::task::IcyTask::build(move || {
      let generation_matches = current_resize_frame_pump_generation(browser_id) == Some(generation);
      let has_pending_resize = generation_matches
        && webview_states().with_borrow(|states| {
          states
            .get(&browser_id)
            .is_some_and(|state| state.render.has_pending_resize_target(target_size))
        });
      if !has_pending_resize {
        return;
      }

      let Some(host) = browser_host_get_browser_by_identifier(browser_id.inner()).and_then(|browser| browser.host())
      else {
        trace!(
          ?browser_id,
          ?target_size,
          generation,
          attempt,
          "webview resize frame pump stopped: missing host"
        );
        return;
      };

      trace!(
        ?browser_id,
        ?target_size,
        generation,
        attempt,
        delay_ms = delay.as_millis(),
        backend = "cef_ui",
        "webview resize frame pump request"
      );
      host.was_resized();
      host.invalidate(cef::sys::cef_paint_element_type_t::PET_VIEW.into());
      schedule_step(browser_id, target_size, generation, attempt_index.saturating_add(1));
    });

    let posted = cef::post_delayed_task(
      cef::sys::cef_thread_id_t::TID_UI.into(),
      Some(&mut task),
      delay.as_millis() as _,
    );
    if posted <= 0 {
      trace!(
        ?browser_id,
        ?target_size,
        generation,
        attempt,
        delay_ms = delay.as_millis(),
        "webview resize frame pump schedule failed"
      );
    }
  }

  schedule_step(browser_id, target_size, generation, 0);
}

/// 缓存的最大帧率，避免每帧都去查配置
static CACHED_MAX_WINDOWLESS_FRAME_RATE: std::sync::atomic::AtomicI32 =
  std::sync::atomic::AtomicI32::new(DEFAULT_MAX_WINDOWLESS_FRAME_RATE);

static WEBVIEW_UI_COMMAND_BUS: LazyLock<broadcast::Sender<WebviewUiCommand>> = LazyLock::new(|| {
  let (sender, _) = broadcast::channel(64);
  sender
});

fn publish_webview_ui_command(command: WebviewUiCommand) {
  let _ = WEBVIEW_UI_COMMAND_BUS.send(command);
}

#[cfg(target_os = "windows")]
fn shared_texture_enabled() -> bool {
  // Official Windows uses CEF's CPU BGRA OnPaint transport for every feature configuration.
  // Never select a platform-specific shared-handle import based on the adapter.
  false
}

#[cfg(not(target_os = "windows"))]
fn shared_texture_enabled() -> bool {
  let backend = gpu_context().map(|context| context.adapter.backend);
  backend == Some(lintel_lib_iced::iced_wgpu::wgpu::Backend::Metal)
}

#[cfg(target_os = "windows")]
fn get_primary_monitor_resolution() -> Option<(i32, i32)> {
  let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
  let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
  (width > 0 && height > 0).then_some((width, height))
}

#[cfg(target_os = "windows")]
fn is_primary_monitor_4k() -> bool {
  // 这里只看主屏物理分辨率；主屏达到 4K 时，短期内统一降到 30fps，避免输入卡顿。
  get_primary_monitor_resolution().is_some_and(|(width, height)| {
    let long_edge = width.max(height);
    let short_edge = width.min(height);
    long_edge >= 3840 && short_edge >= 2160
  })
}

#[cfg(not(target_os = "windows"))]
fn is_primary_monitor_4k() -> bool {
  false
}

pub(crate) fn get_max_windowless_frame_rate() -> i32 {
  let max_frame_rate = if is_primary_monitor_4k() {
    DEFAULT_MIN_WINDOWLESS_FRAME_RATE
  } else {
    DEFAULT_MAX_WINDOWLESS_FRAME_RATE
  };
  CACHED_MAX_WINDOWLESS_FRAME_RATE.store(max_frame_rate, std::sync::atomic::Ordering::Relaxed);
  max_frame_rate
}

/// 获取缓存的最大帧率，避免每帧都去查配置
pub(crate) fn get_cached_max_windowless_frame_rate() -> i32 {
  CACHED_MAX_WINDOWLESS_FRAME_RATE.load(std::sync::atomic::Ordering::Relaxed)
}

fn clamp_windowless_frame_rate(frame_rate: i32, max_frame_rate: i32) -> i32 {
  frame_rate.clamp(1, max_frame_rate.max(1))
}

fn resolve_windowless_frame_rate(
  configured_frame_rate: Option<i32>,
  screen_refresh_rate: Option<i32>,
  max_frame_rate: i32,
) -> i32 {
  clamp_windowless_frame_rate(
    configured_frame_rate.or(screen_refresh_rate).unwrap_or(30),
    max_frame_rate,
  )
}

pub enum CefAction {
  Loaded(BrowserId),
  LoadStart {
    browser_id: BrowserId,
    frame_id: String,
    transition_type: u32,
  },
  LoadChanged {
    browser_id: BrowserId,
    is_loading: bool,
    can_go_forward: bool,
    can_go_back: bool,
  },
  LoadError {
    browser_id: BrowserId,
    frame_id: String,
    error_code: i32,
    error_text: Option<String>,
    failed_url: Option<String>,
    is_main_frame: bool,
  },
  TitleChanged(BrowserId, String),
  UrlChanged(BrowserId, String),
  Created(BrowserId, JsCallRx),
  CreatePopup(BrowserId, String),
  Closed(BrowserId),
  Request(RequestEvent),
  Download(DownloadEvent),
  ContextMenuChanged(BrowserId),
  None,
}

/// Return value of [`CefBrowser::update`].
///
/// The internal iced `Task` drives browser creation and **must** be executed.
/// Call [`CefUpdate::split`] to obtain both the action and the mapped task —
/// you cannot access the action without also taking the task.
#[must_use = "CefBrowser tasks must be executed — call `.split(f)` to get (CefAction, Task<M>)"]
pub struct CefUpdate {
  action: CefAction,
  task: Task<CefMessage>,
}

impl CefUpdate {
  fn new(action: CefAction, task: Task<CefMessage>) -> Self {
    Self { action, task }
  }

  /// Extracts the `CefAction` and maps the internal task to your message type.
  /// The returned `Task<M>` **must** be returned from your `update()`.
  pub fn split<M: Send + 'static>(self, f: impl Fn(CefMessage) -> M + Send + 'static) -> (CefAction, Task<M>) {
    (self.action, self.task.map(f))
  }
}

#[derive(Clone, Debug, Default)]
pub struct ContextMenuLabels {
  pub copy: String,
  pub paste: String,
  pub cut: String,
  pub select_all: String,
  pub copy_link: String,
  pub open_link_in_new_tab: String,
}

impl ContextMenuLabels {
  pub fn chinese_default() -> Self {
    Self {
      copy: "复制".into(),
      paste: "粘贴".into(),
      cut: "剪切".into(),
      select_all: "全选".into(),
      copy_link: "复制链接".into(),
      open_link_in_new_tab: "在新标签打开".into(),
    }
  }
}

#[derive(Clone, Debug)]
pub struct ContextMenuRequest {
  pub browser_id: BrowserId,
  pub x: f32,
  pub y: f32,
  pub page_url: Option<String>,
  pub frame_url: Option<String>,
  pub link_url: Option<String>,
  pub source_url: Option<String>,
  pub selection_text: Option<String>,
  pub is_editable: bool,
  pub has_selection: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextMenuAction {
  Copy,
  Paste,
  Cut,
  SelectAll,
  CopyLink,
  OpenLinkInNewTab,
  Custom(String),
  Separator,
}

#[derive(Clone, Debug)]
pub struct ContextMenuItem {
  pub action: ContextMenuAction,
  pub label: String,
  pub enabled: bool,
}

pub type ContextMenuBuilder =
  Arc<dyn Fn(&ContextMenuRequest, &ContextMenuLabels) -> Vec<ContextMenuItem> + Send + Sync>;

#[derive(Clone)]
pub struct ContextMenuConfig {
  pub labels: ContextMenuLabels,
  pub build: ContextMenuBuilder,
}

impl ContextMenuConfig {
  pub fn with_default_actions(labels: ContextMenuLabels) -> Self {
    Self {
      labels,
      build: Arc::new(default_context_menu_items),
    }
  }

  pub fn chinese_default_actions() -> Self {
    Self::with_default_actions(ContextMenuLabels::chinese_default())
  }
}

fn default_context_menu_items(request: &ContextMenuRequest, labels: &ContextMenuLabels) -> Vec<ContextMenuItem> {
  let mut items = Vec::new();

  if request.has_selection {
    items.push(ContextMenuItem {
      action: ContextMenuAction::Copy,
      label: labels.copy.clone(),
      enabled: true,
    });
  }

  if request.is_editable {
    if request.has_selection {
      items.push(ContextMenuItem {
        action: ContextMenuAction::Cut,
        label: labels.cut.clone(),
        enabled: true,
      });
    }

    items.push(ContextMenuItem {
      action: ContextMenuAction::Paste,
      label: labels.paste.clone(),
      enabled: true,
    });

    items.push(ContextMenuItem {
      action: ContextMenuAction::SelectAll,
      label: labels.select_all.clone(),
      enabled: true,
    });
  }

  if request.link_url.is_some() {
    if !items.is_empty() {
      items.push(ContextMenuItem {
        action: ContextMenuAction::Separator,
        label: String::new(),
        enabled: false,
      });
    }

    items.push(ContextMenuItem {
      action: ContextMenuAction::CopyLink,
      label: labels.copy_link.clone(),
      enabled: true,
    });
    items.push(ContextMenuItem {
      action: ContextMenuAction::OpenLinkInNewTab,
      label: labels.open_link_in_new_tab.clone(),
      enabled: true,
    });
  }

  items
}

#[derive(Clone, Debug, Default)]
pub struct ActiveContextMenu {
  pub request: Option<ContextMenuRequest>,
  pub items: Vec<ContextMenuItem>,
}

impl ActiveContextMenu {
  pub fn is_open(&self) -> bool {
    self.request.is_some() && !self.items.is_empty()
  }

  pub fn position(&self) -> Option<iced::Point> {
    self
      .request
      .as_ref()
      .map(|request| iced::Point::new(request.x, request.y))
  }

  pub fn close(&mut self) {
    self.request = None;
    self.items.clear();
  }
}

#[derive(Clone)]
pub struct JsCallRx(Arc<AtomicTake<tokio::sync::mpsc::UnboundedReceiver<JsInvocation>>>);

impl JsCallRx {
  pub fn new(receiver: Option<tokio::sync::mpsc::UnboundedReceiver<JsInvocation>>) -> Self {
    Self(Arc::new(if let Some(receiver) = receiver {
      AtomicTake::new(receiver)
    } else {
      AtomicTake::empty()
    }))
  }

  fn take(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<JsInvocation>> {
    self.0.take()
  }

  /// Take the receiver and spawn a [`JsActor`] that forwards UI messages as an iced [`Task`].
  pub fn spawn_handler<M, F, H>(self, make_handler: F) -> iced::Task<M>
  where
    M: std::fmt::Debug + Send + Sync + 'static,
    H: JsHandler<Message = JsInvocation> + Send + 'static,
    F: FnOnce(tokio::sync::mpsc::UnboundedSender<M>) -> H,
  {
    let Some(rx) = self.take() else {
      return iced::Task::none();
    };
    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(JsActor::new(make_handler(ui_tx), rx).run());
    iced::Task::stream(tokio_stream::wrappers::UnboundedReceiverStream::new(ui_rx))
  }
}

static RUST_INVOKE_RESPONSE: LazyLock<dashmap::DashMap<String, Sender<Bytes>>> = LazyLock::new(Default::default);

#[derive(Clone)]
pub enum CefMessage {
  LoadStart {
    browser_id: BrowserId,
    frame_id: String,
    transition_type: u32,
  },
  LoadChanged {
    browser_id: BrowserId,
    is_loading: bool,
    can_go_forward: bool,
    can_go_back: bool,
  },
  LoadError {
    browser_id: BrowserId,
    frame_id: String,
    error_code: i32,
    error_text: Option<String>,
    failed_url: Option<String>,
  },
  Loaded {
    browser_id: BrowserId,
    frame_id: String,
    http_status_code: u16,
  },
  Create {
    window_id: window::Id,
    config: WebviewConfig,
  },
  ContextMenuRequested(ContextMenuRequest),
  ContextMenuAction {
    browser_id: BrowserId,
    action: ContextMenuAction,
  },
  ContextMenuClosed(BrowserId),
  CrashView(BrowserId, crash_view::Message),

  KeyEvent(BrowserId, iced::keyboard::Event),
  TitleChanged(BrowserId, String),
  UrlChanged(BrowserId, String),
  CursorChanged(BrowserId, iced::mouse::Interaction),
  CreatePopup(BrowserId, String),
  Created {
    launch_id: LaunchId,
    browser_id: BrowserId,
    js_call_rx: JsCallRx,
  },
  Closed(BrowserId),
  Rescaled(BrowserId, f32),
  BoundsChanged(BrowserId, iced::Rectangle),
  CompositionPositionChanged(BrowserId, f32, f32),
  UpdateCaretOffset(BrowserId, i32),
  FocusedNodeChanged(BrowserId, iced::Rectangle, bool),
  Request(RequestEvent),
  BeginFrame {},
  Download(DownloadEvent),
  DragOperation(BrowserId, DragData),
  SetFrameRate(BrowserId, i32),
  UiCommand(WebviewUiCommand),
  None,
}

#[derive(Clone)]
pub struct DragData {
  pub(crate) data: cef::DragData,
  pub(crate) operation: cef::DragOperationsMask,
  pub(crate) x: i32,
  pub(crate) y: i32,
}

impl std::fmt::Debug for DragData {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Drag").field("x", &self.x).field("y", &self.y).finish()
  }
}

impl CefMessage {
  pub fn browser_id(&self) -> Option<BrowserId> {
    match &self {
      CefMessage::LoadStart { browser_id, .. } => Some(*browser_id),
      CefMessage::LoadChanged { browser_id, .. } => Some(*browser_id),
      CefMessage::LoadError { browser_id, .. } => Some(*browser_id),
      CefMessage::Loaded { browser_id, .. } => Some(*browser_id),
      CefMessage::Create { .. } => None,
      CefMessage::ContextMenuRequested(request) => Some(request.browser_id),
      CefMessage::ContextMenuAction { browser_id, .. } => Some(*browser_id),
      CefMessage::ContextMenuClosed(browser_id) => Some(*browser_id),
      CefMessage::CrashView(browser_id, ..) => Some(*browser_id),
      CefMessage::KeyEvent(browser_id, ..) => Some(*browser_id),
      CefMessage::TitleChanged(browser_id, _) => Some(*browser_id),
      CefMessage::UrlChanged(browser_id, _) => Some(*browser_id),
      CefMessage::CursorChanged(browser_id, ..) => Some(*browser_id),
      CefMessage::CreatePopup(browser_id, _) => Some(*browser_id),
      CefMessage::Created { browser_id, .. } => Some(*browser_id),
      CefMessage::Closed(browser_id) => Some(*browser_id),
      CefMessage::UpdateCaretOffset(browser_id, ..) => Some(*browser_id),
      CefMessage::FocusedNodeChanged(browser_id, ..) => Some(*browser_id),
      CefMessage::CompositionPositionChanged(browser_id, x, y) => Some(*browser_id),
      CefMessage::Request(request_event) => Some(request_event.browser_id()),
      CefMessage::BeginFrame {} => None,
      CefMessage::Download(download_event) => download_event.browser_id().into(),
      CefMessage::DragOperation(browser_id, ..) => Some(*browser_id),
      CefMessage::SetFrameRate(browser_id, ..) => Some(*browser_id),
      CefMessage::BoundsChanged(browser_id, bounds) => Some(*browser_id),
      CefMessage::Rescaled(browser_id, scale_factor) => Some(*browser_id),
      CefMessage::UiCommand(command) => Some(command.browser_id()),
      CefMessage::None => None,
    }
  }
}

fn should_accept_message(
  current_browser_id: Option<BrowserId>,
  current_launch_id: Option<LaunchId>,
  message: &CefMessage,
) -> bool {
  if let CefMessage::Created { launch_id, .. } = message {
    return current_browser_id.is_none() && current_launch_id == Some(*launch_id);
  }

  match (current_browser_id, message.browser_id()) {
    (Some(current_browser_id), Some(message_browser_id)) => current_browser_id == message_browser_id,
    (None, Some(_)) => false,
    (Some(_), None) | (None, None) => {
      matches!(
        message,
        CefMessage::Create { .. } | CefMessage::BeginFrame {} | CefMessage::None
      )
    }
  }
}

impl std::fmt::Debug for CefMessage {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Create { window_id, config } => f.debug_tuple("Create").field(window_id).field(config).finish(),
      Self::ContextMenuRequested(request) => f.debug_tuple("ContextMenuRequested").field(request).finish(),
      Self::ContextMenuAction { browser_id, action } => f
        .debug_struct("ContextMenuAction")
        .field("browser_id", browser_id)
        .field("action", action)
        .finish(),
      Self::ContextMenuClosed(browser_id) => f.debug_tuple("ContextMenuClosed").field(browser_id).finish(),
      Self::Loaded {
        browser_id,
        frame_id,
        http_status_code,
      } => f.debug_struct("Loaded").finish(),
      Self::LoadStart {
        browser_id,
        frame_id,
        transition_type,
      } => f.debug_struct("LoadStart").finish(),
      Self::LoadChanged {
        browser_id,
        is_loading,
        can_go_forward,
        can_go_back,
      } => f.debug_struct("LoadChanged").finish(),
      Self::LoadError {
        browser_id,
        frame_id,
        error_code,
        error_text,
        failed_url,
      } => f.debug_struct("LoadError").finish(),
      Self::TitleChanged(browser_id, title) => f.debug_tuple("TitleChanged").field(browser_id).field(title).finish(),
      Self::UrlChanged(browser_id, url) => f.debug_tuple("UrlChanged").field(browser_id).field(url).finish(),
      Self::CursorChanged(browser_id, cursor) => {
        f.debug_tuple("CursorChanged").field(browser_id).field(cursor).finish()
      }
      Self::KeyEvent(browser_id, event) => f.debug_tuple("KeyEvent").field(event).finish(),
      Self::Created {
        launch_id, browser_id, ..
      } => f.debug_tuple("Created").field(launch_id).field(browser_id).finish(),
      Self::Closed(browser_id) => f.debug_tuple("Closed").field(browser_id).finish(),
      Self::CreatePopup(browser_id, target_url, ..) => f
        .debug_struct("CreatePopup")
        .field("browser_id", browser_id)
        .field("target_url", &target_url)
        .finish(),
      Self::UpdateCaretOffset(caret_offset, browser_id) => f
        .debug_tuple("UpdateCaretOffset")
        .field(caret_offset)
        .field(browser_id)
        .finish(),
      Self::FocusedNodeChanged(browser_id, rectangle, has_focus) => f
        .debug_tuple("FocusedNodeChanged")
        .field(browser_id)
        .field(rectangle)
        .field(has_focus)
        .finish(),
      Self::Request(request_event) => f.debug_tuple("Request").field(request_event).finish(),
      Self::BeginFrame {} => f.debug_tuple("BeginFrame").finish(),
      Self::Download(_) => f.debug_tuple("Download").finish(),
      Self::DragOperation(browser_id, ..) => f.debug_tuple("DragOperation").field(browser_id).finish(),
      Self::SetFrameRate(browser_id, frame_rate) => f
        .debug_tuple("SetFrameRate")
        .field(browser_id)
        .field(frame_rate)
        .finish(),
      Self::None => f.debug_tuple("None").finish(),
      Self::BoundsChanged(browser_id, bounds) => {
        f.debug_tuple("BoundsChanged").field(browser_id).field(bounds).finish()
      }
      Self::CompositionPositionChanged(browser_id, x, y) => f
        .debug_tuple("CompositionPositionChanged")
        .field(browser_id)
        .field(x)
        .field(y)
        .finish(),
      Self::CrashView(..) => f.debug_tuple("CrashView").finish(),
      Self::Rescaled(browser_id, scale_factor) => {
        f.debug_tuple("Rescaled").field(browser_id).field(scale_factor).finish()
      }
      Self::UiCommand(command) => f.debug_tuple("UiCommand").field(command).finish(),
    }
  }
}

impl std::fmt::Debug for CefAction {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      CefAction::Loaded(browser_id) => f.debug_tuple("Loaded").field(browser_id).finish(),
      CefAction::LoadStart {
        browser_id,
        frame_id,
        transition_type,
      } => f.debug_struct("LoadStart").finish(),
      CefAction::LoadChanged {
        browser_id,
        is_loading,
        can_go_forward,
        can_go_back,
      } => f.debug_struct("LoadChanged").finish(),
      CefAction::LoadError {
        browser_id,
        frame_id,
        error_code,
        error_text,
        failed_url,
        is_main_frame,
      } => f.debug_struct("LoadError").finish(),
      CefAction::Created(browser_id, ..) => f.debug_tuple("Created").field(browser_id).finish(),
      CefAction::Closed(browser_id) => f.debug_tuple("Closed").field(browser_id).finish(),
      CefAction::TitleChanged(browser_id, title) => {
        f.debug_tuple("TitleChanged").field(browser_id).field(title).finish()
      }
      CefAction::UrlChanged(browser_id, url) => f.debug_tuple("UrlChanged").field(browser_id).field(url).finish(),
      CefAction::CreatePopup(browser_id, target_url) => f
        .debug_tuple("CreatePopup")
        .field(browser_id)
        .field(target_url)
        .finish(),
      CefAction::None => f.debug_tuple("None").finish(),
      CefAction::Request(request) => f.debug_tuple("Request").field(request).finish(),
      CefAction::ContextMenuChanged(browser_id) => f.debug_tuple("ContextMenuChanged").field(browser_id).finish(),
      CefAction::Download(event) => f.debug_tuple("Download").finish(),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LaunchId(usize);

impl LaunchId {
  pub fn unique() -> Self {
    static NEXT_UNIQUE_LAUNCH_ID: AtomicUsize = AtomicUsize::new(0);
    let id = NEXT_UNIQUE_LAUNCH_ID.fetch_add(1, Ordering::Relaxed);
    Self(id)
  }
}

thread_local! {
    static LAUNCHED_BROWSERS: RefCell<BTreeMap<LaunchId, IcyClientState>> = const { RefCell::new(BTreeMap::new()) };
    static WEBVIEW_STATES: RefCell<BTreeMap<BrowserId, IcyClientState>> = const { RefCell::new(BTreeMap::new()) };
}

pub(crate) fn launched_browsers() -> &'static std::thread::LocalKey<RefCell<BTreeMap<LaunchId, IcyClientState>>> {
  debug_assert!(cef::currently_on(cef::sys::cef_thread_id_t::TID_UI.into()) == 1);
  &LAUNCHED_BROWSERS
}

pub(crate) fn webview_states() -> &'static std::thread::LocalKey<RefCell<BTreeMap<BrowserId, IcyClientState>>> {
  debug_assert!(cef::currently_on(cef::sys::cef_thread_id_t::TID_UI.into()) == 1);
  &WEBVIEW_STATES
}

pub(crate) fn new_browser(launch_id: LaunchId, browser: IcyClientState) {
  if let Err(err) = post_task_sync_ui!(move || {
    launched_browsers().with_borrow_mut(|browsers| {
      browsers.insert(launch_id, browser).inspect(|_| {
        tracing::error!("unexpected browser already exists");
      })
    });
  }) {
    tracing::error!(?err, "cannot add new browser state");
  }
}

pub(crate) fn new_webview(launch_id: LaunchId, browser_id: BrowserId) {
  if let Err(err) = post_task_sync_ui!(move || {
    if let Some((_, state)) = launched_browsers().with_borrow_mut(|browsers| browsers.remove_entry(&launch_id)) {
      webview_states().with_borrow_mut(|states| states.insert(browser_id, state));
    }
  }) {
    tracing::error!(?err, "cannot add new webview state");
  }
}

pub(crate) fn remove_webview(browser_id: BrowserId) {
  clear_resize_frame_pump_generation(browser_id);

  if let Err(err) = post_task_sync_ui!(move || {
    webview_states().with_borrow_mut(|states| {
      states.remove(&browser_id);
    });
  }) {
    tracing::error!(?err, "cannot remove webview state");
  }
}

pub(crate) fn resize(browser_id: BrowserId, bounds: iced::Rectangle) {
  _ = post_task_sync_ui!(move || {
    if let Some((target_size, pump_generation)) = webview_states().with_borrow_mut(|states| {
      let Some(state) = states.get_mut(&browser_id) else {
        return None;
      };

      let Some(target_size) = cef_rect_size_from_bounds(bounds) else {
        warn!(
          ?browser_id,
          bounds_x = bounds.x,
          bounds_y = bounds.y,
          bounds_width = bounds.width,
          bounds_height = bounds.height,
          "webview cef resize skipped: invalid bounds"
        );
        return None;
      };

      let target_rect_width = target_size.width as i32;
      let target_rect_height = target_size.height as i32;
      let bound = cef::Rect {
        x: 0,
        y: 0,
        width: target_rect_width,
        height: target_rect_height,
      };
      let previous_rect = state.render.view_rect();
      let previous_origin = state.render.view_origin();
      let target_origin = iced::Point::new(bounds.x, bounds.y);
      let view_rect_changed = previous_rect.width != target_rect_width || previous_rect.height != target_rect_height;
      let view_origin_changed = previous_origin != target_origin;
      let pump_generation = if view_rect_changed {
        state.render.set_pending_resize_target(target_size);
        next_resize_frame_pump_generation(browser_id)
      } else {
        0
      };
      state.render.set_view_rect(bound);
      state.render.set_view_origin(target_origin);
      if view_rect_changed || view_origin_changed {
        trace!(
          ?browser_id,
          bounds_x = bounds.x,
          bounds_y = bounds.y,
          bounds_width = bounds.width,
          bounds_height = bounds.height,
          previous_origin_x = previous_origin.x,
          previous_origin_y = previous_origin.y,
          target_origin_x = target_origin.x,
          target_origin_y = target_origin.y,
          previous_rect_x = previous_rect.x,
          previous_rect_y = previous_rect.y,
          previous_rect_width = previous_rect.width,
          previous_rect_height = previous_rect.height,
          target_rect_x = 0_i32,
          target_rect_y = 0_i32,
          target_rect_width,
          target_rect_height,
          view_rect_changed,
          view_origin_changed,
          "webview cef resize rect updated"
        );
      }
      view_rect_changed.then_some((target_size, pump_generation))
    }) {
      let Some(host) = browser_host_get_browser_by_identifier(browser_id.inner()).and_then(|b| b.host()) else {
        warn!(?browser_id, "webview cef resize notify skipped: missing host");
        return;
      };
      debug!(
        ?browser_id,
        ?target_size,
        pump_generation,
        "webview cef resize notify host"
      );
      host.notify_move_or_resize_started();
      host.was_resized();
      host.invalidate(cef::sys::cef_paint_element_type_t::PET_VIEW.into());
      host.send_external_begin_frame();
      schedule_resize_frame_pump(browser_id, target_size, pump_generation);
    }
  });
}

pub(crate) fn rescale(browser_id: BrowserId, scale_factor: f32) {
  _ = post_task_sync_ui!(move || {
    if webview_states().with_borrow_mut(|states| {
      if let Some(state) = states.get_mut(&browser_id)
        && scale_factor > 0.0
      {
        state.render.set_device_scale_factor(scale_factor);
        return true;
      }
      false
    }) && let Some(host) = browser_host_get_browser_by_identifier(browser_id.inner()).and_then(|b| b.host())
    {
      host.notify_move_or_resize_started();
      host.was_resized();
      host.notify_screen_info_changed();
    }
  });
}

#[cfg(target_os = "windows")]
fn update_browser_drop_target(
  browser_id: BrowserId,
  display_info: Option<&DisplayInfo>,
  bounds: Option<iced::Rectangle>,
) {
  let (Some(display_info), Some(bounds)) = (display_info, bounds) else {
    return;
  };

  register_browser_drop_target(
    windows::Win32::Foundation::HWND(display_info.raw_handle.handle() as *mut _),
    browser_id,
    bounds,
    display_info.device_scale_factor,
  );
}

#[cfg(target_os = "macos")]
fn update_browser_drop_target(
  browser_id: BrowserId,
  display_info: Option<&DisplayInfo>,
  bounds: Option<iced::Rectangle>,
) {
  let (Some(display_info), Some(bounds)) = (display_info, bounds) else {
    return;
  };

  drop_target_mac::register_browser_drop_target(
    display_info.raw_handle.handle(),
    browser_id,
    bounds,
    display_info.device_scale_factor,
  );
}

#[cfg(target_os = "macos")]
fn unregister_browser_drop_target(browser_id: BrowserId) {
  drop_target_mac::unregister_browser_drop_target(browser_id);
}

pub struct JsInvocation {
  pub browser_id: BrowserId,
  pub context_id: String,
  pub frame_id: String,
  pub fn_name: String,
  pub params: bytes::Bytes,
  /// 可信 browser 入口提供的文档代号；普通 widget 可使用 None。
  /// 不得用 JS 请求中的同名字段替代该值。
  pub host_generation: Option<u64>,
  pub resp_tx: Arc<AtomicTake<oneshot::Sender<anyhow::Result<Bytes>>>>,
}

impl std::fmt::Debug for JsInvocation {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let mut dbg = f.debug_struct("JsInvocation");
    dbg.field("browser_id", &self.browser_id);
    dbg.field("context_id", &self.context_id);
    dbg.field("frame_id", &self.frame_id);
    dbg.field("fn_name", &self.fn_name);
    dbg.field("host_generation", &self.host_generation);

    #[cfg(not(debug_assertions))]
    dbg.field("params", &self.params.len());

    #[cfg(debug_assertions)]
    dbg.field("params", &self.params);

    dbg.field("resp_tx", &self.resp_tx).finish()
  }
}

impl JsInvocation {
  /// 构造普通 widget 的调用；受管文档代号默认 None。
  pub fn new(
    browser_id: BrowserId,
    context_id: String,
    frame_id: String,
    fn_name: String,
    params: bytes::Bytes,
    response: oneshot::Sender<anyhow::Result<Bytes>>,
  ) -> Self {
    Self {
      browser_id,
      context_id,
      frame_id,
      fn_name,
      params,
      host_generation: None,
      resp_tx: Arc::new(AtomicTake::new(response)),
    }
  }

  /// Take the response channel and wrap it as a [`ResponseTx`].
  ///
  /// Returns `None` if the channel was already taken (double-call guard).
  pub fn responder(&self) -> Option<lintel_lib_jsb_macro::ResponseTx<Bytes>> {
    let tx = self.resp_tx.take()?;
    Some(lintel_lib_jsb_macro::ResponseTx::new(self.context_id.clone(), tx))
  }
}

pub trait JsHandler {
  type Message: Send;
  fn handle(&mut self, message: Self::Message);
}

pub struct JsActor<T: JsHandler> {
  handler: T,
  rx: UnboundedReceiver<T::Message>,
}

impl<T: JsHandler> JsActor<T> {
  pub fn new(handler: T, rx: UnboundedReceiver<T::Message>) -> Self {
    Self { handler, rx }
  }

  pub async fn run(mut self) {
    while let Some(message) = self.rx.recv().await {
      self.handler.handle(message);
      tokio::task::yield_now().await;
    }
  }
}

pub struct CefBrowser {
  id: Option<BrowserId>,
  window_id: Option<window::Id>,
  browser: Option<cef::Browser>,
  focus: bool,
  last_click: Option<Click>,
  ime_opened: bool,
  preedit: String,
  last_button_modifiers: u32,
  last_cursor_position: Option<iced::Point>,
  focused_node: Option<iced::Rectangle>,
  left_button_pressing: bool,
  caret_offset: Option<f32>,
  cursor: Option<iced::mouse::Interaction>,
  view: ArcSwapOption<CefView>,
  bounds: Option<iced::Rectangle>,
  pending_view_size: Option<iced::Size>,
  pending_view_size_updated_at: Option<Instant>,
  stale_frame_fallback_logged_size: Mutex<Option<iced::Size>>,
  composition_position: Option<iced::Point>,
  is_popup: bool,
  error_info: Option<CrashInfo>,
  app_info: Option<AppInfo>,
  drag_operations_mask: cef::DragOperationsMask,
  enter_dragging: bool,
  skip_error_info: bool,
  should_clear_load_error_for_download: bool,
  hw_view: Arc<ArcSwapOption<CefView>>,
  popup_view: Arc<ArcSwapOption<CefView>>,
  loaded: bool,
  display_info: Arc<ArcSwapOption<DisplayInfo>>,
  frame_rate: Option<i32>,
  hidden_frame_rate: i32,
  last_render_request: Arc<ArcSwapOption<RenderRequestConfig>>,
  /// Unique id assigned when a browser is launched; drives subscription deduplication.
  subscription_id: Option<LaunchId>,
  /// Sender half of the subscriber handoff – passed into `launch_webview` so it can
  /// deposit the [`ClientEventSubscriber`] once the CEF browser is ready.
  subscriber_depositor: SubscriberDepositor,
  active_context_menu: ActiveContextMenu,
  context_menu_config: Option<ContextMenuConfig>,
  response_policy: NativeResponsePolicy,
}

#[derive(Clone, Copy, PartialEq)]
/// 已下发到 CEF 的渲染配置，用于避免重复执行重操作。
struct RenderRequestConfig {
  frame_rate: i32,
  bounds: Option<iced::Rectangle>,
}

#[derive(Clone)]
enum ReadyViewSelection {
  Ready(Arc<CefView>),
  CachedDuringResize(Arc<CefView>),
}

impl ReadyViewSelection {
  fn view(&self) -> &Arc<CefView> {
    match self {
      Self::Ready(view) | Self::CachedDuringResize(view) => view,
    }
  }

  fn is_cached_during_resize(&self) -> bool {
    matches!(self, Self::CachedDuringResize(_))
  }
}

fn render_mode_for_selected_view(
  requested_render_mode: WebviewRenderMode,
  selected_view: &ReadyViewSelection,
) -> WebviewRenderMode {
  if selected_view.is_cached_during_resize() {
    WebviewRenderMode::Crop
  } else {
    requested_render_mode
  }
}

fn resize_enabled_for_selected_view(selected_view: &ReadyViewSelection, hw_view: Option<&Arc<CefView>>) -> bool {
  matches!(selected_view, ReadyViewSelection::Ready(_))
    && hw_view.is_some_and(|hw_view| Arc::ptr_eq(hw_view, selected_view.view()))
}

// 可能有问题 导致异常关闭, 建议手动释放
// impl Drop for CefBrowser {
//   fn drop(&mut self) {
//     if let Some(host) = self.browser.as_mut().and_then(|browser| browser.host()) {
//       post_task_sync_ui!(|| {
//         if host.try_close_browser() == 0 {
//           tracing::info!("browser closing delayed");
//         } else {
//           tracing::info!("browser to be closed");
//         }
//       });
//     }
//   }
// }

impl CefBrowser {
  /// 丢弃当前 CEF 最新帧，等待 CEF 在下一次绘制时提供新帧。
  ///
  /// 适用于大幅扩容前的过渡场景，避免尚未完成尺寸校验的页面纹理被直接采用。
  /// 已确认可展示的上一帧会继续缓存，但 pending size 不匹配时只会在等待超时后兜底选用。
  pub fn clear_rendered_frame(&mut self) {
    self.hw_view.store(None);
  }

  fn select_ready_view(&self, hw_view: Option<Arc<CefView>>, device_scale_factor: f32) -> Option<ReadyViewSelection> {
    let ready_view = hw_view.filter(|view| self.frame_matches_pending_size(view.as_ref(), device_scale_factor));

    if let Some(view) = ready_view {
      self.view.store(Some(view.clone()));
      Some(ReadyViewSelection::Ready(view))
    } else {
      let fallback_view = self.view.load_full();
      let fallback_matches = fallback_view
        .as_ref()
        .is_some_and(|view| self.frame_matches_pending_size(view.as_ref(), device_scale_factor));

      if fallback_matches {
        fallback_view.map(ReadyViewSelection::Ready)
      } else {
        // resize 期间不显示 loading：一旦已有缓存帧，立即沿用上一帧过渡，
        // 等待新尺寸帧到达。只有从未缓存过帧（首次加载）时才返回 None，
        // 交由上层展示初始加载占位。
        let view = fallback_view?;
        self.log_resize_cached_frame_fallback_once(&view, device_scale_factor);
        Some(ReadyViewSelection::CachedDuringResize(view))
      }
    }
  }

  fn frame_matches_pending_size(&self, view: &CefView, device_scale_factor: f32) -> bool {
    self
      .pending_view_size
      .is_none_or(|size| view.matches_content_size(size, device_scale_factor))
  }

  fn log_resize_cached_frame_fallback_once(&self, view: &CefView, device_scale_factor: f32) {
    let pending_size = self.pending_view_size;
    if let Ok(mut logged_size) = self.stale_frame_fallback_logged_size.lock() {
      if *logged_size == pending_size {
        return;
      }
      *logged_size = pending_size;
    }

    let waited_ms = self
      .pending_view_size_updated_at
      .map(|updated_at| updated_at.elapsed().as_millis())
      .unwrap_or_default();
    warn!(
      browser_id = ?self.id,
      ?pending_size,
      fallback_content_size = ?view.content_size(),
      fallback_display_size = ?view.display_size(),
      fallback_frame_age_ms = view.timestamp.elapsed().as_millis(),
      waited_ms,
      device_scale_factor,
      matches_pending_size = self.frame_matches_pending_size(view, device_scale_factor),
      effective_render_mode = WebviewRenderMode::Crop.status_label(),
      resize_enabled = false,
      "webview resize using cached previous frame while waiting for matching frame"
    );
  }

  /// 预先将 CEF 渲染面调整到目标尺寸。
  ///
  /// 适用于业务侧已知即将使用的尺寸，避免等待下一轮 Iced 布局消息。
  pub fn resize_view_to_size(&mut self, size: iced::Size) {
    let Some(size) = cef_rect_size_from_size(size) else {
      self.clear_pending_view_size("invalid explicit size");
      return;
    };
    self.update_pending_view_size(size);
    if let Some(browser_id) = self.browser_id() {
      resize(browser_id, iced::Rectangle::with_size(size));
    }
  }

  pub fn sync_view_bounds(&mut self, bounds: iced::Rectangle) {
    self.record_view_bounds(bounds);

    let Some(browser_id) = self.browser_id() else {
      return;
    };
    resize(browser_id, bounds);
  }

  fn record_view_bounds(&mut self, bounds: iced::Rectangle) {
    if cef_rect_size_from_bounds(bounds).is_some() {
      self.bounds = Some(bounds);
    } else {
      trace!(
        browser_id = ?self.id,
        previous_bounds = ?self.bounds,
        bounds_x = bounds.x,
        bounds_y = bounds.y,
        bounds_width = bounds.width,
        bounds_height = bounds.height,
        "webview bounds preserved for invalid bounds"
      );
    }
    self.sync_pending_view_size_to_bounds(bounds);
  }

  // 销毁 browser 实例
  pub fn destroy(&mut self) {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    if let Some(browser_id) = self.id {
      unregister_browser_drop_target(browser_id);
      clear_resize_frame_pump_generation(browser_id);
    }
    self.display_info = Arc::new(ArcSwapOption::const_empty());
    self.view.store(None);
    self.hw_view = Arc::new(ArcSwapOption::const_empty());
    self.popup_view = Arc::new(ArcSwapOption::const_empty());
    self.app_info = None;
    self.caret_offset = None;
    self.cursor = None;
    self.focus = false;
    self.error_info = None;
    self.focused_node = None;
    self.bounds = None;
    self.clear_pending_view_size("destroy");
    self.composition_position = None;
    self.loaded = false;
    self.frame_rate = None;
    self.hidden_frame_rate = 1;
    self.last_render_request.store(None);
    if let Some(host) = self.browser.as_mut().and_then(|browser| browser.host()) {
      let browser_id = self.id;
      _ = post_task_sync_ui!(move || {
        if host.try_close_browser() == 0 {
          tracing::info!(?browser_id, "browser closing delayed");
        } else {
          tracing::info!(?browser_id, "browser to be closed");
        }
      });
    }
    self.browser = None;
    self.id = None;
    self.subscription_id = None;
    self.active_context_menu.close();
    self.context_menu_config = None;
  }
}

impl CefBrowser {
  pub fn as_webview_api(&self) -> WebviewApi {
    WebviewApi { id: self.id }
  }
}

/// Lightweight handle for calling JS and querying the renderer process.
/// Unlike `CefBrowser`, this carries only the browser identity — no UI state and
/// no cached CEF handle.
#[derive(Clone)]
pub struct WebviewApi {
  id: Option<BrowserId>,
}

impl Debug for WebviewApi {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("WebviewApi").field("id", &self.id).finish()
  }
}

impl WebviewApi {
  /// Create from a known browser id (no UI state attached).
  pub fn bare_api(id: BrowserId) -> Self {
    Self { id: Some(id) }
  }

  pub fn browser_id(&self) -> Option<BrowserId> {
    self.id
  }

  fn current_browser(&self) -> Option<cef::Browser> {
    self
      .id
      .and_then(|id| cef::browser_host_get_browser_by_identifier(id.inner()))
      .filter(|browser| browser.is_valid() == 1)
  }

  pub fn get_title(&self) -> Option<String> {
    let host = self.current_browser().and_then(|browser| browser.host())?;
    host
      .visible_navigation_entry()
      .map(|nav| CefString::from(&nav.title()).to_string())
  }

  pub fn get_url(&self) -> Option<String> {
    let frame = self.current_browser().and_then(|browser| browser.main_frame())?;
    let url = frame.url();
    Some(CefString::from(&url).to_string())
  }

  pub fn send_key_event(&self, key_event: cef::KeyEvent) {
    let Some(host) = self.current_browser().and_then(|browser| browser.host()) else {
      return;
    };
    host.send_key_event(Some(&key_event));
  }

  pub fn request_webview_focus(&self, focused: bool, reason: WebviewFocusReason) {
    let Some(browser_id) = self.id else {
      return;
    };
    self.request_ui_command(WebviewUiCommand::SetWebviewFocus {
      browser_id,
      focused,
      reason,
    });
  }

  pub fn request_ui_command(&self, command: WebviewUiCommand) {
    if self.id == Some(command.browser_id()) {
      publish_webview_ui_command(command);
    }
  }

  pub fn query_ui_state(&self) -> iced::Task<Option<WebviewUiStateSnapshot>> {
    let Some(browser_id) = self.id else {
      return iced::Task::done(None);
    };
    iced::advanced::widget::operate(WebviewUiStateOperation::new(browser_id))
  }

  pub fn focus_host(&self) {
    self.request_webview_focus(true, WebviewFocusReason::ApiRequested);
  }

  pub fn focus_and_forward_arrow_key(&self, key: &str) {
    self.focus_and_forward_arrow_key_with_reason(key, WebviewKeyReason::ApiRequested);
  }

  pub fn focus_and_forward_arrow_key_with_reason(&self, key: &str, reason: WebviewKeyReason) {
    let Some(key) = WebviewSyntheticKey::from_arrow_key(key) else {
      return;
    };
    let Some(browser_id) = self.id else {
      return;
    };
    tracing::info!(
      ?browser_id,
      ?key,
      ?reason,
      "[im-arrow-key-debug][webview-api] publish focus and forward arrow key command"
    );
    self.request_ui_command(WebviewUiCommand::FocusAndForwardSyntheticKey {
      browser_id,
      key,
      reason,
    });
  }

  pub fn forward_arrow_key(&self, key: &str) {
    self.focus_and_forward_arrow_key(key);
  }

  pub async fn send_lintel_sdk_push(&self, payload: TransferData) -> Result<TransferData, InvokeError> {
    self.invoke("_handleLintelSdkPush".into(), payload, None).await
  }

  pub async fn get_cookies(&self) -> Vec<CefCookie> {
    let Some(host) = self.current_browser().and_then(|browser| browser.host()) else {
      return Vec::new();
    };
    let Some(request_context) = host.request_context() else {
      return Vec::new();
    };
    let Some(cookie_manager) = request_context.cookie_manager(None::<&mut cef::CompletionCallback>) else {
      return Vec::new();
    };

    let (tx, rx) = oneshot::channel();
    if cookie_manager.visit_all_cookies(Some(&mut cookie::IcyCookieVisitorBuilder::build(
      IcyCookieVisitor::new(tx),
    )))
      == 0
    {
      tracing::warn!("cannot access cookies");
      return Vec::new();
    }
    rx.await.unwrap_or_default()
  }

  pub async fn delete_cookie(&self, url: &Url, cookie_name: &str) {
    if let Some(host) = self.current_browser().and_then(|browser| browser.host())
      && let Some(request_context) = host.request_context()
      && let Some(cookie_manager) = request_context.cookie_manager(None::<&mut cef::CompletionCallback>)
    {
      let (tx, rx) = oneshot::channel();
      let done = cookie_manager.delete_cookies(
        Some(&url.as_str().into()),
        Some(&cookie_name.into()),
        Some(&mut IcyDeleteCookiesCallback::build(|success| {
          if success {
            let _ = tx.send(());
          }
        })),
      ) == 1;
      if !done || rx.await.is_err() {
        tracing::warn!("cannot delete cookie");
      }
    }
  }

  pub async fn flush_cookies(&self) {
    if let Some(host) = self.current_browser().and_then(|browser| browser.host())
      && let Some(request_context) = host.request_context()
      && let Some(cookie_manager) = request_context.cookie_manager(None::<&mut cef::CompletionCallback>)
    {
      let (flush_tx, flush_rx) = oneshot::channel::<()>();
      let flushed = {
        cookie_manager.flush_store(Some(&mut IcyCompletionCallback::build(move || {
          _ = flush_tx.send(());
        })))
          == 1
      };

      if !flushed || flush_rx.await.is_err() {
        tracing::warn!(flushed, "cannot flush cookie store");
      }
    }
  }

  pub async fn set_cookie(&self, url: &Url, cookie: CefCookie) {
    if let Some(host) = self.current_browser().and_then(|browser| browser.host())
      && let Some(request_context) = host.request_context()
      && let Some(cookie_manager) = request_context.cookie_manager(None::<&mut cef::CompletionCallback>)
    {
      let browser_id = host.browser().map(|b| b.identifier());
      let (tx, rx) = oneshot::channel();
      let done = {
        #[cfg(debug_assertions)]
        tracing::info!(?browser_id, cookie_name = %cookie.name, "trying to set cookie");
        let cookie = cookie.into_raw();
        cookie_manager.set_cookie(
          Some(&url.as_str().into()),
          Some(&cookie),
          Some(&mut IcySetCookieCallback::build(|success| {
            if success {
              let _ = tx.send(());
            }
          })),
        ) == 1
      };

      if !done || rx.await.is_err() {
        tracing::warn!(done, "cannot set cookie");
      }
    }
  }

  /// 调用 js 注册方法
  /// params 到 就是侧会变成一个 jsonstring
  /// request:
  ///   - browser: rust -> serde_json -> cbor4ii_to_vec -> v8 -> ipc
  ///   - renderer: ipc -> cbor4ii_to_vec - > v8 -> json -> js
  ///
  /// response:
  ///   - renderer: js -> json -> v8 -> cbor4ii_to_vec -> ipc
  ///   - browser: ipc -> cbor4ii_to_vec -> serde_json -> rust
  /// ```rs
  /// // rs
  /// let browser = ...;
  /// #derive(Serialize)
  /// struct Res {
  ///   data: String,
  /// }
  /// browser.invoke::<Res>("fn_name".into(), "test".to_string()).await
  /// ```
  ///
  /// ```js
  /// // js
  /// __X__.register('fn_name', (params: string /* jsonstring */): string => /* jsonstring */{
  ///   const params = JSON.parse(params);
  ///   console.log(params);
  ///   return JSON.stringifys({
  ///     data: 'hello world',
  ///   })
  /// })
  /// ```
  pub async fn invoke(
    &self,
    fn_name: String,
    params: TransferData,
    timeout_dur: Option<u64>,
  ) -> anyhow::Result<TransferData, InvokeError> {
    let context_id = uuid::Uuid::new_v4().to_string();
    let result = timeout(Duration::from_millis(timeout_dur.unwrap_or(16_000)), {
      let context_id = context_id.clone();
      let fn_name = fn_name.clone();
      async move {
        let params = lintel_lib_utils::serde::cbor4ii_to_vec(params).map_err(InvokeError::Serialize)?;
        let (tx, rx) = oneshot::channel::<Bytes>();

        let mut message = cef::process_message_create(Some(&CHANNEL_INVOKE_JS.into()));

        if let Some(args) = message.as_mut().and_then(|m| m.argument_list()) {
          args.set_string(0, Some(&context_id.as_str().into()));
          args.set_string(1, Some(&fn_name.as_str().into()));
          args.set_binary(2, cef::binary_value_create(Some(&params)).as_mut());
        }

        {
          RUST_INVOKE_RESPONSE.insert(context_id, tx);
        }

        {
          let browser = self
            .current_browser()
            .ok_or_else(|| anyhow!("browser not found"))
            .map_err(InvokeError::Cef)?;
          browser
            .main_frame()
            .ok_or_else(|| anyhow!("mainFrame not found"))
            .map_err(InvokeError::Cef)?
            .send_process_message(cef::sys::cef_process_id_t::PID_RENDERER.into(), message.as_mut());
        }

        let ret = rx
          .await
          .map_err(|err| InvokeError::Biz(anyhow::anyhow!("recv error: {err}")))?;

        let ret =
          lintel_lib_utils::serde::cbor4ii_from_slice::<RustInvokeResponse>(&ret).map_err(InvokeError::Serialize)?;

        if ret.success {
          Ok(ret.data.unwrap_or_default())
        } else {
          Err(InvokeError::Biz(anyhow!("js invoke error: {:?}", ret.error)))
        }
      }
    })
    .await
    .map_err(|err| anyhow!("invoke {fn_name} timeout: {err}"))
    .map_err(InvokeError::Timeout);

    // Clean up the pending-response entry on every exit path (timeout, error,
    // or success).  On the happy path `notify_invoke_response` already removed
    // it, so this is a harmless no-op; on timeout/error it prevents the static
    // DashMap from growing unboundedly.
    RUST_INVOKE_RESPONSE.remove(&context_id);

    result?
  }

  // 返回 js 调用结果
  pub async fn notify_invoke_response(context_id: &str, res: Bytes) {
    let tx = RUST_INVOKE_RESPONSE.remove(context_id).map(|(_, v)| v);
    if let Some(tx) = tx {
      match tx.send(res) {
        Ok(_) => {}
        Err(_) => {
          tracing::error!(?context_id, "notify_invoke_response error, send failed")
        }
      }
    } else {
      tracing::error!(?context_id, "notify_invoke_response error, not found channel");
    }
  }

  pub async fn get_renderer_pid(&self) -> anyhow::Result<u32> {
    if let Some(browser_id) = self.browser_id() {
      let resp = tokio::time::timeout(
        Duration::from_millis(1_000 * 5),
        crate::webview::client::ipc_request(browser_id.inner(), "get_renderer_pid", 0)?,
      )
      .await??;
      Ok(postcard::from_bytes(&resp)?)
    } else {
      anyhow::bail!("no browser");
    }
  }

  pub fn has_focused(&self) -> bool {
    self
      .current_browser()
      .and_then(|browser| browser.focused_frame())
      .is_some()
  }
}

impl Debug for CefBrowser {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Webview").field("id", &self.id).finish()
  }
}

#[derive(Debug)]
pub enum InvokeError {
  Cef(anyhow::Error),
  Serialize(anyhow::Error),
  Biz(anyhow::Error),
  Timeout(anyhow::Error),
  Channel(anyhow::Error),
}

impl std::fmt::Display for InvokeError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      InvokeError::Cef(_) => write!(f, "Cef"),
      InvokeError::Serialize(_) => write!(f, "Serialize"),
      InvokeError::Biz(_) => write!(f, "Biz"),
      InvokeError::Timeout(_) => write!(f, "Timeout"),
      InvokeError::Channel(_) => write!(f, "Channel"),
    }
  }
}

impl CefBrowser {
  pub fn new() -> Self {
    Self {
      id: None,
      window_id: None,
      display_info: Arc::new(ArcSwapOption::const_empty()),
      preedit: String::new(),
      browser: None,
      is_popup: false,
      focus: false,
      bounds: None,
      pending_view_size: None,
      pending_view_size_updated_at: None,
      stale_frame_fallback_logged_size: Mutex::new(None),
      loaded: false,
      frame_rate: None,
      hidden_frame_rate: 1,
      last_render_request: Arc::new(ArcSwapOption::const_empty()),
      drag_operations_mask: cef::DragOperationsMask::default(),
      view: ArcSwapOption::const_empty(),
      last_click: None,
      cursor: None,
      composition_position: None,
      last_cursor_position: None,
      last_button_modifiers: 0,
      focused_node: None,
      left_button_pressing: false,
      enter_dragging: false,
      caret_offset: None,
      error_info: None,
      skip_error_info: false,
      ime_opened: false,
      hw_view: Arc::new(ArcSwapOption::const_empty()),
      popup_view: Arc::new(ArcSwapOption::const_empty()),
      should_clear_load_error_for_download: false,
      app_info: AppInfo {
        version: lintel_lib_env::LintelEnv::version(),
        did: lintel_lib_env::LintelEnv::device_id(),
      }
      .into(),
      subscription_id: None,
      subscriber_depositor: Arc::new(Mutex::new(None)),
      active_context_menu: ActiveContextMenu::default(),
      context_menu_config: None,
      response_policy: NativeResponsePolicy::default(),
    }
  }

  /// 在下一次 Create 前将响应的期限、取消和最终回复交给可信 Rust handler。
  ///
  /// handler 必须管理每个请求的所有终态；relay 不再用默认 15 秒与其竞争。
  /// 未调用此方法的普通 widget 继续使用 15 秒期限。活动/创建中的 browser 拒绝更改。
  pub fn set_managed_command_responses(&mut self) -> anyhow::Result<()> {
    anyhow::ensure!(
      self.subscription_id.is_none() && self.id.is_none() && self.browser.is_none(),
      "configure managed command responses before browser creation"
    );
    self.response_policy = NativeResponsePolicy::Managed;
    Ok(())
  }

  pub fn get_title(&self) -> Option<String> {
    self.as_webview_api().get_title()
  }

  pub fn get_url(&self) -> Option<String> {
    self.as_webview_api().get_url()
  }

  fn set_browser_id(&mut self, id: BrowserId) {
    let browser = cef::browser_host_get_browser_by_identifier(id.inner());
    self.is_popup = browser.as_ref().map(|b| b.is_popup() == 1).unwrap_or_default();
    self.id.replace(id);
    self.browser = browser;
    self.view.store(None);
    self.last_render_request.store(None);
  }

  pub fn browser_id(&self) -> Option<BrowserId> {
    self.id
  }

  fn sync_pending_view_size_to_bounds(&mut self, bounds: iced::Rectangle) {
    if let Some(size) = cef_rect_size_from_bounds(bounds) {
      self.update_pending_view_size(size);
    } else {
      trace!(
        browser_id = ?self.id,
        bounds_x = bounds.x,
        bounds_y = bounds.y,
        bounds_width = bounds.width,
        bounds_height = bounds.height,
        pending_size = ?self.pending_view_size,
        "webview pending resize target preserved for invalid bounds"
      );
    }
  }

  fn update_pending_view_size(&mut self, size: iced::Size) {
    if self.pending_view_size != Some(size) {
      let previous_size = self.pending_view_size;
      self.pending_view_size = Some(size);
      self.pending_view_size_updated_at = Some(Instant::now());
      if let Ok(mut logged_size) = self.stale_frame_fallback_logged_size.lock() {
        *logged_size = None;
      }
      trace!(
        browser_id = ?self.id,
        ?previous_size,
        pending_size = ?self.pending_view_size,
        "webview pending resize target updated"
      );
    }
  }

  fn clear_pending_view_size(&mut self, reason: &'static str) {
    if self.pending_view_size.is_none() && self.pending_view_size_updated_at.is_none() {
      return;
    }

    let previous_size = self.pending_view_size;
    self.pending_view_size = None;
    self.pending_view_size_updated_at = None;
    if let Ok(mut logged_size) = self.stale_frame_fallback_logged_size.lock() {
      *logged_size = None;
    }
    trace!(
      browser_id = ?self.id,
      ?previous_size,
      reason,
      "webview pending resize target cleared"
    );
  }

  pub fn is_popup(&self) -> bool {
    self.is_popup
  }

  pub fn execute_javascript(&mut self, script: String) {
    let Some(frame) = self.browser.as_ref().and_then(|b| b.main_frame()) else {
      return;
    };
    let url = frame.url();
    frame.execute_java_script(Some(&script.as_str().into()), Some(&cef::CefString::from(&url)), 0);
  }

  pub fn send_external_begin_frame(&self) {
    let Some(host) = self.browser.as_ref().and_then(|b| b.host()) else {
      return;
    };
    host.send_external_begin_frame();
  }

  fn set_windowless_frame_rate(&mut self, frame_rate: i32) {
    let frame_rate = clamp_windowless_frame_rate(frame_rate, get_cached_max_windowless_frame_rate());
    self.frame_rate = Some(frame_rate);
    if let Some(host) = self.browser.as_ref().and_then(|b| b.host()) {
      host.set_windowless_frame_rate(frame_rate);
    }
  }

  fn invalidate(&self) {
    let Some(host) = self.browser.as_ref().and_then(|b| b.host()) else {
      return;
    };
    host.invalidate(cef::sys::cef_paint_element_type_t::PET_VIEW.into());
  }

  pub fn request_render(&self) {
    let Some(browser_id) = self.browser_id() else {
      return;
    };
    let Some(host) = self.browser.as_ref().and_then(|b| b.host()) else {
      return;
    };

    lintel_lib_iced::ui_perf::mark_webview_interaction(browser_id.inner());

    let max_frame_rate = get_cached_max_windowless_frame_rate();
    let frame_rate = self.frame_rate.unwrap_or(max_frame_rate).clamp(1, max_frame_rate);
    let bounds = self.bounds();
    let last_request = self.last_render_request.load();
    let frame_rate_changed = last_request.as_deref().is_none_or(|last| last.frame_rate != frame_rate);
    let bounds_changed = last_request.as_deref().is_none_or(|last| last.bounds != bounds);

    if frame_rate_changed {
      host.set_windowless_frame_rate(frame_rate);
    }

    if bounds_changed && let Some(bounds) = bounds {
      resize(browser_id, bounds);
    }

    if frame_rate_changed || bounds_changed {
      self
        .last_render_request
        .store(Some(Arc::new(RenderRequestConfig { frame_rate, bounds })));
    }

    host.invalidate(cef::sys::cef_paint_element_type_t::PET_VIEW.into());
    tracing::trace!(?browser_id, ?bounds, "webview render requested");
  }

  fn update_cursor(&mut self, cursor: iced::mouse::Interaction) {
    self.cursor.replace(cursor);
  }

  pub fn load_url(&mut self, url: &Url) {
    let Some(frame) = self.browser.as_ref().and_then(|b| b.main_frame()) else {
      return;
    };
    frame.load_url(Some(&url.as_str().into()));
  }

  pub fn main_frame(&self) -> Option<cef::Frame> {
    self.browser.as_ref().and_then(|b| b.main_frame())
  }

  pub fn can_go_back(&self) -> bool {
    let Some(browser) = self.browser.as_ref() else {
      return false;
    };
    browser.can_go_back() == 1
  }

  pub fn go_back(&self) {
    let Some(browser) = self.browser.as_ref() else {
      return;
    };
    browser.go_back();
  }

  pub fn can_go_forward(&self) -> bool {
    let Some(browser) = self.browser.as_ref() else {
      return false;
    };
    browser.can_go_forward() == 1
  }
  pub fn go_forward(&self) {
    let Some(browser) = self.browser.as_ref() else {
      return;
    };
    browser.go_forward();
  }

  pub fn reload(&self) {
    let Some(browser) = self.browser.as_ref() else {
      return;
    };
    if browser.is_valid() != 1 {
      return;
    }

    browser.reload();
    if let Some(host) = browser.host() {
      host.invalidate(Default::default());
      if let Some(bounds) = self.bounds()
        && let Some(brower_id) = self.browser_id()
      {
        resize(brower_id, bounds);
      }
    }
  }

  pub fn reload_ignore_cache(&self) {
    let Some(browser) = self.browser.as_ref() else {
      return;
    };
    if browser.is_valid() != 1 {
      return;
    }

    browser.reload_ignore_cache();
    if let Some(host) = browser.host() {
      host.invalidate(Default::default());
      if let Some(bounds) = self.bounds()
        && let Some(brower_id) = self.browser_id()
      {
        resize(brower_id, bounds);
      }
    }
  }

  pub fn set_focus(&self, focus: bool) {
    let Some(browser_id) = self.browser_id() else {
      return;
    };
    publish_webview_ui_command(WebviewUiCommand::SetWebviewFocus {
      browser_id,
      focused: focus,
      reason: WebviewFocusReason::ApiRequested,
    });
  }

  pub fn has_focused(&self) -> bool {
    let Some(browser) = self.browser.as_ref() else {
      return false;
    };
    browser.focused_frame().is_some()
  }

  pub fn bounds(&self) -> Option<iced::Rectangle> {
    self.bounds
  }

  pub fn active_context_menu(&self) -> &ActiveContextMenu {
    &self.active_context_menu
  }

  pub fn has_context_menu(&self) -> bool {
    self.active_context_menu.is_open()
  }

  pub fn close_context_menu(&mut self) {
    self.active_context_menu.close();
  }

  fn focused_frame(&self) -> Option<cef::Frame> {
    self
      .browser
      .as_ref()
      .and_then(|browser| browser.focused_frame().or_else(|| browser.main_frame()))
  }

  pub async fn wait_for_focused(&self, timeout: Option<Duration>) {
    let this = self.as_webview_api();

    let task = async move {
      while !this.has_focused() {
        tokio::time::sleep(Duration::from_millis(100)).await;
      }
    };

    if let Some(dur) = timeout {
      match tokio::time::timeout(dur, task).await {
        Ok(_) => {
          // tracing::info!("focused");
        }
        Err(_) => {
          tracing::error!("focus timeout");
        }
      }
    } else {
      let _ = task.await;
    }
  }

  pub fn send_key_event(&self, key_event: cef::KeyEvent) {
    let Some(host) = self
      .browser
      .as_ref()
      .and_then(|b| (b.is_valid() == 1).then(|| b.host()).flatten())
    else {
      return;
    };
    host.send_key_event(Some(&key_event));
  }

  pub fn focus_host(&self) {
    self.set_focus(true);
  }

  pub fn focus_and_forward_arrow_key(&self, key: &str) {
    self.focus_and_forward_arrow_key_with_reason(key, WebviewKeyReason::ApiRequested);
  }

  pub fn focus_and_forward_arrow_key_with_reason(&self, key: &str, reason: WebviewKeyReason) {
    let Some(key) = WebviewSyntheticKey::from_arrow_key(key) else {
      return;
    };
    let Some(browser_id) = self.browser_id() else {
      return;
    };
    publish_webview_ui_command(WebviewUiCommand::FocusAndForwardSyntheticKey {
      browser_id,
      key,
      reason,
    });
  }

  pub fn forward_arrow_key(&self, key: &str) {
    self.focus_and_forward_arrow_key(key);
  }

  pub async fn get_cookies(&self) -> Vec<CefCookie> {
    self.as_webview_api().get_cookies().await
  }

  pub async fn delete_cookie(&self, url: &Url, cookie_name: &str) {
    self.as_webview_api().delete_cookie(url, cookie_name).await;
  }

  pub async fn flush_cookies(&self) {
    self.as_webview_api().flush_cookies().await;
  }

  pub async fn set_cookie(&self, url: &Url, cookie: CefCookie) {
    self.as_webview_api().set_cookie(url, cookie).await;
  }

  pub async fn send_lintel_sdk_push(&self, payload: TransferData) -> Result<TransferData, InvokeError> {
    self
      .as_webview_api()
      .invoke("_handleLintelSdkPush".into(), payload, None)
      .await
  }

  /// 调用 js 注册方法
  /// Delegates to [`WebviewApi::invoke`].
  pub async fn invoke(
    &self,
    fn_name: String,
    params: TransferData,
    timeout_dur: Option<u64>,
  ) -> anyhow::Result<TransferData, InvokeError> {
    self.as_webview_api().invoke(fn_name, params, timeout_dur).await
  }

  // 返回 js 调用结果
  pub async fn notify_invoke_response(&self, context_id: &str, res: Bytes) {
    WebviewApi::notify_invoke_response(context_id, res).await
  }

  pub async fn get_renderer_pid(&self) -> anyhow::Result<u32> {
    self.as_webview_api().get_renderer_pid().await
  }
}

fn adjust_windowless_frame_rate(current: i32, delta: i32) -> i32 {
  clamp_windowless_frame_rate(current + delta, get_cached_max_windowless_frame_rate())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn native_ingress_generation_legacy_constructor_does_not_trust_json_metadata() {
    let (response, _receive) = oneshot::channel();
    let invocation = JsInvocation::new(
      BrowserId::new(17),
      "context".into(),
      "frame".into(),
      "command".into(),
      TransferData::from_json(serde_json::json!({"host_generation":999,"generation":999}))
        .unwrap()
        .to_bytes()
        .unwrap(),
      response,
    );
    assert_eq!(invocation.host_generation, None);
  }

  #[test]
  fn native_response_policy_must_be_selected_before_a_browser_launch() {
    let _ = lintel_lib_env::LintelEnv::init_for_test();
    let mut legacy = CefBrowser::new();
    assert_eq!(legacy.response_policy, NativeResponsePolicy::default());
    legacy.subscription_id = Some(LaunchId::unique());
    assert!(legacy.set_managed_command_responses().is_err());
    assert_eq!(legacy.response_policy, NativeResponsePolicy::default());
    let mut managed = CefBrowser::new();
    managed.set_managed_command_responses().unwrap();
    assert_eq!(managed.response_policy, NativeResponsePolicy::Managed);
    managed.subscription_id = Some(LaunchId::unique());
    assert!(managed.set_managed_command_responses().is_err());
    assert_eq!(managed.response_policy, NativeResponsePolicy::Managed);
  }

  static FRAME_RATE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

  fn arrow_key_events_for_forwarding(key: &str) -> Option<[cef::KeyEvent; 1]> {
    let (windows_key_code, native_key_code): (u32, u32) = match key {
      "ArrowUp" => (0x26, if cfg!(target_os = "macos") { 0x7e } else { 0xe048 }),
      "ArrowDown" => (0x28, if cfg!(target_os = "macos") { 0x7d } else { 0xe050 }),
      _ => return None,
    };
    use cef::sys::*;
    Some([cef::KeyEvent {
      type_: cef_key_event_type_t::KEYEVENT_RAWKEYDOWN.into(),
      windows_key_code: windows_key_code as _,
      native_key_code: native_key_code as _,
      modifiers: cef_event_flags_t::EVENTFLAG_NONE.0 as _,
      ..Default::default()
    }])
  }

  #[test]
  fn forward_arrow_key_uses_only_raw_keydown_to_match_electron() {
    let events = arrow_key_events_for_forwarding("ArrowUp").expect("arrow up events");

    assert_eq!(events.len(), 1);
    assert_eq!(
      events[0].type_,
      cef::sys::cef_key_event_type_t::KEYEVENT_RAWKEYDOWN.into()
    );
    assert_eq!(events[0].windows_key_code, 0x26);
  }

  #[test]
  fn forward_arrow_key_rejects_unknown_key() {
    assert!(arrow_key_events_for_forwarding("Enter").is_none());
  }

  #[test]
  fn webview_bare_api_carries_only_browser_identity() {
    let browser_id = BrowserId::new(42);

    let api = WebviewApi::bare_api(browser_id);

    assert_eq!(api.browser_id(), Some(browser_id));
  }

  #[test]
  fn bounds_changed_refreshes_pending_view_size_to_actual_layout_size() {
    let _ = lintel_lib_env::LintelEnv::init_for_test();
    let browser_id = BrowserId::new(7);
    let mut browser = CefBrowser::new();
    browser.id = Some(browser_id);
    browser.pending_view_size = Some(iced::Size::new(625.0, 500.0));
    let actual_bounds = iced::Rectangle::with_size(iced::Size::new(624.0, 499.0));

    browser.update(CefMessage::BoundsChanged(browser_id, actual_bounds));

    assert_eq!(browser.bounds, Some(actual_bounds));
    assert_eq!(browser.pending_view_size, Some(actual_bounds.size()));
  }

  #[test]
  fn bounds_changed_preserves_last_valid_bounds_for_invalid_layout_size() {
    let _ = lintel_lib_env::LintelEnv::init_for_test();
    let browser_id = BrowserId::new(17);
    let mut browser = CefBrowser::new();
    browser.id = Some(browser_id);
    let valid_bounds = iced::Rectangle::new(iced::Point::new(10.0, 20.0), iced::Size::new(680.0, 520.0));

    browser.update(CefMessage::BoundsChanged(browser_id, valid_bounds));

    assert_eq!(browser.bounds, Some(valid_bounds));
    assert_eq!(browser.pending_view_size, Some(valid_bounds.size()));

    browser.update(CefMessage::BoundsChanged(
      browser_id,
      iced::Rectangle::new(iced::Point::new(0.0, 0.0), iced::Size::new(0.0, 0.0)),
    ));

    assert_eq!(browser.bounds, Some(valid_bounds));
    assert_eq!(browser.pending_view_size, Some(valid_bounds.size()));
  }

  #[test]
  fn pending_view_size_uses_cef_integer_rect_for_fractional_bounds() {
    let _ = lintel_lib_env::LintelEnv::init_for_test();
    let mut browser = CefBrowser::new();
    let bounds = iced::Rectangle::with_size(iced::Size::new(1432.4445, 760.44446));

    browser.sync_pending_view_size_to_bounds(bounds);

    assert_eq!(browser.pending_view_size, Some(iced::Size::new(1432.0, 760.0)));
  }

  #[test]
  fn invalid_bounds_preserve_pending_resize_before_restoring_same_size() {
    let _ = lintel_lib_env::LintelEnv::init_for_test();
    let mut browser = CefBrowser::new();
    let size = iced::Size::new(680.0, 520.0);
    browser.update_pending_view_size(size);
    let updated_at = Instant::now();
    browser.pending_view_size_updated_at = Some(updated_at);

    browser.sync_pending_view_size_to_bounds(iced::Rectangle::with_size(iced::Size::new(0.0, 0.0)));

    assert_eq!(browser.pending_view_size, Some(size));
    assert_eq!(browser.pending_view_size_updated_at, Some(updated_at));

    browser.sync_pending_view_size_to_bounds(iced::Rectangle::with_size(size));

    assert_eq!(browser.pending_view_size, Some(size));
    assert!(browser.pending_view_size_updated_at.is_some());
  }

  #[test]
  fn invalid_bounds_preserve_last_valid_view_bounds() {
    let _ = lintel_lib_env::LintelEnv::init_for_test();
    let mut browser = CefBrowser::new();
    let valid_bounds = iced::Rectangle::new(iced::Point::new(12.0, 24.0), iced::Size::new(680.0, 520.0));

    browser.sync_view_bounds(valid_bounds);

    assert_eq!(browser.bounds, Some(valid_bounds));
    assert_eq!(browser.pending_view_size, Some(valid_bounds.size()));

    browser.sync_view_bounds(iced::Rectangle::new(
      iced::Point::new(12.0, 24.0),
      iced::Size::new(0.0, 520.0),
    ));

    assert_eq!(browser.bounds, Some(valid_bounds));
    assert_eq!(browser.pending_view_size, Some(valid_bounds.size()));
  }

  fn software_view(browser_id: BrowserId, size: iced::Size) -> Arc<CefView> {
    Arc::new(CefView::new_software(
      browser_id,
      iced::widget::image::Handle::from_rgba(1, 1, vec![0, 0, 0, 0]),
      size,
      size,
    ))
  }

  #[test]
  fn ready_view_uses_cached_frame_during_resize_without_loading() {
    let _ = lintel_lib_env::LintelEnv::init_for_test();
    let browser_id = BrowserId::new(8);
    let mut browser = CefBrowser::new();
    let old_size = iced::Size::new(210.0, 28.0);
    let expanded_size = iced::Size::new(680.0, 520.0);

    browser.pending_view_size = Some(old_size);
    let previous_view = software_view(browser_id, old_size);
    let selected = browser
      .select_ready_view(Some(previous_view.clone()), 1.0)
      .expect("old-size frame should be ready before resize");
    assert!(matches!(selected, ReadyViewSelection::Ready(_)));
    assert!(Arc::ptr_eq(selected.view(), &previous_view));

    browser.pending_view_size = Some(expanded_size);
    browser.pending_view_size_updated_at = Some(Instant::now());
    let stale_view = software_view(browser_id, old_size);
    let selected = browser
      .select_ready_view(Some(stale_view), 1.0)
      .expect("cached previous-size frame should keep webview visible immediately during resize");
    assert!(matches!(selected, ReadyViewSelection::CachedDuringResize(_)));
    assert!(Arc::ptr_eq(selected.view(), &previous_view));
    assert_eq!(selected.view().content_size(), old_size);
    assert_eq!(selected.view().display_size(), old_size);
    assert!(!selected.view().matches_content_size(expanded_size, 1.0));

    let selected = browser
      .select_ready_view(None, 1.0)
      .expect("cached previous-size frame should be reused during resize");
    assert!(matches!(selected, ReadyViewSelection::CachedDuringResize(_)));
    assert!(Arc::ptr_eq(selected.view(), &previous_view));
    assert_eq!(selected.view().display_size(), old_size);
    assert!(!selected.view().matches_content_size(expanded_size, 1.0));

    let expanded_view = software_view(browser_id, expanded_size);
    let selected = browser
      .select_ready_view(Some(expanded_view.clone()), 1.0)
      .expect("matching expanded frame should be ready");
    assert!(matches!(selected, ReadyViewSelection::Ready(_)));
    assert!(Arc::ptr_eq(selected.view(), &expanded_view));

    let selected = browser
      .select_ready_view(None, 1.0)
      .expect("latest matching frame should be cached for the pending size");
    assert!(matches!(selected, ReadyViewSelection::Ready(_)));
    assert!(Arc::ptr_eq(selected.view(), &expanded_view));
  }

  #[test]
  fn select_ready_view_returns_none_on_initial_load_without_cached_frame() {
    let _ = lintel_lib_env::LintelEnv::init_for_test();
    let browser_id = BrowserId::new(11);
    let mut browser = CefBrowser::new();
    let pending_size = iced::Size::new(680.0, 520.0);

    // 首次加载：尚未缓存任何帧，且当前帧尺寸不匹配目标尺寸时，
    // 返回 None 交由上层展示初始加载占位，而不是复用不存在的帧。
    browser.pending_view_size = Some(pending_size);
    browser.pending_view_size_updated_at = Some(Instant::now());

    assert!(
      browser.select_ready_view(None, 1.0).is_none(),
      "initial load without a cached frame should keep showing the loading placeholder"
    );

    let stale_view = software_view(browser_id, iced::Size::new(210.0, 28.0));
    assert!(
      browser.select_ready_view(Some(stale_view), 1.0).is_none(),
      "a non-matching frame with no cached frame should not be shown on initial load"
    );
  }

  #[test]
  fn cached_resize_selection_crops_without_stretching() {
    let browser_id = BrowserId::new(9);
    let cached_size = iced::Size::new(680.0, 520.0);
    let target_size = iced::Size::new(210.0, 28.0);
    let cached_view = software_view(browser_id, cached_size);
    let selection = ReadyViewSelection::CachedDuringResize(cached_view.clone());

    let render_mode = render_mode_for_selected_view(WebviewRenderMode::Stretch, &selection);
    let render_size = render_mode_size(render_mode, cached_size, target_size);

    assert_eq!(render_mode, WebviewRenderMode::Crop);
    assert_eq!(render_size, cached_size);
    assert!(!resize_enabled_for_selected_view(&selection, Some(&cached_view)));
  }

  #[test]
  fn resize_frame_pump_outlives_observed_five_second_stall() {
    let total_delay = RESIZE_FRAME_PUMP_DELAYS.iter().copied().sum::<Duration>();

    assert!(total_delay >= Duration::from_secs(5));
  }

  #[test]
  fn resize_frame_pump_repeats_last_backoff_until_target_settles() {
    let last_delay = RESIZE_FRAME_PUMP_DELAYS.last().copied().expect("pump delay");

    assert_eq!(
      resize_frame_pump_delay(RESIZE_FRAME_PUMP_DELAYS.len()),
      Some(last_delay)
    );
    assert_eq!(
      resize_frame_pump_delay(RESIZE_FRAME_PUMP_DELAYS.len() + 100),
      Some(last_delay)
    );
  }

  #[test]
  fn ready_selection_preserves_render_mode_and_resizes_only_current_hw_view() {
    let browser_id = BrowserId::new(10);
    let ready_view = software_view(browser_id, iced::Size::new(680.0, 520.0));
    let other_view = software_view(browser_id, iced::Size::new(680.0, 520.0));
    let selection = ReadyViewSelection::Ready(ready_view.clone());

    assert_eq!(
      render_mode_for_selected_view(WebviewRenderMode::Fill, &selection),
      WebviewRenderMode::Fill
    );
    assert!(resize_enabled_for_selected_view(&selection, Some(&ready_view)));
    assert!(!resize_enabled_for_selected_view(&selection, Some(&other_view)));
    assert!(!resize_enabled_for_selected_view(&selection, None));
  }

  #[test]
  fn get_cookies_returns_empty_without_browser_id() {
    let runtime = tokio::runtime::Builder::new_current_thread()
      .enable_time()
      .build()
      .expect("tokio runtime");
    let api = WebviewApi { id: None };

    let cookies = runtime.block_on(async {
      tokio::time::timeout(std::time::Duration::from_millis(50), api.get_cookies())
        .await
        .expect("get_cookies should not wait for a CEF callback when no browser id exists")
    });

    assert!(cookies.is_empty());
  }

  #[test]
  fn clamps_frame_rate_to_cef_windowless_range() {
    assert_eq!(clamp_windowless_frame_rate(-10, 60), 1);
    assert_eq!(clamp_windowless_frame_rate(0, 60), 1);
    assert_eq!(clamp_windowless_frame_rate(30, 60), 30);
    assert_eq!(clamp_windowless_frame_rate(120, 60), 60);
    assert_eq!(clamp_windowless_frame_rate(120, 240), 120);
  }

  #[test]
  fn adjusts_frame_rate_in_fixed_steps() {
    let _guard = FRAME_RATE_TEST_LOCK.lock().expect("frame rate test lock");
    CACHED_MAX_WINDOWLESS_FRAME_RATE.store(DEFAULT_MAX_WINDOWLESS_FRAME_RATE, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(adjust_windowless_frame_rate(30, -5), 25);
    assert_eq!(adjust_windowless_frame_rate(58, 5), DEFAULT_MAX_WINDOWLESS_FRAME_RATE);
    assert_eq!(adjust_windowless_frame_rate(3, -5), 1);
  }
}

#[derive(Clone)]
struct DisplayInfo {
  window_id: window::Id,
  screen_info: Option<iced::window::ScreenInfo>,
  device_scale_factor: f32,
  raw_handle: RawHandle,
  is_gpu_rendering: bool,
}

impl Debug for DisplayInfo {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("DisplayInfo")
      .field("window_id", &self.window_id)
      .field("screen_info", &self.screen_info)
      .field("device_scale_factor", &self.device_scale_factor)
      .finish()
  }
}

#[derive(Debug, Clone)]
pub(crate) struct RawHandle(RawWindowHandle);

impl RawHandle {
  #[cfg(target_os = "windows")]
  pub(crate) fn handle(&self) -> isize {
    match self.0 {
      RawWindowHandle::Win32(handle) => handle.hwnd.get(),
      _ => unreachable!(),
    }
  }

  #[cfg(target_os = "macos")]
  pub(crate) fn handle(&self) -> std::ptr::NonNull<std::ffi::c_void> {
    match self.0 {
      RawWindowHandle::AppKit(handle) => handle.ns_view,
      _ => unreachable!(),
    }
  }
}

unsafe impl Send for RawHandle {}
unsafe impl Sync for RawHandle {}

fn get_display_info(id: window::Id) -> Task<std::result::Result<DisplayInfo, winit::raw_window_handle::HandleError>> {
  window::run(id, |window| window.window_handle().map(|h| RawHandle(h.as_raw()))).and_then(move |handle| {
    let scale_factor = window::scale_factor(id);
    let renderer_and_scale = scale_factor.map(|factor| (gpu_context().is_some(), factor));
    renderer_and_scale
      .then(move |(is_gpu_rendering, factor)| {
        window::get_screen_info(id).map(move |mut info| {
          if let Some(screen_info) = info.as_mut()
            && let Some(refresh_rate) = screen_info.refresh_rate
            && refresh_rate < 24
          {
            screen_info.refresh_rate = Some(24);
          }
          (is_gpu_rendering, factor, info)
        })
      })
      .map(move |(is_gpu_rendering, factor, screen_info)| {
        tracing::info!(is_gpu_rendering, ?screen_info, ?id);
        Ok(DisplayInfo {
          window_id: id,
          is_gpu_rendering,
          screen_info,
          device_scale_factor: factor,
          raw_handle: handle.clone(),
        })
      })
  })
}

#[derive(Debug, Clone)]
pub(crate) struct LaunchConfig {
  pub launch_id: LaunchId,
  pub subscriber_depositor: SubscriberDepositor,
  pub url: url::Url,
  pub device_scale_factor: f32,
  pub as_js_runner: bool,
  pub partition_key: Option<String>,
  pub session_path: Option<camino::Utf8PathBuf>,
  pub persist_session_cookies: bool,
  pub screen_info: Option<iced::window::ScreenInfo>,
  pub preload: PreloadType,
  pub lifecycle: Option<Arc<dyn ClientLifecyeleImpl>>,
  pub enable_safe_link: bool,

  hw_view: Arc<ArcSwapOption<CefView>>,
  popup_view: Arc<ArcSwapOption<CefView>>,
  raw_handle: RawHandle,
  is_gpu_rendering: bool,
  frame_rate: Option<i32>,
}

#[derive(Clone)]
pub struct WebviewConfig {
  pub url: url::Url,
  pub as_js_runner: bool,
  pub persist_session_cookies: bool,
  pub preload: PreloadType,
  pub lifecycle: Option<Arc<dyn ClientLifecyeleImpl>>,
  pub partition_key: Option<String>,
  pub enable_safe_link: bool,

  pub frame_rate: Option<i32>,
  pub hidden_frame_rate: i32,
  pub context_menu: Option<ContextMenuConfig>,
}

impl Debug for WebviewConfig {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("WebviewConfig")
      .field("url", &self.url)
      .field("as_js_runner", &self.as_js_runner)
      .field("partition_key", &self.partition_key)
      .field("persist_session_cookies", &self.persist_session_cookies)
      .field("preload", &self.preload)
      .field("frame_rate", &self.frame_rate)
      .field("hidden_frame_rate", &self.hidden_frame_rate)
      .field("context_menu", &self.context_menu.as_ref().map(|_| "configured"))
      .finish()
  }
}

impl Default for WebviewConfig {
  fn default() -> Self {
    Self {
      url: url::Url::parse("about:blank").unwrap(),
      as_js_runner: false,
      partition_key: None,
      persist_session_cookies: true,
      preload: PreloadType::UnKnown,
      lifecycle: None,
      enable_safe_link: false,

      frame_rate: None,
      hidden_frame_rate: 1,
      context_menu: None,
    }
  }
}

impl CefBrowser {
  fn launch_webview(config: LaunchConfig) -> Task<CefMessage> {
    // 先调用一次来初始化缓存
    let _ = get_max_windowless_frame_rate();

    let LaunchConfig {
      launch_id,
      subscriber_depositor,
      url,
      device_scale_factor,
      as_js_runner,
      partition_key,
      persist_session_cookies,
      screen_info,
      preload,
      lifecycle,
      raw_handle,
      session_path,
      hw_view,
      popup_view,
      is_gpu_rendering,
      enable_safe_link,

      frame_rate,
    } = config;
    let max_frame_rate = get_cached_max_windowless_frame_rate();
    let screen_refresh_rate = screen_info.as_ref().and_then(|i| i.refresh_rate.map(|r| r as i32));
    let frame_rate = resolve_windowless_frame_rate(frame_rate, screen_refresh_rate, max_frame_rate);
    trace!(
      ?launch_id,
      frame_rate,
      ?screen_refresh_rate,
      is_gpu_rendering,
      "webview launch configuration"
    );
    let (client, handlers) = IcyClient::new(IcyClientConfig {
      launch_id,
      view_rect: cef::Rect {
        x: 0,
        y: 0,
        width: 1,
        height: 1,
      },
      device_scale_factor,
      screen: screen_info,
      as_js_runner,
      lifecycle,
      partition_key: partition_key.clone(),
      hw_view,
      popup_view,
      #[cfg(target_os = "windows")]
      native_window: Some(raw_handle.handle()),
      #[cfg(not(target_os = "windows"))]
      native_window: None,
    });
    let IcyClient {
      state,
      subscribers,
      lifecycle,
    } = client;

    let windowinfo = cef::WindowInfo {
      windowless_rendering_enabled: true as _,
      shared_texture_enabled: shared_texture_enabled() as _,
      //external_begin_frame_enabled: true as _,
      #[cfg(target_os = "windows")]
      parent_window: cef::sys::HWND(raw_handle.handle() as *mut _),
      #[cfg(target_os = "macos")]
      parent_view: raw_handle.handle().as_ptr().cast(),
      runtime_style: cef::sys::cef_runtime_style_t::CEF_RUNTIME_STYLE_ALLOY.into(),
      #[cfg(target_os = "macos")]
      hidden: as_js_runner as _,
      ..Default::default()
    };

    let request_cache_path = super::init::request_context_cache_path(
      session_path.as_ref().map(|path| path.as_std_path()),
      &lintel_lib_env::LintelEnv::cef_user_data_path(),
    );
    let request_context_settings = RequestContextSettings {
      cache_path: request_cache_path
        .as_ref()
        .map(|path| CefString::from(path.to_string_lossy().as_ref()))
        .unwrap_or_default(),
      persist_session_cookies: persist_session_cookies as _,
      ..Default::default()
    };
    let mut context = cef::request_context_create_context(
      Some(&request_context_settings),
      Some(&mut RequestContextHandlerBuilder::build(
        IcyRequestContextHandler {},
        enable_safe_link,
        lifecycle,
      )),
    );
    if let Some(context) = context.as_ref() {
      let actual = context.cache_path();
      let raw: Option<&cef::sys::_cef_string_utf16_t> = (&actual).into();
      // A null userfree pointer is the valid empty/incognito path, not invalid UTF-16.
      let cache_path_is_empty = raw.is_none_or(|value| value.length == 0);
      tracing::info!(
        ?launch_id,
        memory_only = super::init::memory_only_contexts(),
        cache_path_is_empty,
        "CEF request context storage selected"
      );
    }

    let browser_settings = cef::BrowserSettings {
      default_encoding: "utf-8".into(),
      windowless_frame_rate: frame_rate,
      javascript_access_clipboard: cef::sys::cef_state_t::STATE_ENABLED.into(),
      javascript_dom_paste: cef::sys::cef_state_t::STATE_ENABLED.into(),
      javascript: cef::sys::cef_state_t::STATE_ENABLED.into(),
      local_storage: cef::sys::cef_state_t::STATE_ENABLED.into(),
      ..Default::default()
    };

    tracing::info!(?launch_id, "trying to create browser");

    let app_version = lintel_lib_env::LintelEnv::version();
    let dict = BrowerCreateExtraInfo::new(
      app_version.to_string(),
      partition_key.unwrap_or_default(),
      preload,
      lintel_lib_env::LintelEnv::device_id().to_string(),
    );

    let mut dict: Option<DictionaryValue> = dict.into();
    let ret = cef::browser_host_create_browser(
      Some(&windowinfo),
      Some(&mut ClientBuilder::build(handlers)),
      Some(&url.as_str().into()),
      Some(&browser_settings),
      dict.as_mut(),
      context.as_mut(),
    );
    if ret != 1 {
      tracing::error!("cannot create browser");
      return Task::none();
    }

    if let Some(state) = state {
      new_browser(launch_id, state);
    }

    // Deposit the subscriber via the oneshot sender; the iced subscription stream
    // is already awaiting this and will start delivering events immediately.
    if let Ok(mut guard) = subscriber_depositor.lock() {
      if let Some(sender) = guard.take() {
        let _ = sender.send(subscribers).inspect_err(|err| {
          tracing::error!(?err, "cannot send subscriber");
        });
      }
    }
    Task::none()
  }

  pub fn clear_browser_error_info(&mut self) {
    if let Some(info) = self.error_info.as_ref()
      && info.is_browser_failed()
    {
      self.error_info.take();
    };
  }

  pub fn update(&mut self, message: CefMessage) -> CefUpdate {
    let (action, task) = self.update_inner(message);
    CefUpdate::new(action, task)
  }

  fn update_inner(&mut self, message: CefMessage) -> (CefAction, Task<CefMessage>) {
    if !should_accept_message(self.browser_id(), self.subscription_id, &message) {
      // todo: mismatch why? need to fix.
      // error!(
      //   self_browser_id = ?self.browser_id(),
      //   self_launch_id = ?self.subscription_id,
      //   msg_browser_id = ?message.browser_id(),
      //   ?message,
      //   "browser id mismatch",
      // );
      return (CefAction::None, Task::none());
    }

    match message {
      CefMessage::ContextMenuRequested(request) => {
        let Some(config) = self.context_menu_config.as_ref() else {
          return (CefAction::None, Task::none());
        };

        let items = (config.build)(&request, &config.labels);
        self.active_context_menu = ActiveContextMenu {
          request: Some(request.clone()),
          items,
        };
        (CefAction::ContextMenuChanged(request.browser_id), Task::none())
      }
      CefMessage::ContextMenuAction { browser_id, action } => {
        let link_url = self
          .active_context_menu
          .request
          .as_ref()
          .and_then(|request| request.link_url.clone());
        let mut next_action = CefAction::ContextMenuChanged(browser_id);

        match &action {
          ContextMenuAction::Copy => {
            if let Some(frame) = self.focused_frame() {
              frame.copy();
            }
          }
          ContextMenuAction::Paste => {
            if let Some(frame) = self.focused_frame() {
              frame.paste();
            }
          }
          ContextMenuAction::Cut => {
            if let Some(frame) = self.focused_frame() {
              frame.cut();
            }
          }
          ContextMenuAction::SelectAll => {
            if let Some(frame) = self.focused_frame() {
              frame.select_all();
            }
          }
          ContextMenuAction::CopyLink => {}
          ContextMenuAction::OpenLinkInNewTab => {
            if let Some(url) = link_url.clone() {
              next_action = CefAction::CreatePopup(browser_id, url);
            }
          }
          ContextMenuAction::Custom(_) => {}
          ContextMenuAction::Separator => {}
        }

        self.close_context_menu();

        let task = match action {
          ContextMenuAction::CopyLink => {
            if let Some(url) = link_url {
              iced::clipboard::write::<CefMessage>(url).map(|_| CefMessage::None)
            } else {
              Task::none()
            }
          }
          _ => Task::none(),
        };

        (next_action, task)
      }
      CefMessage::ContextMenuClosed(browser_id) => {
        self.close_context_menu();
        (CefAction::ContextMenuChanged(browser_id), Task::none())
      }
      CefMessage::Create { window_id, config } => {
        self.window_id.replace(window_id);
        self.context_menu_config = config.context_menu.clone();
        let max_frame_rate = get_max_windowless_frame_rate();
        self.frame_rate = config
          .frame_rate
          .map(|frame_rate| clamp_windowless_frame_rate(frame_rate, max_frame_rate));
        self.hidden_frame_rate = clamp_windowless_frame_rate(config.hidden_frame_rate, max_frame_rate);
        self.close_context_menu();
        let hw_view = self.hw_view.clone();
        let popup_view = self.popup_view.clone();
        let browser_display_info = self.display_info.clone();

        // Oneshot for the subscriber handoff (launch_webview → relay task).
        let (sub_tx, sub_rx) = tokio::sync::oneshot::channel::<ClientEventSubscriber>();
        let subscriber_depositor: SubscriberDepositor = Arc::new(Mutex::new(Some(sub_tx)));

        let launch_id = LaunchId::unique();
        self.subscription_id = Some(launch_id);
        self.subscriber_depositor = subscriber_depositor.clone();

        let client_events = {
          use futures::StreamExt as _;
          let response_policy = self.response_policy;

          Task::stream(
            futures::stream::once(sub_rx)
              .filter_map(|subscriber| async move {
                subscriber
                  .inspect_err(|err| {
                    tracing::error!(?err, "cannot receive subscriber");
                  })
                  .ok()
              })
              .flat_map(move |subscriber| subscriber.into_stream(response_policy).map(|event| event.msg)),
          )
        };

        let launch_webview = get_display_info(window_id).then(move |display_info| {
          let Ok(display_info) = display_info else {
            return Task::done(CefMessage::None);
          };
          browser_display_info.swap(Some(Arc::new(display_info.clone())));
          let DisplayInfo {
            window_id,
            screen_info,
            device_scale_factor,
            raw_handle,
            is_gpu_rendering,
          } = display_info;

          let WebviewConfig {
            mut url,
            as_js_runner,
            partition_key,
            persist_session_cookies,
            preload,
            lifecycle,
            enable_safe_link,

            frame_rate,
            hidden_frame_rate: _,
            context_menu: _,
          } = config.clone();

          let session_path = partition_key
            .as_deref()
            .and_then(|key| Utf8PathBuf::from_path_buf(lintel_lib_env::LintelEnv::cef_user_data_path().join(key)).ok());

          #[cfg(debug_assertions)]
          {
            if let Some(override_url) = lintel_lib_env::webview_test_url_override() {
              url = Url::parse(&override_url).unwrap();
            }
          }
          let launch_config = LaunchConfig {
            launch_id,
            subscriber_depositor: subscriber_depositor.clone(),
            url,
            device_scale_factor,
            is_gpu_rendering,
            as_js_runner,
            partition_key,
            persist_session_cookies,
            screen_info,
            preload,
            lifecycle,
            raw_handle,
            session_path,
            enable_safe_link,

            hw_view: hw_view.clone(),
            popup_view: popup_view.clone(),
            frame_rate,
          };
          Self::launch_webview(launch_config)
        });

        (CefAction::None, Task::batch([launch_webview, client_events]))
      }
      CefMessage::CreatePopup(browser_id, target_url) => (CefAction::CreatePopup(browser_id, target_url), Task::none()),
      CefMessage::UpdateCaretOffset(browser_id, offset) => {
        if offset < 0 {
          self.caret_offset.take();
        } else {
          self.caret_offset.replace(offset as _);
        }
        (CefAction::None, Task::none())
      }
      CefMessage::FocusedNodeChanged(_browser_id, rect, is_editable) => {
        if is_editable {
          self.focused_node.replace(rect);
        } else {
          self.focused_node.take();
        }
        (CefAction::None, Task::none())
      }

      CefMessage::BoundsChanged(browser_id, bounds) => {
        let previous_bounds = self.bounds;
        trace!(
          ?browser_id,
          state_browser_id = ?self.browser_id(),
          previous_bounds = ?previous_bounds,
          bounds_x = bounds.x,
          bounds_y = bounds.y,
          bounds_width = bounds.width,
          bounds_height = bounds.height,
          "webview bounds changed"
        );
        self.record_view_bounds(bounds);
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        if let Some(browser_id) = self.browser_id() {
          update_browser_drop_target(browser_id, self.display_info.load().as_deref(), self.bounds);
        }
        self.request_render();
        (CefAction::None, Task::none())
      }
      CefMessage::CompositionPositionChanged(_, x, y) => {
        self.composition_position = Some(iced::Point::new(x, y));
        (CefAction::None, Task::none())
      }
      CefMessage::Created {
        launch_id,
        browser_id,
        js_call_rx,
      } => {
        tracing::info!(?launch_id, ?browser_id, "browser created");
        self.set_browser_id(browser_id);
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        update_browser_drop_target(browser_id, self.display_info.load().as_deref(), self.bounds);
        if let Some(frame_rate) = self.frame_rate {
          self.set_windowless_frame_rate(frame_rate);
        }
        self.invalidate();

        (CefAction::Created(browser_id, js_call_rx), Task::none())
      }
      CefMessage::Closed(browser_id) => {
        if Some(browser_id) == self.browser_id() {
          #[cfg(any(target_os = "windows", target_os = "macos"))]
          unregister_browser_drop_target(browser_id);
          self.id = None;
          self.view.store(None);
          self.browser = None;
          self.popup_view.store(None);
          self.loaded = false;
          self.subscription_id = None;
          self.close_context_menu();
          (CefAction::Closed(browser_id), Task::none())
        } else {
          (CefAction::None, Task::none())
        }
      }
      CefMessage::Loaded {
        browser_id,
        frame_id,
        http_status_code,
      } => (CefAction::Loaded(browser_id), Task::none()),
      CefMessage::LoadStart {
        browser_id,
        frame_id,
        transition_type,
      } => {
        if let Some(browser) = cef::browser_host_get_browser_by_identifier(browser_id.inner())
          && let Some(frame) = browser.frame_by_identifier(Some(&CefString::from(frame_id.as_str())))
          && frame.is_main() == 1
        {
          // 开始加载的时候重置
          self.error_info = None;
        }
        (
          CefAction::LoadStart {
            browser_id,
            frame_id,
            transition_type,
          },
          Task::none(),
        )
      }
      CefMessage::LoadChanged {
        browser_id,
        is_loading,
        can_go_forward,
        can_go_back,
      } => (
        CefAction::LoadChanged {
          browser_id,
          is_loading,
          can_go_forward,
          can_go_back,
        },
        Task::none(),
      ),
      CefMessage::LoadError {
        browser_id,
        frame_id,
        error_code,
        error_text,
        failed_url,
      } => {
        let is_main_frame = cef::browser_host_get_browser_by_identifier(browser_id.inner())
          .and_then(|browser| {
            browser
              .frame_by_identifier(Some(&CefString::from(frame_id.as_str())))
              .map(|frame| frame.is_main() == 1)
          })
          .unwrap_or(false);

        if let Some(browser) = cef::browser_host_get_browser_by_identifier(browser_id.inner())
          && let Some(frame) = browser.frame_by_identifier(Some(&CefString::from(frame_id.as_str())))
          && frame.is_main() == 1
        {
          let host_url = CefString::from(&frame.url()).to_string();
          let main_url = browser
            .main_frame()
            .map(|frame| CefString::from(&frame.url()).to_string());

          let frame_count = browser.frame_count();
          error!(failed_url, host_url, main_url, frame_count, "LoadError");
          // 主 frame 才需要显示
          let err_msg = format!("code: {error_code}, error: {error_text:?}, url: {failed_url:?}");

          // 应该是下载导致的 loaderor, 会被忽略掉
          // if !self.should_clear_load_error_for_download {
          self.error_info = Some(CrashInfo::BrowserFailed { err_msg });
          // }
        }

        (
          CefAction::LoadError {
            browser_id,
            frame_id,
            error_code,
            error_text,
            failed_url,
            is_main_frame,
          },
          Task::none(),
        )
      }
      CefMessage::TitleChanged(browser_id, title) => (CefAction::TitleChanged(browser_id, title), Task::none()),
      CefMessage::UrlChanged(browser_id, url) => (CefAction::UrlChanged(browser_id, url), Task::none()),
      CefMessage::CursorChanged(browser_id, cursor) => {
        debug_assert!(self.browser_id() == Some(browser_id));
        self.update_cursor(cursor);
        (CefAction::None, Task::none())
      }
      CefMessage::Request(request) => {
        match &request {
          RequestEvent::DocumentAvailableInMainFrame { browser_id } => {
            self.loaded = true;
            if let Some(bounds) = self.bounds {
              resize(*browser_id, bounds);
              self.invalidate();
              self.send_external_begin_frame();
            }
          }
          RequestEvent::RenderProcessTerminated {
            browser_id,
            status,
            error_code,
            error_string,
          } => match *status.as_ref() {
            cef_termination_status_t::TS_PROCESS_CRASHED => {
              self.error_info = Some(CrashInfo::Crash {
                err_msg: format!(
                  "Process Crashed, code: {error_code}, error: {:?}, url: {:?}",
                  error_string.as_ref().map(|s| s.as_str()).unwrap_or_default(),
                  self.get_url().unwrap_or_default()
                ),
              });
            }
            cef_termination_status_t::TS_PROCESS_OOM => {
              self.error_info = Some(CrashInfo::OutOfMemory {
                err_msg: format!(
                  "Renderer OOM, code: {error_code}, error: {:?}, url: {:?}",
                  error_string.as_ref().map(|s| s.as_str()).unwrap_or_default(),
                  self.get_url().unwrap_or_default()
                ),
              });
            }
            _ => {
              self.error_info = Some(CrashInfo::Terminate {
                err_msg: format!(
                  "Terminate, code: {error_code}, error: {:?}, url: {:?}",
                  error_string.as_ref().map(|s| s.as_str()).unwrap_or_default(),
                  self.get_url().unwrap_or_default()
                ),
              });
            }
          },
          _ => {}
        }
        (CefAction::Request(request), Task::none())
      }
      CefMessage::Download(e) => {
        // 如果发现 loaderror 那么应该是download导致的，忽略掉
        self.clear_browser_error_info();

        // if let &DownloadEvent::CanDownload = &e {
        //   // 开启下载，标记需要清除 loaderror
        //   self.should_clear_load_error_for_download = true;
        // }

        (CefAction::Download(e), Task::none())
      }
      CefMessage::KeyEvent(browser_id, event) => (CefAction::None, Task::none()),
      CefMessage::Rescaled(browser_id, scale_factor) => {
        rescale(browser_id, scale_factor);
        if let Some(info) = self.display_info.load().clone() {
          self.display_info.store(Some(Arc::new(DisplayInfo {
            window_id: info.window_id,
            screen_info: info.screen_info.clone(),
            raw_handle: info.raw_handle.clone(),
            is_gpu_rendering: info.is_gpu_rendering,
            device_scale_factor: scale_factor,
          })));
        }
        #[cfg(any(target_os = "windows", target_os = "macos"))]
        update_browser_drop_target(browser_id, self.display_info.load().as_deref(), self.bounds);
        (CefAction::None, Task::none())
      }
      CefMessage::DragOperation(browser_id, drag_data) => {
        if let Some(bounds) = self.bounds
          && let Some(display_info) = &*self.display_info.load()
        {
          tracing::debug!(?display_info, "start dragging");
          #[cfg(target_os = "windows")]
          if let Ok(allowed_ops) = start_dragging(
            &display_info.raw_handle,
            browser_id,
            bounds,
            display_info.device_scale_factor,
            &drag_data.data,
            drag_data.operation,
          ) {
            tracing::info!("drag and drop done");
            return (CefAction::None, Task::none());
          }
          #[cfg(target_os = "macos")]
          if let Ok(()) = drop_target_mac::start_dragging(
            &display_info.raw_handle,
            browser_id,
            bounds,
            display_info.device_scale_factor,
            &drag_data.data,
            drag_data.operation,
          ) {
            tracing::info!("macos drag session started");
            return (CefAction::None, Task::none());
          }
        }
        if let Some(host) = cef::browser_host_get_browser_by_identifier(browser_id.inner()).and_then(|b| b.host()) {
          host.drag_source_ended_at(drag_data.x, drag_data.y, drag_data.operation);
          host.drag_source_system_drag_ended();
        }

        (CefAction::None, Task::none())
      }
      CefMessage::SetFrameRate(_, frame_rate) => {
        self.set_windowless_frame_rate(frame_rate);
        self.request_render();
        (CefAction::None, Task::none())
      }

      CefMessage::BeginFrame {} => {
        self.send_external_begin_frame();
        (CefAction::None, Task::none())
      }
      CefMessage::UiCommand(command) => {
        if matches!(command, WebviewUiCommand::SetWebviewFocus { focused: false, .. }) {
          self.focused_node = None;
          self.caret_offset = None;
          self.composition_position = None;
          self.ime_opened = false;
          self.preedit.clear();
        }
        (
          CefAction::None,
          iced::advanced::widget::operate(WebviewUiCommandOperation::new(command)).discard(),
        )
      }
      CefMessage::CrashView(browser_id, message) => match message {
        crash_view::Message::Copy => {
          let Some(AppInfo { version, did }) = self.app_info.as_ref() else {
            return (CefAction::None, Task::none());
          };
          let text = format!(
            "设备ID: {} \n版本号: {} \n错误信息: {}",
            did,
            version,
            self
              .error_info
              .as_ref()
              .map(|info| info.get_err_msg())
              .unwrap_or_default()
          );

          (
            CefAction::None,
            iced::clipboard::write::<CefMessage>(text).map(|_| CefMessage::None),
          )
        }
        crash_view::Message::Reload => {
          self.reload_ignore_cache();
          (CefAction::None, Task::none())
        }
        crash_view::Message::Clear => {
          self.clear_browser_error_info();
          (CefAction::None, Task::none())
        }
      },
      CefMessage::None => (CefAction::None, Task::none()),
    }
  }

  pub fn view(&self) -> Element<'_, CefMessage> {
    self.view_with_render_mode(WebviewRenderMode::Crop)
  }

  pub fn view_with_render_mode(&self, render_mode: WebviewRenderMode) -> Element<'_, CefMessage> {
    let Some(browser_id) = self.browser_id() else {
      return Element::from(iced::widget::space());
    };

    if let Some(error_info) = &self.error_info
      && let Some(app_info) = &self.app_info
      && !self.skip_error_info
    {
      let view = CrashView::new(app_info, error_info);
      return view.view().map(move |m| CefMessage::CrashView(browser_id, m));
    }
    let popup_view = self.popup_view.load_full();
    let popup_bounds = popup_view.as_ref().and_then(|view| view.bounds);
    let popup_offset = popup_view.as_ref().and_then(|view| view.popup_offset);
    let device_scale_factor = self
      .display_info
      .load()
      .as_ref()
      .map(|info| info.device_scale_factor)
      .unwrap_or(1.0);
    let hw_view = self.hw_view.load_full();
    let ready_view = self.select_ready_view(hw_view.clone(), device_scale_factor);
    let waiting_for_frame = ready_view
      .as_ref()
      .is_none_or(ReadyViewSelection::is_cached_during_resize);

    let view = Webview::new(
      browser_id,
      if let Some(selected_view) = ready_view {
        let view = selected_view.view().clone();
        let display_size = view.display_size();
        let render_mode = render_mode_for_selected_view(render_mode, &selected_view);
        let resize_enabled = resize_enabled_for_selected_view(&selected_view, hw_view.as_ref());
        if let Some(handle) = view.software_handle() {
          iced::widget::responsive(move |size| {
            let render_size = render_mode_size(render_mode, display_size, size);
            webview_render_container(
              Image::new(handle.clone())
                .width(render_size.width)
                .height(render_size.height)
                .into(),
              render_mode,
            )
          })
          .into()
        } else {
          iced::widget::responsive(move |size| {
            let render_size = render_mode_size(render_mode, display_size, size);
            webview_render_container(
              iced::widget::shader::<CefMessage, _>(CefViewWrapper::with_resize(view.clone(), resize_enabled))
                .width(render_size.width)
                .height(render_size.height)
                .into(),
              render_mode,
            )
          })
          .into()
        }
      } else {
        Element::from(
          container(container(Circular::new()).center(Fill))
            .style(|_| iced::widget::container::Style {
              background: Some(iced::Color::WHITE.into()),
              ..Default::default()
            })
            .width(Fill)
            .height(Fill),
        )
      },
    )
    .waiting_for_frame(waiting_for_frame)
    .popup_bounds(popup_bounds)
    .popup_offset(popup_offset)
    .popup_overlay(if let Some(view) = popup_view {
      let bounds = view
        .bounds
        .unwrap_or_else(|| iced::Rectangle::new(iced::Point::ORIGIN, iced::Size::new(0.0, 0.0)));
      let popup: Element<'_, CefMessage> = if let Some(handle) = view.software_handle() {
        Image::new(handle)
          .width(bounds.width.max(0.0))
          .height(bounds.height.max(0.0))
          .into()
      } else {
        iced::widget::shader::<CefMessage, _>(CefViewWrapper::new(view))
          .width(bounds.width.max(0.0))
          .height(bounds.height.max(0.0))
          .into()
      };
      Some(
        container(iced::widget::column![
          iced::widget::Space::new().height(bounds.y.max(0.0)),
          iced::widget::row![iced::widget::Space::new().width(bounds.x.max(0.0)), popup,]
        ])
        .width(Fill)
        .height(Fill)
        .into(),
      )
    } else {
      None
    })
    .active_context_menu(
      self.has_context_menu().then_some(self.active_context_menu()),
      move |action| CefMessage::ContextMenuAction { browser_id, action },
      CefMessage::ContextMenuClosed(browser_id),
    )
    .caret_offset(self.caret_offset)
    .composition_position(self.composition_position)
    .cursor_interaction(self.cursor)
    .frame_rate(self.frame_rate.or_else(|| {
      let max_frame_rate = get_cached_max_windowless_frame_rate();
      self.display_info.load().as_ref().and_then(|d| {
        d.screen_info
          .as_ref()
          .and_then(|s| s.refresh_rate.map(|r| (r as i32).clamp(1, max_frame_rate)))
      })
    }))
    .hidden_frame_rate(self.hidden_frame_rate)
    .focused_editable_node(self.focused_node)
    .on_keyevent(move |e| CefMessage::KeyEvent(browser_id, e))
    .on_browser_view_bounds_change(move |bounds| CefMessage::BoundsChanged(browser_id, bounds))
    .on_scale_factor_change(CefMessage::Rescaled);

    #[cfg(not(debug_assertions))]
    {
      view.into()
    }
    #[cfg(debug_assertions)]
    {
      use iced::widget::{bottom_right, button, row, text, themer};

      let browser_id = self.browser_id().unwrap_or(999.into());
      let frame_rate = self.frame_rate.unwrap_or_else(get_cached_max_windowless_frame_rate);
      let mut debug_panel = row![text(format!("BrowserId: {}", browser_id.inner())).size(8)]
        .spacing(4)
        .align_y(iced::Alignment::Center);
      if crate::webview::widget::is_devtools_enabled() {
        debug_panel = debug_panel
          .push(button(text("-").size(8)).padding(5).on_press(CefMessage::SetFrameRate(
            browser_id,
            adjust_windowless_frame_rate(frame_rate, -5),
          )))
          .push(text(format!("{frame_rate} fps")).size(8))
          .push(button(text("+").size(8)).padding(5).on_press(CefMessage::SetFrameRate(
            browser_id,
            adjust_windowless_frame_rate(frame_rate, 5),
          )));
      }

      iced::widget::stack![
        view,
        themer(
          None,
          bottom_right(iced::widget::opaque(container(debug_panel).style(container::primary),)),
        )
      ]
      .into()
    }
  }

  pub fn subscription(&self) -> Subscription<CefMessage> {
    let Some(browser_id) = self.browser_id() else {
      return Subscription::none();
    };
    iced::advanced::subscription::from_recipe(WebviewUiCommandRx { browser_id })
  }
}

fn webview_render_container<'a>(
  content: Element<'a, CefMessage>,
  render_mode: WebviewRenderMode,
) -> Element<'a, CefMessage> {
  let content = container(content).clip(true).style(|_| container::Style {
    background: Some(iced::Color::WHITE.into()),
    ..Default::default()
  });

  if render_mode.center_content() {
    content.center(Fill).into()
  } else {
    content.width(Fill).height(Fill).into()
  }
}

#[derive(Debug, Clone, Copy)]
struct WebviewUiCommandRx {
  browser_id: BrowserId,
}

impl iced::advanced::subscription::Recipe for WebviewUiCommandRx {
  type Output = CefMessage;

  fn hash(&self, state: &mut iced::advanced::subscription::Hasher) {
    use std::hash::Hash;
    std::any::TypeId::of::<Self>().hash(state);
    self.browser_id.hash(state);
  }

  fn stream(
    self: Box<Self>,
    _input: std::pin::Pin<Box<dyn tokio_stream::Stream<Item = iced::advanced::subscription::Event> + Send>>,
  ) -> std::pin::Pin<Box<dyn tokio_stream::Stream<Item = CefMessage> + Send>> {
    let browser_id = self.browser_id;
    let receiver = WEBVIEW_UI_COMMAND_BUS.subscribe();
    let stream = futures::stream::unfold(receiver, move |mut receiver| async move {
      loop {
        match receiver.recv().await {
          Ok(command) if command.browser_id() == browser_id => return Some((CefMessage::UiCommand(command), receiver)),
          Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
          Err(broadcast::error::RecvError::Closed) => return None,
        }
      }
    });

    Box::pin(stream)
  }
}

#[cfg(test)]
mod render_mode_tests {
  use super::{WebviewRenderMode, render_mode_size};
  use lintel_lib_iced::iced::Size;

  #[test]
  fn render_mode_size_keeps_crop_at_frame_size() {
    let frame = Size::new(400.0, 200.0);
    let bounds = Size::new(300.0, 300.0);

    assert_eq!(render_mode_size(WebviewRenderMode::Crop, frame, bounds), frame);
  }

  #[test]
  fn render_mode_size_stretches_to_bounds() {
    let frame = Size::new(400.0, 200.0);
    let bounds = Size::new(300.0, 300.0);

    assert_eq!(render_mode_size(WebviewRenderMode::Stretch, frame, bounds), bounds);
  }

  #[test]
  fn render_mode_size_fits_inside_bounds_preserving_aspect() {
    let frame = Size::new(400.0, 200.0);
    let bounds = Size::new(300.0, 300.0);

    assert_eq!(
      render_mode_size(WebviewRenderMode::Fit, frame, bounds),
      Size::new(300.0, 150.0)
    );
  }

  #[test]
  fn render_mode_size_fills_bounds_preserving_aspect() {
    let frame = Size::new(400.0, 200.0);
    let bounds = Size::new(300.0, 300.0);

    assert_eq!(
      render_mode_size(WebviewRenderMode::Fill, frame, bounds),
      Size::new(600.0, 300.0)
    );
  }

  #[test]
  fn render_mode_size_falls_back_to_frame_for_invalid_bounds() {
    let frame = Size::new(400.0, 200.0);

    assert_eq!(
      render_mode_size(WebviewRenderMode::Fill, frame, Size::new(0.0, 300.0)),
      frame
    );
  }
}

#[cfg(all(test, target_os = "windows"))]
mod windows_tests {
  use super::{resolve_windowless_frame_rate, shared_texture_enabled};

  #[test]
  fn official_windows_keeps_cpu_paint_without_profile_feature_requirements() {
    assert!(!shared_texture_enabled());
  }

  #[test]
  fn configured_frame_rate_respects_display_cap() {
    assert_eq!(resolve_windowless_frame_rate(Some(60), Some(120), 60), 60);
    assert_eq!(resolve_windowless_frame_rate(Some(60), Some(120), 30), 30);
    assert_eq!(resolve_windowless_frame_rate(Some(1), Some(60), 30), 1);
    assert_eq!(resolve_windowless_frame_rate(None, Some(144), 60), 60);
  }
}
