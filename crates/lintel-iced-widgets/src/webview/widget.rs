use cef::{BrowserHost, CompositionUnderline, CompositionUnderlineStyle, ImplBrowser, ImplBrowserHost};
use iced::{
  Background, Border, Color, Element, Event, Length, Rectangle, Renderer, Size, Theme,
  advanced::{
    Clipboard, Shell,
    layout::{Layout, Limits, Node},
    mouse::Click,
    overlay,
    renderer::Style,
    widget::{Operation, Tree, Widget, tree},
  },
  mouse::{self, Cursor},
  touch,
  widget::{button, column, container, text},
};
use lintel_lib_iced::{self as iced, iced_core, keyboard::Modifiers};
use std::{
  collections::{BTreeMap, HashSet},
  sync::{Arc, LazyLock, Mutex},
  time::Instant,
};
use winit::event::KeyEvent;

use crate::double_pass::double_pass;
use crate::webview::instance::{
  ActiveContextMenu, ContextMenuAction, ContextMenuItem, get_cached_max_windowless_frame_rate,
  get_max_windowless_frame_rate, resize,
};

type RenderWake = Arc<dyn Fn() + Send + Sync>;

/// 合并同一 WebView 的连续帧通知，并在 present 前到达新帧时补发一次重绘。
struct RenderWaker {
  wake: Mutex<RenderWake>,
  state: Mutex<RenderWakeState>,
}

#[derive(Default)]
struct RenderWakeState {
  frame_generation: u64,
  redraw_pending: bool,
  drawn_generation: Option<u64>,
  latest_frame_produced_at: Option<Instant>,
  drawn_frame_produced_at: Option<Instant>,
}

struct PresentedFrame {
  needs_redraw: bool,
  produced_at: Option<Instant>,
}

impl RenderWaker {
  fn new(wake: RenderWake) -> Self {
    Self {
      wake: Mutex::new(wake),
      state: Mutex::new(RenderWakeState::default()),
    }
  }

  /// 更新 Iced 当前渲染周期提供的 waker，同时保留帧状态机。
  fn replace_wake(&self, wake: RenderWake) {
    *self.wake.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = wake;
  }

  fn wake(&self) {
    let wake = self
      .wake
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone();
    wake();
  }

  fn notify_frame(&self) -> bool {
    self.notify_frame_at(None)
  }

  fn notify_frame_at(&self, produced_at: Option<Instant>) -> bool {
    let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    state.frame_generation = state.frame_generation.wrapping_add(1);
    state.latest_frame_produced_at = produced_at;

    if state.redraw_pending {
      false
    } else {
      state.redraw_pending = true;
      true
    }
  }

  fn has_pending_redraw(&self) -> bool {
    self
      .state
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .redraw_pending
  }

  fn begin_redraw(&self) -> bool {
    let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !state.redraw_pending {
      return false;
    }

    state.drawn_generation = Some(state.frame_generation);
    state.drawn_frame_produced_at = state.latest_frame_produced_at;
    true
  }

  fn finish_present(&self) -> bool {
    self.finish_present_frame().is_some_and(|frame| frame.needs_redraw)
  }

  fn finish_present_frame(&self) -> Option<PresentedFrame> {
    let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(drawn_generation) = state.drawn_generation.take() else {
      return None;
    };
    let produced_at = state.drawn_frame_produced_at.take();

    if state.frame_generation == drawn_generation {
      state.redraw_pending = false;
      Some(PresentedFrame {
        needs_redraw: false,
        produced_at,
      })
    } else {
      Some(PresentedFrame {
        needs_redraw: true,
        produced_at,
      })
    }
  }
}

static WEBVIEW_WAKERS: LazyLock<Mutex<BTreeMap<BrowserId, Arc<RenderWaker>>>> = LazyLock::new(Default::default);

fn register_render_waker(browser_id: BrowserId, create_wake: impl FnOnce() -> RenderWake) {
  if let Ok(mut wakers) = WEBVIEW_WAKERS.lock() {
    let wake = create_wake();
    if let Some(waker) = wakers.get(&browser_id) {
      waker.replace_wake(wake);
    } else {
      wakers.insert(browser_id, Arc::new(RenderWaker::new(wake)));
    }
  }
}

fn unregister_render_waker(browser_id: BrowserId) {
  lintel_lib_iced::ui_perf::clear_webview_interaction(browser_id.inner());
  if let Ok(mut wakers) = WEBVIEW_WAKERS.lock() {
    wakers.remove(&browser_id);
  }
}

pub(crate) fn wake_render_waker(browser_id: BrowserId, produced_at: Option<Instant>) {
  let waker = WEBVIEW_WAKERS
    .lock()
    .ok()
    .and_then(|wakers| wakers.get(&browser_id).cloned());

  if let Some(waker) = waker
    && waker.notify_frame_at(produced_at)
  {
    waker.wake();
  }
}

pub(crate) fn has_render_waker(browser_id: BrowserId) -> bool {
  WEBVIEW_WAKERS
    .lock()
    .is_ok_and(|wakers| wakers.contains_key(&browser_id))
}

fn has_pending_render(browser_id: BrowserId) -> bool {
  WEBVIEW_WAKERS
    .lock()
    .ok()
    .and_then(|wakers| wakers.get(&browser_id).cloned())
    .is_some_and(|waker| waker.has_pending_redraw())
}

fn begin_render(browser_id: BrowserId) {
  if let Some(waker) = WEBVIEW_WAKERS
    .lock()
    .ok()
    .and_then(|wakers| wakers.get(&browser_id).cloned())
  {
    let _ = waker.begin_redraw();
  }
}

fn finish_present_frame(browser_id: BrowserId) -> Option<PresentedFrame> {
  WEBVIEW_WAKERS
    .lock()
    .ok()
    .and_then(|wakers| wakers.get(&browser_id).cloned())
    .and_then(|waker| waker.finish_present_frame())
}

#[cfg(target_os = "macos")]
fn unregister_browser_drop_target(browser_id: BrowserId) {
  crate::webview::drag_drop::drop_target_mac::unregister_browser_drop_target(browser_id);
}

#[cfg(target_os = "windows")]
fn unregister_browser_drop_target(browser_id: BrowserId) {
  crate::webview::drag_drop::unregister_browser_drop_target(browser_id);
}

#[derive(Copy, Clone, PartialEq, PartialOrd, Ord, Eq, Hash)]
pub struct BrowserId(i32);

impl std::fmt::Debug for BrowserId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}", self.0)
  }
}

impl BrowserId {
  pub fn new(id: i32) -> Self {
    BrowserId(id)
  }
  pub fn inner(&self) -> i32 {
    self.0
  }
}

