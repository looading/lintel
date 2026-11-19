//! Native control semantics and ordinary iced messages. No OS permission request is made.
#[cfg(all(feature = "desktop", feature = "public-iced"))]
fn main() -> lintel::Result<()> {
  use lintel::{
    a11y,
    iced::{
      Element, Task,
      widget::{column, container, mouse_area, text},
    },
  };
  #[derive(Default)]
  struct State {
    name: String,
    enabled: bool,
    count: u64,
    disable_after_click: bool,
  }
  #[derive(Clone, Debug)]
  enum Message {
    Name(String),
    Enabled(bool),
    Increment,
    Password(String),
  }
  let disable_after_click = std::env::args_os().any(|arg| arg == "--disable-after-click");
  lintel::application(
    move || State {
      enabled: true,
      disable_after_click,
      ..Default::default()
    },
    |state: &mut State, message| {
      match message {
        Message::Name(value) => {
          state.name = value;
          println!("LINTEL_A11Y_NAME_CHANGED");
        }
        Message::Enabled(value) => {
          state.enabled = value;
          println!("LINTEL_A11Y_CHECKBOX_CHANGED {value}");
        }
        Message::Increment => {
          state.count += 1;
          if state.disable_after_click {
            state.enabled = false;
          }
          println!("LINTEL_A11Y_CLICK {}", state.count);
        }
        Message::Password(_) => println!("LINTEL_A11Y_PASSWORD_ACTION"),
      }
      Task::none()
    },
    |state: &State| -> Element<'_, Message> {
      column![
        text("Lintel accessible controls"),
        a11y::text_input("name", "Name", &state.name, Message::Name),
        a11y::password_input("password", "Password", "private-example-secret", Message::Password),
        a11y::checkbox("enabled", "Enabled", state.enabled, Message::Enabled),
        if state.enabled {
          a11y::button("increment", "Increment", Message::Increment)
        } else {
          a11y::disabled_button("increment", "Increment")
        },
        a11y::disabled_button("unavailable", "Unavailable"),
        a11y::semantic(
          "custom-increment",
          "Custom increment",
          a11y::SemanticRole::Button,
          mouse_area(container(text("Custom control: add one")).padding(12)).on_press(Message::Increment),
        )
        .enabled(state.enabled)
        .on_action(|action| match action {
          a11y::SemanticAction::Click => Some(Message::Increment),
          _ => None,
        }),
        text(format!("Count: {}", state.count)),
      ]
      .spacing(16)
      .padding(32)
      .into()
    },
  )
  .id("dev.lintel.examples.accessibility")
  .title("Lintel accessibility controls")
  .accessibility()
  .run()
}

#[cfg(not(all(feature = "desktop", feature = "public-iced")))]
fn main() -> lintel::Result<()> {
  anyhow::bail!("enable public-iced and desktop")
}
