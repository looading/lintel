use std::{
  io,
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
  },
  thread,
  time::Duration,
};

use mojo_ipc::{Role, Runtime, system::message_pipe::MessageEndpoint};
use mojo_service::{
  AccessPolicy, ProcessEvent, ProcessId, ProcessSnapshot, ProcessSpec, ProcessSupervisor, ServiceDirectory,
  ServiceManager,
};
use tokio::sync::{broadcast, oneshot, watch};

use crate::{DirectoryClient, TrustedRuntimeLibrary};

/// 一个宿主生命周期内最多登记的服务进程数，包括已停止的登记项。
pub const MAX_PROCESS_SPECS: usize = 16;

/// 应用宿主拥有的权限和进程配置；默认不授予宿主发布或连接权限。
pub struct HostConfig {
  library: TrustedRuntimeLibrary,
  processes: Vec<ProcessSpec>,
  policy: AccessPolicy,
  poll_interval: Duration,
  command_capacity: usize,
  event_capacity: usize,
}

impl HostConfig {
  pub fn new(library: TrustedRuntimeLibrary) -> Self {
    Self {
      library,
      processes: Vec::new(),
      policy: AccessPolicy::new(),
      poll_interval: Duration::from_millis(20),
      command_capacity: 32,
      event_capacity: 128,
    }
  }
  pub fn process(mut self, spec: ProcessSpec) -> Self {
    self.processes.push(spec);
    self
  }
  pub fn host_policy(mut self, policy: AccessPolicy) -> Self {
    self.policy = policy;
    self
  }
  pub fn poll_interval(mut self, interval: Duration) -> Self {
    self.poll_interval = interval;
    self
  }
  pub fn queue_limits(mut self, commands: usize, events: usize) -> Self {
    self.command_capacity = commands;
    self.event_capacity = events;
    self
  }

  fn validate(&self) -> io::Result<()> {
    if self.processes.len() > MAX_PROCESS_SPECS
      || !(1..=256).contains(&self.command_capacity)
      || !(1..=4096).contains(&self.event_capacity)
      || self.poll_interval < Duration::from_millis(1)
      || self.poll_interval > Duration::from_secs(1)
    {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid bounded process-host configuration",
      ));
    }
    Ok(())
  }
}

#[derive(Clone, Debug)]
enum Completion {
  Running,
  Finished(Result<(), (io::ErrorKind, String)>),
}

struct EventHub(Mutex<Option<broadcast::Sender<ProcessEvent>>>);
impl EventHub {
  fn new(capacity: usize) -> Self {
    let (sender, _) = broadcast::channel(capacity);
    Self(Mutex::new(Some(sender)))
  }
  fn subscribe(&self) -> broadcast::Receiver<ProcessEvent> {
    match self.0.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
      Some(sender) => sender.subscribe(),
      None => {
        let (sender, receiver) = broadcast::channel(1);
        drop(sender);
        receiver
      }
    }
  }
  fn send(&self, event: ProcessEvent) {
    if let Some(sender) = self.0.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
      let _ = sender.send(event);
    }
  }
  fn close(&self) {
    self.0.lock().unwrap_or_else(|p| p.into_inner()).take();
  }
}

struct CompletionGuard {
  completion: Option<watch::Sender<Completion>>,
  events: Arc<EventHub>,
}
impl CompletionGuard {
  fn new(sender: watch::Sender<Completion>, events: Arc<EventHub>) -> Self {
    Self {
      completion: Some(sender),
      events,
    }
  }
  fn finish(mut self, result: io::Result<()>) {
    self.events.close();
    if let Some(sender) = self.completion.take() {
      sender.send_replace(Completion::Finished(result.map_err(|e| (e.kind(), e.to_string()))));
    }
  }
}
impl Drop for CompletionGuard {
  fn drop(&mut self) {
    self.events.close();
    if let Some(sender) = self.completion.take() {
      sender.send_replace(Completion::Finished(Err((
        io::ErrorKind::Other,
        "process supervision thread panicked".into(),
      ))));
    }
  }
}

