use lintel_iced_widgets::toast::{self, GlobalToast};
use lintel_lib_iced::iced;

use iced::event::{self, Event};
use iced::keyboard;
use iced::keyboard::key;
use iced::widget::{button, center, column, operation, pick_list, row, slider, text, text_input};
use iced::{Center, Element, Fill, Subscription, Task};

use toast::{Status, Toast};

pub fn main() -> iced::Result {
  iced::application(App::default, App::update, App::view)
    .subscription(App::subscription)
    .run()
}

struct App {
  editing: Toast,
  timeout_secs: u64,
  max_toasts: u64,
  global_toast: GlobalToast,
}

#[derive(Debug, Clone)]
#[allow(clippy::enum_variant_names)]
enum Message {
  Add,
  Close(toast::ToastId),
  Title(String),
  Body(String),
  Status(Status),
  Timeout(f64),
  MaxToasts(f64),
  Event(Event),
  None,
}

impl App {
  fn new() -> Self {
    App {
      timeout_secs: toast::DEFAULT_TIMEOUT,
      max_toasts: toast::DEFAULT_MAX_TOASTS as u64,
      editing: Toast::default(),
      global_toast: GlobalToast::new(),
    }
  }

  fn subscription(&self) -> Subscription<Message> {
    event::listen().map(Message::Event)
  }

  fn update(&mut self, message: Message) -> Task<Message> {
    match message {
      Message::Add => {
        if !self.editing.body.is_empty() {
          let data = std::mem::take(&mut self.editing);

          return Task::done(self.global_toast.update(toast::ToastMessage::Show {
            body: data.body,
            status: data.status,
          }))
          .map(|_| Message::None);
        }
        Task::none()
      }
      Message::Close(id) => Task::done(self.global_toast.update(toast::ToastMessage::Close(id))).map(|_| Message::None),
      Message::Title(title) => Task::none(),
      Message::Body(body) => {
        self.editing.body = body;
        Task::none()
      }
      Message::Status(status) => {
        self.editing.status = status;
        Task::none()
      }
      Message::Timeout(timeout) => {
        self.timeout_secs = timeout as u64;
        Task::none()
      }
      Message::MaxToasts(max_toasts) => {
        self.max_toasts = max_toasts as u64;
        self.global_toast.set_max_toasts(self.max_toasts as usize);
        Task::none()
      }
      Message::Event(Event::Keyboard(keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(key::Named::Tab),
        modifiers,
        ..
      }))
        if modifiers.shift() =>
      {
        operation::focus_previous()
      }
      Message::Event(Event::Keyboard(keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(key::Named::Tab),
        ..
      })) => operation::focus_next(),
      Message::Event(_) => Task::none(),
      Message::None => Task::none(),
    }
  }

  fn view(&self) -> Element<'_, Message> {
    let subtitle = |title, content: Element<'static, Message>| column![text(title).size(14), content].spacing(5);

    let add_toast = button("Add Toast").on_press_maybe((!self.editing.body.is_empty()).then_some(Message::Add));

    let content = center(
      column![
        subtitle(
          "Message",
          text_input("", &self.editing.body)
            .on_input(Message::Body)
            .on_submit(Message::Add)
            .into()
        ),
        subtitle(
          "Status",
          pick_list(toast::Status::ALL, Some(self.editing.status), Message::Status)
            .width(Fill)
            .into()
        ),
        subtitle(
          "Timeout",
          row![
            text!("{:0>2} sec", self.timeout_secs),
            slider(1.0..=30.0, self.timeout_secs as f64, Message::Timeout).step(1.0)
          ]
          .spacing(5)
          .into()
        ),
        subtitle(
          "Max Toasts",
          row![
            text!("{:0>2}", self.max_toasts),
            slider(1.0..=30.0, self.max_toasts as f64, Message::MaxToasts).step(1.0)
          ]
          .spacing(5)
          .into()
        ),
        column![add_toast].align_x(Center)
      ]
      .spacing(10)
      .max_width(200),
    );

    self
      .global_toast
      .view(content, Message::Close, self.timeout_secs)
      .into()
  }
}

impl Default for App {
  fn default() -> Self {
    Self::new()
  }
}
