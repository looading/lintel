//! Client-rendered, per-window iced menubar. It does not install a desktop-shell/global menu.
//! Keep MenuModel in application state and map MenuAction to the application's normal Message.
use crate::accessibility::menu::{self as menu_ax, Command as MenuCommand, Kind as MenuKind, Space as MenuSpace};
pub use crate::desktop::client_menu::{MenuAction, MenuModel};
use crate::desktop::{
  MenuEntry,
  client_menu::{MenuKey, MenuSession, entry_enabled},
};
use crate::iced::advanced::Renderer as _;
use crate::iced::{
  self, Element, Event, Length, Point, Rectangle, Size, Theme, Vector,
  advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{self, Tree, tree},
  },
};

// Explicit diagnostic fixture only. It never records text/characters, IME contents or AX values.
pub(crate) fn input_trace_enabled() -> bool {
  static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
  cfg!(debug_assertions) && *ENABLED.get_or_init(|| std::env::args_os().any(|arg| arg == "--trace-input"))
}
pub(crate) fn input_trace(stage: &str, details: std::fmt::Arguments<'_>) {
  if !input_trace_enabled() {
    return;
  }
  static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
  let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
  let millis = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|time| time.as_millis())
    .unwrap_or(0);
  eprintln!(
    "LINTEL_CLIENT_MENU_TRACE pid={} seq={sequence} ms={millis} stage={stage} {details}",
    std::process::id()
  );
}
fn input_event_class(event: &Event) -> Option<String> {
  fn key_class(key: &iced::keyboard::Key) -> String {
    match key {
      iced::keyboard::Key::Named(named) => format!("Named::{named:?}"),
      iced::keyboard::Key::Character(_) => "Character".into(),
      iced::keyboard::Key::Unidentified => "Unidentified".into(),
    }
  }
  match event {
    Event::Keyboard(iced::keyboard::Event::KeyPressed {
      key, modifiers, repeat, ..
    }) => Some(format!(
      "pressed key={} mods={modifiers:?} repeat={repeat}",
      key_class(key)
    )),
    Event::Keyboard(iced::keyboard::Event::KeyReleased { key, modifiers, .. }) => {
      Some(format!("released key={} mods={modifiers:?}", key_class(key)))
    }
    Event::Keyboard(iced::keyboard::Event::ModifiersChanged(modifiers)) => Some(format!("modifiers={modifiers:?}")),
    Event::InputMethod(event) => Some(format!(
      "ime={}",
      match event {
        iced::advanced::input_method::Event::Opened => "Opened",
        iced::advanced::input_method::Event::Closed => "Closed",
        iced::advanced::input_method::Event::Preedit(..) => "Preedit",
        iced::advanced::input_method::Event::Commit(_) => "Commit",
      }
    )),
    Event::Window(iced::window::Event::Focused) => Some("window_focused=true".into()),
    Event::Window(iced::window::Event::Unfocused) => Some("window_focused=false".into()),
    _ => None,
  }
}
fn trace_input_event(stage: &str, event: &Event, status: iced::event::Status, details: std::fmt::Arguments<'_>) {
  if input_trace_enabled() {
    if let Some(class) = input_event_class(event) {
      input_trace(stage, format_args!("{class} status={status:?} {details}"));
    }
  }
}

const BAR_HEIGHT: f32 = 32.0;
const ROW_HEIGHT: f32 = 30.0;
const SEPARATOR_HEIGHT: f32 = 10.0;
const PANEL_WIDTH: f32 = 270.0;
const PADDING: f32 = 4.0;
const MARGIN: f32 = 4.0;

/// Renders a window-local menu and content, using existing iced widgets and overlay ownership.
/// A check action proposes a value; apply it to the model in update. Accelerator text is a hint.
pub fn menu_bar<'a, Message: 'a>(
  model: &'a MenuModel,
  on_select: impl Fn(MenuAction) -> Message + 'a,
  content: impl Into<Element<'a, Message>>,
) -> MenuBar<'a, Message> {
  let headers = model
    .spec()
    .items
    .iter()
    .map(header)
    .collect::<Vec<Element<'a, Message>>>();
  let bar = iced::widget::row(headers)
    .height(BAR_HEIGHT)
    .width(Length::Fill)
    .align_y(iced::Alignment::Center);
  let base = iced::widget::column![bar, content.into()]
    .width(Length::Fill)
    .height(Length::Fill)
    .into();
  MenuBar {
    model,
    on_select: Box::new(on_select),
    base,
    panels: Vec::new(),
  }
}

pub struct MenuBar<'a, Message> {
  model: &'a MenuModel,
  on_select: Box<dyn Fn(MenuAction) -> Message + 'a>,
  base: Element<'a, Message>,
  panels: Vec<Element<'a, Message>>,
}
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum MenuPointer {
  Mouse(mouse::Button),
  Touch(iced::touch::Finger),
}
fn pointer_press(event: &Event) -> Option<MenuPointer> {
  match event {
    Event::Mouse(mouse::Event::ButtonPressed(button)) => Some(MenuPointer::Mouse(*button)),
    Event::Touch(iced::touch::Event::FingerPressed { id, .. }) => Some(MenuPointer::Touch(*id)),
    _ => None,
  }
}
fn pointer_end(event: &Event) -> Option<MenuPointer> {
  match event {
    Event::Mouse(mouse::Event::ButtonReleased(button)) => Some(MenuPointer::Mouse(*button)),
    Event::Touch(iced::touch::Event::FingerLifted { id, .. } | iced::touch::Event::FingerLost { id, .. }) => {
      Some(MenuPointer::Touch(*id))
    }
    _ => None,
  }
}

