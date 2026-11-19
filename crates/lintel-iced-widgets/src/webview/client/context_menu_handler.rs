use cef::{
  ContextMenuHandler, ImplContextMenuHandler, WrapContextMenuHandler,
  rc::{Rc, RcImpl},
  sys, *,
};
use std::ptr::null_mut;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::webview::{BrowserId, ContextMenuRequest};

#[derive(Clone, Debug)]
pub struct ContextMenuEvent {
  pub request: ContextMenuRequest,
}

#[derive(Clone)]
pub struct IcyContextMenuHandler {
  tx: UnboundedSender<ContextMenuEvent>,
}

impl IcyContextMenuHandler {
  pub fn new() -> (Self, UnboundedReceiver<ContextMenuEvent>) {
    let (tx, rx) = unbounded_channel();
    (Self { tx }, rx)
  }

  fn build_request(
    &self,
    browser: Option<&mut Browser>,
    frame: Option<&mut Frame>,
    params: Option<&mut ContextMenuParams>,
  ) -> Option<ContextMenuRequest> {
    let browser_id = browser.as_ref().map(|browser| BrowserId::new(browser.identifier()))?;
    let params = params?;

    let selection_text = {
      let text = CefString::from(&params.selection_text()).to_string();
      (!text.is_empty()).then_some(text)
    };
    let has_selection = selection_text.as_ref().is_some_and(|text| !text.is_empty());

    Some(ContextMenuRequest {
      browser_id,
      x: params.xcoord() as f32,
      y: params.ycoord() as f32,
      page_url: browser
        .as_ref()
        .and_then(|browser| browser.main_frame())
        .map(|frame| CefString::from(&frame.url()).to_string()),
      frame_url: frame.as_ref().map(|frame| CefString::from(&frame.url()).to_string()),
      link_url: {
        let url = CefString::from(&params.link_url()).to_string();
        (!url.is_empty()).then_some(url)
      },
      source_url: {
        let url = CefString::from(&params.source_url()).to_string();
        (!url.is_empty()).then_some(url)
      },
      selection_text,
      is_editable: params.is_editable() == 1,
      has_selection,
    })
  }
}

pub(crate) struct ContextMenuHandlerBuilder {
  object: *mut RcImpl<sys::_cef_context_menu_handler_t, Self>,
  context_menu_handler: IcyContextMenuHandler,
}

impl ContextMenuHandlerBuilder {
  pub(crate) fn build(context_menu_handler: IcyContextMenuHandler) -> ContextMenuHandler {
    ContextMenuHandler::new(Self {
      object: null_mut(),
      context_menu_handler,
    })
  }
}

impl WrapContextMenuHandler for ContextMenuHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_context_menu_handler_t, Self>) {
    self.object = object;
  }
}

impl Rc for ContextMenuHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl Clone for ContextMenuHandlerBuilder {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc_impl = &mut *self.object;
      rc_impl.interface.add_ref();
      rc_impl
    };

    Self {
      object,
      context_menu_handler: self.context_menu_handler.clone(),
    }
  }
}

impl ImplContextMenuHandler for ContextMenuHandlerBuilder {
  fn get_raw(&self) -> *mut sys::_cef_context_menu_handler_t {
    self.object.cast()
  }
  fn on_before_context_menu(
    &self,
    browser: Option<&mut Browser>,
    frame: Option<&mut Frame>,
    params: Option<&mut ContextMenuParams>,
    model: Option<&mut MenuModel>,
  ) {
    if let Some(model) = model {
      model.clear();
    }

    let Some(request) = self.context_menu_handler.build_request(browser, frame, params) else {
      return;
    };

    let _ = self.context_menu_handler.tx.send(ContextMenuEvent { request });
  }
}
