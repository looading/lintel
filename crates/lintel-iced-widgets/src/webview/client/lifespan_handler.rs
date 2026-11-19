use std::{fmt::Debug, ptr::null_mut};

use cef::{ImplBrowser, ImplBrowserHost, ImplLifeSpanHandler, LifeSpanHandler, WrapLifeSpanHandler, rc::*, sys};
use tokio::sync::mpsc::{Receiver, Sender};

use crate::webview::{
  BrowserId,
  instance::{LaunchId, new_webview, remove_webview},
};

#[derive(Clone)]
pub enum LifeSpanEvent {
  Created {
    launch_id: LaunchId,
    browser_id: BrowserId,
    is_popup: bool,
  },
  Closed {
    browser_id: BrowserId,
  },
  CreatePopup {
    browser_id: BrowserId,
    target_url: String,
  },
}

impl Debug for LifeSpanEvent {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Closed { browser_id } => f
        .debug_struct("LifeSpanEvent::Closed")
        .field("browser_id", &browser_id)
        .finish(),
      Self::Created {
        launch_id,
        browser_id,
        is_popup,
      } => f
        .debug_struct("LifeSpanEvent::Created")
        .field("launch_id", &launch_id)
        .field("browser_id", &browser_id)
        .field("is_popup", &is_popup)
        .finish(),
      Self::CreatePopup { browser_id, .. } => f
        .debug_struct("LifeSpanEvent::CreatePopup")
        .field("browser_id", &browser_id)
        .finish(),
    }
  }
}

#[derive(Clone)]
pub struct IcyLifeSpanHandler {
  tx: Sender<LifeSpanEvent>,
  launch_id: std::cell::RefCell<LaunchId>,
}

impl IcyLifeSpanHandler {
  pub fn new(launch_id: LaunchId) -> (Self, Receiver<LifeSpanEvent>) {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    (
      Self {
        tx,
        launch_id: std::cell::RefCell::new(launch_id),
      },
      rx,
    )
  }
}

impl LifeSpanHandlerBuilder {
  pub fn build(handler: IcyLifeSpanHandler) -> LifeSpanHandler {
    LifeSpanHandler::new(Self {
      object: null_mut(),
      handler,
    })
  }
}

pub(crate) struct LifeSpanHandlerBuilder {
  object: *mut RcImpl<sys::cef_life_span_handler_t, Self>,
  handler: IcyLifeSpanHandler,
}

impl Rc for LifeSpanHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl WrapLifeSpanHandler for LifeSpanHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_life_span_handler_t, Self>) {
    self.object = object;
  }
}

impl Clone for LifeSpanHandlerBuilder {
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

impl ImplLifeSpanHandler for LifeSpanHandlerBuilder {
  fn get_raw(&self) -> *mut sys::_cef_life_span_handler_t {
    self.object.cast()
  }

  fn on_after_created(&self, browser: Option<&mut cef::Browser>) {
    let Some(browser) = browser else {
      return;
    };
    if let Some(host) = browser.host() {
      host.set_focus(false as _);
    }

    let browser_id = browser.identifier().into();
    let is_popup = browser.is_popup() == 1;
    let launch_id = *self.handler.launch_id.borrow();
    let event = LifeSpanEvent::Created {
      launch_id,
      browser_id,
      is_popup,
    };
    tracing::info!(?event, "browser created in cef");
    if self
      .handler
      .tx
      .try_send(event)
      .inspect_err(|err| {
        tracing::warn!(?browser_id, is_popup, %err, "cannot send life span event");
      })
      .is_ok()
    {
      new_webview(launch_id, browser_id);
    }
  }

  fn on_before_close(&self, browser: Option<&mut cef::Browser>) {
    let Some(browser) = browser else {
      return;
    };
    let browser_id = browser.identifier().into();

    remove_webview(browser_id);
    let is_popup = browser.is_popup() == 1;
    tracing::info!(?browser_id, "browser closed");
    if let Err(err) = self.handler.tx.try_send(LifeSpanEvent::Closed { browser_id }) {
      tracing::warn!(?browser_id, is_popup, %err, "cannot send life span event");
    }
  }

  fn on_before_popup(
    &self,
    browser: Option<&mut cef::Browser>,
    _frame: Option<&mut cef::Frame>,
    popup_id: ::std::os::raw::c_int,
    target_url: Option<&cef::CefString>,
    _target_frame_name: Option<&cef::CefString>,
    _target_disposition: cef::WindowOpenDisposition,
    user_gesture: ::std::os::raw::c_int,
    _popup_features: Option<&cef::PopupFeatures>,
    _window_info: Option<&mut cef::WindowInfo>,
    _client: Option<&mut Option<cef::Client>>,
    _settings: Option<&mut cef::BrowserSettings>,
    _extra_info: Option<&mut Option<cef::DictionaryValue>>,
    _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
  ) -> ::std::os::raw::c_int {
    tracing::info!(popup_id, user_gesture, "new popup browser");
    if let Some(browser) = browser
      && let Some(target_url) = target_url.map(|url| url.to_string())
    {
      // let launch_id = LaunchId::unique();
      // let (icy_client, handlers) = IcyClient::new(IcyClientConfig {
      //   launch_id,
      //   device_scale_factor: 2.0,
      //   // FIXME: implement default rect
      //   view_rect: cef::Rect {
      //     width: 100,
      //     height: 100,
      //     ..Default::default()
      //   },
      //   screen: None,
      //   as_js_runner: false,
      //   preload: None,
      // });
      // let IcyClient { state, subscribers } = icy_client;

      // if let Some(window_info) = window_info {
      //   *window_info = cef::WindowInfo {
      //     windowless_rendering_enabled: true as _,
      //     shared_texture_enabled: true as _,
      //     runtime_style: cef::sys::cef_runtime_style_t::CEF_RUNTIME_STYLE_ALLOY.into(),
      //     ..Default::default()
      //   };
      // }
      // if let Some(client) = client {
      //   *client = ClientBuilder::build(handlers).into_raw();
      // }
      // if let Some(state) = state {
      //   new_browser(launch_id, state);
      // }
      // if let Some(no_js_access) = no_javascript_access {
      //   *no_js_access = false as _;
      // }

      // Todo：后面再考虑是否优化
      if let Err(err) = self.handler.tx.try_send(LifeSpanEvent::CreatePopup {
        browser_id: browser.identifier().into(),
        // subscribers: std::sync::Arc::new(AtomicTake::new(subscribers)),
        target_url,
      }) {
        tracing::warn!(%err, "cannot send life span event");
      }

      tracing::info!(popup_id, "popup to be created");

      // 取消创建popup
      return true as _;
    }
    true as _
  }

  fn on_before_popup_aborted(&self, browser: Option<&mut cef::Browser>, popup_id: ::std::os::raw::c_int) {
    let browser_id: Option<BrowserId> = browser.map(|b| b.identifier().into());
    tracing::warn!(?browser_id, popup_id, "popup aborted");
  }

  fn on_before_dev_tools_popup(
    &self,
    _browser: Option<&mut cef::Browser>,
    _window_info: Option<&mut cef::WindowInfo>,
    _client: Option<&mut Option<cef::Client>>,
    _settings: Option<&mut cef::BrowserSettings>,
    _extra_info: Option<&mut Option<cef::DictionaryValue>>,
    use_default_window: Option<&mut ::std::os::raw::c_int>,
  ) {
    tracing::info!("open devtools window");
    if let Some(default_window) = use_default_window {
      *default_window = true as _;
    }
  }
}
