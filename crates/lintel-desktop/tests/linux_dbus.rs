#![cfg(all(
  unix,
  feature = "linux-tray",
  feature = "linux-portal-shortcuts",
  not(all(target_os = "macos", feature = "native-ui"))
))]
use lintel_desktop::{
  DesktopError, DesktopEvent, DesktopRuntime, MenuEntry, MenuSpec, TrayIcon, TraySpec,
  linux::{
    LinuxEvent,
    portal::{PortalOptions, RegistryStatus, ShortcutRequest},
  },
};
use std::{
  collections::HashMap,
  io::{BufRead, BufReader},
  process::{Child, Command, Stdio},
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
  },
  time::Duration,
};
use zbus::{
  Connection, Proxy,
  zvariant::{OwnedObjectPath, OwnedValue, Str, Value},
};
type Dict = HashMap<String, OwnedValue>;
struct Bus(Child);
impl Drop for Bus {
  fn drop(&mut self) {
    let _ = self.0.kill();
    let _ = self.0.wait();
  }
}

/// Re-exec isolates DBUS_SESSION_BUS_ADDRESS without mutating this process's environment.
#[test]
fn private_session_bus_fixture() {
  let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/checklist-platform");
  std::fs::create_dir_all(&base).unwrap();
  let dir = tempfile::Builder::new()
    .prefix("dbus-fixture-")
    .tempdir_in(base)
    .unwrap();
  let socket = std::path::Path::new("./bus.sock");
  let config = dir.path().join("bus.conf");
  std::fs::write(&config,format!("<busconfig><type>session</type><listen>unix:path={}</listen><auth>EXTERNAL</auth><policy context=\"default\"><allow own=\"*\"/><allow send_destination=\"*\"/><allow receive_sender=\"*\"/></policy></busconfig>",socket.display())).unwrap();
  let daemon = if std::path::Path::new("/opt/homebrew/bin/dbus-daemon").is_file() {
    "/opt/homebrew/bin/dbus-daemon"
  } else {
    "dbus-daemon"
  };
  let mut bus = Bus(
    Command::new(daemon)
      .current_dir(dir.path())
      .arg("--nofork")
      .arg("--print-address=1")
      .arg(format!("--config-file={}", config.display()))
      .stdout(Stdio::piped())
      .spawn()
      .expect("install dbus-daemon to run the isolated fixture"),
  );
  let mut address = String::new();
  BufReader::new(bus.0.stdout.take().unwrap())
    .read_line(&mut address)
    .unwrap();
  assert!(!address.is_empty());
  let output = Command::new(std::env::current_exe().unwrap())
    .current_dir(dir.path())
    .args(["--exact", "private_dbus_child", "--ignored", "--nocapture"])
    .env("DBUS_SESSION_BUS_ADDRESS", address.trim())
    .env("LINTEL_PRIVATE_DBUS_FIXTURE", "1")
    .output()
    .unwrap();
  print!("{}", String::from_utf8_lossy(&output.stdout));
  eprint!("{}", String::from_utf8_lossy(&output.stderr));
  assert!(output.status.success(), "private bus fixture failed: {}", output.status);
}

