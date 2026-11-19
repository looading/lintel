use super::{
  ProgressError, ProgressEvent, ProgressStream, ToolCatalog, ToolError, ToolErrorCode as Code, ToolPolicy,
  ToolRegistry, ToolResult, Value, progress::Progress,
};
use crate::core::{
  context::AppContext,
  tasks::{TaskCancellation, TaskHandle, TaskScope, TaskSpawnError},
};
use std::{
  collections::BTreeMap,
  future::{Future, pending},
  pin::Pin,
  sync::{Arc, Mutex, OnceLock, Weak},
  task::{Context, Poll},
  time::Duration,
};
use tokio::sync::{Notify, oneshot};

#[derive(Clone, Copy, Debug)]
pub struct ToolLimits {
  pub max_concurrency: usize,
  pub max_input_bytes: usize,
  pub max_output_bytes: usize,
  pub progress_capacity: usize,
  pub max_progress_events: usize,
  pub timeout: Duration,
}
impl Default for ToolLimits {
  fn default() -> Self {
    Self {
      max_concurrency: 4,
      max_input_bytes: 64 * 1024,
      max_output_bytes: 64 * 1024,
      progress_capacity: 16,
      max_progress_events: 256,
      timeout: Duration::from_secs(15),
    }
  }
}
impl ToolLimits {
  fn validate(self) -> ToolResult<Self> {
    if !(1..=32).contains(&self.max_concurrency)
      || !(1..=1024 * 1024).contains(&self.max_input_bytes)
      || !(1..=1024 * 1024).contains(&self.max_output_bytes)
      || !(1..=256).contains(&self.progress_capacity)
      || !(1..=4096).contains(&self.max_progress_events)
      || self.timeout.is_zero()
      || self.timeout > Duration::from_secs(300)
    {
      return Err(ToolError::new(Code::InvalidLimits));
    }
    Ok(self)
  }
}

#[derive(Clone)]
pub struct ToolContext {
  application: Option<AppContext>,
  scope: TaskScope,
  progress: Arc<Progress>,
  cancellation: TaskCancellation,
  output_limit: usize,
}
impl ToolContext {
  /// Trusted handlers use existing services or main-thread message APIs; this is not widget access.
  pub fn app_context(&self) -> &AppContext {
    self
      .application
      .as_ref()
      .expect("application context is present in production tool sessions")
  }
  pub fn scope(&self) -> &TaskScope {
    &self.scope
  }
  pub fn is_cancelled(&self) -> bool {
    self.cancellation.is_cancelled()
  }
  pub fn progress(&self, completed: u64, total: Option<u64>) -> Result<(), ProgressError> {
    if self.cancellation.is_cancelled() {
      self.progress.close();
      return Err(ProgressError::Closed);
    }
    self.progress.report(completed, total)
  }
  pub(super) fn output_limit(&self) -> usize {
    self.output_limit
  }
}

