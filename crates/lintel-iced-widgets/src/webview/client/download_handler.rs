use cef::{rc::*, *};
use std::{fmt::Debug, ptr::null_mut};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{error, info};

use crate::webview::BrowserId;

#[derive(Clone)]
pub struct IcyDownloadHandler {
  tx: UnboundedSender<DownloadEvent>,
  partition_key: Option<String>,
}

#[allow(unused)]
#[derive(Clone)]
pub enum DownloadEvent {
  BeforeDownload {
    url: String,
    browser_id: BrowserId,
  },
  Progress {
    is_complete: bool,
    is_canceled: bool,
    is_in_progress: bool,
    percent_complete: i32,
    total_bytes: i64,
    received_bytes: i64,
    full_path: String,
    download_callback: DownloadItemCallback,
    browser_id: BrowserId,
  },
  CanDownload(BrowserId),
}

impl DownloadEvent {
  pub fn browser_id(&self) -> BrowserId {
    *match &self {
      Self::BeforeDownload { browser_id, .. } => browser_id,
      Self::Progress { browser_id, .. } => browser_id,
      Self::CanDownload(browser_id) => browser_id,
    }
  }
}

impl Debug for DownloadEvent {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::BeforeDownload { url, browser_id } => f.debug_struct("BeforeDownload").field("url", url).finish(),
      Self::Progress {
        is_complete,
        is_canceled,
        is_in_progress,
        percent_complete,
        total_bytes,
        received_bytes,
        full_path,
        download_callback,
        browser_id,
      } => f
        .debug_struct("Progress")
        .field("is_complete", is_complete)
        .field("is_canceled", is_canceled)
        .field("is_in_progress", is_in_progress)
        .field("percent_complete", percent_complete)
        .field("total_bytes", total_bytes)
        .field("received_bytes", received_bytes)
        .field("full_path", full_path)
        .finish(),
      Self::CanDownload(browser_id) => f.debug_struct("CanDownload").field("browser_id", browser_id).finish(),
    }
  }
}

impl IcyDownloadHandler {
  pub fn new(partition_key: Option<String>) -> (Self, UnboundedReceiver<DownloadEvent>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    (Self { tx, partition_key }, rx)
  }
}

pub(crate) struct DownloadHandlerBuilder {
  object: *mut RcImpl<sys::cef_download_handler_t, Self>,
  download_handler: IcyDownloadHandler,
}

impl DownloadHandlerBuilder {
  pub(crate) fn build(download_handler: IcyDownloadHandler) -> DownloadHandler {
    DownloadHandler::new(Self {
      object: null_mut(),
      download_handler,
    })
  }
}

impl WrapDownloadHandler for DownloadHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::cef_download_handler_t, Self>) {
    self.object = object;
  }
}

impl Rc for DownloadHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl Clone for DownloadHandlerBuilder {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc_impl = &mut *self.object;
      rc_impl.interface.add_ref();
      rc_impl
    };

    Self {
      object,
      download_handler: self.download_handler.clone(),
    }
  }
}

impl ImplDownloadHandler for DownloadHandlerBuilder {
  fn get_raw(&self) -> *mut sys::cef_download_handler_t {
    self.object.cast()
  }

  fn on_before_download(
    &self,
    browser: Option<&mut Browser>,
    download_item: Option<&mut DownloadItem>,
    suggested_name: Option<&CefString>,
    callback: Option<&mut BeforeDownloadCallback>,
  ) -> ::std::os::raw::c_int {
    info!(
      "[Download] on_before_download {} {}",
      download_item.is_some(),
      callback.is_some()
    );

    if let Some(callback) = callback
      && let Some(browser) = browser
    {
      let download_path = lintel_lib_env::LintelEnv::download_path()
        .join(
          suggested_name
            .map(|name: &CefStringUtf16| name.to_string())
            .unwrap_or_default(),
        )
        .display()
        .to_string();

      let url = download_item
        .as_ref()
        .map(|item| CefString::from(&item.url()).to_string())
        .unwrap_or_default();

      let _ = self
        .download_handler
        .tx
        .send(DownloadEvent::BeforeDownload {
          url,
          browser_id: browser.identifier().into(),
        })
        .inspect_err(|err| error!(?err, "[Download] send BeforeDownload event failed"));

      let download_path = CefString::from(download_path.as_str());
      callback.cont(Some(&download_path), 1);
    }
    1
  }

  fn can_download(
    &self,
    browser: Option<&mut Browser>,
    url: Option<&CefString>,
    request_method: Option<&CefString>,
  ) -> ::std::os::raw::c_int {
    let Some(browser) = browser else {
      return false as _;
    };
    info!(
      "[Download] can_download url: {:?}, request_method: {:?}",
      url.map(|u| u.to_string()),
      request_method.map(|m| m.to_string())
    );
    let _ = self
      .download_handler
      .tx
      .send(DownloadEvent::CanDownload(browser.identifier().into()))
      .inspect_err(|err| error!(?err, "[Download] send CanDownload event failed"));

    true as _
  }

  fn on_download_updated(
    &self,
    browser: Option<&mut Browser>,
    download_item: Option<&mut DownloadItem>,
    callback: Option<&mut DownloadItemCallback>,
  ) {
    let Some(browser) = browser else {
      return;
    };
    info!("[Download] updated {} {}", download_item.is_some(), callback.is_some());
    if let Some(callback) = callback
      && let Some(download_item) = download_item
    {
      let _ = self
        .download_handler
        .tx
        .send(DownloadEvent::Progress {
          browser_id: browser.identifier().into(),
          is_complete: download_item.is_complete() == 1,
          is_canceled: download_item.is_canceled() == 1,
          is_in_progress: download_item.is_in_progress() == 1,
          percent_complete: download_item.percent_complete(),
          total_bytes: download_item.total_bytes(),
          received_bytes: download_item.received_bytes(),
          full_path: CefString::from(&download_item.full_path()).to_string(),
          download_callback: callback.clone(),
        })
        .inspect_err(|err| error!(?err, "[Download]send Progress event failed"));
    }
  }
}