#[derive(Clone, Default)]
struct Watcher {
  items: Arc<Mutex<Vec<String>>>,
  host: Arc<AtomicBool>,
}
#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
  fn register_status_notifier_item(&self, service: String) {
    self.items.lock().unwrap().push(service);
  }
  #[zbus(property)]
  fn is_status_notifier_host_registered(&self) -> bool {
    self.host.load(Ordering::Acquire)
  }
  #[zbus(property)]
  fn registered_status_notifier_items(&self) -> Vec<String> {
    self.items.lock().unwrap().clone()
  }
  #[zbus(property)]
  fn protocol_version(&self) -> i32 {
    0
  }
}
#[derive(Clone, Default)]
struct PortalState {
  mode: Arc<AtomicU8>,
  requests_closed: Arc<AtomicU32>,
  sessions_closed: Arc<AtomicU32>,
  registered: Arc<AtomicU32>,
  binds: Arc<AtomicU32>,
}
struct Request {
  state: PortalState,
}
#[zbus::interface(name = "org.freedesktop.portal.Request")]
impl Request {
  fn close(&self) {
    self.state.requests_closed.fetch_add(1, Ordering::AcqRel);
  }
}
struct Session {
  state: PortalState,
}
#[zbus::interface(name = "org.freedesktop.portal.Session")]
impl Session {
  fn close(&self) {
    self.state.sessions_closed.fetch_add(1, Ordering::AcqRel);
  }
}
struct Registry {
  state: PortalState,
}
#[zbus::interface(name = "org.freedesktop.host.portal.Registry")]
impl Registry {
  fn register(&self, app_id: String, _options: Dict) -> zbus::fdo::Result<()> {
    if app_id != "com.example.LintelFixture" {
      return Err(zbus::fdo::Error::InvalidArgs("wrong app ID".into()));
    }
    self.state.registered.fetch_add(1, Ordering::AcqRel);
    Ok(())
  }
}
struct Portal {
  state: PortalState,
}
fn text(values: &Dict, key: &str) -> String {
  <&str>::try_from(values.get(key).unwrap()).unwrap().into()
}
fn value(text: &str) -> OwnedValue {
  OwnedValue::from(Str::from(text.to_owned()))
}
fn path(prefix: &str, header: &zbus::message::Header<'_>, token: &str) -> OwnedObjectPath {
  OwnedObjectPath::try_from(format!(
    "/org/freedesktop/portal/desktop/{}/{}/{}",
    prefix,
    header
      .sender()
      .unwrap()
      .as_str()
      .trim_start_matches(':')
      .replace('.', "_"),
    token
  ))
  .unwrap()
}
fn binding(id: String) -> (String, Dict) {
  (
    id,
    HashMap::from([
      ("description".into(), value("Show")),
      ("trigger_description".into(), value("Super+Q (user remapped)")),
    ]),
  )
}
async fn respond(connection: &Connection, path: &OwnedObjectPath, code: u32, results: Dict) {
  connection
    .emit_signal(
      None::<&str>,
      path.as_str(),
      "org.freedesktop.portal.Request",
      "Response",
      &(code, results),
    )
    .await
    .unwrap();
}
#[zbus::interface(name = "org.freedesktop.portal.GlobalShortcuts")]
impl Portal {
  #[zbus(property, name = "version")]
  fn version(&self) -> u32 {
    2
  }
  async fn create_session(
    &self,
    options: Dict,
    #[zbus(header)] header: zbus::message::Header<'_>,
    #[zbus(connection)] connection: &Connection,
  ) -> zbus::fdo::Result<OwnedObjectPath> {
    let request = path("request", &header, &text(&options, "handle_token"));
    let session = path("session", &header, &text(&options, "session_handle_token"));
    connection
      .object_server()
      .at(
        request.clone(),
        Request {
          state: self.state.clone(),
        },
      )
      .await
      .unwrap();
    connection
      .object_server()
      .at(
        session.clone(),
        Session {
          state: self.state.clone(),
        },
      )
      .await
      .unwrap();
    // Response deliberately precedes the method return, covering the subscription race.
    respond(
      connection,
      &request,
      0,
      HashMap::from([("session_handle".into(), value(session.as_str()))]),
    )
    .await;
    Ok(request)
  }
  async fn bind_shortcuts(
    &self,
    _session: OwnedObjectPath,
    shortcuts: Vec<(String, Dict)>,
    _parent: String,
    options: Dict,
    #[zbus(header)] header: zbus::message::Header<'_>,
    #[zbus(connection)] connection: &Connection,
  ) -> zbus::fdo::Result<OwnedObjectPath> {
    self.state.binds.fetch_add(1, Ordering::AcqRel);
    let request = path("request", &header, &text(&options, "handle_token"));
    connection
      .object_server()
      .at(
        request.clone(),
        Request {
          state: self.state.clone(),
        },
      )
      .await
      .unwrap();
    match self.state.mode.load(Ordering::Acquire) {
      1 => respond(connection, &request, 1, Dict::new()).await,
      2 => {}
      _ => {
        let bindings = shortcuts
          .into_iter()
          .take(1)
          .map(|(id, _)| binding(id))
          .collect::<Vec<_>>();
        let values = OwnedValue::try_from(Value::new(bindings)).unwrap();
        respond(connection, &request, 0, HashMap::from([("shortcuts".into(), values)])).await;
      }
    }
    Ok(request)
  }
  async fn list_shortcuts(
    &self,
    _session: OwnedObjectPath,
    options: Dict,
    #[zbus(header)] header: zbus::message::Header<'_>,
    #[zbus(connection)] connection: &Connection,
  ) -> zbus::fdo::Result<OwnedObjectPath> {
    let request = path("request", &header, &text(&options, "handle_token"));
    connection
      .object_server()
      .at(
        request.clone(),
        Request {
          state: self.state.clone(),
        },
      )
      .await
      .unwrap();
    respond(
      connection,
      &request,
      0,
      HashMap::from([(
        "shortcuts".into(),
        OwnedValue::try_from(Value::new(vec![binding("show".into())])).unwrap(),
      )]),
    )
    .await;
    Ok(request)
  }
}
fn options() -> PortalOptions {
  let mut options = PortalOptions::new("com.example.LintelFixture");
  options.response_timeout = Duration::from_millis(500);
  options
}
fn shortcuts() -> Vec<ShortcutRequest> {
  ["show", "other"]
    .into_iter()
    .map(|id| ShortcutRequest {
      id: id.into(),
      description: "Show".into(),
      preferred_trigger: Some("CTRL+ALT+l".into()),
    })
    .collect()
}
async fn until(mut predicate: impl FnMut() -> bool) {
  for _ in 0..200 {
    if predicate() {
      return;
    }
    futures_timer::Delay::new(Duration::from_millis(5)).await;
  }
  assert!(predicate(), "fixture deadline expired");
}

