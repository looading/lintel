use cef::{rc::*, *};
use std::ptr::null_mut;

#[derive(Clone)]
pub struct IcyFocusHandler {}

impl IcyFocusHandler {
  pub fn new() -> Self {
    Self {}
  }
}

pub(crate) struct FocusHandlerBuilder {
  object: *mut RcImpl<sys::_cef_focus_handler_t, Self>,
  keyboard_handler: IcyFocusHandler,
}

impl FocusHandlerBuilder {
  pub(crate) fn build(loader: IcyFocusHandler) -> FocusHandler {
    FocusHandler::new(Self {
      object: null_mut(),
      keyboard_handler: loader,
    })
  }
}

impl WrapFocusHandler for FocusHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_focus_handler_t, Self>) {
    self.object = object;
  }
}

impl Rc for FocusHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl Clone for FocusHandlerBuilder {
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

impl ImplFocusHandler for FocusHandlerBuilder {
  fn get_raw(&self) -> *mut sys::_cef_focus_handler_t {
    self.object.cast()
  }

  fn on_got_focus(&self, browser: Option<&mut Browser>) {}
  fn on_take_focus(&self, browser: Option<&mut Browser>, next: ::std::os::raw::c_int) {}
}
