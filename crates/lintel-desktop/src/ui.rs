use std::{
  collections::HashMap,
  sync::atomic::{AtomicU64, Ordering},
};

use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState, hotkey::HotKey};
use muda::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIconBuilder, TrayIconEvent};

use crate::{
  DesktopError, DesktopEvent, MenuEntry, MenuId, MenuSpec, MouseButton, Result, ShortcutId, TrayIcon, TrayId, TraySpec,
  error::invalid, model::validate_text,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
fn next_id() -> u64 {
  NEXT_ID.fetch_add(1, Ordering::Relaxed)
}
fn missing(kind: &'static str, id: u64) -> DesktopError {
  DesktopError::UnknownHandle { kind, id }
}

#[derive(Clone)]
enum ActionItem {
  Item(MenuItem),
  Check(CheckMenuItem),
}
impl ActionItem {
  fn native_id(&self) -> &muda::MenuId {
    match self {
      Self::Item(i) => i.id(),
      Self::Check(i) => i.id(),
    }
  }
  fn set_enabled(&self, enabled: bool) {
    match self {
      Self::Item(i) => i.set_enabled(enabled),
      Self::Check(i) => i.set_enabled(enabled),
    }
  }
}
struct NativeMenu {
  menu: Menu,
  actions: HashMap<String, ActionItem>,
}
struct NativeTray {
  tray: tray_icon::TrayIcon,
  menu: Option<MenuId>,
}

#[derive(Default)]
pub(crate) struct UiBackend {
  menus: HashMap<MenuId, NativeMenu>,
  trays: HashMap<TrayId, NativeTray>,
  shortcuts: HashMap<ShortcutId, HotKey>,
  hotkeys: Option<GlobalHotKeyManager>,
  application_menu: Option<MenuId>,
  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  window_menus: lintel_desktop_platform::windows::WindowsMenuHost,
  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  window_targets: HashMap<crate::WindowMenuTarget, lintel_desktop_platform::windows::WindowToken>,
  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  window_menu_refs: HashMap<crate::WindowMenuTarget, MenuId>,
  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  window_leases: crate::runtime::WindowsMenuLeases,
  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  window_cleanup_errors: std::collections::VecDeque<DesktopError>,
}

impl UiBackend {
  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  pub(crate) fn register_windows_window<W: crate::windows_menu::HasWindowHandle + Send + Sync + 'static + ?Sized>(
    &mut self,
    owner: std::sync::Arc<W>,
  ) -> Result<crate::WindowMenuTarget> {
    self.sweep_window_leases();
    let native = self
      .window_menus
      .register(owner)
      .map_err(|error| crate::error::native("register window menu target", error))?;
    let target = crate::WindowMenuTarget(next_id());
    self.window_targets.insert(target, native);
    Ok(target)
  }

  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  pub(crate) fn unregister_windows_window(&mut self, target: crate::WindowMenuTarget) -> Result<()> {
    let native = self
      .window_targets
      .remove(&target)
      .ok_or_else(|| missing("window menu target", target.0))?;
    self.window_menu_refs.remove(&target);
    self.window_leases.remove(target);
    self
      .window_menus
      .unregister(native)
      .map_err(|error| crate::error::native("unregister window menu target", error))
  }

  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  pub(crate) fn window_menu_dispatcher(&self) -> crate::windows_menu::WindowsMenuDispatcher {
    self.window_menus.dispatcher()
  }

  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  pub(crate) fn set_window_menu(&mut self, target: crate::WindowMenuTarget, menu: Option<MenuId>) -> Result<()> {
    let native = *self
      .window_targets
      .get(&target)
      .ok_or_else(|| missing("window menu target", target.0))?;
    let root = menu
      .map(|id| {
        self
          .menus
          .get(&id)
          .map(|entry| entry.menu.clone())
          .ok_or_else(|| missing("menu", id.0))
      })
      .transpose()?;
    self
      .window_menus
      .set_menu(native, root)
      .map_err(|error| crate::error::native("attach Windows menu", error))?;
    match menu {
      Some(menu) => {
        self.window_menu_refs.insert(target, menu);
      }
      None => {
        self.window_menu_refs.remove(&target);
      }
    }
    Ok(())
  }

  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  pub(crate) fn watch_window_lease(
    &mut self,
    target: crate::WindowMenuTarget,
    signal: &std::sync::Arc<crate::runtime::WindowsMenuLeaseSignal>,
  ) {
    self.window_leases.watch(target, signal);
  }
  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  pub(crate) fn sweep_window_leases(&mut self) {
    for target in self.window_leases.expired() {
      if let Err(error) = self.unregister_windows_window(target) {
        if self.window_cleanup_errors.len() >= 64 {
          self.window_cleanup_errors.pop_front();
        }
        self.window_cleanup_errors.push_back(error);
      }
    }
  }
  #[cfg(all(feature = "windows-menu", target_os = "windows"))]
  pub(crate) fn take_window_menu_cleanup_errors(&mut self) -> Vec<DesktopError> {
    self.window_cleanup_errors.drain(..).collect()
  }

  pub(crate) fn create_menu(&mut self, spec: MenuSpec) -> Result<MenuId> {
    spec.validate()?;
    let id = MenuId(next_id());
    let menu = Menu::new();
    let mut actions = HashMap::new();
    for item in build_entries(spec.items, id, &mut actions)? {
      menu
        .append(item.as_ref())
        .map_err(|e| crate::error::native("append menu item", e))?;
    }
    self.menus.insert(id, NativeMenu { menu, actions });
    Ok(id)
  }

  pub(crate) fn remove_menu(&mut self, id: MenuId) -> Result<()> {
    if !self.menus.contains_key(&id) {
      return Err(missing("menu", id.0));
    }
    #[cfg(all(feature = "windows-menu", target_os = "windows"))]
    {
      let targets: Vec<_> = self
        .window_menu_refs
        .iter()
        .filter(|(_, menu)| **menu == id)
        .map(|(target, _)| *target)
        .collect();
      for target in targets {
        self.set_window_menu(target, None)?;
      }
    }
    if self.application_menu == Some(id) {
      self.set_application_menu(None)?;
    }
    for tray in self.trays.values_mut().filter(|tray| tray.menu == Some(id)) {
      tray.tray.set_menu(None);
      tray.menu = None;
    }
    self.menus.remove(&id);
    Ok(())
  }

  pub(crate) fn set_application_menu(&mut self, id: Option<MenuId>) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
      if let Some(id) = id {
        self.menus.get(&id).ok_or_else(|| missing("menu", id.0))?;
      }
      if let Some(old) = self.application_menu.take() {
        if let Some(menu) = self.menus.get(&old) {
          menu.menu.remove_for_nsapp();
        }
      }
      if let Some(id) = id {
        self.menus[&id].menu.init_for_nsapp();
      }
      self.application_menu = id;
      Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
      let _ = id;
      Err(crate::error::unsupported(
        "application menu",
        "this facade currently attaches the global macOS application menu only",
      ))
    }
  }

  fn action(&self, menu: MenuId, item: &str) -> Result<&ActionItem> {
    self
      .menus
      .get(&menu)
      .ok_or_else(|| missing("menu", menu.0))?
      .actions
      .get(item)
      .ok_or_else(|| invalid("menu item", format!("no action named {item:?} in menu {}", menu.0)))
  }
  pub(crate) fn set_menu_item_enabled(&mut self, menu: MenuId, item: &str, enabled: bool) -> Result<()> {
    self.action(menu, item)?.set_enabled(enabled);
    Ok(())
  }
  pub(crate) fn set_menu_item_checked(&mut self, menu: MenuId, item: &str, checked: bool) -> Result<()> {
    match self.action(menu, item)? {
      ActionItem::Check(item) => {
        item.set_checked(checked);
        Ok(())
      }
      ActionItem::Item(_) => Err(invalid("menu item", "only a check item has checked state")),
    }
  }

  pub(crate) fn set_menu_item_label(&mut self, menu: MenuId, item: &str, label: &str) -> Result<()> {
    match self.action(menu, item)? {
      ActionItem::Item(item) => item.set_text(label),
      ActionItem::Check(item) => item.set_text(label),
    }
    Ok(())
  }
  pub(crate) fn create_tray(&mut self, spec: TraySpec) -> Result<TrayId> {
    spec.icon.validate()?;
    if let Some(text) = &spec.tooltip {
      validate_text("tray tooltip", text, 4096)?;
    }
    let id = TrayId(next_id());
    let mut builder = TrayIconBuilder::new().with_id(format!("lintel-tray-{}", id.0));
    if let Some(menu) = spec.menu {
      builder = builder.with_menu(Box::new(
        self
          .menus
          .get(&menu)
          .ok_or_else(|| missing("menu", menu.0))?
          .menu
          .clone(),
      ));
    }
    if let Some(tooltip) = &spec.tooltip {
      builder = builder.with_tooltip(tooltip);
    }
    let icon = tray_icon::Icon::from_rgba(spec.icon.rgba, spec.icon.width, spec.icon.height)
      .map_err(|e| crate::error::native("create tray icon pixels", e))?;
    builder = builder.with_icon(icon);
    #[cfg(target_os = "macos")]
    {
      builder = builder.with_icon_as_template(spec.icon.template);
    }
    let tray = builder
      .build()
      .map_err(|e| crate::error::native("create tray icon", e))?;
    self.trays.insert(id, NativeTray { tray, menu: spec.menu });
    Ok(id)
  }
  pub(crate) fn remove_tray(&mut self, id: TrayId) -> Result<()> {
    self.trays.remove(&id).ok_or_else(|| missing("tray", id.0))?;
    Ok(())
  }
  fn tray(&self, id: TrayId) -> Result<&tray_icon::TrayIcon> {
    self
      .trays
      .get(&id)
      .map(|value| &value.tray)
      .ok_or_else(|| missing("tray", id.0))
  }
  pub(crate) fn set_tray_menu(&mut self, id: TrayId, menu: Option<MenuId>) -> Result<()> {
    let native_menu = menu
      .map(|menu| {
        self
          .menus
          .get(&menu)
          .map(|entry| entry.menu.clone())
          .ok_or_else(|| missing("menu", menu.0))
      })
      .transpose()?;
    let tray = self.trays.get_mut(&id).ok_or_else(|| missing("tray", id.0))?;
    tray
      .tray
      .set_menu(native_menu.map(|menu| Box::new(menu) as Box<dyn muda::ContextMenu>));
    tray.menu = menu;
    Ok(())
  }
  pub(crate) fn set_tray_visible(&mut self, id: TrayId, visible: bool) -> Result<()> {
    self
      .tray(id)?
      .set_visible(visible)
      .map_err(|e| crate::error::native("set tray visibility", e))
  }
  pub(crate) fn set_tray_tooltip(&mut self, id: TrayId, tooltip: Option<String>) -> Result<()> {
    self
      .tray(id)?
      .set_tooltip(tooltip.as_deref())
      .map_err(|e| crate::error::native("set tray tooltip", e))
  }
  pub(crate) fn set_tray_icon(&mut self, id: TrayId, icon: TrayIcon) -> Result<()> {
    icon.validate()?;
    let pixels = tray_icon::Icon::from_rgba(icon.rgba, icon.width, icon.height)
      .map_err(|e| crate::error::native("create tray icon pixels", e))?;
    self
      .tray(id)?
      .set_icon(Some(pixels))
      .map_err(|e| crate::error::native("set tray icon", e))?;
    #[cfg(target_os = "macos")]
    self.tray(id)?.set_icon_as_template(icon.template);
    Ok(())
  }

  fn hotkeys(&mut self) -> Result<&GlobalHotKeyManager> {
    if self.hotkeys.is_none() {
      self.hotkeys =
        Some(GlobalHotKeyManager::new().map_err(|e| crate::error::native("initialize global shortcuts", e))?);
    }
    Ok(self.hotkeys.as_ref().expect("hotkey manager initialized above"))
  }
  pub(crate) fn register_shortcut(&mut self, accelerator: &str) -> Result<ShortcutId> {
    validate_text("shortcut", accelerator, 256)?;
    let hotkey: HotKey = accelerator.parse().map_err(|e| invalid("shortcut", format!("{e}")))?;
    if self.shortcuts.values().any(|existing| existing.id() == hotkey.id()) {
      return Err(invalid("shortcut", "this shortcut is already registered"));
    }
    self
      .hotkeys()?
      .register(hotkey)
      .map_err(|e| crate::error::native("register global shortcut", e))?;
    let id = ShortcutId(next_id());
    self.shortcuts.insert(id, hotkey);
    Ok(id)
  }
  pub(crate) fn unregister_shortcut(&mut self, id: ShortcutId) -> Result<()> {
    let hotkey = *self.shortcuts.get(&id).ok_or_else(|| missing("shortcut", id.0))?;
    self
      .hotkeys()?
      .unregister(hotkey)
      .map_err(|e| crate::error::native("unregister global shortcut", e))?;
    self.shortcuts.remove(&id);
    Ok(())
  }

  pub(crate) fn menu_event(&self, event: &muda::MenuEvent) -> Option<DesktopEvent> {
    for (menu_id, menu) in &self.menus {
      for (item_id, item) in &menu.actions {
        if item.native_id() == &event.id {
          return Some(DesktopEvent::Menu {
            menu: *menu_id,
            item: item_id.clone(),
          });
        }
      }
    }
    None
  }
  pub(crate) fn shortcut_event(&self, event: &GlobalHotKeyEvent) -> Option<DesktopEvent> {
    self
      .shortcuts
      .iter()
      .find(|(_, hotkey)| hotkey.id() == event.id)
      .map(|(id, _)| DesktopEvent::Shortcut {
        shortcut: *id,
        pressed: event.state == HotKeyState::Pressed,
      })
  }
  pub(crate) fn tray_event(&self, event: &TrayIconEvent) -> Option<DesktopEvent> {
    let id = self
      .trays
      .iter()
      .find(|(_, value)| value.tray.id() == event.id())
      .map(|(id, _)| *id)?;
    match event {
      TrayIconEvent::Click {
        button, button_state, ..
      } => Some(DesktopEvent::TrayClick {
        tray: id,
        button: mouse_button(*button),
        pressed: *button_state == tray_icon::MouseButtonState::Down,
      }),
      TrayIconEvent::DoubleClick { button, .. } => Some(DesktopEvent::TrayDoubleClick {
        tray: id,
        button: mouse_button(*button),
      }),
      _ => None,
    }
  }
}

