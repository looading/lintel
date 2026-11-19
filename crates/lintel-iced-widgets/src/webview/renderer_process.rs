use std::{
  collections::{HashMap, HashSet},
  fmt::Debug,
  hash::Hash,
  ptr,
  sync::atomic::{AtomicBool, Ordering},
  time::Instant,
};

use atomic_refcell::AtomicRefCell;
use bytes::Bytes;
use cef::{
  rc::*,
  sys::{_cef_v8_exception_t, cef_v8_exception_t},
  *,
};

use lintel_lib_utils::{
  perf::ExecCost,
  serde::{cbor4ii_from_slice, cbor4ii_to_vec},
};
use lintel_shared_data::TransferData;
use tracing::error;

use crate::webview::{
  BrowserId, PreloadType,
  data::{BrowerCreateExtraInfo, CHANNEL_INVOKE_JS, DeskOptions, XAPIValueBuilder, XapiValue},
  v8::{V8ExceptionExt, eval_js},
};

use super::v8::IcyV8HandlerBuilder;

pub struct ChildProcessApp {
  object: *mut RcImpl<sys::_cef_app_t, Self>,
  renderer_handler: RenderProcessHandler,
}

#[derive(Debug, Clone, Eq)]
#[allow(dead_code)]
struct Invoke {
  context_id: String,
  name: String,
  browser_id: i32,
  frame_id: String,
}

impl std::hash::Hash for Invoke {
  fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
    self.context_id.hash(state);
    self.name.hash(state);
    state.finish();
  }
}

impl PartialEq for Invoke {
  fn eq(&self, other: &Self) -> bool {
    self.context_id == other.context_id && self.name == other.name
  }
}

#[derive(Clone)]
struct Callbacks {
  v8_context: cef::V8Context,
  success: Option<cef::V8Value>,
  error: Option<cef::V8Value>,
}

impl std::fmt::Debug for Callbacks {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("JsCallbacks")
      .field("has_success", &self.success.is_some())
      .field("has_error", &self.error.is_some())
      .finish()
  }
}

impl ChildProcessApp {
  #[allow(clippy::new_ret_no_self)]
  pub fn new(app_version: &'static str) -> App {
    App::new(Self {
      object: std::ptr::null_mut(),
      renderer_handler: IcyRenderProcessHandlerBuilder::build(IcyRenderProcessHandler {
        javascript_invocations: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        app_version,
        extra_infos: std::rc::Rc::new(std::cell::RefCell::new(HashMap::new())),
      }),
    })
  }
}

impl WrapApp for ChildProcessApp {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_app_t, Self>) {
    self.object = object;
  }
}

impl Clone for ChildProcessApp {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc_impl = &mut *self.object;
      rc_impl.interface.add_ref();
      self.object
    };

    Self {
      object,
      renderer_handler: self.renderer_handler.clone(),
    }
  }
}

impl Rc for ChildProcessApp {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl ImplApp for ChildProcessApp {
  fn get_raw(&self) -> *mut sys::_cef_app_t {
    self.object as *mut sys::_cef_app_t
  }

  fn render_process_handler(&self) -> Option<RenderProcessHandler> {
    Some(self.renderer_handler.clone())
  }
}

#[derive(Clone)]
struct IcyRenderProcessHandler {
  javascript_invocations: std::sync::Arc<std::sync::Mutex<HashMap<Invoke, Callbacks>>>,
  app_version: &'static str,
  extra_infos: std::rc::Rc<std::cell::RefCell<HashMap<i32, BrowerCreateExtraInfo>>>,
}

impl IcyRenderProcessHandler {
  fn insert(&self, id: Invoke, callbacks: Callbacks) {
    let std::result::Result::Ok(mut lock) = self.javascript_invocations.lock() else {
      return;
    };
    lock.insert(id, callbacks);
  }

  fn remove(&self, id: &Invoke) -> Option<Callbacks> {
    let Ok(mut lock) = self.javascript_invocations.lock() else {
      return None;
    };
    lock.remove(id)
  }

  fn remove_browser_invocations(&self, id: i32) {
    let Ok(mut lock) = self.javascript_invocations.lock() else {
      return;
    };
    lock.retain(|k, _| k.browser_id != id);
  }

  fn set_browser_extra_info(&self, browser_id: i32, extra_info: BrowerCreateExtraInfo) {
    self.extra_infos.borrow_mut().insert(browser_id, extra_info);
  }

  fn get_browser_extra_info(&self, browser_id: i32) -> Option<BrowerCreateExtraInfo> {
    self.extra_infos.borrow().get(&browser_id).cloned()
  }

