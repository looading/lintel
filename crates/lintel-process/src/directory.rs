use std::{
  io,
  sync::{
    Arc, Mutex, MutexGuard, TryLockError,
    atomic::{AtomicBool, Ordering},
  },
  time::{Duration, Instant},
};

use mojo_ipc::{Runtime, bindings::interface::DynMojomInterface};
use mojo_service::{
  CallContext, CallError, ServiceConnection, ServiceDirectory, ServiceInfo, ServiceKey, ServiceSubscription,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use crate::{ConnectedService, ServiceClient};

/// 跨业务 Task 的目录结果；显式擦除原生提交闭包与短期借用。
pub type DirectoryRequest<T> = std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<T>> + Send + 'static>>;

/// 有界目录客户端。异步等待使用宿主已有的 Tokio timer，不建立新执行器。
/// 超时只结束本地等待；已提交控制调用的额度直到回调/断连才释放。
#[derive(Clone)]
pub struct DirectoryClient {
  pub(crate) directory: Arc<Mutex<ServiceDirectory>>,
  capacity: Arc<Semaphore>,
  runtime: Runtime,
  closed: Arc<AtomicBool>,
}

impl DirectoryClient {
  pub(crate) fn new(runtime: &Runtime, directory: ServiceDirectory, closed: Arc<AtomicBool>) -> Self {
    Self {
      directory: Arc::new(Mutex::new(directory)),
      capacity: Arc::new(Semaphore::new(32)),
      runtime: runtime.clone(),
      closed,
    }
  }

  pub fn runtime(&self) -> &Runtime {
    &self.runtime
  }

  pub(crate) fn request_permit(&self) -> io::Result<OwnedSemaphorePermit> {
    admit(&self.capacity, &self.closed)
  }

  pub(crate) fn request<T: Send + 'static>(
    &self,
    timeout: Duration,
    submit: impl FnOnce(&mut ServiceDirectory, Box<dyn FnOnce(io::Result<T>) + Send>),
  ) -> DirectoryRequest<T> {
    // 在返回 Future 前完成提交，避免泛型原生接口/借用闭包进入跨 await 状态。
    let admitted = (|| {
      if timeout.is_zero() || timeout > Duration::from_secs(300) {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "directory timeout must be in (0, 300s]",
        ));
      }
      let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "directory deadline overflow"))?;
      let (mut directory, permit) = admit_request(&self.directory, &self.capacity, &self.closed)?;
      let (send, receive) = oneshot::channel();
      if directory.is_disconnected() {
        return Err(io::Error::new(io::ErrorKind::BrokenPipe, "directory disconnected"));
      }
      submit(
        &mut directory,
        Box::new(move |result| {
          let arrived = Instant::now();
          drop(permit);
          let _ = send.send(deadline_result(deadline, arrived, result));
        }),
      );
      Ok((receive, deadline))
    })();
    Box::pin(async move {
      let (receive, deadline) = admitted?;
      wait_response(receive, deadline).await
    })
  }

  /// 发现并转交指定接口版本；成功仅代表端点路由完成，不代表业务已成功。
  pub fn connect<T: DynMojomInterface + ?Sized + 'static>(
    &self,
    key: &ServiceKey,
    version: u32,
    timeout: Duration,
  ) -> DirectoryRequest<ServiceConnection<T>> {
    self.request(timeout, |directory, done| {
      directory.connect::<T>(key, version, move |result| done(result.map_err(io::Error::other)));
    })
  }

  /// 立即提交目录连接，再绑定供普通 Send Task 使用的有界 typed client。
  /// 沿用 connect 的原截止时间与及时回复规则，不增加 timer、重连或业务重放。
  /// 容量为零或调用上下文已关闭时，在任何原生目录提交之前拒绝。
  pub fn connect_client<T: DynMojomInterface + ?Sized + 'static>(
    &self,
    context: &CallContext,
    key: &ServiceKey,
    version: u32,
    capacity: usize,
    timeout: Duration,
  ) -> DirectoryRequest<ConnectedService<T>> {
    let request = admit_client_request(context, capacity, || self.connect::<T>(key, version, timeout));
    let runtime = self.runtime.clone();
    let context = context.clone();
    Box::pin(async move {
      let connection = request?.await?;
      let client = ServiceClient::bind(&runtime, &context, connection.remote, capacity).map_err(io::Error::other)?;
      Ok(ConnectedService {
        client,
        generation: connection.generation,
      })
    })
  }

  pub fn list(&self, timeout: Duration) -> DirectoryRequest<Vec<ServiceInfo>> {
    self.request(timeout, |directory, done| {
      directory.list(move |result| done(Ok(result)))
    })
  }

  /// 有界原子快照和变化订阅；队列溢出由上游返回 ResyncRequired。
  pub fn watch(&self, capacity: usize, timeout: Duration) -> DirectoryRequest<ServiceSubscription> {
    self.request(timeout, |directory, done| {
      directory.watch(capacity, move |result| done(result.map_err(io::Error::other)));
    })
  }
}