impl From<i32> for BrowserId {
  fn from(id: i32) -> Self {
    BrowserId(id)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebviewFocusReason {
  ApiRequested,
  NativeSearchFocused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebviewKeyReason {
  ApiRequested,
  NativeSearchNavigation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebviewSyntheticKey {
  ArrowUp,
  ArrowDown,
}

impl WebviewSyntheticKey {
  pub fn from_arrow_key(key: &str) -> Option<Self> {
    match key {
      "ArrowUp" => Some(Self::ArrowUp),
      "ArrowDown" => Some(Self::ArrowDown),
      _ => None,
    }
  }

  pub fn to_iced_keys(&self) -> [iced::keyboard::Event; 2] {
    use iced::keyboard::{Key, key};
    match &self {
      WebviewSyntheticKey::ArrowUp => [
        iced::keyboard::Event::KeyPressed {
          key: Key::Named(key::Named::ArrowUp),
          modified_key: Key::Named(key::Named::ArrowUp),
          physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
          location: iced_core::keyboard::Location::Standard,
          modifiers: Modifiers::NONE,
          text: None,
          repeat: false,
        },
        iced::keyboard::Event::KeyReleased {
          key: Key::Named(key::Named::ArrowUp),
          modified_key: Key::Named(key::Named::ArrowUp),
          physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
          location: iced_core::keyboard::Location::Standard,
          modifiers: Modifiers::NONE,
        },
      ],
      WebviewSyntheticKey::ArrowDown => [
        iced::keyboard::Event::KeyPressed {
          key: Key::Named(key::Named::ArrowDown),
          modified_key: Key::Named(key::Named::ArrowDown),
          physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
          location: iced_core::keyboard::Location::Standard,
          modifiers: Modifiers::NONE,
          text: None,
          repeat: false,
        },
        iced::keyboard::Event::KeyReleased {
          key: Key::Named(key::Named::ArrowDown),
          modified_key: Key::Named(key::Named::ArrowDown),
          physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
          location: iced_core::keyboard::Location::Standard,
          modifiers: Modifiers::NONE,
        },
      ],
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebviewUiCommand {
  SetWebviewFocus {
    browser_id: BrowserId,
    focused: bool,
    reason: WebviewFocusReason,
  },
  FocusAndForwardSyntheticKey {
    browser_id: BrowserId,
    key: WebviewSyntheticKey,
    reason: WebviewKeyReason,
  },
}

impl WebviewUiCommand {
  pub fn browser_id(&self) -> BrowserId {
    match self {
      Self::SetWebviewFocus { browser_id, .. } | Self::FocusAndForwardSyntheticKey { browser_id, .. } => *browser_id,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebviewUiStateSnapshot {
  pub browser_id: BrowserId,
  pub webview_focused: bool,
  pub window_focused: bool,
}

#[derive(Debug)]
pub(crate) struct WebviewUiCommandOperation {
  command: WebviewUiCommand,
  applied: bool,
}

impl WebviewUiCommandOperation {
  pub(crate) fn new(command: WebviewUiCommand) -> Self {
    Self {
      command,
      applied: false,
    }
  }
}

impl Operation<bool> for WebviewUiCommandOperation {
  fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<bool>)) {
    operate(self);
  }

  fn custom(&mut self, _id: Option<&iced::widget::Id>, _bounds: Rectangle, state: &mut dyn std::any::Any) {
    let Some(state) = state.downcast_mut::<CefState>() else {
      return;
    };
    if state.browser_id != self.command.browser_id() {
      return;
    }

    state.apply_ui_command(self.command);
    self.applied = true;
  }

  fn finish(&self) -> iced::advanced::widget::operation::Outcome<bool> {
    iced::advanced::widget::operation::Outcome::Some(self.applied)
  }
}

#[derive(Debug)]
pub(crate) struct WebviewUiStateOperation {
  browser_id: BrowserId,
  snapshot: Option<WebviewUiStateSnapshot>,
}

impl WebviewUiStateOperation {
  pub(crate) fn new(browser_id: BrowserId) -> Self {
    Self {
      browser_id,
      snapshot: None,
    }
  }
}

impl Operation<Option<WebviewUiStateSnapshot>> for WebviewUiStateOperation {
  fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Option<WebviewUiStateSnapshot>>)) {
    operate(self);
  }

  fn custom(&mut self, _id: Option<&iced::widget::Id>, _bounds: Rectangle, state: &mut dyn std::any::Any) {
    let Some(state) = state.downcast_mut::<CefState>() else {
      return;
    };
    if state.browser_id != self.browser_id {
      return;
    }

    self.snapshot = Some(state.ui_state_snapshot());
  }

  fn finish(&self) -> iced::advanced::widget::operation::Outcome<Option<WebviewUiStateSnapshot>> {
    iced::advanced::widget::operation::Outcome::Some(self.snapshot)
  }
}

fn iced_keyboard_arrow_key_for_log(event: &iced::keyboard::Event) -> Option<(&'static str, &'static str)> {
  match event {
    iced::keyboard::Event::KeyPressed {
      key: iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowUp),
      ..
    } => Some(("KeyPressed", "ArrowUp")),
    iced::keyboard::Event::KeyPressed {
      key: iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowDown),
      ..
    } => Some(("KeyPressed", "ArrowDown")),
    iced::keyboard::Event::KeyReleased {
      key: iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowUp),
      ..
    } => Some(("KeyReleased", "ArrowUp")),
    iced::keyboard::Event::KeyReleased {
      key: iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowDown),
      ..
    } => Some(("KeyReleased", "ArrowDown")),
    _ => None,
  }
}

const fn arrow_key_forwarding_log_level() -> tracing::Level {
  tracing::Level::DEBUG
}

struct CefState {
  browser_id: BrowserId,
  browser_host: Option<BrowserHost>,
  cursor_in_bounds: Option<iced::Point>,
  last_cursor_position: Option<iced::Point>,
  last_button_modifiers: u32,
  last_click: Option<Click>,
  last_mouse_down_on_view: bool,
  active_touch_ids: HashSet<u64>,
  bounds: iced::Rectangle,
  webview_focused: bool,
  window_focused: bool,
  last_begin_frame_time: std::time::Instant,
  begin_frame_interval: std::time::Duration,
  ime_char_filter: ImeCharFilter,
  monitor_frame_rate: i32,
  hidden_frame_rate: i32,
}

#[derive(Debug, Default)]
struct ImeCharFilter {
  composing: bool,
  pending_commit_text: Option<String>,
  last_commit_text: Option<String>,
}

impl ImeCharFilter {
  fn on_opened(&mut self) {
    self.composing = true;
    self.pending_commit_text = None;
    self.last_commit_text = None;
  }

  fn on_preedit(&mut self, text: &str) {
    if !text.is_empty() {
      self.composing = true;
      self.pending_commit_text = None;
      self.last_commit_text = None;
    }
  }

  fn on_commit(&mut self, text: &str) -> bool {
    let should_suppress_duplicate_commit =
      !text.is_empty() && !self.composing && self.last_commit_text.as_deref() == Some(text);

    self.composing = false;
    self.pending_commit_text = (!text.is_empty()).then(|| text.to_owned());
    self.last_commit_text = (!text.is_empty()).then(|| text.to_owned());

    should_suppress_duplicate_commit
  }

  fn on_closed(&mut self) {
    self.composing = false;
    self.pending_commit_text = None;
    self.last_commit_text = None;
  }

  fn pending_commit_text_for_log(&self) -> Option<String> {
    self.pending_commit_text.as_deref().map(ime_text_for_log)
  }

  fn last_commit_text_for_log(&self) -> Option<String> {
    self.last_commit_text.as_deref().map(ime_text_for_log)
  }

  fn should_suppress_key_char(&mut self, text: Option<&iced_core::SmolStr>) -> bool {
    let Some(text) = text else {
      if !self.composing {
        self.pending_commit_text = None;
      }
      return false;
    };
    let text = text.as_str();
    if text.is_empty() {
      if !self.composing {
        self.pending_commit_text = None;
      }
      return false;
    }

    if self.composing {
      self.pending_commit_text = None;
      return true;
    }

    let Some(committed_text) = self.pending_commit_text.take() else {
      return false;
    };

    let Some(remaining_text) = committed_text.strip_prefix(text) else {
      return false;
    };

    if !remaining_text.is_empty() {
      self.pending_commit_text = Some(remaining_text.to_owned());
    }
    true
  }
}

fn ime_text_for_log(text: &str) -> String {
  const MAX_CHARS: usize = 32;

  let mut truncated = String::new();
  let mut chars = text.chars();
  for _ in 0..MAX_CHARS {
    let Some(ch) = chars.next() else {
      return truncated;
    };
    truncated.push(ch);
  }

  if chars.next().is_some() {
    truncated.push_str("...");
  }
  truncated
}

fn should_filter_win7_ime_on_current_os() -> bool {
  #[cfg(not(target_os = "windows"))]
  {
    false
  }

  #[cfg(target_os = "windows")]
  {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
      let os_info = os_info::get();
      let enabled = should_filter_win7_ime_for_os(os_info.os_type(), os_info.version(), os_info.edition());
      tracing::info!(
        os_type = ?os_info.os_type(),
        os_version = %os_info.version(),
        os_edition = ?os_info.edition(),
        enabled,
        "win7 ime filter evaluated"
      );
      enabled
    })
  }
}

#[cfg(any(target_os = "windows", test))]
fn should_filter_win7_ime_for_os(os_type: os_info::Type, version: &os_info::Version, edition: Option<&str>) -> bool {
  if os_type != os_info::Type::Windows {
    return false;
  }

  if let Some(edition) = edition {
    return edition.contains("Windows 7");
  }

  matches!(version, os_info::Version::Semantic(6, 1, _))
}

struct ContextMenuOverlayState<'a, Message> {
  anchor: iced::Point,
  close_message: Option<Message>,
  content: Element<'a, Message, Theme, Renderer>,
}

impl CefState {
  fn browser_host(browser_id: BrowserId) -> Option<BrowserHost> {
    cef::browser_host_get_browser_by_identifier(browser_id.inner())
      .and_then(|browser| (browser.is_valid() == 1).then(|| browser.host()).flatten())
  }

  fn hide_browser(&self) {
    if let Some(host) = &self.browser_host {
      host.send_capture_lost_event();
      // cannot hide due to cef bug: https://github.com/chromiumembedded/cef/issues/2483#3427/osr-rendering-bug-when-minimizing-and
      // host.was_hidden(true as _);
      host.set_windowless_frame_rate(self.hidden_frame_rate.clamp(1, get_cached_max_windowless_frame_rate()));
    }
  }

  fn show_browser(&self, bounds: iced::Rectangle) {
    if let Some(host) = &self.browser_host {
      //host.was_hidden(false as _);
      let max_frame_rate = get_cached_max_windowless_frame_rate();
      host.set_windowless_frame_rate(self.monitor_frame_rate.clamp(1, max_frame_rate));
      resize(self.browser_id, bounds);
      host.invalidate(Default::default());
      host.send_external_begin_frame();
    }
  }

  fn should_begin_frame(&self) -> bool {
    if self.webview_focused && self.window_focused {
      true
    } else {
      self.last_begin_frame_time.elapsed() >= self.begin_frame_interval
    }
  }

  fn send_begin_frame(&mut self) {
    if let Some(host) = &self.browser_host {
      host.send_external_begin_frame();
      self.last_begin_frame_time = std::time::Instant::now();
    }
  }

  fn try_send_begin_frame(&mut self) {
    if self.should_begin_frame() {
      self.send_begin_frame();
    }
  }

  fn update_browser(&mut self, browser_id: BrowserId, bounds: iced::Rectangle) -> bool {
    let browser_switched = self.browser_id != browser_id;
    if browser_switched {
      self.hide_browser();
      #[cfg(any(target_os = "windows", target_os = "macos"))]
      unregister_browser_drop_target(self.browser_id);
      unregister_render_waker(self.browser_id);

      self.browser_id = browser_id;
      self.browser_host = Self::browser_host(self.browser_id);
      self.show_browser(bounds);

      self.last_click = None;
      self.last_button_modifiers = 0;
      self.last_cursor_position = None;
      self.last_mouse_down_on_view = false;
      self.active_touch_ids.clear();
      self.cursor_in_bounds = None;
    }
    browser_switched
  }

