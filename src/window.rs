//! Managed native windows. Use the handle supplied to [`crate::multi_window`] from state or services.

use std::{
  collections::BTreeMap,
  fmt,
  sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
  },
};

use tokio::sync::mpsc::{Receiver, Sender, channel, error::TrySendError};

use crate::iced::{Task, window as native};

pub use native::{Position, Settings};

/// The same stable, typed ID accepted by iced's window tasks and view callback.
pub type WindowId = native::Id;

/// An owned native window for a platform integration. Release it after detaching the
/// integration and when the managed window closes, so native destruction is not delayed.
#[cfg(all(feature = "public-iced", not(target_arch = "wasm32")))]
pub type NativeWindow = Arc<dyn native::Window + Send + Sync>;

/// A managed window's lifecycle state, including pending native creation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowStatus {
  Opening,
  Open,
  Closing,
}

/// A window command could not be queued or targeted an expired handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowError {
  ManagerClosed,
  QueueFull,
  UnknownWindow(WindowId),
  WindowNotOpen(WindowId),
  DesktopUnavailable,
  InvalidSettings,
  InvalidGeometry,
  OpenTimedOut,
}

impl fmt::Display for WindowError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::ManagerClosed => f.write_str("the window manager has stopped"),
      Self::QueueFull => f.write_str("the window manager command queue is full"),
      Self::UnknownWindow(id) => write!(f, "window {id} is no longer managed"),
      Self::WindowNotOpen(id) => write!(f, "window {id} is not open for a native integration"),
      Self::DesktopUnavailable => f.write_str("desktop services were not enabled for this application"),
      Self::InvalidSettings => f.write_str("window size constraints or coordinates are invalid"),
      Self::InvalidGeometry => f.write_str("window coordinates must be finite and dimensions must be positive"),
      Self::OpenTimedOut => f.write_str("native window creation did not complete before the deadline"),
    }
  }
}

impl std::error::Error for WindowError {}

/// Native lifecycle events always carry the window that produced them.
#[derive(Clone, Debug)]
pub enum WindowEvent {
  Opened(WindowId),
  OpenFailed {
    id: WindowId,
    error: WindowError,
  },
  CloseRequested(WindowId),
  CloseVetoed(WindowId),
  Closed(WindowId),
  /// Other iced events, including focus, resize, move, scale, and file drops.
  Native {
    id: WindowId,
    event: native::Event,
  },
}

impl WindowEvent {
  pub fn id(&self) -> WindowId {
    match self {
      Self::Opened(id) | Self::CloseRequested(id) | Self::CloseVetoed(id) | Self::Closed(id) => *id,
      Self::OpenFailed { id, .. } | Self::Native { id, .. } => *id,
    }
  }
}

pub(crate) enum Command {
  Open { id: WindowId, task: Task<WindowId> },
  Close { id: WindowId, force: bool },
  Title { id: WindowId, title: String },
  Operate { id: WindowId, operation: Operation },
  Quit,
}

impl fmt::Debug for Command {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Open { id, .. } => f.debug_tuple("Open").field(id).finish(),
      Self::Close { id, force } => f.debug_struct("Close").field("id", id).field("force", force).finish(),
      Self::Title { id, title } => f.debug_struct("Title").field("id", id).field("title", title).finish(),
      Self::Operate { id, operation } => f
        .debug_struct("Operate")
        .field("id", id)
        .field("operation", operation)
        .finish(),
      Self::Quit => f.write_str("Quit"),
    }
  }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Operation {
  Focus,
  Show,
  Hide,
  Resize(crate::iced::Size),
  Move(crate::iced::Point),
  Minimize(bool),
  Maximize(bool),
}

impl Operation {
  pub(crate) fn task<Message>(self, id: WindowId) -> Task<Message> {
    match self {
      Self::Focus => native::gain_focus(id),
      Self::Show => native::set_mode(id, native::Mode::Windowed),
      Self::Hide => native::set_mode(id, native::Mode::Hidden),
      Self::Resize(size) => native::resize(id, size),
      Self::Move(position) => native::move_to(id, position),
      Self::Minimize(minimized) => native::minimize(id, minimized),
      Self::Maximize(maximized) => native::maximize(id, maximized),
    }
  }
}

#[derive(Clone, Debug)]
pub(crate) struct WindowInfo {
  pub title: String,
  pub status: WindowStatus,
  pub scope_id: u64,
  pub opened: bool,
}