/// Exposed to the framework's semantic collector only; interaction remains scoped to this Tree.
pub(crate) struct MenuState {
  modal_source: u64,
  session: MenuSession,
  panel_trees: Vec<Tree>,
  scroll: Vec<f32>,
  last_path: Vec<usize>,
  keyboard: bool,
  hovered: Option<usize>,
  last_modal_state: bool,
  modal_boundary_pending: bool,
  deferred_unfocus: bool,
  modal_barrier: Option<std::sync::Arc<dyn Fn(bool) + Send + Sync>>,
  modal_collection_requested: bool,
  owned_pointers: std::collections::HashSet<MenuPointer>,
  activation_pointer: Option<MenuPointer>,
  ax: menu_ax::Semantics,
  ax_headers: Option<(Rectangle, Vec<Rectangle>, MenuSpace)>,
  ax_panels: Vec<menu_ax::Panel>,
}
impl MenuState {
  pub(crate) fn modal_source_id(&self) -> u64 {
    self.modal_source
  }
  pub(crate) fn is_open(&self) -> bool {
    self.session.is_open()
  }
  fn new() -> Self {
    static NEXT_SOURCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let modal_source = NEXT_SOURCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    assert_ne!(modal_source, 0, "menu source identity exhausted");
    Self {
      modal_source,
      session: MenuSession::default(),
      panel_trees: Vec::new(),
      scroll: Vec::new(),
      last_path: Vec::new(),
      keyboard: false,
      hovered: None,
      last_modal_state: false,
      modal_boundary_pending: false,
      deferred_unfocus: false,
      modal_barrier: None,
      modal_collection_requested: false,
      owned_pointers: std::collections::HashSet::new(),
      activation_pointer: None,
      ax: menu_ax::Semantics::new(modal_source),
      ax_headers: None,
      ax_panels: Vec::new(),
    }
  }
  fn close(&mut self) {
    input_trace(
      "state_close",
      format_args!("prior_open={} pending={}", self.is_open(), self.modal_boundary_pending),
    );
    self.session.close();
    self.ax.end_session();
    self.ax_panels.clear();
    self.scroll.clear();
    self.panel_trees.clear();
    self.last_path.clear();
    self.keyboard = false;
    self.sync_modal_boundary();
  }
  fn reconcile(&mut self, model: &MenuModel) {
    self.ax.session_changed(model, &self.session);
    if self.session.reconcile(model) {
      self.ax.end_session();
      self.ax_panels.clear();
      input_trace(
        "state_reconcile",
        format_args!("reason=model_token_changed new_token={:?}", model.token()),
      );
      self.scroll.clear();
      self.panel_trees.clear();
      self.last_path.clear();
      self.keyboard = false;
      self.sync_modal_boundary();
    }
  }
  fn sync_modal_boundary(&mut self) {
    let open = self.is_open();
    if open != self.last_modal_state {
      self.modal_boundary_pending = true;
      self.last_modal_state = open;
      if !open {
        self.activation_pointer = None;
      }
      if let Some(barrier) = &self.modal_barrier {
        barrier(open);
        self.modal_boundary_pending = false;
      }
    }
  }
  /// Collector observes tree transitions; native-action admission is synchronized separately.
  pub(crate) fn take_modal_boundary(&mut self) -> bool {
    std::mem::take(&mut self.modal_boundary_pending)
  }
  /// Bind once from the native collector; the callback captures a Weak WindowState.
  /// Every transition purges old actions synchronously before normal input can resume.
  pub(crate) fn set_modal_barrier(&mut self, barrier: std::sync::Arc<dyn Fn(bool) + Send + Sync>) {
    if self.modal_barrier.is_none() {
      barrier(self.is_open());
      self.modal_boundary_pending = false;
      self.modal_barrier = Some(barrier);
    }
  }
  /// Only the framework collector may request this one traversal for weak resume metadata.
  pub(crate) fn allow_modal_collection(&mut self) {
    self.modal_collection_requested = true;
  }
  pub(crate) fn accessibility_tree(&self) -> Option<menu_ax::MenuTree> {
    self.ax.tree()
  }
  fn refresh_accessibility(&mut self, model: &MenuModel) {
    self.ax.session_changed(model, &self.session);
    if let Some((bar, headers, space)) = &self.ax_headers {
      // Existing ancestor panes retain their identity when a new nested pane opens.
      // Closed branches are filtered by the builder; only newly laid-out panes are added.
      self.ax.rebuild(model, &self.session, *bar, headers, &self.ax_panels);
      if *space == MenuSpace::Window {
        if let Some(mut tree) = self.ax.tree() {
          for node in tree.nodes.iter_mut().filter(|node| node.space == MenuSpace::Widget) {
            node.space = MenuSpace::Window;
          }
          self.ax.set_tree(tree);
        }
      }
    }
  }
  fn native_pending(&self) -> bool {
    self.ax.mailbox.pending()
  }
  fn native_actions(&mut self, model: &MenuModel) -> Vec<MenuAction> {
    let mut messages = Vec::new();
    for request in self.ax.mailbox.drain() {
      if request.target.token != model.token() || !self.ax.mailbox.accepts(&request) {
        input_trace(
          "native_menu_reject",
          format_args!(
            "source={} node={} command={:?}",
            self.modal_source, request.node.0, request.command
          ),
        );
        continue;
      }
      input_trace(
        "native_menu_apply_ui",
        format_args!(
          "source={} node={} command={:?}",
          self.modal_source, request.node.0, request.command
        ),
      );
      let mut path = request.target.path.clone();
      match request.target.kind {
        MenuKind::Bar => {
          let Some(index) = model.spec().items.iter().position(entry_enabled) else {
            continue;
          };
          path = vec![index];
        }
        MenuKind::Popup => {
          if let Some(index) = model
            .items(&path)
            .and_then(|items| items.iter().position(entry_enabled))
          {
            path.push(index);
          }
        }
        _ => {}
      }
      if !model.enabled(&path) {
        continue;
      }
      self.activation_pointer = None;
      self.keyboard = true;
      self.ax.touch_focus();
      match request.command {
        MenuCommand::Focus => {
          self.session.focus_path(model, &path);
        }
        MenuCommand::Expand => {
          self.session.expand_path(model, &path);
        }
        MenuCommand::Collapse => {
          self.session.collapse_path(model, &path);
        }
        MenuCommand::Click => {
          if matches!(model.entry(&path), Some(MenuEntry::Submenu { .. })) {
            if self.session.open_path().starts_with(&path) {
              self.session.collapse_path(model, &path);
            } else {
              self.session.expand_path(model, &path);
            }
          } else {
            self.session.focus_path(model, &path);
            self.ax.session_changed(model, &self.session);
            if let Some(action) = self.session.key(model, MenuKey::Activate) {
              messages.push(action);
            }
          }
        }
      }
      self.refresh_accessibility(model);
      self.sync_modal_boundary();
    }
    messages
  }
  fn body_blocked(&self) -> bool {
    self.is_open()
  }
  fn defer_unfocus(&mut self) {
    self.close();
    self.deferred_unfocus = true;
  }
  fn take_deferred_unfocus(&mut self) -> bool {
    if self.body_blocked() {
      false
    } else {
      std::mem::take(&mut self.deferred_unfocus)
    }
  }
  fn own_pointer_press(&mut self, event: &Event) {
    if let Some(pointer) = pointer_press(event) {
      self.owned_pointers.insert(pointer);
    }
  }
  // A captured menu down owns only its matching up/cancel, even after the overlay disappears.
  fn consume_owned_pointer_end(&mut self, model: &MenuModel, event: &Event) -> bool {
    if let Some(pointer) = pointer_press(event) {
      self.owned_pointers.remove(&pointer); // A fresh down starts a new gesture after a lost up.
      if self.activation_pointer == Some(pointer) {
        self.activation_pointer = None;
      }
      return false;
    }
    let Some(pointer) = pointer_end(event) else {
      return false;
    };
    let owned = self.owned_pointers.remove(&pointer);
    if matches!(event, Event::Touch(iced::touch::Event::FingerLost { .. })) {
      if self.activation_pointer == Some(pointer) {
        self.activation_pointer = None;
        self.session.release(model, None);
      }
      return owned;
    }
    owned && !self.is_open()
  }
  fn release_action(&mut self, model: &MenuModel, event: &Event, path: Option<&[usize]>) -> Option<MenuAction> {
    if self.activation_pointer != pointer_end(event) {
      return None;
    }
    self.activation_pointer = None;
    self.session.release(model, path)
  }
  fn geometry_ready(&self) -> bool {
    self.is_open() && self.last_path == self.session.open_path()
  }
  fn scroll_by(&mut self, level: usize, delta: f32) -> bool {
    if !self.geometry_ready() {
      return false;
    }
    let Some(scroll) = self.scroll.get_mut(level) else {
      return false;
    };
    *scroll = (*scroll - delta).max(0.0);
    true
  }
  fn redraw<Message>(&mut self, model: &MenuModel, shell: &mut Shell<'_, Message>) {
    self.refresh_accessibility(model);
    self.sync_modal_boundary();
    shell.invalidate_layout();
    shell.request_redraw();
  }
}

impl Drop for MenuState {
  fn drop(&mut self) {
    self.ax.mailbox.close();
    if self.is_open() {
      if let Some(barrier) = &self.modal_barrier {
        barrier(false);
      }
    }
  }
}