  fn is_over_popup_widget(&self, point: iced::Point, popup_bounds: Option<iced::Rectangle>) -> bool {
    popup_bounds.is_some_and(|popup| {
      point.x >= popup.x && point.x < popup.x + popup.width && point.y >= popup.y && point.y < popup.y + popup.height
    })
  }

  fn update_focus(&mut self, in_bounds: bool, _default_frame_rate: Option<i32>) {
    if self.webview_focused != in_bounds {
      self.webview_focused = in_bounds;
      self.sync_browser_host_focus();
      let _frame_rate = if self.webview_focused && self.window_focused {
        30
      } else {
        24
      };
      //host.set_windowless_frame_rate(frame_rate);
    }
  }

  fn update_window_focus(&mut self, focused: bool) {
    self.window_focused = focused;
    self.sync_browser_host_focus();
  }

  fn sync_browser_host_focus(&mut self) {
    if let Some(host) = self.browser_host.as_mut() {
      host.set_focus((self.webview_focused && self.window_focused) as _);
    }
  }

  fn ui_state_snapshot(&self) -> WebviewUiStateSnapshot {
    WebviewUiStateSnapshot {
      browser_id: self.browser_id,
      webview_focused: self.webview_focused,
      window_focused: self.window_focused,
    }
  }

  fn set_webview_focus_from_command(&mut self, focused: bool) {
    if self.webview_focused == focused {
      self.sync_browser_host_focus();
      return;
    }

    self.webview_focused = focused;
    if !focused {
      self.ime_char_filter.on_closed();
      self.last_button_modifiers = 0;
      self.last_mouse_down_on_view = false;
    }

    self.sync_browser_host_focus();
    if !focused {
      if let Some(host) = self.browser_host.as_mut() {
        host.ime_cancel_composition();
      }
    }
  }

  fn focus_and_forward_synthetic_key_from_command(&mut self, key: WebviewSyntheticKey, reason: WebviewKeyReason) {
    let webview_focused_before = self.webview_focused;
    let window_focused_before = self.window_focused;
    if matches!(reason, WebviewKeyReason::NativeSearchNavigation) {
      self.window_focused = true;
    }
    self.set_webview_focus_from_command(true);

    tracing::info!(
      browser_id = ?self.browser_id,
      ?key,
      ?reason,
      webview_focused_before,
      webview_focused_after = self.webview_focused,
      window_focused_before,
      window_focused_after = self.window_focused,
      "[im-arrow-key-debug][webview-widget] focus and forward synthetic key command"
    );
    let Some(host) = self.browser_host.as_mut() else {
      tracing::info!(
        browser_id = ?self.browser_id,
        ?key,
        webview_focused = self.webview_focused,
        window_focused = self.window_focused,
        "[im-arrow-key-debug][webview-widget] skip synthetic key because browser host is missing"
      );
      return;
    };
    host.set_focus(true as _);
    for event in key.to_iced_keys() {
      self.send_key_event(&event);
    }
    self.sync_browser_host_focus();
  }

  fn apply_ui_command(&mut self, command: WebviewUiCommand) {
    match command {
      WebviewUiCommand::SetWebviewFocus { focused, .. } => {
        self.set_webview_focus_from_command(focused);
      }
      WebviewUiCommand::FocusAndForwardSyntheticKey { key, reason, .. } => {
        self.focus_and_forward_synthetic_key_from_command(key, reason);
      }
    }
  }

  fn send_ime_event(&mut self, event: &iced_core::input_method::Event) {
    let Some(host) = &self.browser_host else {
      return;
    };
    use iced_core::input_method::Event;

    match event {
      Event::Opened => {
        self.ime_char_filter.on_opened();
      }
      Event::Preedit(text, range) => {
        self.ime_char_filter.on_preedit(text);
        // 空 preedit 表示当前组合被清空，需要取消组合，避免上一次的 preedit 残留。
        if text.is_empty() {
          host.ime_cancel_composition();
        } else {
          // CEF / Chromium 的 IME range 使用 UTF-16 code unit 偏移；
          // iced 的 range 是 UTF-8 字节偏移，需要做一次转换。
          let utf16_len = |byte_offset: usize| -> u32 {
            let clamped = byte_offset.min(text.len());
            text
              .get(..clamped)
              .map(|s| s.encode_utf16().count())
              .unwrap_or_else(|| text.encode_utf16().count()) as u32
          };

          // selection_range 在 CEF 中代表「组合串内被选中/高亮的区间」。
          // 当 from == to 时只显示光标（与浏览器原生 IME 一致），
          // 不能传整段范围，否则整串会按 selection 渲染成蓝色背景。
          let cef_selection_range = match range {
            Some(r) => cef::Range {
              from: utf16_len(r.start),
              to: utf16_len(r.end),
            },
            None => {
              let end = utf16_len(text.len());
              cef::Range { from: end, to: end }
            }
          };
          // underline.range 与 selection_range 含义不同：前者是「组合串里要画下划线的区间」，
          // 必须覆盖整段 preedit；后者是 caret/选区位置（from == to 表示仅光标）。
          // 之前误把 selection_range 复用给 underline.range，长度为 0 导致下划线不可见。
          let underline_range = cef::Range {
            from: 0,
            to: utf16_len(text.len()),
          };
          let underline = CompositionUnderline {
            range: underline_range,
            style: CompositionUnderlineStyle::SOLID,
            thick: 0,
            color: 0,
            background_color: 0,
            ..Default::default()
          };

          host.ime_set_composition(
            Some(&text.as_str().into()),
            Some(&[underline]),
            Some(&cef::Range {
              from: u32::MAX,
              to: u32::MAX,
            }),
            Some(&cef_selection_range),
          );
        }
      }
      Event::Commit(text) => {
        let win7_ime_filter_enabled = should_filter_win7_ime_on_current_os();
        let should_suppress_commit = self.ime_char_filter.on_commit(text) && win7_ime_filter_enabled;
        if should_suppress_commit {
          tracing::info!(
            browser_id = ?self.browser_id,
            text = %ime_text_for_log(text),
            text_len_chars = text.chars().count(),
            text_len_bytes = text.len(),
            ime_composing = self.ime_char_filter.composing,
            pending_commit_text = ?self.ime_char_filter.pending_commit_text_for_log(),
            last_commit_text = ?self.ime_char_filter.last_commit_text_for_log(),
            "win7 ime duplicate commit suppressed"
          );
          return;
        }
        host.ime_commit_text(
          Some(&text.as_str().into()),
          Some(&cef::Range {
            from: u32::MAX,
            to: u32::MAX,
          }),
          0,
        );
      }
      Event::Closed => {
        self.ime_char_filter.on_closed();
        host.ime_finish_composing_text(true as _);
      }
    }
  }

  fn send_mouse_event(
    &mut self,
    cursor: Option<iced::Point>,
    relative_cursor: Option<iced::Point>,
    popup_bounds: Option<iced::Rectangle>,
    popup_offset: Option<iced::Vector>,
    event: iced::mouse::Event,
  ) {
    let Some(host) = &self.browser_host else {
      return;
    };
    let inside_bounds = relative_cursor.is_some();
    use iced::advanced::mouse::click::Kind;
    let Some(point) = relative_cursor.or(cursor) else {
      return;
    };
    let point_for_cef = if self.is_over_popup_widget(point, popup_bounds) {
      if let Some(offset) = popup_offset {
        iced::Point::new(point.x + offset.x, point.y + offset.y)
      } else {
        point
      }
    } else {
      point
    };
    self.last_cursor_position = cursor;
    match event {
      iced::mouse::Event::ButtonPressed(button) => {
        host.set_focus(inside_bounds as _);
        self.last_mouse_down_on_view = !self.is_over_popup_widget(point, popup_bounds);
        if inside_bounds {
          let previous = self.last_click.take();
          self.last_click.replace(Click::new(point, button, previous));
        }

        let (modifier, type_) = match button {
          iced::mouse::Button::Left => (
            cef::sys::cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON,
            cef::sys::cef_mouse_button_type_t::MBT_LEFT,
          ),
          iced::mouse::Button::Right => (
            cef::sys::cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON,
            cef::sys::cef_mouse_button_type_t::MBT_RIGHT,
          ),
          iced::mouse::Button::Middle => (
            cef::sys::cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON,
            cef::sys::cef_mouse_button_type_t::MBT_MIDDLE,
          ),
          _ => return,
        };

        let event = cef::MouseEvent {
          x: point_for_cef.x as _,
          y: point_for_cef.y as _,
          modifiers: modifier.0 as u32,
        };
        let count = self
          .last_click
          .map(|c| match c.kind() {
            Kind::Single => 1,
            Kind::Double => 2,
            Kind::Triple => 3,
          })
          .unwrap_or(1);
        host.send_mouse_click_event(Some(&event), type_.into(), false as _, count);
        if inside_bounds {
          self.last_button_modifiers = modifier.0 as u32;
        }
      }
      iced::mouse::Event::ButtonReleased(button) => {
        let (modifier, type_) = match button {
          iced::mouse::Button::Left => (
            cef::sys::cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON,
            cef::sys::cef_mouse_button_type_t::MBT_LEFT,
          ),
          iced::mouse::Button::Right => (
            cef::sys::cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON,
            cef::sys::cef_mouse_button_type_t::MBT_RIGHT,
          ),
          iced::mouse::Button::Middle => (
            cef::sys::cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON,
            cef::sys::cef_mouse_button_type_t::MBT_MIDDLE,
          ),
          _ => return,
        };
        if self.last_mouse_down_on_view
          && self.is_over_popup_widget(point, popup_bounds)
          && popup_bounds.is_some_and(|popup| popup.x != 0.0 || popup.y != 0.0)
        {
          self.last_button_modifiers = 0;
          return;
        }
        self.last_button_modifiers = 0;
        let event = cef::MouseEvent {
          x: point_for_cef.x as _,
          y: point_for_cef.y as _,
          modifiers: modifier.0 as u32,
        };
        host.send_mouse_click_event(Some(&event), type_.into(), true as _, 1);
      }
      iced::mouse::Event::WheelScrolled { delta } => {
        let Some(point) = relative_cursor else {
          return;
        };
        let mut event = cef::MouseEvent {
          x: point_for_cef.x as _,
          y: point_for_cef.y as _,
          modifiers: 0,
        };
        match delta {
          iced::mouse::ScrollDelta::Lines { x, y } => {
            let x = x * 40.0 * 3.0;
            let y = y * 40.0 * 3.0;
            event.modifiers |= cef::sys::cef_event_flags_t::EVENTFLAG_PRECISION_SCROLLING_DELTA.0 as u32;
            host.send_mouse_wheel_event(Some(&event), x as _, y as _);
          }
          iced::mouse::ScrollDelta::Pixels { x, y } => {
            let x = x * 3.0;
            let y = y * 3.0;
            event.modifiers |= cef::sys::cef_event_flags_t::EVENTFLAG_SCROLL_BY_PAGE.0 as u32;
            host.send_mouse_wheel_event(Some(&event), x as _, y as _);
          }
        }
      }
      iced::mouse::Event::CursorMoved { position } => {
        let position = relative_cursor.unwrap_or(position);
        let event = cef::MouseEvent {
          x: if self.is_over_popup_widget(position, popup_bounds) {
            popup_offset.map(|offset| position.x + offset.x).unwrap_or(position.x)
          } else {
            position.x
          } as _,
          y: if self.is_over_popup_widget(position, popup_bounds) {
            popup_offset.map(|offset| position.y + offset.y).unwrap_or(position.y)
          } else {
            position.y
          } as _,
          modifiers: self.last_button_modifiers,
        };
        let mouse_leave = !inside_bounds;
        host.send_mouse_move_event(Some(&event), mouse_leave as _);
      }
      _ => {}
    }
  }