impl Drop for UiBackend {
  fn drop(&mut self) {
    #[cfg(all(feature = "windows-menu", target_os = "windows"))]
    {
      self.window_menus.clear();
      self.window_targets.clear();
      self.window_menu_refs.clear();
    }

    if let Some(manager) = &self.hotkeys {
      // Resource cleanup is best effort; explicit unregister propagates failures.
      let keys: Vec<_> = self.shortcuts.values().copied().collect();
      let _ = manager.unregister_all(&keys);
    }
    #[cfg(target_os = "macos")]
    if let Some(menu) = self.application_menu.and_then(|id| self.menus.get(&id)) {
      menu.menu.remove_for_nsapp();
    }
    // Tray menus can reference menu resources, so release trays first.
    self.trays.clear();
    self.menus.clear();
  }
}

fn mouse_button(button: tray_icon::MouseButton) -> MouseButton {
  match button {
    tray_icon::MouseButton::Left => MouseButton::Left,
    tray_icon::MouseButton::Right => MouseButton::Right,
    tray_icon::MouseButton::Middle => MouseButton::Middle,
  }
}

fn build_entries(
  entries: Vec<MenuEntry>,
  menu: MenuId,
  actions: &mut HashMap<String, ActionItem>,
) -> Result<Vec<Box<dyn IsMenuItem>>> {
  let mut output: Vec<Box<dyn IsMenuItem>> = Vec::with_capacity(entries.len());
  for entry in entries {
    match entry {
      MenuEntry::Item {
        id,
        label,
        enabled,
        accelerator,
      } => {
        let accelerator = accelerator
          .map(|value| {
            value
              .parse::<muda::accelerator::Accelerator>()
              .map_err(|e| invalid("menu accelerator", e.to_string()))
          })
          .transpose()?;
        let item = MenuItem::with_id(format!("lintel-menu-{}-{id}", menu.0), label, enabled, accelerator);
        actions.insert(id, ActionItem::Item(item.clone()));
        output.push(Box::new(item));
      }
      MenuEntry::Check {
        id,
        label,
        enabled,
        checked,
      } => {
        let item = CheckMenuItem::with_id(format!("lintel-menu-{}-{id}", menu.0), label, enabled, checked, None);
        actions.insert(id, ActionItem::Check(item.clone()));
        output.push(Box::new(item));
      }
      MenuEntry::Submenu { label, enabled, items } => {
        let submenu = Submenu::new(label, enabled);
        for item in build_entries(items, menu, actions)? {
          submenu
            .append(item.as_ref())
            .map_err(|e| crate::error::native("append submenu item", e))?;
        }
        output.push(Box::new(submenu));
      }
      MenuEntry::Separator => output.push(Box::new(PredefinedMenuItem::separator())),
    }
  }
  Ok(output)
}

