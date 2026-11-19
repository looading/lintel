//! GTK-free Linux desktop services. X11 and portal shortcut registration are explicit operations.
//! D-Bus implementations can also be exercised against a private session bus on other Unix hosts.
use std::{
  future::Future,
  sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
  },
  thread::ThreadId,
  time::Duration,
};

use crate::{DesktopError, DesktopEvent, Pending, Result};

#[cfg(feature = "linux-portal-shortcuts")]
pub mod portal;
#[cfg(feature = "linux-tray")]
mod tray;
#[cfg(feature = "linux-x11-shortcuts")]
pub mod x11;

pub(crate) type EventSink = Arc<dyn Fn(DesktopEvent) + Send + Sync>;
type Job = Box<dyn FnOnce() -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + Send>;
const MAX_PENDING: usize = 128;
const DBUS_TIMEOUT: Duration = Duration::from_secs(10);

/// Protocol-level events preserve the semantics of SNI and desktop portals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinuxEvent {
  TrayActivated {
    tray: crate::TrayId,
    secondary: bool,
    x: i32,
    y: i32,
  },
  TrayAvailability {
    tray: crate::TrayId,
    online: bool,
    reason: Option<String>,
  },
  PortalActivated {
    session: String,
    id: String,
    pressed: bool,
    timestamp: u64,
    activation_token: Option<String>,
  },
  PortalBindingsChanged {
    session: String,
    shortcuts: Vec<portal_types::BoundShortcut>,
  },
  BackendStopped {
    backend: &'static str,
    reason: String,
  },
}

/// Shared response types are available independently of the portal feature.
pub mod portal_types {
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct BoundShortcut {
    pub id: String,
    pub description: String,
    pub trigger_description: String,
  }
}

pub(crate) struct CloseSignal {
  closed: AtomicBool,
  semaphore: async_lock::Semaphore,
}
impl CloseSignal {
  fn new() -> Self {
    Self {
      closed: AtomicBool::new(false),
      semaphore: async_lock::Semaphore::new(0),
    }
  }
  pub(crate) fn close(&self) {
    if !self.closed.swap(true, Ordering::AcqRel) {
      self.semaphore.add_permits(1);
    }
  }
  pub(crate) fn is_closed(&self) -> bool {
    self.closed.load(Ordering::Acquire)
  }
  pub(crate) async fn wait(&self) {
    let _guard = self.semaphore.acquire().await;
  }
}

struct Inner {
  closed: AtomicBool,
  pending: AtomicUsize,
  events: EventSink,
  dispatch_thread: ThreadId,
  sender: async_channel::Sender<Job>,
  #[cfg(feature = "linux-tray")]
  tray: async_lock::Mutex<tray::TrayState>,
  sessions: std::sync::Mutex<Vec<std::sync::Weak<CloseSignal>>>,
}

impl Inner {
  fn track(&self, signal: &Arc<CloseSignal>) {
    let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
    sessions.retain(|signal| signal.strong_count() != 0);
    sessions.push(Arc::downgrade(signal));
  }
}

/// Cloneable async service; no GTK loop and no native window objects are stored here.
/// Constructed by DesktopRuntime. Native D-Bus work never blocks its main-thread command drain.
#[derive(Clone)]
pub struct LinuxDesktop {
  inner: Arc<Inner>,
}

struct Admission(Arc<Inner>);
impl Drop for Admission {
  fn drop(&mut self) {
    self.0.pending.fetch_sub(1, Ordering::AcqRel);
  }
}

impl LinuxDesktop {
  pub(crate) fn new(events: EventSink, dispatch_thread: ThreadId) -> Self {
    let (sender, receiver) = async_channel::bounded::<Job>(MAX_PENDING);
    async_global_executor::spawn(async move {
      while let Ok(job) = receiver.recv().await {
        job().await;
      }
    })
    .detach();
    Self {
      inner: Arc::new(Inner {
        closed: AtomicBool::new(false),
        pending: AtomicUsize::new(0),
        events,
        dispatch_thread,
        sender,
        #[cfg(feature = "linux-tray")]
        tray: async_lock::Mutex::new(tray::TrayState::default()),
        sessions: std::sync::Mutex::new(Vec::new()),
      }),
    }
  }