  fn remove_browser_extra_info(&self, browser_id: i32) -> Option<BrowerCreateExtraInfo> {
    self.extra_infos.borrow_mut().remove(&browser_id)
  }
}

struct IcyRenderProcessHandlerBuilder {
  object: *mut RcImpl<sys::cef_render_process_handler_t, Self>,
  handler: IcyRenderProcessHandler,
}

impl IcyRenderProcessHandlerBuilder {
  fn build(handler: IcyRenderProcessHandler) -> RenderProcessHandler {
    RenderProcessHandler::new(Self {
      object: std::ptr::null_mut(),
      handler,
    })
  }
}

impl Rc for IcyRenderProcessHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl WrapRenderProcessHandler for IcyRenderProcessHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_render_process_handler_t, Self>) {
    self.object = object;
  }
}

impl Clone for IcyRenderProcessHandlerBuilder {
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

impl ImplRenderProcessHandler for IcyRenderProcessHandlerBuilder {
  fn get_raw(&self) -> *mut sys::_cef_render_process_handler_t {
    self.object.cast()
  }

  fn on_browser_created(&self, browser: Option<&mut Browser>, extra_info: Option<&mut DictionaryValue>) {
    let Some(browser) = browser else {
      return;
    };
    let browser_id = browser.identifier();
    let extra_info = extra_info.map(BrowerCreateExtraInfo::from);
    match extra_info {
      Some(extra_info) => {
        eprintln!(
          "render: create browser {browser_id}, preload={}, partition_key={}",
          extra_info.preload_type, extra_info.partition_key
        );
        self.handler.set_browser_extra_info(browser_id, extra_info);
      }
      None => {
        eprintln!("render: create browser {browser_id}, missing extra_info");
        self.handler.remove_browser_extra_info(browser_id);
      }
    }
  }

  fn on_web_kit_initialized(&self) {
    eprintln!("render: webkit init");
  }

  fn on_browser_destroyed(&self, browser: Option<&mut Browser>) {
    if let Some(id) = browser.map(|b| b.identifier()) {
      self.handler.remove_browser_invocations(id);
      let removed = self.handler.remove_browser_extra_info(id).is_some();
      eprintln!("render: destroy browser {id}, extra_info_removed={removed}");
    }
  }

  fn on_focused_node_changed(
    &self,
    _browser: Option<&mut Browser>,
    frame: Option<&mut Frame>,
    node: Option<&mut Domnode>,
  ) {
    let Some(frame) = frame else {
      return;
    };

    let Some(mut message) = cef::process_message_create(Some(&cef::CefString::from("renderer.focused_node_changed")))
    else {
      return;
    };
    let Some(args) = message.argument_list() else {
      return;
    };
    let is_editable = node.as_ref().map(|node| node.is_editable() == 1).unwrap_or_default();
    let bounds = node.as_ref().map(|node| node.element_bounds()).unwrap_or_default();
    let element = serde_json::to_string(
        &serde_json::json!({ "x": bounds.x, "y": bounds.y, "width": bounds.width, "height": bounds.height, "is_editable": is_editable }),
      )
      .ok();
    if args.set_string(
      0,
      element
        .as_ref()
        .map(|s| (&cef::CefStringUtf8::from(s.as_str())).into())
        .as_ref(),
    ) != 1
    {
      return;
    }

    frame.send_process_message(cef::sys::cef_process_id_t::PID_BROWSER.into(), Some(&mut message));
  }