enum Command {
  Add(ProcessSpec, oneshot::Sender<io::Result<ProcessId>>),
  Stop(ProcessId, oneshot::Sender<io::Result<()>>),
}

struct HostControl {
  commands: mpsc::SyncSender<Command>,
  stopped: Arc<AtomicBool>,
  thread: thread::Thread,
}
impl HostControl {
  fn request_shutdown(&self) {
    self.stopped.store(true, Ordering::Release);
    self.thread.unpark();
  }
  fn submit(&self, command: Command) -> io::Result<()> {
    if self.stopped.load(Ordering::Acquire) {
      return Err(io::Error::new(io::ErrorKind::BrokenPipe, "process host is stopping"));
    }
    self.commands.try_send(command).map_err(|e| match e {
      mpsc::TrySendError::Full(_) => io::Error::new(io::ErrorKind::WouldBlock, "process command queue is full"),
      mpsc::TrySendError::Disconnected(_) => io::Error::new(io::ErrorKind::BrokenPipe, "process host stopped"),
    })?;
    self.thread.unpark();
    Ok(())
  }
}
impl Drop for HostControl {
  fn drop(&mut self) {
    self.request_shutdown();
  }
}

struct Inner {
  runtime: Runtime,
  directory: DirectoryClient,
  control: HostControl,
  completion: watch::Receiver<Completion>,
  snapshots: Arc<Mutex<Vec<ProcessSnapshot>>>,
  events: Arc<EventHub>,
}

/// 可共享的监管句柄。最后一个句柄 Drop 只请求退出，不阻塞调用线程。
/// 必须 await `shutdown` 才能确认服务进程已经回收。
#[derive(Clone)]
pub struct ProcessHost(Arc<Inner>);

impl ProcessHost {
  /// 在专用线程初始化 broker；只有专用线程执行可能阻塞的 poll/stop。
  /// 返回代表宿主初始化完成，具体服务就绪以 snapshot/事件和服务发现为准。
  pub fn start(config: HostConfig) -> io::Result<Self> {
    config.validate()?;
    let (commands, receive) = mpsc::sync_channel(config.command_capacity);
    let events = Arc::new(EventHub::new(config.event_capacity));
    let (completion, finished) = watch::channel(Completion::Running);
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let stopped = Arc::new(AtomicBool::new(false));
    let (ready, initialized) = mpsc::sync_channel(1);
    let task_snapshots = snapshots.clone();
    let task_stopped = stopped.clone();
    let task_events = events.clone();
    let worker = thread::Builder::new()
      .name("lintel-process-supervisor".into())
      .spawn(move || {
        let completion = CompletionGuard::new(completion, task_events.clone());
        let result = supervise(config, receive, task_stopped, task_snapshots, task_events, ready);
        completion.finish(result);
      })?;
    let thread = worker.thread().clone();
    // 线程资源由操作系统在完成后回收；完成状态在所有子进程 owner 释放后发布。
    drop(worker);
    let (runtime, directory) = initialized
      .recv()
      .map_err(|_| io::Error::other("process host initialization thread ended"))??;
    Ok(Self(Arc::new(Inner {
      runtime,
      directory,
      control: HostControl {
        commands,
        stopped,
        thread,
      },
      completion: finished,
      snapshots,
      events,
    })))
  }

  pub fn runtime(&self) -> &Runtime {
    &self.0.runtime
  }
  pub fn directory(&self) -> &DirectoryClient {
    &self.0.directory
  }

  /// 缓存的有界快照；不会在 UI 线程等待子进程。
  pub fn snapshots(&self) -> Vec<ProcessSnapshot> {
    self.0.snapshots.lock().unwrap_or_else(|p| p.into_inner()).clone()
  }
  pub fn snapshot(&self, id: ProcessId) -> Option<ProcessSnapshot> {
    self.snapshots().into_iter().find(|s| s.id == id)
  }

