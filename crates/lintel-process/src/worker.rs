use std::{
  io,
  sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
  },
  time::Duration,
};

use mojo_ipc::{Role, Runtime, system::message_pipe::MessageEndpoint};
use mojo_service::{
  FactoryBinding, HealthBinding, ServiceDirectory, ServiceKey, bind_factory, bind_health, connect_to_supervisor,
  watch_supervisor,
};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use crate::{DirectoryClient, TrustedRuntimeLibrary};

/// 由框架监管的服务进程入口；保留 stdin 给父进程存活检测。
/// 初始化和连接可能阻塞，应在服务进程入口而非 UI/dispatch 线程调用。
pub struct ProcessWorker {
  runtime: Runtime,
  directory: DirectoryClient,
  disconnected: watch::Receiver<bool>,
  disconnected_blocking: Mutex<mpsc::Receiver<()>>,
  factories: Mutex<Vec<(FactoryBinding, OwnedSemaphorePermit)>>,
  capacity: Arc<Semaphore>,
  health: Mutex<Option<HealthBinding>>,
}

impl ProcessWorker {
  /// 显式在原生 Runtime 初始化前安装当前服务进程的崩溃报告。
  /// stdin 仍由父进程存活监控独占；返回的 guard 必须覆盖服务整个使用期。
  #[cfg(feature = "crash-reporting")]
  pub fn connect_with_crash_reporting(
    library: TrustedRuntimeLibrary,
    service_identity: impl Into<String>,
    config: lintel_crash::CrashConfig,
  ) -> io::Result<crate::CrashReportingWorker> {
    crate::worker_crash::connect(library, service_identity.into(), config)
  }

  pub fn connect(library: TrustedRuntimeLibrary) -> io::Result<Self> {
    watch_supervisor()?;
    // SAFETY: 构造 TrustedRuntimeLibrary 的宿主保证镜像可信且与当前 SDK 匹配。
    let runtime = unsafe { Runtime::load(library.path(), Role::Participant) }.map_err(io::Error::other)?;
    let pipe = connect_to_supervisor(&runtime)?;
    let (disconnected, receive) = watch::channel(false);
    let (closed, blocking) = mpsc::sync_channel(1);
    let lifecycle = Arc::new(AtomicBool::new(false));
    let lifecycle_closed = lifecycle.clone();
    let directory = ServiceDirectory::bind(
      &runtime,
      pipe,
      Some(Box::new(move || {
        lifecycle_closed.store(true, Ordering::Release);
        disconnected.send_replace(true);
        let _ = closed.try_send(());
      })),
    );
    Ok(Self {
      directory: DirectoryClient::new(&runtime, directory, lifecycle),
      runtime,
      disconnected: receive,
      disconnected_blocking: Mutex::new(blocking),
      factories: Mutex::new(Vec::new()),
      capacity: Arc::new(Semaphore::new(64)),
      health: Mutex::new(None),
    })
  }

  pub fn runtime(&self) -> &Runtime {
    &self.runtime
  }
  pub fn directory(&self) -> &DirectoryClient {
    &self.directory
  }

