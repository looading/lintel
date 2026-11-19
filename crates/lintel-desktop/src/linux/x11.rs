//! Explicit X11 passive grabs. No XWayland fallback is selected for a Wayland application.
use super::{CloseSignal, EventSink, LinuxEvent, next_id};
use crate::{DesktopError, DesktopEvent, Pending, Result, ShortcutId};
use std::{
  collections::HashMap,
  sync::{
    Arc,
    mpsc::{self, SyncSender, TrySendError},
  },
  time::Duration,
};
use x11rb::{
  connection::Connection,
  protocol::{
    Event,
    xproto::{ConnectionExt, GrabMode, KeyButMask, ModMask},
  },
  rust_connection::RustConnection,
};

type Reply<T> = futures_channel::oneshot::Sender<Result<T>>;
enum Command {
  Register(Parsed, Reply<ShortcutId>),
  Unregister(ShortcutId, Reply<()>),
}
struct Owner {
  sender: SyncSender<Command>,
  close: Arc<CloseSignal>,
}
impl Drop for Owner {
  fn drop(&mut self) {
    self.close.close();
  }
}
/// A scoped connection to one explicitly selected X11 server. Last Drop releases every grab.
#[derive(Clone)]
pub struct X11Shortcuts {
  owner: Arc<Owner>,
  dispatch_thread: std::thread::ThreadId,
}
impl X11Shortcuts {
  pub(crate) async fn connect(
    display: Option<&str>,
    events: EventSink,
    dispatch_thread: std::thread::ThreadId,
  ) -> Result<Self> {
    let display = display.map(str::to_owned);
    let (ready_tx, ready_rx) = futures_channel::oneshot::channel();
    let (sender, receiver) = mpsc::sync_channel(128);
    let close = Arc::new(CloseSignal::new());
    let signal = close.clone();
    std::thread::Builder::new()
      .name("lintel-x11-shortcuts".into())
      .spawn(move || {
        let connection =
          x11rb::connect(display.as_deref()).map_err(|e| crate::error::native("connect X11 shortcuts", e));
        let (connection, screen) = match connection {
          Ok(connection) => connection,
          Err(error) => {
            let _ = ready_tx.send(Err(error));
            return;
          }
        };
        let root = connection.setup().roots[screen].root;
        let mut state = State {
          connection,
          root,
          grabs: HashMap::new(),
          events,
        };
        if ready_tx.send(Ok(())).is_err() {
          return;
        }
        while !signal.is_closed() {
          for _ in 0..64 {
            match state.connection.poll_for_event() {
              Ok(Some(Event::KeyPress(event))) => state.key(event.detail, event.state, true),
              Ok(Some(Event::KeyRelease(event))) => state.key(event.detail, event.state, false),
              Ok(Some(Event::MappingNotify(_))) => {
                state.stopped("X11 keymap changed; reconnect and register shortcuts again".into());
                signal.close();
                return;
              }
              Ok(Some(_)) => {}
              Ok(None) => break,
              Err(error) => {
                state.stopped(error.to_string());
                signal.close();
                return;
              }
            }
          }
          match receiver.recv_timeout(Duration::from_millis(10)) {
            Ok(Command::Register(parsed, reply)) => {
              let result = state.register(parsed);
              if let Err(Ok(id)) = reply.send(result) {
                let _ = state.unregister(id);
              }
            }
            Ok(Command::Unregister(id, reply)) => {
              let _ = reply.send(state.unregister(id));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
          }
        }
        // 独立连接关闭时 X server 原子释放其全部 passive grabs。
      })
      .map_err(DesktopError::Io)?;
    ready_rx.await.map_err(|_| DesktopError::RuntimeClosed)??;
    Ok(Self {
      owner: Arc::new(Owner { sender, close }),
      dispatch_thread,
    })
  }
  pub(crate) fn close_signal(&self) -> &Arc<CloseSignal> {
    &self.owner.close
  }
  pub fn is_closed(&self) -> bool {
    self.owner.close.is_closed()
  }
  pub fn close(&self) {
    self.owner.close.close();
  }
  pub fn register(&self, accelerator: &str) -> Result<Pending<ShortcutId>> {
    let parsed = Parsed::parse(accelerator)?;
    self.send(|reply| Command::Register(parsed, reply))
  }
  pub fn unregister(&self, id: ShortcutId) -> Result<Pending<()>> {
    self.send(|reply| Command::Unregister(id, reply))
  }
  fn send<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Result<Pending<T>> {
    if self.owner.close.is_closed() {
      return Err(DesktopError::RuntimeClosed);
    }
    let (sender, receiver) = futures_channel::oneshot::channel();
    self.owner.sender.try_send(command(sender)).map_err(|e| match e {
      TrySendError::Full(_) => DesktopError::QueueFull,
      TrySendError::Disconnected(_) => DesktopError::RuntimeClosed,
    })?;
    Ok(Pending::from_receiver(receiver, self.dispatch_thread))
  }
}
struct Grab {
  key: u8,
  mods: u16,
  ignored: u16,
  masks: Vec<u16>,
}
struct State {
  connection: RustConnection,
  root: u32,
  grabs: HashMap<ShortcutId, Grab>,
  events: EventSink,
}
impl State {
  fn stopped(&self, reason: String) {
    (self.events)(DesktopEvent::Linux(LinuxEvent::BackendStopped {
      backend: "x11-shortcuts",
      reason,
    }));
  }
  fn key(&self, key: u8, state: KeyButMask, pressed: bool) {
    let state = u16::from(state) & 0xff;
    for (id, grab) in &self.grabs {
      if grab.key == key && state & !grab.ignored == grab.mods {
        (self.events)(DesktopEvent::Shortcut { shortcut: *id, pressed });
      }
    }
  }
  fn register(&mut self, parsed: Parsed) -> Result<ShortcutId> {
    let setup = self.connection.setup();
    let mapping = self
      .connection
      .get_keyboard_mapping(setup.min_keycode, setup.max_keycode - setup.min_keycode + 1)
      .map_err(|e| crate::error::native("query X11 keymap", e))?
      .reply()
      .map_err(|e| crate::error::native("query X11 keymap", e))?;
    let width = mapping.keysyms_per_keycode as usize;
    if width == 0 {
      return Err(crate::error::invalid("X11 keymap", "empty mapping"));
    }
    let key = mapping
      .keysyms
      .chunks_exact(width)
      .position(|symbols| symbols[0] == parsed.keysym)
      .map(|index| setup.min_keycode + index as u8)
      .ok_or_else(|| crate::error::invalid("X11 shortcut", "key is absent from the unshifted current X11 keymap"))?;
    let modifiers = self
      .connection
      .get_modifier_mapping()
      .map_err(|e| crate::error::native("query X11 modifiers", e))?
      .reply()
      .map_err(|e| crate::error::native("query X11 modifiers", e))?;
    let per_modifier = modifiers.keycodes.len() / 8;
    let mut modifier_symbols: [Vec<u32>; 8] = std::array::from_fn(|_| Vec::new());
    for (modifier, keys) in modifiers.keycodes.chunks_exact(per_modifier.max(1)).take(8).enumerate() {
      for key in keys.iter().copied().filter(|key| *key >= setup.min_keycode) {
        let index = (key - setup.min_keycode) as usize * width;
        if let Some(symbols) = mapping.keysyms.get(index..index + width) {
          modifier_symbols[modifier].extend_from_slice(symbols);
        }
      }
    }
    let mods = resolve_modifiers(parsed.mods, &modifier_symbols)?;
    let mut ignored = u16::from(ModMask::LOCK);
    for (modifier, symbols) in modifier_symbols.iter().enumerate() {
      if symbols.iter().any(|sym| matches!(*sym, 0xff7f | 0xff14)) {
        ignored |= 1 << modifier;
      }
    }
    if mods & ignored != 0 {
      return Err(crate::error::invalid(
        "X11 shortcut",
        "requested modifier is assigned to a lock key",
      ));
    }
    if self.grabs.values().any(|grab| grab.key == key && grab.mods == mods) {
      return Err(crate::error::invalid(
        "X11 shortcut",
        "already registered on this connection",
      ));
    }
    let masks = lock_combinations(ignored);
    let mut grabbed = Vec::new();
    for mask in &masks {
      let result = self
        .connection
        .grab_key(
          false,
          self.root,
          ModMask::from(mods | mask),
          key,
          GrabMode::ASYNC,
          GrabMode::ASYNC,
        )
        .map_err(|e| crate::error::native("grab X11 shortcut", e))
        .and_then(|cookie| cookie.check().map_err(|e| crate::error::native("grab X11 shortcut", e)));
      if let Err(error) = result {
        for previous in grabbed {
          if let Ok(cookie) = self
            .connection
            .ungrab_key(key, self.root, ModMask::from(mods | previous))
          {
            let _ = cookie.check();
          }
        }
        let _ = self.connection.flush();
        return Err(error);
      }
      grabbed.push(*mask);
    }
    self
      .connection
      .flush()
      .map_err(|e| crate::error::native("flush X11 shortcut", e))?;
    let id = ShortcutId(next_id());
    self.grabs.insert(
      id,
      Grab {
        key,
        mods,
        ignored,
        masks,
      },
    );
    Ok(id)
  }
  fn unregister(&mut self, id: ShortcutId) -> Result<()> {
    let grab = self.grabs.get(&id).ok_or(DesktopError::UnknownHandle {
      kind: "shortcut",
      id: id.as_u64(),
    })?;
    for mask in &grab.masks {
      self
        .connection
        .ungrab_key(grab.key, self.root, ModMask::from(grab.mods | mask))
        .map_err(|e| crate::error::native("release X11 shortcut", e))?
        .check()
        .map_err(|e| crate::error::native("release X11 shortcut", e))?;
    }
    self
      .connection
      .flush()
      .map_err(|e| crate::error::native("flush X11 shortcut release", e))?;
    self.grabs.remove(&id);
    Ok(())
  }
}
fn resolve_modifiers(requested: u16, symbols: &[Vec<u32>; 8]) -> Result<u16> {
  let mut resolved = 0;
  for (logical, names) in [
    (1, [0xffe1, 0xffe2]),
    (4, [0xffe3, 0xffe4]),
    (8, [0xffe9, 0xffea]),
    (64, [0xffeb, 0xffec]),
  ] {
    if requested & logical == 0 {
      continue;
    }
    let mut assignments = 0u16;
    for (index, symbols) in symbols.iter().enumerate() {
      if symbols.iter().any(|symbol| names.contains(symbol)) {
        assignments |= 1 << index;
      }
    }
    if assignments.count_ones() != 1 {
      return Err(crate::error::invalid(
        "X11 modifier",
        "modifier is missing or assigned to multiple X11 masks",
      ));
    }
    resolved |= assignments;
  }
  Ok(resolved)
}

fn lock_combinations(mask: u16) -> Vec<u16> {
  let mut masks = vec![0];
  for bit in 0..8 {
    if mask & (1 << bit) != 0 {
      let extra = masks.iter().map(|m| m | (1 << bit)).collect::<Vec<_>>();
      masks.extend(extra);
    }
  }
  masks
}
#[derive(Debug, PartialEq, Eq)]
struct Parsed {
  mods: u16,
  keysym: u32,
}
impl Parsed {
  fn parse(value: &str) -> Result<Self> {
    crate::model::validate_text("X11 shortcut", value, 256)?;
    let mut parts = value.split('+').map(str::trim).collect::<Vec<_>>();
    let key = parts
      .pop()
      .ok_or_else(|| crate::error::invalid("X11 shortcut", "missing key"))?;
    let mut mods = 0;
    for modifier in parts {
      let mask = match modifier.to_ascii_lowercase().as_str() {
        "shift" => 1,
        "control" | "ctrl" => 4,
        "alt" => 8,
        "super" | "meta" | "cmd" | "command" => 64,
        _ => {
          return Err(crate::error::invalid(
            "X11 shortcut",
            format!("unknown modifier {modifier}"),
          ));
        }
      };
      if mods & mask != 0 {
        return Err(crate::error::invalid("X11 shortcut", "duplicate modifier"));
      }
      mods |= mask;
    }
    let lower = key.to_ascii_lowercase();
    let keysym = match lower.as_str() {
      "space" => 0x20,
      "tab" => 0xff09,
      "enter" | "return" => 0xff0d,
      "escape" | "esc" => 0xff1b,
      "backspace" => 0xff08,
      "delete" => 0xffff,
      "insert" => 0xff63,
      "left" => 0xff51,
      "right" => 0xff53,
      "up" => 0xff52,
      "down" => 0xff54,
      "home" => 0xff50,
      "end" => 0xff57,
      "pageup" => 0xff55,
      "pagedown" => 0xff56,
      _ if lower.starts_with('f') && lower[1..].parse::<u32>().is_ok_and(|n| (1..=35).contains(&n)) => {
        0xffbd + lower[1..].parse::<u32>().unwrap()
      }
      _ if lower.chars().count() == 1 => {
        let c = lower.chars().next().unwrap() as u32;
        if c <= 255 { c } else { 0x01000000 | c }
      }
      _ => return Err(crate::error::invalid("X11 shortcut", format!("unsupported key {key}"))),
    };
    Ok(Self { mods, keysym })
  }
}
#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn parses_explicit_modifiers_and_rejects_ambiguity() {
    assert_eq!(Parsed::parse("Ctrl+Shift+L").unwrap(), Parsed { mods: 5, keysym: 108 });
    assert!(Parsed::parse("Ctrl+Control+L").is_err());
    assert!(Parsed::parse("Hyper+L").is_err());
    assert!(Parsed::parse("Ctrl+").is_err());
  }
  #[test]
  fn modifiers_follow_server_mapping_and_reject_ambiguous_aliases() {
    let mut symbols: [Vec<u32>; 8] = std::array::from_fn(|_| Vec::new());
    symbols[2] = vec![0xffe3, 0xffe4];
    symbols[5] = vec![0xffe9, 0xffea];
    assert_eq!(resolve_modifiers(4 | 8, &symbols).unwrap(), 4 | 32);
    assert!(resolve_modifiers(64, &symbols).is_err());
    symbols[6] = vec![0xffe9];
    assert!(resolve_modifiers(8, &symbols).is_err());
  }
  #[test]
  fn failed_connection_cannot_report_registration_success() {
    futures_lite::future::block_on(async {
      let result = X11Shortcuts::connect(Some("invalid-display"), Arc::new(|_| {}), std::thread::current().id()).await;
      assert!(matches!(
        result,
        Err(DesktopError::Native {
          operation: "connect X11 shortcuts",
          ..
        })
      ));
    });
  }

  #[test]
  fn covers_every_lock_combination_once() {
    assert_eq!(lock_combinations(2 | 16 | 32), vec![0, 2, 16, 18, 32, 34, 48, 50]);
  }
}
