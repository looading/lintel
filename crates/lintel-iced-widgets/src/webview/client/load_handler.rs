use cef::{
  ImplLoadHandler, LoadHandler, WrapLoadHandler,
  rc::{Rc, RcImpl},
  sys, *,
};

use std::{ptr::null_mut, sync::Arc};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::webview::{BrowserId, lifecycle::ClientLifecyeleImpl};

#[derive(Debug)]
#[allow(unused)]
pub enum LoadEvent {
  Changed {
    browser_id: BrowserId,
    is_loading: bool,
    can_go_forward: bool,
    can_go_back: bool,
  },
  Start {
    browser_id: BrowserId,
    frame_id: String,
    transition_type: u32,
  },
  End {
    browser_id: BrowserId,
    frame_id: String,
    http_status_code: u16,
  },
  Error {
    browser_id: BrowserId,
    frame_id: String,
    error_code: i32,
    error_text: Option<String>,
    failed_url: Option<String>,
  },
}

#[derive(Clone)]
pub(crate) struct IcyLoadHandler {
  tx: UnboundedSender<LoadEvent>,
  lifecycle: Option<Arc<dyn ClientLifecyeleImpl>>,
}

impl IcyLoadHandler {
  pub fn new() -> (Self, UnboundedReceiver<LoadEvent>) {
    Self::with_lifecycle(None)
  }

  pub(crate) fn with_lifecycle(
    lifecycle: Option<Arc<dyn ClientLifecyeleImpl>>,
  ) -> (Self, UnboundedReceiver<LoadEvent>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (Self { tx, lifecycle }, rx)
  }

  fn publish_start(&self, event: LoadEvent, is_main_frame: bool) {
    // 与 JS IPC 来源回调处在 CEF 的同一 UI 序列，先撤销旧文档，再让 UI 观察加载元数据。
    if is_main_frame {
      if let Some(lifecycle) = &self.lifecycle {
        lifecycle.on_main_frame_load_start();
      }
    }
    if let Err(err) = self.tx.send(event) {
      tracing::error!(%err, "cannot send loading start event");
    }
  }
}

pub(crate) struct LoadHandlerBuilder {
  object: *mut RcImpl<sys::_cef_load_handler_t, Self>,
  load_handler: IcyLoadHandler,
}

impl LoadHandlerBuilder {
  pub(crate) fn build(loader: IcyLoadHandler) -> LoadHandler {
    LoadHandler::new(Self {
      object: null_mut(),
      load_handler: loader,
    })
  }
}

impl WrapLoadHandler for LoadHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_load_handler_t, Self>) {
    self.object = object;
  }
}

impl Rc for LoadHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl Clone for LoadHandlerBuilder {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc_impl = &mut *self.object;
      rc_impl.interface.add_ref();
      rc_impl
    };

    Self {
      object,
      load_handler: self.load_handler.clone(),
    }
  }
}

impl ImplLoadHandler for LoadHandlerBuilder {
  fn get_raw(&self) -> *mut sys::_cef_load_handler_t {
    self.object.cast()
  }

  fn on_loading_state_change(
    &self,
    browser: Option<&mut Browser>,
    is_loading: ::std::os::raw::c_int,
    can_go_back: ::std::os::raw::c_int,
    can_go_forward: ::std::os::raw::c_int,
  ) {
    let Some(browser) = browser else {
      return;
    };
    let event = LoadEvent::Changed {
      browser_id: browser.identifier().into(),
      is_loading: is_loading == 1,
      can_go_forward: can_go_forward == 1,
      can_go_back: can_go_back == 1,
    };
    tracing::info!(?event);
    if let Err(err) = self.load_handler.tx.send(event) {
      tracing::error!(%err, "cannot send loading state changing event");
    }
  }

  fn on_load_start(
    &self,
    browser: Option<&mut Browser>,
    frame: Option<&mut Frame>,
    transition_type: cef::TransitionType,
  ) {
    let Some(frame) = frame else {
      return;
    };
    let Some(browser) = browser else {
      return;
    };
    let event = LoadEvent::Start {
      browser_id: browser.identifier().into(),
      frame_id: CefString::from(&frame.identifier()).to_string(),
      transition_type: (*transition_type.as_ref()) as _,
    };

    tracing::info!(?event);
    self.load_handler.publish_start(event, frame.is_main() == 1);
  }