  /// 服务进程 main 的同步接入，不要求业务建立 Tokio runtime。
  /// 不得从 SDK dispatch 线程调用。
  pub fn publish_blocking(
    &self,
    key: &ServiceKey,
    version: u32,
    bind: impl FnMut(MessageEndpoint) + Send + 'static,
    timeout: Duration,
  ) -> io::Result<u64> {
    if self.runtime.dispatch_runner().runs_tasks_in_current_sequence() {
      return Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "cannot wait for registration on dispatch thread",
      ));
    }
    if timeout.is_zero() || timeout > Duration::from_secs(300) {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "registration timeout must be in (0, 300s]",
      ));
    }
    let permit = self
      .capacity
      .clone()
      .try_acquire_owned()
      .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "service factory limit reached"))?;
    let pending_response = self.directory.request_permit()?;
    let (factory, binding) = bind_factory(&self.runtime, bind).map_err(io::Error::other)?;
    let (send, receive) = mpsc::sync_channel(1);
    self
      .directory
      .directory
      .lock()
      .map_err(|_| io::Error::other("directory lock poisoned"))?
      .register(key, version, factory, move |result| {
        drop(pending_response);
        let _ = send.try_send(result);
      });
    let generation = receive
      .recv_timeout(timeout)
      .map_err(|error| match error {
        mpsc::RecvTimeoutError::Timeout => io::Error::new(io::ErrorKind::TimedOut, "service registration timed out"),
        mpsc::RecvTimeoutError::Disconnected => {
          io::Error::new(io::ErrorKind::BrokenPipe, "directory disconnected before registration")
        }
      })?
      .map_err(io::Error::other)?;
    self
      .factories
      .lock()
      .map_err(|_| io::Error::other("service factory owner lock poisoned"))?
      .push((binding, permit));
    Ok(generation)
  }

  /// 保持同步服务 main 存活，直到宿主关闭目录准入。
  pub fn wait(&self) -> io::Result<()> {
    if self.runtime.dispatch_runner().runs_tasks_in_current_sequence() {
      return Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "cannot wait on dispatch thread",
      ));
    }
    if *self.disconnected.borrow() {
      return Ok(());
    }
    let receiver = self
      .disconnected_blocking
      .lock()
      .map_err(|_| io::Error::other("worker wait lock poisoned"))?;
    if *self.disconnected.borrow() {
      return Ok(());
    }
    receiver
      .recv()
      .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "worker directory lifecycle closed"))
  }

  /// 为显式启用 HealthPolicy 的服务注册短小、非阻塞的业务健康检查。
  pub fn health_blocking(&self, check: impl FnMut() -> bool + Send + 'static, timeout: Duration) -> io::Result<()> {
    if self.runtime.dispatch_runner().runs_tasks_in_current_sequence() {
      return Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "cannot wait for health registration on dispatch thread",
      ));
    }
    if timeout.is_zero() || timeout > Duration::from_secs(300) {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "registration timeout must be in (0, 300s]",
      ));
    }
    let pending_response = self.directory.request_permit()?;
    let (probe, binding) = bind_health(&self.runtime, check).map_err(io::Error::other)?;
    let (send, receive) = mpsc::sync_channel(1);
    self
      .directory
      .directory
      .lock()
      .map_err(|_| io::Error::other("directory lock poisoned"))?
      .set_health_probe(probe, move |result| {
        drop(pending_response);
        let _ = send.try_send(result);
      });
    receive
      .recv_timeout(timeout)
      .map_err(|error| match error {
        mpsc::RecvTimeoutError::Timeout => io::Error::new(io::ErrorKind::TimedOut, "health registration timed out"),
        mpsc::RecvTimeoutError::Disconnected => io::Error::new(
          io::ErrorKind::BrokenPipe,
          "directory disconnected before health registration",
        ),
      })?
      .map_err(io::Error::other)?;
    *self
      .health
      .lock()
      .map_err(|_| io::Error::other("health binding owner lock poisoned"))? = Some(binding);
    Ok(())
  }

  /// 发布并保留服务 factory。回调在 SDK dispatch 线程绑定已知业务 Receiver。
  /// 每进程最多保留 64 个 factory；服务版本必须与消费方精确匹配。
  pub async fn publish(
    &self,
    key: &ServiceKey,
    version: u32,
    bind: impl FnMut(MessageEndpoint) + Send + 'static,
    timeout: Duration,
  ) -> io::Result<u64> {
    let permit = self
      .capacity
      .clone()
      .try_acquire_owned()
      .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "service factory limit reached"))?;
    let (factory, binding) = bind_factory(&self.runtime, bind).map_err(io::Error::other)?;
    let generation = self
      .directory
      .request(timeout, |directory, done| {
        directory.register(key, version, factory, move |result| {
          done(result.map_err(io::Error::other))
        });
      })
      .await?;
    self
      .factories
      .lock()
      .map_err(|_| io::Error::other("service factory owner lock poisoned"))?
      .push((binding, permit));
    Ok(generation)
  }

  /// 父宿主关闭准入后等待业务清理；父进程崩溃还会由 stdin watcher 终止本进程。
  pub async fn disconnected(&self) -> io::Result<()> {
    let mut receiver = self.disconnected.clone();
    loop {
      if *receiver.borrow() {
        return Ok(());
      }
      receiver
        .changed()
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "worker directory lifecycle closed"))?;
    }
  }
}