struct Shared {
  tx: Sender<Command>,
  windows: Mutex<BTreeMap<WindowId, WindowInfo>>,
  #[cfg(feature = "desktop")]
  desktop: std::sync::OnceLock<Arc<lintel_desktop::DesktopService>>,
}

/// A cloneable, thread-safe native window service. Commands run on the UI thread.
///
/// IDs are registered before `open` returns, so initial views and messages can be
/// routed before native creation completes. Keep this handle in application state.
#[derive(Clone)]
pub struct WindowManager(Arc<Shared>);

impl fmt::Debug for WindowManager {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("WindowManager")
      .field("windows", &self.ids())
      .finish_non_exhaustive()
  }
}

static NEXT_SCOPE: AtomicU64 = AtomicU64::new(1);
const COMMAND_CAPACITY: usize = 1024;

pub(crate) fn new_scope_id() -> u64 {
  NEXT_SCOPE.fetch_add(1, Ordering::Relaxed)
}

impl WindowManager {
  pub(crate) fn channel() -> (Self, Receiver<Command>) {
    let (tx, rx) = channel(COMMAND_CAPACITY);
    (
      Self(Arc::new(Shared {
        tx,
        windows: Mutex::new(BTreeMap::new()),
        #[cfg(feature = "desktop")]
        desktop: std::sync::OnceLock::new(),
      })),
      rx,
    )
  }

  /// Opens a managed window and returns its ID immediately.
  ///
  /// Native close buttons are handled through the application's close policy.
  /// Observe `WindowEvent::Opened` / `OpenFailed` to learn the creation result.
  pub fn open(&self, title: impl Into<String>, mut settings: Settings) -> Result<WindowId, WindowError> {
    if !valid_size(settings.size)
      || settings.min_size.is_some_and(|size| !valid_size(size))
      || settings.max_size.is_some_and(|size| !valid_size(size))
      || matches!(settings.position, Position::Specific(point) if !valid_position(point))
      || matches!((settings.min_size, settings.max_size), (Some(min), Some(max)) if min.width > max.width || min.height > max.height)
    {
      return Err(WindowError::InvalidSettings);
    }
    // Reserve admission before allocating an iced ID or registering ownership.
    let permit = self.0.tx.try_reserve().map_err(queue_error)?;
    settings.exit_on_close_request = false;
    let (id, task) = native::open(settings);
    self.0.windows.lock().unwrap().insert(
      id,
      WindowInfo {
        title: title.into(),
        status: WindowStatus::Opening,
        scope_id: new_scope_id(),
        opened: false,
      },
    );
    permit.send(Command::Open { id, task });
    Ok(id)
  }

  /// Requests closure through the same close-veto policy as the native close button.
  pub fn close(&self, id: WindowId) -> Result<(), WindowError> {
    self.send_to(id, Command::Close { id, force: false })
  }

  /// Closes a window without asking the close-veto policy.
  pub fn destroy(&self, id: WindowId) -> Result<(), WindowError> {
    self.send_to(id, Command::Close { id, force: true })
  }

  /// Changes the native title and the title used by the renderer.
  pub fn set_title(&self, id: WindowId, title: impl Into<String>) -> Result<(), WindowError> {
    self.send_to(
      id,
      Command::Title {
        id,
        title: title.into(),
      },
    )
  }

  /// Brings a visible, unminimized window to the foreground and requests keyboard focus.
  pub fn focus(&self, id: WindowId) -> Result<(), WindowError> {
    self.operate(id, Operation::Focus)
  }

  /// Shows the window in windowed mode. This does not request keyboard focus.
  pub fn show(&self, id: WindowId) -> Result<(), WindowError> {
    self.operate(id, Operation::Show)
  }

  pub fn hide(&self, id: WindowId) -> Result<(), WindowError> {
    self.operate(id, Operation::Hide)
  }

  /// Resizes the client area in logical pixels, subject to native minimum and maximum settings.
  pub fn resize(&self, id: WindowId, size: crate::iced::Size) -> Result<(), WindowError> {
    self.require_window(id)?;
    if !valid_size(size) {
      return Err(WindowError::InvalidGeometry);
    }
    self.operate(id, Operation::Resize(size))
  }

  /// Moves the window in logical desktop coordinates. Negative coordinates are valid on other monitors.
  /// Native window movement is unsupported on Wayland.
  pub fn move_to(&self, id: WindowId, position: crate::iced::Point) -> Result<(), WindowError> {
    self.require_window(id)?;
    if !valid_position(position) {
      return Err(WindowError::InvalidGeometry);
    }
    self.operate(id, Operation::Move(position))
  }