  fn send_key_event(&mut self, event: &iced::keyboard::Event) {
    let Some(host) = &self.browser_host else {
      return;
    };
    use super::convert::*;

    let mut key_press_text = None;
    let cef_event = match event {
      iced::keyboard::Event::KeyPressed {
        key,
        modified_key,
        physical_key,
        modifiers,
        text,
        location,
        ..
      } => {
        key_press_text = Some(text.as_ref());
        iced_key_to_cef_key(
          KeyPress::Press,
          Some(modified_key),
          Some(key),
          physical_key.into(),
          modifiers,
          text,
          *location,
        )
      }
      iced::keyboard::Event::KeyReleased {
        key,
        modified_key,
        physical_key,
        modifiers,
        location,
        ..
      } => iced_key_to_cef_key(
        KeyPress::UnPress,
        Some(modified_key),
        Some(key),
        physical_key.into(),
        modifiers,
        &None,
        *location,
      ),
      iced::keyboard::Event::ModifiersChanged(_) => None,
    };
    let arrow_key_for_log = iced_keyboard_arrow_key_for_log(event);
    if let Some((mut event, should_send_char)) = cef_event {
      if let Some((iced_event_type, key)) = arrow_key_for_log {
        tracing::event!(
          arrow_key_forwarding_log_level(),
          browser_id = ?self.browser_id,
          key,
          iced_event_type,
          webview_focused = self.webview_focused,
          window_focused = self.window_focused,
          should_send_char,
          cef_windows_key_code = event.windows_key_code,
          cef_native_key_code = event.native_key_code,
          cef_modifiers = event.modifiers,
          "[im-arrow-key-debug][webview-widget] send iced keyboard event to cef"
        );
      }
      host.send_key_event(Some(&event));
      let win7_ime_filter_enabled = should_filter_win7_ime_on_current_os();
      let key_text_for_log = key_press_text
        .flatten()
        .map(|text| (ime_text_for_log(text.as_str()), text.chars().count(), text.len()));
      let ime_composing_before = self.ime_char_filter.composing;
      let pending_commit_text_before = self.ime_char_filter.pending_commit_text_for_log();
      let should_suppress_char = key_press_text
        .map(|text| win7_ime_filter_enabled && self.ime_char_filter.should_suppress_key_char(text))
        .unwrap_or(false);
      if should_suppress_char {
        tracing::info!(
          browser_id = ?self.browser_id,
          key_text = ?key_text_for_log.as_ref().map(|(text, _, _)| text),
          key_text_len_chars = ?key_text_for_log.as_ref().map(|(_, chars, _)| chars),
          key_text_len_bytes = ?key_text_for_log.as_ref().map(|(_, _, bytes)| bytes),
          should_send_char,
          cef_character = event.character,
          cef_windows_key_code = event.windows_key_code,
          ime_composing_before,
          pending_commit_text_before = ?pending_commit_text_before,
          ime_composing_after = self.ime_char_filter.composing,
          pending_commit_text_after = ?self.ime_char_filter.pending_commit_text_for_log(),
          "win7 ime key char suppressed"
        );
      }
      if should_send_char && !should_suppress_char {
        event.type_ = cef::sys::cef_key_event_type_t::KEYEVENT_CHAR.into();
        #[cfg(target_os = "windows")]
        {
          event.windows_key_code = event.character as _;
        }
        host.send_key_event(Some(&event));
      }
    }
  }

  fn send_touch_event(
    &mut self,
    bounds: iced::Rectangle,
    popup_bounds: Option<iced::Rectangle>,
    popup_offset: Option<iced::Vector>,
    event: &touch::Event,
  ) -> bool {
    let Some(host) = &self.browser_host else {
      return false;
    };

    let (id, position, type_, pressure, track_active, remove_active) = match event {
      touch::Event::FingerPressed { id, position } => (id.0, *position, cef::TouchEventType::PRESSED, 1.0, true, false),
      touch::Event::FingerMoved { id, position } => (id.0, *position, cef::TouchEventType::MOVED, 1.0, false, false),
      touch::Event::FingerLifted { id, position } => (id.0, *position, cef::TouchEventType::RELEASED, 0.0, false, true),
      touch::Event::FingerLost { id, position } => (id.0, *position, cef::TouchEventType::CANCELLED, 0.0, false, true),
    };

    let cef_id = i32::try_from(id.min(i32::MAX as u64)).unwrap_or(i32::MAX);
    let active_id = cef_id as u64;
    let point = iced::Point::new(position.x - bounds.x, position.y - bounds.y);
    let inside_bounds = point.x >= 0.0 && point.x < bounds.width && point.y >= 0.0 && point.y < bounds.height;
    let was_active = self.active_touch_ids.contains(&active_id);

    if !inside_bounds && !was_active {
      return false;
    }

    let point_for_cef = if self.is_over_popup_widget(point, popup_bounds) {
      if let Some(offset) = popup_offset {
        iced::Point::new(point.x + offset.x, point.y + offset.y)
      } else {
        point
      }
    } else {
      point
    };

    if track_active {
      self.active_touch_ids.insert(active_id);
      host.set_focus(true as _);
    }

    host.send_touch_event(Some(&cef::TouchEvent {
      id: cef_id,
      x: point_for_cef.x,
      y: point_for_cef.y,
      radius_x: 0.0,
      radius_y: 0.0,
      rotation_angle: 0.0,
      pressure,
      type_,
      modifiers: 0,
      pointer_type: cef::PointerType::TOUCH,
    }));

    if remove_active {
      self.active_touch_ids.remove(&active_id);
    }

    true
  }
}

impl Drop for CefState {
  fn drop(&mut self) {
    self.hide_browser();
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    unregister_browser_drop_target(self.browser_id);
    unregister_render_waker(self.browser_id);
  }
}

fn should_forward_keyboard_event_to_browser(
  webview_focused: bool,
  window_focused: bool,
  event_already_captured: bool,
) -> bool {
  webview_focused && window_focused && !event_already_captured
}

pub struct Webview<'a, Message> {
  browser_id: BrowserId,
  content: Element<'a, Message, Theme, Renderer>,
  waiting_for_frame: bool,
  popup_overlay: Option<Element<'a, Message, Theme, Renderer>>,
  popup_bounds: Option<iced::Rectangle>,
  popup_offset: Option<iced::Vector>,
  context_menu_overlay: Option<ContextMenuOverlayState<'a, Message>>,
  focused_editable_node: Option<iced::Rectangle>,
  caret_offset: Option<f32>,
  composition_position: Option<iced::Point>,
  cursor: Option<iced::mouse::Interaction>,
  frame_rate: Option<i32>,
  hidden_frame_rate: i32,
  on_keyevent: Option<Box<dyn Fn(iced::keyboard::Event) -> Message + 'a>>,
  on_browser_view_bounds_change: Option<Box<dyn Fn(iced::Rectangle) -> Message + 'a>>,
  on_scale_factor_change: Option<Box<dyn Fn(BrowserId, f32) -> Message + 'a>>,
}

