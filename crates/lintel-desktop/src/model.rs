use std::{collections::HashSet, path::PathBuf};

use crate::{Result, error::invalid};

macro_rules! handle {
  ($name:ident) => {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct $name(pub(crate) u64);
    impl $name {
      pub fn as_u64(self) -> u64 {
        self.0
      }
    }
  };
}
handle!(MenuId);
handle!(TrayId);
handle!(ShortcutId);
handle!(WindowMenuTarget);

/// Declarative menus may be used both as application menus and tray menus.
#[derive(Debug, Clone, Default)]
pub struct MenuSpec {
  pub items: Vec<MenuEntry>,
}

#[derive(Debug, Clone)]
pub enum MenuEntry {
  Item {
    id: String,
    label: String,
    enabled: bool,
    accelerator: Option<String>,
  },
  Check {
    id: String,
    label: String,
    enabled: bool,
    checked: bool,
  },
  Submenu {
    label: String,
    enabled: bool,
    items: Vec<MenuEntry>,
  },
  Separator,
}

impl MenuEntry {
  pub fn item(id: impl Into<String>, label: impl Into<String>) -> Self {
    Self::Item {
      id: id.into(),
      label: label.into(),
      enabled: true,
      accelerator: None,
    }
  }
}

impl MenuSpec {
  /// Reject duplicate action IDs and pathological trees before creating native objects.
  pub fn validate(&self) -> Result<()> {
    fn visit(items: &[MenuEntry], depth: usize, ids: &mut HashSet<String>, count: &mut usize) -> Result<()> {
      if depth > 16 {
        return Err(invalid("menu", "nesting exceeds 16 levels"));
      }
      for item in items {
        *count += 1;
        if *count > 1024 {
          return Err(invalid("menu", "more than 1024 entries"));
        }
        match item {
          MenuEntry::Item { id, label, .. } | MenuEntry::Check { id, label, .. } => {
            validate_text("menu label", label, 4096)?;
            validate_text("menu item id", id, 256)?;
            if id.is_empty() || !ids.insert(id.clone()) {
              return Err(invalid(
                "menu item id",
                "IDs must be nonempty and unique within the menu",
              ));
            }
          }
          MenuEntry::Submenu { label, items, .. } => {
            validate_text("submenu label", label, 4096)?;
            visit(items, depth + 1, ids, count)?;
          }
          MenuEntry::Separator => {}
        }
      }
      Ok(())
    }
    visit(&self.items, 0, &mut HashSet::new(), &mut 0)
  }
}

/// Straight RGBA pixels. The package never decodes untrusted image formats.
#[derive(Debug, Clone)]
pub struct TrayIcon {
  pub rgba: Vec<u8>,
  pub width: u32,
  pub height: u32,
  pub template: bool,
}

impl TrayIcon {
  pub fn validate(&self) -> Result<()> {
    let bytes = self.width.checked_mul(self.height).and_then(|n| n.checked_mul(4));
    if self.width == 0
      || self.height == 0
      || self.width > 4096
      || self.height > 4096
      || bytes.map(|n| n as usize) != Some(self.rgba.len())
    {
      return Err(invalid(
        "tray icon",
        "expected width × height × 4 RGBA bytes, dimensions 1..=4096",
      ));
    }
    Ok(())
  }
}

#[derive(Debug, Clone)]
pub struct TraySpec {
  pub icon: TrayIcon,
  pub tooltip: Option<String>,
  pub menu: Option<MenuId>,
}

#[derive(Debug, Clone, Default)]
pub struct FileDialogOptions {
  pub title: Option<String>,
  pub directory: Option<PathBuf>,
  pub file_name: Option<String>,
  pub filters: Vec<FileFilter>,
}

#[derive(Debug, Clone)]
pub struct FileFilter {
  pub name: String,
  pub extensions: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileDialogKind {
  OpenFile,
  OpenFiles,
  OpenFolder,
  SaveFile,
}

/// A missing selection is represented by `None`. The rfd backend does not distinguish
/// cancellation from native dialog failures; no selection is never reported as success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSelection {
  File(PathBuf),
  Files(Vec<PathBuf>),
  Folder(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Notification {
  /// Installed macOS bundle identifier / Windows AppUserModelID / Linux application name.
  pub application_id: String,
  pub title: String,
  pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
  Left,
  Right,
  Middle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopEvent {
  #[cfg(all(
    unix,
    any(
      feature = "linux-tray",
      feature = "linux-x11-shortcuts",
      feature = "linux-portal-shortcuts"
    )
  ))]
  Linux(crate::linux::LinuxEvent),
  Menu {
    menu: MenuId,
    item: String,
  },
  Shortcut {
    shortcut: ShortcutId,
    pressed: bool,
  },
  TrayClick {
    tray: TrayId,
    button: MouseButton,
    pressed: bool,
  },
  TrayDoubleClick {
    tray: TrayId,
    button: MouseButton,
  },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
  Accessibility,
  AccessibilityPermission,
  Menus,
  ApplicationMenu,
  WindowMenus,
  Tray,
  GlobalShortcuts,
  Clipboard,
  FileDialogs,
  Notifications,
  Opener,
  Credentials,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
  /// Compiled with a platform backend. OS permissions/session can still deny an operation.
  Available,
  FeatureDisabled,
  UnsupportedPlatform,
}