#[derive(Clone)]
enum Owner {
  Application(AppContext),
  #[cfg(test)]
  Tracked {
    tracker: crate::core::tasks::TaskTracker,
    runtime: tokio::runtime::Handle,
  },
}
impl Owner {
  fn spawn<F>(&self, scope: TaskScope, future: F) -> Result<TaskHandle<F::Output>, TaskSpawnError>
  where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
  {
    match self {
      Self::Application(context) => context.spawn_task(scope, future),
      #[cfg(test)]
      Self::Tracked { tracker, runtime } => tracker.spawn(runtime, scope, future),
    }
  }
  fn context(&self) -> Option<AppContext> {
    match self {
      Self::Application(context) => Some(context.clone()),
      #[cfg(test)]
      Self::Tracked { .. } => None,
    }
  }
  fn shutting_down(&self) -> bool {
    match self {
      Self::Application(context) => context.task_tracker().is_shutting_down(),
      #[cfg(test)]
      Self::Tracked { tracker, .. } => tracker.is_shutting_down(),
    }
  }
}
struct Active {
  cancellation: Option<TaskCancellation>,
  progress: Arc<Progress>,
}
#[derive(Default)]
struct State {
  closed: Option<Code>,
  next_id: u64,
  active: BTreeMap<u64, Active>,
}
struct Inner {
  owner: Owner,
  scope: TaskScope,
  registry: Arc<ToolRegistry>,
  policy: ToolPolicy,
  limits: ToolLimits,
  state: Mutex<State>,
  sentinel: OnceLock<TaskCancellation>,
  changed: Notify,
}
impl Inner {
  fn admission_error(&self, state: &State) -> Option<Code> {
    state.closed.or_else(|| {
      (self.owner.shutting_down()
        || self
          .sentinel
          .get()
          .is_some_and(|task| task.is_cancelled() || task.is_finished()))
      .then_some(Code::ScopeClosed)
    })
  }
  fn close(&self, reason: Code) -> usize {
    let (tasks, count, sentinel) = {
      let mut state = self.state.lock().unwrap();
      if state.closed.is_some() {
        return 0;
      }
      state.closed = Some(reason);
      let count = state.active.len();
      let tasks = state
        .active
        .values()
        .map(|active| (active.cancellation.clone(), active.progress.clone()))
        .collect::<Vec<_>>();
      (tasks, count, self.sentinel.get().cloned())
    };
    // Never invoke cancellation/destruction or await while holding the session state lock.
    for (task, progress) in tasks {
      progress.close();
      if let Some(task) = task {
        task.cancel();
      }
    }
    if let Some(task) = sentinel {
      task.cancel();
    }
    self.changed.notify_waiters();
    count
  }
}
impl Drop for Inner {
  fn drop(&mut self) {
    self.close(Code::SessionClosed);
  }
}
struct ScopeLifetime(Weak<Inner>);
impl Drop for ScopeLifetime {
  fn drop(&mut self) {
    if let Some(session) = self.0.upgrade() {
      session.close(Code::ScopeClosed);
    }
  }
}
struct Completion {
  session: Weak<Inner>,
  id: u64,
  progress: Arc<Progress>,
}
impl Drop for Completion {
  fn drop(&mut self) {
    self.progress.close();
    if let Some(session) = self.session.upgrade() {
      session.state.lock().unwrap().active.remove(&self.id);
      session.changed.notify_waiters();
    }
  }
}
// Future is destroyed before Completion: a cancelled handler keeps its concurrency slot until cleanup.
struct Managed<F> {
  future: Pin<Box<F>>,
  _completion: Completion,
}
impl<F: Future> Future for Managed<F> {
  type Output = F::Output;
  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    self.future.as_mut().poll(cx)
  }
}

