use super::{EventSink, LinuxDesktop, LinuxEvent, deadline, next_id};
use crate::{DesktopError, DesktopEvent, MenuEntry, MenuId, MenuSpec, Pending, Result, TrayIcon, TrayId, TraySpec};
use ksni::{
  TrayMethods,
  menu::{CheckmarkItem, MenuItem, StandardItem, SubMenu},
};
use std::collections::HashMap;
use std::sync::atomic::Ordering;

struct TrayData {
  id: TrayId,
  spec: TraySpec,
  menu: Option<(MenuId, MenuSpec)>,
  visible: bool,
  events: EventSink,
}
impl ksni::Tray for TrayData {
  fn id(&self) -> String {
    format!("lintel-{}", self.id.as_u64())
  }
  fn title(&self) -> String {
    self.spec.tooltip.clone().unwrap_or_else(|| "Lintel".into())
  }
  fn status(&self) -> ksni::Status {
    if self.visible {
      ksni::Status::Active
    } else {
      ksni::Status::Passive
    }
  }
  fn icon_pixmap(&self) -> Vec<ksni::Icon> {
    vec![icon(&self.spec.icon)]
  }
  fn tool_tip(&self) -> ksni::ToolTip {
    ksni::ToolTip {
      title: self.title(),
      ..Default::default()
    }
  }
  fn menu(&self) -> Vec<MenuItem<Self>> {
    self
      .menu
      .as_ref()
      .map(|(id, menu)| translate(&menu.items, *id))
      .unwrap_or_default()
  }
  fn activate(&mut self, x: i32, y: i32) {
    (self.events)(DesktopEvent::Linux(LinuxEvent::TrayActivated {
      tray: self.id,
      secondary: false,
      x,
      y,
    }));
  }
  fn secondary_activate(&mut self, x: i32, y: i32) {
    (self.events)(DesktopEvent::Linux(LinuxEvent::TrayActivated {
      tray: self.id,
      secondary: true,
      x,
      y,
    }));
  }
  fn watcher_online(&self) {
    (self.events)(DesktopEvent::Linux(LinuxEvent::TrayAvailability {
      tray: self.id,
      online: true,
      reason: None,
    }));
  }
  fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
    (self.events)(DesktopEvent::Linux(LinuxEvent::TrayAvailability {
      tray: self.id,
      online: false,
      reason: Some(format!("{reason:?}")),
    }));
    true // 保留对象等待 watcher 恢复；API事件明确报告当前不可见。
  }
}
struct TrayHandle {
  handle: ksni::Handle<TrayData>,
  menu: Option<MenuId>,
}
impl Drop for TrayHandle {
  fn drop(&mut self) {
    drop(self.handle.shutdown());
  }
}

