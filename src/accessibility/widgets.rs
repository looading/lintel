use std::{rc::Rc, sync::Arc};

use super::model::{Description, Mailbox, SemanticAction, WindowState};
use crate::desktop::accessibility::accesskit::Role;
use crate::iced::advanced::Renderer as _;
use crate::iced::{
  self, Element, Event, Length, Rectangle, Size, Theme,
  advanced::{
    self, Clipboard, Layout, Shell, Widget, layout, mouse, renderer,
    widget::{self, Tree, tree},
  },
};

/// A normal iced element with explicit semantics. Actions still publish ordinary business messages.
pub struct Semantic<'a, Message> {
  content: Element<'a, Message>,
  description: Description,
  on_action: Option<Box<dyn Fn(SemanticAction) -> Option<Message> + 'a>>,
}

/// 自定义原子控件可以声明的角色。包装器内部的子语义由该节点统一表示。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SemanticRole {
  Button,
  CheckBox,
  TextInput,
  PasswordInput,
  Label,
}

impl SemanticRole {
  fn native(self) -> Role {
    match self {
      Self::Button => Role::Button,
      Self::CheckBox => Role::CheckBox,
      Self::TextInput => Role::TextInput,
      Self::PasswordInput => Role::PasswordInput,
      Self::Label => Role::Label,
    }
  }
}

/// 为一个自定义控件声明语义，继续使用它原有的布局、渲染和普通业务 Message。
///
/// Click/SetValue 需要显式调用 `on_action`。容器应分别包装其交互子控件；
/// 此包装器表示单个原子节点，不展开内部控件的语义。
pub fn semantic<'a, Message>(
  key: impl Into<String>,
  label: impl Into<String>,
  role: SemanticRole,
  content: impl Into<Element<'a, Message>>,
) -> Semantic<'a, Message> {
  wrap(
    content,
    Description {
      key: key.into(),
      label: label.into(),
      role: role.native(),
      value: None,
      checked: (role == SemanticRole::CheckBox).then_some(false),
      enabled: true,
      actionable: false,
    },
  )
}

impl<'a, Message> Semantic<'a, Message> {
  /// 将该角色支持的辅助操作映射为业务消息；无处理器时不发布操作或加入 Tab 顺序。
  pub fn on_action(mut self, handler: impl Fn(SemanticAction) -> Option<Message> + 'a) -> Self {
    self.description.actionable = self.description.role != Role::Label;
    self.on_action = Some(Box::new(handler));
    self
  }

  /// 禁用语义操作、Tab 焦点和内部控件的输入事件。外观仍由内部控件决定。
  pub fn enabled(mut self, enabled: bool) -> Self {
    self.description.enabled = enabled;
    self
  }

  /// 声明当前文本值。安全输入和其他角色不会保存传入的值。
  pub fn value(mut self, value: impl Into<String>) -> Self {
    if self.description.role == Role::TextInput {
      self.description.value = Some(value.into());
    }
    self
  }

  /// 声明复选框的当前状态；其他角色忽略此设置。
  pub fn checked(mut self, checked: bool) -> Self {
    if self.description.role == Role::CheckBox {
      self.description.checked = Some(checked);
    }
    self
  }

  fn dispatch(&self, action: SemanticAction) -> Option<Message> {
    self
      .description
      .accepts(&action)
      .then(|| self.on_action.as_ref().and_then(|handler| handler(action)))
      .flatten()
  }
}
pub(crate) struct SemanticState {
  pub mailbox: Arc<Mailbox>,
  focused: bool,
  focus_revision: u64,
}

fn wrap<'a, Message>(content: impl Into<Element<'a, Message>>, description: Description) -> Semantic<'a, Message> {
  Semantic {
    content: content.into(),
    description,
    on_action: None,
  }
}

fn traverse_tab<Message>(event: &Event, owner: Option<&Arc<WindowState>>, shell: &mut Shell<'_, Message>) -> bool {
  let Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) = event else {
    return false;
  };
  if *key != iced::keyboard::Key::Named(iced::keyboard::key::Named::Tab)
    || modifiers.control()
    || modifiers.logo()
    || modifiers.alt()
  {
    return false;
  }
  if !shell.is_event_captured() && owner.is_some_and(|owner| owner.advance(modifiers.shift())) {
    shell.capture_event();
  }
  shell.is_event_captured()
}

