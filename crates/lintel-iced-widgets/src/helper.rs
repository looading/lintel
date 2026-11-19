use iced::Task;
use lintel_lib_iced::iced;
use lintel_lib_iced::iced_winit::winit::raw_window_handle::RawWindowHandle;

pub fn register_dnd(window: iced::window::Id) -> Task<iced::window::Id> {
  #[cfg(target_os = "windows")]
  return iced::window::run(window, move |handle| {
    if let Ok(RawWindowHandle::Win32(window)) = handle.window_handle().map(|h| h.as_raw()) {
      use windows::Win32::System::Ole::{RegisterDragDrop, RevokeDragDrop};
      let hwnd = windows::Win32::Foundation::HWND(window.hwnd.get() as *mut _);
      let target = crate::webview::drag_drop::DropTargetWin::create(hwnd);
      unsafe {
        _ = RevokeDragDrop(hwnd);
        _ = RegisterDragDrop(hwnd, &target);
      };
    }
    window
  });
  #[cfg(target_os = "macos")]
  return iced::window::run(window, move |handle| {
    if let Ok(RawWindowHandle::AppKit(appkit_handle)) = handle.window_handle().map(|h| h.as_raw()) {
      let ns_view_ptr = appkit_handle.ns_view;
      // SAFETY: ns_view is a valid NSView pointer provided by winit.
      unsafe {
        let mtm = objc2::MainThreadMarker::new_unchecked();
        crate::webview::drag_drop::drop_target_mac::register_drag_types(ns_view_ptr, mtm);
      }
    }
    window
  });
}

pub fn unified_toolbar(window: iced::window::Id) -> iced::Task<iced::window::Id> {
  #[cfg(target_os = "windows")]
  return Task::done(window);

  #[cfg(target_os = "macos")]
  return iced::window::run(window, move |handle| {
    if let Ok(RawWindowHandle::AppKit(window)) = handle.window_handle().map(|h| h.as_raw()) {
      let ns_view = window.ns_view.as_ptr();
      if let Some(ns_view) = unsafe { objc2::rc::Retained::retain(ns_view.cast::<objc2_app_kit::NSView>()) }
        && let Some(window) = ns_view.window()
      {
        unsafe {
          // window.setToolbar(Some(&objc2_app_kit::NSToolbar::new(
          //   objc2::MainThreadMarker::new_unchecked(),
          // )));

          use tracing::info;
          window.setToolbarStyle(objc2_app_kit::NSWindowToolbarStyle::UnifiedCompact);

          info!("unified toolbar");
          if let Some(content_view) = window.contentView() {
            if let Some(layer) = content_view.layer() {
              layer.setCornerRadius(10.0);
            }
          }

          // 实现 titlebar 不能移动窗口， 配合 iced 实现窗口移动,解决tab拖拽冲突问题
          // 阻止窗口背景移动窗口
          window.setMovableByWindowBackground(false);
          // 阻止窗口移动
          window.setMovable(false);
        };
      }
    }
    window
  });
}

/// Windows: 给指定窗口设置独立的 Taskbar 分组 ID（AppUserModelID）。
///
/// 目的：让Lintel/达人两个窗口在任务栏里按不同图标分组显示，而不是合并到同一个应用组里。
pub fn set_taskbar_app_id(window: iced::window::Id, app_id: String) -> Task<iced::window::Id> {
  #[cfg(target_os = "windows")]
  return iced::window::run(window, move |handle| {
    if let Ok(RawWindowHandle::Win32(window_handle)) = handle.window_handle().map(|h| h.as_raw()) {
      use std::mem::ManuallyDrop;
      use windows::Win32::Foundation::HWND;
      use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, PROPERTYKEY, SHGetPropertyStoreForWindow};
      use windows::core::PROPVARIANT;

      let hwnd = HWND(window_handle.hwnd.get() as *mut _);

      unsafe {
        if let Ok(store) = SHGetPropertyStoreForWindow::<_, IPropertyStore>(hwnd) {
          // PKEY_AppUserModel_ID (System.AppUserModel.ID)
          // fmtid: {9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}, pid: 5
          // 手写避免 bindings 差异导致的找不到符号问题。
          let key = PROPERTYKEY {
            fmtid: windows::core::GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
            pid: 5,
          };

          // 注意：当前 windows bindings 没有 `InitPropVariantFromString`。
          // 这里使用 BSTR 形式的 `PROPVARIANT` 来写入属性（windows-core 提供 From<&str> 实现）。
          // Win7 vendor 构建里避免触发 `PropVariantClear` 的导入校验。
          let pv = ManuallyDrop::new(PROPVARIANT::from(app_id.as_str()));

          // Ignore failures: taskbar grouping is best-effort and should not break window creation.
          let _ = store.SetValue(&key, &*pv);
          let _ = store.Commit();
        }
      }
    }
    window
  });

  #[cfg(not(target_os = "windows"))]
  return Task::done(window);
}