  /// Minimizes the window, or restores it when `minimized` is false.
  pub fn minimize(&self, id: WindowId, minimized: bool) -> Result<(), WindowError> {
    self.operate(id, Operation::Minimize(minimized))
  }

  /// Maximizes the window, or restores it when `maximized` is false.
  pub fn maximize(&self, id: WindowId, maximized: bool) -> Result<(), WindowError> {
    self.operate(id, Operation::Maximize(maximized))
  }

  /// Requests explicit application exit, including registered plugin shutdown hooks.
  pub fn quit(&self) -> Result<(), WindowError> {
    self.0.tx.try_send(Command::Quit).map_err(queue_error)
  }

  pub fn ids(&self) -> Vec<WindowId> {
    self.0.windows.lock().unwrap().keys().copied().collect()
  }

  pub fn contains(&self, id: WindowId) -> bool {
    self.0.windows.lock().unwrap().contains_key(&id)
  }

  pub fn status(&self, id: WindowId) -> Option<WindowStatus> {
    self.0.windows.lock().unwrap().get(&id).map(|info| info.status)
  }

  /// Stable task scope key for `TaskScope::Window` and descendant page scopes.
  /// Window IDs are opaque in iced, so task scope IDs are allocated independently.
  pub fn scope_id(&self, id: WindowId) -> Option<u64> {
    self.0.windows.lock().unwrap().get(&id).map(|info| info.scope_id)
  }

  pub fn title(&self, id: WindowId) -> Option<String> {
    self.0.windows.lock().unwrap().get(&id).map(|info| info.title.clone())
  }

  /// Native service installed by the application's `.desktop()` builder option.
  #[cfg(feature = "desktop")]
  pub fn desktop(&self) -> Option<Arc<lintel_desktop::DesktopService>> {
    self.0.desktop.get().cloned()
  }

  #[cfg(feature = "desktop")]
  pub(crate) fn install_desktop(&self, service: Option<Arc<lintel_desktop::DesktopService>>) {
    if let Some(service) = service {
      let _ = self.0.desktop.set(service);
    }
  }

  /// Runs a platform integration on the UI thread with shared ownership of its native window.
  ///
  /// Start after `WindowEvent::Opened`. The callback must be short, and its owner must be
  /// released after detaching the integration on `WindowEvent::Closed`. A window closed
  /// before iced dispatches the task will cancel the task without producing a message.
  #[cfg(all(feature = "public-iced", not(target_arch = "wasm32")))]
  pub fn with_native_window<T: Send + 'static>(
    &self,
    id: WindowId,
    integration: impl FnOnce(NativeWindow) -> T + Send + 'static,
  ) -> Result<Task<Result<T, WindowError>>, WindowError> {
    self.require_window(id)?;
    if self.0.tx.is_closed() {
      return Err(WindowError::ManagerClosed);
    }
    if self.status(id) != Some(WindowStatus::Open) {
      return Err(WindowError::WindowNotOpen(id));
    }
    let windows = self.clone();
    Ok(native::run_owned(id, move |owner| {
      if windows.status(id) != Some(WindowStatus::Open) {
        return Err(WindowError::WindowNotOpen(id));
      }
      Ok(integration(owner))
    }))
  }

  fn send_to(&self, id: WindowId, command: Command) -> Result<(), WindowError> {
    self.require_window(id)?;
    self.0.tx.try_send(command).map_err(queue_error)
  }

  fn require_window(&self, id: WindowId) -> Result<(), WindowError> {
    if self.contains(id) {
      Ok(())
    } else {
      Err(WindowError::UnknownWindow(id))
    }
  }

  fn operate(&self, id: WindowId, operation: Operation) -> Result<(), WindowError> {
    self.send_to(id, Command::Operate { id, operation })
  }

  pub(crate) fn change(&self, id: WindowId, change: impl FnOnce(&mut WindowInfo)) -> bool {
    let mut windows = self.0.windows.lock().unwrap();
    if let Some(info) = windows.get_mut(&id) {
      change(info);
      true
    } else {
      false
    }
  }

  pub(crate) fn remove(&self, id: WindowId) -> Option<WindowInfo> {
    self.0.windows.lock().unwrap().remove(&id)
  }

  pub(crate) fn mark_opened(&self, id: WindowId) -> Option<WindowStatus> {
    let mut windows = self.0.windows.lock().unwrap();
    let info = windows.get_mut(&id)?;
    if info.opened {
      return None;
    }
    info.opened = true;
    if info.status == WindowStatus::Opening {
      info.status = WindowStatus::Open;
    }
    Some(info.status)
  }

  pub(crate) fn awaiting_open(&self, id: WindowId) -> bool {
    self.0.windows.lock().unwrap().get(&id).is_some_and(|info| !info.opened)
  }
}

