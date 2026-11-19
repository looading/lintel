pub(crate) mod cf_html;
pub(crate) mod drag_data;

#[cfg(target_os = "windows")]
mod clipboard_formats;
#[cfg(target_os = "windows")]
mod data_object;
#[cfg(target_os = "windows")]
mod drop_source;
#[cfg(target_os = "windows")]
mod drop_target;
#[cfg(target_os = "windows")]
mod enum_format;
#[cfg(target_os = "windows")]
mod storage;
#[cfg(target_os = "windows")]
mod win32_memory;

#[cfg(target_os = "macos")]
pub(crate) mod drop_target_mac;

pub(crate) use cf_html::{CFHtmlExtractMetadata, CFHtmlToHtml, CfHtmlMetadata, HtmlToCFHtml};
pub(crate) use drag_data::{DraggedFile, OsrDragEvents};

#[cfg(target_os = "windows")]
pub(crate) use data_object::{DataObjectWin, data_object_to_drag_data, drag_data_to_data_object};
#[cfg(target_os = "windows")]
pub(crate) use drop_source::DropSourceWin;
#[cfg(target_os = "windows")]
pub(crate) use drop_target::{
  DropTargetWin, register_browser_drop_target, start_dragging, unregister_browser_drop_target,
};
