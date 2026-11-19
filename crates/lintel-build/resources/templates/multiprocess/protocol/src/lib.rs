//! 主进程与服务进程共享的业务协议。
include!(concat!(env!("OUT_DIR"), "/mojom/mod.rs"));
pub const SERVICE_NAME: &str = "example.echo";
pub const INTERFACE_NAME: &str = "lintel.example.Echo";
