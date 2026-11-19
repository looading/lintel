use lintel_lib_iced::advanced::{
  Clipboard, Shell,
  layout::{self, Layout},
  renderer,
  widget::{self, Widget},
};
use lintel_lib_iced::{Element, Event, Length, Point, Rectangle, Size, iced_core, mouse};

/// A widget that detects visibility and mouse interactions.
///
/// It combines functionality of a visibility sensor (detecting when content enters/exits the viewport)
/// and a mouse area (detecting clicks, hovers, movement, etc.).
///
/// # Example
/// ```no_run
/// WidgetSensor::new(content)
///     .on_appear(Message::ElementAppeared)
///     .on_click(Message::ElementClicked)
/// ```
pub struct WidgetSensor<'a, Message, Theme, Renderer> {
  content: Element<'a, Message, Theme, Renderer>,
  /// Message to emit when the widget enters the viewport.
  on_appear: Option<Box<dyn FnMut() -> Option<Message> + 'a>>,
  /// Message to emit when the widget exits the viewport.
  on_disappear: Option<Box<dyn FnMut() -> Option<Message> + 'a>>,
  /// Message to emit when the widget enters the viewport for the first time.
  on_first_appear: Option<Box<dyn FnMut() -> Option<Message> + 'a>>,
  /// Message to emit when the widget is clicked (left mouse button).
  on_click: Option<Box<dyn FnMut() -> Option<Message> + 'a>>,
  /// Message to emit when a click occurs outside the widget's bounds.
  on_click_outside: Option<Box<dyn FnMut() -> Option<Message> + 'a>>,
  /// Unique key to reset the internal state (like `has_appeared`).
  key: Option<String>,
  /// Message to emit when the left mouse button is pressed.
  on_press: Option<Box<dyn FnMut() -> Option<Message> + 'a>>,
  /// Message to emit when the left mouse button is released.
  on_release: Option<Box<dyn FnMut() -> Option<Message> + 'a>>,
  /// Message to emit on double click.
  on_double_click: Option<Box<dyn FnMut() -> Option<Message> + 'a>>,
  /// Message to emit when the right mouse button is pressed.
  on_right_press: Option<Box<dyn FnMut() -> Option<Message> + 'a>>,
  /// Message to emit when the right mouse button is released.
  on_right_release: Option<Box<dyn FnMut() -> Option<Message> + 'a>>,
  /// Message to emit when the middle mouse button is pressed.
  on_middle_press: Option<Box<dyn FnMut() -> Option<Message> + 'a>>,
  /// Message to emit when the middle mouse button is released.
  on_middle_release: Option<Box<dyn FnMut() -> Option<Message> + 'a>>,
  /// Callback to emit message on mouse scroll.
  on_scroll: Option<Box<dyn FnMut(mouse::ScrollDelta) -> Option<Message> + 'a>>,
  /// Message to emit when the mouse enters the widget area.
  on_enter: Option<Box<dyn FnMut() -> Option<Message> + 'a>>,
  /// Callback to emit message when the mouse moves within the widget area.
  on_move: Option<Box<dyn FnMut(Point) -> Option<Message> + 'a>>,
  /// Message to emit when the mouse exits the widget area.
  on_exit: Option<Box<dyn FnMut() -> Option<Message> + 'a>>,
  /// Custom mouse cursor interaction when hovering.
  interaction: Option<mouse::Interaction>,
}

