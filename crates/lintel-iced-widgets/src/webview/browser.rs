use cef::{
  BrowserProcessHandler, ImplBrowserProcessHandler, WrapBrowserProcessHandler,
  rc::{Rc, RcImpl},
  sys, *,
};
use std::{
  cell::RefCell,
  time::{Duration, Instant},
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

use crate::webview::init::BrowserProcessMessage;

// Keep Chromium's normal origin, TLS, popup and credential protections.
const BROWSER_DEFAULT_SWITCHES: &[&str] = &[
  "disable-gaia-services",
  "disable-javascript-open-windows",
  "disable-chrome-login-prompt",
  "no-startup-window",
  "noerrdialogs",
  "hide-crash-restore-bubble",
  "force-light-mode",
  "disable-spell-checking",
  "enable-main-frame-before-activation",
  "disable-session-crashed-bubble",
];
const CHILD_DEFAULT_SWITCHES: &[&str] = &[
  "disable-javascript-open-windows",
  "hide-crash-restore-bubble",
  "disable-chrome-login-prompt",
  "disable-session-crashed-bubble",
];

#[cfg(test)]
mod default_switch_tests {
  use super::*;

  #[test]
  fn cef_default_switches_keep_origin_tls_popup_and_credential_protections() {
    let defaults: Vec<_> = BROWSER_DEFAULT_SWITCHES
      .iter()
      .chain(CHILD_DEFAULT_SWITCHES)
      .copied()
      .collect();
    for bypass in [
      "disable-web-security",
      "allow-running-insecure-content",
      "ignore-certificate-errors",
      "ignore-ssl-errors",
      "disable-popup-blocking",
      "use-mock-keychain",
      "disable-gpu-sandbox",
      "disable-stack-profiler",
    ] {
      assert!(!defaults.contains(&bypass), "default switches must not add {bypass}");
    }
    for required in [
      "no-startup-window",
      "disable-javascript-open-windows",
      "force-light-mode",
    ] {
      assert!(defaults.contains(&required), "preserve OSR/UI behavior {required}");
    }
  }
}

// TODO: split non ui code to lintel-plugin-cef
#[derive(Clone, Debug)]
pub struct IcyCefApp {}

impl IcyCefApp {
  pub fn new() -> Self {
    Self {}
  }
}

pub(crate) struct AppBuilder {
  object: *mut RcImpl<cef::sys::_cef_app_t, Self>,
  app: IcyCefApp,
  browser_handler: cef::BrowserProcessHandler,
}

impl AppBuilder {
  pub(crate) fn build(app: IcyCefApp, browser_handler: IcyBrowserProcessHandler) -> cef::App {
    cef::App::new(Self {
      object: std::ptr::null_mut(),
      app,
      browser_handler: BrowserProcessHandlerBuilder::build(browser_handler),
    })
  }
}

impl WrapApp for AppBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<cef::sys::_cef_app_t, Self>) {
    self.object = object;
  }
}

impl Clone for AppBuilder {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc = &mut *self.object;
      rc.interface.add_ref();
      self.object
    };
    Self {
      object,
      app: self.app.clone(),
      browser_handler: self.browser_handler.clone(),
    }
  }
}