#[cfg(test)]
mod shortcut_tests {
  use super::*;
  #[test]
  fn cmd_or_ctrl_key_l_native_id_routes_pressed_and_released() {
    let mut backend = UiBackend::default();
    let hotkey: HotKey = "CmdOrCtrl+Shift+KeyL".parse().unwrap();
    assert_eq!(hotkey.key, global_hotkey::hotkey::Code::KeyL);
    assert!(hotkey.mods.contains(global_hotkey::hotkey::Modifiers::SHIFT));
    #[cfg(target_os = "macos")]
    assert!(hotkey.mods.contains(global_hotkey::hotkey::Modifiers::SUPER));
    #[cfg(target_os = "windows")]
    assert!(hotkey.mods.contains(global_hotkey::hotkey::Modifiers::CONTROL));
    let target = ShortcutId(500);
    backend.shortcuts.insert(target, hotkey);
    for (state, pressed) in [(HotKeyState::Pressed, true), (HotKeyState::Released, false)] {
      assert_eq!(
        backend.shortcut_event(&GlobalHotKeyEvent { id: hotkey.id(), state }),
        Some(DesktopEvent::Shortcut {
          shortcut: target,
          pressed
        })
      );
    }
    assert!(
      backend
        .shortcut_event(&GlobalHotKeyEvent {
          id: hotkey.id().wrapping_add(1),
          state: HotKeyState::Pressed
        })
        .is_none()
    );
  }
}
