//! Linux host probe. Portal consent is requested only with --portal-app-id ID.
#[cfg(target_os = "linux")]
mod probe {
  use lintel_desktop::{
    DesktopEvent, DesktopRuntime, DesktopService, MenuEntry, MenuSpec, TrayIcon, TraySpec,
    linux::{
      portal::{PortalOptions, PortalSession, ShortcutRequest},
      x11::X11Shortcuts,
    },
  };
  use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy},
    platform::x11::ActiveEventLoopExtX11,
    window::{Window, WindowId},
  };
  enum Event {
    Wake,
    Ready(Result<Resources, String>),
  }
  struct Resources {
    _x11: Option<X11Shortcuts>,
    _portal: Option<PortalSession>,
  }
  struct App {
    runtime: DesktopRuntime,
    service: DesktopService,
    window: Option<Window>,
    resources: Option<Resources>,
    proxy: EventLoopProxy<Event>,
    tray: bool,
    x11: bool,
    portal_id: Option<String>,
    failure: Option<String>,
  }
  impl ApplicationHandler<Event> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
      if self.window.is_some() {
        return;
      }
      self.window = Some(
        event_loop
          .create_window(Window::default_attributes().with_title("Lintel Linux desktop probe"))
          .expect("create probe window"),
      );
      let service = self.service.clone();
      let proxy = self.proxy.clone();
      let tray = self.tray;
      let x11 = self.x11;
      let is_x11 = event_loop.is_x11();
      let portal_id = self.portal_id.clone();
      async_global_executor::spawn(async move {
        let result = async {
          if tray {
            let menu = service
              .create_menu(MenuSpec {
                items: vec![
                  MenuEntry::item("show", "Show event"),
                  MenuEntry::Separator,
                  MenuEntry::item("quit", "Quit"),
                ],
              })
              .map_err(|e| e.to_string())?
              .await
              .map_err(|e| e.to_string())?;
            let tray = service
              .create_tray(TraySpec {
                icon: TrayIcon {
                  rgba: [50, 120, 220, 255].repeat(16 * 16),
                  width: 16,
                  height: 16,
                  template: false,
                },
                tooltip: Some("Lintel Linux probe".into()),
                menu: Some(menu),
              })
              .map_err(|e| e.to_string())?
              .await
              .map_err(|e| e.to_string())?;
            println!("LINUX_TRAY_READY id={}", tray.as_u64());
          }
          let x11 = if x11 {
            if !is_x11 {
              return Err(
                "--x11-shortcut requires an actual X11 winit host; portal binding is explicit on Wayland".into(),
              );
            }
            let manager = service
              .linux()
              .x11_shortcuts(None)
              .map_err(|e| e.to_string())?
              .await
              .map_err(|e| e.to_string())?;
            let shortcut = manager
              .register("Ctrl+Shift+L")
              .map_err(|e| e.to_string())?
              .await
              .map_err(|e| e.to_string())?;
            println!("LINUX_X11_SHORTCUT_READY id={}", shortcut.as_u64());
            Some(manager)
          } else {
            None
          };
          let portal = if let Some(app_id) = portal_id {
            let session = service
              .linux()
              .portal_session(PortalOptions::new(app_id))
              .await
              .map_err(|e| e.to_string())?;
            // 此分支仅由显式CLI参数启用；正常启动和托盘操作不会申请portal授权。
            let bound = session
              .bind(vec![ShortcutRequest {
                id: "show".into(),
                description: "Show Lintel probe event".into(),
                preferred_trigger: Some("CTRL+SHIFT+l".into()),
              }])
              .await
              .map_err(|e| e.to_string())?;
            for shortcut in bound {
              println!(
                "LINUX_PORTAL_BOUND id={} trigger={}",
                shortcut.id, shortcut.trigger_description
              );
            }
            Some(session)
          } else {
            None
          };
          Ok(Resources {
            _x11: x11,
            _portal: portal,
          })
        }
        .await;
        let _ = proxy.send_event(Event::Ready(result));
      })
      .detach();
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
      match event {
        Event::Wake => self.drain(event_loop),
        Event::Ready(Ok(resources)) => {
          self.resources = Some(resources);
          println!("LINUX_DESKTOP_PROBE_READY");
        }
        Event::Ready(Err(error)) => {
          eprintln!("LINUX_DESKTOP_PROBE_ERROR {error}");
          self.failure = Some(error);
          event_loop.exit();
        }
      }
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
      self.drain(event_loop);
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
      if matches!(event, WindowEvent::CloseRequested) {
        event_loop.exit();
      }
    }
  }
  impl App {
    fn drain(&mut self, event_loop: &ActiveEventLoop) {
      self.runtime.drain(64);
      for event in self.runtime.take_events(64) {
        println!("LINUX_DESKTOP_EVENT {event:?}");
        if matches!(event,DesktopEvent::Menu{ref item,..}if item=="quit") {
          event_loop.exit();
        }
      }
      let lost = self.runtime.take_event_overflow_count();
      if lost != 0 {
        eprintln!("LINUX_DESKTOP_EVENT_OVERFLOW count={lost}");
      }
    }
  }
  pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut tray = false;
    let mut x11 = false;
    let mut portal_id = None;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
      match argument.as_str() {
        "--tray" => tray = true,
        "--x11-shortcut" => x11 = true,
        "--portal-app-id" => {
          portal_id = Some(
            args
              .next()
              .ok_or("--portal-app-id requires an installed desktop app ID")?,
          )
        }
        _ => return Err(format!("unknown option {argument}").into()),
      }
    }
    let event_loop = EventLoop::<Event>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let wake = proxy.clone();
    let (runtime, service) = DesktopRuntime::new(move || {
      let _ = wake.send_event(Event::Wake);
    })?;
    let mut app = App {
      runtime,
      service,
      window: None,
      resources: None,
      proxy,
      tray,
      x11,
      portal_id,
      failure: None,
    };
    event_loop.run_app(&mut app)?;
    if let Some(error) = app.failure.take() {
      return Err(error.into());
    }
    Ok(())
  }
}
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
  probe::run()
}
#[cfg(not(target_os = "linux"))]
fn main() {
  eprintln!("linux_desktop requires a Linux X11/Wayland host; private protocol fixtures are portable Unix tests");
}