fn admit_client_request<R>(context: &CallContext, capacity: usize, submit: impl FnOnce() -> R) -> io::Result<R> {
  if capacity == 0 {
    return Err(io::Error::new(io::ErrorKind::InvalidInput, CallError::InvalidOptions));
  }
  if context.is_shutdown() {
    return Err(io::Error::new(io::ErrorKind::BrokenPipe, CallError::ContextClosed));
  }
  Ok(submit())
}

/// 在同一真实准入路径管理额度和非阻塞锁，锁拒绝会立即归还尚未提交的额度。
fn admit_request<'a, D>(
  directory: &'a Mutex<D>,
  capacity: &Arc<Semaphore>,
  closed: &AtomicBool,
) -> io::Result<(MutexGuard<'a, D>, OwnedSemaphorePermit)> {
  let permit = admit(capacity, closed)?;
  let directory = directory.try_lock().map_err(|error| match error {
    TryLockError::WouldBlock => io::Error::new(io::ErrorKind::WouldBlock, "directory is busy"),
    TryLockError::Poisoned(_) => io::Error::other("directory lock poisoned"),
  })?;
  Ok((directory, permit))
}

fn deadline_result<T>(deadline: Instant, arrived: Instant, result: io::Result<T>) -> io::Result<T> {
  if arrived >= deadline {
    Err(io::Error::new(
      io::ErrorKind::TimedOut,
      "directory response arrived after deadline",
    ))
  } else {
    result
  }
}

async fn wait_response<T>(mut receive: oneshot::Receiver<io::Result<T>>, deadline: Instant) -> io::Result<T> {
  // 及时完成的响应即使在业务延后 poll 时仍可取得；迟到响应在回调端已经拒绝。
  match receive.try_recv() {
    Ok(result) => return result,
    Err(oneshot::error::TryRecvError::Closed) => {
      return Err(io::Error::new(
        io::ErrorKind::BrokenPipe,
        "directory disconnected before response",
      ));
    }
    Err(oneshot::error::TryRecvError::Empty) => {}
  }
  let remaining = deadline
    .checked_duration_since(Instant::now())
    .filter(|remaining| !remaining.is_zero())
    .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "directory request timed out"))?;
  tokio::time::timeout(remaining, receive)
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "directory request timed out"))?
    .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "directory disconnected before response"))?
}