  fn on_load_end(
    &self,
    browser: Option<&mut Browser>,
    frame: Option<&mut Frame>,
    http_status_code: ::std::os::raw::c_int,
  ) {
    let Some(browser) = browser else {
      return;
    };
    let Some(frame) = frame else {
      return;
    };
    let event = LoadEvent::End {
      browser_id: browser.identifier().into(),
      frame_id: CefString::from(&frame.identifier()).to_string(),
      http_status_code: http_status_code as _,
    };
    tracing::info!(?event);
    if let Err(err) = self.load_handler.tx.send(event) {
      tracing::error!(%err, "cannot send loading end event");
    }
  }

  fn on_load_error(
    &self,
    browser: Option<&mut Browser>,
    frame: Option<&mut Frame>,
    error_code: cef::Errorcode,
    error_text: Option<&cef::CefString>,
    failed_url: Option<&cef::CefString>,
  ) {
    let Some(frame) = frame else {
      return;
    };
    let Some(browser) = browser else {
      return;
    };
    let frame_id = CefString::from(&frame.identifier()).to_string();
    let event = LoadEvent::Error {
      browser_id: browser.identifier().into(),
      frame_id,
      error_code: (*error_code.as_ref()) as _,
      error_text: error_text
        .map(cef::CefStringUtf8::from)
        .and_then(|s| s.as_str().map(|s| s.to_string())),
      failed_url: failed_url
        .map(cef::CefStringUtf8::from)
        .and_then(|s| s.as_str().map(|s| s.to_string())),
    };
    tracing::info!(?event);
    if let Err(err) = self.load_handler.tx.send(event) {
      tracing::error!(%err, "cannot send load start event");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
  };

  #[derive(Debug)]
  struct OrderingPolicy {
    receiver: Arc<Mutex<Option<UnboundedReceiver<LoadEvent>>>>,
    called: AtomicUsize,
  }
  impl ClientLifecyeleImpl for OrderingPolicy {
    fn on_main_frame_load_start(&self) {
      // 若实现先发送加载消息再通知，检查会直接观察到队列中的 Start 并失败。
      assert!(matches!(
        self.receiver.lock().unwrap().as_mut().unwrap().try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
      ));
      self.called.fetch_add(1, Ordering::AcqRel);
    }
  }

  fn start_event() -> LoadEvent {
    LoadEvent::Start {
      browser_id: BrowserId::new(17),
      frame_id: "test-frame".into(),
      transition_type: 0,
    }
  }

  #[test]
  fn main_frame_load_start_hook_precedes_async_loading_metadata() {
    let receiver = Arc::new(Mutex::new(None));
    let policy = Arc::new(OrderingPolicy {
      receiver: receiver.clone(),
      called: AtomicUsize::new(0),
    });
    let (handler, receive) = IcyLoadHandler::with_lifecycle(Some(policy.clone()));
    *receiver.lock().unwrap() = Some(receive);
    handler.publish_start(start_event(), true);
    assert_eq!(policy.called.load(Ordering::Acquire), 1);
    assert!(matches!(
      receiver.lock().unwrap().as_mut().unwrap().try_recv(),
      Ok(LoadEvent::Start { .. })
    ));
  }

  #[test]
  fn main_frame_load_start_hook_ignores_subframes_and_legacy_constructor_keeps_notifications() {
    let receiver = Arc::new(Mutex::new(None));
    let policy = Arc::new(OrderingPolicy {
      receiver: receiver.clone(),
      called: AtomicUsize::new(0),
    });
    let (handler, mut receive) = IcyLoadHandler::with_lifecycle(Some(policy.clone()));
    handler.publish_start(start_event(), false);
    assert_eq!(policy.called.load(Ordering::Acquire), 0);
    assert!(matches!(receive.try_recv(), Ok(LoadEvent::Start { .. })));
    let (legacy, mut receive) = IcyLoadHandler::new();
    legacy.publish_start(start_event(), true);
    assert!(matches!(receive.try_recv(), Ok(LoadEvent::Start { .. })));
  }
}
