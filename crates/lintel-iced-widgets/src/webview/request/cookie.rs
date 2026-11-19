use camino::Utf8Path;
use cef::rc::*;
use cef::sys;
use cef::*;

use std::cell::RefCell;
use std::ffi::c_int;
use std::path::Path;
use std::ptr::null_mut;
use tokio::sync::oneshot;

#[derive(Debug, Clone)]
pub struct CefCookie {
  pub name: String,
  pub value: String,
  pub domain: String,
  pub path: String,
  pub secure: bool,
  pub httponly: bool,
  pub creation: i64,
  pub last_access: i64,
  pub has_expires: bool,
  pub expires: i64,
  pub same_site: CookieSameSite,
  pub priority: CookiePriority,
}

impl CefCookie {
  pub fn into_raw(self) -> cef::Cookie {
    cef::Cookie {
      name: self.name.as_str().into(),
      value: self.value.as_str().into(),
      domain: self.domain.as_str().into(),
      path: self.path.as_str().into(),
      secure: self.secure as c_int,
      httponly: self.httponly as c_int,
      creation: cef::Basetime { val: self.creation },
      last_access: cef::Basetime { val: self.last_access },
      has_expires: self.has_expires as c_int,
      expires: cef::Basetime { val: self.expires },
      same_site: self.same_site,
      priority: self.priority,
      ..Default::default()
    }
  }
}

impl From<&cef::Cookie> for CefCookie {
  fn from(cookie: &cef::Cookie) -> Self {
    Self {
      name: cookie.name.to_string(),
      value: cookie.value.to_string(),
      domain: cookie.domain.to_string(),
      path: cookie.path.to_string(),
      secure: cookie.secure == 1,
      httponly: cookie.httponly == 1,
      creation: cookie.creation.val,
      last_access: cookie.last_access.val,
      has_expires: cookie.has_expires == 1,
      expires: cookie.expires.val,
      same_site: cookie.same_site,
      priority: cookie.priority,
    }
  }
}

#[derive(Clone)]
pub struct IcyCookieVisitor {
  cookies: std::rc::Rc<RefCell<Vec<CefCookie>>>,
  tx: std::rc::Rc<RefCell<Option<oneshot::Sender<Vec<CefCookie>>>>>,
}

impl IcyCookieVisitor {
  pub fn new(tx: oneshot::Sender<Vec<CefCookie>>) -> Self {
    Self {
      cookies: std::rc::Rc::new(RefCell::new(vec![])),
      tx: std::rc::Rc::new(RefCell::new(Some(tx))),
    }
  }
}

pub struct IcyCookieVisitorBuilder {
  object: *mut RcImpl<sys::cef_cookie_visitor_t, Self>,
  visitor: IcyCookieVisitor,
}

impl IcyCookieVisitorBuilder {
  pub(crate) fn build(visitor: IcyCookieVisitor) -> cef::CookieVisitor {
    cef::CookieVisitor::new(Self {
      object: null_mut(),
      visitor,
    })
  }
}

impl WrapCookieVisitor for IcyCookieVisitorBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_cookie_visitor_t, Self>) {
    self.object = object;
  }
}

impl Rc for IcyCookieVisitorBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl Clone for IcyCookieVisitorBuilder {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc_impl = &mut *self.object;
      rc_impl.interface.add_ref();
      rc_impl
    };

    Self {
      object,
      visitor: self.visitor.clone(),
    }
  }
}

impl cef::ImplCookieVisitor for IcyCookieVisitorBuilder {
  fn get_raw(&self) -> *mut sys::_cef_cookie_visitor_t {
    self.object.cast()
  }

  fn visit(
    &self,
    cookie: Option<&cef::Cookie>,
    count: ::std::os::raw::c_int,
    total: ::std::os::raw::c_int,
    _delete_cookie: Option<&mut ::std::os::raw::c_int>,
  ) -> ::std::os::raw::c_int {
    if total == 0
      && let Some(tx) = self.visitor.tx.borrow_mut().take()
    {
      if tx.send(vec![]).is_err() {
        tracing::warn!("cookie channel closed");
      }
      return false as _;
    }

    if let Some(cookie) = cookie {
      self.visitor.cookies.borrow_mut().push(cookie.into());
    }
    if count == total - 1
      && let Some(tx) = self.visitor.tx.borrow_mut().take()
    {
      let _ = tx.send(std::mem::take(&mut *self.visitor.cookies.borrow_mut()));
    }
    true as _
  }
}