  /// 等待具名进程 Ready；订阅先于快照检查，返回后进程仍可能退出。
  pub fn wait_ready(
    &self,
    identity: &str,
    timeout: Duration,
  ) -> impl std::future::Future<Output = io::Result<ProcessSnapshot>> + Send + 'static {
    crate::readiness::wait(self.clone(), identity.to_owned(), 0, timeout)
  }

  /// 显式等待比已知 attempt 更新的 Ready，用于业务自行重连；不会重放 RPC。
  pub fn wait_ready_after(
    &self,
    identity: &str,
    previous_attempt: u64,
    timeout: Duration,
  ) -> impl std::future::Future<Output = io::Result<ProcessSnapshot>> + Send + 'static {
    crate::readiness::wait(self.clone(), identity.to_owned(), previous_attempt, timeout)
  }

  pub(crate) fn is_stopping(&self) -> bool {
    self.0.control.stopped.load(Ordering::Acquire) || matches!(*self.0.completion.borrow(), Completion::Finished(_))
  }

  /// 接收方必须处理 Lagged 并用 snapshots 重建状态，事件不会无限累积。
  pub fn subscribe_events(&self) -> broadcast::Receiver<ProcessEvent> {
    self.0.events.subscribe()
  }

  fn submit(&self, command: Command) -> io::Result<()> {
    self.0.control.submit(command)
  }

  pub async fn add_process(&self, spec: ProcessSpec) -> io::Result<ProcessId> {
    let (send, receive) = oneshot::channel();
    self.submit(Command::Add(spec, send))?;
    receive
      .await
      .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "process host stopped before admission"))?
  }
  pub async fn stop_process(&self, id: ProcessId) -> io::Result<()> {
    let (send, receive) = oneshot::channel();
    self.submit(Command::Stop(id, send))?;
    receive
      .await
      .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "process host stopped before stop completion"))?
  }

  pub fn request_shutdown(&self) {
    self.0.control.request_shutdown();
  }

  /// 非阻塞地等待原生监管 owner 释放。可在任何执行器 await，不需要 Tokio timer。
  pub async fn shutdown(&self) -> io::Result<()> {
    self.request_shutdown();
    wait_completion(self.0.completion.clone()).await
  }
}

async fn wait_completion(mut completion: watch::Receiver<Completion>) -> io::Result<()> {
  loop {
    if let Completion::Finished(result) = completion.borrow().clone() {
      return result.map_err(|(kind, text)| io::Error::new(kind, text));
    }
    completion
      .changed()
      .await
      .map_err(|_| io::Error::other("process host completion channel closed"))?;
  }
}

fn cache(supervisor: &ProcessSupervisor, ids: &[ProcessId], snapshots: &Mutex<Vec<ProcessSnapshot>>) {
  *snapshots.lock().unwrap_or_else(|p| p.into_inner()) = ids.iter().filter_map(|id| supervisor.snapshot(*id)).collect();
}

