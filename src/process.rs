//! 可选 Mojo 服务进程；复用现有应用上下文与退出路径。
pub use lintel_process::*;

use std::sync::Arc;

use crate::{AppContext, Result};

/// 在运行时仍存活时等待服务监管线程清理，覆盖 GUI 创建前的错误返回。
pub(crate) struct ProcessOwner {
  host: Option<Arc<ProcessHost>>,
  runtime: tokio::runtime::Handle,
}

impl ProcessOwner {
  pub(crate) fn start(config: Option<HostConfig>, runtime: tokio::runtime::Handle) -> Result<Self> {
    Ok(Self {
      host: config.map(ProcessHost::start).transpose()?.map(Arc::new),
      runtime,
    })
  }

  pub(crate) fn register(&self, context: &AppContext) {
    if let Some(host) = &self.host {
      let host = host.clone();
      context.register_service(move || host.clone());
    }
  }

  pub(crate) async fn shutdown(&mut self) -> Result<()> {
    if let Some(host) = self.host.take() {
      host.shutdown().await?;
    }
    Ok(())
  }
}

impl Drop for ProcessOwner {
  fn drop(&mut self) {
    if let Some(host) = self.host.take() {
      host.request_shutdown();
      // 该 guard 只在 AppBuilder 主线程栈上：正常退出已异步清空；
      // GUI 创建/插件启动失败时事件循环未运行，运行时仍按声明顺序存活。
      if let Err(error) = self.runtime.block_on(host.shutdown()) {
        tracing::error!(%error, "cannot finish service process cleanup after startup failure");
      }
    }
  }
}
