use crate::mouse_area::MouseArea;
use lintel_lib_iced::mouse;
use lintel_lib_iced::widget::canvas::{self, Canvas};
use lintel_lib_iced::widget::{button, column, container, row, stack, text};
use lintel_lib_iced::widget::{space::horizontal, space::vertical};
use lintel_lib_iced::{
  self as iced, Alignment, Background, Border, Color, Element, Fill, Font, Length, Padding, Point, Rectangle, Renderer,
  Shadow, Theme, color,
};
use serde::{Deserialize, Serialize};

const EDGE_PLACEMENT_VISUAL_OFFSET: f32 = 8.0;
const HOVER_HOTZONE_EXPAND: f32 = 32.0;
const HIT_TEST_ALPHA: f32 = 0.001;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonAlignment {
  BottomLeft,
  BottomCenter,
  #[default]
  BottomRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PopoverPlacement {
  Top,
  #[default]
  Right,
  RightTop,
  RightBottom,
  Bottom,
  Left,
  LeftTop,
  LeftBottom,
}

#[derive(Debug, Clone, Copy)]
pub struct PopoverStyle {
  pub background_color: Color,
  pub border_color: Color,
  pub border_radius: f32,
  pub shadow: Shadow,
}

impl Default for PopoverStyle {
  fn default() -> Self {
    Self {
      background_color: color!(0xffffff),
      border_color: color!(0xe6e8ee),
      border_radius: 8.0,
      shadow: Shadow {
        color: color!(0, 0, 0, 0.14),
        offset: iced::Vector::new(0.0, 8.0),
        blur_radius: 24.0,
      },
    }
  }
}

#[derive(Debug, Clone, Copy)]
pub struct PopoverCardStyle {
  pub title_color: Color,
  pub content_color: Color,
  pub button_color: Color,
  pub button_text_color: Color,
}

impl Default for PopoverCardStyle {
  fn default() -> Self {
    Self {
      title_color: color!(0x252931),
      content_color: color!(0x4f566b),
      button_color: color!(0x1966ff),
      button_text_color: color!(0xffffff),
    }
  }
}

#[derive(Debug, Clone)]
pub struct PopoverConfig {
  pub key: String,
  pub placement: PopoverPlacement,
  pub x: f32,
  pub y: f32,
  pub width: f32,
  pub height: f32,
  pub offset: f32,
  pub show_arrow: bool,
  pub arrow_size: f32,
  pub padding: Padding,
  pub close_on_mouse_leave: bool,
  pub style: PopoverStyle,
}

impl Default for PopoverConfig {
  fn default() -> Self {
    Self {
      key: "popover".to_string(),
      placement: PopoverPlacement::Right,
      x: 0.0,
      y: 0.0,
      width: 316.0,
      height: 180.0,
      offset: 2.0,
      show_arrow: true,
      arrow_size: 12.0,
      padding: Padding::from([16.0, 16.0]),
      close_on_mouse_leave: true,
      style: PopoverStyle::default(),
    }
  }
}

#[derive(Debug, Clone, Copy)]
enum ArrowDirection {
  Up,
  Right,
  Down,
  Left,
}

#[derive(Debug, Clone, Copy)]
enum CrossAxisAlignment {
  Start,
  Center,
  End,
}

#[derive(Debug, Clone)]
pub struct PopoverCardConfig {
  pub title: String,
  pub content: String,
  pub button_text: String,
  pub button_alignment: ButtonAlignment,
  pub style: PopoverCardStyle,
}

impl Default for PopoverCardConfig {
  fn default() -> Self {
    Self {
      title: String::new(),
      content: String::new(),
      button_text: "确定".to_string(),
      button_alignment: ButtonAlignment::BottomRight,
      style: PopoverCardStyle::default(),
    }
  }
}

fn action_button(style: PopoverCardStyle) -> impl Fn(&Theme, button::Status) -> button::Style {
  move |_: &Theme, status: button::Status| {
    let mut background = style.button_color;
    if let button::Status::Hovered = status {
      background.a = (background.a + 0.1).min(1.0);
    }
    button::Style {
      background: Some(Background::Color(background)),
      text_color: style.button_text_color,
      border: Border {
        radius: 6.0.into(),
        width: 0.0,
        color: Color::TRANSPARENT,
      },
      ..Default::default()
    }
  }
}

fn arrow_direction(placement: PopoverPlacement) -> ArrowDirection {
  match placement {
    PopoverPlacement::Top => ArrowDirection::Down,
    PopoverPlacement::Right | PopoverPlacement::RightTop | PopoverPlacement::RightBottom => ArrowDirection::Left,
    PopoverPlacement::Bottom => ArrowDirection::Up,
    PopoverPlacement::Left | PopoverPlacement::LeftTop | PopoverPlacement::LeftBottom => ArrowDirection::Right,
  }
}

fn cross_axis_alignment(placement: PopoverPlacement) -> CrossAxisAlignment {
  match placement {
    PopoverPlacement::RightTop | PopoverPlacement::LeftTop => CrossAxisAlignment::Start,
    PopoverPlacement::RightBottom | PopoverPlacement::LeftBottom => CrossAxisAlignment::End,
    _ => CrossAxisAlignment::Center,
  }
}

#[derive(Debug, Clone, Copy)]
struct ArrowCanvas {
  direction: ArrowDirection,
  fill: Color,
  border: Color,
}

impl<Message> canvas::Program<Message> for ArrowCanvas {
  type State = ();

  fn draw(
    &self,
    _state: &(),
    renderer: &Renderer,
    _theme: &Theme,
    bounds: Rectangle,
    _cursor: mouse::Cursor,
  ) -> Vec<canvas::Geometry> {
    let mut frame = canvas::Frame::new(renderer, bounds.size());

    let outer = triangle_path(self.direction, bounds.size(), 0.0);
    let inner = triangle_path(self.direction, bounds.size(), 1.0);

    frame.fill(&outer, self.border);
    frame.fill(&inner, self.fill);

    vec![frame.into_geometry()]
  }
}

fn triangle_path(direction: ArrowDirection, size: iced::Size, inset: f32) -> canvas::Path {
  let w = size.width.max(0.0);
  let h = size.height.max(0.0);
  let left = inset.min(w / 2.0);
  let right = (w - inset).max(left);
  let top = inset.min(h / 2.0);
  let bottom = (h - inset).max(top);
  let mid_x = (left + right) / 2.0;
  let mid_y = (top + bottom) / 2.0;

  let points = match direction {
    ArrowDirection::Left => [
      Point::new(left, mid_y),
      Point::new(right, top),
      Point::new(right, bottom),
    ],
    ArrowDirection::Right => [
      Point::new(right, mid_y),
      Point::new(left, top),
      Point::new(left, bottom),
    ],
    ArrowDirection::Up => [
      Point::new(mid_x, top),
      Point::new(left, bottom),
      Point::new(right, bottom),
    ],
    ArrowDirection::Down => [Point::new(mid_x, bottom), Point::new(left, top), Point::new(right, top)],
  };

  let mut builder = canvas::path::Builder::new();
  builder.move_to(points[0]);
  builder.line_to(points[1]);
  builder.line_to(points[2]);
  builder.close();
  builder.build()
}

fn arrow<'a, Message: 'a>(direction: ArrowDirection, fill: Color, border: Color, size: f32) -> Element<'a, Message> {
  Canvas::new(ArrowCanvas {
    direction,
    fill,
    border,
  })
  .width(size)
  .height(size)
  .into()
}

