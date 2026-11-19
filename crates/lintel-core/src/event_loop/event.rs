use global_hotkey::hotkey::HotKey;
use std::time::Instant;

pub enum LintelEvent {
  AppEvent(AppEvent),
  #[cfg(any(target_os = "macos", target_os = "windows"))]
  MenuEvent(muda::MenuEvent),
  #[cfg(any(target_os = "macos", target_os = "windows"))]
  TrayIconEvent(tray_icon::TrayIconEvent),
  TickCef(Instant),
  IcedApp(lintel_lib_iced::IcedAppEvent),
  Task(Box<dyn FnOnce() + Send>),
  HotkeyManager(HotkeyManagerEvent),
}

pub enum HotkeyManagerEvent {
  Register(HotKey),
  Unregister(HotKey),
}

impl std::fmt::Debug for LintelEvent {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::AppEvent(..) => write!(f, "AppEvent"),
      #[cfg(any(target_os = "macos", target_os = "windows"))]
      Self::MenuEvent(..) => write!(f, "MenuEvent"),
      #[cfg(any(target_os = "macos", target_os = "windows"))]
      Self::TrayIconEvent(..) => write!(f, "TrayIconEvent"),
      Self::TickCef(..) => write!(f, "TickCef"),
      Self::IcedApp(iced) => f.debug_tuple("IceApp").field(iced).finish(),
      Self::Task(..) => write!(f, "Task"),
      Self::HotkeyManager(_) => write!(f, "HotkeyManager"),
    }
  }
}

#[derive(Clone, Debug)]
pub enum AppEvent {
  Quit(QuitState),
  Exit(i32, QuitState),
  Relaunch(Option<Vec<String>>, QuitState),
  Opened { urls: Vec<String> },
}

// 退出状态
#[derive(Clone, Debug)]
pub enum QuitState {
  Init,    // 初始状态
  Waiting, // 等待退出状态
  Done,    // 退出完成状态
}
