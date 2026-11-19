use std::sync::Arc;

pub use lintel_lib_iced::{self as iced, *};

pub enum IcedPluginEvent {
  Create { program: Box<dyn IcedApplication> },
  Close { program_id: ProgramId },
  Task(Arc<dyn AnyMessage>),
}

/// 程序命令未能进入有界渲染队列；成功只表示已按 FIFO 接收。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProgramCommandError {
  QueueFull,
  RendererStopped,
  ApplicationStopping,
  QueuePoisoned,
  DispatchUnsupported,
  CreationUnsupported,
}
impl std::fmt::Display for ProgramCommandError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str(match self {
      Self::QueueFull => "iced program command queue is full",
      Self::RendererStopped => "iced renderer has stopped",
      Self::ApplicationStopping => "application is shutting down",
      Self::QueuePoisoned => "iced program command queue lock is poisoned",
      Self::DispatchUnsupported => "this iced service does not support ordered message dispatch",
      Self::CreationUnsupported => "this iced service does not support bounded program creation",
    })
  }
}
impl std::error::Error for ProgramCommandError {}

pub trait IcedAppPluginApi: Send + Sync + 'static {
  fn create(&self, program: Box<dyn IcedApplication>);
  fn close(&self, program_id: ProgramId);
  /// Explicit bounded creation admission. Older services must opt into this capability.
  fn try_create(&self, _program: Box<dyn IcedApplication>) -> std::result::Result<(), ProgramCommandError> {
    Err(ProgramCommandError::CreationUnsupported)
  }
  /// 与 Create/Close 共用同一 FIFO，避免启动消息先于程序创建。不会阻塞主线程。
  /// 旧实现默认明确拒绝此能力，保持 trait 对已有实现的源码兼容。
  fn try_dispatch(&self, _message: Arc<dyn AnyMessage>) -> std::result::Result<(), ProgramCommandError> {
    Err(ProgramCommandError::DispatchUnsupported)
  }
}

/// 面向具体消息类型的便利入口；底层仍经过 object-safe 的同一 FIFO。
pub trait IcedAppPluginApiExt: IcedAppPluginApi {
  fn dispatch<Message: AnyMessage>(&self, message: Message) -> std::result::Result<(), ProgramCommandError> {
    self.try_dispatch(Arc::new(message))
  }
}
impl<Service: IcedAppPluginApi + ?Sized> IcedAppPluginApiExt for Service {}
