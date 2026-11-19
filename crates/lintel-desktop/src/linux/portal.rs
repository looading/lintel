//! GlobalShortcuts portal: explicit user-mediated binding, scoped sessions and cancellation.
pub use super::portal_types::BoundShortcut;
use super::{CloseSignal, EventSink, LinuxEvent, deadline, next_id};
use crate::{DesktopError, DesktopEvent, Result};
use futures_lite::StreamExt;
use std::{
  collections::{HashMap, HashSet},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  time::Duration,
};
use zbus::{
  Connection, Proxy,
  zvariant::{OwnedObjectPath, OwnedValue, Str, Type},
};

type Dict = HashMap<String, OwnedValue>;
const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";

#[derive(Debug, Clone)]
pub struct PortalOptions {
  /// Must identify an installed `<app_id>.desktop` for unsandboxed host applications.
  pub app_id: String,
  /// Set only when the portal already obtains identity from the application's sandbox.
  pub sandboxed: bool,
  /// Portal window identifier (e.g. x11:... or exported wayland:...), or empty for unparented UI.
  pub parent_window: String,
  /// Time allowed for explicit user interaction, after which Request.Close is sent.
  pub response_timeout: Duration,
}
impl PortalOptions {
  pub fn new(app_id: impl Into<String>) -> Self {
    Self {
      app_id: app_id.into(),
      sandboxed: false,
      parent_window: String::new(),
      response_timeout: Duration::from_secs(300),
    }
  }
  fn validate(&self) -> Result<()> {
    crate::model::validate_text("portal app ID", &self.app_id, 255)?;
    if !self.app_id.contains('.')
      || !self
        .app_id
        .split('.')
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    {
      return Err(crate::error::invalid(
        "portal app ID",
        "expected a reverse-DNS desktop application ID",
      ));
    }
    crate::model::validate_text("portal parent window", &self.parent_window, 4096)?;
    if self.response_timeout.is_zero() || self.response_timeout > Duration::from_secs(3600) {
      return Err(crate::error::invalid("portal timeout", "expected 1ns to one hour"));
    }
    Ok(())
  }
}
#[derive(Debug, Clone)]
pub struct ShortcutRequest {
  pub id: String,
  pub description: String,
  pub preferred_trigger: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryStatus {
  Registered,
  Unavailable,
  SandboxIdentity,
}

struct Owner {
  connection: Connection,
  path: OwnedObjectPath,
  close: Arc<CloseSignal>,
  binding: AtomicBool,
  options: PortalOptions,
  registry: RegistryStatus,
  bindings: Arc<std::sync::RwLock<HashSet<String>>>,
}
/// A scoped portal session. Drop or close terminates all of its bindings and any pending consent.
/// A portal can return a subset, including no bindings; inspect `bind`'s returned values.
pub struct PortalSession {
  owner: Arc<Owner>,
  completion: Option<futures_channel::oneshot::Receiver<Result<()>>>,
}
impl Drop for PortalSession {
  fn drop(&mut self) {
    self.owner.close.close();
  }
}
impl PortalSession {
  pub(crate) async fn connect(options: PortalOptions, events: EventSink) -> Result<Self> {
    options.validate()?;
    let connection = deadline(async {
      Connection::session()
        .await
        .map_err(|e| crate::error::native("connect desktop portal", e))
    })
    .await?;
    Self::connect_on(connection, options, events).await
  }
  async fn connect_on(connection: Connection, options: PortalOptions, events: EventSink) -> Result<Self> {
    let registry = if options.sandboxed {
      RegistryStatus::SandboxIdentity
    } else {
      let registry = Proxy::new(&connection, DEST, PATH, "org.freedesktop.host.portal.Registry")
        .await
        .map_err(|e| crate::error::native("create portal registry proxy", e))?;
      match deadline(async {
        registry
          .call_method("Register", &(options.app_id.clone(), Dict::new()))
          .await
          .map(|_| ())
          .map_err(|e| crate::error::native("register portal app identity", e))
      })
      .await
      {
        Ok(()) => RegistryStatus::Registered,
        Err(DesktopError::Native { message, .. })
          if message.contains("UnknownMethod") || message.contains("UnknownInterface") =>
        {
          RegistryStatus::Unavailable
        }
        Err(error) => return Err(error),
      }
    };
    let portal = Proxy::new(&connection, DEST, PATH, IFACE)
      .await
      .map_err(|e| crate::error::native("create shortcuts portal proxy", e))?;
    let version: u32 = deadline(async {
      portal
        .get_property("version")
        .await
        .map_err(|e| crate::error::native("query shortcuts portal version", e))
    })
    .await?;
    if version < 1 {
      return Err(crate::error::unsupported(
        "portal shortcuts",
        "GlobalShortcuts interface version 1 is required",
      ));
    }
    // 先订阅所有信号；CreateSession或BindShortcuts返回前的信号也不能丢失。
    let mut activated = portal
      .receive_signal("Activated")
      .await
      .map_err(|e| crate::error::native("subscribe portal activation", e))?;
    let mut deactivated = portal
      .receive_signal("Deactivated")
      .await
      .map_err(|e| crate::error::native("subscribe portal deactivation", e))?;
    let mut changed = portal
      .receive_signal("ShortcutsChanged")
      .await
      .map_err(|e| crate::error::native("subscribe portal changes", e))?;
    let mut owner_changed = portal
      .receive_owner_changed()
      .await
      .map_err(|e| crate::error::native("watch portal owner", e))?;
    let session_token = format!("lintel_session_{}", next_id());
    let expected_session = owned_path(&format!(
      "/org/freedesktop/portal/desktop/session/{}/{}",
      peer_path(&connection)?,
      session_token
    ))?;
    // 创建被取消时仍清理预测session路径；request任务独立持有连接直到Close完成。
    let close_guard = SessionCleanup {
      connection: connection.clone(),
      path: expected_session.clone(),
      armed: true,
    };
    let token = format!("lintel_create_{}", next_id());
    let mut opts = Dict::new();
    put(&mut opts, "handle_token", &token);
    put(&mut opts, "session_handle_token", &session_token);
    let results = request(
      connection.clone(),
      "CreateSession",
      opts,
      token,
      options.response_timeout,
      Some(expected_session.clone()),
    )
    .await?;
    let path = results
      .get("session_handle")
      .and_then(|value| <&str>::try_from(value).ok())
      .ok_or_else(|| crate::error::invalid("portal response", "missing string session_handle"))?;
    let path = owned_path(path)?;
    if path != expected_session {
      return Err(crate::error::invalid(
        "portal session",
        "provider did not honor session_handle_token",
      ));
    }
    let close = Arc::new(CloseSignal::new());
    let session = Proxy::new(&connection, DEST, path.clone(), "org.freedesktop.portal.Session")
      .await
      .map_err(|e| crate::error::native("create portal session proxy", e))?;
    let mut closed = session
      .receive_signal("Closed")
      .await
      .map_err(|e| crate::error::native("subscribe portal close", e))?;
    let bindings = Arc::new(std::sync::RwLock::new(HashSet::new()));
    let owner = Arc::new(Owner {
      connection: connection.clone(),
      path: path.clone(),
      close: close.clone(),
      binding: AtomicBool::new(false),
      options,
      registry,
      bindings: bindings.clone(),
    });
    let mut cleanup = close_guard;
    cleanup.armed = false;
    let (completion_tx, completion_rx) = futures_channel::oneshot::channel();
    async_global_executor::spawn(async move {
      enum Signal {
        Activation(Option<zbus::Message>, bool),
        Changed(Option<zbus::Message>),
        Closed,
        Provider,
        Close,
      }
      loop {
        let signal = futures_lite::future::race(
          async {
            close.wait().await;
            Signal::Close
          },
          futures_lite::future::race(
            async { Signal::Activation(activated.next().await, true) },
            futures_lite::future::race(
              async { Signal::Activation(deactivated.next().await, false) },
              futures_lite::future::race(
                async { Signal::Changed(changed.next().await) },
                futures_lite::future::race(
                  async {
                    closed.next().await;
                    Signal::Closed
                  },
                  async {
                    owner_changed.next().await;
                    Signal::Provider
                  },
                ),
              ),
            ),
          ),
        )
        .await;
        match signal {
          Signal::Activation(Some(message), pressed) => {
            if let Ok((signal_path, id, timestamp, options)) =
              message.body().deserialize::<(OwnedObjectPath, String, u64, Dict)>()
            {
              if signal_path == path && bindings.read().unwrap_or_else(|e| e.into_inner()).contains(&id) {
                (events)(DesktopEvent::Linux(LinuxEvent::PortalActivated {
                  session: path.to_string(),
                  id,
                  pressed,
                  timestamp,
                  activation_token: options
                    .get("activation_token")
                    .and_then(|v| <&str>::try_from(v).ok())
                    .map(str::to_owned),
                }));
              }
            }
          }
          Signal::Changed(Some(message)) => {
            if let Ok((signal_path, changed_bindings)) =
              message.body().deserialize::<(OwnedObjectPath, Vec<(String, Dict)>)>()
            {
              if signal_path == path {
                match decode_bindings(changed_bindings) {
                  Ok(shortcuts) => {
                    *bindings.write().unwrap_or_else(|e| e.into_inner()) =
                      shortcuts.iter().map(|s| s.id.clone()).collect();
                    (events)(DesktopEvent::Linux(LinuxEvent::PortalBindingsChanged {
                      session: path.to_string(),
                      shortcuts,
                    }))
                  }
                  Err(e) => (events)(DesktopEvent::Linux(LinuxEvent::BackendStopped {
                    backend: "portal-shortcuts",
                    reason: e.to_string(),
                  })),
                }
              }
            }
          }
          Signal::Closed | Signal::Provider | Signal::Activation(None, _) | Signal::Changed(None) => {
            close.close();
            (events)(DesktopEvent::Linux(LinuxEvent::BackendStopped {
              backend: "portal-shortcuts",
              reason: "portal session or provider closed; explicitly create a new session to reconnect".into(),
            }));
            break;
          }
          Signal::Close => break,
        }
      }
      let result = shutdown_session(&connection, &path).await;
      let _ = completion_tx.send(result);
    })
    .detach();
    Ok(Self {
      owner,
      completion: Some(completion_rx),
    })
  }
  pub fn path(&self) -> &str {
    self.owner.path.as_str()
  }
  pub fn registry_status(&self) -> RegistryStatus {
    self.owner.registry
  }
  pub(crate) fn close_signal(&self) -> &Arc<CloseSignal> {
    &self.owner.close
  }
  pub fn is_closed(&self) -> bool {
    self.owner.close.is_closed()
  }
  /// Consume the session and await its one cleanup operation. Dropping this future still cleans up.
  pub async fn close(mut self) -> Result<()> {
    self.owner.close.close();
    self
      .completion
      .take()
      .ok_or(DesktopError::RuntimeClosed)?
      .await
      .unwrap_or(Err(DesktopError::RuntimeClosed))
  }

