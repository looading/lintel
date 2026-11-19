//! A normal application using the store, diagnostics and network service plugins.
use std::sync::{
  Arc,
  atomic::{AtomicBool, Ordering},
};

use lintel::{
  core::{
    context::AppContext,
    plugin::{Plugin, PluginLifecycle},
    tasks::TaskScope,
  },
  iced::{
    Element, Subscription, Task,
    widget::{button, center, column, text},
    window,
  },
  services::{self, DiagnosticConfig, Diagnostics, FileAccess, NetworkClient, NetworkOptions, Store, StoreConfig},
};

struct CheckServices(Arc<AtomicBool>);

impl Plugin for CheckServices {
  fn name(&self) -> &str {
    "example.services-check"
  }
}

impl PluginLifecycle for CheckServices {
  fn on_start(&self, context: &AppContext) -> lintel::Result<()> {
    let store = context
      .get_service::<Store>()
      .ok_or_else(|| anyhow::anyhow!("store service missing"))?;
    store.set("count".into(), 42.into())?;
    anyhow::ensure!(store.get("count") == Some(42.into()), "store roundtrip failed");
    let diagnostics = context
      .get_service::<Diagnostics>()
      .ok_or_else(|| anyhow::anyhow!("diagnostics missing"))?;
    let network = context
      .get_service::<NetworkClient>()
      .ok_or_else(|| anyhow::anyhow!("network missing"))?;
    let finished = Arc::clone(&self.0);
    context.spawn_task(TaskScope::Application, async move {
      // Empty origin grants reject before DNS or network traffic.
      let result = network.get_bytes("https://example.invalid").await;
      let passed = result.is_err()
        && diagnostics
          .record("services_ready", serde_json::json!({ "store_count": 42 }))
          .is_ok();
      finished.store(passed, Ordering::Release);
    })?;
    Ok(())
  }
}

#[derive(Clone, Debug)]
enum Message {
  Increment,
  Check,
}

struct State {
  count: u32,
  ticks: u32,
  smoke: bool,
  ready: Arc<AtomicBool>,
}

fn update(state: &mut State, message: Message) -> Task<Message> {
  match message {
    Message::Increment => state.count += 1,
    Message::Check => {
      state.ticks += 1;
      if state.smoke && (state.ready.load(Ordering::Acquire) || state.ticks >= 100) {
        return window::latest().then(|id| id.map_or_else(Task::none, window::close));
      }
    }
  }
  Task::none()
}

fn view(state: &State) -> Element<'_, Message> {
  center(
    column![
      text(format!("Count: {}", state.count)),
      text(if state.ready.load(Ordering::Acquire) {
        "Store, files, diagnostics and network policy ready"
      } else {
        "Starting services"
      }),
      button("Increment").on_press(Message::Increment),
    ]
    .spacing(12),
  )
  .into()
}

fn main() -> lintel::Result<()> {
  let smoke = std::env::args().any(|arg| arg == "--smoke");
  let directory = tempfile::tempdir()?;
  let files = FileAccess::read_write(directory.path(), 1024)?;
  files.write("example.json", br#"{"count":42}"#)?;
  anyhow::ensure!(
    files.read("example.json")? == br#"{"count":42}"#,
    "file capability roundtrip failed"
  );
  let ready = Arc::new(AtomicBool::new(false));
  let startup = Arc::clone(&ready);
  let check = Arc::clone(&ready);
  lintel::application(
    move || State {
      count: 0,
      ticks: 0,
      smoke,
      ready: Arc::clone(&startup),
    },
    update,
    view,
  )
  .id("dev.lintel.services")
  .title("Lintel services")
  .subscription(|_| lintel::iced::time::every(std::time::Duration::from_millis(100)).map(|_| Message::Check))
  .plugin(services::store(StoreConfig {
    cwd: directory.path().join("data"),
    name: "settings".into(),
    ..Default::default()
  }))
  .plugin(services::diagnostics(DiagnosticConfig {
    directory: Some(directory.path().join("diagnostics")),
    ..Default::default()
  }))
  .plugin(services::network(NetworkOptions::default()))
  .plugin(move |context| context.register_plugin(Arc::new(CheckServices(check))))
  .run()?;
  anyhow::ensure!(ready.load(Ordering::Acquire), "service startup did not complete");
  if smoke {
    println!("LINTEL_SERVICES_SMOKE_OK");
  }
  Ok(())
}
