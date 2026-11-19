use core::layout;
use core::mouse;
use core::overlay;
use core::renderer;
use core::widget::{Operation, Tree, tree};
use core::{Clipboard, Element, Event, Layout, Length, Rectangle, Shell, Size, Vector, Widget};
use iced::window::Direction;
use lintel_lib_iced as iced;
use lintel_lib_iced::iced_core as core;

pub struct ResizeArea<'a, Message, Theme = iced::Theme, Renderer = iced::Renderer> {
  content: Element<'a, Message, Theme, Renderer>,
  edge_margin: f32,
  enabled: bool,
  window_state: ResizeAreaWindowState,
  on_pressed_move: Option<Box<dyn Fn(Direction) -> Message + 'a>>,
  on_resize: Option<Box<dyn Fn(ResizeAreaEvent) -> Message + 'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizePhase {
  Started,
  Dragged,
  Ended,
}

#[derive(Debug, Clone, Copy)]
pub struct ResizeAreaEvent {
  pub phase: ResizePhase,
  pub direction: Direction,
  pub start: core::Point,
  pub current: core::Point,
  pub delta: Vector,
  pub bounds: Rectangle,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResizeAreaWindowState {
  pub is_maximized: bool,
  pub is_fullscreen: bool,
}

impl ResizeAreaWindowState {
  pub fn new(is_maximized: bool, mode: iced::window::Mode) -> Self {
    Self {
      is_maximized,
      is_fullscreen: mode == iced::window::Mode::Fullscreen,
    }
  }

  pub fn is_resizable(self) -> bool {
    self.allows_resize()
  }

  fn allows_resize(self) -> bool {
    !self.is_maximized && !self.is_fullscreen
  }
}

/// 无边框窗口拖拽缩放所依赖的窗口状态跟踪器。
///
/// 每次查询都会生成新的 revision，只有最后一次查询的结果可以更新状态，
/// 避免最大化、还原和显示等窗口切换期间的旧异步响应覆盖新状态。
#[derive(Debug, Clone, Copy, Default)]
pub struct ResizeAreaWindowStateTracker {
  state: ResizeAreaWindowState,
  revision: u64,
  transition_pending: bool,
}

impl ResizeAreaWindowStateTracker {
  pub fn state(self) -> ResizeAreaWindowState {
    if self.transition_pending {
      ResizeAreaWindowState {
        is_maximized: true,
        is_fullscreen: false,
      }
    } else {
      self.state
    }
  }

  /// 在发出原生窗口状态切换前，使已有查询立即失效并暂时禁用 resize 命中。
  pub fn begin_transition(&mut self) {
    self.advance_revision();
    self.transition_pending = true;
  }

  /// 开始一次权威状态查询；观察性刷新不改变当前交互状态。
  pub fn begin_query(&mut self) -> u64 {
    self.advance_revision();
    self.revision
  }

  pub fn apply_query_result(&mut self, revision: u64, state: ResizeAreaWindowState) -> bool {
    if revision != self.revision {
      return false;
    }

    self.state = state;
    self.transition_pending = false;
    true
  }

  fn advance_revision(&mut self) {
    self.revision = self
      .revision
      .checked_add(1)
      .expect("resize area window state query revision exhausted");
  }
}

pub fn query_window_state(window: iced::window::Id) -> iced::Task<ResizeAreaWindowState> {
  iced::window::is_maximized(window).then(move |is_maximized| {
    iced::window::mode(window).map(move |mode| ResizeAreaWindowState::new(is_maximized, mode))
  })
}

pub fn drag_resize<Message>(
  window: iced::window::Id,
  direction: Direction,
  window_state: ResizeAreaWindowState,
) -> iced::Task<Message>
where
  Message: Send + 'static,
{
  if window_state.allows_resize() {
    iced::window::drag_resize::<Message>(window, direction).discard()
  } else {
    iced::Task::none()
  }
}

#[derive(Debug, Clone, Copy, Default)]
struct State {
  is_pressed: bool,
  direction: Option<Direction>,
  start: Option<core::Point>,
  current: Option<core::Point>,
}

impl<'a, Message, Theme, Renderer> ResizeArea<'a, Message, Theme, Renderer> {
  pub fn new(content: impl Into<Element<'a, Message, Theme, Renderer>>) -> Self {
    Self {
      content: content.into(),
      edge_margin: DEFAULT_EDGE_MARGIN,
      enabled: true,
      window_state: ResizeAreaWindowState::default(),
      on_pressed_move: None,
      on_resize: None,
    }
  }