/// A session owns only its own invocation cancellation handles, never the whole TaskScope.
/// Wrap in Arc to share. Dropping the final ToolSession closes it; there is no detached mode.
pub struct ToolSession {
  inner: Arc<Inner>,
}
impl Drop for ToolSession {
  fn drop(&mut self) {
    self.close();
  }
}
impl ToolSession {
  pub fn new(
    context: &AppContext,
    scope: TaskScope,
    registry: Arc<ToolRegistry>,
    policy: ToolPolicy,
  ) -> ToolResult<Self> {
    Self::with_limits(context, scope, registry, policy, ToolLimits::default())
  }
  pub fn with_limits(
    context: &AppContext,
    scope: TaskScope,
    registry: Arc<ToolRegistry>,
    policy: ToolPolicy,
    limits: ToolLimits,
  ) -> ToolResult<Self> {
    Self::owned(Owner::Application(context.clone()), scope, registry, policy, limits)
  }
  fn owned(
    owner: Owner,
    scope: TaskScope,
    registry: Arc<ToolRegistry>,
    policy: ToolPolicy,
    limits: ToolLimits,
  ) -> ToolResult<Self> {
    let inner = Arc::new(Inner {
      owner,
      scope,
      registry,
      policy,
      limits: limits.validate()?,
      state: Mutex::new(State::default()),
      sentinel: OnceLock::new(),
      changed: Notify::new(),
    });
    // Construct before spawn: cancellation before the first poll must still close this scope binding.
    let lifetime = ScopeLifetime(Arc::downgrade(&inner));
    let task = inner
      .owner
      .spawn(inner.scope.clone(), async move {
        let _lifetime = lifetime;
        pending::<()>().await
      })
      .map_err(|_| ToolError::new(Code::ScopeClosed))?;
    let cancellation = task.cancellation_handle();
    let _ = inner.sentinel.set(cancellation);
    drop(task);
    let error = {
      let state = inner.state.lock().unwrap();
      inner.admission_error(&state)
    };
    if let Some(error) = error {
      inner.close(error);
      return Err(ToolError::new(error));
    }
    Ok(Self { inner })
  }
  /// Policy-filtered discovery is not an execution authorization token.
  pub fn catalog(&self) -> ToolResult<ToolCatalog> {
    {
      let state = self.inner.state.lock().unwrap();
      if let Some(error) = self.inner.admission_error(&state) {
        return Err(ToolError::new(error));
      }
    }
    Ok(
      self
        .inner
        .registry
        .filtered_catalog(|spec| self.inner.policy.permits(spec)),
    )
  }
  /// Input bytes are bounded before copying/deserializing. There is no unbounded request queue.
  pub fn invoke_json(&self, id: &str, input: &[u8]) -> ToolResult<Invocation> {
    self.invoke_with(id, input, || {}, || {})
  }
  fn invoke_with(
    &self,
    id: &str,
    input: &[u8],
    before_spawn: impl FnOnce(),
    before_commit: impl FnOnce(),
  ) -> ToolResult<Invocation> {
    let (entry, invocation_id, progress, stream) = {
      let mut state = self.inner.state.lock().unwrap();
      if let Some(error) = self.inner.admission_error(&state) {
        return Err(ToolError::new(error));
      }
      let entry = self.inner.registry.entry(id)?;
      if !self.inner.policy.permits(&entry.spec) {
        return Err(ToolError::new(Code::Denied));
      }
      if input.len() > self.inner.limits.max_input_bytes {
        return Err(ToolError::new(Code::InputTooLarge));
      }
      if state.active.len() == self.inner.limits.max_concurrency {
        return Err(ToolError::new(Code::Busy));
      }
      let id = state.next_id;
      state.next_id = id.checked_add(1).ok_or(ToolError::new(Code::Unavailable))?;
      let (progress, stream) = Progress::new(
        id,
        self.inner.limits.progress_capacity,
        self.inner.limits.max_progress_events,
      );
      state.active.insert(
        id,
        Active {
          cancellation: None,
          progress: progress.clone(),
        },
      );
      (entry, id, progress, stream)
    };
    let input = input.to_vec();
    let scope = self.inner.scope.clone();
    let context = self.inner.owner.context();
    let weak = Arc::downgrade(&self.inner);
    let task_progress = progress.clone();
    let limits = self.inner.limits;
    let deadline = tokio::time::Instant::now() + limits.timeout;
    let (start, started) = oneshot::channel::<TaskCancellation>();
    let completion = Completion {
      session: Arc::downgrade(&self.inner),
      id: invocation_id,
      progress: progress.clone(),
    };
    let future = async move {
      let cancellation = started.await.map_err(|_| ToolError::new(Code::Cancelled))?;
      // Start only after cancellation registration commits. Closing while spawn is in progress cannot
      // miss this task, and a completed/aborted-before-commit entry is never inserted again.
      {
        let Some(session) = weak.upgrade() else {
          return Err(ToolError::new(Code::Cancelled));
        };
        let state = session.state.lock().unwrap();
        if session.admission_error(&state).is_some() || cancellation.is_cancelled() {
          return Err(ToolError::new(Code::Cancelled));
        }
      }
      if tokio::time::Instant::now() >= deadline {
        return Err(ToolError::new(Code::Timeout));
      }
      let tool_context = ToolContext {
        application: context,
        scope,
        progress: task_progress,
        cancellation,
        output_limit: limits.max_output_bytes,
      };
      let result = tokio::time::timeout_at(deadline, async move { (entry.handler)(tool_context, input).await })
        .await
        .map_err(|_| ToolError::new(Code::Timeout))?;
      // A non-yielding poll cannot be preempted, but it must not report success after returning late.
      if tokio::time::Instant::now() >= deadline {
        Err(ToolError::new(Code::Timeout))
      } else {
        result
      }
    };
    // Do not hold State across spawn: TaskSpawnError destroys the supplied future synchronously.
    before_spawn();
    let task = self
      .inner
      .owner
      .spawn(
        self.inner.scope.clone(),
        Managed {
          future: Box::pin(future),
          _completion: completion,
        },
      )
      .map_err(|_| ToolError::new(Code::ScopeClosed))?;
    let cancellation = task.cancellation_handle();
    before_commit();
    let admitted = {
      let mut state = self.inner.state.lock().unwrap();
      let live = self.inner.admission_error(&state).is_none();
      if let Some(active) = state.active.get_mut(&invocation_id) {
        active.cancellation = Some(cancellation.clone());
        live
      } else {
        false
      }
    };
    if admitted {
      let _ = start.send(cancellation.clone());
    } else {
      progress.close();
      cancellation.cancel();
      drop(start);
    }
    Ok(Invocation {
      id: invocation_id,
      tool_id: id.to_owned(),
      task: Some(task),
      cancellation: InvocationCancellation {
        task: cancellation,
        progress,
      },
      progress: Some(stream),
    })
  }
  pub fn active_count(&self) -> usize {
    self.inner.state.lock().unwrap().active.len()
  }
  pub fn close(&self) -> usize {
    self.inner.close(Code::SessionClosed)
  }
  /// Cancels this session and waits until its handler futures are destroyed, not unrelated scope work.
  pub async fn shutdown(&self) {
    self.close();
    loop {
      let changed = self.inner.changed.notified();
      tokio::pin!(changed);
      changed.as_mut().enable();
      if self.inner.state.lock().unwrap().active.is_empty() {
        return;
      }
      changed.await;
    }
  }
}