fn cross_axis_offset(placement: PopoverPlacement, panel_height: f32, arrow_size: f32) -> f32 {
  let cross_alignment = cross_axis_alignment(placement);
  let edge_offset = 18.0f32;
  let free_space = (panel_height - arrow_size).max(0.0);
  match cross_alignment {
    CrossAxisAlignment::Start => edge_offset.min(free_space),
    CrossAxisAlignment::Center => free_space / 2.0,
    CrossAxisAlignment::End => (free_space - edge_offset).max(0.0),
  }
}

fn arrow_extent(config: &PopoverConfig) -> f32 {
  if config.show_arrow {
    (config.arrow_size - 1.0).max(0.0)
  } else {
    0.0
  }
}

fn panel_origin(config: &PopoverConfig) -> (f32, f32) {
  match config.placement {
    PopoverPlacement::Right => (config.x + config.offset, (config.y - config.height / 2.0).max(0.0)),
    PopoverPlacement::RightTop => (
      config.x + config.offset,
      (config.y - EDGE_PLACEMENT_VISUAL_OFFSET).max(0.0),
    ),
    PopoverPlacement::RightBottom => (
      config.x + config.offset,
      (config.y - config.height + EDGE_PLACEMENT_VISUAL_OFFSET).max(0.0),
    ),
    PopoverPlacement::Left => (
      (config.x - config.offset - config.width).max(0.0),
      (config.y - config.height / 2.0).max(0.0),
    ),
    PopoverPlacement::LeftTop => (
      (config.x - config.offset - config.width).max(0.0),
      (config.y - EDGE_PLACEMENT_VISUAL_OFFSET).max(0.0),
    ),
    PopoverPlacement::LeftBottom => (
      (config.x - config.offset - config.width).max(0.0),
      (config.y - config.height + EDGE_PLACEMENT_VISUAL_OFFSET).max(0.0),
    ),
    PopoverPlacement::Top => (
      (config.x - config.width / 2.0).max(0.0),
      (config.y - config.offset - config.height).max(0.0),
    ),
    PopoverPlacement::Bottom => ((config.x - config.width / 2.0).max(0.0), config.y + config.offset),
  }
}

