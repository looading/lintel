//! Client-rendered menu model and interaction state. This module creates no native/global menubar.
//! Widget adapters publish MenuAction as an ordinary application message; checked state is controlled.
use crate::{MenuEntry, MenuSpec, Result, error::invalid};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuAction {
  pub id: String,
  /// Proposed new state for a check item. Apply it to MenuModel in the application's update.
  pub checked: Option<bool>,
}

/// Validate once in application state. Mutating/replacing a model invalidates open widget sessions.
#[derive(Debug, Clone)]
pub struct MenuModel {
  spec: MenuSpec,
  identity: u64,
  revision: u64,
}
impl MenuModel {
  pub fn new(spec: MenuSpec) -> Result<Self> {
    spec.validate()?;
    static NEXT: AtomicU64 = AtomicU64::new(1);
    Ok(Self {
      spec,
      identity: NEXT.fetch_add(1, Ordering::Relaxed),
      revision: 0,
    })
  }
  fn bump(&mut self) {
    // Cloned models may fork independently; globally unique revisions prevent stale path reuse.
    static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);
    self.revision = NEXT_REVISION.fetch_add(1, Ordering::Relaxed);
  }
  pub fn spec(&self) -> &MenuSpec {
    &self.spec
  }
  pub fn token(&self) -> (u64, u64) {
    (self.identity, self.revision)
  }
  pub fn replace(&mut self, spec: MenuSpec) -> Result<()> {
    spec.validate()?;
    self.spec = spec;
    self.bump();
    Ok(())
  }
  pub fn set_checked(&mut self, id: &str, checked: bool) -> Result<()> {
    match find(&mut self.spec.items, id) {
      Some(MenuEntry::Check { checked: value, .. }) => {
        if *value != checked {
          *value = checked;
          self.bump();
        }
        Ok(())
      }
      Some(_) => Err(invalid("menu item", "item is not checkable")),
      None => Err(invalid("menu item", format!("unknown ID {id}"))),
    }
  }
  pub fn set_enabled(&mut self, id: &str, enabled: bool) -> Result<()> {
    match find(&mut self.spec.items, id) {
      Some(MenuEntry::Item { enabled: value, .. } | MenuEntry::Check { enabled: value, .. }) => {
        if *value != enabled {
          *value = enabled;
          self.bump();
        }
        Ok(())
      }
      _ => Err(invalid("menu item", format!("unknown ID {id}"))),
    }
  }
  pub fn entry(&self, path: &[usize]) -> Option<&MenuEntry> {
    let mut items = &self.spec.items[..];
    for (depth, index) in path.iter().enumerate() {
      let entry = items.get(*index)?;
      if depth + 1 == path.len() {
        return Some(entry);
      }
      let MenuEntry::Submenu { items: children, .. } = entry else {
        return None;
      };
      items = children;
    }
    None
  }
  pub fn items(&self, path: &[usize]) -> Option<&[MenuEntry]> {
    if path.is_empty() {
      return Some(&self.spec.items);
    }
    match self.entry(path)? {
      MenuEntry::Submenu { items, .. } => Some(items),
      _ => None,
    }
  }
  pub fn enabled(&self, path: &[usize]) -> bool {
    !path.is_empty() && (1..=path.len()).all(|length| self.entry(&path[..length]).is_some_and(entry_enabled))
  }
  fn action(&self, path: &[usize]) -> Option<MenuAction> {
    if !self.enabled(path) {
      return None;
    }
    match self.entry(path)? {
      MenuEntry::Item { id, .. } => Some(MenuAction {
        id: id.clone(),
        checked: None,
      }),
      MenuEntry::Check { id, checked, .. } => Some(MenuAction {
        id: id.clone(),
        checked: Some(!checked),
      }),
      _ => None,
    }
  }
}
fn find<'a>(items: &'a mut [MenuEntry], id: &str) -> Option<&'a mut MenuEntry> {
  for item in items {
    match item {
      MenuEntry::Item { id: own, .. } | MenuEntry::Check { id: own, .. } if own == id => return Some(item),
      MenuEntry::Submenu { items, .. } => {
        if let Some(item) = find(items, id) {
          return Some(item);
        }
      }
      _ => {}
    }
  }
  None
}
pub fn entry_enabled(entry: &MenuEntry) -> bool {
  match entry {
    MenuEntry::Item { enabled, .. } | MenuEntry::Check { enabled, .. } | MenuEntry::Submenu { enabled, .. } => *enabled,
    MenuEntry::Separator => false,
  }
}
fn indices(items: &[MenuEntry]) -> Vec<usize> {
  items
    .iter()
    .enumerate()
    .filter(|(_, entry)| entry_enabled(entry))
    .map(|(index, _)| index)
    .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuKey {
  FocusBar,
  Left,
  Right,
  Up,
  Down,
  Home,
  End,
  Activate,
  Escape,
  Tab,
}

/// Per-widget/window interaction state. It stores paths, never callbacks, business state or windows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MenuSession {
  model: Option<(u64, u64)>,
  active: bool,
  top: Option<usize>,
  open: Vec<usize>,
  selected: Vec<Option<usize>>,
  pressed: Option<Vec<usize>>,
}
impl MenuSession {
  pub fn is_open(&self) -> bool {
    self.active
  }
  pub fn top(&self) -> Option<usize> {
    self.top
  }
  pub fn open_path(&self) -> &[usize] {
    &self.open
  }
  pub fn selected(&self, level: usize) -> Option<usize> {
    self.selected.get(level).copied().flatten()
  }
  pub fn close(&mut self) {
    self.active = false;
    self.top = None;
    self.open.clear();
    self.selected.clear();
    self.pressed = None;
  }
  /// Any model mutation closes pending interaction, preventing stale paths activating new actions.
  pub fn reconcile(&mut self, model: &MenuModel) -> bool {
    let token = model.token();
    if self.model == Some(token) {
      return false;
    }
    let changed = self.active || self.pressed.is_some();
    self.close();
    self.model = Some(token);
    changed
  }
  fn open_top(&mut self, model: &MenuModel, index: usize, select: bool) {
    if !model.enabled(&[index]) {
      return;
    }
    self.active = true;
    self.top = Some(index);
    self.open.clear();
    self.selected.clear();
    if let Some(MenuEntry::Submenu { items, .. }) = model.entry(&[index]) {
      self.open.push(index);
      self
        .selected
        .push(select.then(|| indices(items).first().copied()).flatten());
    }
  }
  /// Current keyboard/accessibility focus within the menu, separate from body focus.
  pub fn focused_path(&self) -> Option<Vec<usize>> {
    if !self.active {
      return None;
    }
    if self.open.is_empty() {
      return self.top.map(|top| vec![top]);
    }
    let mut path = self.open.clone();
    if let Some(index) = self.selected(self.open.len() - 1) {
      path.push(index);
    }
    Some(path)
  }
  /// Select a control without invoking it. Closed ancestor menus are opened as necessary.
  pub fn focus_path(&mut self, model: &MenuModel, path: &[usize]) -> bool {
    self.reconcile(model);
    if !model.enabled(path) {
      return false;
    }
    if self.focused_path().as_deref() == Some(path) {
      return true;
    }
    self.active = true;
    self.top = Some(path[0]);
    self.open = path[..path.len() - 1].to_vec();
    self.selected = path.iter().skip(1).copied().map(Some).collect();
    self.pressed = None;
    true
  }
  /// Idempotent expansion for native accessibility providers; it never invokes a business action.
  pub fn expand_path(&mut self, model: &MenuModel, path: &[usize]) -> bool {
    self.reconcile(model);
    if !model.enabled(path) || !matches!(model.entry(path), Some(MenuEntry::Submenu { .. })) {
      return false;
    }
    if self.open.starts_with(path) {
      return true;
    }
    self.focus_path(model, path);
    self.open = path.to_vec();
    self.selected.push(None);
    true
  }
  /// Collapse a visible branch. Collapsing a root header ends the menu session.
  pub fn collapse_path(&mut self, model: &MenuModel, path: &[usize]) -> bool {
    self.reconcile(model);
    if !model.enabled(path) || !matches!(model.entry(path), Some(MenuEntry::Submenu { .. })) {
      return false;
    }
    if !self.open.starts_with(path) {
      return true;
    }
    if path.len() == 1 {
      self.close();
    } else {
      self.open = path[..path.len() - 1].to_vec();
      self.selected = path.iter().skip(1).copied().map(Some).collect();
      self.pressed = None;
    }
    true
  }
  pub fn press_header(&mut self, model: &MenuModel, index: usize) {
    self.reconcile(model);
    if !model.enabled(&[index]) {
      return;
    }
    if matches!(model.entry(&[index]), Some(MenuEntry::Submenu { .. })) {
      if self.active && self.top == Some(index) && !self.open.is_empty() {
        self.close();
      } else {
        self.open_top(model, index, false);
      }
    } else {
      self.open_top(model, index, false);
      self.pressed = Some(vec![index]);
    }
  }
  pub fn hover_header(&mut self, model: &MenuModel, index: usize) {
    self.reconcile(model);
    if self.active && self.top != Some(index) && model.enabled(&[index]) {
      self.pressed = None;
      self.open_top(model, index, false);
    }
  }
  pub fn hover_item(&mut self, model: &MenuModel, level: usize, index: usize) {
    self.reconcile(model);
    if level >= self.open.len() {
      return;
    }
    let mut path = self.open[..=level].to_vec();
    path.push(index);
    if !model.enabled(&path) {
      self.open.truncate(level + 1);
      self.selected.truncate(level + 1);
      self.selected[level] = None;
      return;
    }
    self.selected[level] = Some(index);
    if self.open.get(level + 1) == Some(&index) {
      return;
    }
    self.open.truncate(level + 1);
    self.selected.truncate(level + 1);
    if matches!(model.entry(&path), Some(MenuEntry::Submenu { .. })) {
      self.open.push(index);
      self.selected.push(None);
    }
  }
  pub fn press_item(&mut self, model: &MenuModel, level: usize, index: usize) {
    self.hover_item(model, level, index);
    if level < self.open.len() {
      let mut path = self.open[..=level].to_vec();
      path.push(index);
      self.pressed = model.action(&path).map(|_| path);
    }
  }
  /// A press/release must target the same still-valid action; dragging outside cancels activation.
  pub fn release(&mut self, model: &MenuModel, path: Option<&[usize]>) -> Option<MenuAction> {
    self.reconcile(model);
    let pressed = self.pressed.take()?;
    if path != Some(pressed.as_slice()) {
      return None;
    }
    let action = model.action(&pressed)?;
    self.close();
    Some(action)
  }
  fn step_top(&mut self, model: &MenuModel, forward: bool) {
    let enabled = indices(&model.spec.items);
    if let Some(index) = step(&enabled, self.top, forward) {
      self.open_top(model, index, true);
    }
  }
  fn step_item(&mut self, model: &MenuModel, forward: bool) {
    if self.open.is_empty() {
      if let Some(top) = self.top {
        self.open_top(model, top, true);
      }
      if !forward {
        self.edge_item(model, false);
      }
      return;
    }
    let level = self.open.len() - 1;
    if let Some(items) = model.items(&self.open) {
      self.selected[level] = step(&indices(items), self.selected[level], forward);
    }
  }
  fn edge_item(&mut self, model: &MenuModel, first: bool) {
    if self.open.is_empty() {
      let enabled = indices(&model.spec.items);
      if let Some(index) = if first { enabled.first() } else { enabled.last() } {
        self.open_top(model, *index, false);
      }
      return;
    }
    let level = self.open.len() - 1;
    if let Some(items) = model.items(&self.open) {
      let enabled = indices(items);
      self.selected[level] = if first {
        enabled.first().copied()
      } else {
        enabled.last().copied()
      };
    }
  }
  fn activate(&mut self, model: &MenuModel) -> Option<MenuAction> {
    let path = if self.open.is_empty() {
      vec![self.top?]
    } else {
      let mut path = self.open.clone();
      path.push(self.selected(self.open.len() - 1)?);
      path
    };
    if !model.enabled(&path) {
      return None;
    }
    if let Some(MenuEntry::Submenu { items, .. }) = model.entry(&path) {
      self.open = path;
      self.selected.push(indices(items).first().copied());
      None
    } else {
      let action = model.action(&path)?;
      self.close();
      Some(action)
    }
  }
  pub fn key(&mut self, model: &MenuModel, key: MenuKey) -> Option<MenuAction> {
    self.reconcile(model);
    self.pressed = None;
    if key == MenuKey::FocusBar {
      if self.active {
        self.close();
      } else if let Some(index) = indices(&model.spec.items).first() {
        self.open_top(model, *index, false);
      }
      return None;
    }
    if !self.active {
      return None;
    }
    match key {
      MenuKey::FocusBar => {}
      MenuKey::Tab => self.close(),
      MenuKey::Escape => {
        if self.open.len() > 1 {
          self.open.pop();
          self.selected.pop();
        } else {
          self.close();
        }
      }
      MenuKey::Left => {
        if self.open.len() > 1 {
          self.open.pop();
          self.selected.pop();
        } else {
          self.step_top(model, false);
        }
      }
      MenuKey::Right => {
        let submenu = self
          .open
          .last()
          .and_then(|_| self.selected(self.open.len() - 1))
          .and_then(|index| {
            let mut path = self.open.clone();
            path.push(index);
            model.entry(&path)
          })
          .is_some_and(|item| matches!(item, MenuEntry::Submenu { .. }));
        if submenu {
          self.activate(model);
        } else {
          self.step_top(model, true);
        }
      }
      MenuKey::Down => self.step_item(model, true),
      MenuKey::Up => self.step_item(model, false),
      MenuKey::Home => self.edge_item(model, true),
      MenuKey::End => self.edge_item(model, false),
      MenuKey::Activate => return self.activate(model),
    }
    None
  }
}
fn step(enabled: &[usize], current: Option<usize>, forward: bool) -> Option<usize> {
  if enabled.is_empty() {
    return None;
  }
  let position = current.and_then(|current| enabled.iter().position(|index| *index == current));
  Some(
    enabled[match (position, forward) {
      (Some(index), true) => (index + 1) % enabled.len(),
      (Some(0) | None, false) => enabled.len() - 1,
      (Some(index), false) => index - 1,
      (None, true) => 0,
    }],
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  fn model() -> MenuModel {
    MenuModel::new(MenuSpec {
      items: vec![
        MenuEntry::Submenu {
          label: "File".into(),
          enabled: true,
          items: vec![
            MenuEntry::item("open", "Open"),
            MenuEntry::Separator,
            MenuEntry::Item {
              id: "disabled".into(),
              label: "Disabled".into(),
              enabled: false,
              accelerator: None,
            },
            MenuEntry::Check {
              id: "check".into(),
              label: "Check".into(),
              enabled: true,
              checked: false,
            },
            MenuEntry::Submenu {
              label: "More".into(),
              enabled: true,
              items: vec![MenuEntry::item("nested", "Nested")],
            },
          ],
        },
        MenuEntry::Submenu {
          label: "Hidden".into(),
          enabled: false,
          items: vec![MenuEntry::item("unreachable", "Unreachable")],
        },
        MenuEntry::item("help", "Help"),
      ],
    })
    .unwrap()
  }
  #[test]
  fn keys_skip_disabled_and_separator_and_wrap() {
    let model = model();
    let mut state = MenuSession::default();
    state.key(&model, MenuKey::FocusBar);
    state.key(&model, MenuKey::Down);
    assert_eq!(state.selected(0), Some(0));
    state.key(&model, MenuKey::Down);
    assert_eq!(state.selected(0), Some(3));
    state.key(&model, MenuKey::Up);
    assert_eq!(state.selected(0), Some(0));
    state.key(&model, MenuKey::Up);
    assert_eq!(state.selected(0), Some(4));
    state.key(&model, MenuKey::Home);
    assert_eq!(state.selected(0), Some(0));
    state.key(&model, MenuKey::End);
    assert_eq!(state.selected(0), Some(4));
  }
  #[test]
  fn nested_keyboard_escape_and_tab_release_menu_focus() {
    let model = model();
    let mut state = MenuSession::default();
    state.key(&model, MenuKey::FocusBar);
    state.key(&model, MenuKey::End);
    state.key(&model, MenuKey::Right);
    assert_eq!(state.open_path(), &[0, 4]);
    assert_eq!(state.selected(1), Some(0));
    state.key(&model, MenuKey::Escape);
    assert_eq!(state.open_path(), &[0]);
    state.key(&model, MenuKey::Right);
    state.key(&model, MenuKey::Tab);
    assert!(!state.is_open());
    state.key(&model, MenuKey::FocusBar);
    state.close();
    assert!(!state.is_open());
  }
  #[test]
  fn checked_actions_are_controlled_and_disabled_ancestors_never_activate() {
    let mut model = model();
    assert!(!model.enabled(&[1, 0]));
    let mut state = MenuSession::default();
    state.press_header(&model, 0);
    state.press_item(&model, 0, 3);
    let action = state.release(&model, Some(&[0, 3])).unwrap();
    assert_eq!(
      action,
      MenuAction {
        id: "check".into(),
        checked: Some(true)
      }
    );
    assert!(matches!(
      model.entry(&[0, 3]),
      Some(MenuEntry::Check { checked: false, .. })
    ));
    model.set_checked(&action.id, action.checked.unwrap()).unwrap();
    assert!(matches!(
      model.entry(&[0, 3]),
      Some(MenuEntry::Check { checked: true, .. })
    ));
    state.press_header(&model, 1);
    assert!(!state.is_open());
  }
  #[test]
  fn mutations_cancel_stale_pressed_actions() {
    let mut model = model();
    let mut state = MenuSession::default();
    state.press_header(&model, 0);
    state.press_item(&model, 0, 0);
    model.set_enabled("open", false).unwrap();
    assert!(state.release(&model, Some(&[0, 0])).is_none());
    assert!(!state.is_open());
  }
  #[test]
  fn dragging_outside_and_disabled_rows_do_not_activate() {
    let model = model();
    let mut state = MenuSession::default();
    state.press_header(&model, 0);
    state.press_item(&model, 0, 0);
    assert!(state.release(&model, None).is_none());
    state.press_item(&model, 0, 2);
    assert!(state.release(&model, Some(&[0, 2])).is_none());
    state.close();
    assert!(!state.is_open());
  }
  #[test]
  fn top_keyboard_navigation_skips_disabled_and_activates_business_id() {
    let model = model();
    let mut state = MenuSession::default();
    state.key(&model, MenuKey::FocusBar);
    state.key(&model, MenuKey::Left);
    assert_eq!(state.top(), Some(2));
    assert_eq!(state.key(&model, MenuKey::Activate).unwrap().id, "help");
    assert!(!state.is_open());
  }
  #[test]
  fn invalid_replacement_does_not_change_live_model() {
    let mut model = model();
    let token = model.token();
    assert!(
      model
        .replace(MenuSpec {
          items: vec![MenuEntry::item("dup", "A"), MenuEntry::item("dup", "B")]
        })
        .is_err()
    );
    assert_eq!(model.token(), token);
    assert!(model.set_checked("open", true).is_err());
  }
  #[test]
  fn empty_models_and_paths_are_safe() {
    let model = MenuModel::new(MenuSpec::default()).unwrap();
    let mut state = MenuSession::default();
    for key in [MenuKey::FocusBar, MenuKey::Down, MenuKey::Activate, MenuKey::Escape] {
      assert!(state.key(&model, key).is_none());
    }
    assert!(!state.is_open());
    assert!(model.entry(&[]).is_none());
    assert!(!model.enabled(&[999]));
  }
  #[test]
  fn independently_mutated_clones_cannot_share_an_interaction_token() {
    let original = model();
    let mut a = original.clone();
    let mut b = original;
    a.set_enabled("open", false).unwrap();
    b.set_checked("check", true).unwrap();
    assert_ne!(a.token(), b.token());
    let mut session = MenuSession::default();
    session.press_header(&a, 0);
    session.press_item(&a, 0, 3);
    assert!(session.release(&b, Some(&[0, 3])).is_none());
    assert!(!session.is_open());
  }
  #[test]
  fn replaced_model_cannot_activate_a_new_action_at_the_old_pressed_path() {
    let mut model = model();
    let mut session = MenuSession::default();
    session.press_header(&model, 0);
    session.press_item(&model, 0, 0);
    model
      .replace(MenuSpec {
        items: vec![MenuEntry::Submenu {
          label: "New".into(),
          enabled: true,
          items: vec![MenuEntry::item("different", "Different")],
        }],
      })
      .unwrap();
    assert!(session.release(&model, Some(&[0, 0])).is_none());
    assert!(!session.is_open());
    session.press_header(&model, 0);
    session.press_item(&model, 0, 0);
    assert_eq!(session.release(&model, Some(&[0, 0])).unwrap().id, "different");
  }
  #[test]
  fn native_focus_expand_collapse_do_not_invoke_or_toggle_and_track_focus_path() {
    let model = model();
    let mut session = MenuSession::default();
    assert!(session.focus_path(&model, &[0]));
    assert!(session.is_open());
    assert!(session.open_path().is_empty());
    assert_eq!(session.focused_path(), Some(vec![0]));
    assert!(session.expand_path(&model, &[0]));
    assert!(session.expand_path(&model, &[0]));
    assert_eq!(session.open_path(), &[0]);
    assert!(session.focus_path(&model, &[0, 4]));
    assert_eq!(session.focused_path(), Some(vec![0, 4]));
    assert!(session.expand_path(&model, &[0, 4]));
    assert_eq!(session.open_path(), &[0, 4]);
    assert!(session.collapse_path(&model, &[0, 4]));
    assert_eq!(session.open_path(), &[0]);
    assert_eq!(session.focused_path(), Some(vec![0, 4]));
    assert!(session.collapse_path(&model, &[0]));
    assert!(!session.is_open());
    assert!(!session.focus_path(&model, &[1, 0]));
  }
}