  fn on_context_created(
    &self,
    browser: Option<&mut Browser>,
    frame: Option<&mut Frame>,
    context: Option<&mut V8Context>,
  ) {
    let cost = ExecCost::new();
    let Some(browser) = browser else {
      return;
    };
    let browser_id = browser.identifier();
    eprintln!("render: context created {browser_id}");

    let Some(context) = context else {
      return;
    };
    let Some(frame) = frame else {
      return;
    };
    let Some(extra_info) = self.handler.get_browser_extra_info(browser_id) else {
      eprintln!("render: context created {browser_id}, missing extra_info");
      return;
    };

    let frame_id = cef::CefString::from(&frame.identifier()).to_string();
    let frame_id_for_js = frame_id.clone();

    // js call rust entry
    let handler = self.handler.clone();
    let mut js_call_rust_handler = IcyV8HandlerBuilder::build(move |name, _this, args| {
      if name == "__rsAPI__"
        && let [fn_name, arg, callbacks @ ..] = args
        && let Some(fn_name) = fn_name.as_ref().filter(|v| v.is_string() == 1)
        && let Some(arg) = arg.as_ref().filter(|v| v.is_array_buffer() == 1 || v.is_string() == 1)
        && let Some(context) = v8_context_get_current_context()
      {
        let js_callbacks = Callbacks {
          v8_context: context.clone(),
          success: callbacks
            .get(0)
            .and_then(|v| v.as_ref().filter(|v: &&V8Value| v.is_function() == 1))
            .cloned(),
          error: callbacks
            .get(1)
            .and_then(|v| v.as_ref().filter(|v| v.is_function() == 1))
            .cloned(),
        };
        let uuid = uuid::Uuid::new_v4();
        let invoke_name = cef::CefString::from(&fn_name.string_value()).to_string();
        let invoke_id = Invoke {
          context_id: uuid.into(),
          name: invoke_name.clone(),
          browser_id,
          frame_id: frame_id_for_js.clone(),
        };
        handler.insert(invoke_id, js_callbacks);
        let Some(mut process_message) = cef::process_message_create(Some(&"__rsAPI__.request".into())) else {
          return Err(anyhow::anyhow!("cannot create process message"));
        };
        if let Some(args) = process_message.argument_list() {
          args.set_string(0, Some(&uuid.to_string().as_str().into()));
          args.set_string(1, Some(&(&fn_name.string_value()).into()));
          let buf = {
            let payload = if arg.is_string() == 1 {
              let p = CefString::from(&arg.string_value()).to_string();
              TransferData::String(p)
            } else {
              let p: Vec<u8> = unsafe {
                std::slice::from_raw_parts(arg.array_buffer_data().cast(), arg.array_buffer_byte_length()).to_vec()
              };
              TransferData::Cbor(p)
            };
            cbor4ii_to_vec(payload).map_err(|err| anyhow::anyhow!("cannot encode payload: {}", err.to_string()))?
          };
          args.set_binary(2, cef::binary_value_create(Some(buf.as_slice())).as_mut());
          let Some(frame) = context.frame() else {
            anyhow::bail!("no frame")
          };
          frame.send_process_message(
            cef::sys::cef_process_id_t::PID_BROWSER.into(),
            Some(&mut process_message),
          );
        }
        return cef::v8_value_create_null().ok_or_else(|| anyhow::anyhow!("cannot create null"));
      }
      cef::v8_value_create_null().ok_or_else(|| anyhow::anyhow!("cannot create null"))
    });

    let mut caret_handler = IcyV8HandlerBuilder::build(|name, _this, args| {
      if name != "caret_offset" {
        return cef::v8_value_create_null().ok_or_else(|| anyhow::anyhow!("cannot create null"));
      }
      let [Some(offset)] = args else {
        anyhow::bail!("no args");
      };
      let offset = offset.int_value();

      let Some(mut message) = cef::process_message_create(Some(&cef::CefString::from("renderer.caret_offset_changed")))
      else {
        anyhow::bail!("cannot create ipc message");
      };
      let Some(args) = message.argument_list() else {
        anyhow::bail!("no args");
      };
      let element = serde_json::to_string(&serde_json::json!({ "offset": offset })).ok();
      if args.set_string(
        0,
        element
          .as_ref()
          .map(|s| (&cef::CefStringUtf8::from(s.as_str())).into())
          .as_ref(),
      ) != 1
      {
        anyhow::bail!("cannot set payload");
      }

      let Some(context) = cef::v8_context_get_current_context() else {
        anyhow::bail!("no v8 context")
      };
      let Some(frame) = context.frame() else {
        anyhow::bail!("no frame")
      };
      frame.send_process_message(cef::sys::cef_process_id_t::PID_BROWSER.into(), Some(&mut message));

      cef::v8_value_create_null().ok_or_else(|| anyhow::anyhow!("cannot create v8 value"))
    });

    if let Some(global) = context.global() {
      context.enter();

      if let Some(mut app_version) =
        cef::v8_value_create_string(Some(&self.handler.app_version.to_string().as_str().into()))
      {
        global.set_value_bykey(
          Some(&"lintel_app_version".into()),
          Some(&mut app_version),
          cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.into(),
        );
      }

      let mut api_builder = XAPIValueBuilder::new().with_desk_options(DeskOptions {
        version: extra_info.app_version.clone(),
        partition_key: extra_info.partition_key.clone(),
        device_id: extra_info.device_id.clone(),
      });

      if let Some(mut caret_offset) =
        cef::v8_value_create_function(Some(&"caret_offset".into()), Some(&mut caret_handler))
      {
        global.set_value_bykey(
          Some(&"caret_offset".into()),
          Some(&mut caret_offset),
          cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.into(),
        );
      }

      // 提供给 js 调用的 rust 方法
      if let Some(mut js_call_rust) =
        cef::v8_value_create_function(Some(&"__rsAPI__".into()), Some(&mut js_call_rust_handler))
      {
        api_builder.with_set_value_bykey(
          Some(&"invoke".into()),
          Some(&mut js_call_rust),
          cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.into(),
        );
        global.set_value_bykey(
          Some(&"__rsAPI__".into()),
          Some(&mut js_call_rust),
          cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.into(),
        );
      }

      let api = api_builder.build();
      if let Some(mut api) = api.object {
        global.set_value_bykey(
          Some(&XapiValue::key().into()),
          Some(&mut api),
          cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.into(),
        );
      }

      context.exit();
    }
  }

