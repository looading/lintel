//! A transparent wrapper that can capture keyboard events before they reach its child.

use lintel_lib_iced::iced_core as core;

use core::keyboard;
use core::layout;
use core::mouse;
use core::overlay;
use core::renderer;
use core::widget::{Operation, Tree};
use core::{Clipboard, Element, Event, Layout, Length, Rectangle, Shell, Size, Vector, Widget};

type KeyboardHandler<'a, Message> = Box<dyn FnMut(&keyboard::Event) -> Option<Message> + 'a>;

/// A transparent widget boundary for keyboard events.
pub struct KeyboardBoundary<'a, Message, Theme = lintel_lib_iced::Theme, Renderer = lintel_lib_iced::Renderer> {
  content: Element<'a, Message, Theme, Renderer>,
  active: bool,
  on_key: Option<KeyboardHandler<'a, Message>>,
}

impl<'a, Message, Theme, Renderer> KeyboardBoundary<'a, Message, Theme, Renderer> {
  /// Creates a keyboard boundary around the given content.
  pub fn new(content: impl Into<Element<'a, Message, Theme, Renderer>>) -> Self {
    Self {
      content: content.into(),
      active: true,
      on_key: None,
    }
  }

  /// Enables or disables keyboard capture for this boundary.
  #[must_use]
  pub fn active(mut self, active: bool) -> Self {
    self.active = active;
    self
  }

  /// Maps keyboard events to optional messages.
  #[must_use]
  pub fn on_key_maybe(mut self, f: impl FnMut(&keyboard::Event) -> Option<Message> + 'a) -> Self {
    self.on_key = Some(Box::new(f));
    self
  }
}

fn keyboard_boundary_message_for_event<Message>(
  active: bool,
  event: &Event,
  handler: &mut dyn FnMut(&keyboard::Event) -> Option<Message>,
) -> Option<Message> {
  if !active {
    return None;
  }

  match event {
    Event::Keyboard(event) => handler(event),
    _ => None,
  }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for KeyboardBoundary<'_, Message, Theme, Renderer>
where
  Renderer: renderer::Renderer,
{
  fn children(&self) -> Vec<Tree> {
    vec![Tree::new(&self.content)]
  }

  fn diff(&self, tree: &mut Tree) {
    tree.diff_children(std::slice::from_ref(&self.content));
  }

  fn size(&self) -> Size<Length> {
    self.content.as_widget().size()
  }

  fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
    self
      .content
      .as_widget_mut()
      .layout(&mut tree.children[0], renderer, limits)
  }

  fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
    self
      .content
      .as_widget_mut()
      .operate(&mut tree.children[0], layout, renderer, operation);
  }

  fn update(
    &mut self,
    tree: &mut Tree,
    event: &Event,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    renderer: &Renderer,
    clipboard: &mut dyn Clipboard,
    shell: &mut Shell<'_, Message>,
    viewport: &Rectangle,
  ) {
    if let Some(on_key) = self.on_key.as_mut()
      && let Some(message) = keyboard_boundary_message_for_event(self.active, event, on_key)
    {
      shell.publish(message);
      shell.capture_event();
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
  }

  fn mouse_interaction(
    &self,
    tree: &Tree,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    viewport: &Rectangle,
    renderer: &Renderer,
  ) -> mouse::Interaction {
    self
      .content
      .as_widget()
      .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
  }

  fn draw(
    &self,
    tree: &Tree,
    renderer: &mut Renderer,
    theme: &Theme,
    renderer_style: &renderer::Style,
    layout: Layout<'_>,
    cursor: mouse::Cursor,
    viewport: &Rectangle,
  ) {
    self.content.as_widget().draw(
      &tree.children[0],
      renderer,
      theme,
      renderer_style,
      layout,
      cursor,
      viewport,
    );
  }

  fn overlay<'b>(
    &'b mut self,
    tree: &'b mut Tree,
    layout: Layout<'b>,
    renderer: &Renderer,
    viewport: &Rectangle,
    translation: Vector,
  ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
    self
      .content
      .as_widget_mut()
      .overlay(&mut tree.children[0], layout, renderer, viewport, translation)
  }
}

impl<'a, Message, Theme, Renderer> From<KeyboardBoundary<'a, Message, Theme, Renderer>>
  for Element<'a, Message, Theme, Renderer>
where
  Message: 'a,
  Theme: 'a,
  Renderer: 'a + renderer::Renderer,
{
  fn from(boundary: KeyboardBoundary<'a, Message, Theme, Renderer>) -> Element<'a, Message, Theme, Renderer> {
    Element::new(boundary)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use lintel_lib_iced::{
    Event, iced_core,
    keyboard::{self, Key, Modifiers, key},
  };

  #[derive(Debug, Clone, PartialEq, Eq)]
  enum TestMessage {
    ArrowDown,
  }

  fn key_pressed(named: key::Named) -> Event {
    Event::Keyboard(keyboard::Event::KeyPressed {
      key: Key::Named(named),
      modified_key: Key::Named(named),
      physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
      location: iced_core::keyboard::Location::Standard,
      modifiers: Modifiers::NONE,
      text: None,
      repeat: false,
    })
  }

  #[test]
  fn active_keyboard_boundary_maps_keyboard_event() {
    let mut handler = |event: &keyboard::Event| match event {
      keyboard::Event::KeyPressed {
        key: Key::Named(key::Named::ArrowDown),
        ..
      } => Some(TestMessage::ArrowDown),
      _ => None,
    };

    assert_eq!(
      keyboard_boundary_message_for_event(true, &key_pressed(key::Named::ArrowDown), &mut handler),
      Some(TestMessage::ArrowDown)
    );
  }

  #[test]
  fn inactive_keyboard_boundary_ignores_keyboard_event() {
    let mut handler = |_: &keyboard::Event| Some(TestMessage::ArrowDown);

    assert_eq!(
      keyboard_boundary_message_for_event(false, &key_pressed(key::Named::ArrowDown), &mut handler),
      None
    );
  }
}
