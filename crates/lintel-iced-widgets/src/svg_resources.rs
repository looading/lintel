use once_cell::sync::Lazy;

use crate::svg::Handle;

pub static CLOSE_SVG_BYTES: &[u8] = include_bytes!("../assets/svg/close.svg");
pub static TOAST_SUCCESS_SVG_BYTES: &[u8] = include_bytes!("../assets/success.svg");
pub static TOAST_INFO_SVG_BYTES: &[u8] = include_bytes!("../assets/info.svg");
pub static TOAST_WARN_SVG_BYTES: &[u8] = include_bytes!("../assets/warn.svg");
pub static TOAST_DANGER_SVG_BYTES: &[u8] = include_bytes!("../assets/danger.svg");
pub static MAC_MAX_ACTIVE_SVG_BYTES: &[u8] = include_bytes!("../assets/macMaxActive.svg");
pub static MAC_CLOSE_ACTIVE_SVG_BYTES: &[u8] = include_bytes!("../assets/macCloseActive.svg");
pub static MAC_MIN_ACTIVE_SVG_BYTES: &[u8] = include_bytes!("../assets/macMinActive.svg");

static CLOSE_SVG_HANDLE: Lazy<Handle> = Lazy::new(|| Handle::from_memory(CLOSE_SVG_BYTES));
static TOAST_SUCCESS_SVG_HANDLE: Lazy<Handle> = Lazy::new(|| Handle::from_memory(TOAST_SUCCESS_SVG_BYTES));
static TOAST_INFO_SVG_HANDLE: Lazy<Handle> = Lazy::new(|| Handle::from_memory(TOAST_INFO_SVG_BYTES));
static TOAST_WARN_SVG_HANDLE: Lazy<Handle> = Lazy::new(|| Handle::from_memory(TOAST_WARN_SVG_BYTES));
static TOAST_DANGER_SVG_HANDLE: Lazy<Handle> = Lazy::new(|| Handle::from_memory(TOAST_DANGER_SVG_BYTES));
static MAC_MAX_ACTIVE_SVG_HANDLE: Lazy<Handle> = Lazy::new(|| Handle::from_memory(MAC_MAX_ACTIVE_SVG_BYTES));
static MAC_CLOSE_ACTIVE_SVG_HANDLE: Lazy<Handle> = Lazy::new(|| Handle::from_memory(MAC_CLOSE_ACTIVE_SVG_BYTES));
static MAC_MIN_ACTIVE_SVG_HANDLE: Lazy<Handle> = Lazy::new(|| Handle::from_memory(MAC_MIN_ACTIVE_SVG_BYTES));

pub fn close() -> Handle {
  CLOSE_SVG_HANDLE.clone()
}

pub fn toast_success() -> Handle {
  TOAST_SUCCESS_SVG_HANDLE.clone()
}

pub fn toast_info() -> Handle {
  TOAST_INFO_SVG_HANDLE.clone()
}

pub fn toast_warn() -> Handle {
  TOAST_WARN_SVG_HANDLE.clone()
}

pub fn toast_danger() -> Handle {
  TOAST_DANGER_SVG_HANDLE.clone()
}

pub fn mac_max_active() -> Handle {
  MAC_MAX_ACTIVE_SVG_HANDLE.clone()
}

pub fn mac_close_active() -> Handle {
  MAC_CLOSE_ACTIVE_SVG_HANDLE.clone()
}

pub fn mac_min_active() -> Handle {
  MAC_MIN_ACTIVE_SVG_HANDLE.clone()
}
