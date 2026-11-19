use std::time::Duration;

use lintel::process::{ProcessWorker, ServiceKey, TrustedRuntimeLibrary, ipc};
use lintel_app_protocol::{INTERFACE_NAME, SERVICE_NAME, echo::Echo};

#[path = "../crash_options.rs"]
mod crash_options;

struct EchoService;
#[allow(non_snake_case)]
impl Echo for EchoService {
  fn Ping(&mut self, text: String, reply: impl FnOnce(String, u32) + Send + 'static) {
    reply(text, std::process::id());
  }
}
ipc::register_mojom_state_object_impls!(impl Echo for EchoService);

fn main() -> lintel::Result<()> {
  let _crash = crash_options::parse(
    std::env::args_os().skip(1),
    cfg!(feature = "service-crash-reports"),
    false,
  )
  .map_err(lintel::Error::msg)?;
  // SAFETY: 发行方负责审计随应用打包的 SDK 0.3.0 原生镜像与协议来源。
  let library = unsafe { TrustedRuntimeLibrary::bundled()? };
  #[cfg(feature = "service-crash-reports")]
  if let Some(directory) = _crash.service_directory {
    let mut config = lintel::crash::CrashConfig::new(
      directory,
      lintel::process::service_executable("lintel-worker-crash-helper")?,
    );
    config.application_id = concat!("dev.example.", env!("CARGO_PKG_NAME"), ".echo-worker").into();
    config.version = env!("CARGO_PKG_VERSION").into();
    let reported = ProcessWorker::connect_with_crash_reporting(library, "echo-worker", config)?;
    return serve(&reported);
  }
  let worker = ProcessWorker::connect(library)?;
  serve(&worker)
}

fn serve(worker: &ProcessWorker) -> lintel::Result<()> {
  worker.publish_blocking(
    &ServiceKey::new(SERVICE_NAME, INTERFACE_NAME),
    1,
    |pipe| {
      ipc::PendingReceiver::<dyn Echo>::new(pipe).bind_self_owned(EchoService);
    },
    Duration::from_secs(5),
  )?;
  println!("LINTEL_SERVICE_READY pid={}", std::process::id());
  worker.wait()?;
  Ok(())
}
