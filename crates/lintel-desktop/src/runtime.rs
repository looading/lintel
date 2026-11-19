use crate::DesktopEvent;
use std::{
  cell::RefCell,
  future::Future,
  marker::PhantomData,
  path::PathBuf,
  pin::Pin,
  rc::Rc,
  sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc::{self, Receiver, SyncSender, TrySendError},
  },
  task::{Context, Poll},
  thread::{self, ThreadId},
  time::Duration,
};

use crate::{
  DesktopError, FileDialogKind, FileDialogOptions, FileSelection, MenuId, MenuSpec, Notification, Result, ShortcutId,
  TrayIcon, TrayId, TraySpec, backend::Backend,
};

type Job = Box<dyn FnOnce(&mut Backend) + Send>;
type Wake = Arc<dyn Fn() + Send + Sync>;
const QUEUE_CAPACITY: usize = 128;

#[cfg(feature = "windows-menu")]
pub(crate) struct WindowsMenuLeaseSignal {
  wake: Wake,
}
#[cfg(feature = "windows-menu")]
impl Drop for WindowsMenuLeaseSignal {
  fn drop(&mut self) {
    (self.wake)();
  }
}

/// Scoped window registration. Drop the last clone on logical close to wake main-thread detach;
/// Drop never submits to the bounded command queue. The lease contains no native object.
#[cfg(feature = "windows-menu")]
#[derive(Clone)]
pub struct WindowMenuLease {
  target: crate::WindowMenuTarget,
  _signal: Arc<WindowsMenuLeaseSignal>,
}
#[cfg(feature = "windows-menu")]
impl WindowMenuLease {
  pub fn target(&self) -> crate::WindowMenuTarget {
    self.target
  }
}
#[cfg(feature = "windows-menu")]
impl std::fmt::Debug for WindowMenuLease {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("WindowMenuLease")
      .field("target", &self.target)
      .finish_non_exhaustive()
  }
}

#[cfg(all(feature = "windows-menu", any(target_os = "windows", test)))]
#[derive(Default)]
pub(crate) struct WindowsMenuLeases {
  entries: std::collections::HashMap<crate::WindowMenuTarget, std::sync::Weak<WindowsMenuLeaseSignal>>,
}
#[cfg(all(feature = "windows-menu", any(target_os = "windows", test)))]
impl WindowsMenuLeases {
  pub(crate) fn watch(&mut self, target: crate::WindowMenuTarget, signal: &Arc<WindowsMenuLeaseSignal>) {
    self.entries.insert(target, Arc::downgrade(signal));
  }
  pub(crate) fn remove(&mut self, target: crate::WindowMenuTarget) {
    self.entries.remove(&target);
  }
  pub(crate) fn expired(&mut self) -> Vec<crate::WindowMenuTarget> {
    let expired: Vec<_> = self
      .entries
      .iter()
      .filter(|(_, signal)| signal.strong_count() == 0)
      .map(|(target, _)| *target)
      .collect();
    for target in &expired {
      self.entries.remove(target);
    }
    expired
  }
}

/// Result of a queued native command. Dropping a pending result does not cancel the command.
/// A timeout also does not cancel a queued side effect.
pub struct Pending<T> {
  receiver: RefCell<futures_channel::oneshot::Receiver<Result<T>>>,
  dispatch_thread: ThreadId,
}

impl<T> Pending<T> {
  #[allow(dead_code)]
  pub(crate) fn from_receiver(
    receiver: futures_channel::oneshot::Receiver<Result<T>>,
    dispatch_thread: ThreadId,
  ) -> Self {
    Self {
      receiver: RefCell::new(receiver),
      dispatch_thread,
    }
  }
  pub fn try_recv(&self) -> Option<Result<T>> {
    match self.receiver.borrow_mut().try_recv() {
      Ok(Some(result)) => Some(result),
      Ok(None) => None,
      Err(_) => Some(Err(DesktopError::RuntimeClosed)),
    }
  }

