use cef::{rc::*, *};
use std::{ffi::*, ptr::null_mut};

#[derive(Clone)]
pub struct IcyKeyboardHandler {}

impl IcyKeyboardHandler {
  pub fn new() -> Self {
    Self {}
  }
}

pub(crate) struct KeyboardHandlerBuilder {
  object: *mut RcImpl<sys::_cef_keyboard_handler_t, Self>,
  keyboard_handler: IcyKeyboardHandler,
}

impl KeyboardHandlerBuilder {
  pub(crate) fn build(loader: IcyKeyboardHandler) -> KeyboardHandler {
    KeyboardHandler::new(Self {
      object: null_mut(),
      keyboard_handler: loader,
    })
  }
}

impl WrapKeyboardHandler for KeyboardHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_keyboard_handler_t, Self>) {
    self.object = object;
  }
}

impl Rc for KeyboardHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl Clone for KeyboardHandlerBuilder {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc_impl = &mut *self.object;
      rc_impl.interface.add_ref();
      rc_impl
    };

    Self {
      object,
      keyboard_handler: self.keyboard_handler.clone(),
    }
  }
}

impl ImplKeyboardHandler for KeyboardHandlerBuilder {
  fn get_raw(&self) -> *mut sys::_cef_keyboard_handler_t {
    self.object.cast()
  }

  fn on_pre_key_event(
    &self,
    browser: Option<&mut Browser>,
    event: Option<&cef::KeyEvent>,
    #[cfg(target_os = "windows")] _os_event: Option<&mut sys::MSG>,
    #[cfg(target_os = "macos")] _os_event: *mut u8,
    #[cfg(target_os = "linux")] _os_event: Option<&mut sys::XEvent>,
    is_keyboard_shortcut: Option<&mut c_int>,
  ) -> c_int {
    let Some(browser) = browser else {
      return false as _;
    };
    let Some(frame) = browser.focused_frame() else {
      return false as _;
    };
    let Some(event) = event else {
      return false as _;
    };
    use cef::sys::cef_event_flags_t;
    let modifiers = cef_event_flags_t(event.modifiers as _);

    #[cfg(target_os = "macos")]
    let ctrl = modifiers
      & (cef_event_flags_t::EVENTFLAG_SHIFT_DOWN | {
        cef_event_flags_t::EVENTFLAG_ALT_DOWN | cef_event_flags_t::EVENTFLAG_COMMAND_DOWN
      })
      == cef_event_flags_t::EVENTFLAG_COMMAND_DOWN;

    #[cfg(target_os = "windows")]
    let ctrl = modifiers
      & (cef_event_flags_t::EVENTFLAG_CONTROL_DOWN
        | cef_event_flags_t::EVENTFLAG_SHIFT_DOWN
        | cef_event_flags_t::EVENTFLAG_ALT_DOWN)
      == cef_event_flags_t::EVENTFLAG_CONTROL_DOWN;
    if event.type_ == cef::sys::cef_key_event_type_t::KEYEVENT_RAWKEYDOWN.into() {
      let code = event.windows_key_code;
      if ctrl {
        match code {
          0x41 => frame.select_all(), // A
          0x5a => frame.undo(),       // Z
          0x58 => frame.cut(),        // X
          0x43 => frame.copy(),       // C
          0x56 => frame.paste(),      // V
          0x59 => frame.redo(),       // Y
          _ => return false as _,
        }
        if let Some(is_shortcut) = is_keyboard_shortcut {
          *is_shortcut = true as _;
        }
        return true as _;
      }
    }
    false as _
  }
}