macro_rules! define_icy_callback {
    (
        $(#[$struct_meta:meta])*
        name: $callback_name:ident,
        cef_type: $callback_type:ident,
        sys_type: $cef_type:ty,
        wrap_trait: $wrap_trait:ident,
        impl_trait: $impl_trait:ident,
        callback_sig: $fn_trait:path,
        methods: {
            $($method_def:tt)*
        }
    ) => {
        $(#[$struct_meta])*
        pub struct $callback_name<F: $fn_trait> {
            inner: std::rc::Rc<std::cell::RefCell<Option<F>>>,
            object: *mut RcImpl<$cef_type, Self>,
        }

        impl<F: $fn_trait> $callback_name<F> {
            pub fn build(inner: F) -> cef::$callback_type {
                cef::$callback_type::new(Self {
                    inner: std::rc::Rc::new(std::cell::RefCell::new(Some(inner))),
                    object: std::ptr::null_mut(),
                })
            }
        }

        impl<F: $fn_trait> $wrap_trait for $callback_name<F> {
            fn wrap_rc(&mut self, object: *mut RcImpl<$cef_type, Self>) {
                self.object = object;
            }
        }

        impl<F: $fn_trait> Rc for $callback_name<F> {
            fn as_base(&self) -> &cef::sys::cef_base_ref_counted_t {
                unsafe {
                    let base = &*self.object;
                    std::mem::transmute(&base.cef_object)
                }
            }
        }

        impl<F: $fn_trait> Clone for $callback_name<F> {
            fn clone(&self) -> Self {
                let object = unsafe {
                    let rc_impl = &mut *self.object;
                    rc_impl.interface.add_ref();
                    rc_impl
                };

                Self {
                    object,
                    inner: self.inner.clone(),
                }
            }
        }

        impl<F: $fn_trait> $impl_trait for $callback_name<F> {
            fn get_raw(&self) -> *mut $cef_type {
                self.object.cast()
            }

            $($method_def)*
        }
    };
}

define_icy_callback!(
    name: IcyCompletionCallback,
    cef_type: CompletionCallback,
    sys_type: sys::_cef_completion_callback_t,
    wrap_trait: WrapCompletionCallback,
    impl_trait: ImplCompletionCallback,
    callback_sig: FnOnce(),
    methods: {
        fn on_complete(&self) {
            let Some(func) = self.inner.borrow_mut().take() else {
                return;
            };
            (func)()
        }
    }
);

define_icy_callback!(
    name: IcySetCookieCallback,
    cef_type: SetCookieCallback,
    sys_type: sys::_cef_set_cookie_callback_t,
    wrap_trait: WrapSetCookieCallback,
    impl_trait: ImplSetCookieCallback,
    callback_sig: FnOnce(bool),
    methods: {
        fn on_complete(&self, success: c_int) {
            let Some(func) = self.inner.borrow_mut().take() else {
                return;
            };
            (func)(success == 1)
        }
    }
);

define_icy_callback!(
    name: IcyDeleteCookiesCallback,
    cef_type: DeleteCookiesCallback,
    sys_type: sys::_cef_delete_cookies_callback_t,
    wrap_trait: WrapDeleteCookiesCallback,
    impl_trait: ImplDeleteCookiesCallback,
    callback_sig: FnOnce(bool),
    methods: {
        fn on_complete(&self, success: c_int) {
            let Some(func) = self.inner.borrow_mut().take() else {
                return;
            };
            (func)(success == 1)
        }
    }
);

pub async fn get_cookies(session_path: &Path) -> Vec<CefCookie> {
  let (tx, rx) = tokio::sync::oneshot::channel();
  let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

  let mut complete_handler = IcyCompletionCallback::build(|| {
    let _ = ready_tx.send(true);
  });

  let context = {
    let settings = cef::RequestContextSettings {
      cache_path: session_path.to_string_lossy().to_string().as_str().into(),
      ..Default::default()
    };
    let Some(context) = cef::request_context_create_context(Some(&settings), None) else {
      return vec![];
    };
    context
  };
  {
    let Some(manager) = context.cookie_manager(Some(&mut complete_handler)) else {
      return vec![];
    };
    let _ = ready_rx.await.unwrap_or_default();

    let _ = manager.visit_all_cookies(Some(&mut IcyCookieVisitorBuilder::build(IcyCookieVisitor::new(tx))));
  }

  rx.await.unwrap_or_default()
}
