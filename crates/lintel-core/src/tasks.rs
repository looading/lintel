//! 按应用、窗口和页面生命周期管理异步任务。

use std::{
  collections::HashMap,
  future::Future,
  pin::Pin,
  sync::{
    Arc, OnceLock, Weak,
    atomic::{AtomicBool, Ordering},
  },
  task::{Context, Poll},
};

use parking_lot::Mutex;
use tokio::{
  runtime::Handle,
  task::{AbortHandle, JoinError, JoinHandle},
};

/// 任务所属的生命周期；窗口取消会同时取消该窗口的页面任务。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum TaskScope {
  Application,
  Window(u64),
  Page { window: u64, page: String },
}

impl TaskScope {
  pub fn page(window: u64, page: impl Into<String>) -> Self {
    Self::Page {
      window,
      page: page.into(),
    }
  }

  fn contains(&self, other: &Self) -> bool {
    match (self, other) {
      (Self::Application, _) => true,
      (Self::Window(parent), Self::Window(window) | Self::Page { window, .. }) => parent == window,
      _ => self == other,
    }
  }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum TaskSpawnError {
  #[error("application task tracker is shutting down")]
  ShuttingDown,
  #[error("application task ID space is exhausted")]
  IdExhausted,
}

struct TaskState {
  abort: OnceLock<AbortHandle>,
  cancelled: AtomicBool,
  finished: AtomicBool,
}

impl TaskState {
  fn cancel(&self) {
    self.cancelled.store(true, Ordering::Release);
    if let Some(abort) = self.abort.get() {
      abort.abort();
    }
  }
}

/// 可克隆的取消句柄；取消后仍可通过原任务句柄等待清理完成。
#[derive(Clone)]
pub struct TaskCancellation {
  state: Arc<TaskState>,
}

impl TaskCancellation {
  pub fn cancel(&self) {
    self.state.cancel();
  }

  pub fn is_cancelled(&self) -> bool {
    self.state.cancelled.load(Ordering::Acquire)
  }

  pub fn is_finished(&self) -> bool {
    self.state.finished.load(Ordering::Acquire)
  }
}

/// 受追踪的 Tokio 任务；丢弃句柄不会使任务脱离所属生命周期。
pub struct TaskHandle<T> {
  id: u64,
  scope: TaskScope,
  cancellation: TaskCancellation,
  join: JoinHandle<T>,
}

impl<T> TaskHandle<T> {
  pub fn id(&self) -> u64 {
    self.id
  }

  pub fn scope(&self) -> &TaskScope {
    &self.scope
  }

  pub fn cancel(&self) {
    self.cancellation.cancel();
  }

  pub fn cancellation_handle(&self) -> TaskCancellation {
    self.cancellation.clone()
  }

  pub fn is_finished(&self) -> bool {
    self.cancellation.is_finished()
  }

  /// 等待完成或取消，返回 Tokio 的任务退出原因。
  pub async fn join(self) -> Result<T, JoinError> {
    self.join.await
  }
}

struct TrackedTask {
  scope: TaskScope,
  state: Arc<TaskState>,
}

#[derive(Default)]
struct Registry {
  next_id: u64,
  shutting_down: bool,
  tasks: HashMap<u64, TrackedTask>,
}

impl Drop for Registry {
  fn drop(&mut self) {
    for task in self.tasks.values() {
      task.state.cancel();
    }
  }
}

struct Completion {
  id: u64,
  state: Arc<TaskState>,
  registry: Weak<Mutex<Registry>>,
  notification: Weak<tokio::sync::Notify>,
}

impl Drop for Completion {
  fn drop(&mut self) {
    self.state.finished.store(true, Ordering::Release);
    if let Some(registry) = self.registry.upgrade() {
      registry.lock().tasks.remove(&self.id);
    }
    if let Some(notification) = self.notification.upgrade() {
      notification.notify_waiters();
    }
  }
}

// 字段按声明顺序销毁，保证等待方看到空记录时业务 Future 已完成析构。
struct TrackedFuture<F> {
  future: Pin<Box<F>>,
  _completion: Completion,
}

impl<F: Future> Future for TrackedFuture<F> {
  type Output = F::Output;

  fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
    self.future.as_mut().poll(context)
  }
}

/// 追踪任务直到 Future 被销毁，关闭后拒绝启动新任务。
#[derive(Clone, Default)]
pub struct TaskTracker {
  registry: Arc<Mutex<Registry>>,
  notification: Arc<tokio::sync::Notify>,
}

impl TaskTracker {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn spawn<F>(&self, runtime: &Handle, scope: TaskScope, future: F) -> Result<TaskHandle<F::Output>, TaskSpawnError>
  where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
  {
    let state = Arc::new(TaskState {
      abort: OnceLock::new(),
      cancelled: AtomicBool::new(false),
      finished: AtomicBool::new(false),
    });
    let id = {
      let mut registry = self.registry.lock();
      if registry.shutting_down {
        return Err(TaskSpawnError::ShuttingDown);
      }
      let id = registry.next_id;
      registry.next_id = id.checked_add(1).ok_or(TaskSpawnError::IdExhausted)?;
      registry.tasks.insert(
        id,
        TrackedTask {
          scope: scope.clone(),
          state: state.clone(),
        },
      );
      id
    };
    // 在 spawn 前创建守卫，尚未首次 poll 就被取消也能释放追踪记录。
    let completion = Completion {
      id,
      state: state.clone(),
      registry: Arc::downgrade(&self.registry),
      notification: Arc::downgrade(&self.notification),
    };
    let join = runtime.spawn(TrackedFuture {
      future: Box::pin(future),
      _completion: completion,
    });
    let _ = state.abort.set(join.abort_handle());
    // 处理注册记录与取得 AbortHandle 之间发生的并发关闭。
    if state.cancelled.load(Ordering::Acquire) {
      state.cancel();
    }
    Ok(TaskHandle {
      id,
      scope,
      cancellation: TaskCancellation { state },
      join,
    })
  }

  /// 取消当前作用域及其子作用域的任务；页面再次进入后可启动新任务。
  pub fn cancel_scope(&self, scope: &TaskScope) -> usize {
    let tasks: Vec<_> = self
      .registry
      .lock()
      .tasks
      .values()
      .filter(|task| scope.contains(&task.scope))
      .map(|task| task.state.clone())
      .collect();
    let count = tasks.len();
    for task in tasks {
      task.cancel();
    }
    count
  }

  /// 取消所有任务并永久拒绝新任务；重复调用不产生额外清理。
  pub fn shutdown(&self) -> usize {
    let tasks: Vec<_> = {
      let mut registry = self.registry.lock();
      if registry.shutting_down {
        return 0;
      }
      registry.shutting_down = true;
      registry.tasks.values().map(|task| task.state.clone()).collect()
    };
    let count = tasks.len();
    for task in tasks {
      task.cancel();
    }
    count
  }

  pub fn is_shutting_down(&self) -> bool {
    self.registry.lock().shutting_down
  }

  pub fn active_count(&self) -> usize {
    self.registry.lock().tasks.len()
  }

  /// 等待当前作用域内的 Future 被销毁；通常在 cancel_scope 之后调用。
  pub async fn wait_scope(&self, scope: &TaskScope) {
    loop {
      let notification = self.notification.notified();
      tokio::pin!(notification);
      notification.as_mut().enable();
      if !self
        .registry
        .lock()
        .tasks
        .values()
        .any(|task| scope.contains(&task.scope))
      {
        return;
      }
      notification.await;
    }
  }

  pub async fn wait_idle(&self) {
    self.wait_scope(&TaskScope::Application).await;
  }
}

#[cfg(test)]
mod tests {
  use std::{
    future::pending,
    sync::{
      Arc,
      atomic::{AtomicUsize, Ordering},
    },
  };

  use super::*;

  struct DropSignal(Arc<AtomicUsize>);

