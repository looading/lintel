use cef::{rc::*, *};
use tracing::error;

use crate::webview::{BrowserId, lifecycle::ClientLifecyeleImpl};
use std::ptr::null_mut;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(Clone)]
pub struct IcyRequestHandler {
  tx: UnboundedSender<RequestEvent>,
  pub(super) lifecycle: Option<std::sync::Arc<dyn ClientLifecyeleImpl>>,
  partition_key: Option<String>,
}

#[allow(unused)]
#[derive(Clone)]
pub enum RequestEvent {
  BeforeBrowse {
    browser_id: BrowserId,
    frame_id: String,
    request: Option<Request>,
    user_gesture: i32,
    is_redirect: bool,
  },
  DocumentAvailableInMainFrame {
    browser_id: BrowserId,
  },
  RenderProcessResponsive {
    browser_id: BrowserId,
  },
  RenderProcessUnresponsive {
    browser_id: BrowserId,
  },
  RenderProcessTerminated {
    browser_id: BrowserId,
    status: TerminationStatus,
    error_code: ::std::os::raw::c_int,
    error_string: Option<String>,
  },
}

impl RequestEvent {
  pub fn browser_id(&self) -> BrowserId {
    *match self {
      RequestEvent::BeforeBrowse { browser_id, .. } => browser_id,
      RequestEvent::DocumentAvailableInMainFrame { browser_id } => browser_id,
      RequestEvent::RenderProcessResponsive { browser_id } => browser_id,
      RequestEvent::RenderProcessUnresponsive { browser_id } => browser_id,
      RequestEvent::RenderProcessTerminated { browser_id, .. } => browser_id,
    }
  }
}

impl std::fmt::Debug for RequestEvent {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      RequestEvent::BeforeBrowse {
        browser_id,
        request,
        frame_id,
        user_gesture,
        is_redirect,
      } => f
        .debug_struct("BeforeBrowser")
        .field("browser_id", &browser_id)
        .field("frame_id", &frame_id)
        .field("url", &request.as_ref().map(|r| CefString::from(&r.url()).to_string()))
        .field("user_gesture", &(*user_gesture != 0))
        .field("is_redirect", &is_redirect)
        .finish(),
      RequestEvent::DocumentAvailableInMainFrame { browser_id } => f
        .debug_struct("DocumentAvailableInMainFrame")
        .field("browser_id", &browser_id)
        .finish(),
      RequestEvent::RenderProcessResponsive { browser_id } => f
        .debug_struct("RenderProcessResponsive")
        .field("browser_id", &browser_id)
        .finish(),
      RequestEvent::RenderProcessUnresponsive { browser_id } => f
        .debug_struct("RenderProcessUnresponsive")
        .field("browser_id", &browser_id)
        .finish(),
      RequestEvent::RenderProcessTerminated {
        browser_id,
        status,
        error_code,
        error_string,
      } => f
        .debug_struct("RenderProcessTerminated")
        .field("browser_id", &browser_id)
        .field("status", &status)
        .field("error_code", &error_code)
        .field("error_string", &error_string)
        .finish(),
    }
  }
}

impl IcyRequestHandler {
  pub fn new(
    lifecycle: Option<std::sync::Arc<dyn ClientLifecyeleImpl>>,
    partition_key: Option<String>,
  ) -> (Self, UnboundedReceiver<RequestEvent>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    (
      Self {
        tx,
        lifecycle,
        partition_key,
      },
      rx,
    )
  }
}

pub(crate) struct RequestHandlerBuilder {
  object: *mut RcImpl<sys::cef_request_handler_t, Self>,
  request_handler: IcyRequestHandler,
}

impl RequestHandlerBuilder {
  pub(crate) fn build(request_handler: IcyRequestHandler) -> RequestHandler {
    RequestHandler::new(Self {
      object: null_mut(),
      request_handler,
    })
  }
}

impl WrapRequestHandler for RequestHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::cef_request_handler_t, Self>) {
    self.object = object;
  }
}