pub fn button<'a, Message: Clone + 'a>(
  key: impl Into<String>,
  label: impl Into<String>,
  on_press: Message,
) -> Semantic<'a, Message> {
  let label = label.into();
  let content = iced::widget::button(iced::widget::text(label.clone())).on_press(on_press.clone());
  let mut widget = wrap(
    content,
    Description {
      key: key.into(),
      label,
      role: Role::Button,
      value: None,
      checked: None,
      enabled: true,
      actionable: true,
    },
  );
  widget.on_action = Some(Box::new(move |action| {
    matches!(action, SemanticAction::Click).then(|| on_press.clone())
  }));
  widget
}

pub fn disabled_button<'a, Message: Clone + 'a>(
  key: impl Into<String>,
  label: impl Into<String>,
) -> Semantic<'a, Message> {
  let label = label.into();
  wrap(
    iced::widget::button(iced::widget::text(label.clone())),
    Description {
      key: key.into(),
      label,
      role: Role::Button,
      value: None,
      checked: None,
      enabled: false,
      actionable: false,
    },
  )
}

pub fn checkbox<'a, Message: 'a>(
  key: impl Into<String>,
  label: impl Into<String>,
  checked: bool,
  on_toggle: impl Fn(bool) -> Message + 'a,
) -> Semantic<'a, Message> {
  let label = label.into();
  let on_toggle = Rc::new(on_toggle);
  let callback = on_toggle.clone();
  let content = iced::widget::checkbox(checked)
    .label(label.clone())
    .on_toggle(move |value| callback(value));
  let mut widget = wrap(
    content,
    Description {
      key: key.into(),
      label,
      role: Role::CheckBox,
      value: None,
      checked: Some(checked),
      enabled: true,
      actionable: true,
    },
  );
  widget.on_action = Some(Box::new(move |action| {
    matches!(action, SemanticAction::Click).then(|| on_toggle(!checked))
  }));
  widget
}

pub fn text_input<'a, Message: Clone + 'a>(
  key: impl Into<String>,
  label: &'a str,
  value: &'a str,
  on_input: impl Fn(String) -> Message + 'a,
) -> Semantic<'a, Message> {
  input(key.into(), label, value, on_input, false)
}

/// Secure values are never copied into the accessibility snapshot.
pub fn password_input<'a, Message: Clone + 'a>(
  key: impl Into<String>,
  label: &'a str,
  value: &'a str,
  on_input: impl Fn(String) -> Message + 'a,
) -> Semantic<'a, Message> {
  input(key.into(), label, value, on_input, true)
}

fn input<'a, Message: Clone + 'a>(
  key: String,
  label: &'a str,
  value: &'a str,
  on_input: impl Fn(String) -> Message + 'a,
  secure: bool,
) -> Semantic<'a, Message> {
  let on_input = Rc::new(on_input);
  let callback = on_input.clone();
  let content = iced::widget::text_input(label, value)
    .secure(secure)
    .on_input(move |value| callback(value));
  let mut widget = wrap(
    content,
    Description {
      key,
      label: label.into(),
      role: if secure { Role::PasswordInput } else { Role::TextInput },
      value: (!secure).then(|| value.to_owned()),
      checked: None,
      enabled: true,
      actionable: true,
    },
  );
  widget.on_action = Some(Box::new(move |action| match action {
    SemanticAction::SetValue(value) => Some(on_input(value)),
    _ => None,
  }));
  widget
}

