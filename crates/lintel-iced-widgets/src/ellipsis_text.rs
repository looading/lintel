use std::{borrow::Cow, marker::PhantomData};

use lintel_lib_iced::iced_core::layout;
use lintel_lib_iced::iced_core::mouse;
use lintel_lib_iced::iced_core::renderer;
use lintel_lib_iced::iced_core::text;
use lintel_lib_iced::iced_core::text::paragraph::{self, Paragraph};
use lintel_lib_iced::iced_core::widget::Tree;
use lintel_lib_iced::iced_core::{Color, Element, Font, Layout, Length, Rectangle, Size, Theme, Widget};
use lintel_lib_iced::{
  alignment::{Horizontal, Vertical},
  widget::text::LineHeight,
};
use lintel_lib_utils::time::truncate_with_ellipsis;

#[derive(Debug, Clone, Copy)]
enum EllipsisMode {
  Fixed(usize),
  Auto,
}

pub struct EllipsisText<'a, Message> {
  content: Cow<'a, str>,
  mode: EllipsisMode,
  width: Length,
  align_x: Horizontal,
  align_y: Vertical,
  size: Option<u32>,
  line_height: Option<LineHeight>,
  color: Option<Color>,
  font: Option<Font>,
  _marker: PhantomData<&'a Message>,
}

impl<'a, Message> EllipsisText<'a, Message> {
  pub fn new(content: impl Into<Cow<'a, str>>, limit: usize) -> Self {
    Self {
      content: content.into(),
      mode: EllipsisMode::Fixed(limit),
      width: Length::Shrink,
      align_x: Horizontal::Left,
      align_y: Vertical::Center,
      size: None,
      line_height: None,
      color: None,
      font: None,
      _marker: PhantomData,
    }
  }

  pub fn limit(mut self, limit: usize) -> Self {
    self.mode = EllipsisMode::Fixed(limit);
    self
  }

  pub fn auto_fit(mut self) -> Self {
    self.mode = EllipsisMode::Auto;
    self
  }

  pub fn width(mut self, width: impl Into<Length>) -> Self {
    self.width = width.into();
    self
  }

  pub fn align_x(mut self, align_x: Horizontal) -> Self {
    self.align_x = align_x;
    self
  }

  pub fn align_y(mut self, align_y: Vertical) -> Self {
    self.align_y = align_y;
    self
  }

  pub fn size(mut self, size: u32) -> Self {
    self.size = Some(size);
    self
  }

  pub fn line_height(mut self, line_height: LineHeight) -> Self {
    self.line_height = Some(line_height);
    self
  }

  pub fn color(mut self, color: impl Into<Color>) -> Self {
    self.color = Some(color.into());
    self
  }

  pub fn font(mut self, font: impl Into<Font>) -> Self {
    self.font = Some(font.into());
    self
  }

  fn text_definition<'b, Renderer>(
    &self,
    renderer: &Renderer,
    bounds: Size,
    content: &'b str,
  ) -> text::Text<&'b str, Renderer::Font>
  where
    Renderer: text::Renderer<Font = Font>,
  {
    text::Text {
      content,
      bounds,
      size: self.size.map(Into::into).unwrap_or_else(|| renderer.default_size()),
      line_height: self.line_height.unwrap_or_default(),
      font: self.font.unwrap_or_else(|| renderer.default_font()),
      align_x: self.align_x.into(),
      align_y: self.align_y,
      shaping: text::Shaping::Advanced,
      wrapping: text::Wrapping::None,
      hint_factor: renderer.scale_factor(),
    }
  }

  fn fit_content<Renderer>(&self, renderer: &Renderer, bounds: Size) -> String
  where
    Renderer: text::Renderer<Font = Font>,
  {
    match self.mode {
      EllipsisMode::Fixed(limit) => truncate_with_ellipsis(self.content.as_ref(), limit),
      EllipsisMode::Auto => self.fit_content_to_bounds(renderer, bounds),
    }
  }

  fn fit_content_to_bounds<Renderer>(&self, renderer: &Renderer, bounds: Size) -> String
  where
    Renderer: text::Renderer<Font = Font>,
  {
    if !bounds.width.is_finite() || bounds.width <= 0.0 {
      return self.content.to_string();
    }

    let content = self.content.as_ref();
    let full_text = self.text_definition(renderer, bounds, content);
    if Renderer::Paragraph::with_text(full_text).min_width() <= bounds.width {
      return content.to_string();
    }

    let chars = content.char_indices().map(|(index, _)| index).collect::<Vec<_>>();
    if chars.is_empty() {
      return String::new();
    }

    let ellipsis = "...";
    let ellipsis_text = self.text_definition(renderer, bounds, ellipsis);
    if Renderer::Paragraph::with_text(ellipsis_text).min_width() > bounds.width {
      return String::new();
    }

    let mut low = 0usize;
    let mut high = chars.len();
    let mut best = String::from(ellipsis);

    while low < high {
      let mid = (low + high).div_ceil(2);
      let end = chars.get(mid).copied().unwrap_or(content.len());
      let candidate = format!("{}{}", &content[..end], ellipsis);
      let candidate_text = self.text_definition(renderer, bounds, &candidate);

      if Renderer::Paragraph::with_text(candidate_text).min_width() <= bounds.width {
        best = candidate;
        low = mid;
      } else {
        high = mid - 1;
      }
    }

    best
  }
}

impl<Message, Renderer> Widget<Message, Theme, Renderer> for EllipsisText<'_, Message>
where
  Renderer: text::Renderer<Font = Font>,
{
  fn tag(&self) -> lintel_lib_iced::iced_core::widget::tree::Tag {
    lintel_lib_iced::iced_core::widget::tree::Tag::of::<paragraph::Plain<Renderer::Paragraph>>()
  }

  fn state(&self) -> lintel_lib_iced::iced_core::widget::tree::State {
    lintel_lib_iced::iced_core::widget::tree::State::new(paragraph::Plain::<Renderer::Paragraph>::default())
  }

  fn size(&self) -> Size<Length> {
    Size {
      width: self.width,
      height: Length::Shrink,
    }
  }

  fn layout(&mut self, tree: &mut Tree, renderer: &Renderer, limits: &layout::Limits) -> layout::Node {
    layout::sized(limits, self.width, Length::Shrink, |limits| {
      let bounds = limits.max();
      let content = self.fit_content(renderer, bounds);
      let text = self.text_definition(renderer, bounds, &content);
      let paragraph = tree.state.downcast_mut::<paragraph::Plain<Renderer::Paragraph>>();
      let _ = paragraph.update(text);
      paragraph.min_bounds()
    })
  }

  fn draw(
    &self,
    tree: &Tree,
    renderer: &mut Renderer,
    _theme: &Theme,
    defaults: &renderer::Style,
    layout: Layout<'_>,
    _cursor_position: mouse::Cursor,
    viewport: &Rectangle,
  ) {
    let paragraph = tree.state.downcast_ref::<paragraph::Plain<Renderer::Paragraph>>();
    let anchor = layout
      .bounds()
      .anchor(paragraph.min_bounds(), paragraph.align_x(), paragraph.align_y());

    renderer.fill_paragraph(
      paragraph.raw(),
      anchor,
      self.color.unwrap_or(defaults.text_color),
      *viewport,
    );
  }
}

impl<'a, Message: 'a, Renderer> From<EllipsisText<'a, Message>> for Element<'a, Message, Theme, Renderer>
where
  Renderer: text::Renderer<Font = Font> + 'a,
{
  fn from(widget: EllipsisText<'a, Message>) -> Self {
    Element::new(widget)
  }
}