impl Rc for RequestHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl Clone for RequestHandlerBuilder {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc_impl = &mut *self.object;
      rc_impl.interface.add_ref();
      rc_impl
    };

    Self {
      object,
      request_handler: self.request_handler.clone(),
    }
  }
}

impl ImplRequestHandler for RequestHandlerBuilder {
  fn get_raw(&self) -> *mut sys::cef_request_handler_t {
    self.object.cast()
  }

  fn on_before_browse(
    &self,
    browser: Option<&mut Browser>,
    frame: Option<&mut Frame>,
    request: Option<&mut Request>,
    _user_gesture: ::std::os::raw::c_int,
    is_redirect: ::std::os::raw::c_int,
  ) -> ::std::os::raw::c_int {
    let Some(request) = request else {
      return true as _;
    };
    let Some(browser) = browser else {
      return true as _;
    };
    let Some(frame) = frame else {
      return true as _;
    };
    let url = CefString::from(&request.url()).to_string();
    tracing::info!(browser_id = ?browser.identifier(), frame = ?CefString::from(&frame.identifier()), ?url, is_redirect = is_redirect==1, "before browse");
    if url.is_empty() {
      return true as _;
    }
    if self
      .request_handler
      .lifecycle
      .as_ref()
      .is_some_and(|policy| !policy.allows_navigation(&url, frame.is_main() == 1))
    {
      tracing::warn!(
        browser_id = browser.identifier(),
        "CEF navigation denied by the page policy"
      );
      return true as _;
    }
    false as _
  }

  fn on_render_view_ready(&self, browser: Option<&mut Browser>) {
    let Some(browser) = browser else {
      return;
    };
    tracing::info!(browser_id = ?browser.identifier(), "render view ready");
  }

  fn on_document_available_in_main_frame(&self, browser: Option<&mut Browser>) {
    let Some(browser) = browser else {
      return;
    };
    let browser_id = browser.identifier().into();
    _ = self
      .request_handler
      .tx
      .send(RequestEvent::DocumentAvailableInMainFrame { browser_id })
      .inspect_err(|err| {
        error!(?err, "cannot send on document avaliable in main frame");
      });
  }

  fn on_render_process_responsive(&self, browser: Option<&mut Browser>) {
    let Some(browser) = browser else {
      return;
    };
    let browser_id = browser.identifier().into();
    tracing::info!(?browser_id, "render process responsive");
    _ = self
      .request_handler
      .tx
      .send(RequestEvent::RenderProcessResponsive { browser_id })
      .inspect_err(|err| {
        error!(?err, "cannot send render process responsive event");
      });
  }

  fn on_render_process_unresponsive(
    &self,
    browser: Option<&mut Browser>,
    _callback: Option<&mut UnresponsiveProcessCallback>,
  ) -> ::std::os::raw::c_int {
    let Some(browser) = browser else {
      return false as _;
    };
    let browser_id = browser.identifier().into();
    tracing::warn!(?browser_id, "render process unresponsive");
    _ = self
      .request_handler
      .tx
      .send(RequestEvent::RenderProcessUnresponsive { browser_id })
      .inspect_err(|err| {
        error!(?err, "cannot send render process unresponsive event");
      });
    false as _
  }

  fn on_render_process_terminated(
    &self,
    browser: Option<&mut Browser>,
    status: TerminationStatus,
    error_code: ::std::os::raw::c_int,
    error_string: Option<&CefString>,
  ) {
    let Some(browser) = browser else {
      return;
    };
    let browser_id = browser.identifier().into();
    tracing::warn!(?browser_id, "render process terminated");
    _ = self
      .request_handler
      .tx
      .send(RequestEvent::RenderProcessTerminated {
        browser_id,
        status,
        error_code,
        error_string: error_string.map(|err| err.to_string()),
      })
      .inspect_err(|err| {
        error!(?err, "cannot send render process terminated event");
      });
  }
}