#[derive(Clone)]
pub struct InvocationCancellation {
  task: TaskCancellation,
  progress: Arc<Progress>,
}
impl InvocationCancellation {
  pub fn cancel(&self) {
    self.progress.close();
    self.task.cancel();
  }
  pub fn is_cancelled(&self) -> bool {
    self.task.is_cancelled()
  }
  pub fn is_finished(&self) -> bool {
    self.task.is_finished()
  }
}
pub struct Invocation {
  id: u64,
  tool_id: String,
  task: Option<TaskHandle<ToolResult<Value>>>,
  cancellation: InvocationCancellation,
  progress: Option<ProgressStream>,
}
impl std::fmt::Debug for Invocation {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Invocation")
      .field("id", &self.id)
      .field("tool_id", &self.tool_id)
      .finish_non_exhaustive()
  }
}
impl Drop for Invocation {
  fn drop(&mut self) {
    if self.task.is_some() {
      self.cancellation.cancel();
    }
  }
}
struct CancelJoin {
  cancellation: InvocationCancellation,
  armed: bool,
}
impl Drop for CancelJoin {
  fn drop(&mut self) {
    if self.armed {
      self.cancellation.cancel();
    }
  }
}
impl Invocation {
  pub fn id(&self) -> u64 {
    self.id
  }
  pub fn tool_id(&self) -> &str {
    &self.tool_id
  }
  pub fn cancel(&self) {
    self.cancellation.cancel();
  }
  pub fn cancellation_handle(&self) -> InvocationCancellation {
    self.cancellation.clone()
  }
  pub fn take_progress(&mut self) -> Option<ProgressStream> {
    self.progress.take()
  }
  pub fn is_finished(&self) -> bool {
    self.cancellation.is_finished()
  }
  /// Dropping this join future, before or after its first poll, requests cancellation.
  pub async fn join(mut self) -> ToolResult<Value> {
    let mut guard = CancelJoin {
      cancellation: self.cancellation.clone(),
      armed: true,
    };
    let task = self.task.take().ok_or(ToolError::new(Code::Unavailable))?;
    let result = task.join().await;
    guard.armed = false;
    if self.cancellation.is_cancelled() {
      return Err(ToolError::new(Code::Cancelled));
    }
    match result {
      Ok(result) => result,
      Err(error) if error.is_cancelled() => Err(ToolError::new(Code::Cancelled)),
      Err(_) => Err(ToolError::new(Code::HandlerPanicked)),
    }
  }
}

#[cfg(test)]
impl ToolSession {
  pub(super) fn tracked(
    tracker: crate::core::tasks::TaskTracker,
    scope: TaskScope,
    registry: Arc<ToolRegistry>,
    policy: ToolPolicy,
    limits: ToolLimits,
  ) -> ToolResult<Self> {
    Self::owned(
      Owner::Tracked {
        tracker,
        runtime: tokio::runtime::Handle::current(),
      },
      scope,
      registry,
      policy,
      limits,
    )
  }
  pub(super) fn invoke_before_commit(&self, id: &str, input: &[u8], hook: impl FnOnce()) -> ToolResult<Invocation> {
    self.invoke_with(id, input, || {}, hook)
  }
  pub(super) fn invoke_before_spawn(&self, id: &str, input: &[u8], hook: impl FnOnce()) -> ToolResult<Invocation> {
    self.invoke_with(id, input, hook, || {})
  }
}
