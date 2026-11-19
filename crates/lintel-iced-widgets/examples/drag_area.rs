use lintel_iced_widgets::drag_area::drag_area;
use lintel_lib_iced::iced;

use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Color, Element, Fill, Task, Theme, color, window};

pub fn main() -> iced::Result {
  iced::application(App::default, App::update, App::view)
    .subscription(App::subscription)
    .window(window::Settings {
      decorations: false,
      size: (760.0, 460.0).into(),
      ..Default::default()
    })
    .run()
}

#[derive(Debug, Clone)]
enum Message {
  WindowOpened(window::Id),
  Drag,
  ToggleMaximize,
  Minimize,
  Close,
}

#[derive(Default)]
struct App {
  window: Option<window::Id>,
  is_maximized: bool,
}

impl App {
  fn update(&mut self, message: Message) -> Task<Message> {
    match message {
      Message::WindowOpened(id) => {
        self.window = Some(id);
        Task::none()
      }
      Message::Drag => self.window.map(window::drag).unwrap_or_else(Task::none),
      Message::ToggleMaximize => {
        let Some(window) = self.window else {
          return Task::none();
        };

        self.is_maximized = !self.is_maximized;
        window::maximize(window, self.is_maximized)
      }
      Message::Minimize => self
        .window
        .map(|window| window::minimize(window, true))
        .unwrap_or_else(Task::none),
      Message::Close => self.window.map(window::close).unwrap_or_else(Task::none),
    }
  }

  fn view(&self) -> Element<'_, Message> {
    let titlebar = drag_area(
      container(
        row![
          text("DragArea Example")
            .size(14)
            .color(Color::from_rgb8(0x1d, 0x21, 0x29)),
          row![
            button(text("−").size(16))
              .on_press(Message::Minimize)
              .width(32)
              .height(28),
            button(text(if self.is_maximized { "▢" } else { "□" }).size(14))
              .on_press(Message::ToggleMaximize)
              .width(32)
              .height(28),
            button(text("×").size(16)).on_press(Message::Close).width(32).height(28),
          ]
          .spacing(6)
        ]
        .align_y(Alignment::Center)
        .spacing(12),
      )
      .padding([8, 12])
      .width(Fill)
      .style(titlebar_style),
    )
    .on_drag(Message::Drag)
    .on_double_click(Message::ToggleMaximize);

    let content = container(
      column![
        text("DragArea").size(32).color(color!(0x1d2129)),
        text("拖拽顶部区域移动窗口，双击顶部区域切换最大化/还原。")
          .size(15)
          .color(color!(0x4e5969)),
        text("右侧按钮保留普通按钮行为，不会被 DragArea 抢占。")
          .size(13)
          .color(color!(0x86909c)),
      ]
      .spacing(12)
      .align_x(Alignment::Center),
    )
    .width(Fill)
    .height(Fill)
    .center_x(Fill)
    .center_y(Fill)
    .style(content_style);

    container(column![titlebar, content])
      .width(Fill)
      .height(Fill)
      .style(window_style)
      .into()
  }

  fn subscription(&self) -> iced::Subscription<Message> {
    iced::event::listen_raw(|event, _status, window| match event {
      iced::event::Event::Window(window::Event::Opened { .. }) => Some(Message::WindowOpened(window)),
      _ => None,
    })
  }
}

fn window_style(_theme: &Theme) -> container::Style {
  container::Style {
    background: Some(Color::WHITE.into()),
    border: iced::Border {
      width: 1.0,
      radius: 8.0.into(),
      color: Color::from_rgb8(0xe5, 0xe6, 0xeb),
    },
    ..Default::default()
  }
}

fn titlebar_style(_theme: &Theme) -> container::Style {
  container::Style {
    background: Some(Color::from_rgb8(0xf7, 0xf8, 0xfa).into()),
    border: iced::Border {
      width: 0.0,
      radius: 0.0.into(),
      color: Color::TRANSPARENT,
    },
    ..Default::default()
  }
}

fn content_style(_theme: &Theme) -> container::Style {
  container::Style {
    background: Some(Color::from_rgb8(0xff, 0xff, 0xff).into()),
    ..Default::default()
  }
}
