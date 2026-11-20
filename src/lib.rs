pub mod activation;
#[cfg(feature = "agent")]
pub mod agent;
mod application;
#[cfg(feature = "public-cef")]
pub mod cef;
mod multi_window;
#[cfg(feature = "multiprocess")]
pub mod process;
pub mod runtime;
#[cfg(feature = "services")]
pub mod services;
#[cfg(feature = "public-cef")]
mod webview_app;
#[cfg(feature = "public-cef")]
mod webview_bridge;
pub mod window;
#[cfg(all(feature = "desktop", feature = "public-iced", target_os = "windows"))]
mod window_menu;

pub use anyhow::{Error, Result};
pub use application::{Application, application, application_with_context};
#[doc(hidden)]
pub use application::{ApplicationInitializer, ContextInitialize};
pub use lintel_core as core;
pub use lintel_core::context::{AppContext, AppContextApi};
pub use lintel_core::tasks::TaskScope;
#[cfg(feature = "public-cef")]
pub use lintel_iced_widgets::webview;
pub use lintel_lib_iced as iced;
pub use lintel_plugin_iced::{IcedAppPluginApi, IcedAppPluginApiExt, ProgramCommandError};
#[doc(hidden)]
pub use multi_window::{ContextBoot, MultiWindowBoot};
pub use multi_window::{MultiWindow, multi_window, multi_window_with_context};

#[cfg(feature = "public-cef")]
pub use webview_app::{WebviewApplication, WebviewWindowEvent, WebviewWindows, webview_app};
#[cfg(feature = "public-cef")]
pub use webview_bridge::WebviewCommandContext;

#[cfg(feature = "desktop")]
pub use lintel_desktop as desktop;
pub use window::{WindowError, WindowEvent, WindowId, WindowManager, WindowStatus};
pub mod permissions;

#[cfg(feature = "crash-reporting")]
pub use lintel_crash as crash;

#[cfg(feature = "crash-reports")]
pub mod crash_reports;

/// 业务启动消息与程序创建共用渲染 FIFO，避免 EventLoopProxy 和订阅通道之间的竞态。
pub(crate) fn dispatch_program_message(
  context: &core::context::AppContext,
  message: std::sync::Arc<dyn iced::AnyMessage>,
) -> std::result::Result<(), ProgramCommandError> {
  context
    .service_store
    .get_service::<dyn IcedAppPluginApi>()
    .ok_or(ProgramCommandError::RendererStopped)?
    .try_dispatch(message)
}

#[cfg(all(feature = "desktop", feature = "public-iced"))]
pub mod accessibility;
#[cfg(all(feature = "desktop", feature = "public-iced"))]
pub use accessibility as a11y;

#[cfg(feature = "updater")]
pub use lintel_updater as updater;

#[cfg(all(feature = "desktop", feature = "public-iced"))]
pub mod menu;
