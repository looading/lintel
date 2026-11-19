use lintel_lib_iced::iced_core as core;

use core::layout;
use core::mouse;
use core::overlay;
use core::renderer;
use core::widget::{Operation, Tree, tree};
use core::{Clipboard, Element, Event, Layout, Length, Point, Rectangle, Shell, Size, Vector, Widget};
use lintel_lib_iced as iced;

const DRAG_START_DISTANCE: f32 = 4.0;

/// 标题栏拖拽区域包装组件。
pub struct DragArea<'a, Message, Theme = iced::Theme, Renderer = iced::Renderer> {
  content: Element<'a, Message, Theme, Renderer>,
  on_drag: Option<Message>,
  on_double_click: Option<Message>,
}

#[derive(Debug, Clone, Copy, Default)]
struct PressState {
  previous_click: Option<mouse::Click>,
  pending_drag_start: Option<Point>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressIntent {
  Drag,
  ToggleMaximize,
}

impl<'a, Message, Theme, Renderer> DragArea<'a, Message, Theme, Renderer> {
  /// 使用给定内容创建 [`DragArea`]。
  pub fn new(content: impl Into<Element<'a, Message, Theme, Renderer>>) -> Self {
    Self {
      content: content.into(),
      on_drag: None,
      on_double_click: None,
    }
  }

  /// 确认拖拽意图后发出的原生窗口拖拽消息。
  #[must_use]
  pub fn on_drag(mut self, message: Message) -> Self {
    self.on_drag = Some(message);
    self
  }

  /// 双击时发出的窗口最大化/还原切换消息。
  #[must_use]
  pub fn on_double_click(mut self, message: Message) -> Self {
    self.on_double_click = Some(message);
    self
  }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for DragArea<'_, Message, Theme, Renderer>
where
  Message: Clone,
  Renderer: renderer::Renderer,
{
  fn size(&self) -> Size<Length> {
    self.content.as_widget().size()
  }

  fn tag(&self) -> tree::Tag {
    tree::Tag::of::<PressState>()
  }

  fn state(&self) -> tree::State {
    tree::State::new(PressState::default())
  }

  fn children(&self) -> Vec<Tree> {
    vec![Tree::new(&self.content)]
  }

  fn diff(&self, tree: &mut Tree) {
    tree.diff_children(std::slice::from_ref(&self.content));
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

    if shell.is_event_captured() {
      return;
    }

    match event {
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
        let state = tree.state.downcast_mut::<PressState>();

        if !cursor.is_over(layout.bounds()) {
          state.pending_drag_start = None;
          return;
        }
        let Some(position) = cursor.position() else {
          state.pending_drag_start = None;
          return;
        };

        let (intent, next_state) = classify_left_press(position, *state);
        *state = next_state;

        match intent {
          PressIntent::ToggleMaximize => {
            state.pending_drag_start = None;
            if let Some(message) = self.on_double_click.as_ref().or(self.on_drag.as_ref()) {
              shell.publish(message.clone());
              shell.capture_event();
            }
          }
          PressIntent::Drag => {
            state.pending_drag_start = self.on_drag.as_ref().map(|_| position);
            if state.pending_drag_start.is_some() {
              shell.capture_event();
            }
          }
        }
      }
      Event::Mouse(mouse::Event::CursorMoved { position }) => {
        let state = tree.state.downcast_mut::<PressState>();

        if let Some(start) = state.pending_drag_start
          && drag_start_distance_exceeded(start, *position)
        {
          state.pending_drag_start = None;
          state.previous_click = None;
          if let Some(message) = self.on_drag.as_ref() {
            shell.publish(message.clone());
            shell.capture_event();
          }
        }
      }
      Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
        let state = tree.state.downcast_mut::<PressState>();
        state.pending_drag_start = None;
      }
      _ => {}
    }
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

impl<'a, Message, Theme, Renderer> From<DragArea<'a, Message, Theme, Renderer>>
  for Element<'a, Message, Theme, Renderer>
where
  Message: 'a + Clone,
  Theme: 'a,
  Renderer: 'a + renderer::Renderer,
{
  fn from(area: DragArea<'a, Message, Theme, Renderer>) -> Self {
    Element::new(area)
  }
}

fn classify_left_press(position: Point, mut state: PressState) -> (PressIntent, PressState) {
  let click = mouse::Click::new(position, mouse::Button::Left, state.previous_click);
  let intent = if click.kind() == mouse::click::Kind::Double {
    PressIntent::ToggleMaximize
  } else {
    PressIntent::Drag
  };
  state.previous_click = Some(click);
  (intent, state)
}

fn drag_start_distance_exceeded(start: Point, current: Point) -> bool {
  let delta = current - start;
  delta.x * delta.x + delta.y * delta.y > DRAG_START_DISTANCE * DRAG_START_DISTANCE
}

/// 创建新的 [`DragArea`]。
pub fn drag_area<'a, Message, Theme, Renderer>(
  widget: impl Into<Element<'a, Message, Theme, Renderer>>,
) -> DragArea<'a, Message, Theme, Renderer>
where
  Renderer: core::Renderer,
{
  DragArea::new(widget)
}

#[cfg(test)]
mod tests {
  use super::*;
  use lintel_lib_iced::iced_core::Point;
  use std::cell::Cell;
  use std::rc::Rc;

  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  enum TestMessage {
    Drag,
    DoubleClick,
  }

