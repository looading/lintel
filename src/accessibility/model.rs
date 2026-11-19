use std::{
  collections::{BTreeMap, BTreeSet, VecDeque},
  sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
  },
};

use super::menu::MenuTree;
use crate::desktop::accessibility::accesskit::{self as ak, Action, ActionData, ActionRequest, NodeId};

pub(crate) const MAX_NODES: usize = 4096;
const MAX_ACTIONS: usize = 64;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
pub(crate) fn allocate_node_id() -> NodeId {
  let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
  assert!(id != 0 && id < (1_u64 << 63), "accessibility node identity exhausted");
  NodeId(id)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Description {
  pub key: String,
  pub label: String,
  pub role: ak::Role,
  pub value: Option<String>,
  pub checked: Option<bool>,
  pub enabled: bool,
  pub actionable: bool,
}
impl Description {
  pub fn focusable(&self) -> bool {
    self.enabled
      && self.actionable
      && matches!(
        self.role,
        ak::Role::Button | ak::Role::CheckBox | ak::Role::TextInput | ak::Role::PasswordInput
      )
  }
  pub fn accepts(&self, action: &SemanticAction) -> bool {
    self.focusable()
      && !matches!(action, SemanticAction::SetValue(value) if value.len() > 64 * 1024)
      && matches!(
        (self.role, action),
        (ak::Role::Button | ak::Role::CheckBox, SemanticAction::Click)
          | (
            ak::Role::TextInput | ak::Role::PasswordInput,
            SemanticAction::SetValue(_)
          )
      )
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SemanticAction {
  Click,
  SetValue(String),
}

pub(crate) struct Mailbox {
  pub id: NodeId,
  pub description: Mutex<Description>,
  actions: Mutex<VecDeque<SemanticAction>>,
  pub owner: Mutex<Weak<WindowState>>,
}
impl Mailbox {
  pub fn new(description: Description) -> Arc<Self> {
    let id = allocate_node_id();
    Arc::new(Self {
      id,
      description: Mutex::new(description),
      actions: Mutex::new(VecDeque::new()),
      owner: Mutex::new(Weak::new()),
    })
  }
  pub fn drain(&self) -> Vec<SemanticAction> {
    self.actions.lock().unwrap().drain(..).collect()
  }
  pub fn focus(&self) {
    let owner = { self.owner.lock().unwrap().upgrade() };
    if let Some(owner) = owner {
      owner.focus(self.id);
    }
  }
  pub fn focused(&self) -> bool {
    self
      .owner
      .lock()
      .unwrap()
      .upgrade()
      .is_some_and(|owner| owner.focus_token().0 == self.id.0)
  }
}

#[derive(Default)]
pub(crate) struct Navigation {
  routes: BTreeMap<NodeId, Weak<Mailbox>>,
  order: Vec<NodeId>,
}
impl Navigation {
  pub fn new(routes: BTreeMap<NodeId, Weak<Mailbox>>, order: Vec<NodeId>) -> Self {
    Self { routes, order }
  }
}

pub(crate) struct Snapshot {
  pub update: ak::TreeUpdate,
  pub routes: BTreeMap<NodeId, Weak<Mailbox>>,
  pub order: Vec<NodeId>,
  pub signature: Vec<(u64, Description, [f64; 4])>,
  pub modal: bool,
  pub resume: Option<Navigation>,
  pub menus: Vec<MenuTree>,
}
impl Snapshot {
  fn purge_actions(&self) {
    for mailbox in self
      .routes
      .values()
      .chain(self.resume.iter().flat_map(|navigation| navigation.routes.values()))
      .filter_map(Weak::upgrade)
    {
      mailbox.drain();
    }
  }
  fn suspend(&mut self) {
    if self.resume.is_none() {
      self.resume = Some(Navigation::new(
        std::mem::take(&mut self.routes),
        std::mem::take(&mut self.order),
      ));
    }
    self.routes.clear();
    self.order.clear();
    self.modal = true;
    self.update.nodes.retain(|(id, _)| *id == NodeId(0));
    for (_, root) in &mut self.update.nodes {
      root.set_children(Vec::<NodeId>::new());
    }
    self.update.focus = NodeId(0);
    self.signature.retain(|(id, _, _)| *id == 0);
  }
  fn append_menus(&mut self, active: Option<&BTreeSet<u64>>) {
    let trees = self
      .menus
      .iter()
      .filter(|tree| active.is_none_or(|sources| sources.contains(&tree.source)))
      .collect::<Vec<_>>();
    let roots = trees.iter().map(|tree| tree.root).collect::<Vec<_>>();
    if let Some((_, root)) = self.update.nodes.iter_mut().find(|(id, _)| *id == NodeId(0)) {
      let mut children = roots;
      children.extend(root.children().iter().copied());
      root.set_children(children);
    }
    for tree in &trees {
      self
        .update
        .nodes
        .extend(tree.nodes.iter().map(|node| (node.id, node.node.clone())));
    }
    if active.is_some() {
      self.update.focus = trees
        .iter()
        .filter_map(|tree| tree.focus.map(|focus| (tree.focus_stamp, focus)))
        .max_by_key(|(stamp, _)| *stamp)
        .map(|(_, focus)| focus)
        .unwrap_or(NodeId(0));
    }
  }
  fn menu_owner(&self, id: NodeId) -> Option<Arc<super::menu::Mailbox>> {
    if !self.update.nodes.iter().any(|(node, _)| *node == id) {
      return None;
    }
    self
      .menus
      .iter()
      .find(|tree| tree.nodes.iter().any(|node| node.id == id))
      .and_then(|tree| tree.mailbox.upgrade())
  }
  fn resume_navigation(&mut self) {
    let mut navigation = self.resume.take().unwrap_or_default();
    navigation.routes.retain(|_, mailbox| {
      mailbox
        .upgrade()
        .is_some_and(|mailbox| mailbox.description.lock().unwrap().focusable())
    });
    navigation.order.retain(|id| navigation.routes.contains_key(id));
    self.routes = navigation.routes;
    self.order = navigation.order;
    self.modal = false;
    // The native tree remains root-only until a new nonmodal collection publishes its nodes.
  }
}
pub(crate) struct WindowState {
  pub snapshot: Mutex<Option<Snapshot>>,
  focus: Mutex<(u64, u64)>,
  pub closed: AtomicBool,
  pub wake: Arc<dyn Fn() + Send + Sync>,
  pub generation: AtomicU64,
  modal: AtomicBool,
  // Accessed only while the snapshot lock is held: source membership and admission are one transaction.
  active_menus: Mutex<BTreeSet<u64>>,
}
impl WindowState {
  pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
    Arc::new(Self {
      snapshot: Mutex::new(None),
      focus: Mutex::new((0, 0)),
      closed: AtomicBool::new(false),
      wake: Arc::new(wake),
      generation: AtomicU64::new(0),
      modal: AtomicBool::new(false),
      active_menus: Mutex::new(BTreeSet::new()),
    })
  }
  pub fn tree(&self) -> Option<ak::TreeUpdate> {
    self
      .snapshot
      .lock()
      .unwrap()
      .as_ref()
      .map(|snapshot| snapshot.update.clone())
  }
  /// Synchronously changes admission before the menu returns control to ordinary input.
  #[cfg(test)]
  pub fn menu_modal(&self, open: bool) {
    self.menu_modal_for(0, open);
  }
  pub fn menu_modal_for(&self, source: u64, open: bool) {
    let changed = {
      let mut current = self.snapshot.lock().unwrap();
      if self.closed.load(Ordering::Acquire) {
        return;
      }
      let mut active = self.active_menus.lock().unwrap();
      let source_changed = if open {
        active.insert(source)
      } else {
        active.remove(&source)
      };
      if !source_changed {
        return;
      }
      let modal = !active.is_empty();
      let mode_changed = self.modal.swap(modal, Ordering::AcqRel) != modal;
      if let Some(snapshot) = current.as_mut() {
        if !open {
          for tree in snapshot.menus.iter_mut().filter(|tree| tree.source == source) {
            if let Some(mailbox) = tree.mailbox.upgrade() {
              mailbox.clear_requests();
            }
            tree.closed();
          }
        }
        if mode_changed {
          snapshot.purge_actions();
        }
        if modal {
          for mailbox in snapshot
            .menus
            .iter()
            .filter(|tree| !active.contains(&tree.source))
            .filter_map(|tree| tree.mailbox.upgrade())
          {
            mailbox.clear_requests();
          }
        }
        if modal {
          snapshot.suspend();
          snapshot.append_menus(Some(&active));
        } else if snapshot.modal {
          snapshot.resume_navigation();
          snapshot.update.nodes.retain(|(id, _)| *id == NodeId(0));
          for (_, root) in &mut snapshot.update.nodes {
            root.set_children(Vec::<NodeId>::new());
          }
          snapshot.append_menus(None);
          let mut focus = self.focus.lock().unwrap();
          if focus.0 != 0 && !snapshot.order.contains(&NodeId(focus.0)) {
            focus.0 = 0;
            focus.1 = focus.1.wrapping_add(1);
          }
        }
      }
      true
    };
    if changed {
      self.generation.fetch_add(1, Ordering::Release);
      (self.wake)();
    }
  }
  pub fn publish(&self, mut snapshot: Snapshot, purge_actions: bool) -> bool {
    let mut previous = self.snapshot.lock().unwrap();
    if self.closed.load(Ordering::Acquire) || snapshot.modal != self.modal.load(Ordering::Acquire) {
      return false;
    }
    // Admission, boundary purging and publication share this lock. A stale Collect cannot
    // reopen/close admission or discard actions accepted after the synchronous menu close.
    if snapshot.modal || purge_actions {
      if let Some(old) = previous.as_ref() {
        old.purge_actions();
      }
    }
    if snapshot.modal {
      if snapshot.resume.is_none() {
        snapshot.resume = previous.as_mut().and_then(|old| old.resume.take());
      }
      snapshot.suspend();
      snapshot.append_menus(Some(&self.active_menus.lock().unwrap()));
    } else {
      snapshot.resume = None;
      let focused = NodeId(self.focus.lock().unwrap().0);
      let eligible = snapshot.order.contains(&focused)
        && snapshot
          .routes
          .get(&focused)
          .and_then(Weak::upgrade)
          .is_some_and(|mailbox| mailbox.description.lock().unwrap().focusable());
      let mut focus = self.focus.lock().unwrap();
      if eligible {
        snapshot.update.focus = focused;
      } else {
        snapshot.update.focus = NodeId(0);
        if focus.0 != 0 {
          focus.0 = 0;
          focus.1 = focus.1.wrapping_add(1);
        }
      }
    }
    if !snapshot.modal {
      snapshot.append_menus(None);
    }
    let changed = previous.as_ref().is_none_or(|old| {
      old.signature != snapshot.signature
        || old.update.nodes != snapshot.update.nodes
        || old.update.focus != snapshot.update.focus
        || old.modal != snapshot.modal
    });
    *previous = Some(snapshot);
    changed
  }
  pub fn focus(&self, id: NodeId) {
    {
      let snapshot = self.snapshot.lock().unwrap();
      if self.closed.load(Ordering::Acquire) || self.modal.load(Ordering::Acquire) {
        return;
      }
      let mailbox = snapshot
        .as_ref()
        .filter(|snapshot| !snapshot.modal)
        .and_then(|snapshot| snapshot.routes.get(&id))
        .and_then(Weak::upgrade);
      if !mailbox
        .as_ref()
        .is_some_and(|mailbox| mailbox.description.lock().unwrap().focusable())
      {
        return;
      }
      let mut focus = self.focus.lock().unwrap();
      focus.0 = id.0;
      focus.1 = focus.1.wrapping_add(1);
    }
    (self.wake)();
  }
  pub fn advance(&self, reverse: bool) -> bool {
    {
      let snapshot = self.snapshot.lock().unwrap();
      if self.closed.load(Ordering::Acquire) || self.modal.load(Ordering::Acquire) {
        return false;
      }
      let Some(snapshot) = snapshot.as_ref().filter(|snapshot| !snapshot.modal) else {
        return false;
      };
      let ids: Vec<_> = snapshot
        .order
        .iter()
        .filter(|id| {
          snapshot
            .routes
            .get(id)
            .and_then(Weak::upgrade)
            .is_some_and(|mailbox| mailbox.description.lock().unwrap().focusable())
        })
        .copied()
        .collect();
      if ids.is_empty() {
        return false;
      }
      let mut focus = self.focus.lock().unwrap();
      let position = ids.iter().position(|id| id.0 == focus.0);
      let index = match (position, reverse) {
        (Some(0) | None, true) => ids.len() - 1,
        (Some(index), true) => index - 1,
        (Some(index), false) => (index + 1) % ids.len(),
        (None, false) => 0,
      };
      focus.0 = ids[index].0;
      focus.1 = focus.1.wrapping_add(1);
    }
    (self.wake)();
    true
  }
  pub(crate) fn menu_request_allowed(&self, source: u64, node: NodeId) -> bool {
    let snapshot = self.snapshot.lock().unwrap();
    !self.closed.load(Ordering::Acquire)
      && (!self.modal.load(Ordering::Acquire) || self.active_menus.lock().unwrap().contains(&source))
      && snapshot.as_ref().is_some_and(|snapshot| {
        snapshot
          .menu_owner(node)
          .is_some_and(|mailbox| mailbox.source == source)
      })
  }
  pub fn action(&self, request: ActionRequest) {
    if request.target_tree != ak::TreeId::ROOT {
      return;
    }
    {
      let snapshot = self.snapshot.lock().unwrap();
      if self.closed.load(Ordering::Acquire) {
        return;
      }
      if let Some(mailbox) = snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.menu_owner(request.target_node))
      {
        if self.modal.load(Ordering::Acquire) && !self.active_menus.lock().unwrap().contains(&mailbox.source) {
          return;
        }
        let accepted = mailbox.enqueue(request.target_node, request.action);
        drop(snapshot);
        crate::menu::input_trace(
          "native_menu_enqueue",
          format_args!(
            "source={} node={} action={:?} accepted={accepted}",
            mailbox.source, request.target_node.0, request.action
          ),
        );
        if accepted {
          mailbox.wake();
        }
        return;
      }
    }

    if request.action == Action::Focus {
      self.focus(request.target_node);
      return;
    }
    {
      let snapshot = self.snapshot.lock().unwrap();
      if self.closed.load(Ordering::Acquire) || self.modal.load(Ordering::Acquire) {
        return;
      }
      let mailbox = snapshot
        .as_ref()
        .filter(|snapshot| !snapshot.modal)
        .and_then(|snapshot| snapshot.routes.get(&request.target_node))
        .and_then(Weak::upgrade);
      let Some(mailbox) = mailbox else {
        return;
      };
      let description = mailbox.description.lock().unwrap().clone();
      if !description.focusable() {
        return;
      }
      let action = match (request.action, request.data) {
        (Action::Click, _) if matches!(description.role, ak::Role::Button | ak::Role::CheckBox) => {
          SemanticAction::Click
        }
        (Action::SetValue, Some(ActionData::Value(value)))
          if matches!(description.role, ak::Role::TextInput | ak::Role::PasswordInput) && value.len() <= 64 * 1024 =>
        {
          SemanticAction::SetValue(value.into())
        }
        _ => return,
      };
      let mut actions = mailbox.actions.lock().unwrap();
      if actions.len() == MAX_ACTIONS {
        return;
      }
      actions.push_back(action);
      let mut focus = self.focus.lock().unwrap();
      focus.0 = mailbox.id.0;
      focus.1 = focus.1.wrapping_add(1);
    }
    (self.wake)();
  }
  pub fn close(&self) {
    let mut snapshot = self.snapshot.lock().unwrap();
    self.closed.store(true, Ordering::Release);
    self.active_menus.lock().unwrap().clear();
    self.modal.store(false, Ordering::Release);
    if let Some(snapshot) = snapshot.as_ref() {
      snapshot.purge_actions();
      for mailbox in snapshot.menus.iter().filter_map(|tree| tree.mailbox.upgrade()) {
        mailbox.close();
      }
    }
    snapshot.take();
    let mut focus = self.focus.lock().unwrap();
    focus.0 = 0;
    focus.1 = focus.1.wrapping_add(1);
  }
  pub fn focus_token(&self) -> (u64, u64) {
    *self.focus.lock().unwrap()
  }
  pub fn clear_focus(&self, id: NodeId, revision: u64) {
    let changed = {
      let _snapshot = self.snapshot.lock().unwrap();
      if self.closed.load(Ordering::Acquire) || self.modal.load(Ordering::Acquire) {
        return;
      }
      let mut focus = self.focus.lock().unwrap();
      if *focus == (id.0, revision) {
        focus.0 = 0;
        focus.1 = focus.1.wrapping_add(1);
        true
      } else {
        false
      }
    };
    if changed {
      (self.wake)();
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::AtomicUsize;
  fn description(key: &str) -> Description {
    Description {
      key: key.into(),
      label: key.into(),
      role: ak::Role::Button,
      value: None,
      checked: None,
      enabled: true,
      actionable: true,
    }
  }
  fn install(state: &Arc<WindowState>, boxes: &[Arc<Mailbox>]) {
    *state.snapshot.lock().unwrap() = Some(snapshot(boxes));
  }
  fn snapshot(boxes: &[Arc<Mailbox>]) -> Snapshot {
    let root = ak::Node::new(ak::Role::Window);
    Snapshot {
      update: ak::TreeUpdate {
        nodes: vec![(NodeId(0), root)],
        tree: Some(ak::TreeInfo::new(NodeId(0))),
        tree_id: ak::TreeId::ROOT,
        focus: NodeId(0),
      },
      routes: boxes
        .iter()
        .map(|mailbox| (mailbox.id, Arc::downgrade(mailbox)))
        .collect(),
      order: boxes.iter().map(|mailbox| mailbox.id).collect(),
      signature: vec![],
      modal: false,
      resume: None,
      menus: Vec::new(),
    }
  }
  fn request(id: NodeId, action: Action) -> ActionRequest {
    ActionRequest {
      target_tree: ak::TreeId::ROOT,
      target_node: id,
      action,
      data: None,
    }
  }

  #[test]
  fn actions_reject_disabled_removed_and_closed_nodes_and_are_bounded() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let count = wakes.clone();
    let state = WindowState::new(move || {
      count.fetch_add(1, Ordering::Relaxed);
    });
    let mailbox = Mailbox::new(description("save"));
    install(&state, &[mailbox.clone()]);
    mailbox.description.lock().unwrap().enabled = false;
    state.action(request(mailbox.id, Action::Click));
    assert!(mailbox.drain().is_empty());
    mailbox.description.lock().unwrap().enabled = true;
    for _ in 0..MAX_ACTIONS + 1 {
      state.action(request(mailbox.id, Action::Click));
    }
    assert_eq!(mailbox.drain().len(), MAX_ACTIONS);
    assert_eq!(wakes.load(Ordering::Relaxed), MAX_ACTIONS);
    state.close();
    state.action(request(mailbox.id, Action::Click));
    assert!(mailbox.drain().is_empty());
  }

  #[test]
  fn traversal_handles_reverse_wrap_and_stale_or_disabled_entries() {
    let state = WindowState::new(|| {});
    let a = Mailbox::new(description("a"));
    let b = Mailbox::new(description("b"));
    install(&state, &[a.clone(), b.clone()]);
    assert!(state.advance(true));
    assert_eq!(state.focus_token().0, b.id.0);
    assert!(state.advance(false));
    assert_eq!(state.focus_token().0, a.id.0);
    a.description.lock().unwrap().enabled = false;
    drop(b);
    assert!(!state.advance(false));
  }

  #[test]
  fn repeated_focus_requests_have_a_new_revision_and_blur_cannot_erase_a_new_target() {
    let state = WindowState::new(|| {});
    let a = Mailbox::new(description("a"));
    let b = Mailbox::new(description("b"));
    install(&state, &[a.clone(), b.clone()]);
    state.focus(a.id);
    let first = state.focus_token().1;
    state.focus(a.id);
    assert!(state.focus_token().1 > first);
    state.clear_focus(a.id, first);
    assert_eq!(
      state.focus_token().0,
      a.id.0,
      "a newer same-target request must survive old blur"
    );
    state.focus(b.id);
    state.clear_focus(a.id, first);
    assert_eq!(state.focus_token().0, b.id.0);
    state.clear_focus(b.id, state.focus_token().1);
    assert_eq!(state.focus_token().0, 0);
  }

  #[test]
  fn only_supported_value_actions_are_delivered_without_retaining_removed_widgets() {
    let state = WindowState::new(|| {});
    let mut data = description("name");
    data.role = ak::Role::TextInput;
    let mailbox = Mailbox::new(data);
    install(&state, &[mailbox.clone()]);
    state.action(request(mailbox.id, Action::Click));
    assert!(mailbox.drain().is_empty());
    let mut value = request(mailbox.id, Action::SetValue);
    value.data = Some(ActionData::Value("hello".into()));
    state.action(value);
    assert!(matches!(mailbox.drain().as_slice(),[SemanticAction::SetValue(value)] if value=="hello"));
    let weak = Arc::downgrade(&mailbox);
    let old = mailbox.id;
    drop(mailbox);
    assert!(weak.upgrade().is_none());
    state.action(request(old, Action::Click));
  }

  #[test]
  fn removing_custom_action_handler_revokes_actions_and_keyboard_focus() {
    let state = WindowState::new(|| {});
    let mailbox = Mailbox::new(description("custom"));
    install(&state, &[mailbox.clone()]);
    state.focus(mailbox.id);
    mailbox.description.lock().unwrap().actionable = false;
    state.action(request(mailbox.id, Action::Click));
    assert!(mailbox.drain().is_empty());
    assert!(!state.advance(false));
    assert!(!mailbox.description.lock().unwrap().focusable());
  }

  #[test]
  fn modal_publication_purges_old_actions_blocks_background_and_retains_resume_focus() {
    let state = WindowState::new(|| {});
    let mailbox = Mailbox::new(description("background"));
    install(&state, &[mailbox.clone()]);
    state.action(request(mailbox.id, Action::Click));
    let original_focus = state.focus_token();
    let mut modal = snapshot(&[]);
    modal.modal = true;
    state.menu_modal(true);
    state.publish(modal, false);
    assert!(mailbox.drain().is_empty());
    state.action(request(mailbox.id, Action::Click));
    state.focus(mailbox.id);
    assert!(mailbox.drain().is_empty());
    assert_eq!(state.focus_token(), original_focus);
    assert!(!state.advance(false));
    state.menu_modal(false);
    assert!(state.publish(snapshot(&[mailbox.clone()]), false));
    assert_eq!(state.focus_token(), original_focus);
    state.action(request(mailbox.id, Action::Click));
    assert_eq!(mailbox.drain(), vec![SemanticAction::Click]);
  }

  #[test]
  fn opening_and_closing_before_collection_still_discards_old_background_actions() {
    let state = WindowState::new(|| {});
    let mailbox = Mailbox::new(description("background"));
    install(&state, &[mailbox.clone()]);
    state.action(request(mailbox.id, Action::Click));
    let focus = state.focus_token();
    // 菜单最终已关闭，sticky 边界仍要求清掉旧 snapshot 的待执行动作。
    state.publish(snapshot(&[mailbox.clone()]), true);
    assert!(mailbox.drain().is_empty());
    assert_eq!(state.focus_token(), focus);
    state.action(request(mailbox.id, Action::Click));
    assert_eq!(mailbox.drain(), vec![SemanticAction::Click]);
    state.close();
    assert!(!state.publish(snapshot(&[mailbox]), true));
    assert!(state.tree().is_none());
  }
  #[test]
  fn synchronous_menu_close_restores_navigation_and_actions_before_the_next_collection() {
    let state = WindowState::new(|| {});
    let a = Mailbox::new(description("a"));
    let b = Mailbox::new(description("b"));
    *a.owner.lock().unwrap() = Arc::downgrade(&state);
    *b.owner.lock().unwrap() = Arc::downgrade(&state);
    install(&state, &[a.clone(), b.clone()]);
    state.action(request(a.id, Action::Click));
    let token = state.focus_token();
    state.menu_modal(true);
    assert!(a.drain().is_empty());
    assert_eq!(state.tree().unwrap().focus, NodeId(0));
    assert!(state.tree().unwrap().nodes.iter().all(|(id, _)| *id == NodeId(0)));
    state.action(request(b.id, Action::Focus));
    state.action(request(b.id, Action::Click));
    state.clear_focus(a.id, token.1);
    assert!(!state.advance(false));
    assert_eq!(state.focus_token(), token);
    assert!(b.drain().is_empty());
    let mut root_only = snapshot(&[]);
    root_only.modal = true;
    state.publish(root_only, false);
    state.menu_modal(false);
    assert_eq!(state.focus_token(), token);
    assert_eq!(
      state.tree().unwrap().focus,
      NodeId(0),
      "tree awaits the next Collect while admission is already restored"
    );
    assert!(state.advance(false));
    assert_eq!(state.focus_token().0, b.id.0);
    state.action(request(a.id, Action::Focus));
    assert_eq!(state.focus_token().0, a.id.0);
    state.action(request(a.id, Action::Click));
    state.publish(snapshot(&[a.clone(), b]), false);
    assert_eq!(
      a.drain(),
      vec![SemanticAction::Click],
      "a post-close action survives the next Collect"
    );
  }

  #[test]
  fn stale_opposite_modal_snapshots_cannot_change_admission_or_purge_new_actions() {
    let state = WindowState::new(|| {});
    let mailbox = Mailbox::new(description("background"));
    install(&state, &[mailbox.clone()]);
    state.menu_modal(true);
    assert!(!state.publish(snapshot(&[mailbox.clone()]), true));
    state.action(request(mailbox.id, Action::Click));
    assert!(mailbox.drain().is_empty());
    state.menu_modal(false);
    state.action(request(mailbox.id, Action::Click));
    let mut stale = snapshot(&[]);
    stale.modal = true;
    assert!(!state.publish(stale, true));
    assert_eq!(mailbox.drain(), vec![SemanticAction::Click]);
    assert!(state.advance(false));
    assert_eq!(state.focus_token().0, mailbox.id.0);
  }

  #[test]
  fn modal_backups_are_weak_and_do_not_resume_disabled_removed_or_unhandled_nodes() {
    let state = WindowState::new(|| {});
    let live = Mailbox::new(description("live"));
    let disabled = Mailbox::new(description("disabled"));
    let removed = Mailbox::new(description("removed"));
    let unhandled = Mailbox::new(description("unhandled"));
    install(
      &state,
      &[live.clone(), disabled.clone(), removed.clone(), unhandled.clone()],
    );
    state.focus(disabled.id);
    state.menu_modal(true);
    disabled.description.lock().unwrap().enabled = false;
    unhandled.description.lock().unwrap().actionable = false;
    let weak = Arc::downgrade(&removed);
    let removed_id = removed.id;
    drop(removed);
    assert!(weak.upgrade().is_none());
    let mut root_only = snapshot(&[]);
    root_only.modal = true;
    state.publish(root_only, false);
    state.menu_modal(false);
    assert_eq!(state.focus_token().0, 0);
    assert!(state.advance(false));
    assert_eq!(state.focus_token().0, live.id.0);
    for id in [disabled.id, removed_id, unhandled.id] {
      state.action(request(id, Action::Click));
      state.focus(id);
      assert_eq!(state.focus_token().0, live.id.0);
    }
    assert!(disabled.drain().is_empty() && unhandled.drain().is_empty());
    state.close();
    state.menu_modal(true);
    state.menu_modal(false);
    state.action(request(live.id, Action::Click));
    assert!(live.drain().is_empty());
    assert!(!state.advance(false));
    assert!(state.tree().is_none());
    assert_eq!(state.focus_token().0, 0);
  }

  #[test]
  fn initially_modal_collection_can_cache_weak_background_routes_without_exposing_the_tree() {
    let state = WindowState::new(|| {});
    let mailbox = Mailbox::new(description("initial"));
    state.menu_modal(true);
    let mut initial = snapshot(&[]);
    initial.modal = true;
    initial.resume = Some(Navigation::new(
      [(mailbox.id, Arc::downgrade(&mailbox))].into(),
      vec![mailbox.id],
    ));
    state.publish(initial, false);
    assert!(!state.advance(false));
    assert_eq!(state.tree().unwrap().focus, NodeId(0));
    state.menu_modal(false);
    assert!(state.advance(false));
    assert_eq!(state.focus_token().0, mailbox.id.0);
    state.action(request(mailbox.id, Action::Click));
    assert_eq!(mailbox.drain(), vec![SemanticAction::Click]);
  }

  #[test]
  fn wake_can_reenter_state_and_mailbox_without_locks_or_strong_callback_cycles() {
    let state_slot = Arc::new(Mutex::new(Weak::<WindowState>::new()));
    let mailbox_slot = Arc::new(Mutex::new(Weak::<Mailbox>::new()));
    let state_ref = state_slot.clone();
    let mailbox_ref = mailbox_slot.clone();
    let wakes = Arc::new(AtomicUsize::new(0));
    let count = wakes.clone();
    let state = WindowState::new(move || {
      count.fetch_add(1, Ordering::Relaxed);
      if let Some(state) = state_ref.lock().unwrap().upgrade() {
        assert!(state.snapshot.try_lock().is_ok());
        assert!(state.focus.try_lock().is_ok());
        let _ = state.tree();
        let _ = state.focus_token();
      }
      if let Some(mailbox) = mailbox_ref.lock().unwrap().upgrade() {
        assert!(mailbox.owner.try_lock().is_ok());
        let _ = mailbox.focused();
      }
    });
    *state_slot.lock().unwrap() = Arc::downgrade(&state);
    let mailbox = Mailbox::new(description("reentry"));
    *mailbox.owner.lock().unwrap() = Arc::downgrade(&state);
    *mailbox_slot.lock().unwrap() = Arc::downgrade(&mailbox);
    install(&state, &[mailbox.clone()]);
    mailbox.focus();
    state.menu_modal(true);
    state.menu_modal(false);
    assert!(wakes.load(Ordering::Relaxed) >= 3);
    let weak = Arc::downgrade(&state);
    let callback: Arc<dyn Fn(bool) + Send + Sync> = Arc::new(move |open| {
      if let Some(state) = weak.upgrade() {
        state.menu_modal(open);
      }
    });
    drop(state);
    assert!(state_slot.lock().unwrap().upgrade().is_none());
    callback(false);
  }
  #[test]
  fn multiple_menu_sources_keep_admission_closed_until_every_source_closes() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let count = wakes.clone();
    let state = WindowState::new(move || {
      count.fetch_add(1, Ordering::Relaxed);
    });
    let mailbox = Mailbox::new(description("nested-body"));
    install(&state, &[mailbox.clone()]);
    state.focus(mailbox.id);
    let focus = state.focus_token();
    state.menu_modal_for(1, true);
    // The first collection discovers an already-open outer menu followed by a closed inner menu.
    state.menu_modal_for(2, false);
    let mut modal = snapshot(&[]);
    modal.modal = true;
    state.publish(modal, false);
    state.action(request(mailbox.id, Action::Click));
    assert!(mailbox.drain().is_empty());
    assert!(!state.advance(false));
    assert_eq!(state.focus_token(), focus);
    state.menu_modal_for(2, true);
    state.menu_modal_for(1, false);
    state.action(request(mailbox.id, Action::Click));
    assert!(mailbox.drain().is_empty());
    assert!(!state.publish(snapshot(&[mailbox.clone()]), true));
    assert!(!state.advance(false));
    assert_eq!(state.tree().unwrap().focus, NodeId(0));
    state.menu_modal_for(2, false);
    assert!(state.advance(false));
    state.action(request(mailbox.id, Action::Click));
    let before = wakes.load(Ordering::Relaxed);
    state.menu_modal_for(1, false);
    state.menu_modal_for(2, false);
    state.menu_modal_for(3, false);
    assert_eq!(
      wakes.load(Ordering::Relaxed),
      before,
      "closed/repeated sources are not a new boundary"
    );
    let mut stale = snapshot(&[]);
    stale.modal = true;
    assert!(!state.publish(stale, true));
    assert_eq!(
      mailbox.drain(),
      vec![SemanticAction::Click],
      "unrelated closed source cannot purge an admitted action"
    );
    state.close();
    state.menu_modal_for(1, true);
    assert!(state.tree().is_none());
    assert!(!state.advance(false));
  }
}