  #[must_use]
  pub fn edge_margin(mut self, margin: f32) -> Self {
    self.edge_margin = margin;
    self
  }

  #[must_use]
  pub fn enabled(mut self, enabled: bool) -> Self {
    self.enabled = enabled;
    self
  }

  #[must_use]
  pub fn window_state(mut self, window_state: ResizeAreaWindowState) -> Self {
    self.window_state = window_state;
    self
  }

  #[must_use]
  pub fn on_pressed_move(mut self, on_pressed_move: impl Fn(Direction) -> Message + 'a) -> Self {
    self.on_pressed_move = Some(Box::new(on_pressed_move));
    self
  }

  #[must_use]
  pub fn on_resize(mut self, on_resize: impl Fn(ResizeAreaEvent) -> Message + 'a) -> Self {
    self.on_resize = Some(Box::new(on_resize));
    self
  }

  fn allows_resize(&self) -> bool {
    self.enabled && self.window_state.allows_resize()
  }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for ResizeArea<'_, Message, Theme, Renderer>
where
  Renderer: renderer::Renderer,
  Message: Clone,
{
  fn size(&self) -> Size<Length> {
    self.content.as_widget().size()
  }

  fn tag(&self) -> tree::Tag {
    tree::Tag::of::<State>()
  }

  fn state(&self) -> tree::State {
    tree::State::new(State::default())
  }

  fn children(&self) -> Vec<Tree> {
    vec![Tree::new(&self.content)]
  }

  fn diff(&self, tree: &mut Tree) {
    tree.diff_children(&[&self.content]);
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
    let state = tree.state.downcast_mut::<State>();
    let bounds = layout.bounds();

    if !self.allows_resize() {
      *state = State::default();
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
      return;
    }

    match event {
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
        *state = State::default();

        if (self.on_pressed_move.is_some() || self.on_resize.is_some())
          && let Some(direction) = pressed_resize_direction(
            cursor.position_in(bounds),
            Size::new(bounds.width, bounds.height),
            self.edge_margin,
            self.enabled,
            self.window_state,
          )
        {
          let start = cursor
            .position()
            .or_else(|| cursor.position_in(bounds))
            .unwrap_or(core::Point::ORIGIN);
          state.is_pressed = true;
          state.direction = Some(direction);
          state.start = Some(start);
          state.current = Some(start);
          if let Some(on_resize) = self.on_resize.as_ref() {
            shell.publish(on_resize(resize_event(
              ResizePhase::Started,
              direction,
              start,
              start,
              bounds,
            )));
          }
          shell.capture_event();
          return;
        }
      }
      Event::Mouse(mouse::Event::CursorMoved { .. }) => {
        if state.is_pressed
          && let Some(direction) = state.direction
        {
          if let Some(on_resize) = self.on_resize.as_ref()
            && let Some(start) = state.start
          {
            let current = cursor.position().or(state.current).unwrap_or(start);
            state.current = Some(current);
            shell.publish(on_resize(resize_event(
              ResizePhase::Dragged,
              direction,
              start,
              current,
              bounds,
            )));
          } else if let Some(on_pressed_move) = self.on_pressed_move.as_ref() {
            *state = State::default();
            shell.publish(on_pressed_move(direction));
          }
          shell.capture_event();
          return;
        }
      }
      Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
        if state.is_pressed {
          if let Some(on_resize) = self.on_resize.as_ref()
            && let Some(direction) = state.direction
            && let Some(start) = state.start
          {
            let current = cursor.position().or(state.current).unwrap_or(start);
            shell.publish(on_resize(resize_event(
              ResizePhase::Ended,
              direction,
              start,
              current,
              bounds,
            )));
          }
          *state = State::default();
          shell.capture_event();
          return;
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
    let content_interaction =
      self
        .content
        .as_widget()
        .mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer);

    if !self.allows_resize() {
      return content_interaction;
    }

    let bounds = layout.bounds();
    if let Some(position) = cursor.position_in(bounds) {
      match resize_direction_at(position, Size::new(bounds.width, bounds.height), self.edge_margin) {
        Some(Direction::NorthWest | Direction::SouthEast) => mouse::Interaction::ResizingDiagonallyDown,
        Some(Direction::NorthEast | Direction::SouthWest) => mouse::Interaction::ResizingDiagonallyUp,
        Some(Direction::West | Direction::East) => mouse::Interaction::ResizingHorizontally,
        Some(Direction::North | Direction::South) => mouse::Interaction::ResizingVertically,
        None => content_interaction,
      }
    } else {
      content_interaction
    }
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

impl<'a, Message, Theme, Renderer> From<ResizeArea<'a, Message, Theme, Renderer>>
  for Element<'a, Message, Theme, Renderer>
where
  Message: 'a + Clone,
  Theme: 'a,
  Renderer: 'a + renderer::Renderer,
{
  fn from(area: ResizeArea<'a, Message, Theme, Renderer>) -> Self {
    Element::new(area)
  }
}

pub const DEFAULT_EDGE_MARGIN: f32 = 10.0;

fn pressed_resize_direction(
  position: Option<core::Point>,
  size: Size,
  edge_margin: f32,
  enabled: bool,
  window_state: ResizeAreaWindowState,
) -> Option<Direction> {
  if !enabled || !window_state.allows_resize() {
    return None;
  }

  let Some(position) = position else {
    return None;
  };

  resize_direction_at(position, size, edge_margin)
}

fn resize_direction_at(position: core::Point, size: Size, margin: f32) -> Option<Direction> {
  let on_left = position.x <= margin;
  let on_right = position.x >= size.width - margin;
  let on_top = position.y <= margin;
  let on_bottom = position.y >= size.height - margin;

  if on_left && on_top {
    Some(Direction::NorthWest)
  } else if on_right && on_top {
    Some(Direction::NorthEast)
  } else if on_left && on_bottom {
    Some(Direction::SouthWest)
  } else if on_right && on_bottom {
    Some(Direction::SouthEast)
  } else if on_left {
    Some(Direction::West)
  } else if on_right {
    Some(Direction::East)
  } else if on_top {
    Some(Direction::North)
  } else if on_bottom {
    Some(Direction::South)
  } else {
    None
  }
}

fn resize_event(
  phase: ResizePhase,
  direction: Direction,
  start: core::Point,
  current: core::Point,
  bounds: Rectangle,
) -> ResizeAreaEvent {
  ResizeAreaEvent {
    phase,
    direction,
    start,
    current,
    delta: current - start,
    bounds,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use lintel_lib_iced::iced_core::{Point, Size};
  use std::cell::Cell;
  use std::rc::Rc;

  #[derive(Debug, Clone, Copy)]
  enum TestMessage {
    Resize(Direction),
    ResizeEvent(ResizeAreaEvent),
    Drag,
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

  fn counting_resize_area(updates: Rc<Cell<usize>>) -> ResizeArea<'static, TestMessage, (), TestRenderer> {
    ResizeArea::new(Element::new(CountingContent { updates })).on_pressed_move(TestMessage::Resize)
  }

  fn continuous_resize_area(updates: Rc<Cell<usize>>) -> ResizeArea<'static, TestMessage, (), TestRenderer> {
    ResizeArea::new(Element::new(CountingContent { updates })).on_resize(TestMessage::ResizeEvent)
  }

  fn nested_drag_resize_area(updates: Rc<Cell<usize>>) -> ResizeArea<'static, TestMessage, (), TestRenderer> {
    let drag_area =
      crate::drag_area::DragArea::new(Element::new(CountingContent { updates })).on_drag(TestMessage::Drag);

    ResizeArea::new(Element::new(drag_area)).on_pressed_move(TestMessage::Resize)
  }

  fn resize_area_tree(area: &ResizeArea<'_, TestMessage, (), TestRenderer>) -> Tree {
    Tree {
      tag: tree::Tag::of::<State>(),
      state: tree::State::new(State::default()),
      children: vec![Tree::new(&area.content)],
    }
  }

  fn test_layout_node() -> layout::Node {
    layout::Node::new(Size::new(100.0, 80.0))
  }

  fn update_resize_area(
    area: &mut ResizeArea<'_, TestMessage, (), TestRenderer>,
    tree: &mut Tree,
    event: Event,
    cursor: mouse::Cursor,
    messages: &mut Vec<TestMessage>,
  ) -> bool {
    let node = test_layout_node();
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
  fn stale_window_state_query_cannot_override_latest_state() {
    let mut tracker = ResizeAreaWindowStateTracker::default();
    let stale_revision = tracker.begin_query();
    let latest_revision = tracker.begin_query();
    let normal = ResizeAreaWindowState::default();
    let maximized = ResizeAreaWindowState {
      is_maximized: true,
      is_fullscreen: false,
    };

    assert!(tracker.apply_query_result(latest_revision, normal));
    assert!(!tracker.apply_query_result(stale_revision, maximized));
    assert_eq!(tracker.state(), normal);
  }

  #[test]
  fn lifecycle_refresh_invalidates_query_started_before_transition() {
    let mut tracker = ResizeAreaWindowStateTracker::default();
    let initial_revision = tracker.begin_query();
    let maximized = ResizeAreaWindowState {
      is_maximized: true,
      is_fullscreen: false,
    };
    assert!(tracker.apply_query_result(initial_revision, maximized));

    let before_transition_revision = tracker.begin_query();
    let after_transition_revision = tracker.begin_query();

    assert!(!tracker.apply_query_result(before_transition_revision, ResizeAreaWindowState::default()));
    assert_eq!(tracker.state(), maximized);
    assert!(tracker.apply_query_result(after_transition_revision, ResizeAreaWindowState::default()));
    assert_eq!(tracker.state(), ResizeAreaWindowState::default());
  }

  #[test]
  fn lifecycle_transition_locks_resize_until_latest_query_finishes() {
    let mut tracker = ResizeAreaWindowStateTracker::default();
    let before_transition_revision = tracker.begin_query();
    assert!(tracker.apply_query_result(before_transition_revision, ResizeAreaWindowState::default()));

    tracker.begin_transition();

    assert!(!tracker.state().is_resizable());
    assert!(!tracker.apply_query_result(before_transition_revision, ResizeAreaWindowState::default()));
    assert!(!tracker.state().is_resizable());

    let after_transition_revision = tracker.begin_query();
    assert!(tracker.apply_query_result(after_transition_revision, ResizeAreaWindowState::default()));
    assert!(tracker.state().is_resizable());
  }

  #[test]
  fn pending_transition_releases_top_edge_press_to_child() {
    let mut tracker = ResizeAreaWindowStateTracker::default();
    tracker.begin_transition();
    let updates = Rc::new(Cell::new(0));
    let mut area = counting_resize_area(updates.clone()).window_state(tracker.state());
    let mut tree = resize_area_tree(&area);
    let mut messages = Vec::new();

    let captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(1.0, 1.0)),
      &mut messages,
    );

    assert!(!captured);
    assert!(messages.is_empty());
    assert_eq!(updates.get(), 1);
  }

  #[test]
  fn observational_refresh_between_edge_press_and_move_keeps_resize_gesture() {
    let mut tracker = ResizeAreaWindowStateTracker::default();
    let initial_revision = tracker.begin_query();
    assert!(tracker.apply_query_result(initial_revision, ResizeAreaWindowState::default()));
    let updates = Rc::new(Cell::new(0));
    let mut area = counting_resize_area(updates.clone()).window_state(tracker.state());
    let mut tree = resize_area_tree(&area);
    let mut messages = Vec::new();

    let press_captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(1.0, 1.0)),
      &mut messages,
    );
    let focused_refresh_revision = tracker.begin_query();
    area.window_state = tracker.state();
    let move_captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::CursorMoved {
        position: Point::new(2.0, 2.0),
      }),
      mouse::Cursor::Available(Point::new(2.0, 2.0)),
      &mut messages,
    );

    assert!(press_captured);
    assert!(move_captured);
    assert!(matches!(
      messages.as_slice(),
      [TestMessage::Resize(Direction::NorthWest)]
    ));
    assert_eq!(updates.get(), 0);
    assert!(tracker.apply_query_result(focused_refresh_revision, ResizeAreaWindowState::default()));
  }

  #[test]
  fn disabled_resize_area_ignores_edge_press() {
    let direction = pressed_resize_direction(
      Some(Point::new(1.0, 1.0)),
      Size::new(100.0, 80.0),
      DEFAULT_EDGE_MARGIN,
      false,
      ResizeAreaWindowState::default(),
    );

    assert!(direction.is_none());
  }

  #[test]
  fn maximized_window_state_ignores_edge_press() {
    let direction = pressed_resize_direction(
      Some(Point::new(1.0, 1.0)),
      Size::new(100.0, 80.0),
      DEFAULT_EDGE_MARGIN,
      true,
      ResizeAreaWindowState {
        is_maximized: true,
        is_fullscreen: false,
      },
    );

    assert!(direction.is_none());
  }

  #[test]
  fn fullscreen_window_state_ignores_edge_press() {
    let direction = pressed_resize_direction(
      Some(Point::new(1.0, 1.0)),
      Size::new(100.0, 80.0),
      DEFAULT_EDGE_MARGIN,
      true,
      ResizeAreaWindowState {
        is_maximized: false,
        is_fullscreen: true,
      },
    );

    assert!(direction.is_none());
  }

  #[test]
  fn fullscreen_window_mode_disables_resize() {
    let window_state = ResizeAreaWindowState::new(false, iced::window::Mode::Fullscreen);

    assert!(!window_state.is_resizable());
  }

  #[test]
  fn non_edge_press_does_not_start_resize_session() {
    let direction = pressed_resize_direction(
      Some(Point::new(50.0, 40.0)),
      Size::new(100.0, 80.0),
      DEFAULT_EDGE_MARGIN,
      true,
      ResizeAreaWindowState::default(),
    );

    assert!(direction.is_none());
  }

  #[test]
  fn non_edge_press_updates_child() {
    let updates = Rc::new(Cell::new(0));
    let mut area = counting_resize_area(updates.clone());
    let mut tree = resize_area_tree(&area);
    let mut messages = Vec::new();

    let captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(50.0, 40.0)),
      &mut messages,
    );

    assert!(!captured);
    assert!(messages.is_empty());
    assert_eq!(updates.get(), 1);
  }

  #[test]
  fn edge_press_captures_resize_gesture_and_does_not_update_child() {
    let updates = Rc::new(Cell::new(0));
    let mut area = counting_resize_area(updates.clone());
    let mut tree = resize_area_tree(&area);
    let mut messages = Vec::new();

    let captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(1.0, 1.0)),
      &mut messages,
    );

    assert!(captured);
    assert!(messages.is_empty());
    assert_eq!(updates.get(), 0);
  }

  #[test]
  fn edge_move_after_edge_press_publishes_resize_and_does_not_update_child() {
    let updates = Rc::new(Cell::new(0));
    let mut area = counting_resize_area(updates.clone());
    let mut tree = resize_area_tree(&area);
    let mut messages = Vec::new();

    let press_captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(1.0, 1.0)),
      &mut messages,
    );
    let move_captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::CursorMoved {
        position: Point::new(2.0, 2.0),
      }),
      mouse::Cursor::Available(Point::new(2.0, 2.0)),
      &mut messages,
    );

    assert!(press_captured);
    assert!(move_captured);
    assert!(matches!(
      messages.as_slice(),
      [TestMessage::Resize(Direction::NorthWest)]
    ));
    assert_eq!(updates.get(), 0);
  }

  #[test]
  fn edge_drag_resize_events_report_continuous_delta_until_release() {
    let updates = Rc::new(Cell::new(0));
    let mut area = continuous_resize_area(updates.clone());
    let mut tree = resize_area_tree(&area);
    let mut messages = Vec::new();

    let press_captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(99.0, 40.0)),
      &mut messages,
    );
    let first_move_captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::CursorMoved {
        position: Point::new(120.0, 45.0),
      }),
      mouse::Cursor::Available(Point::new(120.0, 45.0)),
      &mut messages,
    );
    let second_move_captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::CursorMoved {
        position: Point::new(130.0, 50.0),
      }),
      mouse::Cursor::Available(Point::new(130.0, 50.0)),
      &mut messages,
    );
    let release_captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(130.0, 50.0)),
      &mut messages,
    );

    assert!(press_captured);
    assert!(first_move_captured);
    assert!(second_move_captured);
    assert!(release_captured);
    assert_eq!(updates.get(), 0);
    assert_eq!(messages.len(), 4);

    let TestMessage::ResizeEvent(started) = messages[0] else {
      panic!("expected resize started event");
    };
    assert_eq!(started.phase, ResizePhase::Started);
    assert!(matches!(started.direction, Direction::East));
    assert_eq!(started.start, Point::new(99.0, 40.0));
    assert_eq!(started.current, Point::new(99.0, 40.0));
    assert_eq!(started.delta, Vector::new(0.0, 0.0));
    assert_eq!(started.bounds.width, 100.0);
    assert_eq!(started.bounds.height, 80.0);

    let TestMessage::ResizeEvent(first_dragged) = messages[1] else {
      panic!("expected first resize dragged event");
    };
    assert_eq!(first_dragged.phase, ResizePhase::Dragged);
    assert!(matches!(first_dragged.direction, Direction::East));
    assert_eq!(first_dragged.start, Point::new(99.0, 40.0));
    assert_eq!(first_dragged.current, Point::new(120.0, 45.0));
    assert_eq!(first_dragged.delta, Vector::new(21.0, 5.0));

    let TestMessage::ResizeEvent(second_dragged) = messages[2] else {
      panic!("expected second resize dragged event");
    };
    assert_eq!(second_dragged.phase, ResizePhase::Dragged);
    assert_eq!(second_dragged.current, Point::new(130.0, 50.0));
    assert_eq!(second_dragged.delta, Vector::new(31.0, 10.0));

    let TestMessage::ResizeEvent(ended) = messages[3] else {
      panic!("expected resize ended event");
    };
    assert_eq!(ended.phase, ResizePhase::Ended);
    assert_eq!(ended.current, Point::new(130.0, 50.0));
    assert_eq!(ended.delta, Vector::new(31.0, 10.0));
  }

  #[test]
  fn nested_drag_area_receives_non_edge_drag_move() {
    let updates = Rc::new(Cell::new(0));
    let mut area = nested_drag_resize_area(updates.clone());
    let mut tree = resize_area_tree(&area);
    let mut messages = Vec::new();

    let press_captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(50.0, 40.0)),
      &mut messages,
    );
    let move_captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::CursorMoved {
        position: Point::new(56.0, 40.0),
      }),
      mouse::Cursor::Available(Point::new(56.0, 40.0)),
      &mut messages,
    );

    assert!(press_captured);
    assert!(move_captured);
    assert!(matches!(messages.as_slice(), [TestMessage::Drag]));
    assert_eq!(updates.get(), 2);
  }

  #[test]
  fn nested_drag_area_does_not_receive_edge_resize_press_or_move() {
    let updates = Rc::new(Cell::new(0));
    let mut area = nested_drag_resize_area(updates.clone());
    let mut tree = resize_area_tree(&area);
    let mut messages = Vec::new();

    let press_captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(1.0, 1.0)),
      &mut messages,
    );
    let move_captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::CursorMoved {
        position: Point::new(2.0, 2.0),
      }),
      mouse::Cursor::Available(Point::new(2.0, 2.0)),
      &mut messages,
    );

    assert!(press_captured);
    assert!(move_captured);
    assert!(matches!(
      messages.as_slice(),
      [TestMessage::Resize(Direction::NorthWest)]
    ));
    assert_eq!(updates.get(), 0);
  }

  #[test]
  fn cursor_move_without_resize_press_updates_child() {
    let updates = Rc::new(Cell::new(0));
    let mut area = counting_resize_area(updates.clone());
    let mut tree = resize_area_tree(&area);
    let mut messages = Vec::new();

    let captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::CursorMoved {
        position: Point::new(2.0, 20.0),
      }),
      mouse::Cursor::Available(Point::new(2.0, 20.0)),
      &mut messages,
    );

    assert!(!captured);
    assert!(messages.is_empty());
    assert_eq!(updates.get(), 1);
  }

  #[test]
  fn button_release_without_resize_press_updates_child() {
    let updates = Rc::new(Cell::new(0));
    let mut area = counting_resize_area(updates.clone());
    let mut tree = resize_area_tree(&area);
    let mut messages = Vec::new();

    let captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(2.0, 20.0)),
      &mut messages,
    );

    assert!(!captured);
    assert!(messages.is_empty());
    assert_eq!(updates.get(), 1);
  }

  #[test]
  fn disabled_area_edge_press_updates_child() {
    let updates = Rc::new(Cell::new(0));
    let mut area = counting_resize_area(updates.clone()).enabled(false);
    let mut tree = resize_area_tree(&area);
    let mut messages = Vec::new();

    let captured = update_resize_area(
      &mut area,
      &mut tree,
      Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
      mouse::Cursor::Available(Point::new(1.0, 1.0)),
      &mut messages,
    );

    assert!(!captured);
    assert!(messages.is_empty());
    assert_eq!(updates.get(), 1);
  }
}
