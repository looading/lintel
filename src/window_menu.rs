//! Scoped Windows menu attachment for managed native windows.
use std::time::Duration;

use lintel_desktop::{Pending, WindowMenuLease};

use crate::{
  iced::Task,
  window::{WindowError, WindowId, WindowManager, WindowStatus},
};

impl WindowManager {
  /// Binds a menu to an opened Windows window without passing platform handles.
  ///
  /// Store the returned lease in application state and remove it on the manager's
  /// logical `WindowEvent::Closed`. Dropping its last clone detaches the menu and
  /// releases the native owner through the main-thread runtime, even if its queue
  /// is full. Do not wait for Win32 Destroyed, which may depend on this owner.
  /// Change an existing binding through `desktop().set_window_menu(lease.target(), ...)`;
  /// keep one lease per window and do not start concurrent registrations for it.
  pub fn bind_menu(
    &self,
    id: WindowId,
    menu: lintel_desktop::MenuId,
  ) -> Result<Task<Result<WindowMenuLease, String>>, WindowError> {
    let service = self.desktop().ok_or(WindowError::DesktopUnavailable)?;
    let registration_service = service.clone();
    let registration = self.with_native_window(id, move |owner| {
      registration_service.register_windows_window_scoped(owner)
    })?;
    let windows = self.clone();
    Ok(registration.then(move |registration| {
      let service = service.clone();
      let windows = windows.clone();
      Task::perform(
        async move {
          let pending = registration
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
          let lease = receive(pending).await?;
          if windows.status(id) != Some(WindowStatus::Open) {
            return Err(WindowError::WindowNotOpen(id).to_string());
          }
          receive(
            service
              .set_window_menu(lease.target(), Some(menu))
              .map_err(|error| error.to_string())?,
          )
          .await?;
          if windows.status(id) != Some(WindowStatus::Open) {
            return Err(WindowError::WindowNotOpen(id).to_string());
          }
          Ok(lease)
        },
        |result| result,
      )
    }))
  }
}

async fn receive<T: Send + 'static>(pending: Pending<T>) -> Result<T, String> {
  tokio::time::timeout(Duration::from_secs(5), pending)
    .await
    .map_err(|_| "desktop menu command did not complete within 5 seconds".to_owned())?
    .map_err(|error| error.to_string())
}
