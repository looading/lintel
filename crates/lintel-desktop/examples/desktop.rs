//! Opt-in native UI demo; actions that affect the user's desktop require explicit menu selection.
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod desktop {
  use lintel_desktop::{
    DesktopEvent, DesktopRuntime, DesktopService, FileDialogKind, FileDialogOptions, MenuEntry, MenuSpec, TrayIcon,
    TraySpec,
    accessibility::{AccessibilityEvent, AccessibilityWindow, AccessibilityWindowEvent, accesskit},
    native_ui::{global_hotkey, muda, tray_icon},
  };
  use std::sync::Arc;
  use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    window::{Window, WindowId},
  };

  enum Event {
    Wake,
    Menu(muda::MenuEvent),
    Tray(tray_icon::TrayIconEvent),
    Shortcut(global_hotkey::GlobalHotKeyEvent),
    Accessibility(AccessibilityEvent),
  }
  impl From<AccessibilityEvent> for Event {
    fn from(event: AccessibilityEvent) -> Self {
      Self::Accessibility(event)
    }
  }

  struct App {
    runtime: Option<DesktopRuntime>,
    service: DesktopService,
    window: Option<Arc<Window>>,
    #[cfg(all(feature = "windows-menu", target_os = "windows"))]
    window_menu_lease: Option<lintel_desktop::WindowMenuLease>,
    smoke: bool,
    failure: Option<String>,
    accessibility: Option<AccessibilityWindow>,
    proxy: EventLoopProxy<Event>,
  }

  impl App {
    fn setup(&mut self, event_loop: &ActiveEventLoop) -> Result<(), Box<dyn std::error::Error>> {
      self.window = Some(Arc::new(
        event_loop.create_window(
          Window::default_attributes()
            .with_title("Lintel desktop integration demo")
            .with_visible(false),
        )?,
      ));
      self.accessibility = Some(AccessibilityWindow::new(
        event_loop,
        self.window.as_ref().expect("window created"),
        self.proxy.clone(),
      )?);
      let menu = self.service.create_menu(MenuSpec {
        items: vec![MenuEntry::Submenu {
          label: "Lintel".into(),
          enabled: true,
          items: vec![
            MenuEntry::item("clipboard", "Read clipboard text length"),
            MenuEntry::Item {
              id: "file".into(),
              label: "Choose a file…".into(),
              enabled: true,
              accelerator: Some("CmdOrCtrl+KeyO".into()),
            },
            MenuEntry::item("url", "Open Rust documentation"),
            MenuEntry::Separator,
            MenuEntry::item("quit", "Quit"),
          ],
        }],
      })?;
      self.runtime.as_mut().expect("runtime initialized").drain(64);
      let menu = menu.wait()?; // Already completed; never blocks the dispatch thread.
      #[cfg(target_os = "macos")]
      {
        let installed = self.service.set_application_menu(Some(menu))?;
        self.runtime.as_mut().expect("runtime initialized").drain(64);
        installed.wait()?;
      }
      #[cfg(all(feature = "windows-menu", target_os = "windows"))]
      {
        let lease = self
          .runtime
          .as_mut()
          .expect("runtime initialized")
          .register_windows_window_scoped(self.window.as_ref().expect("window created").clone())?;
        let installed = self.service.set_window_menu(lease.target(), Some(menu))?;
        self.runtime.as_mut().expect("runtime initialized").drain(64);
        installed.wait()?;
        println!("WINDOW_MENU_ATTACHED target={}", lease.target().as_u64());
        self.window_menu_lease = Some(lease);
      }
      if !self.smoke {
        self.window.as_ref().expect("window created").set_visible(true);
      }
      let tray = self.service.create_tray(TraySpec {
        icon: TrayIcon {
          rgba: [50, 120, 220, 255].repeat(16 * 16),
          width: 16,
          height: 16,
          template: false,
        },
        tooltip: Some("Lintel desktop integration demo".into()),
        menu: Some(menu),
      })?;
      self.runtime.as_mut().expect("runtime initialized").drain(64);
      tray.wait()?;
      let shortcut = self.service.register_shortcut("CmdOrCtrl+Shift+KeyL".into())?;
      self.runtime.as_mut().expect("runtime initialized").drain(64);
      match shortcut.wait() {
        Ok(target) => {
          let native: global_hotkey::hotkey::HotKey = "CmdOrCtrl+Shift+KeyL".parse()?;
          println!(
            "SHORTCUT_REGISTERED target={} native={} modifiers={:?} key={:?}",
            target.as_u64(),
            native.id(),
            native.mods,
            native.key
          );
          println!("Cmd/Ctrl+Shift+L focuses the demo window");
        }
        Err(error) if self.smoke => return Err(error.into()),
        Err(error) => eprintln!("Global shortcut could not be registered: {error}"),
      }
      if self.smoke {
        let update = self
          .service
          .set_menu_item_label(menu, "clipboard".into(), "Clipboard probe".into())?;
        self.runtime.as_mut().expect("runtime initialized").drain(64);
        update.wait()?;
        let read = self.service.read_clipboard_text()?;
        self.runtime.as_mut().expect("runtime initialized").drain(64);
        match read.wait() {
          Ok(_) => println!("Native clipboard read succeeded (content discarded)"),
          Err(error) => println!("Native clipboard read returned: {error}"),
        }
        println!(
          "Native accessibility adapter attached; permission status: {:?}",
          lintel_desktop::permissions::accessibility_status()?
        );
        println!("Native menu, tray, shortcut, and main-thread command dispatch smoke completed");
        event_loop.exit();
      }
      Ok(())
    }

    fn action(&mut self, event_loop: &ActiveEventLoop, event: DesktopEvent) -> lintel_desktop::Result<()> {
      match &event {
        #[cfg(all(
          unix,
          any(
            feature = "linux-tray",
            feature = "linux-x11-shortcuts",
            feature = "linux-portal-shortcuts"
          )
        ))]
        DesktopEvent::Linux(event) => println!("DESKTOP_EVENT Linux {event:?}"),
        DesktopEvent::Menu { menu, item } => println!("DESKTOP_EVENT Menu menu={} item={item}", menu.as_u64()),
        DesktopEvent::Shortcut { shortcut, pressed } => {
          println!("DESKTOP_EVENT Shortcut id={} pressed={pressed}", shortcut.as_u64())
        }
        DesktopEvent::TrayClick { tray, button, pressed } => println!(
          "DESKTOP_EVENT TrayClick id={} button={button:?} pressed={pressed}",
          tray.as_u64()
        ),
        DesktopEvent::TrayDoubleClick { tray, button } => {
          println!("DESKTOP_EVENT TrayDoubleClick id={} button={button:?}", tray.as_u64())
        }
      }
      match event {
        DesktopEvent::Menu { item, .. } => match item.as_str() {
          "quit" => event_loop.exit(),
          "clipboard" => {
            let pending = self.service.read_clipboard_text()?;
            self.runtime.as_mut().expect("runtime initialized").drain(64);
            println!("Clipboard contains {} text bytes", pending.wait()?.len());
          }
          "file" => {
            let pending = self.service.show_file_dialog(
              FileDialogKind::OpenFile,
              FileDialogOptions {
                title: Some("Choose a file for the Lintel demo".into()),
                ..Default::default()
              },
            )?;
            self.runtime.as_mut().expect("runtime initialized").drain(64);
            match pending.wait()? {
              None => println!("FILE_DIALOG_RESULT None"),
              Some(_) => println!("FILE_DIALOG_RESULT Selected"),
            }
          }
          "url" => lintel_desktop::opener::open_url("https://doc.rust-lang.org/")?,
          _ => {}
        },
        DesktopEvent::Shortcut { pressed: true, .. } => {
          if let Some(window) = &self.window {
            window.set_visible(true);
            window.focus_window();
          }
        }
        _ => {}
      }
      Ok(())
    }
  }

  impl ApplicationHandler<Event> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
      if self.window.is_none() {
        if let Err(error) = self.setup(event_loop) {
          eprintln!("Desktop setup failed: {error}");
          self.failure = Some(error.to_string());
          event_loop.exit();
        }
      }
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
      let Some(runtime) = &mut self.runtime else {
        return;
      };
      let event = match event {
        Event::Wake => {
          runtime.drain(64);
          None
        }
        Event::Menu(event) => runtime.menu_event(&event),
        Event::Tray(event) => runtime.tray_event(&event),
        Event::Shortcut(event) => {
          println!("DESKTOP_NATIVE_SHORTCUT id={} state={:?}", event.id, event.state);
          let logical = runtime.shortcut_event(&event);
          if logical.is_none() {
            println!("DESKTOP_SHORTCUT_UNMAPPED id={}", event.id);
          }
          logical
        }
        Event::Accessibility(event) => {
          if event.window_event == AccessibilityWindowEvent::InitialTreeRequested {
            if let Some(adapter) = &mut self.accessibility {
              adapter.update_if_active(|| {
                let root_id = accesskit::NodeId(0);
                let mut root = accesskit::Node::new(accesskit::Role::Window);
                root.set_label("Lintel desktop integration demo");
                accesskit::TreeUpdate {
                  nodes: vec![(root_id, root)],
                  tree: Some(accesskit::TreeInfo::new(root_id)),
                  tree_id: accesskit::TreeId::ROOT,
                  focus: root_id,
                }
              });
            }
          }
          None
        }
      };
      if let Some(event) = event {
        if let Err(error) = self.action(event_loop, event) {
          eprintln!("Desktop operation failed: {error}");
        }
      }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
      if let (Some(adapter), Some(window)) = (&mut self.accessibility, &self.window) {
        if let Err(error) = adapter.process_event(window, &event) {
          self.failure = Some(error.to_string());
          event_loop.exit();
          return;
        }
      }
      if matches!(event, WindowEvent::CloseRequested) {
        #[cfg(all(feature = "windows-menu", target_os = "windows"))]
        {
          self.window_menu_lease.take();
          if let Some(runtime) = &mut self.runtime {
            runtime.drain(0);
          }
        }
        event_loop.exit();
      }
    }
    fn exiting(&mut self, _: &ActiveEventLoop) {
      #[cfg(all(feature = "windows-menu", target_os = "windows"))]
      {
        self.window_menu_lease.take();
        if let Some(runtime) = &mut self.runtime {
          runtime.drain(0);
        }
      }
      self.accessibility.take();
      self.runtime.take();
    }
  }

  pub(super) fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = EventLoop::<Event>::with_user_event();
    #[cfg(all(feature = "windows-menu", target_os = "windows"))]
    let windows_hook = lintel_desktop::windows_menu::WindowsMenuMessageHook::default();
    #[cfg(all(feature = "windows-menu", target_os = "windows"))]
    windows_hook.install(&mut builder);
    let event_loop = builder.build()?;
    let proxy = event_loop.create_proxy();
    let (runtime, service) = DesktopRuntime::new(move || {
      let _ = proxy.send_event(Event::Wake);
    })?;
    #[cfg(all(feature = "windows-menu", target_os = "windows"))]
    windows_hook.bind(runtime.windows_menu_dispatcher());
    let proxy = event_loop.create_proxy();
    muda::MenuEvent::set_event_handler(Some(move |event| {
      let _ = proxy.send_event(Event::Menu(event));
    }));
    let proxy = event_loop.create_proxy();
    tray_icon::TrayIconEvent::set_event_handler(Some(move |event| {
      let _ = proxy.send_event(Event::Tray(event));
    }));
    let proxy = event_loop.create_proxy();
    global_hotkey::GlobalHotKeyEvent::set_event_handler(Some(move |event| {
      let _ = proxy.send_event(Event::Shortcut(event));
    }));
    let mut app = App {
      runtime: Some(runtime),
      service,
      window: None,
      #[cfg(all(feature = "windows-menu", target_os = "windows"))]
      window_menu_lease: None,
      smoke: std::env::args().any(|argument| argument == "--smoke"),
      failure: None,
      accessibility: None,
      proxy: event_loop.create_proxy(),
    };
    event_loop.run_app(&mut app)?;
    if let Some(failure) = app.failure {
      return Err(failure.into());
    }
    Ok(())
  }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  #[cfg(any(target_os = "macos", target_os = "windows"))]
  {
    desktop::run()
  }
  #[cfg(not(any(target_os = "macos", target_os = "windows")))]
  {
    Err("the native UI demo currently supports macOS and Windows".into())
  }
}
