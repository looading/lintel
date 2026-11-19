use std::ptr::null_mut;

use cef::{rc::*, *};

#[derive(Clone)]
pub struct IcyJsdialogHandler;

impl IcyJsdialogHandler {
  pub fn new() -> Self {
    Self
  }
}

pub(crate) struct JsdialogHandlerBuilder {
  object: *mut RcImpl<sys::_cef_jsdialog_handler_t, Self>,
  jsdialog_handler: IcyJsdialogHandler,
}

impl JsdialogHandlerBuilder {
  pub(crate) fn build(jsdialog_handler: IcyJsdialogHandler) -> JsdialogHandler {
    JsdialogHandler::new(Self {
      object: null_mut(),
      jsdialog_handler,
    })
  }
}

impl WrapJsdialogHandler for JsdialogHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_jsdialog_handler_t, Self>) {
    self.object = object;
  }
}

impl Rc for JsdialogHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl Clone for JsdialogHandlerBuilder {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc_impl = &mut *self.object;
      rc_impl.interface.add_ref();
      rc_impl
    };

    Self {
      object,
      jsdialog_handler: self.jsdialog_handler.clone(),
    }
  }
}

impl ImplJsdialogHandler for JsdialogHandlerBuilder {
  fn get_raw(&self) -> *mut sys::_cef_jsdialog_handler_t {
    self.object.cast()
  }

  fn on_jsdialog(
    &self,
    browser: Option<&mut Browser>,
    origin_url: Option<&CefString>,
    dialog_type: JsdialogType,
    message_text: Option<&CefString>,
    _default_prompt_text: Option<&CefString>,
    _callback: Option<&mut JsdialogCallback>,
    suppress_message: Option<&mut ::std::os::raw::c_int>,
  ) -> ::std::os::raw::c_int {
    if let Some(suppress_message) = suppress_message {
      *suppress_message = true as _;
    }

    tracing::info!(
      browser_id = browser.map(|browser| browser.identifier()),
      origin_url = origin_url.map(|url| url.to_string()),
      ?dialog_type,
      message_text = message_text.map(|text| text.to_string()),
      "blocked javascript dialog"
    );

    false as _
  }

  fn on_before_unload_dialog(
    &self,
    browser: Option<&mut Browser>,
    message_text: Option<&CefString>,
    is_reload: ::std::os::raw::c_int,
    callback: Option<&mut JsdialogCallback>,
  ) -> ::std::os::raw::c_int {
    if let Some(callback) = callback {
      callback.cont(true as _, None);
    }

    tracing::info!(
      browser_id = browser.map(|browser| browser.identifier()),
      message_text = message_text.map(|text| text.to_string()),
      is_reload = is_reload != 0,
      "blocked javascript beforeunload dialog"
    );

    true as _
  }
}
