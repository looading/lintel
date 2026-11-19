use lintel_iced_widgets::popover::{
  self, ButtonAlignment, PopoverCardConfig, PopoverCardStyle, PopoverConfig, PopoverStyle,
};
use lintel_lib_iced::iced;

use iced::widget::{button, center, column, container, row, slider, stack, text};
use iced::{Alignment, Color, Element, Fill, Length, Task, Theme, color};

pub fn main() -> iced::Result {
  iced::application(App::default, App::update, App::view).run()
}

#[derive(Debug, Clone)]
enum Message {
  Show,
  Hide,
  Confirm,
  XChanged(f32),
  YChanged(f32),
  WidthChanged(f32),
  HeightChanged(f32),
}

struct App {
  visible: bool,
  last_action: String,
  x: f32,
  y: f32,
  width: f32,
  height: f32,
}

impl Default for App {
  fn default() -> Self {
    Self {
      visible: false,
      last_action: "点击按钮创建 Popover".to_string(),
      x: 120.0,
      y: 120.0,
      width: 320.0,
      height: 176.0,
    }
  }
}

impl App {
  fn update(&mut self, message: Message) -> Task<Message> {
    match message {
      Message::Show => {
        self.visible = true;
        self.last_action = "Popover 已创建".to_string();
      }
      Message::Hide => {
        self.visible = false;
        self.last_action = "鼠标离开 Popover，已关闭".to_string();
      }
      Message::Confirm => {
        self.visible = false;
        self.last_action = "点击了确认按钮".to_string();
      }
      Message::XChanged(value) => self.x = value,
      Message::YChanged(value) => self.y = value,
      Message::WidthChanged(value) => self.width = value,
      Message::HeightChanged(value) => self.height = value,
    }
    Task::none()
  }

  fn view(&self) -> Element<'_, Message> {
    let panel = container(
      column![
        text("lintel-iced-widgets Popover").size(28),
        text("这个例子演示一个通用 Popover：支持即时创建、绝对定位、鼠标离开关闭和确认回调。")
          .size(14)
          .color(color!(0x4f566b)),
        row![
          text(format!("x: {:.0}", self.x)).width(80),
          slider(0.0..=600.0, self.x, Message::XChanged).step(1.0),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
        row![
          text(format!("y: {:.0}", self.y)).width(80),
          slider(0.0..=400.0, self.y, Message::YChanged).step(1.0),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
        row![
          text(format!("width: {:.0}", self.width)).width(80),
          slider(220.0..=420.0, self.width, Message::WidthChanged).step(1.0),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
        row![
          text(format!("height: {:.0}", self.height)).width(80),
          slider(120.0..=260.0, self.height, Message::HeightChanged).step(1.0),
        ]
        .spacing(12)
        .align_y(Alignment::Center),
        row![
          button("创建 Popover").on_press(Message::Show),
          text(&self.last_action).size(14).color(Color::from_rgb(0.35, 0.4, 0.5)),
        ]
        .spacing(16)
        .align_y(Alignment::Center),
        container(
          text("触发区域示意")
            .size(16)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center),
        )
        .width(180)
        .height(56)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(|_: &Theme| container::Style {
          background: Some(Color::from_rgb(0.92, 0.95, 1.0).into()),
          border: iced::Border {
            radius: 8.0.into(),
            width: 1.0,
            color: Color::from_rgb(0.6, 0.72, 0.98),
          },
          ..Default::default()
        }),
      ]
      .spacing(18)
      .max_width(760),
    )
    .padding(24)
    .center_x(Fill)
    .center_y(Fill);

    let base = center(panel).width(Fill).height(Fill);

    if !self.visible {
      return base.into();
    }

    let config = PopoverConfig {
      key: "popover-example".to_string(),
      x: self.x,
      y: self.y,
      width: self.width,
      height: self.height,
      close_on_mouse_leave: true,
      style: PopoverStyle {
        background_color: Color::WHITE,
        ..PopoverStyle::default()
      },
      ..PopoverConfig::default()
    };

    let card = PopoverCardConfig {
      title: "通用 Popover".to_string(),
      content: "这是一个与业务解耦的通用组件。你可以在任意场景创建它，并在确认或移出时触发回调。".to_string(),
      button_text: "确认".to_string(),
      button_alignment: ButtonAlignment::BottomRight,
      style: PopoverCardStyle {
        button_color: color!(0x1966ff),
        ..PopoverCardStyle::default()
      },
    };

    stack![
      base,
      popover::popover_card(&config, &card, Message::Confirm, Some(Message::Hide))
    ]
    .width(Fill)
    .height(Fill)
    .into()
  }
}
