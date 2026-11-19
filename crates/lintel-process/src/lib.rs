//! 使用既有 Mojo 服务网络管理应用自己的服务进程。
//!
//! IPC 与监管由固定版本的上游 SDK 实现；Lintel 仅负责线程、生命周期与接入。
//! 原生库是可信代码边界，不会从环境变量或工作目录自动加载。

mod calls;
mod directory;
mod host;
mod library;
mod readiness;
mod worker;
#[cfg(feature = "crash-reporting")]
mod worker_crash;

pub use calls::{ConnectedService, ServiceCall, ServiceClient};
pub use directory::{DirectoryClient, DirectoryRequest};
pub use host::{HostConfig, MAX_PROCESS_SPECS, ProcessHost};
pub use library::{TrustedRuntimeLibrary, service_executable};
pub use worker::ProcessWorker;
#[cfg(feature = "crash-reporting")]
pub use worker_crash::CrashReportingWorker;

#[cfg(feature = "crash-reporting")]
pub use lintel_crash as crash;
pub use mojo_ipc as ipc;
pub use mojo_service as service;
pub use mojo_service::{
  AccessPolicy, Call, CallCancellation, CallClient, CallContext, CallError, CallResponder, FactoryBinding,
  HealthPolicy, HealthStatus, ProcessEvent, ProcessEventKind, ProcessId, ProcessSnapshot, ProcessSpec, ProcessState,
  RestartPolicy, ServiceConnection, ServiceDirectory, ServiceError, ServiceInfo, ServiceKey, ServiceSubscription,
  ServiceWatchBatch, WatchError,
};

#[cfg(test)]
mod tests;
