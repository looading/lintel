use super::{
  model::{Description, MAX_NODES, Mailbox, Navigation, Snapshot, WindowState},
  widgets::SemanticState,
};
use crate::{
  desktop::accessibility::accesskit as ak,
  iced::{Rectangle, advanced::widget},
};
use std::{
  collections::{BTreeMap, BTreeSet},
  sync::{Arc, Weak, atomic::Ordering},
};

#[derive(Clone, Copy, Default)]
struct Viewport {
  offset: crate::iced::Vector,
  clip: Option<Rectangle>,
  hidden: bool,
}
impl Viewport {
  fn bounds(self, bounds: Rectangle) -> Option<Rectangle> {
    if self.hidden {
      return None;
    }
    let rect = Rectangle {
      x: bounds.x - self.offset.x,
      y: bounds.y - self.offset.y,
      ..bounds
    };
    let result = match self.clip {
      Some(clip) => rect.intersection(&clip),
      None => Some(rect),
    };
    result.filter(|rect| rect.width > 0.0 && rect.height > 0.0)
  }
  fn scroll(self, bounds: Rectangle, translation: crate::iced::Vector) -> Self {
    let clip = self.bounds(bounds);
    Self {
      offset: self.offset + translation,
      clip,
      hidden: self.hidden || clip.is_none(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::iced::{Point, Size, Vector};
  fn rect(x: f32, y: f32, w: f32, h: f32) -> Rectangle {
    Rectangle::new(Point::new(x, y), Size::new(w, h))
  }
  #[test]
  fn nested_scroll_transforms_accumulate_and_clip_to_each_viewport() {
    let outer = Viewport::default().scroll(rect(10.0, 20.0, 100.0, 100.0), Vector::new(0.0, 50.0));
    assert_eq!(
      outer.bounds(rect(10.0, 70.0, 40.0, 20.0)),
      Some(rect(10.0, 20.0, 40.0, 20.0))
    );
    assert!(outer.bounds(rect(10.0, 10.0, 10.0, 10.0)).is_none());
    let inner = outer.scroll(rect(10.0, 80.0, 80.0, 40.0), Vector::new(0.0, 10.0));
    assert_eq!(
      inner.bounds(rect(10.0, 95.0, 20.0, 20.0)),
      Some(rect(10.0, 35.0, 20.0, 20.0))
    );
    assert!(inner.bounds(rect(10.0, 200.0, 20.0, 10.0)).is_none());
    let hidden = outer.scroll(rect(300.0, 300.0, 10.0, 10.0), Vector::ZERO);
    assert!(hidden.bounds(rect(0.0, 0.0, 1000.0, 1000.0)).is_none());
  }
}

struct Entry {
  id: ak::NodeId,
  description: Description,
  bounds: Rectangle,
  mailbox: Option<Weak<Mailbox>>,
}
pub(crate) struct Collect {
  pub state: Arc<WindowState>,
  pub window: Weak<winit::window::Window>,
  pub title: String,
  entries: Vec<Entry>,
  skip_children: bool,
  text_index: u64,
  viewport: Viewport,
  pending_viewport: Option<Viewport>,
  modal: bool,
  purge_actions: bool,
  menus: Vec<super::menu::MenuTree>,
  skip_menu_labels: usize,
}
impl Collect {
  pub fn new(state: Arc<WindowState>, window: Weak<winit::window::Window>, title: String) -> Self {
    Self {
      state,
      window,
      title,
      entries: Vec::new(),
      skip_children: false,
      text_index: 0,
      viewport: Viewport::default(),
      pending_viewport: None,
      modal: false,
      purge_actions: false,
      menus: Vec::new(),
      skip_menu_labels: 0,
    }
  }
  fn menu_tree(&mut self, mut tree: super::menu::MenuTree) {
    let scale = self.window.upgrade().map(|window| window.scale_factor()).unwrap_or(1.0);
    // Header layout follows ancestor widget translations; popup layouts are already window-local.
    for node in &mut tree.nodes {
      let mut bounds = node.bounds;
      if node.space == super::menu::Space::Widget {
        bounds.x -= self.viewport.offset.x;
        bounds.y -= self.viewport.offset.y;
      }
      node.bounds = bounds;
      node.space = super::menu::Space::Window;
      if bounds.width > 0.0 && bounds.height > 0.0 {
        node.node.set_bounds(ak::Rect::new(
          bounds.x as f64 * scale,
          bounds.y as f64 * scale,
          (bounds.x + bounds.width) as f64 * scale,
          (bounds.y + bounds.height) as f64 * scale,
        ));
      } else {
        node.node.clear_bounds();
      }
    }
    let count = self
      .menus
      .iter()
      .filter(|old| old.source != tree.source)
      .map(|old| old.nodes.len())
      .sum::<usize>();
    if count + tree.nodes.len() > MAX_NODES {
      return;
    }
    if let Some(old) = self.menus.iter_mut().find(|old| old.source == tree.source) {
      *old = tree;
    } else {
      self.menus.push(tree);
    }
  }
}
impl widget::Operation for Collect {
  fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn widget::Operation)) {
    if self.skip_children {
      self.skip_children = false;
      return;
    }
    let old = self.viewport;
    if let Some(viewport) = self.pending_viewport.take() {
      self.viewport = viewport;
    }
    operate(self);
    self.viewport = old;
  }
  fn scrollable(
    &mut self,
    _id: Option<&widget::Id>,
    bounds: Rectangle,
    _content: Rectangle,
    translation: crate::iced::Vector,
    _state: &mut dyn widget::operation::Scrollable,
  ) {
    self.pending_viewport = Some(self.viewport.scroll(bounds, translation));
  }
  fn custom(&mut self, _id: Option<&widget::Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
    if let Some(overlay) = state.downcast_mut::<super::menu::OverlayTree>() {
      if let Some(tree) = overlay.tree.clone() {
        self.menu_tree(tree);
      }
      return;
    }
    if let Some(menu) = state.downcast_mut::<crate::menu::MenuState>() {
      // Read hidden body semantics only for weak resume navigation. All other widget
      // operations remain blocked by the menu's one-shot modal collection barrier.
      menu.allow_modal_collection();
      let owner = Arc::downgrade(&self.state);
      let source = menu.modal_source_id();
      menu.set_modal_barrier(Arc::new(move |open| {
        if let Some(state) = owner.upgrade() {
          state.menu_modal_for(source, open);
        }
      }));
      self.modal |= menu.is_open();
      self.purge_actions |= menu.take_modal_boundary();
      if let Some(tree) = menu.accessibility_tree() {
        if let Some(mailbox) = tree.mailbox.upgrade() {
          mailbox.bind(&self.state);
        }
        self.skip_menu_labels = tree
          .nodes
          .iter()
          .filter(|node| node.path.len() == 1 && node.kind != super::menu::Kind::Popup)
          .count();
        self.menu_tree(tree);
      }
      return;
    }
    let Some(semantic) = state.downcast_mut::<SemanticState>() else {
      return;
    };
    *semantic.mailbox.owner.lock().unwrap() = Arc::downgrade(&self.state);
    if let Some(bounds) = self.viewport.bounds(bounds).filter(|_| self.entries.len() < MAX_NODES) {
      self.entries.push(Entry {
        id: semantic.mailbox.id,
        description: semantic.mailbox.description.lock().unwrap().clone(),
        bounds,
        mailbox: Some(Arc::downgrade(&semantic.mailbox)),
      });
    }
    // The helper declares its own label/value; do not duplicate its internal text content.
    self.skip_children = true;
  }
  fn text(&mut self, _id: Option<&widget::Id>, bounds: Rectangle, text: &str) {
    if self.skip_menu_labels != 0 {
      self.skip_menu_labels -= 1;
      return;
    }
    if self.entries.len() >= MAX_NODES || text.is_empty() {
      return;
    }
    self.text_index += 1;
    let Some(bounds) = self.viewport.bounds(bounds) else {
      return;
    };
    self.entries.push(Entry {
      id: ak::NodeId((1_u64 << 63) | self.text_index),
      description: Description {
        key: format!("__text_{}", self.text_index),
        label: text.into(),
        role: ak::Role::Label,
        value: None,
        checked: None,
        enabled: false,
        actionable: false,
      },
      bounds,
      mailbox: None,
    });
  }
  fn finish(&self) -> widget::operation::Outcome<()> {
    if self.state.closed.load(Ordering::Acquire) {
      return widget::operation::Outcome::None;
    }
    let Some(window) = self.window.upgrade() else {
      return widget::operation::Outcome::None;
    };
    let scale = window.scale_factor();
    let size = window.inner_size();
    let viewport = Rectangle::new(
      crate::iced::Point::ORIGIN,
      crate::iced::Size::new((size.width as f64 / scale) as f32, (size.height as f64 / scale) as f32),
    );
    let mut keys = BTreeSet::new();
    let mut duplicate = false;
    let mut routes = BTreeMap::new();
    let mut order = Vec::new();
    let mut nodes = Vec::new();
    let mut signature = Vec::new();
    let body_limit = MAX_NODES.saturating_sub(self.menus.iter().map(|tree| tree.nodes.len()).sum::<usize>());
    for entry in self.entries.iter().take(body_limit) {
      let Some(bounds) = entry.bounds.intersection(&viewport) else {
        continue;
      };
      if entry.description.key.is_empty() || entry.description.key.len() > 256 || !keys.insert(&entry.description.key) {
        duplicate = true;
        break;
      }
      let rect = [
        bounds.x as f64 * scale,
        bounds.y as f64 * scale,
        (bounds.x + bounds.width) as f64 * scale,
        (bounds.y + bounds.height) as f64 * scale,
      ];
      let mut node = ak::Node::new(entry.description.role);
      node.set_label(entry.description.label.clone());
      node.set_bounds(ak::Rect::new(rect[0], rect[1], rect[2], rect[3]));
      if !entry.description.enabled && entry.description.role != ak::Role::Label {
        node.set_disabled();
      }
      if let Some(value) = &entry.description.value {
        node.set_value(value.clone());
      }
      if let Some(checked) = entry.description.checked {
        node.set_toggled(if checked { ak::Toggled::True } else { ak::Toggled::False });
      }
      if entry.description.focusable() {
        node.add_action(ak::Action::Focus);
        order.push(entry.id);
        if matches!(entry.description.role, ak::Role::Button | ak::Role::CheckBox) {
          node.add_action(ak::Action::Click);
        }
        if matches!(entry.description.role, ak::Role::TextInput | ak::Role::PasswordInput) {
          node.add_action(ak::Action::SetValue);
        }
      }
      if let Some(mailbox) = &entry.mailbox {
        routes.insert(entry.id, mailbox.clone());
      }
      signature.push((entry.id.0, entry.description.clone(), rect));
      nodes.push((entry.id, node));
    }
    if duplicate {
      tracing::error!("accessibility keys must be nonempty, distinct per window, and at most 256 bytes");
      nodes.clear();
      routes.clear();
      order.clear();
      signature.clear();
    }
    let focus_token = self.state.focus_token();
    let focused = ak::NodeId(focus_token.0);
    let focused = if self.modal {
      // 菜单期间对外焦点为窗口，保留原 token 以便关闭菜单后恢复输入焦点。
      ak::NodeId(0)
    } else if order.contains(&focused) {
      focused
    } else {
      ak::NodeId(0)
    };
    let mut root = ak::Node::new(ak::Role::Window);
    root.set_label(self.title.clone());
    root.set_children(nodes.iter().map(|(id, _)| *id).collect::<Vec<_>>());
    root.set_bounds(ak::Rect::new(0.0, 0.0, size.width as f64, size.height as f64));
    nodes.insert(0, (ak::NodeId(0), root));
    let mut update = ak::TreeUpdate {
      nodes,
      tree: Some(ak::TreeInfo::new(ak::NodeId(0))),
      tree_id: ak::TreeId::ROOT,
      focus: focused,
    };
    signature.insert(
      0,
      (
        0,
        Description {
          key: "__window".into(),
          label: self.title.clone(),
          role: ak::Role::Window,
          value: None,
          checked: None,
          enabled: false,
          actionable: false,
        },
        [0.0, 0.0, size.width as f64, size.height as f64],
      ),
    );
    let resume = if self.modal {
      update.nodes.retain(|(id, _)| *id == ak::NodeId(0));
      for (_, root) in &mut update.nodes {
        root.set_children(Vec::<ak::NodeId>::new());
      }
      signature.retain(|(id, _, _)| *id == 0);
      Some(Navigation::new(std::mem::take(&mut routes), std::mem::take(&mut order)))
    } else {
      None
    };
    let changed = self.state.publish(
      Snapshot {
        update,
        routes,
        order,
        signature,
        modal: self.modal,
        resume,
        menus: self.menus.clone(),
      },
      self.purge_actions,
    );
    if changed {
      self.state.generation.fetch_add(1, Ordering::Release);
      (self.state.wake)();
    }
    if self.purge_actions {
      // 同批次开关菜单也需要在清理后重新分发 redraw，恢复暂停的内部控件。
      (self.state.wake)();
    }
    widget::operation::Outcome::None
  }
}