fn bubble_size(config: &PopoverConfig) -> (f32, f32) {
  let extent = arrow_extent(config);
  match config.placement {
    PopoverPlacement::Right
    | PopoverPlacement::RightTop
    | PopoverPlacement::RightBottom
    | PopoverPlacement::Left
    | PopoverPlacement::LeftTop
    | PopoverPlacement::LeftBottom => (config.width + extent, config.height),
    PopoverPlacement::Top | PopoverPlacement::Bottom => (config.width, config.height + extent),
  }
}

fn bubble_origin(config: &PopoverConfig, panel_x: f32, panel_y: f32) -> (f32, f32) {
  let extent = arrow_extent(config);
  match config.placement {
    PopoverPlacement::Right | PopoverPlacement::RightTop | PopoverPlacement::RightBottom => {
      ((panel_x - extent).max(0.0), panel_y)
    }
    PopoverPlacement::Left | PopoverPlacement::LeftTop | PopoverPlacement::LeftBottom => (panel_x, panel_y),
    PopoverPlacement::Top => (panel_x, panel_y),
    PopoverPlacement::Bottom => (panel_x, (panel_y - extent).max(0.0)),
  }
}

fn hover_insets(config: &PopoverConfig) -> (f32, f32, f32, f32) {
  match config.placement {
    PopoverPlacement::Right | PopoverPlacement::RightTop | PopoverPlacement::RightBottom => {
      (HOVER_HOTZONE_EXPAND, 0.0, 0.0, 0.0)
    }
    PopoverPlacement::Left | PopoverPlacement::LeftTop | PopoverPlacement::LeftBottom => {
      (0.0, HOVER_HOTZONE_EXPAND, 0.0, 0.0)
    }
    PopoverPlacement::Top => (0.0, 0.0, 0.0, HOVER_HOTZONE_EXPAND),
    PopoverPlacement::Bottom => (0.0, 0.0, HOVER_HOTZONE_EXPAND, 0.0),
  }
}

fn panel_local_origin(config: &PopoverConfig) -> (f32, f32) {
  let extent = arrow_extent(config);
  match config.placement {
    PopoverPlacement::Right | PopoverPlacement::RightTop | PopoverPlacement::RightBottom => (extent, 0.0),
    PopoverPlacement::Left | PopoverPlacement::LeftTop | PopoverPlacement::LeftBottom => (0.0, 0.0),
    PopoverPlacement::Top => (0.0, 0.0),
    PopoverPlacement::Bottom => (0.0, extent),
  }
}

fn arrow_local_origin(config: &PopoverConfig) -> Option<(f32, f32)> {
  if !config.show_arrow {
    return None;
  }

  let extent = arrow_extent(config);
  let overlap = config.arrow_size - extent;
  let y_offset = cross_axis_offset(config.placement, config.height, config.arrow_size);
  let x_center = ((config.width - config.arrow_size) / 2.0).max(0.0);

  match config.placement {
    PopoverPlacement::Right | PopoverPlacement::RightTop | PopoverPlacement::RightBottom => Some((0.0, y_offset)),
    PopoverPlacement::Left | PopoverPlacement::LeftTop | PopoverPlacement::LeftBottom => {
      Some((config.width - overlap, y_offset))
    }
    PopoverPlacement::Top => Some((x_center, config.height - overlap)),
    PopoverPlacement::Bottom => Some((x_center, 0.0)),
  }
}