  impl Drop for DropSignal {
    fn drop(&mut self) {
      self.0.fetch_add(1, Ordering::SeqCst);
    }
  }

  fn pending_task(signal: Arc<AtomicUsize>) -> impl Future<Output = ()> + Send {
    let signal = DropSignal(signal);
    async move {
      let _signal = signal;
      pending().await
    }
  }

  #[tokio::test]
  async fn window_cancellation_includes_its_pages_and_keeps_other_owners() {
    let tracker = TaskTracker::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let app = tracker
      .spawn(&Handle::current(), TaskScope::Application, pending_task(drops.clone()))
      .unwrap();
    let window = tracker
      .spawn(&Handle::current(), TaskScope::Window(1), pending_task(drops.clone()))
      .unwrap();
    let page = tracker
      .spawn(
        &Handle::current(),
        TaskScope::page(1, "settings"),
        pending_task(drops.clone()),
      )
      .unwrap();
    let other = tracker
      .spawn(
        &Handle::current(),
        TaskScope::page(2, "settings"),
        pending_task(drops.clone()),
      )
      .unwrap();
    assert_ne!(app.id(), window.id());
    assert_eq!(tracker.cancel_scope(&TaskScope::Window(1)), 2);
    assert!(window.join().await.unwrap_err().is_cancelled());
    assert!(page.join().await.unwrap_err().is_cancelled());
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert!(!app.is_finished());
    assert!(!other.is_finished());
    tracker.shutdown();
    let _ = app.join().await;
    let _ = other.join().await;
    assert_eq!(tracker.active_count(), 0);
  }

  #[tokio::test]
  async fn page_cancellation_does_not_cancel_sibling_pages_or_window_tasks() {
    let tracker = TaskTracker::new();
    let page = tracker
      .spawn(&Handle::current(), TaskScope::page(1, "a"), pending::<()>())
      .unwrap();
    let sibling = tracker
      .spawn(&Handle::current(), TaskScope::page(1, "b"), pending::<()>())
      .unwrap();
    let window = tracker
      .spawn(&Handle::current(), TaskScope::Window(1), pending::<()>())
      .unwrap();
    assert_eq!(tracker.cancel_scope(&TaskScope::page(1, "a")), 1);
    assert!(page.join().await.unwrap_err().is_cancelled());
    assert!(!sibling.is_finished());
    assert!(!window.is_finished());
    tracker.shutdown();
    let _ = sibling.join().await;
    let _ = window.join().await;
  }

  #[tokio::test]
  async fn shutdown_and_cancellation_handles_are_idempotent_and_await_cleanup() {
    let tracker = TaskTracker::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let task = tracker
      .spawn(&Handle::current(), TaskScope::Application, pending_task(drops.clone()))
      .unwrap();
    let cancellation = task.cancellation_handle();
    assert_eq!(tracker.shutdown(), 1);
    assert_eq!(tracker.shutdown(), 0);
    cancellation.cancel();
    assert!(task.join().await.unwrap_err().is_cancelled());
    assert!(cancellation.is_finished());
    assert!(cancellation.is_cancelled());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(tracker.active_count(), 0);
    assert!(matches!(
      tracker.spawn(&Handle::current(), TaskScope::Application, async {}),
      Err(TaskSpawnError::ShuttingDown)
    ));
  }

  #[tokio::test]
  async fn completed_and_detached_tasks_remain_owned_by_the_tracker() {
    let tracker = TaskTracker::new();
    let completed = tracker
      .spawn(&Handle::current(), TaskScope::Application, async { 42 })
      .unwrap();
    assert_eq!(completed.join().await.unwrap(), 42);
    assert_eq!(tracker.active_count(), 0);
    let drops = Arc::new(AtomicUsize::new(0));
    let task = tracker
      .spawn(&Handle::current(), TaskScope::Window(3), pending_task(drops.clone()))
      .unwrap();
    let cancellation = task.cancellation_handle();
    drop(task);
    assert_eq!(tracker.shutdown(), 1);
    tracker.wait_idle().await;
    assert!(cancellation.is_finished());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
  }
}