fn label(entry: &MenuEntry) -> String {
  match entry {
    MenuEntry::Item { label, .. } | MenuEntry::Submenu { label, .. } => label.clone(),
    MenuEntry::Check { label, checked, .. } => format!("{} {label}", if *checked { "✓" } else { "  " }),
    MenuEntry::Separator => String::new(),
  }
}
fn header<'a, Message: 'a>(entry: &MenuEntry) -> Element<'a, Message> {
  let enabled = entry_enabled(entry);
  if matches!(entry, MenuEntry::Separator) {
    return iced::widget::space().width(8).into();
  }
  iced::widget::container(iced::widget::text(label(entry)).size(14).style(move |theme: &Theme| {
    iced::widget::text::Style {
      color: Some(iced::Color {
        a: if enabled { 1.0 } else { 0.4 },
        ..theme.palette().text
      }),
    }
  }))
  .padding([4, 10])
  .height(BAR_HEIGHT)
  .center_y(BAR_HEIGHT)
  .into()
}
fn row_height(entry: &MenuEntry) -> f32 {
  if matches!(entry, MenuEntry::Separator) {
    SEPARATOR_HEIGHT
  } else {
    ROW_HEIGHT
  }
}
fn row_start(items: &[MenuEntry], index: usize) -> f32 {
  PADDING + items.iter().take(index).map(row_height).sum::<f32>()
}
fn panel<'a, Message: 'a>(items: &[MenuEntry], selected: Option<usize>) -> Element<'a, Message> {
  let rows = items
    .iter()
    .enumerate()
    .map(|(index, entry)| {
      if matches!(entry, MenuEntry::Separator) {
        return iced::widget::container(iced::widget::rule::horizontal(1))
          .height(SEPARATOR_HEIGHT)
          .center_y(SEPARATOR_HEIGHT)
          .padding([0, 8])
          .into();
      }
      let enabled = entry_enabled(entry);
      let chosen = selected == Some(index) && enabled;
      let checked = match entry {
        MenuEntry::Check { checked: true, .. } => "✓",
        _ => "",
      };
      let accelerator = match entry {
        MenuEntry::Item {
          accelerator: Some(value),
          ..
        } => value.clone(),
        _ => String::new(),
      };
      let title = match entry {
        MenuEntry::Item { label, .. } | MenuEntry::Check { label, .. } | MenuEntry::Submenu { label, .. } => {
          label.clone()
        }
        _ => String::new(),
      };
      let arrow = if matches!(entry, MenuEntry::Submenu { .. }) {
        "›"
      } else {
        ""
      };
      let text = iced::widget::row![
        iced::widget::text(checked).width(16),
        iced::widget::text(title).width(Length::Fill).size(14),
        iced::widget::text(accelerator).size(12),
        iced::widget::text(arrow).width(12)
      ]
      .spacing(6)
      .align_y(iced::Alignment::Center);
      iced::widget::container(text)
        .height(ROW_HEIGHT)
        .center_y(ROW_HEIGHT)
        .padding([0, 8])
        .style(move |theme: &Theme| iced::widget::container::Style {
          text_color: Some(iced::Color {
            a: if enabled { 1.0 } else { 0.4 },
            ..theme.palette().text
          }),
          background: chosen.then(|| theme.extended_palette().primary.weak.color.into()),
          ..Default::default()
        })
        .into()
    })
    .collect::<Vec<Element<'a, Message>>>();
  iced::widget::container(iced::widget::column(rows))
    .padding(PADDING)
    .width(PANEL_WIDTH)
    .style(|theme: &Theme| iced::widget::container::Style {
      background: Some(theme.palette().background.into()),
      border: iced::Border {
        color: theme.extended_palette().background.strong.color,
        width: 1.0,
        radius: 4.0.into(),
      },
      shadow: iced::Shadow {
        color: iced::Color {
          a: 0.18,
          ..iced::Color::BLACK
        },
        offset: Vector::new(0.0, 3.0),
        blur_radius: 8.0,
      },
      ..Default::default()
    })
    .into()
}
fn header_bounds(layout: Layout<'_>, translation: Vector) -> Vec<Rectangle> {
  layout
    .children()
    .next()
    .map(|row| row.children().map(|header| header.bounds() + translation).collect())
    .unwrap_or_default()
}
fn header_at(headers: &[Rectangle], position: Option<Point>) -> Option<usize> {
  let position = position?;
  headers.iter().position(|bounds| bounds.contains(position))
}
fn position(event: &Event, cursor: mouse::Cursor) -> Option<Point> {
  match event {
    Event::Touch(
      iced::touch::Event::FingerPressed { position, .. }
      | iced::touch::Event::FingerLifted { position, .. }
      | iced::touch::Event::FingerMoved { position, .. },
    ) => Some(*position),
    _ => cursor.position(),
  }
}
fn pressed(event: &Event) -> bool {
  matches!(
    event,
    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
      | Event::Touch(iced::touch::Event::FingerPressed { .. })
  )
}
fn released(event: &Event) -> bool {
  matches!(
    event,
    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
      | Event::Touch(iced::touch::Event::FingerLifted { .. })
  )
}
fn moved(event: &Event) -> bool {
  matches!(
    event,
    Event::Mouse(mouse::Event::CursorMoved { .. }) | Event::Touch(iced::touch::Event::FingerMoved { .. })
  )
}
fn key(event: &Event) -> Option<MenuKey> {
  let Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) = event else {
    return None;
  };
  use iced::keyboard::key::Named;
  let iced::keyboard::Key::Named(named) = key else {
    return None;
  };
  if modifiers.control() || modifiers.alt() || modifiers.logo() {
    return None;
  }
  Some(match named {
    Named::F10 if !modifiers.shift() => MenuKey::FocusBar,
    Named::ArrowLeft => MenuKey::Left,
    Named::ArrowRight => MenuKey::Right,
    Named::ArrowUp => MenuKey::Up,
    Named::ArrowDown => MenuKey::Down,
    Named::Home => MenuKey::Home,
    Named::End => MenuKey::End,
    Named::Enter | Named::Space => MenuKey::Activate,
    Named::Escape => MenuKey::Escape,
    Named::Tab => MenuKey::Tab,
    _ => return None,
  })
}
fn keyboard_event(event: &Event) -> bool {
  matches!(event, Event::Keyboard(_) | Event::InputMethod(_))
}

fn route_keyboard<Message>(
  state: &mut MenuState,
  model: &MenuModel,
  event: &Event,
  on_select: &dyn Fn(MenuAction) -> Message,
  shell: &mut Shell<'_, Message>,
) -> bool {
  if let Some(menu_key) = key(event) {
    if state.is_open() || menu_key == MenuKey::FocusBar {
      state.keyboard = true;
      if let Some(action) = state.session.key(model, menu_key) {
        shell.publish(on_select(action));
      }
      state.redraw(model, shell);
      shell.capture_event();
      return true;
    }
  }
  if state.is_open() && keyboard_event(event) {
    shell.capture_event();
    return true;
  }
  false
}

