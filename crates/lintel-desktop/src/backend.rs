#![allow(unused_variables)]

use crate::{
  FileDialogKind, FileDialogOptions, FileSelection, MenuId, MenuSpec, Notification, Result, ShortcutId, TrayIcon,
  TrayId, TraySpec, model::validate_text,
};

#[derive(Default)]
pub(crate) struct Backend {
  #[cfg(all(feature = "native-ui", any(target_os = "macos", target_os = "windows")))]
  pub(crate) ui: crate::ui::UiBackend,
  #[cfg(feature = "clipboard")]
  clipboard: Option<arboard::Clipboard>,
}

macro_rules! ui_call {
  ($self:ident, $capability:literal, $method:ident($($arg:expr),*)) => {{
    #[cfg(all(feature = "native-ui", any(target_os = "macos", target_os = "windows")))]
    { $self.ui.$method($($arg),*) }
    #[cfg(not(all(feature = "native-ui", any(target_os = "macos", target_os = "windows"))))]
    { Err(crate::error::unsupported($capability, "enable native-ui on macOS/Windows; Linux requires a host GTK/X11 integration")) }
  }};
}

impl Backend {
  pub(crate) fn set_window_menu(&mut self, target: crate::WindowMenuTarget, menu: Option<MenuId>) -> Result<()> {
    #[cfg(all(feature = "windows-menu", target_os = "windows"))]
    {
      self.ui.set_window_menu(target, menu)
    }
    #[cfg(not(all(feature = "windows-menu", target_os = "windows")))]
    {
      let _ = (target, menu);
      Err(crate::error::unsupported(
        "window menus",
        "requires windows-menu on Windows and a registered owned native window",
      ))
    }
  }
  pub(crate) fn create_menu(&mut self, spec: MenuSpec) -> Result<MenuId> {
    ui_call!(self, "menus", create_menu(spec))
  }
  pub(crate) fn remove_menu(&mut self, id: MenuId) -> Result<()> {
    ui_call!(self, "menus", remove_menu(id))
  }
  pub(crate) fn set_application_menu(&mut self, id: Option<MenuId>) -> Result<()> {
    ui_call!(self, "application menu", set_application_menu(id))
  }
  pub(crate) fn set_menu_item_enabled(&mut self, id: MenuId, item: &str, enabled: bool) -> Result<()> {
    ui_call!(self, "menus", set_menu_item_enabled(id, item, enabled))
  }
  pub(crate) fn set_menu_item_checked(&mut self, id: MenuId, item: &str, checked: bool) -> Result<()> {
    ui_call!(self, "menus", set_menu_item_checked(id, item, checked))
  }
  pub(crate) fn set_menu_item_label(&mut self, id: MenuId, item: &str, label: &str) -> Result<()> {
    ui_call!(self, "menus", set_menu_item_label(id, item, label))
  }
  pub(crate) fn create_tray(&mut self, spec: TraySpec) -> Result<TrayId> {
    ui_call!(self, "tray", create_tray(spec))
  }
  pub(crate) fn remove_tray(&mut self, id: TrayId) -> Result<()> {
    ui_call!(self, "tray", remove_tray(id))
  }
  pub(crate) fn set_tray_menu(&mut self, id: TrayId, menu: Option<MenuId>) -> Result<()> {
    ui_call!(self, "tray", set_tray_menu(id, menu))
  }
  pub(crate) fn set_tray_visible(&mut self, id: TrayId, visible: bool) -> Result<()> {
    ui_call!(self, "tray", set_tray_visible(id, visible))
  }
  pub(crate) fn set_tray_icon(&mut self, id: TrayId, icon: TrayIcon) -> Result<()> {
    ui_call!(self, "tray", set_tray_icon(id, icon))
  }
  pub(crate) fn set_tray_tooltip(&mut self, id: TrayId, tooltip: Option<String>) -> Result<()> {
    if let Some(text) = &tooltip {
      validate_text("tray tooltip", text, 4096)?;
    }
    ui_call!(self, "tray", set_tray_tooltip(id, tooltip))
  }
  pub(crate) fn register_shortcut(&mut self, accelerator: &str) -> Result<ShortcutId> {
    ui_call!(self, "global shortcuts", register_shortcut(accelerator))
  }
  pub(crate) fn unregister_shortcut(&mut self, id: ShortcutId) -> Result<()> {
    ui_call!(self, "global shortcuts", unregister_shortcut(id))
  }