fn position_in_box<'a, Message: 'a>(
  x: f32,
  y: f32,
  width: f32,
  height: f32,
  content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
  container(column![
    vertical().height(y.max(0.0)),
    row![
      horizontal().width(x.max(0.0)),
      content.into(),
      horizontal().width(Length::Fill)
    ],
    vertical().height(Length::Fill)
  ])
  .width(width)
  .height(height)
  .into()
}

pub fn popover<'a, Message: Clone + 'a>(
  config: &PopoverConfig,
  content: impl Into<Element<'a, Message>>,
  on_close: Option<Message>,
) -> Element<'a, Message> {
  let style = config.style;
  let panel = container(content)
    .padding(config.padding)
    .width(config.width)
    .height(config.height)
    .style(move |_| container::Style {
      background: Some(Background::Color(style.background_color)),
      border: Border {
        radius: style.border_radius.into(),
        width: 1.0,
        color: style.border_color,
      },
      shadow: style.shadow,
      ..Default::default()
    });

  let (bubble_width, bubble_height) = bubble_size(config);
  let (panel_local_x, panel_local_y) = panel_local_origin(config);

  let with_arrow: Element<'a, Message> = if let Some((arrow_x, arrow_y)) = arrow_local_origin(config) {
    stack![
      position_in_box(panel_local_x, panel_local_y, bubble_width, bubble_height, panel),
      position_in_box(
        arrow_x,
        arrow_y,
        bubble_width,
        bubble_height,
        arrow(
          arrow_direction(config.placement),
          style.background_color,
          style.border_color,
          config.arrow_size,
        )
      )
    ]
    .width(bubble_width)
    .height(bubble_height)
    .into()
  } else {
    position_in_box(panel_local_x, panel_local_y, bubble_width, bubble_height, panel)
  };

  let (panel_x, panel_y) = panel_origin(config);
  let (origin_x, origin_y) = bubble_origin(config, panel_x, panel_y);
  let (expand_left, expand_right, expand_top, expand_bottom) = hover_insets(config);
  let hotzone_width = bubble_width + expand_left + expand_right;
  let hotzone_height = bubble_height + expand_top + expand_bottom;
  let hotzone_content: Element<'a, Message> = container(position_in_box(
    expand_left,
    expand_top,
    hotzone_width,
    hotzone_height,
    with_arrow,
  ))
  .width(hotzone_width)
  .height(hotzone_height)
  .style(|_| container::Style {
    // Keep the hover bridge visually invisible while still participating in hit-testing
    // above native surfaces like the embedded webview.
    background: Some(Background::Color(Color {
      a: HIT_TEST_ALPHA,
      ..Color::TRANSPARENT
    })),
    ..Default::default()
  })
  .into();
  let bubble: Element<'a, Message> = match (config.close_on_mouse_leave, on_close) {
    (true, Some(on_close)) => MouseArea::new(hotzone_content, Some(config.key.clone()))
      .capture_clicks()
      .on_exit(on_close)
      .into(),
    _ => MouseArea::new(hotzone_content, Some(config.key.clone()))
      .capture_clicks()
      .into(),
  };

  container(column![
    vertical().height((origin_y - expand_top).max(0.0)),
    row![
      horizontal().width((origin_x - expand_left).max(0.0)),
      bubble,
      horizontal().width(Length::Fill)
    ],
    vertical().height(Length::Fill)
  ])
  .width(Length::Fill)
  .height(Length::Fill)
  .style(|_| container::Style {
    background: Some(Background::Color(Color::TRANSPARENT)),
    ..Default::default()
  })
  .into()
}

