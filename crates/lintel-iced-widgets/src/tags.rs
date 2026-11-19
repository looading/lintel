use std::marker::PhantomData;

use lintel_lib_iced::{
  self as iced, Border, Color, Element, Padding, normal_font,
  widget::{button, column, container, row, text},
};
use serde::{Deserialize, Serialize};

use crate::{svg, svg_resources};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagValue {
  pub key: String,
  pub text: String,
}

impl TagValue {
  pub fn new(key: impl Into<String>, text: impl Into<String>) -> Self {
    Self {
      key: key.into(),
      text: text.into(),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TagType {
  Info,
  Error,
  Warn,
  Cancel,
  Default,
  Custom(TagPreset),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TagItem {
  pub value: TagValue,
  pub tag_type: TagType,
  pub category: TagCategory,
}

impl TagItem {
  pub fn new(key: impl Into<String>, text: impl Into<String>, tag_type: TagType, category: TagCategory) -> Self {
    Self {
      value: TagValue::new(key, text),
      tag_type,
      category,
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagCategory {
  Strong,
  Normal,
  Weak,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagSize {
  Small,
  Normal,
  Large,
  Custom(u32),
}

impl TagSize {
  fn text_size(self) -> u32 {
    match self {
      TagSize::Small => 12,
      TagSize::Normal => 14,
      TagSize::Large => 20,
      TagSize::Custom(size) => size,
    }
  }

  fn line_height(self) -> u32 {
    match self {
      TagSize::Small => 16,
      TagSize::Normal => 18,
      TagSize::Large => 24,
      TagSize::Custom(size) => size,
    }
  }

  fn close_icon_size(self) -> u32 {
    match self {
      TagSize::Small => 12,
      TagSize::Normal => 12,
      TagSize::Large => 14,
      TagSize::Custom(size) => size.min(16),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagsDirection {
  Horizontal,
  Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TagPreset {
  pub color: Color,
}

impl TagPreset {
  fn from_tag_type(tag_type: TagType) -> Self {
    match tag_type {
      TagType::Info => Self {
        color: Color::from_rgb8(0x19, 0x66, 0xFF),
      },
      TagType::Error => Self {
        color: Color::from_rgb8(0xF5, 0x3F, 0x3F),
      },
      TagType::Warn => Self {
        color: Color::from_rgb8(0xFF, 0x7D, 0x00),
      },
      TagType::Cancel => Self {
        color: Color::from_rgb8(0x89, 0x8B, 0x8F),
      },
      TagType::Default => Self {
        color: Color::from_rgb8(0x56, 0x59, 0x60),
      },
      TagType::Custom(style) => style,
    }
  }

  fn colors(self, category: TagCategory) -> (Color, Color, Color) {
    match category {
      TagCategory::Strong => (Color::WHITE, self.color, Color::TRANSPARENT),
      TagCategory::Normal => (self.color, Color { a: 0.08, ..self.color }, Color::TRANSPARENT),
      TagCategory::Weak => (self.color, Color::TRANSPARENT, Color { a: 0.24, ..self.color }),
    }
  }
}

pub struct Tag<'a, Message> {
  value: TagValue,
  tag_type: TagType,
  category: TagCategory,
  padding: Padding,
  size: TagSize,
  closeable: bool,
  on_close: Option<Message>,
  _marker: PhantomData<&'a Message>,
}

impl<'a, Message> Tag<'a, Message> {
  pub fn new(value: TagValue, tag_type: TagType, category: TagCategory) -> Self {
    Self {
      value,
      tag_type,
      category,
      padding: Padding {
        top: 2.0,
        right: 6.0,
        bottom: 2.0,
        left: 6.0,
      },
      size: TagSize::Small,
      closeable: false,
      on_close: None,
      _marker: PhantomData,
    }
  }

  pub fn padding(mut self, padding: Padding) -> Self {
    self.padding = padding;
    self
  }

  pub fn size(mut self, size: TagSize) -> Self {
    self.size = size;
    self
  }

  pub fn text_size(mut self, text_size: u32) -> Self {
    self.size = TagSize::Custom(text_size);
    self
  }

  pub fn closeable(mut self, closeable: bool) -> Self {
    self.closeable = closeable;
    self
  }

  pub fn on_close(mut self, message: Message) -> Self {
    self.on_close = Some(message);
    self
  }
}

impl<'a, Message: Clone + 'a> From<Tag<'a, Message>> for Element<'a, Message> {
  fn from(tag: Tag<'a, Message>) -> Self {
    let preset = TagPreset::from_tag_type(tag.tag_type);
    let (text_color, background_color, border_color) = preset.colors(tag.category);
    let show_close_button = tag.closeable && tag.on_close.is_some();
    let text_size = tag.size.text_size();
    let line_height = tag.size.line_height();
    let close_icon_size = tag.size.close_icon_size();

    let close_button: Element<'a, Message> = if tag.closeable {
      if let Some(message) = tag.on_close {
        container(
          button(
            svg::Svg::new(svg_resources::close())
              .width(close_icon_size)
              .height(close_icon_size)
              .style(move |_, _| svg::Style {
                color: Some(text_color),
              }),
          )
          .padding(Padding {
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
            left: 0.0,
          })
          .height(line_height)
          .style(move |_, _| iced::widget::button::Style {
            background: None,
            text_color,
            border: Border {
              radius: 0.0.into(),
              width: 0.0,
              color: Color::TRANSPARENT,
            },
            ..Default::default()
          })
          // .height(line_height)
          .on_press(message),
        )
        .width(close_icon_size)
        // .height(line_height)
        // .center_y(iced::Length::Shrink)
        .into()
      } else {
        container("")
          .width(0)
          // .height(line_height)
          // .center_y(iced::Length::Shrink)
          .into()
      }
    } else {
      container("")
        .width(0)
        // .height(line_height)
        // .center_y(iced::Length::Shrink)
        .into()
    };

    container(
      row![
        text(tag.value.text.clone())
          .font(normal_font())
          .size(text_size)
          .line_height(iced::widget::text::LineHeight::Absolute(line_height.into()))
          .height(line_height)
          .color(text_color),
        container("").width(if show_close_button { 2 } else { 0 }),
        close_button,
      ]
      .align_y(iced::Alignment::Center)
      .height(line_height),
    )
    .padding(tag.padding)
    .style(move |_theme| iced::widget::container::Style {
      background: Some(iced::Background::Color(background_color)),
      border: Border {
        radius: 4.0.into(),
        width: 1.0,
        color: border_color,
      },
      text_color: Some(text_color),
      ..Default::default()
    })
    .into()
  }
}

pub struct Tags<'a, Message> {
  values: Vec<TagItem>,
  spacing: f32,
  direction: TagsDirection,
  tag_padding: Padding,
  size: TagSize,
  closeable: bool,
  on_close: Option<Box<dyn Fn(&TagItem) -> Message + 'a>>,
  _marker: PhantomData<Message>,
}

impl<'a, Message> Tags<'a, Message> {
  pub fn new(values: impl Into<Vec<TagItem>>) -> Self {
    Self {
      values: values.into(),
      spacing: 4.0,
      direction: TagsDirection::Horizontal,
      tag_padding: Padding {
        top: 2.0,
        right: 6.0,
        bottom: 2.0,
        left: 6.0,
      },
      size: TagSize::Small,
      closeable: false,
      on_close: None,
      _marker: PhantomData,
    }
  }

  pub fn spacing(mut self, spacing: impl Into<f32>) -> Self {
    self.spacing = spacing.into();
    self
  }

  pub fn direction(mut self, direction: TagsDirection) -> Self {
    self.direction = direction;
    self
  }

  pub fn tag_padding(mut self, tag_padding: Padding) -> Self {
    self.tag_padding = tag_padding;
    self
  }

  pub fn size(mut self, size: TagSize) -> Self {
    self.size = size;
    self
  }

  pub fn text_size(mut self, text_size: u32) -> Self {
    self.size = TagSize::Custom(text_size);
    self
  }

  pub fn closeable(mut self, closeable: bool) -> Self {
    self.closeable = closeable;
    self
  }

  pub fn on_close(mut self, on_close: impl Fn(&TagItem) -> Message + 'a) -> Self {
    self.on_close = Some(Box::new(on_close));
    self
  }
}

impl<'a, Message: Clone + 'a> From<Tags<'a, Message>> for Element<'a, Message> {
  fn from(tags: Tags<'a, Message>) -> Self {
    let children = tags.values.into_iter().map(|item| {
      let tag = Tag::<Message>::new(item.value.clone(), item.tag_type, item.category)
        .padding(tags.tag_padding)
        .size(tags.size)
        .closeable(tags.closeable);

      let tag = if tags.closeable {
        if let Some(on_close) = tags.on_close.as_ref() {
          tag.on_close(on_close(&item))
        } else {
          tag
        }
      } else {
        tag
      };

      Element::from(tag)
    });

    match tags.direction {
      TagsDirection::Horizontal => row(children).spacing(tags.spacing).into(),
      TagsDirection::Vertical => column(children).spacing(tags.spacing).into(),
    }
  }
}