  #[cfg(feature = "clipboard")]
  fn clipboard(&mut self) -> Result<&mut arboard::Clipboard> {
    if self.clipboard.is_none() {
      self.clipboard = Some(arboard::Clipboard::new().map_err(|e| crate::error::native("initialize clipboard", e))?);
    }
    Ok(self.clipboard.as_mut().expect("clipboard initialized above"))
  }
  pub(crate) fn read_clipboard_text(&mut self) -> Result<String> {
    #[cfg(feature = "clipboard")]
    {
      self
        .clipboard()?
        .get_text()
        .map_err(|e| crate::error::native("read clipboard text", e))
    }
    #[cfg(not(feature = "clipboard"))]
    {
      Err(crate::error::unsupported("clipboard", "enable the clipboard feature"))
    }
  }
  pub(crate) fn write_clipboard_text(&mut self, text: String) -> Result<()> {
    #[cfg(feature = "clipboard")]
    {
      self
        .clipboard()?
        .set_text(text)
        .map_err(|e| crate::error::native("write clipboard text", e))
    }
    #[cfg(not(feature = "clipboard"))]
    {
      Err(crate::error::unsupported("clipboard", "enable the clipboard feature"))
    }
  }
  pub(crate) fn clear_clipboard(&mut self) -> Result<()> {
    #[cfg(feature = "clipboard")]
    {
      self
        .clipboard()?
        .clear()
        .map_err(|e| crate::error::native("clear clipboard", e))
    }
    #[cfg(not(feature = "clipboard"))]
    {
      Err(crate::error::unsupported("clipboard", "enable the clipboard feature"))
    }
  }

  pub(crate) fn show_file_dialog(
    &mut self,
    kind: FileDialogKind,
    options: FileDialogOptions,
  ) -> Result<Option<FileSelection>> {
    if let Some(title) = &options.title {
      validate_text("dialog title", title, 4096)?;
    }
    if let Some(file_name) = &options.file_name {
      validate_text("dialog file name", file_name, 4096)?;
    }
    for filter in &options.filters {
      validate_text("file filter name", &filter.name, 4096)?;
      for extension in &filter.extensions {
        validate_text("file extension", extension, 256)?;
      }
    }
    #[cfg(feature = "dialogs")]
    {
      let mut dialog = rfd::FileDialog::new();
      if let Some(title) = options.title {
        dialog = dialog.set_title(title);
      }
      if let Some(directory) = options.directory {
        dialog = dialog.set_directory(directory);
      }
      if let Some(file_name) = options.file_name {
        dialog = dialog.set_file_name(file_name);
      }
      for filter in options.filters {
        dialog = dialog.add_filter(filter.name, &filter.extensions);
      }
      Ok(match kind {
        FileDialogKind::OpenFile => dialog.pick_file().map(FileSelection::File),
        FileDialogKind::OpenFiles => dialog.pick_files().map(FileSelection::Files),
        FileDialogKind::OpenFolder => dialog.pick_folder().map(FileSelection::Folder),
        FileDialogKind::SaveFile => dialog.save_file().map(FileSelection::File),
      })
    }
    #[cfg(not(feature = "dialogs"))]
    {
      Err(crate::error::unsupported("file dialogs", "enable the dialogs feature"))
    }
  }

  pub(crate) fn show_notification(&mut self, notification: Notification) -> Result<()> {
    validate_text("notification application ID", &notification.application_id, 256)?;
    if notification.application_id.is_empty() {
      return Err(crate::error::invalid(
        "notification application ID",
        "must identify the installed application",
      ));
    }
    validate_text("notification title", &notification.title, 4096)?;
    validate_text("notification body", &notification.body, 64 * 1024)?;
    #[cfg(feature = "notifications")]
    {
      #[cfg(target_os = "macos")]
      configure_mac_notification_identity(&notification.application_id)?;
      let mut builder = notify_rust::Notification::new();
      builder.summary(&notification.title).body(&notification.body);
      #[cfg(target_os = "windows")]
      builder.app_id(&notification.application_id);
      #[cfg(all(unix, not(target_os = "macos")))]
      builder.appname(&notification.application_id);
      builder
        .show()
        .map_err(|e| crate::error::native("show notification", e))?;
      Ok(())
    }
    #[cfg(not(feature = "notifications"))]
    {
      Err(crate::error::unsupported(
        "notifications",
        "enable the notifications feature",
      ))
    }
  }
}

#[cfg(all(feature = "notifications", target_os = "macos"))]
fn configure_mac_notification_identity(application_id: &str) -> Result<()> {
  static APPLICATION: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
  let mut current = APPLICATION
    .lock()
    .map_err(|_| crate::error::native("configure notification identity", "identity lock poisoned"))?;
  if let Some(current) = current.as_deref() {
    if current != application_id {
      return Err(crate::error::invalid(
        "notification application ID",
        "notification identity is process-wide and was already configured differently",
      ));
    }
  } else {
    notify_rust::set_application(application_id)
      .map_err(|e| crate::error::native("configure notification application", e))?;
    *current = Some(application_id.to_owned());
  }
  Ok(())
}