impl<'a, Message, Theme, Renderer> WidgetSensor<'a, Message, Theme, Renderer>
where
  Message: 'a,
{
  /// Creates a new [`WidgetSensor`] with the given content.
  pub fn new(content: impl Into<Element<'a, Message, Theme, Renderer>>) -> Self {
    Self {
      content: content.into(),
      on_appear: None,
      on_disappear: None,
      on_first_appear: None,
      on_click: None,
      on_click_outside: None,
      key: None,
      on_press: None,
      on_release: None,
      on_double_click: None,
      on_right_press: None,
      on_right_release: None,
      on_middle_press: None,
      on_middle_release: None,
      on_scroll: None,
      on_enter: None,
      on_move: None,
      on_exit: None,
      interaction: None,
    }
  }

  /// Sets the message to emit when the widget enters the viewport.
  pub fn on_appear(mut self, message: Message) -> Self {
    let mut message = Some(message);
    self.on_appear = Some(Box::new(move || message.take()));
    self
  }

  pub fn on_appear_with(mut self, f: impl FnMut() -> Message + 'a) -> Self {
    let mut f = f;
    self.on_appear = Some(Box::new(move || Some(f())));
    self
  }

  pub fn on_appear_maybe(mut self, f: impl FnMut() -> Option<Message> + 'a) -> Self {
    self.on_appear = Some(Box::new(f));
    self
  }

  /// Sets the message to emit when the widget exits the viewport.
  pub fn on_disappear(mut self, message: Message) -> Self {
    let mut message = Some(message);
    self.on_disappear = Some(Box::new(move || message.take()));
    self
  }

  pub fn on_disappear_with(mut self, f: impl FnMut() -> Message + 'a) -> Self {
    let mut f = f;
    self.on_disappear = Some(Box::new(move || Some(f())));
    self
  }

  pub fn on_disappear_maybe(mut self, f: impl FnMut() -> Option<Message> + 'a) -> Self {
    self.on_disappear = Some(Box::new(f));
    self
  }

  /// Sets the message to emit when the widget enters the viewport for the first time.
  pub fn on_first_appear(mut self, message: Message) -> Self {
    let mut message = Some(message);
    self.on_first_appear = Some(Box::new(move || message.take()));
    self
  }

  pub fn on_first_appear_with(mut self, f: impl FnMut() -> Message + 'a) -> Self {
    let mut f = f;
    self.on_first_appear = Some(Box::new(move || Some(f())));
    self
  }

  pub fn on_first_appear_maybe(mut self, f: impl FnMut() -> Option<Message> + 'a) -> Self {
    self.on_first_appear = Some(Box::new(f));
    self
  }

  /// Sets the message to emit when the widget is clicked (left mouse button).
  pub fn on_click(mut self, message: Message) -> Self {
    let mut message = Some(message);
    self.on_click = Some(Box::new(move || message.take()));
    self
  }

  pub fn on_click_with(mut self, f: impl FnMut() -> Message + 'a) -> Self {
    let mut f = f;
    self.on_click = Some(Box::new(move || Some(f())));
    self
  }

  pub fn on_click_maybe(mut self, f: impl FnMut() -> Option<Message> + 'a) -> Self {
    self.on_click = Some(Box::new(f));
    self
  }

  /// Sets the message to emit when a click occurs outside the widget's bounds.
  pub fn on_click_outside(mut self, message: Message) -> Self {
    let mut message = Some(message);
    self.on_click_outside = Some(Box::new(move || message.take()));
    self
  }

  pub fn on_click_outside_with(mut self, f: impl FnMut() -> Message + 'a) -> Self {
    let mut f = f;
    self.on_click_outside = Some(Box::new(move || Some(f())));
    self
  }

  pub fn on_click_outside_maybe(mut self, f: impl FnMut() -> Option<Message> + 'a) -> Self {
    self.on_click_outside = Some(Box::new(f));
    self
  }

  /// Sets a unique key for the widget state.
  pub fn key(mut self, key: impl Into<String>) -> Self {
    self.key = Some(key.into());
    self
  }

  /// Sets the message to emit when the left mouse button is pressed.
  pub fn on_press(mut self, message: Message) -> Self {
    let mut message = Some(message);
    self.on_press = Some(Box::new(move || message.take()));
    self
  }

  pub fn on_press_with(mut self, f: impl FnMut() -> Message + 'a) -> Self {
    let mut f = f;
    self.on_press = Some(Box::new(move || Some(f())));
    self
  }

  pub fn on_press_maybe(mut self, f: impl FnMut() -> Option<Message> + 'a) -> Self {
    self.on_press = Some(Box::new(f));
    self
  }

  /// Sets the message to emit when the left mouse button is released.
  pub fn on_release(mut self, message: Message) -> Self {
    let mut message = Some(message);
    self.on_release = Some(Box::new(move || message.take()));
    self
  }

  pub fn on_release_with(mut self, f: impl FnMut() -> Message + 'a) -> Self {
    let mut f = f;
    self.on_release = Some(Box::new(move || Some(f())));
    self
  }

  pub fn on_release_maybe(mut self, f: impl FnMut() -> Option<Message> + 'a) -> Self {
    self.on_release = Some(Box::new(f));
    self
  }

  /// Sets the message to emit on double click.
  pub fn on_double_click(mut self, message: Message) -> Self {
    let mut message = Some(message);
    self.on_double_click = Some(Box::new(move || message.take()));
    self
  }

  pub fn on_double_click_with(mut self, f: impl FnMut() -> Message + 'a) -> Self {
    let mut f = f;
    self.on_double_click = Some(Box::new(move || Some(f())));
    self
  }

  pub fn on_double_click_maybe(mut self, f: impl FnMut() -> Option<Message> + 'a) -> Self {
    self.on_double_click = Some(Box::new(f));
    self
  }

  /// Sets the message to emit when the right mouse button is pressed.
  pub fn on_right_press(mut self, message: Message) -> Self {
    let mut message = Some(message);
    self.on_right_press = Some(Box::new(move || message.take()));
    self
  }

  pub fn on_right_press_with(mut self, f: impl FnMut() -> Message + 'a) -> Self {
    let mut f = f;
    self.on_right_press = Some(Box::new(move || Some(f())));
    self
  }

  pub fn on_right_press_maybe(mut self, f: impl FnMut() -> Option<Message> + 'a) -> Self {
    self.on_right_press = Some(Box::new(f));
    self
  }

  /// Sets the message to emit when the right mouse button is released.
  pub fn on_right_release(mut self, message: Message) -> Self {
    let mut message = Some(message);
    self.on_right_release = Some(Box::new(move || message.take()));
    self
  }

  pub fn on_right_release_with(mut self, f: impl FnMut() -> Message + 'a) -> Self {
    let mut f = f;
    self.on_right_release = Some(Box::new(move || Some(f())));
    self
  }

  pub fn on_right_release_maybe(mut self, f: impl FnMut() -> Option<Message> + 'a) -> Self {
    self.on_right_release = Some(Box::new(f));
    self
  }

  /// Sets the message to emit when the middle mouse button is pressed.
  pub fn on_middle_press(mut self, message: Message) -> Self {
    let mut message = Some(message);
    self.on_middle_press = Some(Box::new(move || message.take()));
    self
  }

  pub fn on_middle_press_with(mut self, f: impl FnMut() -> Message + 'a) -> Self {
    let mut f = f;
    self.on_middle_press = Some(Box::new(move || Some(f())));
    self
  }

  pub fn on_middle_press_maybe(mut self, f: impl FnMut() -> Option<Message> + 'a) -> Self {
    self.on_middle_press = Some(Box::new(f));
    self
  }

  /// Sets the message to emit when the middle mouse button is released.
  pub fn on_middle_release(mut self, message: Message) -> Self {
    let mut message = Some(message);
    self.on_middle_release = Some(Box::new(move || message.take()));
    self
  }

  pub fn on_middle_release_with(mut self, f: impl FnMut() -> Message + 'a) -> Self {
    let mut f = f;
    self.on_middle_release = Some(Box::new(move || Some(f())));
    self
  }

  pub fn on_middle_release_maybe(mut self, f: impl FnMut() -> Option<Message> + 'a) -> Self {
    self.on_middle_release = Some(Box::new(f));
    self
  }

  /// Sets the callback to emit a message on mouse scroll.
  pub fn on_scroll(mut self, f: impl FnMut(mouse::ScrollDelta) -> Message + 'a) -> Self {
    let mut f = f;
    self.on_scroll = Some(Box::new(move |d| Some(f(d))));
    self
  }

  pub fn on_scroll_maybe(mut self, f: impl FnMut(mouse::ScrollDelta) -> Option<Message> + 'a) -> Self {
    self.on_scroll = Some(Box::new(f));
    self
  }

  /// Sets the message to emit when the mouse enters the widget area.
  pub fn on_enter(mut self, message: Message) -> Self {
    let mut message = Some(message);
    self.on_enter = Some(Box::new(move || message.take()));
    self
  }

  pub fn on_enter_with(mut self, f: impl FnMut() -> Message + 'a) -> Self {
    let mut f = f;
    self.on_enter = Some(Box::new(move || Some(f())));
    self
  }

  pub fn on_enter_maybe(mut self, f: impl FnMut() -> Option<Message> + 'a) -> Self {
    self.on_enter = Some(Box::new(f));
    self
  }

  /// Sets the callback to emit a message when the mouse moves within the widget area.
  pub fn on_move(mut self, f: impl FnMut(Point) -> Message + 'a) -> Self {
    let mut f = f;
    self.on_move = Some(Box::new(move |p| Some(f(p))));
    self
  }

  pub fn on_move_maybe(mut self, f: impl FnMut(Point) -> Option<Message> + 'a) -> Self {
    self.on_move = Some(Box::new(f));
    self
  }

  /// Sets the message to emit when the mouse exits the widget area.
  pub fn on_exit(mut self, message: Message) -> Self {
    let mut message = Some(message);
    self.on_exit = Some(Box::new(move || message.take()));
    self
  }

  pub fn on_exit_with(mut self, f: impl FnMut() -> Message + 'a) -> Self {
    let mut f = f;
    self.on_exit = Some(Box::new(move || Some(f())));
    self
  }

  pub fn on_exit_maybe(mut self, f: impl FnMut() -> Option<Message> + 'a) -> Self {
    self.on_exit = Some(Box::new(f));
    self
  }

  /// Sets the mouse cursor interaction when hovering the widget.
  pub fn interaction(mut self, interaction: mouse::Interaction) -> Self {
    self.interaction = Some(interaction);
    self
  }
}

