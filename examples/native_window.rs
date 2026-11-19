//! A small owned-window integration without exposing platform handles to application state.
#[cfg(all(feature = "public-iced", not(target_arch = "wasm32")))]
fn main() -> lintel::Result<()> {
  use lintel::{
    WindowEvent, WindowManager,
    iced::{Element, Task, widget::text},
  };
  use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  };

  struct State {
    windows: WindowManager,
    proof: Arc<AtomicBool>,
  }
  #[derive(Clone, Debug)]
  enum Message {
    Window(WindowEvent),
    Proof(lintel::window::WindowId, Result<bool, lintel::window::WindowError>),
  }
  let proof = Arc::new(AtomicBool::new(false));
  let completed = proof.clone();
  lintel::multi_window(
    move |windows: WindowManager| {
      windows
        .open("Lintel native integration", Default::default())
        .expect("queue native window");
      (
        State {
          windows,
          proof: completed.clone(),
        },
        Task::none(),
      )
    },
    |state: &mut State, message| {
      match message {
        Message::Window(WindowEvent::Opened(id)) => {
          return state
            .windows
            .with_native_window(id, |window| window.window_handle().is_ok())
            .expect("opened window admits native task")
            .map(move |proof| Message::Proof(id, proof));
        }
        Message::Proof(id, proof) => {
          assert!(
            proof.expect("native task stayed live"),
            "native window handle is available"
          );
          state.proof.store(true, Ordering::SeqCst);
          println!("LINTEL_NATIVE_WINDOW_PROOF_OK");
          state.windows.destroy(id).expect("close native probe");
        }
        _ => {}
      }
      Task::none()
    },
    |_state: &State, _| -> Element<'_, Message> { text("Lintel native integration").into() },
  )
  .id("dev.lintel.examples.native-window")
  .on_window_event(Message::Window)
  .run()?;
  anyhow::ensure!(proof.load(Ordering::SeqCst), "native integration did not run");
  Ok(())
}

#[cfg(not(all(feature = "public-iced", not(target_arch = "wasm32"))))]
fn main() -> lintel::Result<()> {
  anyhow::bail!("enable the public-iced feature on a native target")
}