  fn submit<T, F, Fut>(&self, operation: F) -> Result<Pending<T>>
  where
    T: Send + 'static,
    F: FnOnce(Arc<Inner>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<T>> + Send + 'static,
  {
    if self.inner.closed.load(Ordering::Acquire) {
      return Err(DesktopError::RuntimeClosed);
    }
    self
      .inner
      .pending
      .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
        (n < MAX_PENDING).then_some(n + 1)
      })
      .map_err(|_| DesktopError::QueueFull)?;
    let admission = Admission(self.inner.clone());
    let inner = self.inner.clone();
    let (sender, receiver) = futures_channel::oneshot::channel();
    let job: Job = Box::new(move || {
      Box::pin(async move {
        let result = if inner.closed.load(Ordering::Acquire) {
          Err(DesktopError::RuntimeClosed)
        } else {
          operation(inner).await
        };
        let _ = sender.send(result);
        drop(admission);
      })
    });
    self.inner.sender.try_send(job).map_err(|e| match e {
      async_channel::TrySendError::Full(_) => DesktopError::QueueFull,
      async_channel::TrySendError::Closed(_) => DesktopError::RuntimeClosed,
    })?;
    Ok(Pending::from_receiver(receiver, self.inner.dispatch_thread))
  }

  pub(crate) fn close(&self) {
    if self.inner.closed.swap(true, Ordering::AcqRel) {
      return;
    }
    self.inner.sender.close();
    for signal in self
      .inner
      .sessions
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .drain(..)
      .filter_map(|s| s.upgrade())
    {
      signal.close();
    }
    #[cfg(feature = "linux-tray")]
    {
      let inner = self.inner.clone();
      async_global_executor::spawn(async move {
        inner.tray.lock().await.clear();
      })
      .detach();
    }
  }

  /// Connect explicitly to an X11 server. This is never inferred from DISPLAY in a Wayland host.
  #[cfg(feature = "linux-x11-shortcuts")]
  pub fn x11_shortcuts(&self, display: Option<String>) -> Result<Pending<x11::X11Shortcuts>> {
    self.submit(move |inner| async move {
      let manager = deadline(x11::X11Shortcuts::connect(
        display.as_deref(),
        inner.events.clone(),
        inner.dispatch_thread,
      ))
      .await?;
      inner.track(manager.close_signal());
      if inner.closed.load(Ordering::Acquire) {
        manager.close();
        return Err(DesktopError::RuntimeClosed);
      }
      Ok(manager)
    })
  }

  /// Explicitly begin a portal session. Creating the returned future does not call D-Bus.
  /// Binding and any user-facing consent happen only through PortalSession::bind.
  #[cfg(feature = "linux-portal-shortcuts")]
  pub fn portal_session(
    &self,
    options: portal::PortalOptions,
  ) -> impl Future<Output = Result<portal::PortalSession>> + Send + 'static {
    let inner = self.inner.clone();
    async move {
      if inner.closed.load(Ordering::Acquire) {
        return Err(DesktopError::RuntimeClosed);
      }
      let session = portal::PortalSession::connect(options, inner.events.clone()).await?;
      inner.track(session.close_signal());
      if inner.closed.load(Ordering::Acquire) {
        session.close().await?;
        return Err(DesktopError::RuntimeClosed);
      }
      Ok(session)
    }
  }
}

pub(crate) async fn deadline<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
  futures_lite::future::race(future, async {
    futures_timer::Delay::new(DBUS_TIMEOUT).await;
    Err(DesktopError::Timeout(DBUS_TIMEOUT))
  })
  .await
}

pub(crate) fn next_id() -> u64 {
  static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 48);
  NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn runtime_close_rejects_new_linux_work() {
    let service = LinuxDesktop::new(Arc::new(|_| {}), std::thread::current().id());
    service.close();
    assert!(matches!(
      service.submit(|_| async { Ok(()) }),
      Err(DesktopError::RuntimeClosed)
    ));
  }
  #[test]
  fn async_fifo_preserves_submission_order_and_bounded_admission() {
    futures_lite::future::block_on(async {
      let service = LinuxDesktop::new(Arc::new(|_| {}), std::thread::current().id());
      let (release, gate) = futures_channel::oneshot::channel();
      let first = service
        .submit(move |_| async move {
          let _ = gate.await;
          Ok(0usize)
        })
        .unwrap();
      let mut pending = Vec::new();
      let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
      for index in 1..MAX_PENDING {
        let seen = seen.clone();
        pending.push(
          service
            .submit(move |_| async move {
              seen.lock().unwrap().push(index);
              Ok(index)
            })
            .unwrap(),
        );
      }
      assert!(matches!(
        service.submit(|_| async { Ok(()) }),
        Err(DesktopError::QueueFull)
      ));
      release.send(()).unwrap();
      assert_eq!(first.await.unwrap(), 0);
      for (index, pending) in pending.into_iter().enumerate() {
        assert_eq!(pending.await.unwrap(), index + 1);
      }
      assert_eq!(*seen.lock().unwrap(), (1..MAX_PENDING).collect::<Vec<_>>());
      service.close();
    });
  }
  #[test]
  fn protocol_event_inbox_is_bounded_and_reports_loss() {
    futures_lite::future::block_on(async {
      let (mut runtime, service) = crate::runtime::DesktopRuntime::create(Arc::new(|| {}));
      let linux = service.linux();
      linux
        .submit(|inner| async move {
          for i in 0..130 {
            (inner.events)(DesktopEvent::Menu {
              menu: crate::MenuId(1),
              item: i.to_string(),
            });
          }
          Ok(())
        })
        .unwrap()
        .await
        .unwrap();
      assert_eq!(runtime.take_events(16).len(), 16);
      assert_eq!(runtime.take_events(256).len(), 112);
      assert_eq!(runtime.take_event_overflow_count(), 2);
      assert_eq!(runtime.take_event_overflow_count(), 0);
    });
  }
}