fn valid_size(size: crate::iced::Size) -> bool {
  size.width.is_finite() && size.height.is_finite() && size.width > 0.0 && size.height > 0.0
}

fn valid_position(position: crate::iced::Point) -> bool {
  position.x.is_finite() && position.y.is_finite()
}

fn queue_error<T>(error: TrySendError<T>) -> WindowError {
  match error {
    TrySendError::Full(_) => WindowError::QueueFull,
    TrySendError::Closed(_) => WindowError::ManagerClosed,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn open_registers_unique_pending_ids_before_native_dispatch() {
    let (windows, mut rx) = WindowManager::channel();
    let a = windows.open("A", Settings::default()).unwrap();
    let b = windows.open("B", Settings::default()).unwrap();
    assert_ne!(a, b);
    assert_ne!(windows.scope_id(a), windows.scope_id(b));
    assert_eq!(windows.status(a), Some(WindowStatus::Opening));
    assert_eq!(windows.title(b).as_deref(), Some("B"));
    assert!(matches!(rx.try_recv().unwrap(), Command::Open { id, .. } if id == a));
    assert!(matches!(rx.try_recv().unwrap(), Command::Open { id, .. } if id == b));
  }

  #[test]
  fn failed_queue_rolls_back_pending_window_and_rejects_late_commands() {
    let (windows, rx) = WindowManager::channel();
    drop(rx);
    assert_eq!(windows.open("A", Settings::default()), Err(WindowError::ManagerClosed));
    assert!(windows.ids().is_empty());
    assert_eq!(windows.quit(), Err(WindowError::ManagerClosed));
  }

  #[test]
  fn invalid_dimensions_and_expired_ids_are_rejected() {
    let (windows, _rx) = WindowManager::channel();
    for width in [0.0, -1.0, f32::INFINITY, f32::NAN] {
      let settings = Settings {
        size: crate::iced::Size::new(width, 100.0),
        ..Default::default()
      };
      assert_eq!(windows.open("invalid", settings), Err(WindowError::InvalidSettings));
    }
    let id = windows.open("A", Settings::default()).unwrap();
    windows.remove(id);
    assert_eq!(windows.close(id), Err(WindowError::UnknownWindow(id)));
    assert_eq!(windows.destroy(id), Err(WindowError::UnknownWindow(id)));
    assert_eq!(windows.set_title(id, "late"), Err(WindowError::UnknownWindow(id)));
  }

  #[test]
  #[cfg(all(feature = "public-iced", not(target_arch = "wasm32")))]
  fn native_integration_rejects_pending_closed_and_expired_windows() {
    let (windows, rx) = WindowManager::channel();
    let id = windows.open("A", Settings::default()).unwrap();
    assert!(matches!(
      windows.with_native_window(id, |_| ()),
      Err(WindowError::WindowNotOpen(found)) if found == id
    ));
    windows.mark_opened(id);
    windows.change(id, |info| info.status = WindowStatus::Closing);
    assert!(matches!(
      windows.with_native_window(id, |_| ()),
      Err(WindowError::WindowNotOpen(_))
    ));
    windows.change(id, |info| info.status = WindowStatus::Open);
    drop(rx);
    assert!(matches!(
      windows.with_native_window(id, |_| ()),
      Err(WindowError::ManagerClosed)
    ));
    windows.remove(id);
    assert!(matches!(
      windows.with_native_window(id, |_| ()),
      Err(WindowError::UnknownWindow(_))
    ));
  }

  #[test]
  fn open_acknowledgement_is_once_only_and_preserves_early_close() {
    let (windows, _rx) = WindowManager::channel();
    let a = windows.open("A", Settings::default()).unwrap();
    let b = windows.open("B", Settings::default()).unwrap();
    windows.change(a, |info| info.status = WindowStatus::Closing);
    assert!(windows.awaiting_open(a));
    assert_eq!(windows.mark_opened(a), Some(WindowStatus::Closing));
    assert!(!windows.awaiting_open(a));
    assert_eq!(windows.mark_opened(a), None);
    assert_eq!(windows.mark_opened(b), Some(WindowStatus::Open));
    windows.remove(a);
    assert_eq!(windows.mark_opened(a), None);
    assert_eq!(windows.status(b), Some(WindowStatus::Open));
  }

  #[test]
  fn native_controls_keep_the_target_window_and_arguments() {
    let (windows, mut rx) = WindowManager::channel();
    let a = windows.open("A", Settings::default()).unwrap();
    let b = windows.open("B", Settings::default()).unwrap();
    let _ = rx.try_recv().unwrap();
    let _ = rx.try_recv().unwrap();
    windows.focus(b).unwrap();
    windows.show(b).unwrap();
    windows.hide(b).unwrap();
    windows.resize(b, crate::iced::Size::new(640.0, 480.0)).unwrap();
    windows.move_to(b, crate::iced::Point::new(-100.0, 25.0)).unwrap();
    windows.minimize(b, true).unwrap();
    windows.minimize(b, false).unwrap();
    windows.maximize(b, true).unwrap();
    windows.maximize(b, false).unwrap();
    for expected in [
      Operation::Focus,
      Operation::Show,
      Operation::Hide,
      Operation::Resize(crate::iced::Size::new(640.0, 480.0)),
      Operation::Move(crate::iced::Point::new(-100.0, 25.0)),
      Operation::Minimize(true),
      Operation::Minimize(false),
      Operation::Maximize(true),
      Operation::Maximize(false),
    ] {
      assert!(matches!(rx.try_recv().unwrap(), Command::Operate { id, operation } if id == b && operation == expected));
    }
    assert_eq!(windows.status(a), Some(WindowStatus::Opening));
    assert!(rx.try_recv().is_err());
    windows.remove(b);
    for result in [
      windows.focus(b),
      windows.show(b),
      windows.hide(b),
      windows.resize(b, crate::iced::Size::new(640.0, 480.0)),
      windows.move_to(b, crate::iced::Point::ORIGIN),
      windows.minimize(b, true),
      windows.maximize(b, true),
    ] {
      assert_eq!(result, Err(WindowError::UnknownWindow(b)));
    }
    assert!(rx.try_recv().is_err());
  }

  #[test]
  fn geometry_validation_rejects_nonfinite_sizes_and_coordinates_before_queueing() {
    let (windows, mut rx) = WindowManager::channel();
    let id = windows.open("A", Settings::default()).unwrap();
    let _ = rx.try_recv().unwrap();
    for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -1.0] {
      assert_eq!(
        windows.resize(id, crate::iced::Size::new(invalid, 100.0)),
        Err(WindowError::InvalidGeometry)
      );
      assert_eq!(
        windows.resize(id, crate::iced::Size::new(100.0, invalid)),
        Err(WindowError::InvalidGeometry)
      );
    }
    for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
      assert_eq!(
        windows.move_to(id, crate::iced::Point::new(invalid, 0.0)),
        Err(WindowError::InvalidGeometry)
      );
      assert_eq!(
        windows.move_to(id, crate::iced::Point::new(0.0, invalid)),
        Err(WindowError::InvalidGeometry)
      );
      let settings = Settings {
        position: Position::Specific(crate::iced::Point::new(invalid, 0.0)),
        ..Default::default()
      };
      assert_eq!(windows.open("invalid", settings), Err(WindowError::InvalidSettings));
    }
    let settings = Settings {
      min_size: Some(crate::iced::Size::new(300.0, 200.0)),
      max_size: Some(crate::iced::Size::new(200.0, 100.0)),
      ..Default::default()
    };
    assert_eq!(windows.open("invalid", settings), Err(WindowError::InvalidSettings));
    assert_eq!(windows.ids(), vec![id]);
    assert!(rx.try_recv().is_err());
  }

  #[test]
  fn full_queue_rejects_open_without_registering_a_window_and_recovers_after_drain() {
    let (windows, mut rx) = WindowManager::channel();
    for _ in 0..COMMAND_CAPACITY {
      windows.quit().unwrap();
    }
    assert_eq!(windows.quit(), Err(WindowError::QueueFull));
    assert_eq!(
      windows.open("rejected", Settings::default()),
      Err(WindowError::QueueFull)
    );
    assert!(windows.ids().is_empty());
    let _ = rx.try_recv().unwrap();
    let id = windows.open("accepted", Settings::default()).unwrap();
    assert_eq!(windows.ids(), vec![id]);
    assert_eq!(windows.hide(id), Err(WindowError::QueueFull));
    assert_eq!(windows.set_title(id, "rejected title"), Err(WindowError::QueueFull));
    assert_eq!(windows.title(id).as_deref(), Some("accepted"));
    drop(rx);
    assert_eq!(windows.focus(id), Err(WindowError::ManagerClosed));
  }
}
