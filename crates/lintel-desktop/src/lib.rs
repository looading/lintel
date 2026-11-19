//! Native desktop integrations with a thread-safe service and a main-thread resource owner.
//!
//! Enable `native` for all OS backends or individual capabilities (`native-ui`, `clipboard`,
//! `dialogs`, `notifications`, `opener`, `credentials`, `permissions`, `accessibility`). Without features, validated protocol
//! routing and Unix single-instance activation still work; native operations return typed errors.
//! See the package README for host event-loop wiring and platform limitations.
#![forbid(unsafe_code)]

#[cfg(feature = "accessibility")]
pub mod accessibility;
mod backend;
pub mod client_menu;
pub mod credentials;
mod error;
pub mod instance;
#[cfg(all(
  unix,
  any(
    feature = "linux-tray",
    feature = "linux-x11-shortcuts",
    feature = "linux-portal-shortcuts"
  )
))]
pub mod linux;
mod model;
pub mod opener;
pub mod permissions;
pub mod protocol;
mod runtime;
#[cfg(all(feature = "windows-menu", target_os = "windows"))]
pub mod windows_menu;

#[cfg(all(feature = "native-ui", any(target_os = "macos", target_os = "windows")))]
mod ui;

pub use credentials::{CredentialStore, Secret};
pub use error::{DesktopError, Result};
pub use model::*;
#[cfg(feature = "windows-menu")]
pub use runtime::WindowMenuLease;
pub use runtime::{DesktopRuntime, DesktopService, Pending};

/// Use these exact native event types when forwarding host menu, tray, and hotkey events.
#[cfg(all(feature = "native-ui", any(target_os = "macos", target_os = "windows")))]
pub mod native_ui {
  pub use global_hotkey;
  pub use muda;
  pub use tray_icon;
}