impl<'a, Message: 'a> Webview<'a, Message> {
  pub fn new(browser_id: BrowserId, content: impl Into<Element<'a, Message, Theme, Renderer>>) -> Self {
    Webview {
      browser_id,
      content: content.into(),
      waiting_for_frame: false,
      popup_overlay: None,
      popup_bounds: None,
      popup_offset: None,
      context_menu_overlay: None,
      focused_editable_node: None,
      caret_offset: None,
      composition_position: None,
      cursor: None,
      frame_rate: None,
      hidden_frame_rate: 1,
      on_keyevent: None,
      on_browser_view_bounds_change: None,
      on_scale_factor_change: None,
    }
  }

  pub(crate) fn waiting_for_frame(mut self, waiting: bool) -> Self {
    self.waiting_for_frame = waiting;
    self
  }

  pub fn on_keyevent(mut self, on_keyevent: impl Fn(iced::keyboard::Event) -> Message + 'a) -> Self {
    self.on_keyevent = Some(Box::new(on_keyevent));
    self
  }

  pub fn on_browser_view_bounds_change(
    mut self,
    on_browser_view_change: impl Fn(iced::Rectangle) -> Message + 'a,
  ) -> Self {
    self.on_browser_view_bounds_change = Some(Box::new(on_browser_view_change));
    self
  }

  pub fn on_scale_factor_change(mut self, on_scale_factor_change: impl Fn(BrowserId, f32) -> Message + 'a) -> Self {
    self.on_scale_factor_change = Some(Box::new(on_scale_factor_change));
    self
  }

  pub fn popup_overlay(mut self, popup_overlay: Option<Element<'a, Message, Theme, Renderer>>) -> Self {
    self.popup_overlay = popup_overlay;
    self
  }

  pub fn popup_bounds(mut self, popup_bounds: Option<iced::Rectangle>) -> Self {
    self.popup_bounds = popup_bounds;
    self
  }

  pub fn popup_offset(mut self, popup_offset: Option<iced::Vector>) -> Self {
    self.popup_offset = popup_offset;
    self
  }

  pub fn active_context_menu(
    mut self,
    menu: Option<&ActiveContextMenu>,
    on_action: impl Fn(ContextMenuAction) -> Message,
    on_close: Message,
  ) -> Self
  where
    Message: Clone,
  {
    self.context_menu_overlay = menu.and_then(|menu| Self::build_context_menu_overlay(menu, &on_action, on_close));
    self
  }

  pub fn cursor_interaction(mut self, cursor: Option<iced::mouse::Interaction>) -> Self {
    self.cursor = cursor;
    self
  }

  pub fn frame_rate(mut self, frame_rate: Option<i32>) -> Self {
    self.frame_rate = frame_rate;
    self
  }

  pub fn hidden_frame_rate(mut self, hidden_frame_rate: i32) -> Self {
    self.hidden_frame_rate = hidden_frame_rate;
    self
  }

  pub fn focused_editable_node(mut self, node: Option<iced::Rectangle>) -> Self {
    self.focused_editable_node = node;
    self
  }

  pub fn caret_offset(mut self, offset: Option<f32>) -> Self {
    self.caret_offset = offset;
    self
  }

  pub fn composition_position(mut self, pos: Option<iced::Point>) -> Self {
    self.composition_position = pos;
    self
  }

  fn input_method(&self, cursor: Option<iced::Point>, bound: iced::Rectangle) -> iced_core::InputMethod {
    let Some(node) = self.focused_editable_node else {
      return iced_core::input_method::InputMethod::Disabled;
    };
    if let Some(node) = self.composition_position.or(Some(node.position())) {
      return iced_core::input_method::InputMethod::Enabled {
        cursor: iced::Rectangle {
          x: node.x + bound.x,
          y: node.y + bound.y,
          width: 5.0,
          height: 20.0,
        },
        purpose: iced_core::input_method::Purpose::Normal,
        preedit: None,
      };
    }
    iced_core::input_method::InputMethod::Disabled
  }

  fn build_context_menu_overlay(
    menu: &ActiveContextMenu,
    on_action: &dyn Fn(ContextMenuAction) -> Message,
    on_close: Message,
  ) -> Option<ContextMenuOverlayState<'a, Message>>
  where
    Message: Clone,
  {
    let request = menu.request.as_ref()?;
    if menu.items.is_empty() {
      return None;
    }

    let build_menu = |length| -> Element<'a, Message, Theme, Renderer> {
      container(
        column(
          menu
            .items
            .iter()
            .map(|item| Self::render_context_menu_item(item, length, on_action)),
        )
        .spacing(0)
        .padding(4),
      )
      .style(context_menu_panel_style)
      .into()
    };

    let content = double_pass(build_menu(Length::Shrink), build_menu(Length::Fill));

    Some(ContextMenuOverlayState {
      anchor: iced::Point::new(request.x + 4.0, request.y + 4.0),
      close_message: Some(on_close),
      content,
    })
  }

  fn render_context_menu_item(
    item: &ContextMenuItem,
    length: Length,
    on_action: &dyn Fn(ContextMenuAction) -> Message,
  ) -> Element<'a, Message, Theme, Renderer>
  where
    Message: Clone,
  {
    if matches!(item.action, ContextMenuAction::Separator) {
      return container(
        container(iced::widget::Space::new())
          .height(Length::Fixed(1.0))
          .style(|_| iced::widget::container::Style {
            background: Some(Background::Color(Color::from_rgba8(0x1D, 0x21, 0x29, 0.08))),
            ..Default::default()
          }),
      )
      .padding([4, 8])
      .width(length)
      .into();
    }

    let message = item.enabled.then(|| on_action(item.action.clone()));

    button(
      container(text(item.label.clone()).size(13))
        .width(length)
        .padding([7, 10]),
    )
    .padding(0)
    .width(length)
    .style(context_menu_item_style(item.enabled))
    .on_press_maybe(message)
    .into()
  }
}

