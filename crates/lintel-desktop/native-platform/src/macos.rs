//! Audited UserNotifications completion boundary. This never installs or replaces a delegate.
use std::{ptr::NonNull, sync::Mutex};

use block2::RcBlock;
use futures_channel::oneshot;
use objc2::{rc::Retained, runtime::Bool};
use objc2_foundation::{NSBundle, NSError};
use objc2_user_notifications::{UNAuthorizationOptions, UNNotificationSettings, UNUserNotificationCenter};

fn center() -> Result<Retained<UNUserNotificationCenter>, String> {
  let bundle = NSBundle::mainBundle();
  let identified = bundle
    .bundleIdentifier()
    .is_some_and(|identifier| !identifier.is_empty());
  let application = bundle
    .bundleURL()
    .pathExtension()
    .is_some_and(|extension| extension.to_string() == "app");
  if !identified || !application {
    return Err("UserNotifications requires this process to run from an identified .app bundle".into());
  }
  Ok(UNUserNotificationCenter::currentNotificationCenter())
}

/// Passive query of this application's real UNAuthorizationStatus; no permission request/delegate.
// Keep + Send an explicit public contract while making construction completely lazy.
#[allow(clippy::manual_async_fn)]
pub fn notification_status() -> impl std::future::Future<Output = Result<isize, String>> + Send {
  async {
    let receiver = {
      let center = center()?;
      let (sender, receiver) = oneshot::channel();
      let sender = Mutex::new(Some(sender));
      let completion = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
        // SAFETY: Apple supplies a live non-null object for this callback. Copy only the scalar;
        // no Objective-C reference escapes or crosses an await/thread boundary.
        let status = unsafe { settings.as_ref().authorizationStatus().0 };
        if let Some(sender) = sender.lock().unwrap_or_else(|poison| poison.into_inner()).take() {
          let _ = sender.send(status);
        }
      });
      // Apple copies this block. Its owned sender remains alive if the Rust future is canceled.
      center.getNotificationSettingsWithCompletionHandler(&completion);
      receiver
    }; // Native center and our block reference are released before await; the Future is Send.
    receiver
      .await
      .map_err(|_| "notification settings completion was dropped".to_owned())
  }
}

/// Explicit, lazy request. UI can appear only after this returned Future is polled.
/// Completion reports the native decision; it never replaces the notification delegate.
// async fn cannot spell the returned Future's Send bound on Rust 1.85.
#[allow(clippy::manual_async_fn)]
pub fn request_notifications() -> impl std::future::Future<Output = Result<bool, String>> + Send {
  async {
    let receiver = {
      let center = center()?;
      let (sender, receiver) = oneshot::channel();
      let sender = Mutex::new(Some(sender));
      let completion = RcBlock::new(move |granted: Bool, error: *mut NSError| {
        let result = if let Some(error) = NonNull::new(error) {
          // SAFETY: Apple's non-null NSError is live for this callback. Copy its string now.
          Err(unsafe { error.as_ref().localizedDescription().to_string() })
        } else {
          Ok(granted.as_bool())
        };
        if let Some(sender) = sender.lock().unwrap_or_else(|poison| poison.into_inner()).take() {
          let _ = sender.send(result);
        }
      });
      center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound | UNAuthorizationOptions::Badge,
        &completion,
      );
      receiver
    };
    receiver
      .await
      .map_err(|_| "notification authorization completion was dropped".to_owned())?
  }
}