impl<'a, Message: 'a> Widget<Message, Theme, iced::Renderer> for MenuBar<'a, Message> {
  fn size(&self) -> Size<Length> {
    self.base.as_widget().size()
  }
  fn size_hint(&self) -> Size<Length> {
    self.base.as_widget().size_hint()
  }
  fn tag(&self) -> tree::Tag {
    tree::Tag::of::<MenuState>()
  }
  fn state(&self) -> tree::State {
    tree::State::new(MenuState::new())
  }
  fn children(&self) -> Vec<Tree> {
    vec![Tree::new(&self.base)]
  }
  fn diff(&self, tree: &mut Tree) {
    tree.state.downcast_mut::<MenuState>().reconcile(self.model);
    tree.diff_children(std::slice::from_ref(&self.base));
  }
  fn layout(&mut self, tree: &mut Tree, renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
    let node = self
      .base
      .as_widget_mut()
      .layout(&mut tree.children[0], renderer, limits);
    let state = tree.state.downcast_mut::<MenuState>();
    let layout = Layout::new(&node);
    let bar = layout.children().next().map(|row| row.bounds()).unwrap_or_default();
    state.ax_headers = Some((bar, header_bounds(layout, Vector::ZERO), MenuSpace::Widget));
    state.refresh_accessibility(self.model);
    node
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
    let state = tree.state.downcast_ref::<MenuState>();
    let headers = header_bounds(layout, Vector::ZERO);
    if let Some(bar) = layout.children().next() {
      renderer.fill_quad(
        renderer::Quad {
          bounds: bar.bounds(),
          ..Default::default()
        },
        theme.extended_palette().background.weak.color,
      );
    }
    for (index, bounds) in headers.iter().enumerate() {
      if state.session.top() == Some(index) || state.hovered == Some(index) {
        renderer.fill_quad(
          renderer::Quad {
            bounds: *bounds,
            ..Default::default()
          },
          theme.extended_palette().primary.weak.color,
        );
      }
    }
    self
      .base
      .as_widget()
      .draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
  }
  fn operate(
    &mut self,
    tree: &mut Tree,
    layout: Layout<'_>,
    renderer: &iced::Renderer,
    operation: &mut dyn widget::Operation,
  ) {
    let state = tree.state.downcast_mut::<MenuState>();
    let bar = layout.children().next().map(|row| row.bounds()).unwrap_or_default();
    state.ax_headers = Some((bar, header_bounds(layout, Vector::ZERO), MenuSpace::Widget));
    state.refresh_accessibility(self.model);
    state.modal_collection_requested = false;
    operation.custom(None, layout.bounds(), state);
    let collect_background = std::mem::take(&mut state.modal_collection_requested);
    if state.is_open() && !collect_background {
      return;
    }
    operation.traverse(&mut |operation| {
      self
        .base
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
    if input_trace_enabled() {
      let state = tree.state.downcast_ref::<MenuState>();
      trace_input_event(
        "menu_enter",
        event,
        shell.event_status(),
        format_args!(
          "open={} pending={} blocked={} candidate={:?}",
          state.is_open(),
          state.modal_boundary_pending,
          state.body_blocked(),
          key(event)
        ),
      );
    }
    if shell.is_event_captured() {
      return;
    }
    let state = tree.state.downcast_mut::<MenuState>();
    state.reconcile(self.model);
    state.ax_headers = Some((
      layout.children().next().map(|row| row.bounds()).unwrap_or_default(),
      header_bounds(layout, Vector::ZERO),
      MenuSpace::Widget,
    ));
    state.refresh_accessibility(self.model);
    let handled_native = state.native_pending();
    let native = state.native_actions(self.model);
    for action in native {
      shell.publish((self.on_select)(action));
    }
    if handled_native {
      state.redraw(self.model, shell);
    }
    if state.consume_owned_pointer_end(self.model, event) {
      shell.capture_event();
      return;
    }
    if matches!(event, Event::Window(iced::window::Event::Unfocused)) && state.body_blocked() {
      state.defer_unfocus();
      state.redraw(self.model, shell);
    }
    let headers = header_bounds(layout, Vector::ZERO);
    let hit = header_at(&headers, position(event, cursor));
    let consumed = route_keyboard(state, self.model, event, self.on_select.as_ref(), shell);
    trace_input_event(
      "menu_route",
      event,
      shell.event_status(),
      format_args!(
        "consumed={consumed} open={} pending={} blocked={}",
        state.is_open(),
        state.modal_boundary_pending,
        state.body_blocked()
      ),
    );
    if consumed {
      return;
    }

    if pressed(event) && hit.is_some() {
      state.own_pointer_press(event);
      state.activation_pointer = pointer_press(event);
      state.keyboard = false;
      state.session.press_header(self.model, hit.unwrap());
      state.redraw(self.model, shell);
      shell.capture_event();
      return;
    }
    if released(event) && hit.is_some() {
      let path = [hit.unwrap()];
      if let Some(action) = state.release_action(self.model, event, Some(&path)) {
        shell.publish((self.on_select)(action));
      }
      state.redraw(self.model, shell);
      shell.capture_event();
      return;
    }
    if moved(event) && state.hovered != hit {
      state.hovered = hit;
      shell.request_redraw();
    }
    // The overlay may not exist yet for the first events in the same F10/open batch.
    if state.is_open() && matches!(event, Event::Mouse(_) | Event::Touch(_)) {
      state.own_pointer_press(event);
      if pressed(event) {
        state.close();
        state.redraw(self.model, shell);
      }
      shell.capture_event();
      return;
    }
    // Open menus suspend redraw-driven AX mailbox delivery. Closing synchronously purges
    // the old native routes through the private barrier; normal input must not wait for a redraw.
    if state.body_blocked() {
      trace_input_event(
        "body_blocked",
        event,
        shell.event_status(),
        format_args!("open={} pending={}", state.is_open(), state.modal_boundary_pending),
      );
      return;
    }
    if state.take_deferred_unfocus() {
      self.base.as_widget_mut().update(
        &mut tree.children[0],
        &Event::Window(iced::window::Event::Unfocused),
        layout,
        cursor,
        renderer,
        clipboard,
        shell,
        viewport,
      );
      if matches!(event, Event::Window(iced::window::Event::Unfocused)) {
        return;
      }
    }
    self.base.as_widget_mut().update(
      &mut tree.children[0],
      event,
      layout,
      cursor,
      renderer,
      clipboard,
      shell,
      viewport,
    );
  }
  fn mouse_interaction(
    &self,
    tree: &Tree,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    viewport: &Rectangle,
    renderer: &iced::Renderer,
  ) -> mouse::Interaction {
    if header_at(&header_bounds(layout, Vector::ZERO), cursor.position()).is_some() {
      mouse::Interaction::Pointer
    } else {
      self
        .base
        .as_widget()
        .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }
  }
  fn overlay<'b>(
    &'b mut self,
    tree: &'b mut Tree,
    layout: Layout<'b>,
    renderer: &iced::Renderer,
    viewport: &Rectangle,
    translation: Vector,
  ) -> Option<overlay::Element<'b, Message, Theme, iced::Renderer>> {
    let state = tree.state.downcast_mut::<MenuState>();
    state.reconcile(self.model);
    if state.is_open() {
      Some(overlay::Element::new(Box::new(MenuOverlay {
        model: self.model,
        on_select: &self.on_select,
        panels: &mut self.panels,
        state,
        headers: header_bounds(layout, translation),
        bar_bounds: layout
          .children()
          .next()
          .map(|row| row.bounds() + translation)
          .unwrap_or_default(),
      })))
    } else {
      self
        .base
        .as_widget_mut()
        .overlay(&mut tree.children[0], layout, renderer, viewport, translation)
    }
  }
}
impl<'a, Message: 'a> From<MenuBar<'a, Message>> for Element<'a, Message> {
  fn from(bar: MenuBar<'a, Message>) -> Self {
    Element::new(bar)
  }
}