  fn on_context_released(
    &self,
    browser: Option<&mut Browser>,
    _frame: Option<&mut Frame>,
    _context: Option<&mut V8Context>,
  ) {
    if let Some(browser) = browser {
      eprintln!("render: context released {}", browser.identifier());
    }
  }

  fn on_process_message_received(
    &self,
    browser: Option<&mut Browser>,
    frame: Option<&mut Frame>,
    _source_process: ProcessId,
    message: Option<&mut ProcessMessage>,
  ) -> ::std::os::raw::c_int {
    let cmd = cef::command_line_get_global();
    if let Some(message) = message
      && let Some(browser) = browser
      && let Some(frame) = frame
      && let Some(args) = message.argument_list()
    {
      let ipc_invoke = cef::CefString::from(&message.name()).to_string();
      match ipc_invoke.as_str() {
        "__rsAPI__.response" => {
          let context_id: String = cef::CefString::from(&args.string(0)).to_string();
          let method_name: String = cef::CefString::from(&args.string(1)).to_string();
          let invoke_id = Invoke {
            context_id,
            name: method_name,
            browser_id: browser.identifier(),
            frame_id: cef::CefString::from(&frame.identifier()).to_string(),
          };
          if let Some(Callbacks {
            v8_context,
            success,
            error,
          }) = self.handler.remove(&invoke_id)
          {
            let is_success = args.bool(3) == 1;
            let context = scopeguard::guard(v8_context, |ctx| {
              ctx.exit();
            });
            context.enter();
            let arg = if let Some(payload) = args.binary(2) {
              let mut data = vec![0; payload.size()];

              payload.data(Some(&mut data), 0);

              let ret = if let Ok(data) = TransferData::from_bytes(Bytes::from(data)) {
                let ret = match data {
                  TransferData::String(s) => cef::v8_value_create_string(Some(&s.as_str().into())),
                  TransferData::Prost(d) => {
                    cef::v8_value_create_array_buffer_with_copy(d.as_ptr() as *mut _, d.len() as _)
                  }
                  TransferData::Cbor(d) => {
                    cef::v8_value_create_array_buffer_with_copy(d.as_ptr() as *mut _, d.len() as _)
                  }
                };
                ret
              } else {
                cef::v8_value_create_undefined()
              };

              ret
            } else {
              // hack: construct an empty json string
              cef::v8_value_create_undefined()
            };

            match is_success {
              true => {
                if success.and_then(|s| s.execute_function(None, Some(&[arg]))).is_none() {
                  eprintln!("cannot execute success callback {invoke_id:?}");
                  return false as _;
                }
              }
              false => {
                if error.and_then(|e| e.execute_function(None, Some(&[arg]))).is_none() {
                  eprintln!("cannot execute error callback {invoke_id:?}");
                  return false as _;
                }
              }
            }
            // eprintln!("javascript callback executed {invoke_id:?}, is_success_callback: {is_success}");
          }
          return true as _;
        }

        // 响应 rust invoke js 调用
        CHANNEL_INVOKE_JS => {
          let context_id: String = cef::CefString::from(&args.string(0)).to_string();
          let method_name: String = cef::CefString::from(&args.string(1)).to_string();
          let _ = frame.v8_context().map(move |context| {
            context.enter();
            // 需要在 v8 中执行
            let args: Option<V8Value> = args.binary(2).and_then(|b| {
              let mut data = vec![0; b.size()];
              b.data(Some(&mut data), 0);
              let ret = cbor4ii_from_slice::<TransferData>(&data).unwrap_or_default();

              // serverice 接受的可能是 string 或 arraybuffer
              match ret {
                TransferData::String(s) => cef::v8_value_create_string(Some(&s.as_str().into())),
                TransferData::Prost(mut d) => cef::v8_value_create_array_buffer_with_copy(d.as_mut_ptr(), d.len()),
                TransferData::Cbor(mut d) => {
                  // array buffer 创建需要在 v8 context 中执行
                  cef::v8_value_create_array_buffer_with_copy(d.as_mut_ptr(), d.len())
                }
              }
            });

            // Browser-process calls should use WebviewApi::bare_api(BrowserId::new(browser.identifier())).

            let _ = context.global().map(move |global| {
              let api = XapiValue::from(global.value_bykey(Some(&XapiValue::key().into())));

              let res = api.invoke_service(method_name.as_str(), args, Some(frame.clone()), context_id.clone());

              if let Err(err) = res {
                eprintln!("__invoke_js__ res: {context_id:?} {method_name:?} {err:?}");
              }
            });
            context.exit();
          });

          return true as _;
        }
        "renderer.ipc.request" => {
          let Some(binary) = args.binary(0) else {
            return false as _;
          };
          if binary.raw_data().is_null() {
            return false as _;
          };
          let Ok(req) = postcard::from_bytes::<crate::webview::client::Request>(unsafe {
            std::slice::from_raw_parts(binary.raw_data().cast(), binary.size())
          }) else {
            return false as _;
          };
          if req.req_id.method == "get_renderer_pid"
            && let Some(mut message) = cef::process_message_create(Some(&cef::CefString::from("renderer.ipc.response")))
            && let Some(mut args) = message.argument_list()
          {
            let pid = std::process::id();
            let resp = crate::webview::client::Response {
              req_id: req.req_id,
              response: postcard::to_stdvec(&pid).unwrap_or_default(),
            };
            let mut binary = cef::binary_value_create(postcard::to_stdvec(&resp).ok().as_deref());
            if args.set_binary(0, binary.as_mut()) == 1 {
              frame.send_process_message(cef::sys::cef_process_id_t::PID_BROWSER.into(), Some(&mut message));
              return true as _;
            }
          }
        }
        call @ _ => {
          eprintln!("invalid ipc call from browser process: {call}");
        }
      }
    }
    false as _
  }