impl<Message> Widget<Message, Theme, iced::Renderer> for Semantic<'_, Message> {
  fn size(&self) -> Size<Length> {
    self.content.as_widget().size()
  }
  fn size_hint(&self) -> Size<Length> {
    self.content.as_widget().size_hint()
  }
  fn tag(&self) -> tree::Tag {
    tree::Tag::of::<SemanticState>()
  }
  fn state(&self) -> tree::State {
    tree::State::new(SemanticState {
      mailbox: Mailbox::new(self.description.clone()),
      focused: false,
      focus_revision: 0,
    })
  }
  fn children(&self) -> Vec<Tree> {
    vec![Tree::new(self.content.as_widget())]
  }
  fn diff(&self, tree: &mut Tree) {
    let state = tree.state.downcast_mut::<SemanticState>();
    let previous = state.mailbox.description.lock().unwrap().clone();
    let reset_input = previous.enabled && !self.description.enabled;
    if previous.key != self.description.key || previous.role != self.description.role {
      state.mailbox = Mailbox::new(self.description.clone());
      state.focused = false;
      state.focus_revision = 0;
    } else {
      *state.mailbox.description.lock().unwrap() = self.description.clone();
    }
    if reset_input {
      // 丢弃禁用前的按下、输入焦点和内部 overlay 状态，避免重新启用后补发旧操作。
      tree.children = vec![Tree::new(self.content.as_widget())];
    } else {
      tree.diff_children(std::slice::from_ref(&self.content));
    }
  }
  fn layout(&mut self, tree: &mut Tree, renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
    self
      .content
      .as_widget_mut()
      .layout(&mut tree.children[0], renderer, limits)
  }
  fn draw(
    &self,
    tree: &Tree,
    renderer: &mut iced::Renderer,
    theme: &Theme,
    style: &renderer::Style,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    viewport: &Rectangle,
  ) {
    self
      .content
      .as_widget()
      .draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    if self.description.focusable()
      && tree.state.downcast_ref::<SemanticState>().focused
      && matches!(self.description.role, Role::Button | Role::CheckBox)
    {
      renderer.fill_quad(
        renderer::Quad {
          bounds: layout.bounds(),
          border: iced::Border {
            color: iced::Color::from_rgb(0.1, 0.45, 0.9),
            width: 2.0,
            ..Default::default()
          },
          ..Default::default()
        },
        iced::Color::TRANSPARENT,
      );
    }
  }
  fn operate(
    &mut self,
    tree: &mut Tree,
    layout: Layout<'_>,
    renderer: &iced::Renderer,
    operation: &mut dyn widget::Operation,
  ) {
    operation.custom(None, layout.bounds(), tree.state.downcast_mut::<SemanticState>());
    operation.traverse(&mut |operation| {
      self
        .content
        .as_widget_mut()
        .operate(&mut tree.children[0], layout, renderer, operation)
    });
  }
  fn update(
    &mut self,
    tree: &mut Tree,
    event: &Event,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    renderer: &iced::Renderer,
    clipboard: &mut dyn Clipboard,
    shell: &mut Shell<'_, Message>,
    viewport: &Rectangle,
  ) {
    let state = tree.state.downcast_mut::<SemanticState>();
    let owner = state.mailbox.owner.lock().unwrap().upgrade();
    // 在 iced 分发层处理 Tab，使先打开的菜单/overlay 能在同一批事件内优先消费键盘。
    if traverse_tab(event, owner.as_ref(), shell) {
      return;
    }
    if self.description.focusable()
      && matches!(
        event,
        Event::Mouse(iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left))
      )
      && cursor.is_over(layout.bounds())
    {
      state.mailbox.focus();
    }
    let token = owner.as_ref().map(|owner| owner.focus_token()).unwrap_or((0, 0));
    let focused = token.0 == state.mailbox.id.0 && self.description.focusable();
    let revision = token.1;
    if focused != state.focused || (focused && revision != state.focus_revision) {
      state.focused = focused;
      state.focus_revision = revision;
      let mut operation = SetFocus(focused);
      self
        .content
        .as_widget_mut()
        .operate(&mut tree.children[0], layout, renderer, &mut operation);
    }
    let mut actions = state.mailbox.drain();
    if focused && matches!(self.description.role, Role::Button | Role::CheckBox) {
      if let Event::Keyboard(iced::keyboard::Event::KeyPressed { key, .. }) = event {
        if matches!(
          key,
          iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter | iced::keyboard::key::Named::Space)
        ) {
          actions.push(SemanticAction::Click);
          shell.capture_event();
        }
      }
    }
    for action in actions {
      if let Some(message) = self.dispatch(action) {
        shell.publish(message);
      }
    }
    if !self.description.enabled {
      return;
    }
    self.content.as_widget_mut().update(
      &mut tree.children[0],
      event,
      layout,
      cursor,
      renderer,
      clipboard,
      shell,
      viewport,
    );
    if state.focused && matches!(self.description.role, Role::TextInput | Role::PasswordInput) {
      let mut probe = ReadFocus(false);
      self
        .content
        .as_widget_mut()
        .operate(&mut tree.children[0], layout, renderer, &mut probe);
      if !probe.0 {
        state.focused = false;
        if let Some(owner) = owner {
          owner.clear_focus(state.mailbox.id, revision);
        }
      }
    }
  }
  fn mouse_interaction(
    &self,
    tree: &Tree,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    viewport: &Rectangle,
    renderer: &iced::Renderer,
  ) -> mouse::Interaction {
    if !self.description.enabled {
      return mouse::Interaction::None;
    }
    self
      .content
      .as_widget()
      .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
  }
  fn overlay<'b>(
    &'b mut self,
    tree: &'b mut Tree,
    layout: Layout<'b>,
    renderer: &iced::Renderer,
    viewport: &Rectangle,
    translation: iced::Vector,
  ) -> Option<advanced::overlay::Element<'b, Message, Theme, iced::Renderer>> {
    if !self.description.enabled {
      return None;
    }
    self
      .content
      .as_widget_mut()
      .overlay(&mut tree.children[0], layout, renderer, viewport, translation)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn secure_input_semantics_do_not_contain_the_secret() {
    let widget = password_input("secret", "Password", "never-publish-this", |_| ());
    let tree = widget.state();
    let state = tree.downcast_ref::<SemanticState>();
    let data = state.mailbox.description.lock().unwrap();
    assert_eq!(data.role, Role::PasswordInput);
    assert!(data.value.is_none());
    assert_eq!(data.label, "Password");
  }

  #[test]
  fn custom_atomic_control_publishes_only_supported_actions_and_revokes_when_disabled() {
    let calls = Rc::new(std::cell::Cell::new(0));
    let count = calls.clone();
    let widget = semantic(
      "custom-check",
      "Custom check",
      SemanticRole::CheckBox,
      iced::widget::text("A custom shape"),
    )
    .checked(true)
    .on_action(move |action| {
      count.set(count.get() + 1);
      Some(action)
    });
    assert_eq!(widget.description.checked, Some(true));
    assert_eq!(widget.dispatch(SemanticAction::Click), Some(SemanticAction::Click));
    assert_eq!(widget.dispatch(SemanticAction::SetValue("ignored".into())), None);
    let widget = widget.enabled(false);
    assert_eq!(widget.dispatch(SemanticAction::Click), None);
    assert_eq!(calls.get(), 1);
    assert!(!widget.description.focusable());
  }

  #[test]
  fn custom_password_and_label_never_keep_text_values_or_advertise_unhandled_actions() {
    for role in [SemanticRole::PasswordInput, SemanticRole::Label, SemanticRole::Button] {
      let widget =
        semantic::<()>("custom", "Custom", role, iced::widget::text("Visual label")).value("private-example-secret");
      assert!(widget.description.value.is_none());
      assert!(!widget.description.focusable());
      assert_eq!(widget.dispatch(SemanticAction::Click), None);
    }
    let widget = semantic(
      "custom-password",
      "Password",
      SemanticRole::PasswordInput,
      iced::widget::text("••••"),
    )
    .value("private-example-secret")
    .on_action(Some);
    assert!(widget.description.value.is_none());
    assert_eq!(
      widget.dispatch(SemanticAction::SetValue("new value".into())),
      Some(SemanticAction::SetValue("new value".into()))
    );
    assert!(
      widget
        .dispatch(SemanticAction::SetValue("x".repeat(64 * 1024 + 1)))
        .is_none()
    );
    let label = semantic("label", "Summary", SemanticRole::Label, iced::widget::text("Summary")).on_action(Some);
    assert!(!label.description.focusable());
    assert_eq!(label.dispatch(SemanticAction::Click), None);
    let input = semantic("input", "Input", SemanticRole::TextInput, iced::widget::text("Initial"))
      .value("Initial")
      .on_action(Some);
    assert_eq!(input.description.value.as_deref(), Some("Initial"));
    assert_eq!(
      input.dispatch(SemanticAction::SetValue("Next".into())),
      Some(SemanticAction::SetValue("Next".into()))
    );
  }

  #[test]
  fn custom_identity_is_retained_for_value_changes_and_replaced_for_role_or_key_changes() {
    let mut widget = semantic::<()>("name", "Name", SemanticRole::TextInput, iced::widget::text("A"))
      .value("A")
      .on_action(|_| Some(()));
    let mut tree = Tree::new(&widget as &dyn Widget<(), Theme, iced::Renderer>);
    let initial = tree.state.downcast_ref::<SemanticState>().mailbox.id;
    widget = widget.value("B");
    widget.diff(&mut tree);
    assert_eq!(tree.state.downcast_ref::<SemanticState>().mailbox.id, initial);
    let replacement = semantic::<()>("name", "Name", SemanticRole::PasswordInput, iced::widget::text("••"));
    replacement.diff(&mut tree);
    let replaced = tree.state.downcast_ref::<SemanticState>().mailbox.id;
    assert_ne!(replaced, initial);
    let renamed = semantic::<()>("renamed", "Name", SemanticRole::PasswordInput, iced::widget::text("••"));
    renamed.diff(&mut tree);
    assert_ne!(tree.state.downcast_ref::<SemanticState>().mailbox.id, replaced);
  }

  #[test]
  fn disabling_custom_input_clears_child_focus_without_replacing_semantic_identity() {
    type InputState = iced::widget::text_input::State<<iced::Renderer as advanced::text::Renderer>::Paragraph>;
    let input = iced::widget::text_input("Name", "A").on_input(|value| value);
    let widget = semantic("name", "Name", SemanticRole::TextInput, input).on_action(|_| Some(String::new()));
    let mut tree = Tree::new(&widget as &dyn Widget<String, Theme, iced::Renderer>);
    let id = tree.state.downcast_ref::<SemanticState>().mailbox.id;
    tree.children[0].state.downcast_mut::<InputState>().focus();
    assert!(tree.children[0].state.downcast_ref::<InputState>().is_focused());
    let widget = widget.enabled(false);
    widget.diff(&mut tree);
    assert!(!tree.children[0].state.downcast_ref::<InputState>().is_focused());
    assert_eq!(tree.state.downcast_ref::<SemanticState>().mailbox.id, id);
    widget.enabled(true).diff(&mut tree);
    assert!(!tree.children[0].state.downcast_ref::<InputState>().is_focused());
  }

  #[test]
  fn real_window_owner_resumes_same_batch_tab_text_and_ax_click_after_modal_collection() {
    use super::super::model::{Navigation, Snapshot};
    use crate::desktop::accessibility::accesskit as ak;
    use crate::iced::advanced::renderer::Headless;
    use crate::iced::iced_winit::runtime::user_interface::{Cache, UserInterface};
    use crate::iced::keyboard::{Key, key::Named};
    use std::collections::BTreeMap;
    use std::sync::Weak;
    #[derive(Clone, Debug, PartialEq)]
    enum Message {
      A(String),
      B(String),
      Click,
      Menu(crate::menu::MenuAction),
    }
    struct Bind {
      owner: Arc<WindowState>,
      routes: BTreeMap<ak::NodeId, Weak<Mailbox>>,
      order: Vec<ak::NodeId>,
      modal: bool,
      bind_menus: bool,
    }
    impl widget::Operation for Bind {
      fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn widget::Operation)) {
        operate(self);
      }
      fn custom(&mut self, _id: Option<&widget::Id>, _bounds: Rectangle, value: &mut dyn std::any::Any) {
        if let Some(menu) = value.downcast_mut::<crate::menu::MenuState>() {
          if !self.bind_menus {
            return;
          }
          menu.allow_modal_collection();
          let owner = Arc::downgrade(&self.owner);
          let source = menu.modal_source_id();
          menu.set_modal_barrier(Arc::new(move |open| {
            if let Some(owner) = owner.upgrade() {
              owner.menu_modal_for(source, open);
            }
          }));
          self.modal |= menu.is_open();
          assert!(!menu.take_modal_boundary());
        }
        if let Some(semantic) = value.downcast_mut::<SemanticState>() {
          *semantic.mailbox.owner.lock().unwrap() = Arc::downgrade(&self.owner);
          self
            .routes
            .insert(semantic.mailbox.id, Arc::downgrade(&semantic.mailbox));
          if semantic.mailbox.description.lock().unwrap().focusable() {
            self.order.push(semantic.mailbox.id);
          }
        }
      }
    }
    impl Bind {
      fn new(owner: Arc<WindowState>) -> Self {
        Self {
          owner,
          routes: BTreeMap::new(),
          order: Vec::new(),
          modal: false,
          bind_menus: true,
        }
      }
      fn publish(self) {
        let mut root = ak::Node::new(ak::Role::Window);
        root.set_children(self.order.clone());
        let mut nodes = vec![(ak::NodeId(0), root)];
        for (id, mailbox) in &self.routes {
          if let Some(mailbox) = mailbox.upgrade() {
            nodes.push((*id, ak::Node::new(mailbox.description.lock().unwrap().role)));
          }
        }
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
              tree: Some(ak::TreeInfo::new(ak::NodeId(0))),
              tree_id: ak::TreeId::ROOT,
              focus: ak::NodeId(0),
            },
            routes,
            order,
            signature: Vec::new(),
            modal: self.modal,
            resume,
            menus: Vec::new(),
          },
          false,
        );
      }
    }
    let keyboard = |key: Key| {
      Event::Keyboard(iced::keyboard::Event::KeyPressed {
        modified_key: key.clone(),
        key,
        physical_key: iced::keyboard::key::Physical::Unidentified(iced::keyboard::key::NativeCode::Unidentified),
        location: iced::keyboard::Location::Standard,
        modifiers: iced::keyboard::Modifiers::empty(),
        text: None,
        repeat: false,
      })
    };
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let mut renderer = runtime
      .block_on(<iced::Renderer as Headless>::new(
        iced::Font::DEFAULT,
        iced::Pixels(14.0),
        Some("tiny-skia"),
      ))
      .expect("headless software renderer");
    let model = crate::menu::MenuModel::new(crate::desktop::MenuSpec {
      items: vec![crate::desktop::MenuEntry::Submenu {
        label: "File".into(),
        enabled: true,
        items: vec![crate::desktop::MenuEntry::item("run", "Run")],
      }],
    })
    .unwrap();
    let waker = iced::iced_core::shell::Waker::noop();
    let mut clipboard = iced::advanced::clipboard::Null;
    let mut character = keyboard(Key::Character("b".into()));
    if let Event::Keyboard(iced::keyboard::Event::KeyPressed { text, .. }) = &mut character {
      *text = Some("b".into());
    }
    for close_key in [Named::Tab, Named::Escape] {
      for text in [
        character.clone(),
        Event::InputMethod(iced::advanced::input_method::Event::Commit("b".into())),
      ] {
        let owner = WindowState::new(|| {});
        let body = iced::widget::column![
          text_input("a", "A", "", Message::A),
          text_input("b", "B", "", Message::B),
          button("click", "Click", Message::Click)
        ];
        let view: Element<'_, Message> = crate::menu::menu_bar(&model, Message::Menu, body).into();
        let mut ui = UserInterface::build(view, Size::new(800.0, 500.0), Cache::new(), &mut renderer);
        let mut bind = Bind::new(owner.clone());
        ui.operate(&renderer, &mut bind);
        let a = bind.order[0];
        let b = bind.order[1];
        let click = bind.order[2];
        bind.publish();
        owner.focus(a);
        let redraw = Event::Window(iced::window::Event::RedrawRequested(
          iced::iced_core::time::Instant::now(),
        ));
        let mut messages = Vec::new();
        ui.update(
          &waker,
          &[redraw.clone(), keyboard(Key::Named(Named::F10))],
          mouse::Cursor::Unavailable,
          &mut renderer,
          &mut clipboard,
          &mut messages,
        );
        let mut modal = Bind::new(owner.clone());
        ui.operate(&renderer, &mut modal);
        assert!(modal.modal);
        modal.publish();
        assert!(!owner.advance(false));
        assert_eq!(owner.tree().unwrap().focus, ak::NodeId(0));
        let batch = [keyboard(Key::Named(close_key)), keyboard(Key::Named(Named::Tab)), text];
        let (_, statuses) = ui.update(
          &waker,
          &batch,
          mouse::Cursor::Unavailable,
          &mut renderer,
          &mut clipboard,
          &mut messages,
        );
        assert_eq!(statuses.len(), batch.len());
        assert_eq!(
          messages,
          vec![Message::B("b".into())],
          "owner=Some close -> second Tab -> input must target only B"
        );
        assert_eq!(owner.focus_token().0, b.0);
        owner.action(ak::ActionRequest {
          target_tree: ak::TreeId::ROOT,
          target_node: click,
          action: ak::Action::Click,
          data: None,
        });
        let mut collect = Bind::new(owner.clone());
        ui.operate(&renderer, &mut collect);
        assert!(!collect.modal);
        collect.publish();
        ui.update(
          &waker,
          &[redraw.clone()],
          mouse::Cursor::Unavailable,
          &mut renderer,
          &mut clipboard,
          &mut messages,
        );
        assert_eq!(
          messages,
          vec![Message::B("b".into()), Message::Click],
          "next Collect must not purge a post-close AX Click"
        );
        ui.update(
          &waker,
          &[redraw],
          mouse::Cursor::Unavailable,
          &mut renderer,
          &mut clipboard,
          &mut messages,
        );
        assert_eq!(messages.len(), 2, "a later redraw cannot replay the action");
      }
    }
    // A newly inserted outer/inner pair can first bind after only the outer is already open.
    // The closed inner source must not undo the outer's synchronous admission barrier.
    let owner = WindowState::new(|| {});
    let body = button("nested-body", "Nested body", Message::Click);
    let inner = crate::menu::menu_bar(&model, Message::Menu, body);
    let view: Element<'_, Message> = crate::menu::menu_bar(&model, Message::Menu, inner).into();
    let mut ui = UserInterface::build(view, Size::new(800.0, 500.0), Cache::new(), &mut renderer);
    let mut previous = Bind::new(owner.clone());
    previous.bind_menus = false;
    ui.operate(&renderer, &mut previous);
    let id = previous.order[0];
    let mailbox = previous.routes[&id].upgrade().unwrap();
    previous.publish();
    owner.focus(id);
    let token = owner.focus_token();
    let mut messages = Vec::new();
    ui.update(
      &waker,
      &[keyboard(Key::Named(Named::F10))],
      mouse::Cursor::Unavailable,
      &mut renderer,
      &mut clipboard,
      &mut messages,
    );
    let mut first = Bind::new(owner.clone());
    ui.operate(&renderer, &mut first);
    assert!(first.modal);
    first.publish();
    assert!(!owner.advance(false));
    owner.focus(id);
    assert_eq!(owner.focus_token(), token);
    owner.action(ak::ActionRequest {
      target_tree: ak::TreeId::ROOT,
      target_node: id,
      action: ak::Action::Click,
      data: None,
    });
    assert!(mailbox.drain().is_empty());
    assert_eq!(owner.tree().unwrap().focus, ak::NodeId(0));
    ui.update(
      &waker,
      &[keyboard(Key::Named(Named::Escape))],
      mouse::Cursor::Unavailable,
      &mut renderer,
      &mut clipboard,
      &mut messages,
    );
    owner.action(ak::ActionRequest {
      target_tree: ak::TreeId::ROOT,
      target_node: id,
      action: ak::Action::Click,
      data: None,
    });
    ui.update(
      &waker,
      &[Event::Window(iced::window::Event::RedrawRequested(
        iced::iced_core::time::Instant::now(),
      ))],
      mouse::Cursor::Unavailable,
      &mut renderer,
      &mut clipboard,
      &mut messages,
    );
    assert_eq!(
      messages,
      vec![Message::Click],
      "background admission resumes only after the last real menu closes"
    );
  }

  #[test]
  fn tab_is_consumed_once_after_parent_handlers_and_preserves_modified_shortcuts() {
    use super::super::model::Snapshot;
    use crate::desktop::accessibility::accesskit as ak;
    let owner = WindowState::new(|| {});
    let a = Mailbox::new(button("a", "A", ()).description);
    let b = Mailbox::new(button("b", "B", ()).description);
    *owner.snapshot.lock().unwrap() = Some(Snapshot {
      update: ak::TreeUpdate {
        nodes: vec![],
        tree: None,
        tree_id: ak::TreeId::ROOT,
        focus: ak::NodeId(0),
      },
      routes: [(a.id, Arc::downgrade(&a)), (b.id, Arc::downgrade(&b))].into(),
      order: vec![a.id, b.id],
      signature: vec![],
      modal: false,
      resume: None,
      menus: Vec::new(),
    });
    let event = |modifiers| {
      let key = iced::keyboard::Key::Named(iced::keyboard::key::Named::Tab);
      Event::Keyboard(iced::keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::Tab),
        location: iced::keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat: false,
      })
    };
    let mut messages: Vec<()> = vec![];
    let mut shell = Shell::new(iced::iced_core::shell::Waker::noop(), &mut messages);
    assert!(traverse_tab(
      &event(iced::keyboard::Modifiers::empty()),
      Some(&owner),
      &mut shell
    ));
    let first = owner.focus_token();
    assert_eq!(first.0, a.id.0);
    assert!(traverse_tab(
      &event(iced::keyboard::Modifiers::empty()),
      Some(&owner),
      &mut shell
    ));
    assert_eq!(
      owner.focus_token(),
      first,
      "later wrappers must not advance the same event again"
    );
    let mut shell = Shell::new(iced::iced_core::shell::Waker::noop(), &mut messages);
    shell.capture_event(); // 同批次更早的菜单/overlay 已处理 Tab。
    assert!(traverse_tab(
      &event(iced::keyboard::Modifiers::empty()),
      Some(&owner),
      &mut shell
    ));
    assert_eq!(owner.focus_token(), first);
    let mut shell = Shell::new(iced::iced_core::shell::Waker::noop(), &mut messages);
    assert!(!traverse_tab(
      &event(iced::keyboard::Modifiers::CTRL),
      Some(&owner),
      &mut shell
    ));
    assert_eq!(owner.focus_token(), first);
    assert!(traverse_tab(
      &event(iced::keyboard::Modifiers::SHIFT),
      Some(&owner),
      &mut shell
    ));
    assert_eq!(owner.focus_token().0, b.id.0);
  }
}

struct SetFocus(bool);
struct ReadFocus(bool);
impl widget::Operation for ReadFocus {
  fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn widget::Operation)) {
    operate(self);
  }
  fn focusable(&mut self, _id: Option<&widget::Id>, _bounds: Rectangle, state: &mut dyn widget::operation::Focusable) {
    self.0 |= state.is_focused();
  }
}
impl widget::Operation for SetFocus {
  fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn widget::Operation)) {
    operate(self);
  }
  fn focusable(&mut self, _id: Option<&widget::Id>, _bounds: Rectangle, state: &mut dyn widget::operation::Focusable) {
    if self.0 {
      state.focus();
    } else {
      state.unfocus();
    }
  }
}
impl<'a, Message: 'a> From<Semantic<'a, Message>> for Element<'a, Message> {
  fn from(widget: Semantic<'a, Message>) -> Self {
    Element::new(widget)
  }
}