  /// Explicit consent operation. Only one bind attempt is permitted per portal session.
  /// Dropping/canceling this future sends Request.Close; dropping the session also closes it.
  pub async fn bind(&self, shortcuts: Vec<ShortcutRequest>) -> Result<Vec<BoundShortcut>> {
    validate_shortcuts(&shortcuts)?;
    if self.is_closed() {
      return Err(DesktopError::RuntimeClosed);
    }
    if self.owner.binding.swap(true, Ordering::AcqRel) {
      return Err(crate::error::invalid(
        "portal binding",
        "each session permits only one bind attempt",
      ));
    }
    let mut binding_guard = BindCleanup {
      close: self.owner.close.clone(),
      armed: true,
    };
    let token = format!("lintel_bind_{}", next_id());
    let mut opts = Dict::new();
    put(&mut opts, "handle_token", &token);
    let requested = shortcuts.iter().map(|s| s.id.clone()).collect::<HashSet<_>>();
    *self.owner.bindings.write().unwrap_or_else(|e| e.into_inner()) = requested.clone();
    let shortcuts = shortcuts
      .into_iter()
      .map(|shortcut| {
        let mut values = Dict::new();
        put(&mut values, "description", &shortcut.description);
        if let Some(trigger) = shortcut.preferred_trigger {
          put(&mut values, "preferred_trigger", &trigger);
        }
        (shortcut.id, values)
      })
      .collect::<Vec<_>>();
    let results = futures_lite::future::race(
      request(
        self.owner.connection.clone(),
        "BindShortcuts",
        (
          self.owner.path.clone(),
          shortcuts,
          self.owner.options.parent_window.clone(),
          opts,
        ),
        token,
        self.owner.options.response_timeout,
        None,
      ),
      async {
        self.owner.close.wait().await;
        Err(DesktopError::RuntimeClosed)
      },
    )
    .await?;
    let values = results
      .into_iter()
      .find(|(key, _)| key == "shortcuts")
      .map(|(_, value)| value)
      .ok_or_else(|| crate::error::invalid("portal response", "missing shortcuts array"))?;
    let bound = decode_bindings(
      Vec::<(String, Dict)>::try_from(values).map_err(|e| crate::error::native("decode portal bindings", e))?,
    )?;
    if bound.iter().any(|s| !requested.contains(&s.id)) {
      return Err(crate::error::invalid(
        "portal response",
        "provider returned an unrequested shortcut ID",
      ));
    }
    *self.owner.bindings.write().unwrap_or_else(|e| e.into_inner()) = bound.iter().map(|s| s.id.clone()).collect();
    binding_guard.armed = false;
    Ok(bound)
  }
  pub async fn list(&self) -> Result<Vec<BoundShortcut>> {
    if self.is_closed() {
      return Err(DesktopError::RuntimeClosed);
    }
    let token = format!("lintel_list_{}", next_id());
    let mut opts = Dict::new();
    put(&mut opts, "handle_token", &token);
    let result = request(
      self.owner.connection.clone(),
      "ListShortcuts",
      (self.owner.path.clone(), opts),
      token,
      self.owner.options.response_timeout,
      None,
    )
    .await?;
    let values = result
      .into_iter()
      .find(|(key, _)| key == "shortcuts")
      .map(|(_, value)| value)
      .ok_or_else(|| crate::error::invalid("portal response", "missing shortcuts array"))?;
    decode_bindings(
      Vec::<(String, Dict)>::try_from(values).map_err(|e| crate::error::native("decode portal bindings", e))?,
    )
  }
}

fn validate_shortcuts(shortcuts: &[ShortcutRequest]) -> Result<()> {
  if shortcuts.is_empty() || shortcuts.len() > 128 {
    return Err(crate::error::invalid("portal shortcuts", "expected 1 to 128 shortcuts"));
  }
  let mut ids = HashSet::new();
  for shortcut in shortcuts {
    crate::model::validate_text("portal shortcut ID", &shortcut.id, 256)?;
    if shortcut.id.is_empty() || !ids.insert(&shortcut.id) {
      return Err(crate::error::invalid("portal shortcut ID", "empty or duplicated ID"));
    }
    crate::model::validate_text("portal shortcut description", &shortcut.description, 4096)?;
    if let Some(trigger) = &shortcut.preferred_trigger {
      crate::model::validate_text("preferred portal trigger", trigger, 256)?;
    }
  }
  Ok(())
}
fn decode_bindings(values: Vec<(String, Dict)>) -> Result<Vec<BoundShortcut>> {
  let mut ids = HashSet::new();
  values
    .into_iter()
    .map(|(id, values)| {
      if !ids.insert(id.clone()) {
        return Err(crate::error::invalid("portal response", "duplicate binding ID"));
      }
      let text = |key| {
        values
          .get(key)
          .and_then(|v| <&str>::try_from(v).ok())
          .map(str::to_owned)
          .ok_or_else(|| crate::error::invalid("portal response", format!("missing {key}")))
      };
      Ok(BoundShortcut {
        id,
        description: text("description")?,
        trigger_description: text("trigger_description")?,
      })
    })
    .collect()
}
fn put(values: &mut Dict, key: &str, value: &str) {
  values.insert(key.into(), OwnedValue::from(Str::from(value.to_owned())));
}
fn peer_path(connection: &Connection) -> Result<String> {
  Ok(
    connection
      .unique_name()
      .ok_or(DesktopError::RuntimeClosed)?
      .as_str()
      .trim_start_matches(':')
      .replace('.', "_"),
  )
}
fn owned_path(path: &str) -> Result<OwnedObjectPath> {
  OwnedObjectPath::try_from(path.to_owned()).map_err(|e| crate::error::native("parse portal object path", e))
}
async fn close_object(connection: &Connection, path: &OwnedObjectPath, interface: &str) -> Result<()> {
  deadline(async {
    connection
      .call_method(Some(DEST), path.as_str(), Some(interface), "Close", &())
      .await
      .map(|_| ())
      .map_err(|e| crate::error::native("close portal object", e))
  })
  .await
}
async fn shutdown_session(connection: &Connection, path: &OwnedObjectPath) -> Result<()> {
  let result = close_object(connection, path, "org.freedesktop.portal.Session").await;
  // 每个session独占一条连接；即使provider拒绝Close，断开peer也会撤销该peer的session。
  let disconnected = deadline(async {
    connection
      .clone()
      .close()
      .await
      .map_err(|e| crate::error::native("disconnect portal session", e))
  })
  .await;
  result.and(disconnected)
}

struct SessionCleanup {
  connection: Connection,
  path: OwnedObjectPath,
  armed: bool,
}
impl Drop for SessionCleanup {
  fn drop(&mut self) {
    if self.armed {
      let connection = self.connection.clone();
      let path = self.path.clone();
      async_global_executor::spawn(async move {
        let _ = shutdown_session(&connection, &path).await;
      })
      .detach();
    }
  }
}
struct BindCleanup {
  close: Arc<CloseSignal>,
  armed: bool,
}
impl Drop for BindCleanup {
  fn drop(&mut self) {
    if self.armed {
      self.close.close();
    }
  }
}
struct CancelRequest(Option<futures_channel::oneshot::Sender<()>>);
impl Drop for CancelRequest {
  fn drop(&mut self) {
    if let Some(sender) = self.0.take() {
      let _ = sender.send(());
    }
  }
}

/// Independent request task retains the reply even if its caller disappears. It subscribes before
/// invoking the method, then closes the actual request after cancellation or timeout.
async fn request<B>(
  connection: Connection,
  method: &'static str,
  body: B,
  token: String,
  timeout: Duration,
  cleanup_session: Option<OwnedObjectPath>,
) -> Result<Dict>
where
  B: serde::Serialize + Type + Send + Sync + 'static,
{
  let path = owned_path(&format!(
    "/org/freedesktop/portal/desktop/request/{}/{}",
    peer_path(&connection)?,
    token
  ))?;
  let (cancel_tx, cancel_rx) = futures_channel::oneshot::channel();
  let (reply_tx, reply_rx) = futures_channel::oneshot::channel();
  let mut guard = CancelRequest(Some(cancel_tx));
  async_global_executor::spawn(async move {
    let result = async {
      let proxy = Proxy::new(&connection, DEST, path.clone(), "org.freedesktop.portal.Request")
        .await
        .map_err(|e| crate::error::native("create portal request proxy", e))?;
      let mut responses = proxy
        .receive_signal("Response")
        .await
        .map_err(|e| crate::error::native("subscribe portal response", e))?;
      let reply = deadline(async {
        connection
          .call_method(Some(DEST), PATH, Some(IFACE), method, &body)
          .await
          .map_err(|e| crate::error::native(method, e))
      })
      .await?;
      let actual: OwnedObjectPath = reply
        .body()
        .deserialize()
        .map_err(|e| crate::error::native("decode portal request handle", e))?;
      if actual != path {
        let _ = close_object(&connection, &actual, "org.freedesktop.portal.Request").await;
        return Err(crate::error::invalid(
          "portal request",
          "provider did not honor handle_token",
        ));
      }
      futures_lite::future::race(
        async {
          let response = responses.next().await.ok_or(DesktopError::RuntimeClosed)?;
          let (code, results): (u32, Dict) = response
            .body()
            .deserialize()
            .map_err(|e| crate::error::native("decode portal response", e))?;
          if code != 0 {
            return Err(DesktopError::PortalResponse {
              operation: method,
              response: code,
            });
          }
          Ok(results)
        },
        futures_lite::future::race(
          async {
            let _ = cancel_rx.await;
            Err(DesktopError::RuntimeClosed)
          },
          async {
            futures_timer::Delay::new(timeout).await;
            Err(DesktopError::Timeout(timeout))
          },
        ),
      )
      .await
    }
    .await;
    if result.is_err() {
      let _ = close_object(&connection, &path, "org.freedesktop.portal.Request").await;
      if let Some(session) = &cleanup_session {
        let _ = close_object(&connection, session, "org.freedesktop.portal.Session").await;
      }
    }
    if reply_tx.send(result).is_err() {
      let _ = close_object(&connection, &path, "org.freedesktop.portal.Request").await;
      if let Some(session) = &cleanup_session {
        let _ = close_object(&connection, session, "org.freedesktop.portal.Session").await;
      }
    }
  })
  .detach();
  let result = reply_rx.await.map_err(|_| DesktopError::RuntimeClosed)?;
  guard.0.take();
  result
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn validates_identity_and_duplicate_shortcuts() {
    assert!(PortalOptions::new("com.example.Lintel").validate().is_ok());
    assert!(PortalOptions::new("lintel").validate().is_err());
    let item = ShortcutRequest {
      id: "show".into(),
      description: "Show".into(),
      preferred_trigger: None,
    };
    assert!(validate_shortcuts(&[item.clone(), item]).is_err());
  }
  #[test]
  fn binding_response_preserves_remapping_and_empty_subset() {
    assert!(decode_bindings(vec![]).unwrap().is_empty());
    let mut dict = Dict::new();
    put(&mut dict, "description", "Show");
    put(&mut dict, "trigger_description", "Super+Q (user choice)");
    let bound = decode_bindings(vec![("show".into(), dict)]).unwrap();
    assert_eq!(bound[0].trigger_description, "Super+Q (user choice)");
  }
}
