//! Windows message-loop integration. Business code uses WindowMenuTarget, never a naked HWND.
//! Register an owned native window on the UI thread; unregister before the host releases it.
pub use lintel_desktop_platform::windows::{
  HasWindowHandle, WindowsMenuDispatcher, WindowsMenuMessageHook, WindowsMessage,
};
