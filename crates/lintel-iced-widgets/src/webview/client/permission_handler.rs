use cef::{rc::*, *};
use std::ptr::null_mut;

#[derive(Clone)]
pub struct IcyPermissionHandler {}

impl IcyPermissionHandler {
  pub fn new() -> Self {
    Self {}
  }
}

pub(crate) struct PermissionHandlerBuilder {
  object: *mut RcImpl<sys::_cef_permission_handler_t, Self>,
  permission_handler: IcyPermissionHandler,
}

impl PermissionHandlerBuilder {
  pub(crate) fn build(permission_handler: IcyPermissionHandler) -> PermissionHandler {
    PermissionHandler::new(Self {
      object: null_mut(),
      permission_handler,
    })
  }
}

impl WrapPermissionHandler for PermissionHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_permission_handler_t, Self>) {
    self.object = object;
  }
}

impl Rc for PermissionHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl Clone for PermissionHandlerBuilder {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc_impl = &mut *self.object;
      rc_impl.interface.add_ref();
      rc_impl
    };

    Self {
      object,
      permission_handler: self.permission_handler.clone(),
    }
  }
}

impl ImplPermissionHandler for PermissionHandlerBuilder {
  fn get_raw(&self) -> *mut sys::_cef_permission_handler_t {
    self.object.cast()
  }

  fn on_show_permission_prompt(
    &self,
    _browser: Option<&mut Browser>,
    _prompt_id: u64,
    _requesting_origin: Option<&CefString>,
    _requested_permissions: u32,
    callback: Option<&mut PermissionPromptCallback>,
  ) -> ::std::os::raw::c_int {
    if let Some(callback) = callback {
      callback.cont(PermissionRequestResult::ACCEPT);
      true as _
    } else {
      false as _
    }
  }
}
