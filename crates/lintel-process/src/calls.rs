use std::{
  any::Any,
  future::Future,
  marker::PhantomData,
  pin::Pin,
  sync::{Arc, Mutex, MutexGuard, TryLockError},
  task::{Context, Poll},
  time::Duration,
};

use mojo_ipc::{PendingRemote, Remote, Runtime, bindings::interface::DynMojomInterface};
use mojo_service::{Call, CallCancellation, CallClient, CallContext, CallError, CallResponder};

type ClientOwner = Arc<dyn Any + Send + Sync>;

/// 已发现并绑定的类型化服务；保留目录选择的 provider generation 供业务显式重连。
/// generation 不是进程 identity，也不会使框架自动重放调用。
pub struct ConnectedService<T: DynMojomInterface + ?Sized + 'static> {
  pub client: ServiceClient<T>,
  pub generation: u64,
}

/// 可共享的类型化代理，返回值 Future 不保留接口泛型的原生投影。
/// 这样真实 Mojom trait object 可以直接进入要求 Send 的 UI Task。
pub struct ServiceClient<T: DynMojomInterface + ?Sized + 'static> {
  owner: ClientOwner,
  interface: PhantomData<fn(*const T)>,
}

impl<T: DynMojomInterface + ?Sized + 'static> Clone for ServiceClient<T> {
  fn clone(&self) -> Self {
    Self {
      owner: self.owner.clone(),
      interface: PhantomData,
    }
  }
}

impl<T: DynMojomInterface + ?Sized + 'static> ServiceClient<T> {
  pub fn bind(
    runtime: &Runtime,
    context: &CallContext,
    remote: PendingRemote<T>,
    capacity: usize,
  ) -> Result<Self, CallError> {
    let client = CallClient::bind(runtime, context, remote, capacity)?;
    Ok(Self {
      owner: Arc::new(Mutex::new(client)),
      interface: PhantomData,
    })
  }

  fn client(&self) -> Result<&Mutex<CallClient<T>>, CallError> {
    self
      .owner
      .downcast_ref::<Mutex<CallClient<T>>>()
      .ok_or(CallError::InvalidOptions)
  }

  fn lock(&self) -> Result<MutexGuard<'_, CallClient<T>>, CallError> {
    lock_client(self.client()?)
  }

  /// 同步提交一个响应 RPC，闭包必须立即发送且不能阻塞。
  /// Future/本地取消的语义沿用上游，已发送请求不自动重放或回滚。
  pub fn call<R: Send + 'static>(
    &self,
    timeout: Duration,
    send: impl FnOnce(&mut Remote<T>, CallResponder<R>),
  ) -> Result<ServiceCall<R>, CallError> {
    let call = self.lock()?.call(timeout, send)?;
    Ok(ServiceCall {
      call,
      owner: self.owner.clone(),
    })
  }

  pub fn is_connected(&self) -> Result<bool, CallError> {
    Ok(self.lock()?.is_connected())
  }
  pub fn outstanding_requests(&self) -> Result<usize, CallError> {
    Ok(self.lock()?.outstanding_requests())
  }
  pub fn pending_calls(&self) -> Result<usize, CallError> {
    Ok(self.lock()?.pending_calls())
  }
}

fn lock_client<T>(client: &Mutex<T>) -> Result<MutexGuard<'_, T>, CallError> {
  client.try_lock().map_err(|error| match error {
    TryLockError::WouldBlock => CallError::WouldBlock,
    TryLockError::Poisoned(_) => CallError::Disconnected,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn reentrant_client_access_reports_would_block_and_poison_is_terminal() {
    let client = Mutex::new(0u32);
    let active_send = lock_client(&client).unwrap();
    assert_eq!(lock_client(&client).unwrap_err(), CallError::WouldBlock);
    drop(active_send);
    assert!(lock_client(&client).is_ok());
    let _ = std::panic::catch_unwind(|| {
      let _active_send = client.lock().unwrap();
      panic!("synthetic send callback failure");
    });
    assert_eq!(lock_client(&client).unwrap_err(), CallError::Disconnected);
  }
}

/// 保留代理 owner 到调用结束或被丢弃；接口类型不进入业务 Future 状态。
pub struct ServiceCall<R: Send + 'static> {
  call: Call<R>,
  owner: ClientOwner,
}

impl<R: Send + 'static> ServiceCall<R> {
  pub fn cancel(&self) -> bool {
    self.call.cancel()
  }
  pub fn cancellation_handle(&self) -> CallCancellation {
    self.call.cancellation_handle()
  }
  pub fn is_finished(&self) -> bool {
    self.call.is_finished()
  }
  pub fn wait(self) -> Result<R, CallError> {
    self.call.wait()
  }
}

impl<R: Send + 'static> Future for ServiceCall<R> {
  type Output = Result<R, CallError>;
  fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
    let this = self.get_mut();
    // owner 在 call Future 之后释放，避免等待期间代理断连。
    let _owner = &this.owner;
    Pin::new(&mut this.call).poll(context)
  }
}