#[derive(Default)]
pub(super) struct TrayState {
  menus: HashMap<MenuId, MenuSpec>,
  trays: HashMap<TrayId, TrayHandle>,
}
impl TrayState {
  pub(super) fn clear(&mut self) {
    self.trays.clear();
    self.menus.clear();
  }
  fn menu(&self, id: MenuId) -> Result<MenuSpec> {
    self.menus.get(&id).cloned().ok_or_else(|| unknown("menu", id.as_u64()))
  }
  async fn refresh_menu(&self, id: MenuId) -> Result<()> {
    let spec = self.menu(id)?;
    for tray in self.trays.values().filter(|tray| tray.menu == Some(id)) {
      let menu = spec.clone();
      if tray
        .handle
        .update(move |data| data.menu = Some((id, menu)))
        .await
        .is_none()
      {
        return Err(DesktopError::RuntimeClosed);
      }
    }
    Ok(())
  }
}
fn unknown(kind: &'static str, id: u64) -> DesktopError {
  DesktopError::UnknownHandle { kind, id }
}
fn icon(icon: &TrayIcon) -> ksni::Icon {
  let mut data = icon.rgba.clone();
  for pixel in data.chunks_exact_mut(4) {
    pixel.rotate_right(1);
  }
  ksni::Icon {
    width: icon.width as i32,
    height: icon.height as i32,
    data,
  }
}
fn translate(entries: &[MenuEntry], menu: MenuId) -> Vec<MenuItem<TrayData>> {
  entries
    .iter()
    .map(|entry| match entry {
      MenuEntry::Separator => MenuItem::Separator,
      MenuEntry::Item {
        id,
        label,
        enabled,
        accelerator,
      } => {
        let id = id.clone();
        StandardItem {
          label: label.replace('_', "__"),
          enabled: *enabled,
          shortcut: accelerator.as_deref().map(shortcut).unwrap_or_default(),
          activate: Box::new(move |tray: &mut TrayData| (tray.events)(DesktopEvent::Menu { menu, item: id.clone() })),
          ..Default::default()
        }
        .into()
      }
      MenuEntry::Check {
        id,
        label,
        enabled,
        checked,
      } => {
        let id = id.clone();
        CheckmarkItem {
          label: label.replace('_', "__"),
          enabled: *enabled,
          checked: *checked,
          activate: Box::new(move |tray: &mut TrayData| (tray.events)(DesktopEvent::Menu { menu, item: id.clone() })),
          ..Default::default()
        }
        .into()
      }
      MenuEntry::Submenu { label, enabled, items } => SubMenu {
        label: label.replace('_', "__"),
        enabled: *enabled,
        submenu: translate(items, menu),
        ..Default::default()
      }
      .into(),
    })
    .collect()
}
fn shortcut(accelerator: &str) -> Vec<Vec<String>> {
  vec![
    accelerator
      .split('+')
      .map(|part| match part.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "cmdorctrl" | "commandorcontrol" => "Control".into(),
        "alt" => "Alt".into(),
        "shift" => "Shift".into(),
        "super" | "meta" | "cmd" | "command" => "Super".into(),
        _ => part
          .trim()
          .strip_prefix("Key")
          .filter(|key| key.len() == 1)
          .or_else(|| part.trim().strip_prefix("Digit").filter(|key| key.len() == 1))
          .unwrap_or(part.trim())
          .into(),
      })
      .collect(),
  ]
}
fn entry_mut<'a>(items: &'a mut [MenuEntry], id: &str) -> Option<&'a mut MenuEntry> {
  for entry in items {
    match entry {
      MenuEntry::Item { id: own, .. } | MenuEntry::Check { id: own, .. } if own == id => return Some(entry),
      MenuEntry::Submenu { items, .. } => {
        if let Some(entry) = entry_mut(items, id) {
          return Some(entry);
        }
      }
      _ => {}
    }
  }
  None
}

