//! Private menu semantics and a bounded native-to-UI mailbox. No application callback lives here.
use super::model::{WindowState, allocate_node_id};
use crate::desktop::accessibility::accesskit::{self as ak, NodeId};
use crate::{
  desktop::{
    MenuEntry,
    client_menu::{MenuModel, MenuSession, entry_enabled},
  },
  iced::Rectangle,
};
use std::{
  collections::{BTreeMap, VecDeque},
  sync::{Arc, Mutex, Weak},
};

const MAX_REQUESTS: usize = 64;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
  Focus,
  Click,
  Expand,
  Collapse,
}
impl Command {
  pub fn from_action(action: ak::Action) -> Option<Self> {
    Some(match action {
      ak::Action::Focus => Self::Focus,
      ak::Action::Click => Self::Click,
      ak::Action::Expand => Self::Expand,
      ak::Action::Collapse => Self::Collapse,
      _ => return None,
    })
  }
  fn action(self) -> ak::Action {
    match self {
      Self::Focus => ak::Action::Focus,
      Self::Click => ak::Action::Click,
      Self::Expand => ak::Action::Expand,
      Self::Collapse => ak::Action::Collapse,
    }
  }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
  Bar,
  Popup,
  Item,
  Check,
  Submenu,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
  kind: Kind,
  path: Vec<usize>,
  identity: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Target {
  pub token: (u64, u64),
  pub epoch: u64,
  pub kind: Kind,
  pub path: Vec<usize>,
  pub enabled: bool,
  pub actions: Vec<Command>,
}
#[derive(Clone, Debug)]
pub(crate) struct Request {
  pub node: NodeId,
  pub target: Target,
  pub command: Command,
}
struct Inbox {
  nodes: BTreeMap<NodeId, Target>,
  requests: VecDeque<Request>,
  closed: bool,
}
pub(crate) struct Mailbox {
  pub source: u64,
  inbox: Mutex<Inbox>,
  owner: Mutex<Weak<WindowState>>,
}
impl Mailbox {
  pub fn new(source: u64) -> Arc<Self> {
    Arc::new(Self {
      source,
      inbox: Mutex::new(Inbox {
        nodes: BTreeMap::new(),
        requests: VecDeque::new(),
        closed: false,
      }),
      owner: Mutex::new(Weak::new()),
    })
  }
  pub fn bind(&self, owner: &Arc<WindowState>) {
    *self.owner.lock().unwrap() = Arc::downgrade(owner);
  }
  pub fn replace(&self, nodes: BTreeMap<NodeId, Target>) {
    let mut inbox = self.inbox.lock().unwrap();
    if inbox.closed {
      return;
    }
    inbox
      .requests
      .retain(|request| nodes.get(&request.node) == Some(&request.target));
    inbox.nodes = nodes;
  }
  /// Called while WindowState's snapshot lock serializes admission with menu/modal teardown.
  pub fn enqueue(&self, node: NodeId, action: ak::Action) -> bool {
    let Some(command) = Command::from_action(action) else {
      return false;
    };
    let mut inbox = self.inbox.lock().unwrap();
    if inbox.closed || inbox.requests.len() == MAX_REQUESTS {
      return false;
    }
    let Some(target) = inbox
      .nodes
      .get(&node)
      .filter(|target| target.enabled && target.actions.contains(&command))
      .cloned()
    else {
      return false;
    };
    inbox.requests.push_back(Request { node, target, command });
    true
  }
  pub fn pending(&self) -> bool {
    !self.inbox.lock().unwrap().requests.is_empty()
  }
  pub fn drain(&self) -> Vec<Request> {
    self.inbox.lock().unwrap().requests.drain(..).collect()
  }
  pub fn accepts(&self, request: &Request) -> bool {
    let owner = { self.owner.lock().unwrap().upgrade() };
    if owner
      .as_ref()
      .is_some_and(|owner| !owner.menu_request_allowed(self.source, request.node))
    {
      return false;
    }
    let inbox = self.inbox.lock().unwrap();
    !inbox.closed
      && inbox.nodes.get(&request.node) == Some(&request.target)
      && request.target.enabled
      && request.target.actions.contains(&request.command)
  }
  fn retain_nodes(&self, live: &std::collections::BTreeSet<NodeId>) {
    let mut inbox = self.inbox.lock().unwrap();
    inbox.nodes.retain(|id, _| live.contains(id));
    inbox.requests.retain(|request| live.contains(&request.node));
  }
  pub fn revoke_popups(&self) {
    let mut inbox = self.inbox.lock().unwrap();
    inbox
      .nodes
      .retain(|_, target| target.kind == Kind::Bar || target.path.len() == 1 && target.kind != Kind::Popup);
    let live = inbox.nodes.keys().copied().collect::<std::collections::BTreeSet<_>>();
    inbox.requests.retain(|request| live.contains(&request.node));
  }
  pub fn clear_requests(&self) {
    self.inbox.lock().unwrap().requests.clear();
  }
  pub fn close(&self) {
    let mut inbox = self.inbox.lock().unwrap();
    inbox.closed = true;
    inbox.nodes.clear();
    inbox.requests.clear();
  }
  pub fn wake(&self) {
    let owner = { self.owner.lock().unwrap().upgrade() };
    if let Some(owner) = owner {
      (owner.wake)();
    }
  }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Space {
  Widget,
  Window,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MenuNode {
  pub id: NodeId,
  pub node: ak::Node,
  pub bounds: Rectangle,
  pub space: Space,
  pub kind: Kind,
  pub path: Vec<usize>,
}
#[derive(Clone)]
pub(crate) struct MenuTree {
  pub source: u64,
  pub root: NodeId,
  pub nodes: Vec<MenuNode>,
  pub focus: Option<NodeId>,
  pub mailbox: Weak<Mailbox>,
  pub focus_stamp: u64,
}
impl MenuTree {
  pub fn closed(&mut self) {
    self
      .nodes
      .retain(|node| node.kind == Kind::Bar || node.path.len() == 1 && node.kind != Kind::Popup);
    let ids = self
      .nodes
      .iter()
      .map(|node| node.id)
      .collect::<std::collections::BTreeSet<_>>();
    for node in &mut self.nodes {
      let children = node
        .node
        .children()
        .iter()
        .copied()
        .filter(|id| ids.contains(id))
        .collect::<Vec<_>>();
      node.node.set_children(children);
      if node.kind == Kind::Submenu {
        node.node.set_expanded(false);
      }
    }
    self.focus = None;
  }
}
#[derive(Clone)]
pub(crate) struct Panel {
  pub path: Vec<usize>,
  pub bounds: Rectangle,
  pub rows: Vec<Rectangle>,
}
pub(crate) struct Semantics {
  pub mailbox: Arc<Mailbox>,
  ids: BTreeMap<Key, NodeId>,
  tree: Option<MenuTree>,
  last_open: Vec<usize>,
  token: Option<(u64, u64)>,
  epoch: u64,
  active: bool,
  focus_stamp: u64,
}
impl Semantics {
  pub fn new(source: u64) -> Self {
    Self {
      mailbox: Mailbox::new(source),
      ids: BTreeMap::new(),
      tree: None,
      last_open: Vec::new(),
      token: None,
      epoch: 0,
      active: false,
      focus_stamp: 0,
    }
  }
  pub fn tree(&self) -> Option<MenuTree> {
    self.tree.clone()
  }
  pub fn invalidate_popups(&mut self) {
    self
      .ids
      .retain(|key, _| key.kind == Kind::Bar || key.path.len() == 1 && key.kind != Kind::Popup);
    if let Some(tree) = &mut self.tree {
      tree.closed();
    }
    self.mailbox.revoke_popups();
    self.last_open.clear();
  }
  pub fn session_changed(&mut self, model: &MenuModel, session: &MenuSession) {
    if self.active && !session.is_open() {
      self.epoch = self.epoch.wrapping_add(1);
      self.mailbox.clear_requests();
    }
    self.active = session.is_open();
    if self.token != Some(model.token()) {
      // Revision invalidates queued intent, not the identity of a still-visible control.
      self.mailbox.clear_requests();
      self.token = Some(model.token());
    }
    if self.last_open != session.open_path() {
      let open = session.open_path();
      let keep = |kind: Kind, path: &[usize]| {
        kind == Kind::Bar
          || path.len() == 1 && kind != Kind::Popup
          || if kind == Kind::Popup {
            !path.is_empty() && open.starts_with(path)
          } else {
            path.len() > 1 && open.starts_with(&path[..path.len() - 1])
          }
      };
      self.ids.retain(|key, _| keep(key.kind, &key.path));
      if let Some(tree) = &mut self.tree {
        tree.nodes.retain(|node| keep(node.kind, &node.path));
        let ids = tree
          .nodes
          .iter()
          .map(|node| node.id)
          .collect::<std::collections::BTreeSet<_>>();
        for node in &mut tree.nodes {
          node.node.set_children(
            node
              .node
              .children()
              .iter()
              .copied()
              .filter(|id| ids.contains(id))
              .collect::<Vec<_>>(),
          );
        }
        if tree.focus.is_some_and(|id| !ids.contains(&id)) {
          tree.focus = None;
        }
      }
      self.mailbox.retain_nodes(&self.ids.values().copied().collect());
    }
  }
  pub fn rebuild(
    &mut self,
    model: &MenuModel,
    session: &MenuSession,
    bar: Rectangle,
    headers: &[Rectangle],
    panels: &[Panel],
  ) {
    self.session_changed(model, session);
    let token = model.token();
    let mut nodes = Vec::new();
    let mut targets = BTreeMap::new();
    let mut live = std::collections::BTreeSet::new();
    let bar_key = Key {
      kind: Kind::Bar,
      path: vec![],
      identity: String::new(),
    };
    let root = self.id(bar_key.clone(), &mut live);
    let mut bar_node = ak::Node::new(ak::Role::MenuBar);
    bar_node.set_label("Application menu");
    bar_node.set_orientation(ak::Orientation::Horizontal);
    let enabled = model.spec().items.iter().any(entry_enabled);
    if enabled {
      bar_node.add_action(ak::Action::Focus);
    }
    targets.insert(
      root,
      Target {
        token,
        epoch: self.epoch,
        kind: Kind::Bar,
        path: vec![],
        enabled,
        actions: if enabled { vec![Command::Focus] } else { vec![] },
      },
    );
    nodes.push(MenuNode {
      id: root,
      node: bar_node,
      bounds: bar,
      space: Space::Widget,
      kind: Kind::Bar,
      path: vec![],
    });
    let mut path_ids = BTreeMap::<Vec<usize>, NodeId>::new();
    let mut popup_ids = BTreeMap::<Vec<usize>, NodeId>::new();
    for (index, entry) in model.spec().items.iter().enumerate() {
      if let Some(bounds) = headers.get(index) {
        self.item(
          model,
          session,
          vec![index],
          entry,
          *bounds,
          Space::Widget,
          &mut nodes,
          &mut targets,
          &mut live,
          &mut path_ids,
        );
      }
    }
    for panel in panels {
      if !session.is_open() || !session.open_path().starts_with(&panel.path) || !model.enabled(&panel.path) {
        continue;
      }
      let Some(items) = model.items(&panel.path) else {
        continue;
      };
      let Some(parent) = path_ids.get(&panel.path).copied() else {
        continue;
      };
      let key = Key {
        kind: Kind::Popup,
        path: panel.path.clone(),
        identity: String::new(),
      };
      let id = self.id(key, &mut live);
      popup_ids.insert(panel.path.clone(), id);
      let mut node = ak::Node::new(ak::Role::Menu);
      node.set_label(entry_label(model.entry(&panel.path)));
      node.set_orientation(ak::Orientation::Vertical);
      node.add_action(ak::Action::Focus);
      targets.insert(
        id,
        Target {
          token,
          epoch: self.epoch,
          kind: Kind::Popup,
          path: panel.path.clone(),
          enabled: true,
          actions: vec![Command::Focus],
        },
      );
      nodes.push(MenuNode {
        id,
        node,
        bounds: panel.bounds,
        space: Space::Window,
        kind: Kind::Popup,
        path: panel.path.clone(),
      });
      if let Some(parent_node) = nodes.iter_mut().find(|node| node.id == parent) {
        parent_node.node.set_children(vec![id]);
      }
      for (index, entry) in items.iter().enumerate() {
        if let Some(bounds) = panel.rows.get(index) {
          let mut path = panel.path.clone();
          path.push(index);
          self.item(
            model,
            session,
            path,
            entry,
            bounds
              .intersection(&panel.bounds)
              .unwrap_or_else(|| Rectangle::new(panel.bounds.position(), crate::iced::Size::ZERO)),
            Space::Window,
            &mut nodes,
            &mut targets,
            &mut live,
            &mut path_ids,
          );
        }
      }
    }
    let header_ids = model
      .spec()
      .items
      .iter()
      .enumerate()
      .filter_map(|(index, _)| path_ids.get(&vec![index]).copied())
      .collect::<Vec<_>>();
    nodes[0].node.set_children(header_ids);
    for (path, id) in popup_ids {
      let children = model
        .items(&path)
        .unwrap_or_default()
        .iter()
        .enumerate()
        .filter_map(|(index, _)| {
          let mut child = path.clone();
          child.push(index);
          path_ids.get(&child).copied()
        })
        .collect::<Vec<_>>();
      if let Some(node) = nodes.iter_mut().find(|node| node.id == id) {
        node.node.set_children(children);
      }
    }
    let focus = session
      .focused_path()
      .filter(|path| model.enabled(path))
      .and_then(|path| path_ids.get(&path).copied());
    self.ids.retain(|key, _| live.contains(key));
    self.last_open = session.open_path().to_vec();
    self.token = Some(token);
    if self.tree.as_ref().and_then(|tree| tree.focus) != focus {
      self.touch_focus();
    }
    self.mailbox.replace(targets);
    self.tree = Some(MenuTree {
      source: self.mailbox.source,
      root,
      nodes,
      focus,
      mailbox: Arc::downgrade(&self.mailbox),
      focus_stamp: self.focus_stamp,
    });
  }
  pub fn set_tree(&mut self, tree: MenuTree) {
    self.tree = Some(tree);
  }
  pub fn end_session(&mut self) {
    if self.active {
      self.epoch = self.epoch.wrapping_add(1);
    }
    self.active = false;
    self.mailbox.clear_requests();
    self.invalidate_popups();
  }
  pub fn touch_focus(&mut self) {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    self.focus_stamp = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
  }
  fn id(&mut self, key: Key, live: &mut std::collections::BTreeSet<Key>) -> NodeId {
    live.insert(key.clone());
    *self.ids.entry(key).or_insert_with(allocate_node_id)
  }
  #[allow(clippy::too_many_arguments)]
  fn item(
    &mut self,
    model: &MenuModel,
    session: &MenuSession,
    path: Vec<usize>,
    entry: &MenuEntry,
    bounds: Rectangle,
    space: Space,
    nodes: &mut Vec<MenuNode>,
    targets: &mut BTreeMap<NodeId, Target>,
    live: &mut std::collections::BTreeSet<Key>,
    path_ids: &mut BTreeMap<Vec<usize>, NodeId>,
  ) {
    let (kind, role, identity) = match entry {
      MenuEntry::Item { id, .. } => (Kind::Item, ak::Role::MenuItem, id.clone()),
      MenuEntry::Check { id, .. } => (Kind::Check, ak::Role::MenuItemCheckBox, id.clone()),
      MenuEntry::Submenu { .. } => (Kind::Submenu, ak::Role::MenuItem, String::new()),
      MenuEntry::Separator => return,
    };
    let id = self.id(
      Key {
        kind,
        path: path.clone(),
        identity,
      },
      live,
    );
    path_ids.insert(path.clone(), id);
    let enabled = model.enabled(&path);
    let mut node = ak::Node::new(role);
    node.set_label(entry_label(Some(entry)));
    if !enabled {
      node.set_disabled();
    }
    if let MenuEntry::Check { checked, .. } = entry {
      node.set_toggled(if *checked {
        ak::Toggled::True
      } else {
        ak::Toggled::False
      });
    }
    let mut actions = Vec::new();
    if enabled {
      actions.extend([Command::Focus, Command::Click]);
    }
    if kind == Kind::Submenu {
      node.set_has_popup(ak::HasPopup::Menu);
      node.set_expanded(enabled && session.is_open() && session.open_path().starts_with(&path));
      if enabled {
        actions.extend([Command::Expand, Command::Collapse]);
      }
    }
    for action in &actions {
      node.add_action(action.action());
    }
    targets.insert(
      id,
      Target {
        token: model.token(),
        epoch: self.epoch,
        kind,
        path: path.clone(),
        enabled,
        actions,
      },
    );
    nodes.push(MenuNode {
      id,
      node,
      bounds,
      space,
      kind,
      path,
    });
  }
}
fn entry_label(entry: Option<&MenuEntry>) -> String {
  match entry {
    Some(MenuEntry::Item { label, .. } | MenuEntry::Check { label, .. } | MenuEntry::Submenu { label, .. }) => {
      label.clone()
    }
    _ => String::new(),
  }
}

pub(crate) struct OverlayTree {
  pub tree: Option<MenuTree>,
}

#[cfg(test)]
mod tests {
  use super::super::model::Snapshot;
  use super::*;
  use crate::{
    desktop::{MenuEntry, MenuSpec},
    iced::{Point, Size},
  };
  fn model() -> MenuModel {
    MenuModel::new(MenuSpec {
      items: vec![
        MenuEntry::Submenu {
          label: "File".into(),
          enabled: true,
          items: vec![
            MenuEntry::item("run", "Run"),
            MenuEntry::Check {
              id: "check".into(),
              label: "Check".into(),
              enabled: true,
              checked: false,
            },
            MenuEntry::Item {
              id: "disabled".into(),
              label: "Disabled".into(),
              enabled: false,
              accelerator: None,
            },
            MenuEntry::Submenu {
              label: "More".into(),
              enabled: true,
              items: vec![MenuEntry::item("child", "Child")],
            },
          ],
        },
        MenuEntry::Check {
          id: "pin".into(),
          label: "Pin".into(),
          enabled: true,
          checked: false,
        },
      ],
    })
    .unwrap()
  }
  fn rect(x: f32, y: f32, w: f32, h: f32) -> Rectangle {
    Rectangle::new(Point::new(x, y), Size::new(w, h))
  }
  fn headers() -> [Rectangle; 2] {
    [rect(0.0, 0.0, 60.0, 32.0), rect(60.0, 0.0, 60.0, 32.0)]
  }
  fn panels() -> [Panel; 2] {
    [
      Panel {
        path: vec![0],
        bounds: rect(0.0, 32.0, 270.0, 128.0),
        rows: (0..4)
          .map(|index| rect(4.0, 36.0 + index as f32 * 30.0, 262.0, 30.0))
          .collect(),
      },
      Panel {
        path: vec![0, 3],
        bounds: rect(270.0, 126.0, 270.0, 38.0),
        rows: vec![rect(274.0, 130.0, 262.0, 30.0)],
      },
    ]
  }
  fn build(semantics: &mut Semantics, model: &MenuModel, session: &MenuSession) {
    semantics.rebuild(model, session, rect(0.0, 0.0, 800.0, 32.0), &headers(), &panels());
  }
  fn node<'a>(tree: &'a MenuTree, kind: Kind, path: &[usize]) -> &'a MenuNode {
    tree
      .nodes
      .iter()
      .find(|node| node.kind == kind && node.path == path)
      .unwrap()
  }
  fn install(owner: &Arc<WindowState>, menus: Vec<MenuTree>) {
    for mailbox in menus.iter().filter_map(|tree| tree.mailbox.upgrade()) {
      mailbox.bind(owner);
    }
    assert!(owner.publish(
      Snapshot {
        update: ak::TreeUpdate {
          nodes: vec![(NodeId(0), ak::Node::new(ak::Role::Window))],
          tree: Some(ak::TreeInfo::new(NodeId(0))),
          tree_id: ak::TreeId::ROOT,
          focus: NodeId(0)
        },
        routes: BTreeMap::new(),
        order: Vec::new(),
        signature: Vec::new(),
        modal: false,
        resume: None,
        menus
      },
      false
    ));
  }
  fn request(id: NodeId, action: ak::Action) -> ak::ActionRequest {
    ak::ActionRequest {
      target_tree: ak::TreeId::ROOT,
      target_node: id,
      action,
      data: None,
    }
  }
  #[test]
  fn menu_roles_layout_properties_and_focus_follow_the_real_open_branch() {
    let model = model();
    let mut session = MenuSession::default();
    session.expand_path(&model, &[0]);
    session.focus_path(&model, &[0, 1]);
    let mut semantics = Semantics::new(11);
    build(&mut semantics, &model, &session);
    let tree = semantics.tree().unwrap();
    assert_eq!(node(&tree, Kind::Bar, &[]).node.role(), ak::Role::MenuBar);
    assert_eq!(node(&tree, Kind::Popup, &[0]).node.role(), ak::Role::Menu);
    let header = node(&tree, Kind::Submenu, &[0]);
    assert_eq!(header.node.label(), Some("File"));
    assert_eq!(header.node.has_popup(), Some(ak::HasPopup::Menu));
    assert_eq!(header.node.is_expanded(), Some(true));
    assert!(header.node.supports_action(ak::Action::Click));
    assert!(header.node.supports_action(ak::Action::Expand));
    assert!(header.node.supports_action(ak::Action::Collapse));
    let check = node(&tree, Kind::Check, &[0, 1]);
    assert_eq!(check.node.role(), ak::Role::MenuItemCheckBox);
    assert_eq!(check.node.label(), Some("Check"));
    assert_eq!(check.node.toggled(), Some(ak::Toggled::False));
    assert_eq!(tree.focus, Some(check.id));
    assert_eq!(check.bounds, panels()[0].rows[1]);
    let disabled = node(&tree, Kind::Item, &[0, 2]);
    assert!(disabled.node.is_disabled());
    assert!(!disabled.node.supports_action(ak::Action::Click));
    assert!(!semantics.mailbox.enqueue(disabled.id, ak::Action::Click));
    assert!(!semantics.mailbox.enqueue(check.id, ak::Action::SetValue));
  }
  #[test]
  fn property_and_hover_changes_keep_control_identity_but_closed_popups_expire() {
    let mut model = model();
    let mut session = MenuSession::default();
    let mut semantics = Semantics::new(12);
    build(&mut semantics, &model, &session);
    let pin = node(&semantics.tree().unwrap(), Kind::Check, &[1]).id;
    model.set_checked("pin", true).unwrap();
    model.set_enabled("pin", false).unwrap();
    build(&mut semantics, &model, &session);
    let tree = semantics.tree().unwrap();
    assert_eq!(node(&tree, Kind::Check, &[1]).id, pin);
    assert_eq!(node(&tree, Kind::Check, &[1]).node.toggled(), Some(ak::Toggled::True));
    session.expand_path(&model, &[0]);
    build(&mut semantics, &model, &session);
    let run = node(&semantics.tree().unwrap(), Kind::Item, &[0, 0]).id;
    let checked_id = node(&semantics.tree().unwrap(), Kind::Check, &[0, 1]).id;
    model.set_checked("check", true).unwrap();
    model.set_enabled("run", false).unwrap();
    build(&mut semantics, &model, &session);
    assert_eq!(node(&semantics.tree().unwrap(), Kind::Check, &[0, 1]).id, checked_id);
    assert_eq!(node(&semantics.tree().unwrap(), Kind::Item, &[0, 0]).id, run);
    session.expand_path(&model, &[0]);
    build(&mut semantics, &model, &session);
    let run = node(&semantics.tree().unwrap(), Kind::Item, &[0, 0]).id;
    session.hover_item(&model, 0, 3);
    build(&mut semantics, &model, &session);
    let tree = semantics.tree().unwrap();
    assert_eq!(node(&tree, Kind::Item, &[0, 0]).id, run);
    let child = node(&tree, Kind::Item, &[0, 3, 0]).id;
    assert!(semantics.mailbox.enqueue(child, ak::Action::Click));
    session.collapse_path(&model, &[0, 3]);
    build(&mut semantics, &model, &session);
    assert!(semantics.mailbox.drain().is_empty());
    assert!(!semantics.mailbox.enqueue(child, ak::Action::Click));
    assert_eq!(node(&semantics.tree().unwrap(), Kind::Item, &[0, 0]).id, run);
    session.expand_path(&model, &[0, 3]);
    build(&mut semantics, &model, &session);
    assert_ne!(node(&semantics.tree().unwrap(), Kind::Item, &[0, 3, 0]).id, child);
  }
  #[test]
  fn queue_is_bounded_revision_checked_and_cannot_retarget_a_replaced_path() {
    let mut model = model();
    let session = MenuSession::default();
    let mut semantics = Semantics::new(13);
    build(&mut semantics, &model, &session);
    let pin = node(&semantics.tree().unwrap(), Kind::Check, &[1]).id;
    for _ in 0..MAX_REQUESTS {
      assert!(semantics.mailbox.enqueue(pin, ak::Action::Click));
    }
    assert!(!semantics.mailbox.enqueue(pin, ak::Action::Click));
    let requests = semantics.mailbox.drain();
    assert_eq!(requests.len(), MAX_REQUESTS);
    model.set_checked("pin", true).unwrap();
    build(&mut semantics, &model, &session);
    assert!(!semantics.mailbox.accepts(&requests[0]));
    assert_eq!(node(&semantics.tree().unwrap(), Kind::Check, &[1]).id, pin);
    model
      .replace(MenuSpec {
        items: vec![MenuEntry::item("replacement", "Replacement")],
      })
      .unwrap();
    build(&mut semantics, &model, &session);
    assert!(!semantics.mailbox.enqueue(pin, ak::Action::Click));
    assert_ne!(node(&semantics.tree().unwrap(), Kind::Item, &[0]).id, pin);
  }
  #[test]
  fn native_threads_only_enqueue_and_window_close_and_modal_sources_reject_stale_requests() {
    let owner = WindowState::new(|| {});
    let model = model();
    let session = MenuSession::default();
    let mut a = Semantics::new(21);
    let mut b = Semantics::new(22);
    build(&mut a, &model, &session);
    build(&mut b, &model, &session);
    let a_id = node(&a.tree().unwrap(), Kind::Submenu, &[0]).id;
    let b_id = node(&b.tree().unwrap(), Kind::Submenu, &[0]).id;
    install(&owner, vec![a.tree().unwrap(), b.tree().unwrap()]);
    let native = owner.clone();
    std::thread::spawn(move || native.action(request(a_id, ak::Action::Click)))
      .join()
      .unwrap();
    let queued = a.mailbox.drain();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].command, Command::Click);
    owner.action(request(b_id, ak::Action::Click));
    owner.menu_modal_for(21, true);
    assert!(
      b.mailbox.drain().is_empty(),
      "requests accepted before another source became modal must be canceled"
    );
    owner.action(request(b_id, ak::Action::Click));
    assert!(b.mailbox.drain().is_empty());
    owner.action(request(a_id, ak::Action::Expand));
    assert_eq!(a.mailbox.drain().len(), 1);
    owner.menu_modal_for(22, true);
    owner.menu_modal_for(21, false);
    owner.action(request(a_id, ak::Action::Click));
    assert!(a.mailbox.drain().is_empty());
    owner.action(request(b_id, ak::Action::Click));
    assert_eq!(b.mailbox.drain().len(), 1);
    owner.close();
    owner.action(request(b_id, ak::Action::Click));
    assert!(!b.mailbox.enqueue(b_id, ak::Action::Click));
    assert!(b.mailbox.drain().is_empty());
  }
  #[test]
  fn routes_and_owner_links_are_weak() {
    let owner = WindowState::new(|| {});
    let weak_owner = Arc::downgrade(&owner);
    let model = model();
    let session = MenuSession::default();
    let mut semantics = Semantics::new(31);
    build(&mut semantics, &model, &session);
    let tree = semantics.tree().unwrap();
    let weak = tree.mailbox.clone();
    install(&owner, vec![tree]);
    drop(semantics);
    assert!(weak.upgrade().is_none());
    drop(owner);
    assert!(weak_owner.upgrade().is_none());
  }
}

