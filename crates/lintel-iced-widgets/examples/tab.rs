use lintel_iced_widgets::{
  crash_view::{AppInfo, CrashInfo, CrashView},
  mouse_area::mouse_area,
  tab::{DragDirection, DragEvent, DropPosition, row::row},
  webview::BrowserId,
};
use lintel_lib_iced::{
  Alignment, Background, Color, IcedApplication, Subscription, Task, Theme,
  advanced::Clipboard,
  color, iced,
  widget::{button, canvas::path::lyon_path::geom::euclid::Length, container, stack, text},
};

#[derive(Debug, Clone)]
enum Message {
  Drag(DragEvent),
  Add,
  Delete(usize),
  None,
  BlurTab(String),
  Hover(String),
  Active(String),
}

fn main() {
  let res = lintel_lib_env::LintelEnv::init_for_test();
  iced::application::application(|| App::new(), App::update, App::view)
    .subscription(App::subscription)
    // .theme(App::theme)
    .run();
}

fn new_tab() -> (String, String) {
  (uuid::Uuid::new_v4().to_string(), "点我".into())
}
#[derive(Clone)]
pub struct App {
  items: Vec<(String, String)>,
  hover_index: Option<String>,
  active_index: Option<String>,
}

impl App {
  pub fn new() -> Self {
    Self {
      items: vec![new_tab(), new_tab(), new_tab()],
      hover_index: None,
      active_index: None,
    }
  }
}

pub fn custom_btn(_: &Theme, _status: button::Status) -> button::Style {
  button::Style {
    background: Some(Background::Color(color!(0, 0, 0, 1.0))),
    text_color: Color::WHITE,
    ..Default::default()
  }
}

impl App {
  pub fn view(&self) -> iced::Element<'_, Message> {
    let items: lintel_iced_widgets::tab::row::Row<'_, Message, Theme> =
      row(self.items.iter().enumerate().map(|(index, (tab_id, item))| {
        let is_active = self.active_index.as_ref().map(|i| i == tab_id).unwrap_or(false);
        let ele: iced::widget::Row<'_, Message> = if let Some(hover_index) = self.hover_index.as_ref() {
          if tab_id == hover_index {
            iced::widget::row![
              text!("{}", item),
              button("x").style(custom_btn).on_press(Message::Delete(index))
            ]
            .height(iced::Length::Fixed(50.0))
            .width(iced::Length::Fixed(200.0))
          } else {
            iced::widget::row![text!("{}", item)]
              .height(iced::Length::Fixed(50.0))
              .width(iced::Length::Fixed(200.0))
          }
        } else {
          iced::widget::row![text!("{}", item)]
            .height(iced::Length::Fixed(50.0))
            .width(iced::Length::Fixed(200.0))
        };

        let ele = container(ele).style(move |s| container::Style {
          background: Some(Background::Color(if is_active {
            Color::from_rgb8(0, 255, 0)
          } else {
            Color::from_rgb8(0, 0, 0)
          })),
          text_color: Some(if is_active {
            Color::from_rgb8(0, 0, 0)
          } else {
            Color::from_rgb8(255, 255, 255)
          }),
          ..Default::default()
        });

        container(
          mouse_area(ele, Some(tab_id.clone()))
            .on_enter(Message::Hover(tab_id.clone()))
            .on_exit(Message::BlurTab(tab_id.clone()))
            .on_press(Message::Active(tab_id.clone())),
        )
        .into()
      }))
      .spacing(0)
      .width(iced::Length::Fill)
      .height(iced::Length::Shrink)
      .align_y(iced::Alignment::Center)
      .drag_direction(DragDirection::Horizontal)
      .on_drag(Message::Drag);

    // stack![
    //   container(iced::widget::row![items, button("Add").on_press(Message::Add),])
    //     .align_x(Alignment::Center)
    //     .align_y(Alignment::Center)
    //     .width(iced::Length::Fill)
    //     .height(iced::Length::Fill)
    //     .center(iced::Length::Fill)
    // ]
    // .into()

    container(iced::widget::row![items, button("Add").on_press(Message::Add),])
      .align_x(Alignment::Center)
      .align_y(Alignment::Center)
      .width(iced::Length::Fill)
      .height(iced::Length::Fill)
      .center(iced::Length::Fill)
      .into()
  }

  pub fn update(&mut self, message: Message) -> iced::Task<Message> {
    match message {
      Message::Drag(drag_event) => match drag_event {
        DragEvent::Picked { index } => Task::done(Message::None),
        DragEvent::Dropped {
          index,
          target_index,
          drop_position,
        } => {
          println!(
            "Dropped item at index {} to target index {} at position {:?}",
            index, target_index, drop_position
          );
          match drop_position {
            DropPosition::Before | DropPosition::After => {
              if target_index != index && target_index != index + 1 {
                let item = self.items.remove(index);
                let insert_index = if index < target_index {
                  target_index - 1
                } else {
                  target_index
                };
                self.items.insert(insert_index, item);
              }
              return Task::none();
            }
            DropPosition::Swap => {
              if target_index != index {
                self.items.swap(index, target_index);
              }
              return Task::none();
            }
          }
        }
        DragEvent::Canceled { index } => Task::done(Message::None),
      },
      Message::Add => {
        self.items.push(new_tab());
        Task::none()
      }
      Message::Hover(index) => {
        self.hover_index = Some(index);
        Task::none()
      }
      Message::BlurTab(index) => {
        if let Some(hover_index) = self.hover_index.as_ref() {
          if hover_index == &index {
            self.hover_index = None;
          }
        }
        Task::none()
      }
      Message::Delete(index) => {
        let ret = self.items.remove(index);

        Task::done(Message::BlurTab(ret.0))
      }
      Message::Active(index) => {
        self.active_index = Some(index);
        Task::none()
      }
      Message::None => Task::none(),
    }
  }

  pub fn subscription(&self) -> Subscription<Message> {
    Subscription::none()
  }
}
