//! Passive permission probe. This executable never polls request()/request_accessibility().
use lintel_desktop::permissions::{self, PermissionContext, PermissionKind};

fn main() -> lintel_desktop::Result<()> {
  let context = PermissionContext {
    application_id: std::env::args().find_map(|argument| argument.strip_prefix("--app-id=").map(str::to_owned)),
    ..Default::default()
  };
  let require_notifications = std::env::args().any(|argument| argument == "--require-notifications");
  let mut rows = Vec::new();
  for permission in [
    PermissionKind::Notifications,
    PermissionKind::ScreenRecording,
    PermissionKind::InputMonitoring,
    PermissionKind::Accessibility,
  ] {
    let support = permissions::support(permission);
    let status = futures_lite::future::block_on(permissions::status(permission, &context));
    if require_notifications && permission == PermissionKind::Notifications && status.is_err() {
      return status.map(|_| ());
    }
    rows.push(match status {
      Ok(status) => serde_json::json!({ "permission": permission, "support": support, "status": status }),
      Err(error) => serde_json::json!({ "permission": permission, "support": support, "error": error.to_string() }),
    });
  }
  println!(
    "{}",
    serde_json::json!({ "permission_requests_executed": 0, "permissions": rows })
  );
  Ok(())
}
