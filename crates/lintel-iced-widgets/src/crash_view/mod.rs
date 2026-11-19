use crate::{context_menu::Catalog, svg};
use iced::{Element, Length, Theme};
use lintel_lib_iced::{self as iced, alignment::Horizontal, color, normal_font};
use lintel_lib_utils::resource::ResourcePathImpl;
use semver::Version;

#[derive(Debug, Clone)]
pub enum Message {
  Reload,
  Copy,
  Clear,
}

#[derive(Debug, Clone)]
pub struct AppInfo {
  pub version: &'static Version,
  pub did: u64,
}

#[derive(Debug, Clone)]
pub enum CrashInfo {
  Crash { err_msg: String },
  BrowserFailed { err_msg: String },
  Terminate { err_msg: String },
  OutOfMemory { err_msg: String },
}

impl CrashInfo {
  pub fn get_err_msg(&self) -> &str {
    match self {
      CrashInfo::Crash { err_msg } => err_msg,
      CrashInfo::BrowserFailed { err_msg } => err_msg,
      CrashInfo::Terminate { err_msg } => err_msg,
      CrashInfo::OutOfMemory { err_msg } => err_msg,
    }
  }

  pub fn is_browser_failed(&self) -> bool {
    match self {
      CrashInfo::Crash { .. } => false,
      CrashInfo::BrowserFailed { .. } => true,
      CrashInfo::Terminate { .. } => false,
      CrashInfo::OutOfMemory { .. } => false,
    }
  }
}

pub struct CrashView<'a> {
  app_info: &'a AppInfo,
  error_info: &'a CrashInfo,
}

impl<'a> CrashView<'a> {
  pub fn new(app_info: &'a AppInfo, error_info: &'a CrashInfo) -> Self {
    Self { error_info, app_info }
  }
}

impl<'a> CrashView<'a> {
  pub fn view(&self) -> Element<'a, Message> {
    let reload_btn = iced::widget::button(
      iced::widget::text("重新加载")
        .font(normal_font())
        .size(14)
        .center()
        .line_height(iced::widget::text::LineHeight::Absolute(20.into())),
    )
    .padding(0)
    .style(|_, _| iced::widget::button::Style {
      background: Some(iced::Color::from_rgba8(255, 255, 255, 0.0).into()),
      text_color: color!(0x1966ff),
      ..Default::default()
    })
    .on_press(Message::Reload);

    #[cfg(debug_assertions)]
    let clear_btn = iced::widget::button(
      iced::widget::text("清除")
        .font(normal_font())
        .size(14)
        .center()
        .line_height(iced::widget::text::LineHeight::Absolute(20.into())),
    )
    .padding(0)
    .style(|_, _| iced::widget::button::Style {
      background: Some(iced::Color::from_rgba8(255, 255, 255, 0.0).into()),
      text_color: color!(0x1966ff),
      ..Default::default()
    })
    .on_press(Message::Clear);

    #[cfg(not(debug_assertions))]
    let clear_btn = iced::widget::space();

    let copy_btn = iced::widget::button(
      iced::widget::text("复制信息")
        .font(normal_font())
        .size(14)
        .center()
        .line_height(iced::widget::text::LineHeight::Absolute(20.into())),
    )
    .padding(0)
    .style(|_, _| iced::widget::button::Style {
      background: Some(iced::Color::from_rgba8(255, 255, 255, 0.0).into()),
      text_color: color!(0x1966ff),
      ..Default::default()
    })
    .on_press(Message::Copy);

    let content = iced::widget::container(
      iced::widget::column![
        iced::widget::container(
          svg::Svg::new("assets/svg/error.svg".to_resource_path())
            .width(240)
            .height(168)
        )
        .center_x(Length::Fill),
        iced::widget::container(
          iced::widget::row![
            iced::widget::text("出错了,")
              .font(normal_font())
              .size(14)
              .center()
              .line_height(iced::widget::text::LineHeight::Absolute(20.into()))
              .color(color!(0x85878a)),
            reload_btn,
            copy_btn,
            clear_btn,
          ]
          .spacing(10)
        )
        .center_x(Length::Fill),
        iced::widget::container(
          iced::widget::column![
            iced::widget::text(format!("版本号: {}", self.app_info.version))
              .font(normal_font())
              .size(14)
              .center()
              .width(Length::Fill)
              .line_height(iced::widget::text::LineHeight::Absolute(20.into()))
              .color(color!(0x85878a)),
            iced::widget::text(format!("设备ID: {}", self.app_info.did))
              .font(normal_font())
              .size(14)
              .center()
              .width(Length::Fill)
              .line_height(iced::widget::text::LineHeight::Absolute(20.into()))
              .color(color!(0x85878a)),
            iced::widget::text(format!("错误信息: {}", self.error_info.get_err_msg()))
              .font(normal_font())
              .size(14)
              .center()
              .width(Length::Fill)
              .line_height(iced::widget::text::LineHeight::Absolute(20.into()))
              .color(color!(0x85878a)),
          ]
          .spacing(10)
          .align_x(Horizontal::Center)
        )
        .center_x(Length::Fill),
      ]
      .spacing(10),
    )
    .center(Length::Fill)
    .style(|_| iced::widget::container::Style {
      background: Some(iced::color!(255, 255, 255).into()),
      ..Default::default()
    });
    content.into()
  }
}

impl Catalog for Theme {
  type Class<'a> = iced::widget::container::StyleFn<'a, Self>;

  fn default<'a>() -> Self::Class<'a> {
    Box::new(iced::widget::container::transparent)
  }

  fn style(&self, class: &Self::Class<'_>) -> iced::widget::container::Style {
    class(self)
  }
}