impl<Message: std::fmt::Debug + Clone> Widget<Message, Theme, Renderer> for Webview<'_, Message>
where
  Renderer: iced::advanced::Renderer,
{
  fn tag(&self) -> tree::Tag {
    tree::Tag::of::<CefState>()
  }

  fn state(&self) -> tree::State {
    let state = CefState {
      browser_id: self.browser_id,
      browser_host: CefState::browser_host(self.browser_id),
      cursor_in_bounds: None,
      last_button_modifiers: 0,
      last_click: None,
      last_mouse_down_on_view: false,
      active_touch_ids: HashSet::new(),
      last_cursor_position: None,
      bounds: iced::Rectangle::with_size(iced::Size::ZERO),
      webview_focused: false,
      window_focused: false,
      last_begin_frame_time: std::time::Instant::now(),
      begin_frame_interval: std::time::Duration::from_micros(16_666),
      ime_char_filter: ImeCharFilter::default(),
      monitor_frame_rate: self.frame_rate.unwrap_or(get_cached_max_windowless_frame_rate()),
      hidden_frame_rate: self.hidden_frame_rate,
    };
    state.show_browser(state.bounds);
    tree::State::new(state)
  }

  fn children(&self) -> Vec<Tree> {
    let mut children = vec![Tree::new(&self.content)];
    if let Some(popup_overlay) = &self.popup_overlay {
      children.push(Tree::new(popup_overlay));
    }
    if let Some(context_menu_overlay) = &self.context_menu_overlay {
      children.push(Tree::new(&context_menu_overlay.content));
    }
    children
  }

  fn diff(&self, tree: &mut Tree) {
    if tree.state.downcast_ref::<CefState>().browser_id != self.browser_id {
      *tree = Tree {
        tag: self.tag(),
        state: self.state(),
        children: self.children(),
      };
      return;
    }

    tree.children[0].diff(&self.content);
    let mut next_index = 1;

    if let Some(popup_overlay) = &self.popup_overlay {
      match tree.children.get_mut(next_index) {
        Some(child) => child.diff(popup_overlay),
        None => tree.children.push(Tree::new(popup_overlay)),
      }
      next_index += 1;
    }

    if let Some(context_menu_overlay) = &self.context_menu_overlay {
      match tree.children.get_mut(next_index) {
        Some(child) => child.diff(&context_menu_overlay.content),
        None => tree.children.push(Tree::new(&context_menu_overlay.content)),
      }
      next_index += 1;
    }

    tree.children.truncate(next_index);
  }

  fn size(&self) -> Size<Length> {
    Size::new(Length::Fill, Length::Fill)
  }

  fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &Limits) -> Node {
    let child_layout = self
      .content
      .as_widget_mut()
      .layout(&mut tree.children[0], renderer, limits);

    Node::with_children(child_layout.size(), vec![child_layout])
  }

  fn draw(
    &self,
    tree: &Tree,
    renderer: &mut Renderer,
    theme: &Theme,
    style: &Style,
    layout: Layout<'_>,
    cursor: Cursor,
    viewport: &Rectangle,
  ) {
    begin_render(self.browser_id);

    #[allow(clippy::unwrap_used)]
    self.content.as_widget().draw(
      &tree.children[0],
      renderer,
      theme,
      style,
      layout.children().next().unwrap(),
      cursor,
      viewport,
    );
  }

  fn update(
    &mut self,
    tree: &mut Tree,
    event: &Event,
    layout: Layout<'_>,
    cursor: iced::advanced::mouse::Cursor,
    renderer: &Renderer,
    clipboard: &mut dyn Clipboard,
    shell: &mut Shell<'_, Message>,
    viewport: &Rectangle,
  ) {
    let state = tree.state.downcast_mut::<CefState>();
    let bounds = layout.bounds();

    let previous_browser_id = state.browser_id;
    let previous_bounds = state.bounds;
    let browser_switched = state.update_browser(self.browser_id, bounds);
    register_render_waker(self.browser_id, || {
      let waker = shell.waker().clone();
      Arc::new(move || waker.wake())
    });

    if browser_switched {
      if let Some(host) = &state.browser_host {
        host.invalidate(cef::sys::cef_paint_element_type_t::PET_VIEW.into());
      }
      if bounds.width > 0.0 && bounds.height > 0.0 {
        state.send_begin_frame();
      }
      shell.request_redraw();
    }

    if state.bounds != bounds {
      let previous_bounds = state.bounds;
      state.bounds = bounds;
      if !browser_switched {
        resize(self.browser_id, bounds);
      }

      if let Some(host) = &state.browser_host {
        host.invalidate(cef::sys::cef_paint_element_type_t::PET_VIEW.into());
      }

      if previous_bounds.width > 0.0 && previous_bounds.height > 0.0 && bounds.width > 0.0 && bounds.height > 0.0 {
        state.send_begin_frame();
      }

      shell.request_redraw();

      if let Some(on_browser_view_change) = self.on_browser_view_bounds_change.as_ref() {
        shell.publish(on_browser_view_change(bounds));
      }
    }

    match event {
      Event::Keyboard(event) => {
        let event_already_captured = shell.is_event_captured();
        let should_forward =
          should_forward_keyboard_event_to_browser(state.webview_focused, state.window_focused, event_already_captured);
        if let Some((iced_event_type, key)) = iced_keyboard_arrow_key_for_log(event) {
          tracing::event!(
            arrow_key_forwarding_log_level(),
            browser_id = ?state.browser_id,
            key,
            iced_event_type,
            webview_focused = state.webview_focused,
            window_focused = state.window_focused,
            event_already_captured,
            should_forward,
            cursor_in_bounds = cursor.position_in(bounds).is_some(),
            "[im-arrow-key-debug][webview-widget] receive iced keyboard event"
          );
        }

        if state.webview_focused {
          lintel_lib_iced::ui_perf::mark_webview_interaction(self.browser_id.inner());
        }
        if should_forward {
          state.send_key_event(event);
        }

        if matches!(
          event,
          iced::keyboard::Event::KeyPressed {
            key: iced::keyboard::Key::Named(iced::keyboard::key::Named::F12),
            ..
          }
        ) && let Some(host) = &state.browser_host
          && let Some(point) = cursor.position_in(bounds)
          && is_devtools_enabled()
        {
          if host.has_dev_tools() == 1 {
            host.close_dev_tools();
          } else {
            host.show_dev_tools(
              None,
              None,
              Some(&Default::default()),
              Some(&cef::Point {
                x: point.x as i32,
                y: point.y as i32,
              }),
            );
          }
        }
        if cursor.position_in(bounds).is_some() {
          shell.capture_event();
        }
      }
      Event::Mouse(event) => {
        let cursor_position = cursor.position();
        let cursor_in_view =
          cursor_position.map(|position| iced::Point::new(position.x - bounds.x, position.y - bounds.y));
        let cursor_in_popup = cursor_in_view.filter(|point| state.is_over_popup_widget(*point, self.popup_bounds));
        state.cursor_in_bounds = cursor.position_in(bounds).or(cursor_in_popup);

        if matches!(event, iced::mouse::Event::ButtonPressed(_)) {
          state.update_focus(state.cursor_in_bounds.is_some(), self.frame_rate);
        }

        if let Some(point) = state.cursor_in_bounds {
          lintel_lib_iced::ui_perf::mark_webview_interaction(self.browser_id.inner());
          state.send_mouse_event(
            cursor_position,
            Some(point),
            self.popup_bounds,
            self.popup_offset,
            *event,
          );
          shell.capture_event();
        };
      }
      Event::Touch(event) => {
        let is_finger_pressed = matches!(event, touch::Event::FingerPressed { .. });
        if state.send_touch_event(bounds, self.popup_bounds, self.popup_offset, event) {
          lintel_lib_iced::ui_perf::mark_webview_interaction(self.browser_id.inner());
          if is_finger_pressed {
            state.update_focus(true, self.frame_rate);
          }
          shell.capture_event();
        }
      }
      Event::Window(iced::window::Event::RedrawRequested(now)) => {
        //state.try_send_begin_frame();
        //if state.webview_focused {
        let ime = self.input_method(cursor.position(), bounds);
        shell.request_input_method::<String>(&ime);
        //}
      }
      Event::Waken => {
        if has_pending_render(self.browser_id) {
          // Redraw reuses the current widget tree. Replace a placeholder or cached resize view.
          if self.waiting_for_frame {
            shell.invalidate_widgets();
          }
          shell.request_redraw();
        }
      }
      Event::Presented => {
        if let Some(frame) = finish_present_frame(self.browser_id) {
          if let Some(produced_at) = frame.produced_at {
            lintel_lib_iced::ui_perf::record_webview_interaction_frame_to_present_for_browser(
              self.browser_id.inner(),
              produced_at.elapsed(),
            );
          }
          // A newer frame may have arrived before draw but after this view was built.
          if self.waiting_for_frame {
            shell.invalidate_widgets();
          }
          if frame.needs_redraw {
            shell.request_redraw();
          }
        }
      }
      Event::Window(iced::window::Event::Rescaled(scale_factor)) => {
        if let Some(on_scale_factor_change) = self.on_scale_factor_change.as_ref() {
          shell.publish(on_scale_factor_change(state.browser_id, *scale_factor));
        }
      }
      Event::Window(iced::window::Event::Focused) => {
        state.update_window_focus(true);
      }
      Event::Window(iced::window::Event::Unfocused) => {
        state.update_window_focus(false);
      }

      Event::InputMethod(event) => {
        if state.webview_focused {
          lintel_lib_iced::ui_perf::mark_webview_interaction(self.browser_id.inner());
        }
        state.send_ime_event(event);
      }
      _ => {}
    }

    self.content.as_widget_mut().update(
      &mut tree.children[0],
      event,
      layout,
      cursor,
      renderer,
      clipboard,
      shell,
      viewport,
    );
  }

  fn operate(&mut self, state: &mut Tree, layout: Layout<'_>, _renderer: &Renderer, operation: &mut dyn Operation) {
    let state = state.state.downcast_mut::<CefState>();
    operation.custom(None, layout.bounds(), state);
  }

  fn mouse_interaction(
    &self,
    _state: &Tree,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    _viewport: &Rectangle,
    _renderer: &Renderer,
  ) -> mouse::Interaction {
    if cursor.position_in(layout.bounds()).is_some() {
      return self.cursor.unwrap_or_default();
    }
    mouse::Interaction::default()
  }

  fn overlay<'a>(
    &'a mut self,
    tree: &'a mut Tree,
    layout: Layout<'a>,
    renderer: &Renderer,
    viewport: &Rectangle,
    translation: iced_core::Vector,
  ) -> Option<iced_core::overlay::Element<'a, Message, Theme, Renderer>> {
    let (content_tree, mut rest) = tree.children.split_at_mut(1);
    let content_overlay = self.content.as_widget_mut().overlay(
      &mut content_tree[0],
      layout.children().next().unwrap(),
      renderer,
      viewport,
      translation,
    );

    let popup_overlay = if let Some(popup_overlay) = self.popup_overlay.as_mut() {
      let (popup_tree, next_rest) = rest.split_at_mut(1);
      rest = next_rest;
      Some(overlay::Element::new(Box::new(PopupOverlay {
        popup_overlay,
        tree: &mut popup_tree[0],
        position: layout.bounds().position() + translation,
        size: layout.bounds().size(),
      })))
    } else {
      None
    };

    let context_menu_overlay = if let Some(state) = self.context_menu_overlay.as_mut() {
      let (context_tree, _next_rest) = rest.split_at_mut(1);
      Some(overlay::Element::new(Box::new(ContextMenuOverlay {
        state,
        tree: &mut context_tree[0],
        origin: layout.bounds().position() + translation,
        viewport_size: layout.bounds().size(),
      })))
    } else {
      None
    };

    let overlays = content_overlay
      .into_iter()
      .chain(popup_overlay)
      .chain(context_menu_overlay)
      .collect::<Vec<_>>();
    (!overlays.is_empty()).then(|| overlay::Group::with_children(overlays).overlay())
  }
}

struct PopupOverlay<'a, 'b, Message> {
  popup_overlay: &'b mut Element<'a, Message, Theme, Renderer>,
  tree: &'b mut Tree,
  position: iced::Point,
  size: iced::Size,
}

impl<Message> overlay::Overlay<Message, Theme, Renderer> for PopupOverlay<'_, '_, Message> {
  fn layout(&mut self, renderer: &Renderer, _bounds: Size) -> Node {
    let limits = Limits::new(Size::ZERO, self.size)
      .width(Length::Fill)
      .height(Length::Fill);
    self
      .popup_overlay
      .as_widget_mut()
      .layout(self.tree, renderer, &limits)
      .move_to(self.position)
  }

  fn draw(&self, renderer: &mut Renderer, theme: &Theme, style: &Style, layout: Layout<'_>, cursor: mouse::Cursor) {
    self
      .popup_overlay
      .as_widget()
      .draw(self.tree, renderer, theme, style, layout, cursor, &layout.bounds());
  }

  fn update(
    &mut self,
    event: &Event,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    renderer: &Renderer,
    clipboard: &mut dyn Clipboard,
    shell: &mut Shell<'_, Message>,
  ) {
    let viewport = layout.bounds();
    self
      .popup_overlay
      .as_widget_mut()
      .update(self.tree, event, layout, cursor, renderer, clipboard, shell, &viewport);
  }

  fn mouse_interaction(&self, layout: Layout<'_>, cursor: mouse::Cursor, renderer: &Renderer) -> mouse::Interaction {
    self
      .popup_overlay
      .as_widget()
      .mouse_interaction(self.tree, layout, cursor, &layout.bounds(), renderer)
  }

