pub mod cookie;
pub mod resource_request_handler;

use cef::{
  CefString, ImplRequestContextHandler, ImplValue, RequestContextHandler, WrapRequestContextHandler,
  rc::{Rc, RcImpl},
  sys, *,
};

use std::{ptr::null_mut, sync::Arc};

use crate::webview::{
  lifecycle::ClientLifecyeleImpl, request::resource_request_handler::ResourceRequestHandlerBuilder,
};

#[derive(Clone)]
pub struct IcyRequestContextHandler {}

pub(crate) struct RequestContextHandlerBuilder {
  object: *mut RcImpl<sys::cef_request_context_handler_t, Self>,
  handler: IcyRequestContextHandler,
  enable_safe_link: bool,
  resource_policy: Option<Arc<dyn ClientLifecyeleImpl>>,
}

impl RequestContextHandlerBuilder {
  pub(crate) fn build(
    handler: IcyRequestContextHandler,
    enable_safe_link: bool,
    lifecycle: Option<Arc<dyn ClientLifecyeleImpl>>,
  ) -> RequestContextHandler {
    RequestContextHandler::new(Self {
      object: null_mut(),
      handler,
      enable_safe_link,
      resource_policy: lifecycle.filter(|policy| policy.restricts_resources()),
    })
  }
}

impl WrapRequestContextHandler for RequestContextHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_request_context_handler_t, Self>) {
    self.object = object;
  }
}

impl Rc for RequestContextHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl Clone for RequestContextHandlerBuilder {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc_impl = &mut *self.object;
      rc_impl.interface.add_ref();
      rc_impl
    };

    Self {
      object,
      handler: self.handler.clone(),
      enable_safe_link: self.enable_safe_link,
      resource_policy: self.resource_policy.clone(),
    }
  }
}

impl ImplRequestContextHandler for RequestContextHandlerBuilder {
  fn get_raw(&self) -> *mut sys::_cef_request_context_handler_t {
    self.object.cast()
  }

  fn on_request_context_initialized(&self, request_context: Option<&mut cef::RequestContext>) {
    const DISABLE_CRED: &str = "credentials_enable_service";
    // https://source.chromium.org/chromium/chromium/src/+/main:chrome/browser/themes/theme_service.h;l=60;drc=ebb226bf037f76c123aca243e97dd13886e8ee17
    const THEME1: &str = "browser.theme.color_scheme";
    const THEME2: &str = "browser.theme.color_scheme2";
    const FOLLOW: &str = "browser.theme.follows_system_colors";
    let Some(ctxt) = request_context else {
      return;
    };
    ctxt.set_chrome_color_scheme(cef::sys::cef_color_variant_t::CEF_COLOR_VARIANT_LIGHT.into(), true as _);

    if let Some(mut value) = cef::value_create() {
      // FIXME: better string alloc/free
      let error = unsafe { cef::sys::cef_string_userfree_utf16_alloc() };
      let mut error = CefString::from(error);
      value.set_bool(false as _);
      if ctxt.set_preference(Some(&CefString::from(DISABLE_CRED)), Some(&mut value), Some(&mut error)) != 1 {
        tracing::error!(key = DISABLE_CRED, "cannot set preference");
      }
    }

    if let Some(mut value) = cef::value_create() {
      // BrowserColorTheme::Light
      value.set_int(1);
      let error = unsafe { cef::sys::cef_string_userfree_utf16_alloc() };
      let mut error = CefString::from(error);

      if ctxt.set_preference(Some(&CefString::from(THEME1)), Some(&mut value), Some(&mut error)) != 1 {
        tracing::error!(key = THEME1, "cannot set preference");
      }
    }
    if let Some(mut value) = cef::value_create() {
      // BrowserColorTheme::Light
      value.set_int(1);
      let error = unsafe { cef::sys::cef_string_userfree_utf16_alloc() };
      let mut error = CefString::from(error);

      if ctxt.set_preference(Some(&CefString::from(THEME2)), Some(&mut value), Some(&mut error)) != 1 {
        tracing::error!(key = THEME2, "cannot set preference");
      }
    }

    if let Some(mut value) = cef::value_create() {
      value.set_bool(false as _);
      let error = unsafe { cef::sys::cef_string_userfree_utf16_alloc() };
      let mut error = CefString::from(error);

      if ctxt.set_preference(Some(&CefString::from(FOLLOW)), Some(&mut value), Some(&mut error)) != 1 {
        tracing::error!(key = FOLLOW, "cannot set preference");
      }
    }

    // 为所有 origin 默认开启 clipboard content setting (CONTENT_SETTING_ALLOW = 1)。
    //
    // 背景：
    // - Chromium 源码确认，document.execCommand("paste") 的可读性判断除了
    //   BrowserSettings.javascript_access_clipboard / javascript_dom_paste 外，
    //   还会走 ContentSettingsClient::AllowReadFromClipboard()
    // - 因此 profile content settings 中的
    //   profile.content_settings.exceptions.clipboard 会直接影响页面能否读取剪贴板
    // - 同时，Blink 在 <textarea> 等非 richly editable 场景下还存在
    //   PasteMode::kPlainTextOnly 过滤逻辑，该逻辑会只保留 text/plain
    //
    // 这里的配置用于解决“clipboard read gate”问题，不把它表述为
    // “直接绕过 textarea 图片类型限制”，因为图片类型是否最终暴露到
    // clipboardData.items / files，还取决于具体的 paste event / beforeinput 链路。
    //
    // 现网现象上，这个配置可以修复部分页面通过 execCommand("paste") 无法读取截图图片的问题。
    //
    // 注意：
    // - BrowserSettings 只决定 JS 是否具备调用 paste 相关能力，不能替代 content setting
    // - Ctrl+V / 菜单粘贴走 kMenuOrKeyBinding 路径，Chromium 会直接放行读取判断
    // - Chromium 108 的行为更宽松，升级到 Chromium 142 后需要显式补齐该配置
    const CLIPBOARD_PREF: &str = "profile.content_settings.exceptions.clipboard";
    if let Some(mut value) = cef::value_create() {
      if let Some(mut origin_dict) = cef::dictionary_value_create() {
        if let Some(mut setting_dict) = cef::dictionary_value_create() {
          setting_dict.set_int(Some(&"setting".into()), 1);
          origin_dict.set_dictionary(Some(&"*,*".into()), Some(&mut setting_dict));
        }
        value.set_dictionary(Some(&mut origin_dict));
      }
      let error = unsafe { cef::sys::cef_string_userfree_utf16_alloc() };
      let mut error = CefString::from(error);

      if ctxt.set_preference(
        Some(&CefString::from(CLIPBOARD_PREF)),
        Some(&mut value),
        Some(&mut error),
      ) != 1
      {
        tracing::error!(key = CLIPBOARD_PREF, "cannot set clipboard preference");
      }
    }
  }

  fn resource_request_handler(
    &self,
    _browser: Option<&mut Browser>,
    _frame: Option<&mut Frame>,
    _request: Option<&mut Request>,
    _is_navigation: ::std::os::raw::c_int,
    _is_download: ::std::os::raw::c_int,
    _request_initiator: Option<&CefString>,
    _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
  ) -> Option<ResourceRequestHandler> {
    (self.enable_safe_link || self.resource_policy.is_some())
      .then(|| ResourceRequestHandlerBuilder::build(self.enable_safe_link, self.resource_policy.clone()))
  }
}