struct MenuOverlay<'b, 'a, Message> {
  model: &'a MenuModel,
  on_select: &'b (dyn Fn(MenuAction) -> Message + 'a),
  panels: &'b mut Vec<Element<'a, Message>>,
  state: &'b mut MenuState,
  headers: Vec<Rectangle>,
  bar_bounds: Rectangle,
}
impl<Message> MenuOverlay<'_, '_, Message> {
  fn path_at(&self, layout: Layout<'_>, position: Option<Point>) -> Option<(usize, usize)> {
    if !self.state.geometry_ready() {
      return None;
    }
    let position = position?;
    for (level, pane) in layout.children().enumerate().collect::<Vec<_>>().into_iter().rev() {
      if level >= self.state.scroll.len() || !pane.bounds().contains(position) {
        continue;
      }
      let column = pane.children().next()?.children().next()?;
      if let Some(index) = column.children().position(|row| row.bounds().contains(position)) {
        return Some((level, index));
      }
    }
    None
  }
  fn action_path(&self, hit: Option<(usize, usize)>, header: Option<usize>) -> Option<Vec<usize>> {
    if let Some((level, index)) = hit {
      let mut path = self.state.session.open_path().get(..=level)?.to_vec();
      path.push(index);
      Some(path)
    } else {
      header.map(|index| vec![index])
    }
  }
}
impl<'a, Message: 'a> overlay::Overlay<Message, Theme, iced::Renderer> for MenuOverlay<'_, 'a, Message> {
  fn layout(&mut self, renderer: &iced::Renderer, bounds: Size) -> layout::Node {
    let path = self.state.session.open_path().to_vec();
    let shared = self
      .state
      .last_path
      .iter()
      .zip(&path)
      .take_while(|(a, b)| a == b)
      .count();
    self.state.scroll.truncate(shared);
    self.state.panel_trees.truncate(shared);
    self.state.scroll.resize(path.len(), 0.0);
    self.state.panel_trees.resize_with(path.len(), Tree::empty);
    self.state.last_path = path.clone();
    *self.panels = (0..path.len())
      .filter_map(|level| {
        self
          .model
          .items(&path[..=level])
          .map(|items| panel(items, self.state.session.selected(level)))
      })
      .collect();
    let mut nodes: Vec<layout::Node> = Vec::new();
    for (level, panel) in self.panels.iter_mut().enumerate() {
      self.state.panel_trees[level].diff(panel.as_widget());
      let width = PANEL_WIDTH.min((bounds.width - 2.0 * MARGIN).max(1.0));
      let full = panel.as_widget_mut().layout(
        &mut self.state.panel_trees[level],
        renderer,
        &layout::Limits::new(Size::ZERO, Size::new(width, f32::INFINITY)).width(width),
      );
      let height = full.size().height.min((bounds.height - 2.0 * MARGIN).max(1.0));
      let max_scroll = (full.size().height - height).max(0.0);
      let items = self.model.items(&path[..=level]).unwrap_or_default();
      let mut scroll = self.state.scroll[level].clamp(0.0, max_scroll);
      if self.state.keyboard {
        if let Some(selected) = self.state.session.selected(level) {
          let start = row_start(items, selected);
          let end = start + items.get(selected).map(row_height).unwrap_or(0.0);
          if start < scroll + PADDING {
            scroll = (start - PADDING).max(0.0);
          }
          if end > scroll + height - PADDING {
            scroll = (end - height + PADDING).clamp(0.0, max_scroll);
          }
        }
      }
      self.state.scroll[level] = scroll;
      let anchor = if level == 0 {
        self
          .headers
          .get(path[0])
          .map(|r| Point::new(r.x, r.y + r.height))
          .unwrap_or(Point::ORIGIN)
      } else {
        let previous = &nodes[level - 1];
        let parent_items = self.model.items(&path[..level]).unwrap_or_default();
        Point::new(
          previous.bounds().x + previous.size().width,
          previous.bounds().y + row_start(parent_items, path[level]) - self.state.scroll[level - 1],
        )
      };
      let mut x = anchor.x;
      if x + width > bounds.width - MARGIN {
        x = if level == 0 {
          bounds.width - MARGIN - width
        } else {
          nodes[level - 1].bounds().x - width
        };
      }
      x = x.clamp(MARGIN, (bounds.width - width - MARGIN).max(MARGIN));
      let y = anchor.y.clamp(MARGIN, (bounds.height - height - MARGIN).max(MARGIN));
      let child = full.move_to(Point::new(0.0, -scroll));
      nodes.push(layout::Node::with_children(Size::new(width, height), vec![child]).move_to(Point::new(x, y)));
    }
    let root = layout::Node::with_children(bounds, nodes);
    self.state.ax_panels = Layout::new(&root)
      .children()
      .enumerate()
      .map(|(level, pane)| menu_ax::Panel {
        path: path[..=level].to_vec(),
        bounds: pane.bounds(),
        rows: pane
          .children()
          .next()
          .and_then(|content| content.children().next())
          .map(|column| column.children().map(|row| row.bounds()).collect())
          .unwrap_or_default(),
      })
      .collect();
    self.state.ax_headers = Some((self.bar_bounds, self.headers.clone(), MenuSpace::Window));
    self.state.refresh_accessibility(self.model);
    root
  }
  fn draw(
    &self,
    renderer: &mut iced::Renderer,
    theme: &Theme,
    style: &renderer::Style,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
  ) {
    for ((panel, tree), pane) in self.panels.iter().zip(&self.state.panel_trees).zip(layout.children()) {
      if let Some(content) = pane.children().next() {
        renderer.with_layer(pane.bounds(), |renderer| {
          panel
            .as_widget()
            .draw(tree, renderer, theme, style, content, cursor, &pane.bounds())
        });
      }
    }
  }
  fn operate(&mut self, layout: Layout<'_>, _renderer: &iced::Renderer, operation: &mut dyn widget::Operation) {
    self.state.ax_headers = Some((self.bar_bounds, self.headers.clone(), MenuSpace::Window));
    self.state.refresh_accessibility(self.model);
    let mut semantics = menu_ax::OverlayTree {
      tree: self.state.accessibility_tree(),
    };
    operation.custom(None, layout.bounds(), &mut semantics);
  }
  fn update(
    &mut self,
    event: &Event,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    _renderer: &iced::Renderer,
    _clipboard: &mut dyn Clipboard,
    shell: &mut Shell<'_, Message>,
  ) {
    let changed = self.state.native_pending();
    for action in self.state.native_actions(self.model) {
      shell.publish((self.on_select)(action));
    }
    if changed {
      self.state.redraw(self.model, shell);
    }
    if self.state.consume_owned_pointer_end(self.model, event) {
      shell.capture_event();
      return;
    }
    if matches!(event, Event::Window(iced::window::Event::Unfocused)) {
      self.state.defer_unfocus();
      self.state.redraw(self.model, shell);
      return;
    }
    trace_input_event(
      "overlay_enter",
      event,
      shell.event_status(),
      format_args!(
        "open={} pending={} candidate={:?}",
        self.state.is_open(),
        self.state.modal_boundary_pending,
        key(event)
      ),
    );
    let consumed = route_keyboard(self.state, self.model, event, self.on_select, shell);
    trace_input_event(
      "overlay_route",
      event,
      shell.event_status(),
      format_args!(
        "consumed={consumed} open={} pending={}",
        self.state.is_open(),
        self.state.modal_boundary_pending
      ),
    );
    if consumed {
      return;
    }

    self.state.own_pointer_press(event);
    let position = position(event, cursor);
    let header = header_at(&self.headers, position);
    let hit = self.path_at(layout, position);
    if !self.state.geometry_ready() && header.is_none() && matches!(event, Event::Mouse(_) | Event::Touch(_)) {
      // A previous event changed the submenu path or closed it. Old pane geometry is no longer authoritative.
      if released(event) {
        self.state.release_action(self.model, event, None);
      }
      self.state.redraw(self.model, shell);
      shell.capture_event();
      return;
    }
    if moved(event) {
      self.state.keyboard = false;
      if let Some(header) = header {
        self.state.session.hover_header(self.model, header);
      } else if let Some((level, index)) = hit {
        self.state.session.hover_item(self.model, level, index);
      }
      self.state.redraw(self.model, shell);
      shell.capture_event();
      return;
    }
    if pressed(event) {
      self.state.activation_pointer = pointer_press(event);
      self.state.keyboard = false;
      if let Some(header) = header {
        self.state.session.press_header(self.model, header);
      } else if let Some((level, index)) = hit {
        self.state.session.press_item(self.model, level, index);
      } else {
        self.state.close();
      }
      self.state.redraw(self.model, shell);
      shell.capture_event();
      return;
    }
    if released(event) {
      let path = self.action_path(hit, header);
      if let Some(action) = self.state.release_action(self.model, event, path.as_deref()) {
        shell.publish((self.on_select)(action));
      }
      self.state.redraw(self.model, shell);
      shell.capture_event();
      return;
    }
    if let Event::Mouse(mouse::Event::WheelScrolled { delta }) = event {
      self.state.keyboard = false;
      if let Some((level, _)) = hit {
        let y = match delta {
          mouse::ScrollDelta::Lines { y, .. } => *y * ROW_HEIGHT,
          mouse::ScrollDelta::Pixels { y, .. } => *y,
        };
        if self.state.scroll_by(level, y) {
          self.state.redraw(self.model, shell);
        }
      }
      shell.capture_event();
      return;
    }
    // While modal, swallow all pointer buttons/releases (including right-click) without activating content.
    if matches!(event, Event::Mouse(_) | Event::Touch(_)) {
      shell.capture_event();
    }
  }
  fn mouse_interaction(
    &self,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    _renderer: &iced::Renderer,
  ) -> mouse::Interaction {
    if self.path_at(layout, cursor.position()).is_some() || header_at(&self.headers, cursor.position()).is_some() {
      mouse::Interaction::Pointer
    } else {
      mouse::Interaction::None
    }
  }
  fn index(&self) -> f32 {
    10_000.0
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  fn model() -> MenuModel {
    MenuModel::new(crate::desktop::MenuSpec {
      items: vec![MenuEntry::Submenu {
        label: "File".into(),
        enabled: true,
        items: vec![MenuEntry::item("run", "Run")],
      }],
    })
    .unwrap()
  }
  fn keyboard(key: iced::keyboard::Key) -> Event {
    Event::Keyboard(iced::keyboard::Event::KeyPressed {
      modified_key: key.clone(),
      key,
      physical_key: iced::keyboard::key::Physical::Unidentified(iced::keyboard::key::NativeCode::Unidentified),
      location: iced::keyboard::Location::Standard,
      modifiers: iced::keyboard::Modifiers::empty(),
      text: None,
      repeat: false,
    })
  }
  #[test]
  fn same_batch_f10_letters_arrows_and_tab_never_reach_background() {
    let model = model();
    let mut state = MenuState::new();
    state.reconcile(&model);
    let mut messages = Vec::new();
    let mut background = 0;
    use iced::keyboard::{Key, key::Named};
    for key in [
      Key::Named(Named::F10),
      Key::Character("x".into()),
      Key::Named(Named::ArrowDown),
      Key::Named(Named::Tab),
    ] {
      let mut shell = Shell::new(iced::iced_core::shell::Waker::noop(), &mut messages);
      if !route_keyboard(&mut state, &model, &keyboard(key), &|action| action, &mut shell) {
        background += 1;
      }
      assert!(shell.is_event_captured());
    }
    assert_eq!(background, 0);
    assert!(messages.is_empty());
    assert!(!state.is_open());
    let mut shell = Shell::new(iced::iced_core::shell::Waker::noop(), &mut messages);
    assert!(!route_keyboard(
      &mut state,
      &model,
      &keyboard(Key::Character("x".into())),
      &|action| action,
      &mut shell
    ));
  }
  #[test]
  fn menu_activation_publishes_one_normal_business_message() {
    let model = model();
    let mut state = MenuState::new();
    state.reconcile(&model);
    let mut messages = Vec::new();
    use iced::keyboard::{Key, key::Named};
    for named in [Named::F10, Named::ArrowDown, Named::Enter] {
      let mut shell = Shell::new(iced::iced_core::shell::Waker::noop(), &mut messages);
      assert!(route_keyboard(
        &mut state,
        &model,
        &keyboard(Key::Named(named)),
        &|action| action,
        &mut shell
      ));
    }
    assert_eq!(
      messages,
      vec![MenuAction {
        id: "run".into(),
        checked: None
      }]
    );
    assert!(!state.is_open());
  }
  #[test]
  fn caps_and_num_lock_do_not_block_f10_but_command_chords_do_not_activate() {
    use iced::keyboard::{Key, Modifiers, key::Named};
    let mut event = keyboard(Key::Named(Named::F10));
    if let Event::Keyboard(iced::keyboard::Event::KeyPressed { modifiers, .. }) = &mut event {
      *modifiers = Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK;
    }
    assert_eq!(key(&event), Some(MenuKey::FocusBar));
    for modifier in [Modifiers::SHIFT, Modifiers::CTRL, Modifiers::ALT, Modifiers::LOGO] {
      if let Event::Keyboard(iced::keyboard::Event::KeyPressed { modifiers, .. }) = &mut event {
        *modifiers = modifier;
      }
      assert_eq!(key(&event), None);
    }
    let model = model();
    let mut state = MenuState::new();
    state.session.key(&model, MenuKey::FocusBar);
    state.session.key(&model, MenuKey::Down);
    let mut event = keyboard(Key::Named(Named::Enter));
    if let Event::Keyboard(iced::keyboard::Event::KeyPressed { modifiers, .. }) = &mut event {
      *modifiers = Modifiers::CTRL;
    }
    let mut messages = Vec::new();
    let mut shell = Shell::new(iced::iced_core::shell::Waker::noop(), &mut messages);
    assert!(route_keyboard(&mut state, &model, &event, &|action| action, &mut shell));
    assert!(messages.is_empty());
    assert!(state.is_open());
  }
  #[test]
  fn changed_submenu_or_closed_session_cannot_hit_old_overlay_geometry_or_scroll() {
    let model = MenuModel::new(crate::desktop::MenuSpec {
      items: vec![MenuEntry::Submenu {
        label: "File".into(),
        enabled: true,
        items: vec![MenuEntry::Submenu {
          label: "More".into(),
          enabled: true,
          items: vec![MenuEntry::item("nested", "Nested")],
        }],
      }],
    })
    .unwrap();
    let mut state = MenuState::new();
    state.session.key(&model, MenuKey::FocusBar);
    state.last_path = vec![0];
    state.scroll = vec![0.0];
    let row = layout::Node::new(Size::new(100.0, 30.0));
    let column = layout::Node::with_children(Size::new(100.0, 30.0), vec![row]);
    let content = layout::Node::with_children(Size::new(100.0, 30.0), vec![column]);
    let pane = layout::Node::with_children(Size::new(100.0, 30.0), vec![content]);
    let root = layout::Node::with_children(Size::new(400.0, 400.0), vec![pane]);
    let mut panels = Vec::new();
    let callback = |action| action;
    let mut overlay = MenuOverlay {
      model: &model,
      on_select: &callback,
      panels: &mut panels,
      state: &mut state,
      headers: vec![],
      bar_bounds: Rectangle::default(),
    };
    assert_eq!(
      overlay.path_at(Layout::new(&root), Some(Point::new(10.0, 10.0))),
      Some((0, 0))
    );
    overlay.state.session.hover_item(&model, 0, 0);
    assert_eq!(overlay.state.session.open_path(), &[0, 0]);
    assert!(
      overlay
        .path_at(Layout::new(&root), Some(Point::new(10.0, 10.0)))
        .is_none()
    );
    assert!(!overlay.state.scroll_by(0, 10.0));
    overlay.state.close();
    assert!(
      overlay
        .path_at(Layout::new(&root), Some(Point::new(10.0, 10.0)))
        .is_none()
    );
    assert!(!overlay.state.scroll_by(0, 10.0));
  }
  #[test]
  fn transient_open_close_purges_synchronously_before_background_resume() {
    use iced::keyboard::{Key, key::Named};
    let model = model();
    let mut state = MenuState::new();
    state.reconcile(&model);
    let mut messages = Vec::new();
    let queued = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let pending = queued.clone();
    let transitions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = transitions.clone();
    state.set_modal_barrier(std::sync::Arc::new(move |open| {
      pending.lock().unwrap().clear();
      seen.lock().unwrap().push(open);
    }));
    for named in [Named::F10, Named::Escape] {
      queued.lock().unwrap().push(1);
      let mut shell = Shell::new(iced::iced_core::shell::Waker::noop(), &mut messages);
      assert!(route_keyboard(
        &mut state,
        &model,
        &keyboard(Key::Named(named)),
        &|action| action,
        &mut shell
      ));
      assert!(queued.lock().unwrap().is_empty());
    }
    assert!(!state.is_open());
    assert!(
      !state.body_blocked(),
      "following keyboard input cannot depend on a later Collect"
    );
    assert_eq!(*transitions.lock().unwrap(), vec![false, true, false]);
    assert!(
      !state.take_modal_boundary(),
      "a later collector cannot purge newly admitted actions for an already completed boundary"
    );
  }
  #[test]
  fn window_unfocus_is_replayed_once_after_the_synchronous_close_barrier() {
    let model = model();
    let mut state = MenuState::new();
    let transitions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = transitions.clone();
    state.set_modal_barrier(std::sync::Arc::new(move |open| seen.lock().unwrap().push(open)));
    state.session.key(&model, MenuKey::FocusBar);
    state.sync_modal_boundary();
    state.defer_unfocus();
    assert!(!state.is_open());
    assert!(!state.body_blocked());
    assert_eq!(*transitions.lock().unwrap(), vec![false, true, false]);
    assert!(state.take_deferred_unfocus());
    assert!(!state.take_deferred_unfocus());
  }
  #[test]
  fn menu_without_native_accessibility_does_not_wait_for_an_absent_collector() {
    let model = model();
    let mut state = MenuState::new();
    state.session.key(&model, MenuKey::FocusBar);
    state.sync_modal_boundary();
    assert!(state.body_blocked());
    state.close();
    assert!(!state.body_blocked());
  }
  #[test]
  fn real_user_interface_text_then_f10_batch_matches_separate_f10() {
    use iced::advanced::renderer::Headless;
    use iced::iced_winit::runtime::user_interface::{Cache, UserInterface};
    use iced::keyboard::{Key, key::Named};
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Message {
      Name(String),
      Menu(MenuAction),
    }
    struct FocusInput;
    impl widget::Operation for FocusInput {
      fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn widget::Operation)) {
        operate(self);
      }
      fn focusable(
        &mut self,
        _id: Option<&widget::Id>,
        _bounds: Rectangle,
        state: &mut dyn widget::operation::Focusable,
      ) {
        state.focus();
      }
    }
    #[derive(Default)]
    struct ReadMenu(bool);
    impl widget::Operation for ReadMenu {
      fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn widget::Operation)) {
        operate(self);
      }
      fn custom(&mut self, _id: Option<&widget::Id>, _bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(menu) = state.downcast_ref::<MenuState>() {
          self.0 = menu.is_open();
        }
      }
    }
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let mut renderer = runtime
      .block_on(<iced::Renderer as Headless>::new(
        iced::Font::DEFAULT,
        iced::Pixels(14.0),
        Some("tiny-skia"),
      ))
      .expect("software renderer requires no native window or GPU");
    let model = MenuModel::new(crate::desktop::MenuSpec {
      items: vec![MenuEntry::Submenu {
        label: "File".into(),
        enabled: true,
        items: vec![
          MenuEntry::item("run", "Run"),
          MenuEntry::Check {
            id: "checked".into(),
            label: "Checked".into(),
            enabled: true,
            checked: false,
          },
        ],
      }],
    })
    .unwrap();
    let view = || -> Element<'_, Message> {
      let body = crate::a11y::text_input("input", "Name", "", Message::Name);
      let element: Element<'_, Message> = menu_bar(&model, Message::Menu, body).into();
      // IcedAppProxy adds mapped message wrappers around the ordinary application view.
      element.map(|message| message).map(|message| message)
    };
    let mut character = keyboard(Key::Character("c".into()));
    if let Event::Keyboard(iced::keyboard::Event::KeyPressed { text, .. }) = &mut character {
      *text = Some("c".into());
    }
    let waker = iced::iced_core::shell::Waker::noop();
    let mut clipboard = iced::advanced::clipboard::Null;
    for batch in [
      vec![keyboard(Key::Named(Named::F10))],
      vec![character.clone(), keyboard(Key::Named(Named::F10))],
      vec![
        Event::InputMethod(iced::advanced::input_method::Event::Commit("c".into())),
        keyboard(Key::Named(Named::F10)),
      ],
    ] {
      let mut ui = UserInterface::build(view(), Size::new(800.0, 500.0), Cache::new(), &mut renderer);
      ui.operate(&renderer, &mut FocusInput);
      let mut messages = Vec::new();
      let (_, statuses) = ui.update(
        &waker,
        &batch,
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard,
        &mut messages,
      );
      assert_eq!(statuses.len(), batch.len(), "every input event must have a status");
      let mut menu = ReadMenu::default();
      ui.operate(&renderer, &mut menu);
      assert!(
        menu.0,
        "F10 must open after an input in the same UI batch; statuses={statuses:?}, messages={messages:?}"
      );
      assert_eq!(statuses.last(), Some(&iced::event::Status::Captured));
      if batch.len() > 1 {
        assert_eq!(messages, vec![Message::Name("c".into())]);
      }
    }
    let mut ui = UserInterface::build(view(), Size::new(800.0, 500.0), Cache::new(), &mut renderer);
    ui.operate(&renderer, &mut FocusInput);
    let batch = vec![
      character,
      keyboard(Key::Named(Named::F10)),
      keyboard(Key::Named(Named::ArrowDown)),
      keyboard(Key::Named(Named::ArrowDown)),
      keyboard(Key::Named(Named::Space)),
    ];
    let mut messages = Vec::new();
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
      vec![
        Message::Name("c".into()),
        Message::Menu(MenuAction {
          id: "checked".into(),
          checked: Some(true)
        })
      ]
    );
    let mut menu = ReadMenu::default();
    ui.operate(&renderer, &mut menu);
    assert!(!menu.0);
  }
  #[test]
  fn diagnostic_event_summary_does_not_record_input_or_ime_contents() {
    let secret = "private-test-text";
    let event = keyboard(iced::keyboard::Key::Character(secret.into()));
    let class = input_event_class(&event).unwrap();
    assert!(class.contains("Character"));
    assert!(!class.contains(secret));
    let class = input_event_class(&Event::InputMethod(iced::advanced::input_method::Event::Commit(
      secret.into(),
    )))
    .unwrap();
    assert_eq!(class, "ime=Commit");
  }
  #[test]
  fn real_user_interface_close_boundary_preserves_following_text_once() {
    use iced::advanced::renderer::Headless;
    use iced::iced_winit::runtime::user_interface::{Cache, UserInterface};
    use iced::keyboard::{Key, key::Named};
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Message {
      Name(String),
      Menu(MenuAction),
    }
    struct FocusInput;
    impl widget::Operation for FocusInput {
      fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn widget::Operation)) {
        operate(self);
      }
      fn focusable(
        &mut self,
        _id: Option<&widget::Id>,
        _bounds: Rectangle,
        state: &mut dyn widget::operation::Focusable,
      ) {
        state.focus();
      }
    }
    struct PublishBoundary;
    impl widget::Operation for PublishBoundary {
      fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn widget::Operation)) {
        operate(self);
      }
      fn custom(&mut self, _id: Option<&widget::Id>, _bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(menu) = state.downcast_mut::<MenuState>() {
          menu.set_modal_barrier(std::sync::Arc::new(|_| {}));
          menu.take_modal_boundary();
        }
      }
    }
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let mut renderer = runtime
      .block_on(<iced::Renderer as Headless>::new(
        iced::Font::DEFAULT,
        iced::Pixels(14.0),
        Some("tiny-skia"),
      ))
      .expect("software renderer");
    let model = model();
    let waker = iced::iced_core::shell::Waker::noop();
    let mut clipboard = iced::advanced::clipboard::Null;
    let mut character = keyboard(Key::Character("b".into()));
    if let Event::Keyboard(iced::keyboard::Event::KeyPressed { text, .. }) = &mut character {
      *text = Some("b".into());
    }
    for initially_open in [false, true] {
      for close_key in [Named::Tab, Named::Escape] {
        for text_event in [
          character.clone(),
          Event::InputMethod(iced::advanced::input_method::Event::Commit("b".into())),
        ] {
          let view: Element<'_, Message> = menu_bar(
            &model,
            Message::Menu,
            crate::a11y::text_input("input", "Name", "", Message::Name),
          )
          .into();
          let mut ui = UserInterface::build(view, Size::new(800.0, 500.0), Cache::new(), &mut renderer);
          ui.operate(&renderer, &mut PublishBoundary);
          ui.operate(&renderer, &mut FocusInput);
          let mut messages = Vec::new();
          if initially_open {
            ui.update(
              &waker,
              &[keyboard(Key::Named(Named::F10))],
              mouse::Cursor::Unavailable,
              &mut renderer,
              &mut clipboard,
              &mut messages,
            );
            ui.operate(&renderer, &mut PublishBoundary);
          }
          let mut batch = Vec::new();
          if !initially_open {
            batch.push(keyboard(Key::Named(Named::F10)));
          }
          batch.push(keyboard(Key::Named(close_key)));
          batch.push(text_event);
          let (_, statuses) = ui.update(
            &waker,
            &batch,
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard,
            &mut messages,
          );
          assert_eq!(
            statuses.len(),
            batch.len(),
            "closing an overlay must not truncate the remaining batch"
          );
          assert_eq!(
            messages,
            vec![Message::Name("b".into())],
            "closed-menu typeahead must follow the synchronous barrier immediately"
          );
          ui.operate(&renderer, &mut PublishBoundary);
          let redraw = Event::Window(iced::window::Event::RedrawRequested(
            iced::iced_core::time::Instant::now(),
          ));
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
            vec![Message::Name("b".into())],
            "input after Tab closed the menu must survive the native-action purge barrier"
          );
          messages.clear();
          ui.update(
            &waker,
            &[redraw],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard,
            &mut messages,
          );
          assert!(messages.is_empty(), "a later redraw cannot replay the input again");
        }
      }
    }
  }
  #[test]
  fn dismissed_pointer_end_is_scoped_to_button_or_finger_and_never_consumes_text() {
    let model = model();
    let mut state = MenuState::new();
    let down = Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
    state.own_pointer_press(&down);
    assert!(!state.consume_owned_pointer_end(&model, &keyboard(iced::keyboard::Key::Character("b".into()))));
    assert!(!state.consume_owned_pointer_end(
      &model,
      &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right))
    ));
    assert!(state.consume_owned_pointer_end(&model, &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))));
    assert!(!state.consume_owned_pointer_end(&model, &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))));
    let finger = iced::touch::Finger(1);
    let position = Point::ORIGIN;
    state.own_pointer_press(&Event::Touch(iced::touch::Event::FingerPressed {
      id: finger,
      position,
    }));
    assert!(!state.consume_owned_pointer_end(
      &model,
      &Event::Touch(iced::touch::Event::FingerLifted {
        id: iced::touch::Finger(2),
        position
      })
    ));
    assert!(state.consume_owned_pointer_end(
      &model,
      &Event::Touch(iced::touch::Event::FingerLost { id: finger, position })
    ));
    state.own_pointer_press(&down);
    assert!(!state.consume_owned_pointer_end(&model, &down));
    assert!(
      !state.consume_owned_pointer_end(&model, &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))),
      "a fresh normal gesture cannot inherit an old cancellation"
    );
  }
  #[test]
  fn real_user_interface_dismissed_menu_release_does_not_trigger_background_mouse_area() {
    use iced::advanced::renderer::Headless;
    use iced::iced_winit::runtime::user_interface::{Cache, UserInterface};
    use iced::keyboard::{Key, key::Named};
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Message {
      Release,
      Menu(MenuAction),
    }
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let mut renderer = runtime
      .block_on(<iced::Renderer as Headless>::new(
        iced::Font::DEFAULT,
        iced::Pixels(14.0),
        Some("tiny-skia"),
      ))
      .expect("software renderer");
    let model = model();
    let waker = iced::iced_core::shell::Waker::noop();
    let mut clipboard = iced::advanced::clipboard::Null;
    let cursor = mouse::Cursor::Available(Point::new(600.0, 400.0));
    for separate_batches in [false, true] {
      let body = iced::widget::mouse_area(
        iced::widget::container(iced::widget::space())
          .width(Length::Fill)
          .height(Length::Fill),
      )
      .on_release(Message::Release);
      let view: Element<'_, Message> = menu_bar(&model, Message::Menu, body).into();
      let mut ui = UserInterface::build(view, Size::new(800.0, 500.0), Cache::new(), &mut renderer);
      let mut messages = Vec::new();
      ui.update(
        &waker,
        &[keyboard(Key::Named(Named::F10))],
        cursor,
        &mut renderer,
        &mut clipboard,
        &mut messages,
      );
      let down = Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
      let up = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
      if separate_batches {
        ui.update(
          &waker,
          &[down.clone()],
          cursor,
          &mut renderer,
          &mut clipboard,
          &mut messages,
        );
        ui.update(
          &waker,
          &[up.clone()],
          cursor,
          &mut renderer,
          &mut clipboard,
          &mut messages,
        );
      } else {
        let (_, statuses) = ui.update(
          &waker,
          &[down.clone(), up.clone()],
          cursor,
          &mut renderer,
          &mut clipboard,
          &mut messages,
        );
        assert_eq!(statuses.len(), 2);
      }
      assert!(
        messages.is_empty(),
        "the release of a menu-consumed outside down cannot activate mouse_area.on_release"
      );
      ui.update(
        &waker,
        &[down, up],
        cursor,
        &mut renderer,
        &mut clipboard,
        &mut messages,
      );
      assert_eq!(
        messages,
        vec![Message::Release],
        "a new complete background gesture remains usable"
      );
    }
  }
}
