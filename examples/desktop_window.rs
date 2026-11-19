//! Native menus through the high-level window manager; no platform handles in business state.
#[cfg(all(
  feature = "desktop",
  feature = "public-iced",
  any(target_os = "macos", target_os = "windows")
))]
fn main() -> lintel::Result<()> {
  use lintel::{
    WindowEvent, WindowManager,
    desktop::{DesktopEvent, DesktopService, MenuEntry, MenuId, MenuSpec},
    iced::{Element, Task, widget::text},
  };
  use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  };

  struct State {
    windows: WindowManager,
    desktop: Arc<DesktopService>,
    id: lintel::window::WindowId,
    menu: Option<MenuId>,
    opened: bool,
    binding: bool,
    interactive: bool,
    proof: Arc<AtomicBool>,
    #[cfg(target_os = "windows")]
    lease: Option<lintel::desktop::WindowMenuLease>,
  }
  #[derive(Clone, Debug)]
  enum Message {
    Window(WindowEvent),
    Desktop(DesktopEvent),
    Menu(Result<MenuId, String>),
    #[cfg(target_os = "macos")]
    Bound(Result<(), String>),
    #[cfg(target_os = "windows")]
    Bound(Result<lintel::desktop::WindowMenuLease, String>),
    NativeProof(Result<bool, lintel::window::WindowError>),
  }
  impl State {
    fn bind(&mut self) -> Task<Message> {
      let Some(menu) = self.menu else {
        return Task::none();
      };
      if !self.opened || self.binding {
        return Task::none();
      }
      self.binding = true;
      #[cfg(target_os = "windows")]
      return self
        .windows
        .bind_menu(self.id, menu)
        .expect("bind managed window menu")
        .map(Message::Bound);
      #[cfg(target_os = "macos")]
      {
        let desktop = self.desktop.clone();
        Task::perform(
          async move {
            desktop
              .set_application_menu(Some(menu))
              .map_err(|error| error.to_string())?
              .await
              .map_err(|error| error.to_string())
          },
          Message::Bound,
        )
      }
    }
  }
  let interactive = std::env::args_os().any(|arg| arg == "--interactive");
  let proof = Arc::new(AtomicBool::new(false));
  let evidence = proof.clone();
  lintel::multi_window(
    move |windows: WindowManager| {
      let desktop = windows.desktop().expect("desktop service is available in boot");
      let id = windows
        .open("Lintel managed desktop window", Default::default())
        .expect("open managed window");
      let menu_service = desktop.clone();
      let task = Task::perform(
        async move {
          menu_service
            .create_menu(MenuSpec {
              items: vec![MenuEntry::Submenu {
                label: "Lintel".into(),
                enabled: true,
                items: vec![MenuEntry::item("quit", "Close this window")],
              }],
            })
            .map_err(|error| error.to_string())?
            .await
            .map_err(|error| error.to_string())
        },
        Message::Menu,
      );
      (
        State {
          windows,
          desktop,
          id,
          menu: None,
          opened: false,
          binding: false,
          interactive,
          proof: evidence.clone(),
          #[cfg(target_os = "windows")]
          lease: None,
        },
        task,
      )
    },
    |state: &mut State, message| {
      match message {
        Message::Window(WindowEvent::Opened(_)) => {
          state.opened = true;
          return state.bind();
        }
        Message::Window(WindowEvent::Closed(_)) => {
          state.opened = false;
          #[cfg(target_os = "windows")]
          {
            state.lease.take();
          }
        }
        Message::Menu(menu) => {
          state.menu = Some(menu.expect("create menu"));
          return state.bind();
        }
        Message::Bound(binding) => {
          if !state.opened {
            return Task::none();
          }
          #[cfg(target_os = "windows")]
          {
            state.lease = Some(binding.expect("attach scoped window menu"));
          }
          #[cfg(target_os = "macos")]
          binding.expect("attach application menu");
          return state
            .windows
            .with_native_window(state.id, |window| window.window_handle().is_ok())
            .expect("native integration admitted")
            .map(Message::NativeProof);
        }
        Message::NativeProof(proof) => {
          if !state.opened {
            return Task::none();
          }
          assert!(proof.expect("native integration completed"));
          state.proof.store(true, Ordering::SeqCst);
          println!("LINTEL_DESKTOP_WINDOW_SMOKE_OK");
          if !state.interactive {
            state.windows.destroy(state.id).expect("close probe window");
          }
        }
        Message::Desktop(DesktopEvent::Menu { item, .. }) if item == "quit" => {
          println!("LINTEL_MANAGED_MENU_CLICK_OK");
          state.windows.close(state.id).expect("close from native menu");
        }
        _ => {}
      }
      Task::none()
    },
    |_state: &State, _| -> Element<'_, Message> { text("Use the Lintel menu to close this window").into() },
  )
  .id("dev.lintel.examples.desktop-window")
  .desktop()
  .on_window_event(Message::Window)
  .on_desktop_event(Message::Desktop)
  .run()?;
  anyhow::ensure!(
    interactive || proof.load(Ordering::SeqCst),
    "managed desktop service proof did not complete"
  );
  Ok(())
}

#[cfg(not(all(
  feature = "desktop",
  feature = "public-iced",
  any(target_os = "macos", target_os = "windows")
)))]
fn main() -> lintel::Result<()> {
  anyhow::bail!("enable public-iced and desktop on macOS or Windows")
}