  fn overlay<'c>(
    &'c mut self,
    layout: Layout<'c>,
    renderer: &Renderer,
  ) -> Option<overlay::Element<'c, Message, Theme, Renderer>> {
    self.popup_overlay.as_widget_mut().overlay(
      self.tree,
      layout,
      renderer,
      &layout.bounds(),
      iced_core::Vector::default(),
    )
  }
}

struct ContextMenuOverlay<'a, 'b, Message> {
  state: &'b mut ContextMenuOverlayState<'a, Message>,
  tree: &'b mut Tree,
  origin: iced::Point,
  viewport_size: iced::Size,
}

impl<Message> overlay::Overlay<Message, Theme, Renderer> for ContextMenuOverlay<'_, '_, Message>
where
  Message: Clone,
{
  fn layout(&mut self, renderer: &Renderer, _bounds: Size) -> Node {
    let limits = Limits::new(Size::ZERO, self.viewport_size)
      .width(Length::Shrink)
      .height(Length::Shrink);

    let mut node = self.state.content.as_widget_mut().layout(self.tree, renderer, &limits);
    let size = node.size();

    let min_x = self.origin.x + 4.0;
    let min_y = self.origin.y + 4.0;
    let max_x = (self.origin.x + self.viewport_size.width - size.width - 4.0).max(min_x);
    let max_y = (self.origin.y + self.viewport_size.height - size.height - 4.0).max(min_y);

    let x = (self.origin.x + self.state.anchor.x).clamp(min_x, max_x);
    let y = (self.origin.y + self.state.anchor.y).clamp(min_y, max_y);

    node.move_to(iced::Point::new(x, y))
  }

  fn draw(&self, renderer: &mut Renderer, theme: &Theme, style: &Style, layout: Layout<'_>, cursor: mouse::Cursor) {
    self
      .state
      .content
      .as_widget()
      .draw(self.tree, renderer, theme, style, layout, cursor, &layout.bounds());
  }

  fn update(
    &mut self,
    event: &Event,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    renderer: &Renderer,
    clipboard: &mut dyn Clipboard,
    shell: &mut Shell<'_, Message>,
  ) {
    if let Event::Mouse(mouse::Event::ButtonPressed(_)) = event
      && cursor.position_over(layout.bounds()).is_none()
    {
      if let Some(message) = self.state.close_message.clone() {
        shell.publish(message);
        shell.capture_event();
      }
      return;
    }

    let viewport = layout.bounds();
    self
      .state
      .content
      .as_widget_mut()
      .update(self.tree, event, layout, cursor, renderer, clipboard, shell, &viewport);
  }

  fn mouse_interaction(&self, layout: Layout<'_>, cursor: mouse::Cursor, renderer: &Renderer) -> mouse::Interaction {
    self
      .state
      .content
      .as_widget()
      .mouse_interaction(self.tree, layout, cursor, &layout.bounds(), renderer)
  }

  fn overlay<'c>(
    &'c mut self,
    layout: Layout<'c>,
    renderer: &Renderer,
  ) -> Option<overlay::Element<'c, Message, Theme, Renderer>> {
    self.state.content.as_widget_mut().overlay(
      self.tree,
      layout,
      renderer,
      &layout.bounds(),
      iced_core::Vector::default(),
    )
  }
}

fn context_menu_panel_style(_: &Theme) -> iced::widget::container::Style {
  iced::widget::container::Style {
    background: Some(Background::Color(Color::from_rgb8(0xFF, 0xFF, 0xFF))),
    border: Border {
      radius: 8.0.into(),
      width: 1.0,
      color: Color::from_rgba8(0x1D, 0x21, 0x29, 0.08),
    },
    shadow: iced::Shadow {
      color: Color::from_rgba8(0x00, 0x00, 0x00, 0.12),
      offset: iced::Vector::new(0.0, 8.0),
      blur_radius: 24.0,
    },
    ..Default::default()
  }
}

fn context_menu_item_style(enabled: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
  move |_: &Theme, status: button::Status| {
    let background = if !enabled {
      Color::TRANSPARENT
    } else {
      match status {
        button::Status::Hovered => Color::from_rgb8(0xF2, 0xF3, 0xF5),
        _ => Color::TRANSPARENT,
      }
    };

    let text_color = if enabled {
      Color::from_rgb8(0x1D, 0x21, 0x29)
    } else {
      Color::from_rgb8(0xC9, 0xCD, 0xD4)
    };

    button::Style {
      background: Some(Background::Color(background)),
      text_color,
      border: Border {
        radius: 6.0.into(),
        width: 0.0,
        color: Color::TRANSPARENT,
      },
      ..Default::default()
    }
  }
}

impl<'a, Message: 'a, Theme, Renderer> From<Webview<'a, Message>> for Element<'a, Message, Theme, Renderer>
where
  Renderer: iced::advanced::Renderer,
  Webview<'a, Message>: Widget<Message, Theme, Renderer>,
{
  fn from(wrapper: Webview<'a, Message>) -> Self {
    Self::new(wrapper)
  }
}