  /// Wait only from a worker. Main-thread callers may await without blocking the native loop,
  /// or drain the runtime and use try_recv. A synchronous premature wait is rejected.
  pub fn wait(self) -> Result<T> {
    if let Some(result) = self.try_recv() {
      return result;
    }
    if thread::current().id() == self.dispatch_thread {
      return Err(DesktopError::WouldBlockMainThread);
    }
    futures_lite::future::block_on(self)
  }

  pub fn wait_timeout(self, timeout: Duration) -> Result<T> {
    if let Some(result) = self.try_recv() {
      return result;
    }
    if thread::current().id() == self.dispatch_thread {
      return Err(DesktopError::WouldBlockMainThread);
    }
    futures_lite::future::block_on(futures_lite::future::race(self, async move {
      futures_timer::Delay::new(timeout).await;
      Err(DesktopError::Timeout(timeout))
    }))
  }
}

impl<T> Future for Pending<T> {
  type Output = Result<T>;
  fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
    // oneshot owns the race-free sender/poll wake protocol and wakes on sender/job drop.
    Pin::new(self.get_mut().receiver.get_mut())
      .poll(context)
      .map(|result| result.unwrap_or(Err(DesktopError::RuntimeClosed)))
  }
}

/// Cloneable, bounded command sender. Native objects never enter this service or its closures.
#[derive(Clone)]
pub struct DesktopService {
  sender: SyncSender<Job>,
  wake: Wake,
  dispatch_thread: ThreadId,
  #[cfg(all(
    unix,
    any(
      feature = "linux-tray",
      feature = "linux-x11-shortcuts",
      feature = "linux-portal-shortcuts"
    )
  ))]
  linux: crate::linux::LinuxDesktop,
}

