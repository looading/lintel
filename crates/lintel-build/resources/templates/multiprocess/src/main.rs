use std::{
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  time::Duration,
};

use lintel::{
  AppContextApi,
  iced::{
    Element, Subscription, Task,
    widget::{button, column, text},
  },
  process::{
    AccessPolicy, CallContext, HostConfig, ProcessHost, ProcessSpec, RestartPolicy, ServiceKey, TrustedRuntimeLibrary,
    service_executable,
  },
};
use lintel_app_protocol::{INTERFACE_NAME, SERVICE_NAME, echo::Echo};

mod crash_options;

struct State {
  host: Arc<ProcessHost>,
  calls: CallContext,
  status: String,
  started: bool,
  smoke: bool,
  ticks: u32,
  succeeded: Arc<AtomicBool>,
}
#[derive(Clone, Debug)]
enum Message {
  Ping,
  Reply(Result<(String, u32), String>),
  Tick,
}

fn key() -> ServiceKey {
  ServiceKey::new(SERVICE_NAME, INTERFACE_NAME)
}

async fn ping(host: Arc<ProcessHost>, calls: CallContext) -> Result<(String, u32), String> {
  host
    .wait_ready("echo-worker", Duration::from_secs(5))
    .await
    .map_err(|e| e.to_string())?;
  let connection = host
    .directory()
    .connect_client::<dyn Echo>(&calls, &key(), 1, 16, Duration::from_secs(5))
    .await
    .map_err(|e| e.to_string())?;
  connection
    .client
    .call(Duration::from_secs(3), |remote, reply| {
      remote.Ping("Hello from Lintel".into(), move |text, pid| {
        reply.respond((text, pid));
      });
    })
    .map_err(|e| e.to_string())?
    .await
    .map_err(|e| e.to_string())
}

fn update(state: &mut State, message: Message) -> Task<Message> {
  match message {
    Message::Ping => return Task::perform(ping(state.host.clone(), state.calls.clone()), Message::Reply),
    Message::Reply(Ok((reply, pid))) => {
      let separate = pid != std::process::id();
      state.succeeded.store(separate, Ordering::Release);
      state.status = format!("{reply} — service PID {pid}");
      println!(
        "LINTEL_MULTIPROCESS_RPC_OK host_pid={} service_pid={pid}",
        std::process::id()
      );
      if state.smoke {
        return lintel::iced::exit();
      }
    }
    Message::Reply(Err(error)) => {
      state.status = error;
      if state.smoke {
        return lintel::iced::exit();
      }
    }
    Message::Tick => {
      state.ticks += 1;
      if state.smoke && !state.started {
        state.started = true;
        return update(state, Message::Ping);
      }
      if state.smoke && state.ticks >= 300 {
        return lintel::iced::exit();
      }
    }
  }
  Task::none()
}
fn view(state: &State) -> Element<'_, Message> {
  column![
    text("Lintel multi-process service"),
    text(&state.status),
    button("Call independent Echo service").on_press(Message::Ping),
  ]
  .spacing(16)
  .padding(32)
  .into()
}

fn main() -> lintel::Result<()> {
  let crash = crash_options::parse(
    std::env::args_os().skip(1),
    cfg!(feature = "service-crash-reports"),
    false,
  )
  .map_err(lintel::Error::msg)?;
  let smoke = std::env::args_os().any(|a| a == "--smoke");
  let fail_boot = std::env::args_os().any(|a| a == "--fail-boot");
  let succeeded = Arc::new(AtomicBool::new(false));
  let proof = succeeded.clone();
  // SAFETY: 此应用的发行方明确选择并审计了包内匹配 SDK 的原生库。
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
  lintel::application_with_context(
    move |context| {
      if fail_boot {
        return Err(lintel::Error::msg("requested multiprocess initialization failure"));
      }
      let host = context
        .get_service::<ProcessHost>()
        .ok_or_else(|| lintel::Error::msg("process host was not started"))?;
      Ok(State {
        host,
        calls: CallContext::new()?,
        status: "Service is starting".into(),
        started: false,
        smoke,
        ticks: 0,
        succeeded: proof.clone(),
      })
    },
    update,
    view,
  )
  .id(concat!("dev.example.", env!("CARGO_PKG_NAME")))
  .title("Lintel Multi-process")
  .processes(config)
  .subscription(|_: &State| lintel::iced::time::every(Duration::from_millis(50)).map(|_| Message::Tick))
  .run()?;
  if smoke && !succeeded.load(Ordering::Acquire) {
    return Err(lintel::Error::msg("cross-process service call did not complete"));
  }
  println!("LINTEL_MULTIPROCESS_CLEANED");
  Ok(())
}