fn admit(capacity: &Arc<Semaphore>, closed: &AtomicBool) -> io::Result<OwnedSemaphorePermit> {
  if closed.load(Ordering::Acquire) {
    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "directory owner is stopping"));
  }
  capacity
    .clone()
    .try_acquire_owned()
    .map_err(|_| io::Error::new(io::ErrorKind::WouldBlock, "directory request capacity reached"))
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::cell::Cell;

  #[test]
  fn busy_directory_rejects_without_blocking_or_leaking_unsubmitted_quota() {
    let directory = Mutex::new(0u32);
    let capacity = Arc::new(Semaphore::new(1));
    let closed = AtomicBool::new(false);
    let guard = directory.lock().unwrap();
    for _ in 0..3 {
      assert_eq!(
        admit_request(&directory, &capacity, &closed).unwrap_err().kind(),
        io::ErrorKind::WouldBlock
      );
      assert_eq!(capacity.available_permits(), 1);
    }
    drop(guard);
    let (guard, response) = admit_request(&directory, &capacity, &closed).unwrap();
    drop(guard);
    assert_eq!(capacity.available_permits(), 0);
    assert_eq!(
      admit_request(&directory, &capacity, &closed).unwrap_err().kind(),
      io::ErrorKind::WouldBlock
    );
    drop(response);
    assert!(admit_request(&directory, &capacity, &closed).is_ok());
  }

  #[test]
  fn poisoned_directory_remains_terminal_and_releases_unsubmitted_quota() {
    let directory = Mutex::new(0u32);
    let capacity = Arc::new(Semaphore::new(1));
    let closed = AtomicBool::new(false);
    let _ = std::panic::catch_unwind(|| {
      let _guard = directory.lock().unwrap();
      panic!("synthetic directory submission failure");
    });
    for _ in 0..2 {
      let error = admit_request(&directory, &capacity, &closed).unwrap_err();
      assert_eq!(error.kind(), io::ErrorKind::Other);
      assert_eq!(error.to_string(), "directory lock poisoned");
      assert_eq!(capacity.available_permits(), 1);
    }
  }

  #[test]
  fn connected_client_validates_options_before_submission_and_preserves_call_error() {
    let context = CallContext::new().unwrap();
    let submitted = Cell::new(0);
    let submit = || submitted.set(submitted.get() + 1);
    let invalid = admit_client_request(&context, 0, submit).unwrap_err();
    assert_eq!(invalid.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(
      invalid.get_ref().unwrap().downcast_ref::<CallError>(),
      Some(&CallError::InvalidOptions)
    );
    assert_eq!(submitted.get(), 0);
    admit_client_request(&context, 1, submit).unwrap();
    assert_eq!(submitted.get(), 1);
    context.shutdown();
    let closed = admit_client_request(&context, 1, submit).unwrap_err();
    assert_eq!(closed.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(
      closed.get_ref().unwrap().downcast_ref::<CallError>(),
      Some(&CallError::ContextClosed)
    );
    assert_eq!(submitted.get(), 1);
  }

  #[test]
  fn retained_control_response_keeps_capacity_and_closed_owner_rejects_new_work() {
    let capacity = Arc::new(Semaphore::new(1));
    let closed = AtomicBool::new(false);
    let in_flight = admit(&capacity, &closed).unwrap();
    assert_eq!(admit(&capacity, &closed).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    closed.store(true, Ordering::Release);
    drop(in_flight);
    assert_eq!(admit(&capacity, &closed).unwrap_err().kind(), io::ErrorKind::BrokenPipe);
  }

  #[tokio::test(flavor = "current_thread")]
  async fn timely_result_survives_late_first_poll_but_late_callback_cannot_succeed() {
    let deadline = Instant::now() - Duration::from_secs(1);
    let (send, receive) = oneshot::channel();
    send
      .send(deadline_result(deadline, deadline - Duration::from_millis(1), Ok(7)))
      .unwrap();
    assert_eq!(wait_response(receive, deadline).await.unwrap(), 7);
    let (send, receive) = oneshot::channel();
    send
      .send(deadline_result(deadline, deadline + Duration::from_millis(1), Ok(9)))
      .unwrap();
    assert_eq!(
      wait_response(receive, deadline).await.unwrap_err().kind(),
      io::ErrorKind::TimedOut
    );
  }

  #[tokio::test(flavor = "current_thread")]
  async fn submission_deadline_does_not_restart_on_first_poll() {
    let (_send, receive) = oneshot::channel::<io::Result<()>>();
    let deadline = Instant::now() - Duration::from_secs(1);
    assert_eq!(
      wait_response(receive, deadline).await.unwrap_err().kind(),
      io::ErrorKind::TimedOut
    );
  }
}