#[test]
#[ignore = "only the parent test launches this against its private session bus"]
fn private_dbus_child() {
  assert_eq!(std::env::var("LINTEL_PRIVATE_DBUS_FIXTURE").as_deref(), Ok("1"));
  futures_lite::future::block_on(async {
    let (mut runtime, service) = DesktopRuntime::new(|| {}).unwrap();
    let linux = service.linux();
    let menu = linux
      .create_menu(MenuSpec {
        items: vec![MenuEntry::item("open", "Open")],
      })
      .unwrap()
      .await
      .unwrap();
    let spec = || TraySpec {
      icon: TrayIcon {
        rgba: vec![1, 2, 3, 4],
        width: 1,
        height: 1,
        template: false,
      },
      tooltip: Some("Lintel fixture".into()),
      menu: Some(menu),
    };
    assert!(
      linux.create_tray(spec()).unwrap().await.is_err(),
      "missing watcher must fail"
    );
    let watcher = Watcher::default();
    let server = zbus::connection::Builder::session()
      .unwrap()
      .name("org.kde.StatusNotifierWatcher")
      .unwrap()
      .serve_at("/StatusNotifierWatcher", watcher.clone())
      .unwrap()
      .build()
      .await
      .unwrap();
    assert!(
      linux.create_tray(spec()).unwrap().await.is_err(),
      "watcher without host must fail"
    );
    watcher.host.store(true, Ordering::Release);
    let tray = linux.create_tray(spec()).unwrap().await.unwrap();
    assert!(
      linux.remove_menu(menu).unwrap().await.is_err(),
      "attached menus cannot be destroyed"
    );
    let name = watcher.items.lock().unwrap().last().unwrap().clone();
    let proxy = zbus::proxy::Builder::<Proxy<'_>>::new(&server)
      .destination(name.as_str())
      .unwrap()
      .path("/StatusNotifierItem")
      .unwrap()
      .interface("org.kde.StatusNotifierItem")
      .unwrap()
      .cache_properties(zbus::proxy::CacheProperties::No)
      .build()
      .await
      .unwrap();
    assert_eq!(
      proxy
        .get_property::<Vec<(i32, i32, Vec<u8>)>>("IconPixmap")
        .await
        .unwrap()[0]
        .2,
      vec![4, 1, 2, 3]
    );
    proxy.call_method("Activate", &(10i32, 20i32)).await.unwrap();
    let mut activated = false;
    until(|| {
      activated |= runtime
        .take_events(128)
        .iter()
        .any(|event| matches!(event,DesktopEvent::Linux(LinuxEvent::TrayActivated{tray:id,x:10,y:20,..})if *id==tray));
      activated
    })
    .await;
    let dbus_menu = Proxy::new(&server, name.as_str(), "/MenuBar", "com.canonical.dbusmenu")
      .await
      .unwrap();
    let layout = dbus_menu
      .call_method("GetLayout", &(0i32, -1i32, Vec::<String>::new()))
      .await
      .unwrap();
    let (_, (_, _, children)): (u32, (i32, Dict, Vec<OwnedValue>)) = layout.body().deserialize().unwrap();
    let child = zbus::zvariant::Structure::try_from(children.into_iter().next().unwrap()).unwrap();
    let id = i32::try_from(&child.fields()[0]).unwrap();
    dbus_menu
      .call_method("Event", &(id, "clicked", OwnedValue::from(0i32), 0u32))
      .await
      .unwrap();
    let mut clicked = false;
    until(|| {
      clicked |= runtime
        .take_events(128)
        .iter()
        .any(|event| matches!(event,DesktopEvent::Menu{menu:id,item}if *id==menu&&item=="open"));
      clicked
    })
    .await;
    linux
      .set_menu_item_label(menu, "open".into(), "Renamed".into())
      .unwrap()
      .await
      .unwrap();
    let property = dbus_menu.call_method("GetProperty", &(id, "label")).await.unwrap();
    assert_eq!(
      <&str>::try_from(&property.body().deserialize::<OwnedValue>().unwrap()).unwrap(),
      "Renamed"
    );
    linux.set_tray_visible(tray, false).unwrap().await.unwrap();
    assert_eq!(proxy.get_property::<String>("Status").await.unwrap(), "Passive");
    server.release_name("org.kde.StatusNotifierWatcher").await.unwrap();
    let mut offline = false;
    until(|| {
      offline |= runtime.take_events(128).iter().any(
        |event| matches!(event,DesktopEvent::Linux(LinuxEvent::TrayAvailability{tray:id,online:false,..})if *id==tray),
      );
      offline
    })
    .await;
    server.request_name("org.kde.StatusNotifierWatcher").await.unwrap();
    let mut online = false;
    until(|| {
      online |= runtime.take_events(128).iter().any(
        |event| matches!(event,DesktopEvent::Linux(LinuxEvent::TrayAvailability{tray:id,online:true,..})if *id==tray),
      );
      online
    })
    .await;
    linux.remove_tray(tray).unwrap().await.unwrap();
    assert!(proxy.get_property::<String>("Status").await.is_err());
    linux.remove_menu(menu).unwrap().await.unwrap();
    println!(
      "LINUX_SNI_FIXTURE_OK missing_watcher missing_host registration argb activation dbusmenu update watcher_recovery shutdown"
    );

    let state = PortalState::default();
    let portal = zbus::connection::Builder::session()
      .unwrap()
      .name("org.freedesktop.portal.Desktop")
      .unwrap()
      .serve_at("/org/freedesktop/portal/desktop", Portal { state: state.clone() })
      .unwrap()
      .serve_at("/org/freedesktop/portal/desktop", Registry { state: state.clone() })
      .unwrap()
      .build()
      .await
      .unwrap();
    let session = linux.portal_session(options()).await.unwrap();
    assert_eq!(session.registry_status(), RegistryStatus::Registered);
    let bound = session.bind(shortcuts()).await.unwrap();
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0].id, "show");
    assert_eq!(bound[0].trigger_description, "Super+Q (user remapped)");
    assert!(session.bind(shortcuts()).await.is_err());
    assert_eq!(session.list().await.unwrap(), bound);
    let actual_path = OwnedObjectPath::try_from(session.path()).unwrap();
    portal
      .emit_signal(
        None::<&str>,
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.GlobalShortcuts",
        "Activated",
        &(actual_path.clone(), "other", 1u64, Dict::new()),
      )
      .await
      .unwrap();
    portal
      .emit_signal(
        None::<&str>,
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.GlobalShortcuts",
        "Activated",
        &(
          actual_path.clone(),
          "show",
          2u64,
          HashMap::from([("activation_token".to_owned(), value("fixture-token"))]),
        ),
      )
      .await
      .unwrap();
    let mut received = false;
    until(|| {
      for event in runtime.take_events(128) {
        if let DesktopEvent::Linux(LinuxEvent::PortalActivated {
          id,
          timestamp,
          activation_token,
          ..
        }) = event
        {
          assert_eq!(id, "show");
          assert_eq!(timestamp, 2);
          assert_eq!(activation_token.as_deref(), Some("fixture-token"));
          received = true;
        }
      }
      received
    })
    .await;
    session.close().await.unwrap();
    state.mode.store(1, Ordering::Release);
    let rejected = linux.portal_session(options()).await.unwrap();
    assert!(matches!(
      rejected.bind(shortcuts()).await,
      Err(DesktopError::PortalResponse { response: 1, .. })
    ));
    assert!(rejected.is_closed());
    state.mode.store(2, Ordering::Release);
    let canceled = linux.portal_session(options()).await.unwrap();
    let before = state.requests_closed.load(Ordering::Acquire);
    let binds = state.binds.load(Ordering::Acquire);
    let task = async_global_executor::spawn(async move { canceled.bind(shortcuts()).await });
    until(|| state.binds.load(Ordering::Acquire) > binds).await;
    drop(task);
    until(|| state.requests_closed.load(Ordering::Acquire) > before).await;
    let timed = linux.portal_session(options()).await.unwrap();
    assert!(matches!(timed.bind(shortcuts()).await, Err(DesktopError::Timeout(_))));
    assert!(timed.is_closed());
    state.mode.store(0, Ordering::Release);
    let lost = linux.portal_session(options()).await.unwrap();
    portal.close().await.unwrap();
    until(|| lost.is_closed()).await;
    assert!(state.registered.load(Ordering::Acquire) >= 5);
    assert!(state.sessions_closed.load(Ordering::Acquire) >= 3);
    println!(
      "LINUX_PORTAL_FIXTURE_OK registry early_response subset remap signal_filter user_reject cancellation timeout close provider_loss"
    );
    drop(runtime);
    assert!(matches!(
      linux.create_menu(MenuSpec::default()),
      Err(DesktopError::RuntimeClosed)
    ));
  });
}
