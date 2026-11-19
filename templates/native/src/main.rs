use lintel::iced::{
  Element, Task,
  widget::{button, center, column, text},
};

fn main() -> lintel::Result<()> {
  lintel::application(u64::default, update, view)
    .id(concat!("dev.example.", env!("CARGO_PKG_NAME")))
    .title("Counter")
    .run()
}

#[derive(Debug, Clone)]
enum Message {
  Increment,
}

fn update(count: &mut u64, message: Message) -> Task<Message> {
  match message {
    Message::Increment => *count += 1,
  }
  Task::none()
}

fn view(count: &u64) -> Element<'_, Message> {
  center(
    column![
      text(format!("Count: {count}")),
      button("Increment").on_press(Message::Increment),
    ]
    .spacing(12),
  )
  .into()
}
