use lintel_lib_iced::widget::button::{Status, Style};
use lintel_lib_iced::widget::{button, row};
use lintel_lib_iced::widget::{container, text};
use lintel_lib_iced::{Background, Border, Color, Element, color, iced};

use crate::mouse_area::{self};
use crate::svg::Svg;
use crate::svg_resources;

#[derive(Debug, Clone)]
pub enum ToolbarMessage {
  UpdateState(ToolbarState),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ToolbarAction {
  Close,
  Maximize,
  Minimize,
  FullScreen(bool),
  Hover(bool),
}

#[derive(Debug, Clone, Default)]
pub struct ToolbarState {
  pub is_fullscreen: bool,
  pub is_hovered: bool,
}

pub fn toolbar(state: &ToolbarState) -> Element<'static, ToolbarAction> {
  let is_hovered = state.is_hovered;
  let is_fullscreen = state.is_fullscreen;

  let close_btn = button(if is_hovered {
    container(Svg::new(svg_resources::mac_close_active()).width(16.0).height(16.0)).center(16.0)
  } else {
    container(text!(""))
  })
  .style(move |_, status| {
    let (bg_color, t_color) = match (status, is_hovered) {
      (Status::Active, false) => (color!(0xFF6159), color!(0x4D0000)),
      (Status::Hovered, _) | (Status::Active, true) => (color!(0xFF6159), color!(0x4D0000)),
      (Status::Pressed, _) => (color!(0xBF4942), color!(0x190000)),
      (Status::Disabled, _) => (color!(0xdddddd), color!(0xdddddd)),
    };

    Style {
      background: Some(Background::Color(bg_color)),
      text_color: t_color,
      border: Border {
        radius: 8.0.into(),        // 设置8px圆角半径
        width: 1.0,                // 边框宽度
        color: Color::TRANSPARENT, // 边框颜色
      },
      ..Default::default()
    }
  })
  .width(16.0)
  .height(16.0)
  .padding(0)
  .on_press(ToolbarAction::Close);

  let min_btn = button(if !is_fullscreen && is_hovered {
    container(Svg::new(svg_resources::mac_min_active()).width(16.0).height(16.0)).center(16.0)
  } else {
    container(text!(""))
  })
  .on_press_maybe(if !is_fullscreen {
    Some(ToolbarAction::Minimize)
  } else {
    None
  })
  .width(16.0)
  .height(16.0)
  .padding(0)
  .style(move |_: &iced::Theme, status| {
    let (bg_color, t_color) = match (status, is_hovered) {
      (Status::Active, false) => (color!(0xFFBD2E), color!(0x995700)),
      (Status::Hovered, _) | (Status::Active, true) => (color!(0xFFBD2E), color!(0x995700)),
      (Status::Pressed, _) => (color!(0xBF8E22), color!(0x592800)),
      (Status::Disabled, _) => (color!(0xdddddd), color!(0xdddddd)),
    };

    Style {
      background: Some(Background::Color(bg_color)),
      text_color: t_color,
      border: Border {
        radius: 8.0.into(),        // 设置8px圆角半径
        width: 1.0,                // 边框宽度
        color: Color::TRANSPARENT, // 边框颜色
      },
      ..Default::default()
    }
  });

  let fullscreen_btn = button(if is_hovered {
    container(Svg::new(svg_resources::mac_max_active()).width(16.0).height(16.0)).center(16.0)
  } else {
    container(text!(""))
  })
  .on_press(ToolbarAction::FullScreen(!is_fullscreen))
  .width(16.0)
  .height(16.0)
  .padding(0)
  .style(move |_: &iced::Theme, status| {
    let (bg_color, t_color) = match (status, is_hovered) {
      (Status::Active, false) => (color!(0x28C941), color!(0x006500)),
      (Status::Hovered, _) | (Status::Active, true) => (color!(0x28C941), color!(0x006500)),
      (Status::Pressed, _) => (color!(0x1D9730), color!(0x003200)),
      (Status::Disabled, _) => (color!(0xdddddd), color!(0xdddddd)),
    };

    Style {
      background: Some(Background::Color(bg_color)),
      text_color: t_color,
      border: Border {
        radius: 8.0.into(),        // 设置8px圆角半径
        width: 1.0,                // 边框宽度
        color: Color::TRANSPARENT, // 边框颜色
      },
      ..Default::default()
    }
  });

  let content = mouse_area::mouse_area(row![close_btn, min_btn, fullscreen_btn].spacing(10.0), None)
    .on_enter(ToolbarAction::Hover(true))
    .on_exit(ToolbarAction::Hover(false));
  content.into()
}