fn supervise(
  config: HostConfig,
  commands: mpsc::Receiver<Command>,
  stopped: Arc<AtomicBool>,
  snapshots: Arc<Mutex<Vec<ProcessSnapshot>>>,
  events: Arc<EventHub>,
  ready: mpsc::SyncSender<io::Result<(Runtime, DirectoryClient)>>,
) -> io::Result<()> {
  let initialize = || -> io::Result<_> {
    // SAFETY: TrustedRuntimeLibrary 的构造者明确承担匹配可信原生镜像的前提。
    let runtime = unsafe { Runtime::load(config.library.path(), Role::Broker) }.map_err(io::Error::other)?;
    let manager = ServiceManager::new(&runtime);
    let (host, server) = MessageEndpoint::create_pipe()
      .map_err(|e| io::Error::other(format!("cannot create host directory pipe: {e:?}")))?;
    let lease = manager.attach("lintel-host", config.policy, server);
    let directory = DirectoryClient::new(&runtime, ServiceDirectory::bind(&runtime, host, None), stopped.clone());
    let mut supervisor = ProcessSupervisor::new(&runtime, &manager);
    let mut ids = Vec::new();
    for spec in config.processes {
      ids.push(supervisor.add(spec)?);
    }
    Ok((runtime, manager, lease, directory, supervisor, ids))
  };
  let (runtime, _manager, _lease, directory, mut supervisor, mut ids) = match initialize() {
    Ok(state) => state,
    Err(error) => {
      let copy = io::Error::new(error.kind(), error.to_string());
      let _ = ready.send(Err(error));
      return Err(copy);
    }
  };
  cache(&supervisor, &ids, &snapshots);
  if ready.send(Ok((runtime, directory))).is_err() {
    return supervisor.stop_all();
  }
  let result = (|| -> io::Result<()> {
    while !stopped.load(Ordering::Acquire) {
      // 每轮最多处理固定数量，避免持续生产命令使监管和退出饥饿。
      for _ in 0..32 {
        if stopped.load(Ordering::Acquire) {
          break;
        }
        match commands.try_recv() {
          Ok(Command::Add(spec, reply)) => {
            let result = if ids.len() >= MAX_PROCESS_SPECS {
              Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "process registration limit reached",
              ))
            } else {
              supervisor.add(spec).inspect(|id| ids.push(*id))
            };
            // 先发布状态，再确认登记；收到 ID 的调用方可立即读取其快照。
            cache(&supervisor, &ids, &snapshots);
            let _ = reply.send(result);
          }
          Ok(Command::Stop(id, reply)) => {
            let result = supervisor.stop(id);
            // 停止事件及成功回复的观察者必须先看见对应的最新状态。
            cache(&supervisor, &ids, &snapshots);
            let result = result.map(|event| {
              let _ = events.send(event);
            });
            let _ = reply.send(result);
          }
          Err(mpsc::TryRecvError::Empty) => break,
          Err(mpsc::TryRecvError::Disconnected) => {
            stopped.store(true, Ordering::Release);
            break;
          }
        }
      }
      if stopped.load(Ordering::Acquire) {
        break;
      }
      let polled = supervisor.poll()?;
      cache(&supervisor, &ids, &snapshots);
      for event in polled {
        let _ = events.send(event);
      }
      thread::park_timeout(config.poll_interval);
    }
    Ok(())
  })();
  stopped.store(true, Ordering::Release);
  let cleanup = supervisor.stop_all();
  cache(&supervisor, &ids, &snapshots);
  // supervise 返回之后所有 manager/lease/supervisor 才释放，随后外层发布完成。
  result.and(cleanup)
}

#[cfg(test)]
pub(crate) mod testing {
  use super::*;

  pub async fn completion_panics_fail_waiters() {
    let (send, receive) = watch::channel(Completion::Running);
    drop(CompletionGuard::new(send, Arc::new(EventHub::new(1))));
    assert_eq!(wait_completion(receive).await.unwrap_err().kind(), io::ErrorKind::Other);
  }
  pub async fn completion_retains_cleanup_error_for_all_waiters() {
    let (send, receive) = watch::channel(Completion::Running);
    let first = receive.clone();
    CompletionGuard::new(send, Arc::new(EventHub::new(1)))
      .finish(Err(io::Error::new(io::ErrorKind::PermissionDenied, "cleanup failed")));
    assert_eq!(
      wait_completion(first).await.unwrap_err().kind(),
      io::ErrorKind::PermissionDenied
    );
    assert_eq!(
      wait_completion(receive).await.unwrap_err().kind(),
      io::ErrorKind::PermissionDenied
    );
  }

  #[test]
  fn full_queue_refuses_new_work_and_closed_queue_fails_pending_reply() {
    let (commands, receive) = mpsc::sync_channel(1);
    let control = HostControl {
      commands,
      stopped: Arc::new(AtomicBool::new(false)),
      thread: thread::current(),
    };
    let (first, mut reply) = oneshot::channel();
    control
      .submit(Command::Add(
        ProcessSpec::new("one", "ignored", AccessPolicy::new()),
        first,
      ))
      .unwrap();
    let (second, _) = oneshot::channel();
    assert_eq!(
      control
        .submit(Command::Add(
          ProcessSpec::new("two", "ignored", AccessPolicy::new()),
          second
        ))
        .unwrap_err()
        .kind(),
      io::ErrorKind::WouldBlock
    );
    drop(receive);
    assert!(matches!(reply.try_recv(), Err(oneshot::error::TryRecvError::Closed)));
    let (third, _) = oneshot::channel();
    assert_eq!(
      control
        .submit(Command::Add(
          ProcessSpec::new("three", "ignored", AccessPolicy::new()),
          third
        ))
        .unwrap_err()
        .kind(),
      io::ErrorKind::BrokenPipe
    );
  }