impl Rc for AppBuilder {
  fn as_base(&self) -> &cef::sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl ImplApp for AppBuilder {
  fn get_raw(&self) -> *mut cef::sys::_cef_app_t {
    self.object as *mut cef::sys::_cef_app_t
  }

  fn on_before_command_line_processing(
    &self,
    process_type: Option<&cef::CefStringUtf16>,
    command_line: Option<&mut cef::CommandLine>,
  ) {
    if let Some(process_type) = process_type {
      tracing::info!(
        process_type = cef::CefStringUtf8::from(process_type).to_string(),
        "process launching",
      );
    }
    let Some(command_line) = command_line else {
      tracing::error!("no command line");
      return;
    };

    for &switch in BROWSER_DEFAULT_SWITCHES {
      command_line.append_switch(Some(&switch.into()));
    }
    command_line.append_switch_with_value(Some(&"num-raster-threads".into()), Some(&"4".into()));
    command_line.append_switch_with_value(Some(&"lang".into()), Some(&"zh-CN".into()));
    command_line.append_switch_with_value(Some(&"locale".into()), Some(&"zh_CN".into()));

    #[cfg(target_vendor = "win7")]
    {
      command_line.append_switch(Some(&"disable-gpu".into()));
      command_line.append_switch(Some(&"disable-gpu-compositing".into()));
    }

    #[cfg(not(debug_assertions))]
    {
      // ensure debugging port is not set in release mode
      command_line.remove_switch(Some(&"remote-debugging-port".into()));
    }

    #[cfg(debug_assertions)]
    {
      // DevTools uses the in-process API without a default listening TCP port.
      command_line.append_switch(Some(&"enable-logging".into()));
    }
    let disabled_features = command_line.switch_value(Some(&"disable-features".into()));
    let disabled_features =
      "AutoCloseFileDialogs,OptimizationGuideOnDeviceModel,OnDeviceModelService,GCM,SpareRendererForSitePerProcess"
        .to_string()
        + ","
        + cef::CefString::from(&disabled_features).to_string().as_str();

    command_line.append_switch_with_value(
      Some(&"disable-features".into()),
      Some(&disabled_features.as_str().into()),
    );

    tracing::info!("pre-set command line done");
  }

  fn browser_process_handler(&self) -> Option<cef::BrowserProcessHandler> {
    Some(self.browser_handler.clone())
  }
}

#[derive(Clone)]
pub struct IcyBrowserProcessHandler {
  cef_ready: std::rc::Rc<RefCell<bool>>,
  cef_loop_pump_tx: UnboundedSender<BrowserProcessMessage>,
  device_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChromiumGpuInfo {
  pub vendor: u32,
  pub device: u32,
  pub render: String,
  pub gl_implementation_parts: String,
}

impl IcyBrowserProcessHandler {
  pub fn new(
    device_id: Option<String>,
  ) -> (
    Self,
    UnboundedReceiver<BrowserProcessMessage>,
    oneshot::Receiver<ChromiumGpuInfo>,
  ) {
    let (tx, rx) = unbounded_channel();
    let (gpu_info_tx, gpu_info_rx) = oneshot::channel();
    // Official CEF has no GPU-info extension callback. Preserve the receiver shape and close it.
    drop(gpu_info_tx);
    (
      Self {
        cef_ready: std::rc::Rc::new(RefCell::new(false)),
        cef_loop_pump_tx: tx,
        device_id,
      },
      rx,
      gpu_info_rx,
    )
  }
}

pub(crate) struct BrowserProcessHandlerBuilder {
  object: *mut RcImpl<sys::cef_browser_process_handler_t, Self>,
  handler: IcyBrowserProcessHandler,
}

impl BrowserProcessHandlerBuilder {
  pub(crate) fn build(handler: IcyBrowserProcessHandler) -> BrowserProcessHandler {
    BrowserProcessHandler::new(Self {
      object: std::ptr::null_mut(),
      handler,
    })
  }
}

impl Rc for BrowserProcessHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl WrapBrowserProcessHandler for BrowserProcessHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_browser_process_handler_t, Self>) {
    self.object = object;
  }
}

impl Clone for BrowserProcessHandlerBuilder {
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

impl ImplBrowserProcessHandler for BrowserProcessHandlerBuilder {
  fn get_raw(&self) -> *mut sys::_cef_browser_process_handler_t {
    self.object.cast()
  }

  fn on_context_initialized(&self) {
    tracing::info!("cef context intialized");
    *self.handler.cef_ready.borrow_mut() = true;
  }

  fn on_before_child_process_launch(&self, command_line: Option<&mut CommandLine>) {
    let Some(command_line) = command_line else {
      return;
    };

    for &switch in CHILD_DEFAULT_SWITCHES {
      command_line.append_switch(Some(&switch.into()));
    }
    command_line.append_switch_with_value(
      Some(&"lintel-device-id".into()),
      self.handler.device_id.as_deref().map(|id| id.into()).as_ref(),
    );
  }

  fn on_schedule_message_pump_work(&self, delay_ms: i64) {
    if let Err(err) = self.handler.cef_loop_pump_tx.send(BrowserProcessMessage::Tick(
      Instant::now() + Duration::from_millis(delay_ms as _),
    )) {
      //tracing::warn!(%err, "cannot pump cef loop");
    }
  }
}