pub fn popover_card<'a, Message: Clone + 'a>(
  config: &PopoverConfig,
  card: &PopoverCardConfig,
  on_confirm: Message,
  on_close: Option<Message>,
) -> Element<'a, Message> {
  let style = card.style;
  let button = button(text(card.button_text.clone()).size(14).font(Font {
    weight: iced::font::Weight::Medium,
    ..Default::default()
  }))
  .padding(Padding::from([8.0, 16.0]))
  .style(action_button(style))
  .on_press(on_confirm);

  let footer = match card.button_alignment {
    ButtonAlignment::BottomLeft => row![button, horizontal().width(Length::Fill)],
    ButtonAlignment::BottomCenter => row![
      horizontal().width(Length::Fill),
      button,
      horizontal().width(Length::Fill)
    ],
    ButtonAlignment::BottomRight => row![horizontal().width(Length::Fill), button],
  }
  .align_y(Alignment::Center);

  let content = column![
    text(card.title.clone()).size(16).color(style.title_color).font(Font {
      weight: iced::font::Weight::Semibold,
      ..Default::default()
    }),
    vertical().height(12.0),
    text(card.content.clone()).size(14).color(style.content_color),
    vertical().height(Length::Fill),
    footer,
    vertical().height(8.0),
  ];

  popover(config, content, on_close)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn base_config(placement: PopoverPlacement) -> PopoverConfig {
    PopoverConfig {
      placement,
      x: 120.0,
      y: 140.0,
      width: 316.0,
      height: 180.0,
      show_arrow: false,
      ..PopoverConfig::default()
    }
  }

  fn hotzone_origin(config: &PopoverConfig) -> (f32, f32) {
    let (panel_x, panel_y) = panel_origin(config);
    let (origin_x, origin_y) = bubble_origin(config, panel_x, panel_y);
    let (expand_left, _expand_right, expand_top, _expand_bottom) = hover_insets(config);
    ((origin_x - expand_left).max(0.0), (origin_y - expand_top).max(0.0))
  }

  #[test]
  fn hover_insets_expand_towards_trigger_for_right_family() {
    let config = base_config(PopoverPlacement::Right);
    assert_eq!(hover_insets(&config), (HOVER_HOTZONE_EXPAND, 0.0, 0.0, 0.0));

    let config = base_config(PopoverPlacement::RightTop);
    assert_eq!(hover_insets(&config), (HOVER_HOTZONE_EXPAND, 0.0, 0.0, 0.0));

    let config = base_config(PopoverPlacement::RightBottom);
    assert_eq!(hover_insets(&config), (HOVER_HOTZONE_EXPAND, 0.0, 0.0, 0.0));
  }

  #[test]
  fn hover_insets_expand_towards_trigger_for_left_top_and_bottom() {
    let left = base_config(PopoverPlacement::Left);
    assert_eq!(hover_insets(&left), (0.0, HOVER_HOTZONE_EXPAND, 0.0, 0.0));

    let top = base_config(PopoverPlacement::Top);
    assert_eq!(hover_insets(&top), (0.0, 0.0, 0.0, HOVER_HOTZONE_EXPAND));

    let bottom = base_config(PopoverPlacement::Bottom);
    assert_eq!(hover_insets(&bottom), (0.0, 0.0, HOVER_HOTZONE_EXPAND, 0.0));
  }

  #[test]
  fn hotzone_origin_shifts_back_by_expanded_side() {
    let right = base_config(PopoverPlacement::Right);
    let right_panel = panel_origin(&right);
    let right_origin = hotzone_origin(&right);
    assert_eq!(right_origin.0, (right_panel.0 - HOVER_HOTZONE_EXPAND).max(0.0));
    assert_eq!(right_origin.1, right_panel.1);

    let top = base_config(PopoverPlacement::Top);
    let top_panel = panel_origin(&top);
    let top_origin = hotzone_origin(&top);
    assert_eq!(top_origin.0, top_panel.0);
    assert_eq!(top_origin.1, top_panel.1);

    let bottom = base_config(PopoverPlacement::Bottom);
    let bottom_panel = panel_origin(&bottom);
    let bottom_origin = hotzone_origin(&bottom);
    assert_eq!(bottom_origin.0, bottom_panel.0);
    assert_eq!(bottom_origin.1, (bottom_panel.1 - HOVER_HOTZONE_EXPAND).max(0.0));
  }

  #[test]
  fn hotzone_size_grows_by_expanded_distance() {
    let right = base_config(PopoverPlacement::Right);
    let (expand_left, expand_right, expand_top, expand_bottom) = hover_insets(&right);
    let (bubble_width, bubble_height) = bubble_size(&right);
    assert_eq!(
      bubble_width + expand_left + expand_right,
      right.width + HOVER_HOTZONE_EXPAND
    );
    assert_eq!(bubble_height + expand_top + expand_bottom, right.height);

    let top = base_config(PopoverPlacement::Top);
    let (expand_left, expand_right, expand_top, expand_bottom) = hover_insets(&top);
    let (bubble_width, bubble_height) = bubble_size(&top);
    assert_eq!(bubble_width + expand_left + expand_right, top.width);
    assert_eq!(
      bubble_height + expand_top + expand_bottom,
      top.height + HOVER_HOTZONE_EXPAND
    );
  }
}
