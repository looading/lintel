//! Run once with `--expect-secondary`, then launch the same binary with
//! `lintel-activation://open/42 --from-secondary`. The secondary exits before GUI setup.
#[cfg(feature = "desktop")]
fn main() -> lintel::Result<()> {
  use lintel::{
    WindowManager,
    iced::{Element, Task, widget::text, window},
    runtime::{ActivationEvent, ActivationSource},
  };
  use std::{
    sync::{
      Arc,
      atomic::{AtomicBool, Ordering},
    },
    time::Duration,
  };

  struct State {
    windows: WindowManager,
    received: Arc<AtomicBool>,
    initial_received: Arc<AtomicBool>,
    expect_initial: bool,
    expect_reopen: bool,
  }
  #[derive(Clone, Debug)]
  enum Message {
    Activation(ActivationEvent),
    Timeout,
  }

  let expect_initial = std::env::args().any(|argument| argument == "--expect-initial");
  let expect_secondary = std::env::args().any(|argument| argument == "--expect-secondary");
  let expect_protocol = std::env::args().any(|argument| argument == "--expect-protocol");
  let expect_reopen = std::env::args().any(|argument| argument == "--expect-reopen");
  let show_window = std::env::args().any(|argument| argument == "--show-window");
  let scheme = std::env::args()
    .find_map(|argument| argument.strip_prefix("--protocol-scheme=").map(str::to_owned))
    .unwrap_or_else(|| "lintel-activation".into());
  let app_id = std::env::args()
    .find_map(|argument| argument.strip_prefix("--app-id=").map(str::to_owned))
    .unwrap_or_else(|| "org.lintel.examples.activation".into());
  let received = Arc::new(AtomicBool::new(false));
  let evidence = received.clone();
  let initial_received = Arc::new(AtomicBool::new(false));
  let initial_evidence = initial_received.clone();
  let app = lintel::multi_window(
    move |windows: WindowManager| {
      println!("PRIMARY_BOOT");
      windows
        .open(
          "Lintel activation probe",
          window::Settings {
            visible: show_window,
            ..Default::default()
          },
        )
        .expect("queue probe window");
      (
        State {
          windows,
          received: evidence.clone(),
          initial_received: initial_evidence.clone(),
          expect_initial,
          expect_reopen,
        },
        Task::perform(async { tokio::time::sleep(Duration::from_secs(10)).await }, |_| {
          Message::Timeout
        }),
      )
    },
    |state: &mut State, message| {
      match message {
        Message::Activation(ActivationEvent::Launch {
          source: ActivationSource::SecondaryInstance,
          arguments,
          working_directory,
          urls,
        }) => {
          println!("SECONDARY_ACTIVATION arguments={arguments:?} cwd={working_directory:?} urls={urls:?}");
          state.received.store(true, Ordering::SeqCst);
          state.windows.quit().expect("queue probe shutdown");
        }
        Message::Activation(ActivationEvent::Launch {
          source: ActivationSource::Initial,
          arguments,
          working_directory,
          urls,
        }) => {
          println!("INITIAL_ACTIVATION arguments={arguments:?} cwd={working_directory:?} urls={urls:?}");
          state.initial_received.store(true, Ordering::SeqCst);
          if state.expect_initial {
            state.windows.quit().expect("queue initial activation shutdown");
          }
        }
        Message::Activation(ActivationEvent::Opened { urls }) => {
          println!("PROTOCOL_ACTIVATION urls={urls:?}");
          state.received.store(true, Ordering::SeqCst);
          state.windows.quit().expect("queue native protocol shutdown");
        }
        Message::Activation(ActivationEvent::Reopen { has_visible_window }) if state.expect_reopen => {
          println!("REOPEN_ACTIVATION has_visible_window={has_visible_window}");
          state.received.store(true, Ordering::SeqCst);
          state.windows.quit().expect("queue native reopen shutdown");
        }
        Message::Timeout => {
          state.windows.quit().expect("queue timeout shutdown");
        }
        _ => {}
      }
      Task::none()
    },
    |_state: &State, _window| -> Element<'_, Message> { text("Lintel activation probe").into() },
  )
  .id(app_id)
  .single_instance()
  .protocol_schemes([scheme])
  .on_activation(Message::Activation);
  app.run()?;
  if expect_initial && !initial_received.load(Ordering::SeqCst) {
    anyhow::bail!("initial activation did not reach application state");
  }
  if (expect_secondary || expect_protocol || expect_reopen) && !received.load(Ordering::SeqCst) {
    anyhow::bail!("primary did not receive the expected launch/protocol activation before timeout");
  }
  println!("PROCESS_EXIT");
  Ok(())
}

#[cfg(not(feature = "desktop"))]
fn main() -> lintel::Result<()> {
  anyhow::bail!("enable the desktop feature to run the activation example")
}
