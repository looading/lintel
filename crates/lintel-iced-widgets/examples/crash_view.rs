use lintel_iced_widgets::{
  crash_view::{AppInfo, CrashInfo, CrashView, Message},
  webview::BrowserId,
};
use lintel_lib_iced::{IcedApplication, Subscription, advanced::Clipboard, iced};

fn main() {
  let res = lintel_lib_env::LintelEnv::init_for_test();
  iced::application::application(
    || {
      App::new(
        lintel_lib_env::LintelEnv::version(),
        lintel_lib_env::LintelEnv::device_id(),
      )
    },
    App::update,
    App::view,
  )
  .subscription(App::subscription)
  // .theme(App::theme)
  .run();
}

#[derive(Clone)]
pub struct App {
  app_info: AppInfo,
  error_info: Option<CrashInfo>,
}

impl App {
  pub fn new(version: &'static semver::Version, did: u64) -> Self {
    Self {
      app_info: AppInfo { version, did },
      error_info: Some(CrashInfo::Crash {
        err_msg: "The example page could not be loaded. Reload to try again.".into(),
      }),
    }
  }
}

impl App {
  pub fn view(&self) -> iced::Element<'_, Message> {
    if let Some(error_info) = &self.error_info {
      CrashView::new(&self.app_info, error_info).view().into()
    } else {
      iced::widget::text!("没有错误信息").into()
    }
  }

  pub fn update(&mut self, message: Message) -> iced::Task<Message> {
    match message {
      Message::Reload => iced::Task::none(),
      Message::Copy => {
        // 处理复制错误信息逻辑
        let err_text = format!(
          "设备ID: {} \n版本号: {} \n错误信息: {}",
          self.app_info.did,
          self.app_info.version,
          self
            .error_info
            .as_ref()
            .map(|info| info.get_err_msg())
            .unwrap_or_default()
        );
        iced::clipboard::write(err_text)
      }
      Message::Clear => {
        self.error_info = None;
        iced::Task::none()
      }
    }
  }

  pub fn subscription(&self) -> Subscription<Message> {
    Subscription::none()
  }
}
