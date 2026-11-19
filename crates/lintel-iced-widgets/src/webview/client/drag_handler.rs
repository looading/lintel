use cef::Browser;
use cef::DragHandler;
use cef::DragOperationsMask;
use cef::ImplDragHandler;
use cef::WrapDragHandler;
use cef::rc::*;
use cef::sys;

#[derive(Clone)]
pub(crate) struct IcyDragHandler {}

pub(crate) struct IcyDragHandlerBuilder {
  object: *mut RcImpl<cef::sys::cef_drag_handler_t, Self>,
  handler: IcyDragHandler,
}

impl IcyDragHandlerBuilder {
  pub(crate) fn build(handler: IcyDragHandler) -> DragHandler {
    DragHandler::new(Self {
      object: std::ptr::null_mut(),
      handler,
    })
  }
}

impl WrapDragHandler for IcyDragHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<cef::sys::_cef_drag_handler_t, Self>) {
    self.object = object;
  }
}

impl Rc for IcyDragHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl Clone for IcyDragHandlerBuilder {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc_impl = &mut *self.object;
      rc_impl.interface.add_ref();
      rc_impl
    };

    Self {
      object,
      handler: self.handler.clone(),
    }
  }
}

impl ImplDragHandler for IcyDragHandlerBuilder {
  fn get_raw(&self) -> *mut cef::sys::_cef_drag_handler_t {
    self.object.cast()
  }

  fn on_drag_enter(
    &self,
    _browser: Option<&mut Browser>,
    _drag_data: Option<&mut cef::DragData>,
    _mask: DragOperationsMask,
  ) -> ::std::os::raw::c_int {
    false as _
  }

  fn on_draggable_regions_changed(
    &self,
    _browser: Option<&mut Browser>,
    _frame: Option<&mut cef::Frame>,
    _regions: Option<&[cef::DraggableRegion]>,
  ) {
  }
}