impl LinuxDesktop {
  pub fn create_menu(&self, spec: MenuSpec) -> Result<Pending<MenuId>> {
    spec.validate()?;
    self.submit(move |inner| async move {
      let id = MenuId(next_id());
      inner.tray.lock().await.menus.insert(id, spec);
      Ok(id)
    })
  }
  pub fn remove_menu(&self, id: MenuId) -> Result<Pending<()>> {
    self.submit(move |inner| async move {
      let mut state = inner.tray.lock().await;
      if state.trays.values().any(|tray| tray.menu == Some(id)) {
        return Err(crate::error::invalid("menu", "menu is still attached to a tray"));
      }
      state.menus.remove(&id).ok_or_else(|| unknown("menu", id.as_u64()))?;
      Ok(())
    })
  }
  pub fn create_tray(&self, spec: TraySpec) -> Result<Pending<TrayId>> {
    spec.icon.validate()?;
    if let Some(tooltip) = &spec.tooltip {
      crate::model::validate_text("tray tooltip", tooltip, 4096)?;
    }
    self.submit(move |inner| async move {
      deadline(async {
        let mut state = inner.tray.lock().await;
        let menu = spec.menu.map(|id| state.menu(id).map(|spec| (id, spec))).transpose()?;
        let id = TrayId(next_id());
        let menu_id = spec.menu;
        let handle = TrayData {
          id,
          spec,
          menu,
          visible: true,
          events: inner.events.clone(),
        }
        .spawn()
        .await
        .map_err(|e| crate::error::native("register StatusNotifierItem", e))?;
        let handle = TrayHandle { handle, menu: menu_id };
        if inner.closed.load(Ordering::Acquire) {
          return Err(DesktopError::RuntimeClosed);
        }
        state.trays.insert(id, handle);
        Ok(id)
      })
      .await
    })
  }
  pub fn remove_tray(&self, id: TrayId) -> Result<Pending<()>> {
    self.submit(move |inner| async move {
      let tray = inner
        .tray
        .lock()
        .await
        .trays
        .remove(&id)
        .ok_or_else(|| unknown("tray", id.as_u64()))?;
      deadline(async {
        tray.handle.shutdown().await;
        Ok(())
      })
      .await
    })
  }
  pub fn set_tray_menu(&self, id: TrayId, menu: Option<MenuId>) -> Result<Pending<()>> {
    self.submit(move |inner| async move {
      deadline(async {
        let mut state = inner.tray.lock().await;
        let spec = menu.map(|id| state.menu(id).map(|spec| (id, spec))).transpose()?;
        let tray = state.trays.get_mut(&id).ok_or_else(|| unknown("tray", id.as_u64()))?;
        tray
          .handle
          .update(move |data| data.menu = spec)
          .await
          .ok_or(DesktopError::RuntimeClosed)?;
        tray.menu = menu;
        Ok(())
      })
      .await
    })
  }
  fn update_tray(&self, id: TrayId, update: impl FnOnce(&mut TrayData) + Send + 'static) -> Result<Pending<()>> {
    self.submit(move |inner| async move {
      deadline(async {
        let state = inner.tray.lock().await;
        let tray = state.trays.get(&id).ok_or_else(|| unknown("tray", id.as_u64()))?;
        tray.handle.update(update).await.ok_or(DesktopError::RuntimeClosed)
      })
      .await
    })
  }
  pub fn set_tray_icon(&self, id: TrayId, icon: TrayIcon) -> Result<Pending<()>> {
    icon.validate()?;
    self.update_tray(id, move |data| data.spec.icon = icon)
  }
  pub fn set_tray_visible(&self, id: TrayId, visible: bool) -> Result<Pending<()>> {
    self.update_tray(id, move |data| data.visible = visible)
  }
  pub fn set_tray_tooltip(&self, id: TrayId, tooltip: Option<String>) -> Result<Pending<()>> {
    if let Some(tooltip) = &tooltip {
      crate::model::validate_text("tray tooltip", tooltip, 4096)?;
    }
    self.update_tray(id, move |data| data.spec.tooltip = tooltip)
  }
  fn update_menu(
    &self,
    id: MenuId,
    item: String,
    update: impl FnOnce(&mut MenuEntry) -> Result<()> + Send + 'static,
  ) -> Result<Pending<()>> {
    self.submit(move |inner| async move {
      deadline(async {
        let mut state = inner.tray.lock().await;
        let spec = state.menus.get_mut(&id).ok_or_else(|| unknown("menu", id.as_u64()))?;
        let entry = entry_mut(&mut spec.items, &item)
          .ok_or_else(|| crate::error::invalid("menu item", format!("unknown item {item}")))?;
        update(entry)?;
        state.refresh_menu(id).await
      })
      .await
    })
  }
  pub fn set_menu_item_enabled(&self, id: MenuId, item: String, enabled: bool) -> Result<Pending<()>> {
    self.update_menu(id, item, move |entry| {
      match entry {
        MenuEntry::Item { enabled: value, .. } | MenuEntry::Check { enabled: value, .. } => *value = enabled,
        _ => unreachable!(),
      };
      Ok(())
    })
  }
  pub fn set_menu_item_checked(&self, id: MenuId, item: String, checked: bool) -> Result<Pending<()>> {
    self.update_menu(id, item, move |entry| match entry {
      MenuEntry::Check { checked: value, .. } => {
        *value = checked;
        Ok(())
      }
      _ => Err(crate::error::invalid("menu item", "item is not checkable")),
    })
  }
  pub fn set_menu_item_label(&self, id: MenuId, item: String, label: String) -> Result<Pending<()>> {
    crate::model::validate_text("menu label", &label, 4096)?;
    self.update_menu(id, item, move |entry| {
      match entry {
        MenuEntry::Item { label: value, .. } | MenuEntry::Check { label: value, .. } => *value = label,
        _ => unreachable!(),
      };
      Ok(())
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn argb_network_order_preserves_each_pixel() {
    let icon = icon(&TrayIcon {
      rgba: vec![1, 2, 3, 4, 5, 6, 7, 8],
      width: 2,
      height: 1,
      template: false,
    });
    assert_eq!(icon.data, vec![4, 1, 2, 3, 8, 5, 6, 7]);
  }
  #[test]
  fn translated_menu_preserves_ids_check_state_and_literal_underscores() {
    let menu = MenuId(7);
    let spec = MenuSpec {
      items: vec![
        MenuEntry::Item {
          id: "open".into(),
          label: "a_b".into(),
          enabled: true,
          accelerator: Some("Ctrl+Shift+O".into()),
        },
        MenuEntry::Check {
          id: "check".into(),
          label: "Check".into(),
          enabled: false,
          checked: true,
        },
      ],
    };
    let items = translate(&spec.items, menu);
    let MenuItem::Standard(item) = &items[0] else { panic!() };
    assert_eq!(item.label, "a__b");
    assert_eq!(item.shortcut, vec![vec!["Control", "Shift", "O"]]);
    let MenuItem::Checkmark(item) = &items[1] else { panic!() };
    assert!(item.checked);
    assert!(!item.enabled);
  }
}
