use lintel_iced_widgets::widget_sensor::WidgetSensor;
use lintel_lib_iced::iced::{
  self, Alignment, Color, Element, Length, Renderer, Subscription, Task, Theme, font,
  widget::{column, container, row, scrollable, text},
  window,
};

pub fn main() -> iced::Result {
  iced::application(App::new, App::update, App::view).run()
}

struct App {
  logs: Vec<String>,
  sensor_key: String,
  is_sensor_visible: bool,
  show_scroll_test: bool,
  counter: usize,
}

#[derive(Debug)]
enum Message {
  ClearLogs,
  ToggleVisibility,
  ToggleScrollTest,
  ResetKey,
  SensorEvent(String),
  Tick,
}

impl App {
  fn new() -> (Self, Task<Message>) {
    (
      Self {
        logs: Vec::new(),
        sensor_key: "initial_key".to_string(),
        is_sensor_visible: true,
        show_scroll_test: false,
        counter: 0,
      },
      Task::none(),
    )
  }

  fn update(&mut self, message: Message) -> Task<Message> {
    match message {
      Message::ClearLogs => {
        self.logs.clear();
        self.counter = 0;
      }
      Message::ToggleVisibility => {
        self.is_sensor_visible = !self.is_sensor_visible;
        self.log(format!("Visibility toggled: {}", self.is_sensor_visible));
      }
      Message::ToggleScrollTest => {
        self.show_scroll_test = !self.show_scroll_test;
        self.log(format!("Scroll Test toggled: {}", self.show_scroll_test));
      }
      Message::ResetKey => {
        use std::time::{SystemTime, UNIX_EPOCH};
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
        self.sensor_key = format!("key_{}", timestamp);
        self.log(format!("Key reset to: {}", self.sensor_key));
      }
      Message::SensorEvent(msg) => {
        self.log(msg);
      }
      Message::Tick => {}
    }
    Task::none()
  }

  fn log(&mut self, msg: String) {
    self.counter += 1;
    self.logs.push(format!("#{}: {}", self.counter, msg));
    if self.logs.len() > 20 {
      self.logs.remove(0);
    }
  }