impl DesktopService {
  #[cfg(all(
    unix,
    any(
      feature = "linux-tray",
      feature = "linux-x11-shortcuts",
      feature = "linux-portal-shortcuts"
    )
  ))]
  pub fn linux(&self) -> &crate::linux::LinuxDesktop {
    &self.linux
  }
  fn submit<T: Send + 'static>(
    &self,
    operation: impl FnOnce(&mut Backend) -> Result<T> + Send + 'static,
  ) -> Result<Pending<T>> {
    let (sender, receiver) = futures_channel::oneshot::channel();
    let job: Job = Box::new(move |backend| {
      let _ = sender.send(operation(backend));
    });
    self.sender.try_send(job).map_err(|error| match error {
      TrySendError::Full(_) => DesktopError::QueueFull,
      TrySendError::Disconnected(_) => DesktopError::RuntimeClosed,
    })?;
    (self.wake)();
    Ok(Pending {
      receiver: RefCell::new(receiver),
      dispatch_thread: self.dispatch_thread,
    })
  }

  /// Register an owned native window through the same main-thread FIFO. The returned token is
  /// the only business-facing menu target; ownership is held until explicit unregister.
  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  pub fn register_windows_window<W: crate::windows_menu::HasWindowHandle + Send + Sync + 'static + ?Sized>(
    &self,
    owner: Arc<W>,
  ) -> Result<Pending<crate::WindowMenuTarget>> {
    self.submit(move |backend| backend.ui.register_windows_window(owner))
  }
  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  pub fn register_windows_window_scoped<W: crate::windows_menu::HasWindowHandle + Send + Sync + 'static + ?Sized>(
    &self,
    owner: Arc<W>,
  ) -> Result<Pending<WindowMenuLease>> {
    let wake = self.wake.clone();
    self.submit(move |backend| {
      let target = backend.ui.register_windows_window(owner)?;
      let signal = Arc::new(WindowsMenuLeaseSignal { wake });
      backend.ui.watch_window_lease(target, &signal);
      Ok(WindowMenuLease {
        target,
        _signal: signal,
      })
    })
  }
  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  pub fn unregister_windows_window(&self, target: crate::WindowMenuTarget) -> Result<Pending<()>> {
    self.submit(move |backend| backend.ui.unregister_windows_window(target))
  }
  pub fn set_window_menu(&self, target: crate::WindowMenuTarget, menu: Option<MenuId>) -> Result<Pending<()>> {
    self.submit(move |backend| backend.set_window_menu(target, menu))
  }

  #[allow(unreachable_code)]
  pub fn create_menu(&self, spec: MenuSpec) -> Result<Pending<MenuId>> {
    #[cfg(all(target_os = "linux", feature = "linux-tray"))]
    {
      return self.linux.create_menu(spec);
    }
    spec.validate()?;
    self.submit(move |backend| backend.create_menu(spec))
  }
  #[allow(unreachable_code)]
  pub fn remove_menu(&self, id: MenuId) -> Result<Pending<()>> {
    #[cfg(all(target_os = "linux", feature = "linux-tray"))]
    {
      return self.linux.remove_menu(id);
    }
    self.submit(move |backend| backend.remove_menu(id))
  }
  /// macOS global application menu. Windows/Linux window-menu attachment is unsupported here.
  pub fn set_application_menu(&self, id: Option<MenuId>) -> Result<Pending<()>> {
    self.submit(move |backend| backend.set_application_menu(id))
  }
  #[allow(unreachable_code)]
  pub fn set_menu_item_enabled(&self, menu: MenuId, item: String, enabled: bool) -> Result<Pending<()>> {
    #[cfg(all(target_os = "linux", feature = "linux-tray"))]
    {
      return self.linux.set_menu_item_enabled(menu, item, enabled);
    }
    self.submit(move |backend| backend.set_menu_item_enabled(menu, &item, enabled))
  }
  #[allow(unreachable_code)]
  pub fn set_menu_item_checked(&self, menu: MenuId, item: String, checked: bool) -> Result<Pending<()>> {
    #[cfg(all(target_os = "linux", feature = "linux-tray"))]
    {
      return self.linux.set_menu_item_checked(menu, item, checked);
    }
    self.submit(move |backend| backend.set_menu_item_checked(menu, &item, checked))
  }
  #[allow(unreachable_code)]
  pub fn set_menu_item_label(&self, menu: MenuId, item: String, label: String) -> Result<Pending<()>> {
    #[cfg(all(target_os = "linux", feature = "linux-tray"))]
    {
      return self.linux.set_menu_item_label(menu, item, label);
    }
    crate::model::validate_text("menu label", &label, 4096)?;
    self.submit(move |backend| backend.set_menu_item_label(menu, &item, &label))
  }
  #[allow(unreachable_code)]
  pub fn create_tray(&self, spec: TraySpec) -> Result<Pending<TrayId>> {
    #[cfg(all(target_os = "linux", feature = "linux-tray"))]
    {
      return self.linux.create_tray(spec);
    }
    spec.icon.validate()?;
    self.submit(move |backend| backend.create_tray(spec))
  }
  #[allow(unreachable_code)]
  pub fn remove_tray(&self, id: TrayId) -> Result<Pending<()>> {
    #[cfg(all(target_os = "linux", feature = "linux-tray"))]
    {
      return self.linux.remove_tray(id);
    }
    self.submit(move |backend| backend.remove_tray(id))
  }
  #[allow(unreachable_code)]
  pub fn set_tray_menu(&self, id: TrayId, menu: Option<MenuId>) -> Result<Pending<()>> {
    #[cfg(all(target_os = "linux", feature = "linux-tray"))]
    {
      return self.linux.set_tray_menu(id, menu);
    }
    self.submit(move |backend| backend.set_tray_menu(id, menu))
  }
  #[allow(unreachable_code)]
  pub fn set_tray_visible(&self, id: TrayId, visible: bool) -> Result<Pending<()>> {
    #[cfg(all(target_os = "linux", feature = "linux-tray"))]
    {
      return self.linux.set_tray_visible(id, visible);
    }
    self.submit(move |backend| backend.set_tray_visible(id, visible))
  }
  #[allow(unreachable_code)]
  pub fn set_tray_icon(&self, id: TrayId, icon: TrayIcon) -> Result<Pending<()>> {
    #[cfg(all(target_os = "linux", feature = "linux-tray"))]
    {
      return self.linux.set_tray_icon(id, icon);
    }
    icon.validate()?;
    self.submit(move |backend| backend.set_tray_icon(id, icon))
  }
  #[allow(unreachable_code)]
  pub fn set_tray_tooltip(&self, id: TrayId, tooltip: Option<String>) -> Result<Pending<()>> {
    #[cfg(all(target_os = "linux", feature = "linux-tray"))]
    {
      return self.linux.set_tray_tooltip(id, tooltip);
    }
    self.submit(move |backend| backend.set_tray_tooltip(id, tooltip))
  }
  pub fn register_shortcut(&self, accelerator: String) -> Result<Pending<ShortcutId>> {
    self.submit(move |backend| backend.register_shortcut(&accelerator))
  }
  pub fn unregister_shortcut(&self, id: ShortcutId) -> Result<Pending<()>> {
    self.submit(move |backend| backend.unregister_shortcut(id))
  }
  pub fn read_clipboard_text(&self) -> Result<Pending<String>> {
    self.submit(Backend::read_clipboard_text)
  }
  pub fn write_clipboard_text(&self, text: String) -> Result<Pending<()>> {
    self.submit(move |backend| backend.write_clipboard_text(text))
  }
  pub fn clear_clipboard(&self) -> Result<Pending<()>> {
    self.submit(Backend::clear_clipboard)
  }
  /// A synchronous native dialog runs on the UI thread; its native modal loop handles dialog events.
  pub fn show_file_dialog(
    &self,
    kind: FileDialogKind,
    options: FileDialogOptions,
  ) -> Result<Pending<Option<FileSelection>>> {
    self.submit(move |backend| backend.show_file_dialog(kind, options))
  }
  /// Successful submission to the OS does not guarantee the user saw a notification.
  pub fn show_notification(&self, notification: Notification) -> Result<Pending<()>> {
    self.submit(move |backend| backend.show_notification(notification))
  }
  pub fn open_url(&self, url: String) -> Result<Pending<()>> {
    self.submit(move |_| crate::opener::open_url(&url))
  }
  pub fn open_path(&self, path: PathBuf) -> Result<Pending<()>> {
    self.submit(move |_| crate::opener::open_path(&path))
  }
}