/// Internal state of the [`WidgetSensor`].
#[derive(Debug, Clone, Default)]
pub struct State {
  /// Whether the widget is currently visible in the viewport.
  is_visible: bool,
  /// Whether the widget has appeared at least once.
  has_appeared: bool,
  /// The last key used to identify the widget state.
  last_key: Option<String>,
  /// Whether the mouse is currently hovering over the widget.
  is_hovered: bool,
  /// The bounds of the widget in the previous frame.
  bounds: Rectangle,
  /// The cursor position in the previous frame.
  cursor_position: Option<Point>,
  /// The last click event for double-click detection.
  previous_click: Option<iced_core::mouse::Click>,
  /// Whether the left mouse button (or touch) was pressed over the widget.
  is_pressed: bool,
}

impl<'a, Message, Theme, Renderer> Widget<Message, Theme, Renderer> for WidgetSensor<'a, Message, Theme, Renderer>
where
  Renderer: lintel_lib_iced::advanced::Renderer,
{
  fn size(&self) -> Size<Length> {
    self.content.as_widget().size()
  }

  fn layout(&mut self, tree: &mut widget::Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
    self
      .content
      .as_widget_mut()
      .layout(&mut tree.children[0], renderer, limits)
  }

  fn draw(
    &self,
    tree: &widget::Tree,
    renderer: &mut Renderer,
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
  }

  fn children(&self) -> Vec<widget::Tree> {
    vec![widget::Tree::new(&self.content)]
  }

  fn diff(&self, tree: &mut widget::Tree) {
    tree.diff_children(&[&self.content]);
  }

  fn state(&self) -> widget::tree::State {
    widget::tree::State::new(State::default())
  }

  fn tag(&self) -> widget::tree::Tag {
    widget::tree::Tag::of::<State>()
  }

  fn update(
    &mut self,
    tree: &mut widget::Tree,
    event: &Event,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    renderer: &Renderer,
    clipboard: &mut dyn Clipboard,
    shell: &mut Shell<'_, Message>,
    viewport: &Rectangle,
  ) {
    let state = tree.state.downcast_mut::<State>();
    let bounds = layout.bounds();

    // Reset state if key changes
    if self.key != state.last_key {
      state.is_visible = false;
      state.has_appeared = false;
      state.last_key = self.key.clone();
      state.is_hovered = false;
      state.bounds = Rectangle::default();
      state.cursor_position = None;
      state.previous_click = None;
      state.is_pressed = false;
    }

    // Handle lifecycle events
    let is_visible = viewport.intersects(&bounds);

    if is_visible != state.is_visible {
      if is_visible {
        if let Some(on_appear) = &mut self.on_appear {
          if let Some(msg) = on_appear() {
            shell.publish(msg);
          }
        }
      } else if let Some(on_disappear) = &mut self.on_disappear {
        if let Some(msg) = on_disappear() {
          shell.publish(msg);
        }
      }
      state.is_visible = is_visible;
    }

    if is_visible && !state.has_appeared {
      if let Some(on_first_appear) = &mut self.on_first_appear {
        if let Some(msg) = on_first_appear() {
          shell.publish(msg);
        }
      }
      state.has_appeared = true;
    }

    // Handle mouse events
    let cursor_position = cursor.position();
    let was_hovered = state.is_hovered;
    let current_hovered = cursor.is_over(layout.bounds());

    if state.cursor_position != cursor_position || state.bounds != bounds || was_hovered != current_hovered {
      state.cursor_position = cursor_position;
      state.bounds = bounds;
      state.is_hovered = current_hovered;

      match (self.on_enter.as_mut(), self.on_move.as_mut(), self.on_exit.as_mut()) {
        (Some(on_enter), _, _) if state.is_hovered && !was_hovered => {
          if let Some(msg) = on_enter() {
            shell.publish(msg);
          }
        }
        (_, Some(on_move), _) if state.is_hovered => {
          if let Some(position) = cursor.position_in(layout.bounds()) {
            if let Some(msg) = on_move(position) {
              shell.publish(msg);
            }
          }
        }
        (_, _, Some(on_exit)) if !state.is_hovered && was_hovered => {
          if let Some(msg) = on_exit() {
            shell.publish(msg);
          }
        }
        _ => {}
      }
    }

    // MouseArea specific events
    match event {
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
      | Event::Touch(iced_core::touch::Event::FingerPressed { .. }) => {
        if cursor.is_over(bounds) {
          state.is_pressed = true;

          if let Some(on_press) = &mut self.on_press {
            if let Some(msg) = on_press() {
              shell.publish(msg);
              shell.capture_event();
            }
          }

          if let Some(position) = cursor_position {
            if let Some(on_double_click) = &mut self.on_double_click {
              let new_click = iced_core::mouse::Click::new(position, mouse::Button::Left, state.previous_click);
              if new_click.kind() == iced_core::mouse::click::Kind::Double {
                if let Some(msg) = on_double_click() {
                  shell.publish(msg);
                }
              }
              state.previous_click = Some(new_click);
              shell.capture_event();
            }
          }
        } else {
          state.is_pressed = false;
          if let Some(on_click_outside) = &mut self.on_click_outside {
            if let Some(msg) = on_click_outside() {
              shell.publish(msg);
            }
          }
        }
      }
      Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
      | Event::Touch(iced_core::touch::Event::FingerLifted { .. }) => {
        if cursor.is_over(bounds) {
          if let Some(on_release) = &mut self.on_release {
            if let Some(msg) = on_release() {
              shell.publish(msg);
            }
          }
          if state.is_pressed {
            if let Some(on_click) = &mut self.on_click {
              if let Some(msg) = on_click() {
                shell.publish(msg);
              }
            }
          }
        }
        state.is_pressed = false;
      }
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)) => {
        if cursor.is_over(bounds) {
          if let Some(on_right_press) = &mut self.on_right_press {
            if let Some(msg) = on_right_press() {
              shell.publish(msg);
              shell.capture_event();
            }
          }
        }
      }
      Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)) => {
        if cursor.is_over(bounds) {
          if let Some(on_right_release) = &mut self.on_right_release {
            if let Some(msg) = on_right_release() {
              shell.publish(msg);
            }
          }
        }
      }
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Middle)) => {
        if cursor.is_over(bounds) {
          if let Some(on_middle_press) = &mut self.on_middle_press {
            if let Some(msg) = on_middle_press() {
              shell.publish(msg);
              shell.capture_event();
            }
          }
        }
      }
      Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Middle)) => {
        if cursor.is_over(bounds) {
          if let Some(on_middle_release) = &mut self.on_middle_release {
            if let Some(msg) = on_middle_release() {
              shell.publish(msg);
            }
          }
        }
      }
      Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
        if cursor.is_over(bounds) {
          if let Some(on_scroll) = &mut self.on_scroll {
            if let Some(msg) = on_scroll(*delta) {
              shell.publish(msg);
              shell.capture_event();
            }
          }
        }
      }
      _ => {}
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
  }

  fn mouse_interaction(
    &self,
    tree: &widget::Tree,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    viewport: &Rectangle,
    renderer: &Renderer,
  ) -> mouse::Interaction {
    let content_interaction =
      self
        .content
        .as_widget()
        .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer);

    match (self.interaction, content_interaction) {
      (Some(interaction), mouse::Interaction::None) if cursor.is_over(layout.bounds()) => interaction,
      _ => content_interaction,
    }
  }

  fn overlay<'b>(
    &'b mut self,
    tree: &'b mut widget::Tree,
    layout: Layout<'b>,
    renderer: &Renderer,
    viewport: &Rectangle,
    translation: lintel_lib_iced::Vector,
  ) -> Option<lintel_lib_iced::advanced::overlay::Element<'b, Message, Theme, Renderer>> {
    self
      .content
      .as_widget_mut()
      .overlay(&mut tree.children[0], layout, renderer, viewport, translation)
  }
}

impl<'a, Message, Theme, Renderer> From<WidgetSensor<'a, Message, Theme, Renderer>>
  for Element<'a, Message, Theme, Renderer>
where
  Message: 'a,
  Theme: 'a,
  Renderer: 'a + lintel_lib_iced::advanced::Renderer,
{
  fn from(sensor: WidgetSensor<'a, Message, Theme, Renderer>) -> Self {
    Element::new(sensor)
  }
}
