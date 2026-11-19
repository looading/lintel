#[cfg(all(feature = "public-cef", not(any(target_os = "macos", target_os = "windows"))))]
compile_error!("public-cef currently supports macOS and bootstrapped Windows CPU OSR only");

#[cfg(feature = "public-cef")]
const _: () = assert!(
  cef::sys::CEF_VERSION_MAJOR == 143 && cef::sys::CEF_VERSION_MINOR == 0 && cef::sys::CEF_VERSION_PATCH == 14,
  "public-cef requires the pinned public CEF 143.0.14 bindings and matching SDK"
);

mod browser;
mod client;
mod convert;
mod data;
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub(crate) mod drag_drop;
mod error;
mod init;
mod instance;
mod lifecycle;
mod preload;
pub mod renderer_process;
mod request;
mod runtime;
pub mod task;
mod v8;
mod web_pages;
mod widget;
#[cfg(any(target_os = "windows", test))]
pub mod windows_bootstrap;

pub use browser::{ChromiumGpuInfo, IcyBrowserProcessHandler, IcyCefApp};
pub use client::{DownloadEvent, RequestEvent};
pub use error::{CefError, Result};
pub use init::{BrowserProcessMessage, init_cef, init_cef_with_root_cache, pre_init_cef};
pub use instance::{
  ActiveContextMenu, CefAction, CefBrowser, CefMessage, CefUpdate, ContextMenuAction, ContextMenuConfig,
  ContextMenuItem, ContextMenuLabels, ContextMenuRequest, InvokeError, JsActor, JsHandler, JsInvocation, WebviewApi,
  WebviewConfig, WebviewRenderMode,
};
pub use lifecycle::{ClientLifecyeleImpl, LocalPagePolicy};
pub use preload::PreloadType;
pub use request::cookie::{CefCookie, get_cookies};
pub use v8::IcyV8HandlerBuilder;
pub use web_pages::webview_pages;
pub use widget::{
  BrowserId, Webview, WebviewFocusReason, WebviewKeyReason, WebviewSyntheticKey, WebviewUiCommand,
  WebviewUiStateSnapshot, toggle_devtools,
};