#[cfg(test)]
mod ui_tests {
  use super::super::{
    model::{Navigation, Snapshot},
    widgets::SemanticState,
  };
  use super::*;
  use crate::{
    desktop::{MenuEntry, MenuSpec},
    iced::{
      self, Element, Event, Point, Size, Theme,
      advanced::{Layout, mouse, renderer::Headless, widget},
    },
  };
  use iced::iced_winit::runtime::user_interface::{Cache, UserInterface};
  #[derive(Clone, Debug, PartialEq, Eq)]
  enum Message {
    Menu(crate::menu::MenuAction),
    Name(String),
    Body,
  }
  struct Bind {
    owner: Arc<WindowState>,
    menus: Vec<MenuTree>,
    routes: BTreeMap<NodeId, Weak<super::super::model::Mailbox>>,
    order: Vec<NodeId>,
    nodes: Vec<(NodeId, ak::Node)>,
    modal: bool,
  }
  impl widget::Operation for Bind {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn widget::Operation)) {
      operate(self);
    }
    fn custom(&mut self, _id: Option<&widget::Id>, bounds: iced::Rectangle, state: &mut dyn std::any::Any) {
      if let Some(menu) = state.downcast_mut::<crate::menu::MenuState>() {
        menu.allow_modal_collection();
        let owner = Arc::downgrade(&self.owner);
        let source = menu.modal_source_id();
        menu.set_modal_barrier(Arc::new(move |open| {
          if let Some(owner) = owner.upgrade() {
            owner.menu_modal_for(source, open);
          }
        }));
        self.modal |= menu.is_open();
        if let Some(tree) = menu.accessibility_tree() {
          self.add(tree);
        }
        return;
      }
      if let Some(overlay) = state.downcast_mut::<OverlayTree>() {
        if let Some(tree) = overlay.tree.clone() {
          self.add(tree);
        }
        return;
      }
      if let Some(semantic) = state.downcast_mut::<SemanticState>() {
        *semantic.mailbox.owner.lock().unwrap() = Arc::downgrade(&self.owner);
        let description = semantic.mailbox.description.lock().unwrap();
        let id = semantic.mailbox.id;
        let mut node = ak::Node::new(description.role);
        node.set_label(description.label.clone());
        node.set_bounds(ak::Rect::new(
          bounds.x as f64,
          bounds.y as f64,
          (bounds.x + bounds.width) as f64,
          (bounds.y + bounds.height) as f64,
        ));
        if description.focusable() {
          node.add_action(ak::Action::Focus);
          self.order.push(id);
        }
        self.routes.insert(id, Arc::downgrade(&semantic.mailbox));
        self.nodes.push((id, node));
      }
    }
  }
  impl Bind {
    fn add(&mut self, mut tree: MenuTree) {
      if let Some(mailbox) = tree.mailbox.upgrade() {
        mailbox.bind(&self.owner);
      }
      for node in &mut tree.nodes {
        let b = node.bounds;
        node.node.set_bounds(ak::Rect::new(
          b.x as f64,
          b.y as f64,
          (b.x + b.width) as f64,
          (b.y + b.height) as f64,
        ));
      }
      self.menus.retain(|old| old.source != tree.source);
      self.menus.push(tree);
    }
    fn publish(self) {
      let mut root = ak::Node::new(ak::Role::Window);
      root.set_children(self.nodes.iter().map(|(id, _)| *id).collect::<Vec<_>>());
      let mut nodes = vec![(NodeId(0), root)];
      nodes.extend(self.nodes);
      let (routes, order, resume) = if self.modal {
        (
          BTreeMap::new(),
          Vec::new(),
          Some(Navigation::new(self.routes, self.order)),
        )
      } else {
        (self.routes, self.order, None)
      };
      self.owner.publish(
        Snapshot {
          update: ak::TreeUpdate {
            nodes,
            tree: Some(ak::TreeInfo::new(NodeId(0))),
            tree_id: ak::TreeId::ROOT,
            focus: NodeId(0),
          },
          routes,
          order,
          signature: Vec::new(),
          modal: self.modal,
          resume,
          menus: self.menus,
        },
        false,
      );
    }
  }
  fn publish(
    ui: &mut UserInterface<'_, Message, Theme, iced::Renderer>,
    renderer: &iced::Renderer,
    owner: &Arc<WindowState>,
  ) {
    let mut bind = Bind {
      owner: owner.clone(),
      menus: Vec::new(),
      routes: BTreeMap::new(),
      order: Vec::new(),
      nodes: Vec::new(),
      modal: false,
    };
    ui.operate(renderer, &mut bind);
    bind.publish();
  }
  fn frame(
    ui: &mut UserInterface<'_, Message, Theme, iced::Renderer>,
    renderer: &mut iced::Renderer,
    owner: &Arc<WindowState>,
    messages: &mut Vec<Message>,
  ) {
    let redraw = Event::Window(iced::window::Event::RedrawRequested(
      iced::iced_core::time::Instant::now(),
    ));
    ui.update(
      &iced::iced_core::shell::Waker::noop(),
      &[redraw],
      mouse::Cursor::Unavailable,
      renderer,
      &mut iced::advanced::clipboard::Null,
      messages,
    );
    publish(ui, renderer, owner);
  }
  fn id(owner: &WindowState, role: ak::Role, label: &str) -> NodeId {
    owner
      .snapshot
      .lock()
      .unwrap()
      .as_ref()
      .unwrap()
      .update
      .nodes
      .iter()
      .find(|(_, node)| node.role() == role && node.label() == Some(label))
      .map(|(id, _)| *id)
      .unwrap()
  }
  fn action(owner: &Arc<WindowState>, id: NodeId, action: ak::Action) {
    let native = owner.clone();
    std::thread::spawn(move || {
      native.action(ak::ActionRequest {
        target_tree: ak::TreeId::ROOT,
        target_node: id,
        action,
        data: None,
      })
    })
    .join()
    .unwrap();
  }
  #[test]
  fn real_native_requests_focus_expand_click_and_collapse_on_ui_thread_only() {
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let mut renderer = runtime
      .block_on(<iced::Renderer as Headless>::new(
        iced::Font::DEFAULT,
        iced::Pixels(14.0),
        Some("tiny-skia"),
      ))
      .unwrap();
    let model = MenuModel::new(MenuSpec {
      items: vec![MenuEntry::Submenu {
        label: "File".into(),
        enabled: true,
        items: vec![
          MenuEntry::item("run", "Run"),
          MenuEntry::Check {
            id: "check".into(),
            label: "Check".into(),
            enabled: true,
            checked: false,
          },
          MenuEntry::Item {
            id: "disabled".into(),
            label: "Disabled".into(),
            enabled: false,
            accelerator: None,
          },
          MenuEntry::Submenu {
            label: "More".into(),
            enabled: true,
            items: vec![MenuEntry::item("child", "Child")],
          },
        ],
      }],
    })
    .unwrap();
    let view: Element<'_, Message> = crate::menu::menu_bar(
      &model,
      Message::Menu,
      iced::widget::column![
        crate::a11y::text_input("input", "Name", "", Message::Name),
        crate::a11y::button("body", "Body", Message::Body)
      ],
    )
    .into();
    let owner = WindowState::new(|| {});
    let mut ui = UserInterface::build(view, Size::new(800.0, 500.0), Cache::new(), &mut renderer);
    publish(&mut ui, &renderer, &owner);
    let input = id(&owner, ak::Role::TextInput, "Name");
    let body = id(&owner, ak::Role::Button, "Body");
    owner.focus(input);
    let mut messages = Vec::new();
    frame(&mut ui, &mut renderer, &owner, &mut messages);
    messages.clear();
    let header = id(&owner, ak::Role::MenuItem, "File");
    action(&owner, header, ak::Action::Focus);
    action(&owner, header, ak::Action::Expand);
    assert!(messages.is_empty());
    frame(&mut ui, &mut renderer, &owner, &mut messages);
    assert!(messages.is_empty());
    let snapshot = owner.tree().unwrap();
    assert_eq!(snapshot.focus, header);
    assert_eq!(owner.focus_token().0, input.0);
    assert!(snapshot.nodes.iter().any(|(_, node)| node.role() == ak::Role::Menu));
    assert!(!snapshot.nodes.iter().any(|(id, _)| *id == input || *id == body));
    let check = id(&owner, ak::Role::MenuItemCheckBox, "Check");
    let disabled = id(&owner, ak::Role::MenuItem, "Disabled");
    action(&owner, disabled, ak::Action::Click);
    action(&owner, body, ak::Action::Click);
    frame(&mut ui, &mut renderer, &owner, &mut messages);
    assert!(messages.is_empty());
    action(&owner, check, ak::Action::Focus);
    frame(&mut ui, &mut renderer, &owner, &mut messages);
    assert_eq!(owner.tree().unwrap().focus, check);
    assert_eq!(owner.focus_token().0, input.0);
    action(&owner, check, ak::Action::Click);
    assert!(messages.is_empty(), "native threads cannot dispatch business messages");
    frame(&mut ui, &mut renderer, &owner, &mut messages);
    assert_eq!(
      messages,
      vec![Message::Menu(crate::menu::MenuAction {
        id: "check".into(),
        checked: Some(true)
      })]
    );
    assert_eq!(owner.tree().unwrap().focus, input);
    messages.clear();
    action(&owner, check, ak::Action::Click);
    frame(&mut ui, &mut renderer, &owner, &mut messages);
    assert!(messages.is_empty(), "closed popup node IDs cannot reactivate a menu");
    action(&owner, header, ak::Action::Click);
    frame(&mut ui, &mut renderer, &owner, &mut messages);
    let more = id(&owner, ak::Role::MenuItem, "More");
    action(&owner, more, ak::Action::Expand);
    frame(&mut ui, &mut renderer, &owner, &mut messages);
    let child = id(&owner, ak::Role::MenuItem, "Child");
    action(&owner, more, ak::Action::Collapse);
    frame(&mut ui, &mut renderer, &owner, &mut messages);
    action(&owner, child, ak::Action::Click);
    frame(&mut ui, &mut renderer, &owner, &mut messages);
    assert!(messages.is_empty());
    action(&owner, header, ak::Action::Collapse);
    frame(&mut ui, &mut renderer, &owner, &mut messages);
    assert_eq!(owner.tree().unwrap().focus, input);
    owner.close();
    action(&owner, header, ak::Action::Click);
    frame(&mut ui, &mut renderer, &owner, &mut messages);
    assert!(messages.is_empty());
  }
}