  #[derive(Default)]
  struct TestRenderer {
    scale_factor: Option<f32>,
  }

  impl renderer::Renderer for TestRenderer {
    fn start_layer(&mut self, _bounds: Rectangle) {}

    fn end_layer(&mut self) {}

    fn start_transformation(&mut self, _transformation: core::Transformation) {}

    fn end_transformation(&mut self) {}

    fn fill_quad(&mut self, _quad: renderer::Quad, _background: impl Into<core::Background>) {}

    fn allocate_image(
      &mut self,
      _handle: &core::image::Handle,
      callback: impl FnOnce(Result<core::image::Allocation, core::image::Error>) + Send + 'static,
    ) {
      callback(Err(core::image::Error::Unsupported));
    }

    fn hint(&mut self, scale_factor: f32) {
      self.scale_factor = Some(scale_factor);
    }

    fn scale_factor(&self) -> Option<f32> {
      self.scale_factor
    }

    fn reset(&mut self, _new_bounds: Rectangle) {}
  }

  struct CountingContent {
    updates: Rc<Cell<usize>>,
  }

  impl<Message> Widget<Message, (), TestRenderer> for CountingContent {
    fn size(&self) -> Size<Length> {
      Size::new(Length::Fill, Length::Fill)
    }

    fn layout(&mut self, _tree: &mut Tree, _renderer: &TestRenderer, _limits: &layout::Limits) -> layout::Node {
      layout::Node::new(Size::new(100.0, 80.0))
    }

    fn update(
      &mut self,
      _tree: &mut Tree,
      _event: &Event,
      _layout: Layout<'_>,
      _cursor: mouse::Cursor,
      _renderer: &TestRenderer,
      _clipboard: &mut dyn Clipboard,
      _shell: &mut Shell<'_, Message>,
      _viewport: &Rectangle,
    ) {
      self.updates.set(self.updates.get() + 1);
    }

    fn draw(
      &self,
      _tree: &Tree,
      _renderer: &mut TestRenderer,
      _theme: &(),
      _renderer_style: &renderer::Style,
      _layout: Layout<'_>,
      _cursor: mouse::Cursor,
      _viewport: &Rectangle,
    ) {
    }
  }

  fn counting_drag_area(updates: Rc<Cell<usize>>) -> DragArea<'static, TestMessage, (), TestRenderer> {
    DragArea::new(Element::new(CountingContent { updates }))
      .on_drag(TestMessage::Drag)
      .on_double_click(TestMessage::DoubleClick)
  }

  fn drag_area_tree(area: &DragArea<'_, TestMessage, (), TestRenderer>) -> Tree {
    Tree {
      tag: tree::Tag::of::<PressState>(),
      state: tree::State::new(PressState::default()),
      children: vec![Tree::new(&area.content)],
    }
  }

  fn update_drag_area(
    area: &mut DragArea<'_, TestMessage, (), TestRenderer>,
    tree: &mut Tree,
    event: Event,
    cursor: mouse::Cursor,
    messages: &mut Vec<TestMessage>,
  ) -> bool {
    let node = layout::Node::new(Size::new(100.0, 80.0));
    let layout = Layout::new(&node);
    let viewport = Rectangle {
      x: 0.0,
      y: 0.0,
      width: 100.0,
      height: 80.0,
    };
    let renderer = TestRenderer::default();
    let mut clipboard = core::clipboard::Null;
    let mut shell = Shell::new(core::Waker::noop(), messages);

    area.update(
      tree,
      &event,
      layout,
      cursor,
      &renderer,
      &mut clipboard,
      &mut shell,
      &viewport,
    );

    shell.is_event_captured()
  }

