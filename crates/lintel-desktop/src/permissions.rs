//! Unified system permission capabilities. Reading status never requests permission.
//! Requests are explicit API calls; none are performed by runtime setup, examples, or tests.
use std::time::Duration;

use crate::{Result, error::unsupported};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionKind {
  Notifications,
  ScreenRecording,
  InputMonitoring,
  Accessibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionStatus {
  Granted,
  /// Only a negative observation is available; it does not distinguish refused from never asked.
  NotGranted,
  Denied,
  NotDetermined,
  Restricted,
  Provisional,
  Ephemeral,
  Unknown,
  /// No comparable per-application consent gate. Protected/elevated resources can still deny access.
  NotRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StatusSupport {
  Native,
  NotRequired,
  Unsupported,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RequestSupport {
  /// The native completion reports a decision (notifications on macOS).
  NativeDecision,
  /// Request submits guidance/prompt; re-query later rather than treating submission as a grant.
  NativePrompt,
  NotRequired,
  Unsupported,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionSupport {
  pub status: StatusSupport,
  pub request: RequestSupport,
}

#[derive(Debug, Clone)]
pub struct PermissionContext {
  /// Required for Windows notification status: this must be the installed AppUserModelID.
  /// macOS always queries its own actual bundle; it does not impersonate this identifier.
  pub application_id: Option<String>,
  /// Bounds asynchronous notification callbacks. Timeout does not dismiss an already shown prompt.
  pub timeout: Duration,
}
impl Default for PermissionContext {
  fn default() -> Self {
    Self {
      application_id: None,
      timeout: Duration::from_secs(30),
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionRequestOutcome {
  Completed(PermissionStatus),
  /// No callback exists for the user's eventual choice. This is only the current observation.
  PromptRequested {
    current: PermissionStatus,
  },
  NotRequired,
}

#[derive(Clone, Copy)]
enum Platform {
  Mac,
  Windows,
  Linux,
  Other,
}
fn platform() -> Platform {
  if cfg!(target_os = "macos") {
    Platform::Mac
  } else if cfg!(target_os = "windows") {
    Platform::Windows
  } else if cfg!(target_os = "linux") {
    Platform::Linux
  } else {
    Platform::Other
  }
}

/// Compilation/platform support only, separate from the OS's current authorization state.
/// A native backend can still report a missing bundle, missing session, policy denial, or OS error.
pub fn support(permission: PermissionKind) -> PermissionSupport {
  support_for(platform(), cfg!(feature = "permissions"), permission)
}
fn support_for(platform: Platform, enabled: bool, permission: PermissionKind) -> PermissionSupport {
  use PermissionKind::*;
  if !enabled {
    return PermissionSupport {
      status: StatusSupport::Unsupported,
      request: RequestSupport::Unsupported,
    };
  }
  match (platform, permission) {
    (Platform::Mac, Notifications) => PermissionSupport {
      status: StatusSupport::Native,
      request: RequestSupport::NativeDecision,
    },
    (Platform::Mac, _) => PermissionSupport {
      status: StatusSupport::Native,
      request: RequestSupport::NativePrompt,
    },
    (Platform::Windows, Notifications) => PermissionSupport {
      status: StatusSupport::Native,
      request: RequestSupport::Unsupported,
    },
    (Platform::Windows, _) => PermissionSupport {
      status: StatusSupport::NotRequired,
      request: RequestSupport::NotRequired,
    },
    // Linux portal grants are per-session; neither X11 nor Wayland provides a universal global
    // TCC-style status/request API. The host must use its actual portal/session backend.
    (Platform::Linux | Platform::Other, _) => PermissionSupport {
      status: StatusSupport::Unsupported,
      request: RequestSupport::Unsupported,
    },
  }
}

fn unavailable(permission: PermissionKind, operation: &'static str) -> crate::DesktopError {
  let capability = match permission {
    PermissionKind::Notifications => "notification permission",
    PermissionKind::ScreenRecording => "screen recording permission",
    PermissionKind::InputMonitoring => "input monitoring permission",
    PermissionKind::Accessibility => "accessibility permission",
  };
  let reason = if !cfg!(feature = "permissions") {
    "enable the permissions feature"
  } else if cfg!(target_os = "windows") && operation == "request" {
    "Windows toast permission has no universal request API; application registration/user settings are separate"
  } else {
    "this platform requires a host-specific portal/session permission backend"
  };
  unsupported(capability, reason)
}

/// Query authorization without showing OS UI. macOS notification queries require a real .app
/// bundle; Windows notification queries require its installed AppUserModelID in context.
pub async fn status(permission: PermissionKind, context: &PermissionContext) -> Result<PermissionStatus> {
  match support(permission).status {
    StatusSupport::Unsupported => Err(unavailable(permission, "status")),
    StatusSupport::NotRequired => Ok(PermissionStatus::NotRequired),
    StatusSupport::Native => native_status(permission, context).await,
  }
}

/// May display OS authorization UI. Call from an explicit user action; startup never calls this.
/// PromptRequested is not approval. Re-query after the user returns from System Settings.
pub async fn request(permission: PermissionKind, context: &PermissionContext) -> Result<PermissionRequestOutcome> {
  match support(permission).request {
    RequestSupport::Unsupported => Err(unavailable(permission, "request")),
    RequestSupport::NotRequired => Ok(PermissionRequestOutcome::NotRequired),
    RequestSupport::NativeDecision | RequestSupport::NativePrompt => native_request(permission, context).await,
  }
}

#[cfg(all(feature = "permissions", any(target_os = "macos", test)))]
async fn bounded<T>(future: impl std::future::Future<Output = Result<T>>, timeout: Duration) -> Result<T> {
  use futures_util::future::{Either, select};
  if timeout.is_zero() {
    return Err(crate::error::invalid("permission timeout", "must be greater than zero"));
  }
  match select(Box::pin(future), Box::pin(futures_timer::Delay::new(timeout))).await {
    Either::Left((result, _)) => result,
    Either::Right(_) => Err(crate::DesktopError::Timeout(timeout)),
  }
}

async fn native_status(permission: PermissionKind, context: &PermissionContext) -> Result<PermissionStatus> {
  #[cfg(all(feature = "permissions", target_os = "macos"))]
  {
    match permission {
      PermissionKind::Notifications => {
        bounded(
          async {
            let raw = lintel_desktop_platform::macos::notification_status()
              .await
              .map_err(|e| crate::error::native("query notification authorization", e))?;
            Ok(notification_status(raw))
          },
          context.timeout,
        )
        .await
      }
      PermissionKind::ScreenRecording => Ok(observed(objc2_core_graphics::CGPreflightScreenCaptureAccess())),
      PermissionKind::InputMonitoring => {
        use objc2_io_kit::{IOHIDCheckAccess, IOHIDRequestType};
        Ok(input_status(IOHIDCheckAccess(IOHIDRequestType::ListenEvent).0))
      }
      PermissionKind::Accessibility => accessibility_status(),
    }
  }
  #[cfg(all(feature = "permissions", target_os = "windows"))]
  {
    use windows::{UI::Notifications::ToastNotificationManager, core::HSTRING};
    if permission != PermissionKind::Notifications {
      return Ok(PermissionStatus::NotRequired);
    }
    let id = context.application_id.as_deref().ok_or_else(|| {
      crate::error::invalid(
        "notification application ID",
        "Windows status requires an installed AppUserModelID",
      )
    })?;
    crate::model::validate_text("notification application ID", id, 256)?;
    if id.is_empty() {
      return Err(crate::error::invalid(
        "notification application ID",
        "must not be empty",
      ));
    }
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(id))
      .map_err(|e| crate::error::native("create notification permission probe", e))?;
    let setting = notifier
      .Setting()
      .map_err(|e| crate::error::native("query Windows notification setting", e))?;
    Ok(windows_notification_status(setting.0))
  }
  #[cfg(not(all(feature = "permissions", any(target_os = "macos", target_os = "windows"))))]
  {
    let _ = context;
    Err(unavailable(permission, "status"))
  }
}

async fn native_request(permission: PermissionKind, context: &PermissionContext) -> Result<PermissionRequestOutcome> {
  #[cfg(all(feature = "permissions", target_os = "macos"))]
  {
    if permission == PermissionKind::Notifications {
      return bounded(
        async {
          lintel_desktop_platform::macos::request_notifications()
            .await
            .map_err(|e| crate::error::native("request notification authorization", e))?;
          let raw = lintel_desktop_platform::macos::notification_status()
            .await
            .map_err(|e| crate::error::native("query requested notification authorization", e))?;
          Ok(PermissionRequestOutcome::Completed(notification_status(raw)))
        },
        context.timeout,
      )
      .await;
    }
    match permission {
      PermissionKind::Accessibility => {
        let _ = request_accessibility()?;
      }
      PermissionKind::ScreenRecording => {
        let _ = objc2_core_graphics::CGRequestScreenCaptureAccess();
      }
      PermissionKind::InputMonitoring => {
        let _ = objc2_io_kit::IOHIDRequestAccess(objc2_io_kit::IOHIDRequestType::ListenEvent);
      }
      PermissionKind::Notifications => unreachable!(),
    }
    Ok(PermissionRequestOutcome::PromptRequested {
      current: native_status(permission, context).await?,
    })
  }
  #[cfg(not(all(feature = "permissions", target_os = "macos")))]
  {
    let _ = context;
    Err(unavailable(permission, "request"))
  }
}

#[cfg(any(all(feature = "permissions", target_os = "macos"), test))]
fn observed(granted: bool) -> PermissionStatus {
  if granted {
    PermissionStatus::Granted
  } else {
    PermissionStatus::NotGranted
  }
}
#[cfg(any(all(feature = "permissions", target_os = "macos"), test))]
fn notification_status(raw: isize) -> PermissionStatus {
  match raw {
    0 => PermissionStatus::NotDetermined,
    1 => PermissionStatus::Denied,
    2 => PermissionStatus::Granted,
    3 => PermissionStatus::Provisional,
    4 => PermissionStatus::Ephemeral,
    _ => PermissionStatus::Unknown,
  }
}
#[cfg(any(all(feature = "permissions", target_os = "macos"), test))]
fn input_status(raw: u32) -> PermissionStatus {
  match raw {
    0 => PermissionStatus::Granted,
    1 => PermissionStatus::Denied,
    2 => PermissionStatus::NotDetermined,
    _ => PermissionStatus::Unknown,
  }
}
#[cfg(any(all(feature = "permissions", target_os = "windows"), test))]
fn windows_notification_status(raw: i32) -> PermissionStatus {
  match raw {
    0 => PermissionStatus::Granted,
    1 | 2 => PermissionStatus::Denied,
    3 | 4 => PermissionStatus::Restricted,
    _ => PermissionStatus::Unknown,
  }
}

/// Compatibility entry point for the real macOS Accessibility trust probe; never prompts.
pub fn accessibility_status() -> Result<PermissionStatus> {
  #[cfg(all(feature = "permissions", target_os = "macos"))]
  {
    Ok(observed(
      macos_accessibility_client::accessibility::application_is_trusted(),
    ))
  }
  #[cfg(all(feature = "permissions", target_os = "windows"))]
  {
    Ok(PermissionStatus::NotRequired)
  }
  #[cfg(not(all(feature = "permissions", any(target_os = "macos", target_os = "windows"))))]
  {
    Err(unavailable(PermissionKind::Accessibility, "status"))
  }
}

/// Compatibility explicit Accessibility guidance request. Return value is only current trust.
pub fn request_accessibility() -> Result<PermissionStatus> {
  #[cfg(all(feature = "permissions", target_os = "macos"))]
  {
    Ok(observed(
      macos_accessibility_client::accessibility::application_is_trusted_with_prompt(),
    ))
  }
  #[cfg(all(feature = "permissions", target_os = "windows"))]
  {
    Ok(PermissionStatus::NotRequired)
  }
  #[cfg(not(all(feature = "permissions", any(target_os = "macos", target_os = "windows"))))]
  {
    Err(unavailable(PermissionKind::Accessibility, "request"))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn support_distinguishes_query_request_and_session_specific_gaps() {
    let windows = support_for(Platform::Windows, true, PermissionKind::Notifications);
    assert_eq!(windows.status, StatusSupport::Native);
    assert_eq!(windows.request, RequestSupport::Unsupported);
    assert_eq!(
      support_for(Platform::Mac, true, PermissionKind::Accessibility).request,
      RequestSupport::NativePrompt
    );
    assert_eq!(
      support_for(Platform::Mac, true, PermissionKind::Notifications).request,
      RequestSupport::NativeDecision
    );
    assert_eq!(
      support_for(Platform::Linux, true, PermissionKind::ScreenRecording).status,
      StatusSupport::Unsupported
    );
    assert_eq!(
      support_for(Platform::Mac, false, PermissionKind::Notifications).status,
      StatusSupport::Unsupported
    );
  }
  #[test]
  fn native_mappings_preserve_ambiguity_and_future_statuses() {
    assert_eq!(observed(false), PermissionStatus::NotGranted);
    assert_eq!(notification_status(0), PermissionStatus::NotDetermined);
    assert_eq!(notification_status(3), PermissionStatus::Provisional);
    assert_eq!(notification_status(99), PermissionStatus::Unknown);
    assert_eq!(input_status(2), PermissionStatus::NotDetermined);
    assert_eq!(input_status(99), PermissionStatus::Unknown);
    assert_eq!(windows_notification_status(3), PermissionStatus::Restricted);
    assert_eq!(windows_notification_status(99), PermissionStatus::Unknown);
  }
  #[cfg(feature = "permissions")]
  #[test]
  fn async_timeout_is_not_reported_as_denial() {
    let result: Result<()> = futures_lite::future::block_on(bounded(std::future::pending(), Duration::from_millis(1)));
    assert!(matches!(result, Err(crate::DesktopError::Timeout(_))));
  }
  #[cfg(not(feature = "permissions"))]
  #[test]
  fn disabled_operations_explicitly_reject_without_native_requests() {
    let context = PermissionContext::default();
    for kind in [
      PermissionKind::Notifications,
      PermissionKind::ScreenRecording,
      PermissionKind::InputMonitoring,
      PermissionKind::Accessibility,
    ] {
      assert!(matches!(
        futures_lite::future::block_on(status(kind, &context)),
        Err(crate::DesktopError::Unsupported { .. })
      ));
      assert!(matches!(
        futures_lite::future::block_on(request(kind, &context)),
        Err(crate::DesktopError::Unsupported { .. })
      ));
    }
  }
}
