use std::{path::Path, ptr::null_mut, sync::Arc};

use cef::{
  Browser, Callback, CefString, Frame, ImplRequest, ImplResourceRequestHandler, Request, ResourceRequestHandler,
  ReturnValue, WrapResourceRequestHandler,
  rc::{Rc, RcImpl},
  sys::{self, cef_resource_type_t, cef_return_value_t},
};

use crate::webview::lifecycle::ClientLifecyeleImpl;

pub(crate) struct ResourceRequestHandlerBuilder {
  object: *mut RcImpl<sys::cef_resource_request_handler_t, Self>,
  enable_safe_link: bool,
  resource_policy: Option<Arc<dyn ClientLifecyeleImpl>>,
}

impl ResourceRequestHandlerBuilder {
  pub(crate) fn build(
    enable_safe_link: bool,
    resource_policy: Option<Arc<dyn ClientLifecyeleImpl>>,
  ) -> ResourceRequestHandler {
    ResourceRequestHandler::new(Self {
      object: null_mut(),
      enable_safe_link,
      resource_policy,
    })
  }
}

impl WrapResourceRequestHandler for ResourceRequestHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::cef_resource_request_handler_t, Self>) {
    self.object = object;
  }
}

impl Rc for ResourceRequestHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    // SAFETY: CEF initializes this wrapper in wrap_rc and holds a live reference.
    unsafe { &(*self.object).cef_object.base }
  }
}

impl Clone for ResourceRequestHandlerBuilder {
  fn clone(&self) -> Self {
    // SAFETY: A live wrapper holds a reference to the initialized CEF object.
    unsafe { (*self.object).interface.add_ref() };
    Self {
      object: self.object,
      enable_safe_link: self.enable_safe_link,
      resource_policy: self.resource_policy.clone(),
    }
  }
}

impl ImplResourceRequestHandler for ResourceRequestHandlerBuilder {
  fn get_raw(&self) -> *mut sys::_cef_resource_request_handler_t {
    self.object.cast()
  }

  fn on_before_resource_load(
    &self,
    _browser: Option<&mut Browser>,
    _frame: Option<&mut Frame>,
    request: Option<&mut Request>,
    _callback: Option<&mut Callback>,
  ) -> ReturnValue {
    let Some(request) = request else {
      return cef_return_value_t::RV_CANCEL.into();
    };
    let resource_type = request.resource_type();
    let main_frame = resource_type.as_ref() == &cef_resource_type_t::RT_MAIN_FRAME;
    if self.resource_policy.is_some() || (self.enable_safe_link && main_frame) {
      let url = CefString::from(&request.url()).to_string();
      if let Some(policy) = &self.resource_policy {
        let navigation = main_frame || resource_type.as_ref() == &cef_resource_type_t::RT_SUB_FRAME;
        if !policy.allows_resource(&url) || (navigation && !policy.allows_navigation(&url, main_frame)) {
          tracing::warn!("CEF resource denied by the page policy");
          return cef_return_value_t::RV_CANCEL.into();
        }
      }
      if self.enable_safe_link
        && main_frame
        && !allows_local_main_frame(&url, lintel_lib_env::LintelEnv::resources_path())
      {
        tracing::warn!("blocked main-frame navigation outside the application resources");
        return cef_return_value_t::RV_CANCEL.into();
      }
    }
    cef_return_value_t::RV_CONTINUE.into()
  }
}

fn allows_local_main_frame(raw_url: &str, resource_dir: &Path) -> bool {
  if raw_url == "about:blank" {
    return true;
  }
  let Ok(url) = url::Url::parse(raw_url) else {
    return false;
  };
  if url.host_str().is_some() {
    return false;
  }
  let Ok(path) = url.to_file_path() else {
    return false;
  };
  let (Ok(resource_dir), Ok(path)) = (resource_dir.canonicalize(), path.canonicalize()) else {
    return false;
  };
  path.starts_with(resource_dir)
}

#[cfg(test)]
mod tests {
  use super::allows_local_main_frame;
  use std::path::Path;

  #[test]
  fn safe_link_fallback_accepts_application_files_and_blank_page() {
    let resource_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let local_url = url::Url::from_file_path(resource_dir.join("Cargo.toml")).unwrap();
    assert!(allows_local_main_frame(local_url.as_str(), resource_dir));
    assert!(allows_local_main_frame("about:blank", resource_dir));
  }

  #[test]
  fn safe_link_fallback_rejects_remote_invalid_and_outside_resources() {
    let resource_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let outside_url = url::Url::from_file_path(resource_dir.join("../../Cargo.toml")).unwrap();
    for url in [
      outside_url.as_str(),
      "https://example.test",
      "file://fileserver/share/document.html",
      "data:text/html,Hello",
      "invalid",
    ] {
      assert!(!allows_local_main_frame(url, resource_dir), "{url}");
    }
  }
}