  #[test]
  fn first_press_records_drag_intent() {
    let (intent, state) = classify_left_press(Point::new(10.0, 8.0), PressState::default());

    assert_eq!(intent, PressIntent::Drag);
    assert!(state.previous_click.is_some());
  }

  #[test]
  fn double_press_requests_maximize_toggle_without_dragging() {
    let position = Point::new(10.0, 8.0);
    let (_, state) = classify_left_press(position, PressState::default());

    let (intent, _) = classify_left_press(position, state);

    assert_eq!(intent, PressIntent::ToggleMaximize);
  }

  #[test]
  fn first_press_waits_for_drag_intent_before_publishing_drag() {
    let updates = Rc::new(Cell::new(0));
    let mut area = counting_drag_area(updates.clone());
    let mut tree = drag_area_tree(&area);
    let mut messages = Vec::new();

    update_drag_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(50.0, 40.0)),
      &mut messages,
    );

    assert!(messages.is_empty());
    assert_eq!(updates.get(), 1);
  }

  #[test]
  fn double_press_publishes_double_click_without_prior_drag() {
    let updates = Rc::new(Cell::new(0));
    let mut area = counting_drag_area(updates);
    let mut tree = drag_area_tree(&area);
    let mut messages = Vec::new();
    let position = Point::new(50.0, 40.0);

    update_drag_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(position),
      &mut messages,
    );
    update_drag_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
      mouse::Cursor::Available(position),
      &mut messages,
    );
    update_drag_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(position),
      &mut messages,
    );

    assert_eq!(messages, [TestMessage::DoubleClick]);
  }

  #[test]
  fn cursor_move_beyond_threshold_publishes_drag_once() {
    let updates = Rc::new(Cell::new(0));
    let mut area = counting_drag_area(updates);
    let mut tree = drag_area_tree(&area);
    let mut messages = Vec::new();

    update_drag_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(50.0, 40.0)),
      &mut messages,
    );
    let small_move_captured = update_drag_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::CursorMoved {
        position: Point::new(52.0, 40.0),
      }),
      mouse::Cursor::Available(Point::new(52.0, 40.0)),
      &mut messages,
    );
    let drag_move_captured = update_drag_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::CursorMoved {
        position: Point::new(56.0, 40.0),
      }),
      mouse::Cursor::Available(Point::new(56.0, 40.0)),
      &mut messages,
    );
    update_drag_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::CursorMoved {
        position: Point::new(70.0, 40.0),
      }),
      mouse::Cursor::Available(Point::new(70.0, 40.0)),
      &mut messages,
    );

    assert!(!small_move_captured);
    assert!(drag_move_captured);
    assert_eq!(messages, [TestMessage::Drag]);
  }

  #[test]
  fn release_before_threshold_clears_pending_drag() {
    let updates = Rc::new(Cell::new(0));
    let mut area = counting_drag_area(updates);
    let mut tree = drag_area_tree(&area);
    let mut messages = Vec::new();

    update_drag_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(50.0, 40.0)),
      &mut messages,
    );
    update_drag_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(50.0, 40.0)),
      &mut messages,
    );
    update_drag_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::CursorMoved {
        position: Point::new(70.0, 40.0),
      }),
      mouse::Cursor::Available(Point::new(70.0, 40.0)),
      &mut messages,
    );

    assert!(messages.is_empty());
  }

  #[test]
  fn started_drag_does_not_count_as_first_click_for_double_click() {
    let updates = Rc::new(Cell::new(0));
    let mut area = counting_drag_area(updates);
    let mut tree = drag_area_tree(&area);
    let mut messages = Vec::new();

    update_drag_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(50.0, 40.0)),
      &mut messages,
    );
    update_drag_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::CursorMoved {
        position: Point::new(56.0, 40.0),
      }),
      mouse::Cursor::Available(Point::new(56.0, 40.0)),
      &mut messages,
    );
    update_drag_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(56.0, 40.0)),
      &mut messages,
    );
    messages.clear();

    update_drag_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(50.0, 40.0)),
      &mut messages,
    );

    assert!(messages.is_empty());
  }
}