  #[test]
  fn shutdown_bypasses_full_queue_and_prevents_later_admission() {
    let (commands, _receive) = mpsc::sync_channel(1);
    let stopped = Arc::new(AtomicBool::new(false));
    let control = HostControl {
      commands,
      stopped: stopped.clone(),
      thread: thread::current(),
    };
    let (send, _) = oneshot::channel();
    control
      .submit(Command::Add(
        ProcessSpec::new("one", "ignored", AccessPolicy::new()),
        send,
      ))
      .unwrap();
    control.request_shutdown();
    assert!(stopped.load(Ordering::Acquire));
    let (send, _) = oneshot::channel();
    assert_eq!(
      control
        .submit(Command::Add(
          ProcessSpec::new("late", "ignored", AccessPolicy::new()),
          send
        ))
        .unwrap_err()
        .kind(),
      io::ErrorKind::BrokenPipe
    );
  }

  #[test]
  fn only_last_shared_control_owner_requests_shutdown_and_wakes_thread() {
    let (ready, receive) = mpsc::channel();
    let stopped = Arc::new(AtomicBool::new(false));
    let observed = stopped.clone();
    let worker = thread::spawn(move || {
      ready.send(()).unwrap();
      while !observed.load(Ordering::Acquire) {
        thread::park_timeout(Duration::from_secs(2));
      }
    });
    receive.recv_timeout(Duration::from_secs(2)).unwrap();
    let (commands, _receive) = mpsc::sync_channel(1);
    let control = Arc::new(HostControl {
      commands,
      stopped: stopped.clone(),
      thread: worker.thread().clone(),
    });
    let clone = control.clone();
    drop(control);
    assert!(!stopped.load(Ordering::Acquire));
    drop(clone);
    assert!(stopped.load(Ordering::Acquire));
    worker.join().unwrap();
  }

  #[tokio::test(flavor = "current_thread")]
  async fn completion_closes_existing_and_late_event_subscribers() {
    let events = Arc::new(EventHub::new(1));
    let mut before = events.subscribe();
    let (send, receive) = watch::channel(Completion::Running);
    CompletionGuard::new(send, events.clone()).finish(Ok(()));
    wait_completion(receive).await.unwrap();
    assert!(matches!(before.recv().await, Err(broadcast::error::RecvError::Closed)));
    assert!(matches!(
      events.subscribe().recv().await,
      Err(broadcast::error::RecvError::Closed)
    ));
  }

  #[tokio::test(flavor = "current_thread")]
  async fn cancelled_shutdown_wait_does_not_lose_completion_for_other_owner() {
    let (send, receive) = watch::channel(Completion::Running);
    let mut cancelled = Box::pin(wait_completion(receive.clone()));
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    assert!(std::future::Future::poll(cancelled.as_mut(), &mut context).is_pending());
    drop(cancelled);
    CompletionGuard::new(send, Arc::new(EventHub::new(1))).finish(Ok(()));
    wait_completion(receive).await.unwrap();
  }
  pub fn validate_config(library: TrustedRuntimeLibrary) {
    assert!(HostConfig::new(library.clone()).validate().is_ok());
    assert!(HostConfig::new(library.clone()).queue_limits(0, 1).validate().is_err());
    assert!(
      HostConfig::new(library.clone())
        .queue_limits(1, 4097)
        .validate()
        .is_err()
    );
    assert!(
      HostConfig::new(library.clone())
        .poll_interval(Duration::ZERO)
        .validate()
        .is_err()
    );
    let mut config = HostConfig::new(library);
    for i in 0..=MAX_PROCESS_SPECS {
      config = config.process(ProcessSpec::new(i.to_string(), "ignored", AccessPolicy::new()));
    }
    assert!(config.validate().is_err());
  }
}