  fn view(&self) -> Element<'_, Message> {
    let content: Element<'_, Message> = if self.show_scroll_test {
      let create_sensor = |label: &'static str, suffix: &'static str| -> Element<'_, Message> {
        WidgetSensor::<Message, Theme, Renderer>::new(
          container(
            text(label)
              .size(20)
              .align_y(Alignment::Center)
              .align_x(Alignment::Center),
          )
          .width(Length::Fill)
          .height(Length::Fixed(150.0))
          .style(|_: &Theme| container::Style {
            background: Some(Color::from_rgb(0.8, 0.9, 1.0).into()),
            border: iced::Border {
              radius: 10.0.into(),
              width: 2.0,
              color: Color::from_rgb(0.4, 0.6, 0.9),
            },
            ..Default::default()
          })
          .center_x(Length::Fill)
          .center_y(Length::Fill),
        )
        .key(format!("{}_{}", self.sensor_key, suffix))
        .on_appear(Message::SensorEvent(format!("{}: Appear", label)))
        .on_disappear(Message::SensorEvent(format!("{}: Disappear", label)))
        .on_first_appear(Message::SensorEvent(format!("{}: First Appear", label)))
        .on_click(Message::SensorEvent(format!("{}: Click", label)))
        .into()
      };

      scrollable(
        column![
          text("Scroll down to find sensors!").size(24),
          container(text("Spacer (400px)"))
            .height(Length::Fixed(400.0))
            .style(|_: &Theme| container::Style {
              background: Some(Color::from_rgb(0.9, 0.9, 0.9).into()),
              ..Default::default()
            })
            .center_x(Length::Fill)
            .center_y(Length::Fill),
          create_sensor("Sensor A", "A"),
          container(text("Spacer (600px)"))
            .height(Length::Fixed(600.0))
            .style(|_: &Theme| container::Style {
              background: Some(Color::from_rgb(0.9, 0.9, 0.9).into()),
              ..Default::default()
            })
            .center_x(Length::Fill)
            .center_y(Length::Fill),
          create_sensor("Sensor B", "B"),
          container(text("Spacer (600px)"))
            .height(Length::Fixed(600.0))
            .style(|_: &Theme| container::Style {
              background: Some(Color::from_rgb(0.9, 0.9, 0.9).into()),
              ..Default::default()
            })
            .center_x(Length::Fill)
            .center_y(Length::Fill),
          create_sensor("Sensor C", "C"),
          container(text("Spacer (200px)"))
            .height(Length::Fixed(200.0))
            .style(|_: &Theme| container::Style {
              background: Some(Color::from_rgb(0.9, 0.9, 0.9).into()),
              ..Default::default()
            })
            .center_x(Length::Fill)
            .center_y(Length::Fill),
        ]
        .spacing(20)
        .padding(20),
      )
      .height(Length::Fill)
      .on_scroll(|v| Message::Tick)
      .into()
    } else if self.is_sensor_visible {
      WidgetSensor::<Message, Theme, Renderer>::new(
        container(
          text("Interact with me!\n(Click, Hover, Scroll, Resize Window)")
            .size(20)
            .align_y(Alignment::Center)
            .align_x(Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fixed(200.0))
        .style(|_: &Theme| container::Style {
          background: Some(Color::from_rgb(0.8, 0.9, 1.0).into()),
          border: iced::Border {
            radius: 10.0.into(),
            width: 2.0,
            color: Color::from_rgb(0.4, 0.6, 0.9),
          },
          ..Default::default()
        })
        .center_x(Length::Fill)
        .center_y(Length::Fill),
      )
      .key(&self.sensor_key)
      .on_appear(Message::SensorEvent("Event: Appear".to_string()))
      .on_disappear(Message::SensorEvent("Event: Disappear".to_string()))
      .on_first_appear(Message::SensorEvent("Event: First Appear".to_string()))
      .on_enter(Message::SensorEvent("Event: Mouse Enter".to_string()))
      .on_exit(Message::SensorEvent("Event: Mouse Exit".to_string()))
      .on_click(Message::SensorEvent("Event: Click (Left)".to_string()))
      .on_press(Message::SensorEvent("Event: Press (Left)".to_string()))
      .on_release(Message::SensorEvent("Event: Release (Left)".to_string()))
      .on_right_press(Message::SensorEvent("Event: Press (Right)".to_string()))
      .on_right_release(Message::SensorEvent("Event: Release (Right)".to_string()))
      .on_double_click(Message::SensorEvent("Event: Double Click".to_string()))
      .on_click_outside(Message::SensorEvent("Event: Click Outside".to_string()))
      .into()
    } else {
      container(text("Sensor Hidden").size(20).color(Color::from_rgb(0.5, 0.5, 0.5)))
        .width(Length::Fill)
        .height(Length::Fixed(200.0))
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
    };

    let simple_button = |label: &'static str, msg: Message| -> Element<'_, Message> {
      WidgetSensor::new(container(text(label)).padding(10).style(|_: &Theme| container::Style {
        background: Some(Color::from_rgb(0.9, 0.9, 0.9).into()),
        border: iced::Border {
          radius: 5.0.into(),
          width: 1.0,
          color: Color::from_rgb(0.7, 0.7, 0.7),
        },
        ..Default::default()
      }))
      .on_click(msg)
      .into()
    };

    let controls = row![
      simple_button(
        if self.show_scroll_test {
          "Back to Single"
        } else {
          "Scroll Test"
        },
        Message::ToggleScrollTest
      ),
      simple_button("Toggle Visibility", Message::ToggleVisibility),
      simple_button("Reset Key", Message::ResetKey),
      simple_button("Clear Logs", Message::ClearLogs),
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let logs = scrollable(
      column(
        self
          .logs
          .iter()
          .rev()
          .map(|log| text(log).size(14).font(font::Font::MONOSPACE).into())
          .collect::<Vec<_>>(),
      )
      .spacing(5),
    )
    .height(Length::Fill)
    .width(Length::Fill);

    column![
      text("Widget Sensor Example").size(30).font(font::Font {
        family: font::Family::Name("Helvetica".into()),
        weight: font::Weight::Bold,
        ..font::Font::DEFAULT
      }),
      content,
      controls,
      container(logs).padding(10).style(|_: &Theme| container::Style {
        background: Some(Color::from_rgb(0.95, 0.95, 0.95).into()),
        border: iced::Border {
          radius: 5.0.into(),
          width: 1.0,
          color: Color::from_rgb(0.8, 0.8, 0.8),
        },
        ..Default::default()
      })
    ]
    .spacing(20)
    .padding(20)
    .into()
  }
}