/// Owner of all native resources. Create after the native event loop starts, retain on its UI thread,
/// call `drain` from a wake event, and drop on that same thread before the event loop exits.
///
/// ```compile_fail
/// # use lintel_desktop::DesktopRuntime;
/// let (runtime, _) = DesktopRuntime::new(|| {}).unwrap();
/// std::thread::spawn(move || drop(runtime)); // native resources cannot cross threads
/// ```
pub struct DesktopRuntime {
  receiver: Receiver<Job>,
  backend: Backend,
  events: Receiver<DesktopEvent>,
  events_lost: Arc<AtomicU64>,
  #[cfg(all(
    unix,
    any(
      feature = "linux-tray",
      feature = "linux-x11-shortcuts",
      feature = "linux-portal-shortcuts"
    )
  ))]
  linux: crate::linux::LinuxDesktop,
  wake: Wake,
  _main_thread: PhantomData<Rc<()>>,
}

impl DesktopRuntime {
  pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Result<(Self, DesktopService)> {
    #[cfg(all(feature = "native-ui", target_os = "macos"))]
    if objc2::MainThreadMarker::new().is_none() {
      return Err(DesktopError::MainThreadRequired);
    }
    Ok(Self::create(Arc::new(wake)))
  }

  pub(crate) fn create(wake: Wake) -> (Self, DesktopService) {
    let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
    let (event_sender, events) = mpsc::sync_channel(QUEUE_CAPACITY);
    let events_lost = Arc::new(AtomicU64::new(0));
    #[cfg(all(
      unix,
      any(
        feature = "linux-tray",
        feature = "linux-x11-shortcuts",
        feature = "linux-portal-shortcuts"
      )
    ))]
    let linux = {
      let wake = wake.clone();
      let lost = events_lost.clone();
      crate::linux::LinuxDesktop::new(
        Arc::new(move |event| {
          if let Err(TrySendError::Full(_)) = event_sender.try_send(event) {
            lost.fetch_add(1, Ordering::Relaxed);
          }
          wake();
        }),
        thread::current().id(),
      )
    };
    #[cfg(not(all(
      unix,
      any(
        feature = "linux-tray",
        feature = "linux-x11-shortcuts",
        feature = "linux-portal-shortcuts"
      )
    )))]
    drop(event_sender);
    let service = DesktopService {
      sender,
      wake: wake.clone(),
      dispatch_thread: thread::current().id(),
      #[cfg(all(
        unix,
        any(
          feature = "linux-tray",
          feature = "linux-x11-shortcuts",
          feature = "linux-portal-shortcuts"
        )
      ))]
      linux: linux.clone(),
    };
    (
      Self {
        receiver,
        backend: Backend::default(),
        events,
        events_lost,
        #[cfg(all(
          unix,
          any(
            feature = "linux-tray",
            feature = "linux-x11-shortcuts",
            feature = "linux-portal-shortcuts"
          )
        ))]
        linux,
        wake,
        _main_thread: PhantomData,
      },
      service,
    )
  }

  /// Drain protocol events on the host thread after commands, preserving bounded fairness.
  pub fn take_events(&mut self, limit: usize) -> Vec<DesktopEvent> {
    let events = self.events.try_iter().take(limit).collect::<Vec<_>>();
    if limit != 0 && events.len() == limit {
      (self.wake)();
    }
    events
  }
  /// Number of dropped background events since the previous call (queue capacity is 128).
  pub fn take_event_overflow_count(&self) -> u64 {
    self.events_lost.swap(0, Ordering::AcqRel)
  }

  /// Process at most `limit` commands; schedules another wake if the limit is reached.
  /// Return value is the number processed, allowing the host to maintain a fair event loop.
  pub fn drain(&mut self, limit: usize) -> usize {
    #[cfg(all(feature = "windows-menu", target_os = "windows"))]
    self.backend.ui.sweep_window_leases();
    let mut processed = 0;
    while processed < limit {
      let Ok(job) = self.receiver.try_recv() else {
        break;
      };
      job(&mut self.backend);
      processed += 1;
    }
    #[cfg(all(feature = "windows-menu", target_os = "windows"))]
    self.backend.ui.sweep_window_leases();
    if limit != 0 && processed == limit {
      (self.wake)();
    }
    processed
  }

  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  pub fn register_windows_window<W: crate::windows_menu::HasWindowHandle + Send + Sync + 'static + ?Sized>(
    &mut self,
    owner: Arc<W>,
  ) -> Result<crate::WindowMenuTarget> {
    self.backend.ui.register_windows_window(owner)
  }
  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  pub fn register_windows_window_scoped<W: crate::windows_menu::HasWindowHandle + Send + Sync + 'static + ?Sized>(
    &mut self,
    owner: Arc<W>,
  ) -> Result<WindowMenuLease> {
    let target = self.backend.ui.register_windows_window(owner)?;
    let signal = Arc::new(WindowsMenuLeaseSignal {
      wake: self.wake.clone(),
    });
    self.backend.ui.watch_window_lease(target, &signal);
    Ok(WindowMenuLease {
      target,
      _signal: signal,
    })
  }
  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  pub fn take_window_menu_cleanup_errors(&mut self) -> Vec<DesktopError> {
    self.backend.ui.take_window_menu_cleanup_errors()
  }
  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  pub fn unregister_windows_window(&mut self, target: crate::WindowMenuTarget) -> Result<()> {
    self.backend.ui.unregister_windows_window(target)
  }
  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  pub fn windows_menu_dispatcher(&self) -> crate::windows_menu::WindowsMenuDispatcher {
    self.backend.ui.window_menu_dispatcher()
  }

  /// Route existing native events without installing competing global handlers.
  #[cfg(all(feature = "native-ui", any(target_os = "macos", target_os = "windows")))]
  pub fn menu_event(&self, event: &muda::MenuEvent) -> Option<DesktopEvent> {
    self.backend.ui.menu_event(event)
  }
  #[cfg(all(feature = "native-ui", any(target_os = "macos", target_os = "windows")))]
  pub fn tray_event(&self, event: &tray_icon::TrayIconEvent) -> Option<DesktopEvent> {
    self.backend.ui.tray_event(event)
  }
  #[cfg(all(feature = "native-ui", any(target_os = "macos", target_os = "windows")))]
  pub fn shortcut_event(&self, event: &global_hotkey::GlobalHotKeyEvent) -> Option<DesktopEvent> {
    self.backend.ui.shortcut_event(event)
  }
}

