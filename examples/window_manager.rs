//! `cargo run --example window_manager -- --smoke` exercises managed window lifecycle and routing.
use std::{
  collections::BTreeMap,
  sync::{Arc, Mutex},
  time::Duration,
};

use lintel::{
  WindowEvent, WindowId, WindowManager,
  iced::{
    Element, Task,
    widget::{button, center, column, text},
    window,
  },
};

#[derive(Default, Debug)]
struct Evidence {
  opened: usize,
  closed: usize,
  vetoed: usize,
  frames: BTreeMap<WindowId, u64>,
  final_counts: BTreeMap<String, u32>,
  errors: Vec<String>,
}

struct Entry {
  name: String,
  count: u32,
}

struct State {
  windows: WindowManager,
  entries: BTreeMap<WindowId, Entry>,
  a: WindowId,
  b: WindowId,
  reopened: Option<WindowId>,
  veto_a: bool,
  smoke: bool,
  evidence: Arc<Mutex<Evidence>>,
}

#[derive(Clone, Debug)]
enum Message {
  Window(WindowEvent),
  Increment(WindowId),
  Capture(WindowId),
  Frame(WindowId, Arc<window::Screenshot>),
  Open,
  Close(WindowId),
  Quit,
  Timeout,
}

fn later(message: Message, millis: u64) -> Task<Message> {
  Task::perform(
    async move {
      tokio::time::sleep(Duration::from_millis(millis)).await;
      message
    },
    |message| message,
  )
}

fn update(state: &mut State, message: Message) -> Task<Message> {
  match message {
    Message::Increment(id) => {
      if let Some(entry) = state.entries.get_mut(&id) {
        entry.count += 1;
      }
    }
    Message::Open => {
      let id = state
        .windows
        .open("Lintel replacement", window::Settings::default())
        .unwrap();
      state.entries.insert(
        id,
        Entry {
          name: "Replacement".into(),
          count: 7,
        },
      );
    }
    Message::Close(id) => {
      let _ = state.windows.close(id);
    }
    Message::Quit => {
      let _ = state.windows.quit();
    }
    Message::Timeout => {
      if state.smoke {
        state
          .evidence
          .lock()
          .unwrap()
          .errors
          .push("window smoke timed out".into());
        let _ = state.windows.quit();
      }
    }
    Message::Window(WindowEvent::Opened(id)) => {
      state.evidence.lock().unwrap().opened += 1;
      if state.smoke {
        if id == state.a || id == state.b {
          state.entries.get_mut(&id).unwrap().count = if id == state.a { 1 } else { 2 };
        }
        return later(Message::Capture(id), 250);
      }
    }
    Message::Capture(id) if state.windows.contains(id) => {
      return window::screenshot(id).map(move |frame| Message::Frame(id, Arc::new(frame)));
    }
    Message::Frame(id, frame) => {
      use std::hash::{Hash, Hasher};
      let mut hasher = std::hash::DefaultHasher::new();
      frame.rgba.hash(&mut hasher);
      let fingerprint = hasher.finish();
      let mut pixels = frame.rgba.chunks_exact(4);
      let valid = frame.rgba.len() == frame.size.width as usize * frame.size.height as usize * 4
        && pixels
          .next()
          .is_some_and(|first| pixels.any(|pixel| pixel[..3] != first[..3]));
      let mut evidence = state.evidence.lock().unwrap();
      if !valid {
        evidence
          .errors
          .push(format!("window {id} did not render visible content"));
      }
      evidence.frames.insert(id, fingerprint);
      let first_pair_ready = evidence.frames.contains_key(&state.a) && evidence.frames.contains_key(&state.b);
      drop(evidence);
      if Some(id) == state.reopened {
        state.windows.close(state.b).unwrap();
      } else if first_pair_ready && state.veto_a {
        state.windows.close(state.a).unwrap();
      }
    }
    Message::Window(WindowEvent::CloseVetoed(id)) => {
      state.evidence.lock().unwrap().vetoed += 1;
      if state.smoke && id == state.a {
        state.veto_a = false;
        state.windows.close(id).unwrap();
      }
    }
    Message::Window(WindowEvent::Closed(id)) => {
      let entry = state.entries.remove(&id).unwrap();
      let mut evidence = state.evidence.lock().unwrap();
      evidence.closed += 1;
      evidence.final_counts.insert(entry.name, entry.count);
      drop(evidence);
      if state.smoke && id == state.a {
        assert!(state.windows.contains(state.b));
        assert!(
          state.windows.close(id).is_err(),
          "expired handles must reject late closes"
        );
        state.entries.get_mut(&state.b).unwrap().count += 1;
        let id = state
          .windows
          .open("Lintel reopened C", window::Settings::default())
          .unwrap();
        state.reopened = Some(id);
        state.entries.insert(
          id,
          Entry {
            name: "C".into(),
            count: 7,
          },
        );
      } else if state.smoke && id == state.b {
        let c = state.reopened.unwrap();
        assert!(state.windows.contains(c));
        state.windows.close(c).unwrap();
      }
    }
    Message::Window(WindowEvent::OpenFailed { id, error }) => {
      state
        .evidence
        .lock()
        .unwrap()
        .errors
        .push(format!("window {id}: {error}"));
      let _ = state.windows.quit();
    }
    _ => {}
  }
  Task::none()
}

