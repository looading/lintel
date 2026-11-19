use lintel_iced_widgets::{
  drag_area::drag_area,
  toolbar::{self, ToolbarAction},
  webview::BrowserId,
};
use lintel_lib_iced::{
  IcedApplication, Length, Subscription, Task,
  advanced::Clipboard,
  iced, iced_core, mouse,
  widget::{container, row, text},
  window::{self, Settings, settings::PlatformSpecific},
};
use lintel_lib_iced::{iced_winit::winit::raw_window_handle::RawWindowHandle, window::Id};
use objc2::{
  self, MainThreadMarker,
  ffi::{NO, id, nil},
};
use objc2_app_kit::{
  NSAccessibility, NSToolbar, NSToolbarDisplayMode, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
  NSWindowTitleVisibility,
};
use serde::de;

fn main() {
  let res = lintel_lib_env::LintelEnv::init_for_test();
  println!("{:?}", res);
  println!("{:?}", lintel_lib_env::LintelEnv::resources_path());
  iced::application::application(|| App::new(), App::update, App::view)
    .subscription(App::subscription)
    .window(Settings {
      // decorations: false,
      platform_specific: PlatformSpecific {
        title_hidden: true,
        titlebar_buttons_hidden: false,
        titlebar_transparent: true,
        fullsize_content_view: false,
        ..Default::default()
      },
      ..Default::default()
    })
    // .theme(App::theme)
    .run();
}

#[derive(Debug, Clone)]
enum AppMessage {
  Toolbar(ToolbarAction),
  FullScreen(bool),
  None,
  WindowOpened(Id),
  Drag,
}

#[derive(Clone)]
pub struct App {
  state: toolbar::ToolbarState,
  window: Option<Id>,
}

impl App {
  pub fn new() -> Self {
    Self {
      state: toolbar::ToolbarState::default(),
      window: None,
    }
  }
}

impl App {
  pub fn view(&self) -> iced::Element<'_, AppMessage> {
    drag_area(
      container(
        row![
          toolbar::toolbar(&self.state).map(|m| AppMessage::Toolbar(m)),
          text!("Hello World"),
        ]
        .spacing(5.0),
      )
      .padding(10.0)
      .center_y(50.0)
      .width(Length::Fill),
    )
    .on_drag(AppMessage::Drag)
    .into()
  }

  pub fn update(&mut self, message: AppMessage) -> iced::Task<AppMessage> {
    match message {
      AppMessage::Toolbar(action) => match action {
        toolbar::ToolbarAction::Close => iced::window::latest().and_then(|id| iced::window::close(id)),
        toolbar::ToolbarAction::Maximize => iced::window::latest().and_then(|id| iced::window::maximize(id, true)),
        toolbar::ToolbarAction::Minimize => iced::window::latest().and_then(|id| iced::window::minimize(id, true)),
        toolbar::ToolbarAction::FullScreen(full) => iced::window::latest().and_then(move |id| {
          return iced::window::set_mode(
            id,
            if full {
              iced_core::window::Mode::Fullscreen
            } else {
              iced_core::window::Mode::Windowed
            },
          )
          .chain(Task::done(AppMessage::FullScreen(full)));
        }),

        toolbar::ToolbarAction::Hover(yes) => {
          self.state.is_hovered = yes;
          Task::none()
        }
      },
      AppMessage::FullScreen(full) => {
        self.state.is_fullscreen = full;
        Task::none()
      }
      AppMessage::WindowOpened(id) => {
        self.window.insert(id);

        window::run(id, |window| {
          if let Ok(RawWindowHandle::AppKit(window)) = window.window_handle().map(|h| h.as_raw()) {
            let ns_view = window.ns_view.as_ptr();
            if let Some(ns_view) = unsafe { objc2::rc::Retained::retain(ns_view.cast::<objc2_app_kit::NSView>()) }
              && let Some(window) = ns_view.window()
            {
              unsafe {
                // window.setStyleMask(
                //   NSWindowStyleMask::Miniaturizable
                //     | NSWindowStyleMask::Closable
                //     | NSWindowStyleMask::Resizable
                //     | NSWindowStyleMask::FullScreen,
                // );

                // 实现 titlebar 不能移动窗口， 配合 iced 实现窗口移动,解决tab拖拽冲突问题
                // 阻止窗口背景移动窗口
                window.setMovableByWindowBackground(false);
                // 阻止窗口移动
                window.setMovable(false);
              };
            }
          };
          AppMessage::None
        })
      }
      AppMessage::Drag => {
        if let Some(id) = self.window.as_ref() {
          return window::drag(id.clone()).chain(Task::done(AppMessage::None));
        };
        Task::done(AppMessage::None)
      }
      AppMessage::None => Task::none(),
    }
  }

  pub fn subscription(&self) -> Subscription<AppMessage> {
    iced::event::listen_raw(|e, s, id| match e {
      iced::event::Event::Window(iced::window::Event::Opened { position, size }) => Some(AppMessage::WindowOpened(id)),
      _ => None,
    })
  }
}