  fn on_uncaught_exception(
    &self,
    browser: Option<&mut Browser>,
    frame: Option<&mut Frame>,
    context: Option<&mut V8Context>,
    exception: Option<&mut V8Exception>,
    stack_trace: Option<&mut V8StackTrace>,
  ) {
    if let Some(frame) = frame {
      let url = CefString::from(&frame.url()).to_string();
      let error = exception.map(|e| e.fmt_message());

      error!(url, "uncaught exception: {:?}", error);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn build_extra_info(preload_type: PreloadType, partition_key: &str) -> BrowerCreateExtraInfo {
    BrowerCreateExtraInfo::new(
      "2.0.17".to_string(),
      partition_key.to_string(),
      preload_type,
      "device-id".to_string(),
    )
  }

  fn build_handler() -> IcyRenderProcessHandler {
    IcyRenderProcessHandler {
      javascript_invocations: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
      app_version: "2.0.17",
      extra_infos: std::rc::Rc::new(std::cell::RefCell::new(HashMap::new())),
    }
  }

  #[test]
  fn test_browser_extra_info_keeps_isolation_between_browsers() {
    let handler = build_handler();

    handler.set_browser_extra_info(1, build_extra_info(PreloadType::Ops, "ops-shop"));
    handler.set_browser_extra_info(2, build_extra_info(PreloadType::Menu, "menu-shop"));

    let ops_info = handler.get_browser_extra_info(1).unwrap();
    let menu_info = handler.get_browser_extra_info(2).unwrap();

    assert_eq!(ops_info.preload_type, PreloadType::Ops);
    assert_eq!(ops_info.partition_key, "ops-shop");
    assert_eq!(menu_info.preload_type, PreloadType::Menu);
    assert_eq!(menu_info.partition_key, "menu-shop");
  }

  #[test]
  fn test_remove_browser_extra_info_only_affects_target_browser() {
    let handler = build_handler();

    handler.set_browser_extra_info(1, build_extra_info(PreloadType::Ops, "ops-shop"));
    handler.set_browser_extra_info(2, build_extra_info(PreloadType::Menu, "menu-shop"));

    let removed = handler.remove_browser_extra_info(2).unwrap();

    assert_eq!(removed.preload_type, PreloadType::Menu);
    assert!(handler.get_browser_extra_info(2).is_none());
    assert_eq!(
      handler.get_browser_extra_info(1).unwrap().preload_type,
      PreloadType::Ops
    );
  }
}