fn view(state: &State, id: WindowId) -> Element<'_, Message> {
  let Some(entry) = state.entries.get(&id) else {
    return text("Closing…").into();
  };
  center(
    column![
      text(entry.name.as_str()).size(32),
      text(format!("Window {id}: local count {}", entry.count)),
      button("Increment this window").on_press(Message::Increment(id)),
      button("Open another window").on_press(Message::Open),
      button("Close this window").on_press(Message::Close(id)),
      button("Quit application").on_press(Message::Quit),
    ]
    .spacing(12),
  )
  .into()
}

fn main() -> lintel::Result<()> {
  tracing_subscriber::fmt().with_target(false).init();
  let smoke = std::env::args().any(|arg| arg == "--smoke");
  let evidence = Arc::new(Mutex::new(Evidence::default()));
  let boot_evidence = evidence.clone();
  let app = lintel::multi_window(
    move |windows: WindowManager| {
      let a = windows.open("Lintel managed A", window::Settings::default()).unwrap();
      let b = windows.open("Lintel managed B", window::Settings::default()).unwrap();
      let entries = BTreeMap::from([
        (
          a,
          Entry {
            name: "A".into(),
            count: 0,
          },
        ),
        (
          b,
          Entry {
            name: "B".into(),
            count: 0,
          },
        ),
      ]);
      (
        State {
          windows,
          entries,
          a,
          b,
          reopened: None,
          veto_a: smoke,
          smoke,
          evidence: boot_evidence.clone(),
        },
        if smoke {
          later(Message::Timeout, 12_000)
        } else {
          Task::none()
        },
      )
    },
    update,
    view,
  )
  .id("dev.lintel.window-manager")
  .title("Lintel Window Manager")
  .on_window_event(Message::Window)
  .close_request(|state, id| !(state.veto_a && id == state.a));
  #[cfg(feature = "crash-reporting")]
  let app = match crash_options()? {
    Some(config) => app.crash_reporting(config),
    None => app,
  };
  app.run()?;
  if smoke {
    let evidence = evidence.lock().unwrap();
    anyhow::ensure!(evidence.errors.is_empty(), "smoke failures: {:?}", evidence.errors);
    anyhow::ensure!(
      evidence.opened == 3 && evidence.closed == 3 && evidence.vetoed == 1,
      "expected A/B/reopened C to open and close, with one close veto: {evidence:?}"
    );
    anyhow::ensure!(
      evidence.frames.len() == 3
        && evidence
          .frames
          .values()
          .collect::<std::collections::BTreeSet<_>>()
          .len()
          == 3,
      "each window must render a distinct frame: {evidence:?}"
    );
    anyhow::ensure!(
      evidence.final_counts == BTreeMap::from([("A".into(), 1), ("B".into(), 3), ("C".into(), 7)]),
      "window-local state routing failed: {evidence:?}"
    );
    println!(
      "managed multi-window smoke passed: A/B closed independently, C reopened, routing and close veto verified"
    );
  }
  Ok(())
}

#[cfg(feature = "crash-reporting")]
fn crash_options() -> lintel::Result<Option<lintel::crash::CrashConfig>> {
  use anyhow::{Context, ensure};
  let mut helper = None;
  let mut directory = None;
  let mut arguments = std::env::args_os().skip(1);
  while let Some(argument) = arguments.next() {
    if argument == "--crash-helper" {
      ensure!(helper.is_none(), "--crash-helper can only be supplied once");
      helper = Some(std::path::PathBuf::from(
        arguments.next().context("--crash-helper requires an absolute path")?,
      ));
    } else if argument == "--crash-directory" {
      ensure!(directory.is_none(), "--crash-directory can only be supplied once");
      directory = Some(std::path::PathBuf::from(
        arguments
          .next()
          .context("--crash-directory requires an absolute path")?,
      ));
    }
  }
  if helper.is_none() && directory.is_none() {
    return Ok(None);
  }
  let mut config = lintel::crash::CrashConfig::new(
    directory.context("--crash-directory is required with --crash-helper")?,
    helper.context("--crash-helper is required with --crash-directory")?,
  );
  config.application_id = "dev.lintel.window-manager".into();
  config.version = env!("CARGO_PKG_VERSION").into();
  Ok(Some(config))
}
