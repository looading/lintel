use std::{
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
  time::Duration,
};

use lintel::{
  TaskScope, WebviewCommandContext, WebviewWindowEvent,
  iced::futures::channel::oneshot,
  process::{
    AccessPolicy, CallContext, HostConfig, ProcessHost, ProcessSpec, RestartPolicy, ServiceKey, TrustedRuntimeLibrary,
    service_executable,
  },
};
use lintel_app_protocol::{INTERFACE_NAME, SERVICE_NAME, echo::Echo};

mod crash_options;

fn key() -> ServiceKey {
  ServiceKey::new(SERVICE_NAME, INTERFACE_NAME)
}

async fn echo(context: WebviewCommandContext, text: String) -> lintel::Result<(String, u32)> {
  let host = context.require_service::<ProcessHost>()?;
  let calls = context.require_service::<CallContext>()?;
  host.wait_ready("echo-worker", Duration::from_secs(5)).await?;
  let connection = host
    .directory()
    .connect_client::<dyn Echo>(&calls, &key(), 1, 16, Duration::from_secs(3))
    .await?;
  Ok(
    connection
      .client
      .call(Duration::from_secs(3), |remote, reply| {
        remote.Ping(text, move |text, pid| {
          reply.respond((text, pid));
        });
      })?
      .await?,
  )
}

fn main() -> lintel::Result<()> {
  let crash = crash_options::parse(
    std::env::args_os().skip(1),
    cfg!(feature = "service-crash-reports"),
    true,
  )
  .map_err(lintel::Error::msg)?;
  let smoke = std::env::args_os().any(|arg| arg == "--smoke");
  let fail_boot = std::env::args_os().any(|arg| arg == "--fail-boot");
  let entered = Arc::new(AtomicBool::new(false));
  let passed = Arc::new(AtomicBool::new(false));
  let (send, receive) = oneshot::channel::<bool>();
  let complete = Arc::new(Mutex::new(Some(send)));
  let receive = Arc::new(Mutex::new(Some(receive)));
  let report = complete.clone();
  let report_passed = passed.clone();
  let errors = complete.clone();
  let initialized = entered.clone();
  // SAFETY: 发行方明确选择并审计包内匹配 SDK 0.3.0 的原生库。
  // 此处只定位镜像；Mojo 与业务 timer 的初始化在浏览器角色内进行。
  let library = unsafe { TrustedRuntimeLibrary::bundled()? };
  let mut service = ProcessSpec::new(
    "echo-worker",
    service_executable("echo-worker")?,
    AccessPolicy::new().allow_publish(key()),
  )
  .require(key(), 1)
  .startup_timeout(Duration::from_secs(5))
  .shutdown_grace(Duration::from_millis(500))
  .restart_policy(RestartPolicy::OnFailure {
    max_restarts: 2,
    backoff: Duration::from_millis(250),
  });
  if let Some(directory) = crash.service_directory {
    // 显式报告需要容纳 helper 的 5 秒启动和 2 秒清理期限，再留出 Mojo 接入与退出余量。
    service = service
      .arg("--service-crash-directory")
      .arg(directory)
      .startup_timeout(Duration::from_secs(10))
      .shutdown_grace(Duration::from_secs(3));
  }
  let config = HostConfig::new(library)
    .host_policy(AccessPolicy::new().allow_connect(key()))
    .process(service);

  let mut app = lintel::webview_app()
    .id(concat!("dev.example.", env!("CARGO_PKG_NAME")))
    .title("Lintel WebView Services")
    .local_page(include_str!("../ui/index.html"))
    .processes(config)
    .async_command_with_context("echo", echo)
    .command("demo.smoke", move |_: ()| Ok(smoke))
    .command("demo.complete", move |result: (String, u32)| {
      let ok = result.0 == "Hello from WebView" && result.1 != std::process::id();
      report_passed.store(ok, Ordering::Release);
      if let Some(send) = report.lock().unwrap().take() {
        let _ = send.send(ok);
      }
      if ok {
        println!(
          "LINTEL_WEBVIEW_SERVICE_RPC_OK host_pid={} service_pid={}",
          std::process::id(),
          result.1
        );
      }
      Ok(())
    })
    .on_window_event(move |_, event| {
      if smoke
        && matches!(
          event,
          WebviewWindowEvent::LoadFailed { .. } | WebviewWindowEvent::RendererTerminated { .. }
        )
      {
        if let Some(send) = errors.lock().unwrap().take() {
          let _ = send.send(false);
        }
      }
    })
    .try_setup(move |context, windows| {
      initialized.store(true, Ordering::Release);
      let calls = Arc::new(CallContext::new()?);
      context.register_service(move || calls.clone());
      if fail_boot {
        // 验证排队的窗口在初始化失败时回滚，未创建的浏览器不会阻塞退出。
        windows.reopen("main")?;
        return Err(lintel::Error::msg("requested WebView service initialization failure"));
      }
      if smoke {
        let receive = receive
          .lock()
          .unwrap()
          .take()
          .ok_or_else(|| lintel::Error::msg("smoke receiver already used"))?;
        context.spawn_task(TaskScope::Application, async move {
          let _ = receive.await;
          let _ = windows.quit();
        })?;
      }
      Ok(())
    });
  if let Some(directory) = crash.cef_directory {
    app = app.cef_crash_reports(directory);
  }
  app.run(env!("CARGO_PKG_VERSION"))?;
  // CEF Helper 也执行相同 main；只有浏览器角色进入 try_setup。
  if smoke && entered.load(Ordering::Acquire) {
    if !passed.load(Ordering::Acquire) {
      return Err(lintel::Error::msg("WebView service smoke failed"));
    }
    println!("LINTEL_WEBVIEW_SERVICE_CLEANED");
  }
  Ok(())
}
