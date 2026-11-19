//! Window-local client-rendered menus, also usable on Linux without GTK/global shell menus.
#[cfg(all(feature = "desktop", feature = "public-iced"))]
fn main() -> lintel::Result<()> {
  use lintel::{
    a11y,
    desktop::{MenuEntry, MenuSpec},
    iced::{
      Element, Task,
      widget::{column, text},
    },
    menu::{self, MenuAction, MenuModel},
  };
  struct State {
    menu: MenuModel,
    name: String,
    checked: bool,
    enabled: bool,
    count: u64,
    background: u64,
    last: String,
  }
  #[derive(Debug, Clone)]
  enum Message {
    Menu(MenuAction),
    Name(String),
    Background,
  }
  fn trace(
    event: lintel::iced::Event,
    status: lintel::iced::event::Status,
    window: lintel::iced::window::Id,
  ) -> Option<Message> {
    use lintel::iced::{
      Event,
      keyboard::{Event as Keyboard, Key},
    };
    let key = |key: Key| match key {
      Key::Named(named) => format!("Named::{named:?}"),
      Key::Character(_) => "Character".into(),
      Key::Unidentified => "Unidentified".into(),
    };
    let class = match event {
      Event::Keyboard(Keyboard::KeyPressed {
        key: code,
        modifiers,
        repeat,
        ..
      }) => Some(format!("pressed key={} mods={modifiers:?} repeat={repeat}", key(code))),
      Event::Keyboard(Keyboard::KeyReleased {
        key: code, modifiers, ..
      }) => Some(format!("released key={} mods={modifiers:?}", key(code))),
      Event::Keyboard(Keyboard::ModifiersChanged(modifiers)) => Some(format!("modifiers={modifiers:?}")),
      Event::InputMethod(event) => Some(format!(
        "ime={}",
        match event {
          lintel::iced::advanced::input_method::Event::Opened => "Opened",
          lintel::iced::advanced::input_method::Event::Closed => "Closed",
          lintel::iced::advanced::input_method::Event::Preedit(..) => "Preedit",
          lintel::iced::advanced::input_method::Event::Commit(_) => "Commit",
        }
      )),
      Event::Window(lintel::iced::window::Event::Focused) => Some("focused=true".into()),
      Event::Window(lintel::iced::window::Event::Unfocused) => Some("focused=false".into()),
      _ => None,
    };
    if let Some(class) = class {
      let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_millis())
        .unwrap_or(0);
      eprintln!(
        "LINTEL_CLIENT_MENU_TRACE pid={} ms={millis} stage=subscription window={window:?} {class} status={status:?}",
        std::process::id()
      );
    }
    None // Diagnostics must not enqueue a business message or trigger another view rebuild.
  }
  let trace_input = std::env::args_os().any(|arg| arg == "--trace-input");
  if trace_input {
    eprintln!("LINTEL_CLIENT_MENU_TRACE pid={} enabled=true", std::process::id());
  }
  fn spec() -> MenuSpec {
    MenuSpec {
      items: vec![
        MenuEntry::Submenu {
          label: "File".into(),
          enabled: true,
          items: vec![
            MenuEntry::Item {
              id: "increment".into(),
              label: "Increment".into(),
              enabled: true,
              accelerator: Some("Ctrl+I".into()),
            },
            MenuEntry::Check {
              id: "checked".into(),
              label: "Checked action".into(),
              enabled: true,
              checked: false,
            },
            MenuEntry::Item {
              id: "disabled".into(),
              label: "Initially disabled".into(),
              enabled: false,
              accelerator: None,
            },
            MenuEntry::item("enable", "Toggle disabled action"),
            MenuEntry::Separator,
            MenuEntry::Submenu {
              label: "More".into(),
              enabled: true,
              items: vec![
                MenuEntry::item("nested", "Nested action"),
                MenuEntry::Submenu {
                  label: "Deeper".into(),
                  enabled: true,
                  items: vec![MenuEntry::item("deep", "Deep action")],
                },
              ],
            },
            MenuEntry::item("quit", "Quit"),
          ],
        },
        MenuEntry::Submenu {
          label: "Unavailable".into(),
          enabled: false,
          items: vec![MenuEntry::item("blocked", "Must not fire")],
        },
        MenuEntry::Submenu {
          label: "Long menu".into(),
          enabled: true,
          items: (0..45)
            .map(|index| MenuEntry::item(format!("long-{index}"), format!("Action {index}")))
            .collect(),
        },
        MenuEntry::item("help", "Help"),
      ],
    }
  }
  lintel::application(
    || State {
      menu: MenuModel::new(spec()).expect("static valid menu"),
      name: String::new(),
      checked: false,
      enabled: false,
      count: 0,
      background: 0,
      last: "No selection".into(),
    },
    |state: &mut State, message| {
      match message {
        Message::Menu(action) => {
          println!(
            "LINTEL_CLIENT_MENU_ACTION id={} checked={:?}",
            action.id, action.checked
          );
          match action.id.as_str() {
            "quit" => return lintel::iced::exit(),
            "checked" => {
              state.checked = action.checked.unwrap_or(false);
              state
                .menu
                .set_checked("checked", state.checked)
                .expect("known check item");
            }
            "enable" => {
              state.enabled = !state.enabled;
              state.menu.set_enabled("disabled", state.enabled).expect("known item");
            }
            _ => state.count += 1,
          }
          state.last = action.id;
        }
        Message::Name(value) => {
          state.name = value;
          println!("LINTEL_CLIENT_MENU_INPUT_CHANGED len={}", state.name.len());
        }
        Message::Background => {
          state.background += 1;
          println!("LINTEL_CLIENT_MENU_BACKGROUND {}", state.background);
        }
      }
      Task::none()
    },
    |state: &State| -> Element<'_, Message> {
      menu::menu_bar(
        &state.menu,
        Message::Menu,
        column![
          text("Client-rendered menubar; no GTK or desktop-global registration"),
          text("F10 opens; arrows/Home/End navigate; Enter/Space selects; Escape/Tab closes."),
          a11y::text_input("input", "Retained input focus", &state.name, Message::Name),
          a11y::button("background", "Background action", Message::Background),
          text(format!(
            "Menu actions: {}, background: {}, last: {}",
            state.count, state.background, state.last
          )),
          text("Ctrl+I is shown as metadata; this demo does not register that accelerator."),
        ]
        .padding(24)
        .spacing(16),
      )
      .into()
    },
  )
  .id("dev.lintel.examples.client-menubar")
  .title("Lintel client-rendered menubar")
  .accessibility()
  .subscription(move |_: &State| {
    if trace_input {
      lintel::iced::event::listen_with(trace)
    } else {
      lintel::iced::Subscription::none()
    }
  })
  .run()
}
#[cfg(not(all(feature = "desktop", feature = "public-iced")))]
fn main() -> lintel::Result<()> {
  anyhow::bail!("enable public-iced and desktop")
}