pub fn availability(capability: Capability) -> Availability {
  use Availability::{Available, FeatureDisabled, UnsupportedPlatform};
  use Capability::*;
  let (enabled, supported) = match capability {
    Accessibility => (
      cfg!(feature = "accessibility"),
      cfg!(any(target_os = "macos", target_os = "windows", target_os = "linux")),
    ),
    AccessibilityPermission => (
      cfg!(feature = "permissions"),
      cfg!(any(target_os = "macos", target_os = "windows")),
    ),
    Menus | Tray => (
      cfg!(feature = "native-ui") || cfg!(all(target_os = "linux", feature = "linux-tray")),
      cfg!(any(target_os = "macos", target_os = "windows")) || cfg!(all(target_os = "linux", feature = "linux-tray")),
    ),
    GlobalShortcuts => (
      cfg!(feature = "native-ui")
        || cfg!(all(
          target_os = "linux",
          any(feature = "linux-x11-shortcuts", feature = "linux-portal-shortcuts")
        )),
      cfg!(any(target_os = "macos", target_os = "windows"))
        || cfg!(all(
          target_os = "linux",
          any(feature = "linux-x11-shortcuts", feature = "linux-portal-shortcuts")
        )),
    ),
    WindowMenus => (cfg!(feature = "windows-menu"), cfg!(target_os = "windows")),
    ApplicationMenu => (cfg!(feature = "native-ui"), cfg!(target_os = "macos")),
    Clipboard => (
      cfg!(feature = "clipboard"),
      cfg!(any(target_os = "macos", target_os = "windows", target_os = "linux")),
    ),
    FileDialogs => (
      cfg!(feature = "dialogs"),
      cfg!(any(target_os = "macos", target_os = "windows", target_os = "linux")),
    ),
    Notifications => (
      cfg!(feature = "notifications"),
      cfg!(any(target_os = "macos", target_os = "windows", target_os = "linux")),
    ),
    Opener => (
      cfg!(feature = "opener"),
      cfg!(any(target_os = "macos", target_os = "windows", target_os = "linux")),
    ),
    Credentials => (
      cfg!(feature = "credentials"),
      cfg!(any(target_os = "macos", target_os = "windows", target_os = "linux")),
    ),
  };
  if !enabled {
    FeatureDisabled
  } else if supported {
    Available
  } else {
    UnsupportedPlatform
  }
}

pub(crate) fn validate_text(field: &'static str, value: &str, max_bytes: usize) -> Result<()> {
  if value.contains('\0') || value.len() > max_bytes {
    return Err(invalid(
      field,
      format!("must contain no NUL bytes and at most {max_bytes} bytes"),
    ));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn duplicate_nested_menu_ids_are_rejected() {
    let spec = MenuSpec {
      items: vec![
        MenuEntry::item("open", "Open"),
        MenuEntry::Submenu {
          label: "More".into(),
          enabled: true,
          items: vec![MenuEntry::item("open", "Again")],
        },
      ],
    };
    assert!(spec.validate().is_err());
    assert!(
      MenuSpec {
        items: vec![MenuEntry::item("open", "Open")]
      }
      .validate()
      .is_ok()
    );
  }

  #[test]
  fn invalid_rgba_and_overflow_are_rejected() {
    for (width, height, length) in [(0, 1, 0), (1, 1, 3), (u32::MAX, u32::MAX, 0)] {
      assert!(
        TrayIcon {
          rgba: vec![0; length],
          width,
          height,
          template: false
        }
        .validate()
        .is_err()
      );
    }
    assert!(
      TrayIcon {
        rgba: vec![0; 4],
        width: 1,
        height: 1,
        template: false
      }
      .validate()
      .is_ok()
    );
  }
}