impl Drop for DesktopRuntime {
  fn drop(&mut self) {
    #[cfg(all(
      unix,
      any(
        feature = "linux-tray",
        feature = "linux-x11-shortcuts",
        feature = "linux-portal-shortcuts"
      )
    ))]
    self.linux.close();
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use static_assertions::{assert_impl_all, assert_not_impl_any};
  use std::sync::atomic::{AtomicUsize, Ordering};

  assert_impl_all!(DesktopService: Send, Sync, Clone);
  assert_not_impl_any!(DesktopRuntime: Send, Sync);

  #[cfg(all(feature = "native-ui", target_os = "macos"))]
  #[test]
  fn native_runtime_constructor_rejects_a_worker_thread() {
    let rejected = thread::spawn(|| matches!(DesktopRuntime::new(|| {}), Err(DesktopError::MainThreadRequired)));
    assert!(rejected.join().unwrap());
  }

  #[cfg(feature = "windows-menu")]
  #[test]
  fn cancelled_registration_result_expires_lease_without_command_queue_admission() {
    let wake_count = Arc::new(AtomicUsize::new(0));
    let observed = wake_count.clone();
    let (mut runtime, service) = DesktopRuntime::create(Arc::new(|| {}));
    let signal = Arc::new(WindowsMenuLeaseSignal {
      wake: Arc::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
      }),
    });
    let mut leases = WindowsMenuLeases::default();
    let target = crate::WindowMenuTarget(123);
    leases.watch(target, &signal);
    let pending = service
      .submit(move |_| {
        Ok(WindowMenuLease {
          target,
          _signal: signal,
        })
      })
      .unwrap();
    drop(pending);
    runtime.drain(64);
    assert_eq!(leases.expired(), vec![target]);
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);
  }

  #[cfg(feature = "windows-menu")]
  #[test]
  fn last_scoped_lease_drop_wakes_even_with_full_command_queue() {
    let (runtime, service) = DesktopRuntime::create(Arc::new(|| {}));
    for _ in 0..QUEUE_CAPACITY {
      service.submit(|_| Ok(())).unwrap();
    }
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let signal = Arc::new(WindowsMenuLeaseSignal {
      wake: Arc::new(move || {
        observed.fetch_add(1, Ordering::SeqCst);
      }),
    });
    let target = crate::WindowMenuTarget(456);
    let mut leases = WindowsMenuLeases::default();
    leases.watch(target, &signal);
    let lease = WindowMenuLease {
      target,
      _signal: signal,
    };
    let clone = lease.clone();
    drop(lease);
    assert!(leases.expired().is_empty());
    drop(clone);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(leases.expired(), vec![target]);
    drop(runtime);
  }

  struct WakeCounter(AtomicUsize);
  impl std::task::Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
      self.0.fetch_add(1, Ordering::SeqCst);
    }
  }

  #[test]
  fn awaiting_reply_handles_poll_before_send_and_send_before_poll() {
    let (mut runtime, service) = DesktopRuntime::create(Arc::new(|| {}));
    let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waker = std::task::Waker::from(counter.clone());
    let mut context = Context::from_waker(&waker);
    let mut pending = Box::pin(service.submit(|_| Ok(42)).unwrap());
    assert!(pending.as_mut().poll(&mut context).is_pending());
    runtime.drain(64);
    assert!(counter.0.load(Ordering::SeqCst) > 0);
    assert!(matches!(pending.as_mut().poll(&mut context), Poll::Ready(Ok(42))));
    let pending = service.submit(|_| Ok(73)).unwrap();
    runtime.drain(64);
    assert_eq!(futures_lite::future::block_on(pending).unwrap(), 73);
  }

  #[test]
  fn runtime_job_drop_wakes_awaiting_receiver_with_disconnect() {
    let (runtime, service) = DesktopRuntime::create(Arc::new(|| {}));
    let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waker = std::task::Waker::from(counter.clone());
    let mut context = Context::from_waker(&waker);
    let mut pending = Box::pin(service.submit(|_| Ok(())).unwrap());
    assert!(pending.as_mut().poll(&mut context).is_pending());
    drop(runtime);
    assert!(counter.0.load(Ordering::SeqCst) > 0);
    assert!(matches!(
      pending.as_mut().poll(&mut context),
      Poll::Ready(Err(DesktopError::RuntimeClosed))
    ));
  }

  #[test]
  fn reply_and_first_poll_can_race_without_lost_wakeup() {
    for index in 0..64 {
      let (sender, receiver) = futures_channel::oneshot::channel();
      let pending = Pending {
        receiver: RefCell::new(receiver),
        dispatch_thread: thread::current().id(),
      };
      let barrier = Arc::new(std::sync::Barrier::new(2));
      let worker = barrier.clone();
      let sender_thread = thread::spawn(move || {
        worker.wait();
        let _ = sender.send(Ok(index));
      });
      barrier.wait();
      assert_eq!(futures_lite::future::block_on(pending).unwrap(), index);
      sender_thread.join().unwrap();
    }
  }

  #[cfg(feature = "windows-menu")]
  #[test]
  fn explicit_unregister_removes_lease_watch_before_last_drop() {
    let signal = Arc::new(WindowsMenuLeaseSignal { wake: Arc::new(|| {}) });
    let target = crate::WindowMenuTarget(789);
    let mut leases = WindowsMenuLeases::default();
    leases.watch(target, &signal);
    leases.remove(target);
    drop(signal);
    assert!(leases.expired().is_empty());
  }

  #[test]
  fn commands_execute_only_when_ui_runtime_drains() {
    let wake_count = Arc::new(AtomicUsize::new(0));
    let count = wake_count.clone();
    let (mut runtime, service) = DesktopRuntime::create(Arc::new(move || {
      count.fetch_add(1, Ordering::SeqCst);
    }));
    let pending = service.submit(|_| Ok(thread::current().id())).unwrap();
    assert!(pending.try_recv().is_none());
    assert_eq!(wake_count.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.drain(64), 1);
    assert_eq!(pending.wait().unwrap(), thread::current().id());
  }

  #[test]
  fn waiting_on_dispatch_thread_is_rejected() {
    let (_runtime, service) = DesktopRuntime::create(Arc::new(|| {}));
    assert!(matches!(
      service.submit(|_| Ok(())).unwrap().wait(),
      Err(DesktopError::WouldBlockMainThread)
    ));
  }

  #[test]
  fn bounded_queue_and_shutdown_have_distinct_errors() {
    let (runtime, service) = DesktopRuntime::create(Arc::new(|| {}));
    for _ in 0..QUEUE_CAPACITY {
      service.submit(|_| Ok(())).unwrap();
    }
    assert!(matches!(service.submit(|_| Ok(())), Err(DesktopError::QueueFull)));
    drop(runtime);
    assert!(matches!(service.submit(|_| Ok(())), Err(DesktopError::RuntimeClosed)));
  }

  #[test]
  fn worker_receives_result_from_dispatch_thread() {
    let (mut runtime, service) = DesktopRuntime::create(Arc::new(|| {}));
    let pending = service.submit(|_| Ok(42)).unwrap();
    let worker = thread::spawn(move || pending.wait_timeout(Duration::from_secs(2)));
    runtime.drain(64);
    assert_eq!(worker.join().unwrap().unwrap(), 42);
  }
}