static ENABLE_DEVTOOLS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn toggle_devtools(state: bool) {
  ENABLE_DEVTOOLS.store(state, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn is_devtools_enabled() -> bool {
  ENABLE_DEVTOOLS.load(std::sync::atomic::Ordering::Relaxed) || cfg!(debug_assertions)
}

#[cfg(test)]
mod tests {
  use std::{
    sync::{
      Arc,
      atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
  };

  use super::{
    BrowserId, RenderWaker, arrow_key_forwarding_log_level, begin_render, finish_present_frame, register_render_waker,
    unregister_render_waker, wake_render_waker,
  };

  fn render_waker() -> RenderWaker {
    RenderWaker::new(Arc::new(|| {}))
  }

  #[test]
  fn consecutive_frames_share_one_pending_wake() {
    let waker = render_waker();

    assert!(waker.notify_frame());
    assert!(!waker.notify_frame());
    assert!(!waker.notify_frame());
    assert!(waker.has_pending_redraw());
  }

  #[test]
  fn register_render_waker_refreshes_wake_callback_without_resetting_frame_state() {
    let browser_id = BrowserId::new(i32::MAX);
    let stale_wake_count = Arc::new(AtomicUsize::new(0));
    let current_wake_count = Arc::new(AtomicUsize::new(0));

    register_render_waker(browser_id, {
      let stale_wake_count = stale_wake_count.clone();
      move || {
        Arc::new(move || {
          stale_wake_count.fetch_add(1, Ordering::Relaxed);
        })
      }
    });
    wake_render_waker(browser_id, None);
    begin_render(browser_id);

    register_render_waker(browser_id, {
      let current_wake_count = current_wake_count.clone();
      move || {
        Arc::new(move || {
          current_wake_count.fetch_add(1, Ordering::Relaxed);
        })
      }
    });

    assert!(finish_present_frame(browser_id).is_some());
    wake_render_waker(browser_id, None);
    assert_eq!(stale_wake_count.load(Ordering::Relaxed), 1);
    assert_eq!(current_wake_count.load(Ordering::Relaxed), 1);
    unregister_render_waker(browser_id);
  }

  #[test]
  fn completed_present_releases_pending_wake() {
    let waker = render_waker();
    assert!(waker.notify_frame());
    assert!(waker.begin_redraw());

    assert!(waker.has_pending_redraw());
    assert!(!waker.finish_present());
    assert!(!waker.has_pending_redraw());
    assert!(waker.notify_frame());
  }

  #[test]
  fn frame_arriving_before_present_schedules_follow_up_redraw() {
    let waker = render_waker();
    assert!(waker.notify_frame());
    assert!(waker.begin_redraw());

    assert!(!waker.notify_frame());
    assert!(waker.finish_present());
    assert!(waker.has_pending_redraw());
  }

  #[test]
  fn presented_frame_keeps_the_timestamp_of_the_generation_that_was_drawn() {
    let waker = render_waker();
    let first_frame_at = Instant::now();
    assert!(waker.notify_frame_at(Some(first_frame_at)));
    assert!(waker.begin_redraw());

    assert!(!waker.notify_frame_at(Some(Instant::now())));
    let presented = waker.finish_present_frame().unwrap();

    assert_eq!(presented.produced_at, Some(first_frame_at));
    assert!(presented.needs_redraw);
  }

  #[test]
  fn presented_without_a_matching_draw_does_not_release_pending_wake() {
    let waker = render_waker();
    assert!(waker.notify_frame());

    assert!(!waker.finish_present());
    assert!(waker.has_pending_redraw());
  }

  #[test]
  fn arrow_key_forwarding_log_uses_debug_level_for_high_frequency_input() {
    assert_eq!(arrow_key_forwarding_log_level(), tracing::Level::DEBUG);
  }
}

#[cfg(test)]
mod ime_char_filter_tests {
  use super::*;

  fn text(value: &str) -> iced_core::SmolStr {
    iced_core::SmolStr::new(value)
  }

  fn test_cef_state(browser_id: BrowserId) -> CefState {
    CefState {
      browser_id,
      browser_host: None,
      cursor_in_bounds: None,
      last_cursor_position: None,
      last_button_modifiers: 0,
      last_click: None,
      last_mouse_down_on_view: false,
      active_touch_ids: HashSet::new(),
      bounds: iced::Rectangle::with_size(iced::Size::ZERO),
      webview_focused: false,
      window_focused: true,
      last_begin_frame_time: std::time::Instant::now(),
      begin_frame_interval: std::time::Duration::from_micros(16_666),
      ime_char_filter: ImeCharFilter::default(),
      monitor_frame_rate: 30,
      hidden_frame_rate: 1,
    }
  }

  #[test]
  fn set_webview_focus_command_updates_component_focus_state() {
    let browser_id = BrowserId::new(1);
    let mut state = test_cef_state(browser_id);

    state.apply_ui_command(WebviewUiCommand::SetWebviewFocus {
      browser_id,
      focused: true,
      reason: WebviewFocusReason::ApiRequested,
    });

    assert!(state.webview_focused);
  }

  #[test]
  fn blur_command_clears_component_input_state() {
    let browser_id = BrowserId::new(1);
    let mut state = test_cef_state(browser_id);
    state.webview_focused = true;
    state.last_button_modifiers = 1;
    state.last_mouse_down_on_view = true;
    state.ime_char_filter.on_opened();
    state.ime_char_filter.on_preedit("a");

    state.apply_ui_command(WebviewUiCommand::SetWebviewFocus {
      browser_id,
      focused: false,
      reason: WebviewFocusReason::NativeSearchFocused,
    });

    assert!(!state.webview_focused);
    assert_eq!(state.last_button_modifiers, 0);
    assert!(!state.last_mouse_down_on_view);
    assert!(!state.ime_char_filter.composing);
  }

  #[test]
  fn synthetic_arrow_key_forward_focuses_component_for_native_search_handoff() {
    let browser_id = BrowserId::new(1);
    let mut state = test_cef_state(browser_id);
    state.webview_focused = false;
    state.window_focused = true;

    state.apply_ui_command(WebviewUiCommand::FocusAndForwardSyntheticKey {
      browser_id,
      key: WebviewSyntheticKey::ArrowDown,
      reason: WebviewKeyReason::NativeSearchNavigation,
    });

    assert!(state.webview_focused);
    assert!(state.window_focused);
  }

  #[test]
  fn synthetic_arrow_key_forward_from_native_search_repairs_stale_window_focus() {
    let browser_id = BrowserId::new(1);
    let mut state = test_cef_state(browser_id);
    state.webview_focused = false;
    state.window_focused = false;

    state.apply_ui_command(WebviewUiCommand::FocusAndForwardSyntheticKey {
      browser_id,
      key: WebviewSyntheticKey::ArrowDown,
      reason: WebviewKeyReason::NativeSearchNavigation,
    });

    assert!(state.webview_focused);
    assert!(state.window_focused);
  }

  #[test]
  fn window_focus_event_preserves_component_blur_state() {
    let browser_id = BrowserId::new(1);
    let mut state = test_cef_state(browser_id);
    state.webview_focused = true;
    state.window_focused = true;

    state.apply_ui_command(WebviewUiCommand::SetWebviewFocus {
      browser_id,
      focused: false,
      reason: WebviewFocusReason::NativeSearchFocused,
    });
    state.update_window_focus(false);
    state.update_window_focus(true);

    assert!(state.window_focused);
    assert!(!state.webview_focused);
  }

  #[test]
  fn window_focus_roundtrip_restores_host_focus_only_when_webview_was_focused() {
    let mut state = test_cef_state(BrowserId::new(1));
    state.update_focus(true, None);
    state.update_window_focus(false);

    assert!(!state.window_focused);
    assert!(state.webview_focused);

    state.update_window_focus(true);

    assert!(state.window_focused);
    assert!(state.webview_focused);
  }

  #[test]
  fn ui_command_operation_ignores_other_browser_state() {
    let mut state = test_cef_state(BrowserId::new(1));
    let mut operation = WebviewUiCommandOperation::new(WebviewUiCommand::SetWebviewFocus {
      browser_id: BrowserId::new(2),
      focused: true,
      reason: WebviewFocusReason::ApiRequested,
    });

    operation.custom(None, iced::Rectangle::with_size(iced::Size::ZERO), &mut state);

    assert!(!state.webview_focused);
    assert!(matches!(
      operation.finish(),
      iced::advanced::widget::operation::Outcome::Some(false)
    ));
  }

  #[test]
  fn ui_state_operation_reads_matching_component_state() {
    let mut state = test_cef_state(BrowserId::new(1));
    state.webview_focused = true;
    state.window_focused = false;
    let mut operation = WebviewUiStateOperation::new(BrowserId::new(1));

    operation.custom(None, iced::Rectangle::with_size(iced::Size::ZERO), &mut state);

    assert!(matches!(
      operation.finish(),
      iced::advanced::widget::operation::Outcome::Some(Some(WebviewUiStateSnapshot {
        browser_id,
        webview_focused: true,
        window_focused: false,
      })) if browser_id == BrowserId::new(1)
    ));
  }

  #[test]
  fn ui_state_operation_ignores_other_browser_state() {
    let mut state = test_cef_state(BrowserId::new(1));
    let mut operation = WebviewUiStateOperation::new(BrowserId::new(2));

    operation.custom(None, iced::Rectangle::with_size(iced::Size::ZERO), &mut state);

    assert!(matches!(
      operation.finish(),
      iced::advanced::widget::operation::Outcome::Some(None)
    ));
  }

  #[test]
  fn test_ime_char_filter_suppresses_key_char_after_composition_opened() {
    let mut filter = ImeCharFilter::default();
    let key_text = text("a");

    filter.on_opened();

    assert!(filter.should_suppress_key_char(Some(&key_text)));
  }

  #[test]
  fn test_ime_char_filter_suppresses_key_char_while_preediting() {
    let mut filter = ImeCharFilter::default();
    let key_text = text("a");

    filter.on_opened();
    filter.on_preedit("a");

    assert!(filter.should_suppress_key_char(Some(&key_text)));
  }

  #[test]
  fn test_ime_char_filter_suppresses_duplicate_key_char_after_commit_once() {
    let mut filter = ImeCharFilter::default();
    let key_text = text("a");

    assert!(!filter.on_commit("a"));

    assert!(filter.should_suppress_key_char(Some(&key_text)));
    assert!(!filter.should_suppress_key_char(Some(&key_text)));
  }

  #[test]
  fn test_ime_char_filter_suppresses_segmented_key_chars_after_commit() {
    let mut filter = ImeCharFilter::default();
    let key_text = text("哈");

    assert!(!filter.on_commit("哈哈"));

    assert!(filter.should_suppress_key_char(Some(&key_text)));
    assert!(filter.should_suppress_key_char(Some(&key_text)));
    assert!(!filter.should_suppress_key_char(Some(&key_text)));
  }

  #[test]
  fn test_ime_char_filter_suppresses_duplicate_commit_before_closed() {
    let mut filter = ImeCharFilter::default();

    filter.on_opened();
    filter.on_preedit("ha");
    filter.on_preedit("");

    assert!(!filter.on_commit("哈哈"));

    filter.on_preedit("");

    assert!(filter.on_commit("哈哈"));
  }

  #[test]
  fn test_ime_char_filter_allows_same_commit_in_new_composition() {
    let mut filter = ImeCharFilter::default();

    filter.on_opened();
    filter.on_preedit("ha");
    assert!(!filter.on_commit("哈哈"));
    assert!(filter.on_commit("哈哈"));

    filter.on_closed();
    filter.on_opened();
    filter.on_preedit("ha");

    assert!(!filter.on_commit("哈哈"));
  }

  #[test]
  fn test_ime_char_filter_does_not_suppress_empty_commit() {
    let mut filter = ImeCharFilter::default();

    assert!(!filter.on_commit(""));
    assert!(!filter.on_commit(""));
  }

  #[test]
  fn test_ime_char_filter_keeps_plain_keyboard_text_without_ime() {
    let mut filter = ImeCharFilter::default();
    let key_text = text("a");

    assert!(!filter.should_suppress_key_char(Some(&key_text)));
  }

  #[test]
  fn test_ime_char_filter_does_not_suppress_different_text_after_commit() {
    let mut filter = ImeCharFilter::default();
    let key_text = text("b");

    assert!(!filter.on_commit("a"));

    assert!(!filter.should_suppress_key_char(Some(&key_text)));
  }

  #[test]
  fn test_ime_char_filter_closed_clears_composing_state() {
    let mut filter = ImeCharFilter::default();
    let key_text = text("a");

    filter.on_opened();
    filter.on_preedit("a");
    filter.on_closed();

    assert!(!filter.should_suppress_key_char(Some(&key_text)));
  }

  #[test]
  fn test_ime_char_filter_only_enabled_for_windows_7() {
    assert!(should_filter_win7_ime_for_os(
      os_info::Type::Windows,
      &os_info::Version::Semantic(6, 1, 7601),
      Some("Windows 7 Professional")
    ));
    assert!(!should_filter_win7_ime_for_os(
      os_info::Type::Windows,
      &os_info::Version::Semantic(10, 0, 19045),
      Some("Windows 10 Pro")
    ));
    assert!(!should_filter_win7_ime_for_os(
      os_info::Type::Windows,
      &os_info::Version::Semantic(6, 1, 7601),
      Some("Windows Server 2008 R2")
    ));
    assert!(!should_filter_win7_ime_for_os(
      os_info::Type::Macos,
      &os_info::Version::Semantic(14, 0, 0),
      None
    ));
  }

  #[test]
  fn forwards_keyboard_event_only_when_webview_and_window_are_focused() {
    assert!(should_forward_keyboard_event_to_browser(true, true, false));
    assert!(!should_forward_keyboard_event_to_browser(false, true, false));
    assert!(!should_forward_keyboard_event_to_browser(true, false, false));
    assert!(!should_forward_keyboard_event_to_browser(false, false, false));
  }

  #[test]
  fn captured_keyboard_event_is_not_forwarded_to_webview() {
    assert!(!should_forward_keyboard_event_to_browser(true, true, true));
  }
}
