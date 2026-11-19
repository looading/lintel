use std::collections::HashMap;

use lintel_iced_widgets::window_debug_info::{WindowDebugInfo, WindowDebugInfoMessage};
use lintel_lib_iced::iced::{
  self, Alignment, Element, Length, Subscription, Task, Theme,
  widget::{button, column, container, text},
};

pub fn main() -> iced::Result {
  iced::application(App::new, App::update, App::view)
    .subscription(App::subscription)
    .run()
}

struct App {
  window_debug_info: WindowDebugInfo,
  count: usize,
}

#[derive(Debug, Clone)]
enum Message {
  Increase,
  WindowOpened(iced::window::Id),
  WindowDebugInfo(WindowDebugInfoMessage),
}

impl App {
  fn new() -> (Self, Task<Message>) {
    let mut exera_info = HashMap::new();
    exera_info.insert("env".to_string(), "dev".to_string());
    exera_info.insert("route".to_string(), "/example/window_debug_info".to_string());
    exera_info.insert("session".to_string(), "local-preview".to_string());

    (
      Self {
        window_debug_info: WindowDebugInfo::default().with_exera_info(exera_info),
        count: 0,
      },
      Task::none(),
    )
  }

  fn update(&mut self, message: Message) -> Task<Message> {
    match message {
      Message::Increase => {
        self.count += 1;
        Task::none()
      }
      Message::WindowOpened(window_id) => self
        .window_debug_info
        .set_window_id(Some(window_id))
        .map(Message::WindowDebugInfo),
      Message::WindowDebugInfo(message) => self.window_debug_info.update(message).map(Message::WindowDebugInfo),
    }
  }

  fn subscription(&self) -> Subscription<Message> {
    Subscription::batch([
      self.window_debug_info.subscription().map(Message::WindowDebugInfo),
      iced::event::listen_raw(|event, _status, window_id| match event {
        iced::event::Event::Window(iced::window::Event::Opened { .. }) => Some(Message::WindowOpened(window_id)),
        _ => None,
      }),
    ])
  }

  fn view(&self) -> Element<'_, Message> {
    let content = container(
      column![
        text("WindowDebugInfo Example").size(32),
        text("组件会在右上角展示当前窗口状态，并支持拖动位置、折叠展开；window_id 由外部显式绑定，ui_fps 由面板内部开关控制。")
          .size(16),
        button(text(format!("Count: {}", self.count)))
          .padding([8, 14])
          .on_press(Message::Increase),
      ]
      .spacing(16)
      .align_x(Alignment::Center),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .center_x(Length::Fill)
    .center_y(Length::Fill)
    .style(|_: &Theme| container::Style {
      background: Some(iced::Background::Color(iced::Color::from_rgb8(0xF5, 0xF7, 0xFB))),
      ..Default::default()
    });

    self.window_debug_info.overlay(content, Message::WindowDebugInfo)
  }
}
