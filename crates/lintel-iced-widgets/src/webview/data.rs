use cef::{
  CefString, CefStringUtf16, DictionaryValue, Frame, ImplDictionaryValue, ImplFrame, ImplListValue, ImplProcessMessage,
  ImplV8Value, V8Handler, V8Propertyattribute, V8Value,
  rc::{Rc, RcImpl},
  sys::{self},
  v8_value_create_string,
};
use lintel_lib_utils::prost::from_bytes;
use lintel_shared_data::{LintelInvokeAsyncParams, LintelInvokeAsyncResponse, LintelPushPayload, TransferData};
use serde::{Deserialize, Serialize, de};
use std::slice;

use crate::webview::{IcyV8HandlerBuilder, PreloadType, v8::V8ExceptionExt};

pub const CHANNEL_INVOKE_JS: &str = "__rust_invoke_js__";

// The legacy JavaScript helpers only read the optional error field. Other
// fields in the full protobuf envelope remain wire-compatible and are skipped.
#[derive(Clone, PartialEq, prost::Message)]
struct LegacyPackedError {
  #[prost(string, optional, tag = "5")]
  error: Option<String>,
}

/// 调用 js 方法的返回值
#[derive(Deserialize, Serialize, Default, Debug)]
pub struct RustInvokeResponse {
  pub data: Option<TransferData>,
  pub success: bool,
  pub error: Option<String>,
}

#[derive(Default)]
pub struct XapiAccessor {
  pub object: *mut RcImpl<sys::_cef_v8_accessor_t, Self>,
}

impl cef::rc::Rc for XapiAccessor {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl Clone for XapiAccessor {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc = &mut *self.object;
      rc.interface.add_ref();
      self.object
    };
    Self { object }
  }
}

impl cef::ImplV8Accessor for XapiAccessor {
  fn get_raw(&self) -> *mut sys::_cef_v8_accessor_t {
    self.object.cast()
  }
}

impl cef::WrapV8Accessor for XapiAccessor {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_v8_accessor_t, Self>) {
    self.object = object;
  }
}

#[derive(Default)]
pub struct XAPIInterceptor {
  pub object: *mut RcImpl<sys::_cef_v8_interceptor_t, Self>,
}

impl cef::rc::Rc for XAPIInterceptor {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl cef::ImplV8Interceptor for XAPIInterceptor {
  fn get_raw(&self) -> *mut sys::_cef_v8_interceptor_t {
    self.object.cast()
  }
}

impl cef::WrapV8Interceptor for XAPIInterceptor {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_v8_interceptor_t, Self>) {
    self.object = object;
  }
}

impl Clone for XAPIInterceptor {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc = &mut *self.object;
      rc.interface.add_ref();
      self.object
    };
    Self { object }
  }
}

pub struct XapiValue {
  pub object: Option<cef::V8Value>,
}

impl XapiValue {
  #[inline]
  #[must_use]
  #[allow(dead_code)]
  pub fn key() -> &'static str {
    "__X_BRIDGE__"
  }
}

impl XapiValue {
  /// for rust
  pub fn invoke_service(
    &self,
    name: &str,
    params: Option<V8Value>,
    frame: Option<Frame>,
    context_id: String,
  ) -> anyhow::Result<()> {
    if let Some(object) = self.object.as_ref() {
      let services = ServicesValue::from(object.value_bykey(Some(&ServicesValue::key().into())));
      services.invoke_service(name, params, frame, context_id);
    }
    Ok(())
  }
}

pub struct XAPIValueBuilder {
  pub object: Option<cef::V8Value>,
  pub options: DeskOptions,
}

impl From<Option<cef::V8Value>> for XapiValue {
  fn from(object: Option<cef::V8Value>) -> Self {
    Self { object }
  }
}

impl XAPIValueBuilder {
  pub fn new() -> Self {
    let mut api_accessor = cef::V8Accessor::new(XapiAccessor::default());
    let mut api_interceptor = cef::V8Interceptor::new(XAPIInterceptor::default());
    let object = cef::v8_value_create_object(Some(&mut api_accessor), Some(&mut api_interceptor));
    Self {
      object,
      options: DeskOptions::default(),
    }
  }
}

impl XAPIValueBuilder {
  pub fn with_set_value_bykey(
    &mut self,
    key: Option<&CefString>,
    value: Option<&mut V8Value>,
    attribute: V8Propertyattribute,
  ) {
    if let Some(object) = self.object.as_mut() {
      object.set_value_bykey(key, value, attribute);
    }
  }

  fn set_method_register_service(&mut self) {
    // set servicemap
    let mut service = ServicesBuilder::new().build();
    self.with_set_value_bykey(
      Some(&ServicesValue::key().into()),
      service.object.as_mut(),
      cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.into(),
    );

    // set register service
    let mut register_service_handler = IcyV8HandlerBuilder::build(move |name, _this, args| {
      if let Some(Some(name)) = args.first()
        && let Some(Some(service_fn)) = args.get(1)
        && name.is_string() == 1
        && service_fn.is_function() == 1
      {
        let name = CefStringUtf16::from(&name.string_value()).to_string();
        let name = name.as_str();
        let f = service_fn.clone();

        if let Err(err) = service.register_service(name, Some(f)) {
          eprintln!("register service failed: {err}");
        }
      }

      cef::v8_value_create_null().ok_or_else(|| anyhow::anyhow!("cannot create null"))
    });
    let mut register_service =
      cef::v8_value_create_function(Some(&"register_service".into()), Some(&mut register_service_handler));

    self.with_set_value_bykey(
      Some(&"registerService".into()),
      register_service.as_mut(),
      cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.into(),
    );
  }

  // 提供给 js 调用，用于解码 lintel .invoke 方法的返回值
  // 返回 [bool, arraybuffer, string] | undefined
  fn set_lintel_invoke_response_decode(&mut self) {
    let mut lintel_invoke_response_decode_handler = IcyV8HandlerBuilder::build(move |_name, _this, args| {
      if let Some(Some(response)) = args.first()
        && response.is_array_buffer() == 1
      {
        let payload = unsafe {
          std::slice::from_raw_parts(response.array_buffer_data().cast(), response.array_buffer_byte_length())
        };

        let ret =
          TransferData::cbor_decode(payload).and_then(|LintelInvokeAsyncResponse { has_error, mut payload }| {
            cef::v8_value_create_array(2)
              .ok_or_else(|| anyhow::anyhow!("cannot create array"))
              .and_then(|f| {
                f.set_value_byindex(0, cef::v8_value_create_bool(has_error as _).as_mut());

                let mut data = cef::v8_value_create_array_buffer_with_copy(payload.as_mut_ptr(), payload.len());
                f.set_value_byindex(1, data.as_mut());

                let error = if has_error {
                  let req = from_bytes::<LegacyPackedError>(&payload)
                    .map_err(|err| anyhow::anyhow!("packedMessage req error: {err:?}"))?;
                  req.error().to_string()
                } else {
                  "".into()
                };

                f.set_value_byindex(2, cef::v8_value_create_string(Some(&error.as_str().into())).as_mut());

                Ok(f)
              })
          });
        return ret;
      }

      Err(anyhow::anyhow!("lintel_invoke_response_decode error"))
    });
    let mut lintel_invoke_response_decode = cef::v8_value_create_function(
      Some(&"lintel_invoke_response_decode".into()),
      Some(&mut lintel_invoke_response_decode_handler),
    );
    // lintel invoke response decode
    self.with_set_value_bykey(
      Some(&"pird".into()),
      lintel_invoke_response_decode.as_mut(),
      cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.into(),
    );
  }

  // 提供给 js 调用，用于编码 lintel .invoke 方法的参数
  // 输入 (client_id, method, params)
  // 返回 arraybuffer
  fn set_lintel_invoke_request_encode(&mut self) {
    let mut lintel_invoke_request_encode_handler = IcyV8HandlerBuilder::build(move |_name, _this, args| {
      if let Some(Some(client_id)) = args.first()
        && let Some(Some(method)) = args.get(1)
        && let Some(Some(params)) = args.get(2)
        && client_id.is_string() == 1
        && method.is_string() == 1
        && params.is_array_buffer() == 1
      {
        let payload: Vec<u8> = unsafe {
          std::slice::from_raw_parts(params.array_buffer_data().cast(), params.array_buffer_byte_length()).to_vec()
        };

        let client_id = CefStringUtf16::from(&client_id.string_value()).to_string();
        let command_id = CefStringUtf16::from(&method.string_value()).to_string();

        let req = LintelInvokeAsyncParams {
          client_id,
          command_id,
          payload,
        };

        let ret = TransferData::from_cbor(req)
          .and_then(|data| data.cbor_value().ok_or(anyhow::anyhow!("cbor_value error")))
          .and_then(|mut req| {
            cef::v8_value_create_array_buffer_with_copy(req.as_mut_ptr(), req.len())
              .ok_or_else(|| anyhow::anyhow!("cannot create array buffer"))
          });

        return ret;
      }
      Err(anyhow::anyhow!("lintel_invoke_request_encode error"))
    });
    let mut lintel_invoke_request_encode = cef::v8_value_create_function(
      Some(&"lintel_invoke_request_encode".into()),
      Some(&mut lintel_invoke_request_encode_handler),
    );
    // lintel invoke request encode
    self.with_set_value_bykey(
      Some(&"pire".into()),
      lintel_invoke_request_encode.as_mut(),
      cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.into(),
    );
  }

  // 提供给 js 调用，用于编码 lintel .invoke 方法的参数
  // 输入 (client_id, method, params)
  // 返回 arraybuffer
  fn set_lintel_packed_message_get_error(&mut self) {
    let mut lintel_packed_message_get_error_handler = IcyV8HandlerBuilder::build(move |_name, _this, args| {
      if let Some(Some(params)) = args.first()
        && params.is_array_buffer() == 1
      {
        let payload =
          unsafe { std::slice::from_raw_parts(params.array_buffer_data().cast(), params.array_buffer_byte_length()) };

        let ret = TransferData::prost_decode::<LegacyPackedError>(payload);

        let ret = ret.and_then(|ret| {
          cef::v8_value_create_string(Some(&ret.error().into())).ok_or(anyhow::anyhow!("cannot create string"))
        });
        return ret;
      }
      Err(anyhow::anyhow!("cannot create undefined"))
    });
    let mut lintel_packed_message_get_error = cef::v8_value_create_function(
      Some(&"lintel_packed_message_get_error".into()),
      Some(&mut lintel_packed_message_get_error_handler),
    );
    // lintel packed message get error
    self.with_set_value_bykey(
      Some(&"ppmge".into()),
      lintel_packed_message_get_error.as_mut(),
      cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.into(),
    );
  }

  // 提供给 lintel push data decode
  fn set_lintel_push_decode(&mut self) {
    let mut lintel_push_decode_handler = IcyV8HandlerBuilder::build(move |_name, _this, args| {
      if let Some(Some(response)) = args.first()
        && response.is_array_buffer() == 1
      {
        let payload = unsafe {
          std::slice::from_raw_parts(response.array_buffer_data().cast(), response.array_buffer_byte_length())
        };

        // 序列化失败了
        let LintelPushPayload { client_id, mut payload } = TransferData::cbor_decode(payload)?;
        let f = cef::v8_value_create_array(2).ok_or(anyhow::anyhow!("create array failed."))?;
        f.set_value_byindex(
          0,
          cef::v8_value_create_string(Some(&client_id.as_str().into())).as_mut(),
        );

        let mut data = cef::v8_value_create_array_buffer_with_copy(payload.as_mut_ptr(), payload.len());
        f.set_value_byindex(1, data.as_mut());
        return Ok(f);
      }

      Err(anyhow::anyhow!(
        "lintel_push_decode error, response is not a arraybuffer"
      ))
    });
    let mut lintel_push_decode = cef::v8_value_create_function(
      Some(&"lintel_push_decode".into()),
      Some(&mut lintel_push_decode_handler),
    );
    // piggeon push decode
    self.with_set_value_bykey(
      Some(&"ppd".into()),
      lintel_push_decode.as_mut(),
      cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.into(),
    );
  }

  fn set_desk_options(&mut self) {
    self.with_set_value_bykey(
      Some(&"appVersion".into()),
      v8_value_create_string(Some(&self.options.version.as_str().into())).as_mut(),
      cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.into(),
    );

    self.with_set_value_bykey(
      Some(&"partitionKey".into()),
      v8_value_create_string(Some(&self.options.partition_key.as_str().into())).as_mut(),
      cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.into(),
    );

    self.with_set_value_bykey(
      Some(&"deviceId".into()),
      v8_value_create_string(Some(&self.options.device_id.as_str().into())).as_mut(),
      cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.into(),
    );
  }

  pub fn with_desk_options(mut self, options: DeskOptions) -> Self {
    self.options = options;
    self
  }

  pub fn build(mut self) -> XapiValue {
    self.set_method_register_service();
    self.set_lintel_invoke_response_decode();
    self.set_lintel_invoke_request_encode();
    self.set_lintel_push_decode();
    self.set_lintel_packed_message_get_error();
    self.set_desk_options();
    XapiValue::from(self.object)
  }
}

#[derive(Clone, Debug, Default)]
pub struct DeskOptions {
  pub version: String,
  pub partition_key: String,
  pub device_id: String,
}

#[derive(Clone)]
pub struct ServicesValue {
  pub object: Option<cef::V8Value>,
}

impl ServicesValue {
  #[inline]
  #[must_use]
  #[allow(dead_code)]
  pub fn key() -> &'static str {
    "__SERVICES__"
  }
}

impl ServicesValue {
  pub fn set_value_bykey(&self, key: Option<&CefString>, value: Option<&mut V8Value>, attribute: V8Propertyattribute) {
    if let Some(object) = self.object.as_ref() {
      object.set_value_bykey(key, value, attribute);
    }
  }

  pub fn register_service(&self, name: &str, service: Option<V8Value>) -> anyhow::Result<()> {
    if let Some(mut service) = service {
      if service.is_function() == 0 {
        anyhow::bail!("service is not function: {name}");
      }

      self.set_value_bykey(
        Some(&name.into()),
        Some(&mut service),
        cef::sys::cef_v8_propertyattribute_t::V8_PROPERTY_ATTRIBUTE_READONLY.into(),
      );
    }
    Ok(())
  }

  pub fn invoke_service(&self, name: &str, args: Option<V8Value>, frame: Option<Frame>, context_id: String) {
    let callback = ServicePromiseCallback::new(frame, context_id);
    let success_callback =
      cef::v8_value_create_function(Some(&"success".into()), Some(&mut callback.create_success_callback()));
    let error_callback =
      cef::v8_value_create_function(Some(&"error".into()), Some(&mut callback.create_error_callback()));

    let error_callback_ = error_callback.clone();
    let fail = |error_msg: &str| {
      let error_msg = cef::v8_value_create_string(Some(&error_msg.into()));
      error_callback_.map(|f| f.execute_function(None, Some(std::slice::from_ref(&error_msg))));
    };

    if let Some(object) = self.object.as_ref() {
      let service = object.value_bykey(Some(&name.into()));

      if let Some(service) = service
        && service.is_function() == 1
      {
        if let Some(ret) = service.execute_function(None, Some(&[args])) {
          if ret.is_promise() == 1 {
            let mut promise = ret.clone();
            let _ = ret.value_bykey(Some(&"then".into())).map(move |t| {
              // .then call must has reference to a Promise object
              t.execute_function(Some(&mut promise), Some(&[success_callback]));
            });

            let mut promise = ret.clone();
            let _ = ret.value_bykey(Some(&"catch".into())).map(move |t| {
              // .catch call must has reference to a Promise object
              t.execute_function(Some(&mut promise), Some(&[error_callback]));
            });
          } else {
            fail(&format!("service return not promise: {name}"))
          }
        } else {
          fail("service execute_function failed.")
        }
      } else {
        fail(&format!("service not found: {name}"))
      }
    } else {
      fail(&format!("service not found: {name}"))
    }
  }
}

impl From<Option<cef::V8Value>> for ServicesValue {
  fn from(object: Option<cef::V8Value>) -> Self {
    Self { object }
  }
}

pub struct ServicesBuilder {
  pub object: Option<cef::V8Value>,
}

impl ServicesBuilder {
  pub fn new() -> Self {
    let mut api_accessor = cef::V8Accessor::new(XapiAccessor::default());
    let mut api_interceptor = cef::V8Interceptor::new(XAPIInterceptor::default());
    let object = cef::v8_value_create_object(Some(&mut api_accessor), Some(&mut api_interceptor));
    Self { object }
  }
}

impl ServicesBuilder {
  pub fn build(self) -> ServicesValue {
    ServicesValue::from(self.object)
  }
}

#[derive(Clone)]
pub struct ServicePromiseCallback {
  frame: Option<Frame>,
  context_id: String,
}

impl ServicePromiseCallback {
  pub fn new(frame: Option<Frame>, context_id: String) -> Self {
    Self { frame, context_id }
  }
}

impl ServicePromiseCallback {
  #[inline]
  #[must_use]
  #[allow(dead_code)]
  pub fn channel_key() -> &'static str {
    "renderer.rust_invoke_response"
  }
}

impl ServicePromiseCallback {
  pub fn create_success_callback(&self) -> V8Handler {
    // 这很重要 -_-
    let frame = self.frame.clone();
    let context_id = self.context_id.clone();
    IcyV8HandlerBuilder::build(move |_, _this, args| {
      let frame = frame.as_ref().ok_or(anyhow::anyhow!("frame is none"))?;
      let mut message = cef::process_message_create(Some(&ServicePromiseCallback::channel_key().into()))
        .ok_or(anyhow::anyhow!("cannot create message"))?;

      let list = message
        .argument_list()
        .ok_or(anyhow::anyhow!("cannot get argument list"))?;

      let mut res = RustInvokeResponse {
        success: true,
        ..Default::default()
      };

      let arg = args
        .first()
        .ok_or(anyhow::anyhow!("args is empty"))?
        .as_ref()
        .ok_or(anyhow::anyhow!("args is empty"))?;

      if arg.is_string() == 1 {
        let arg = arg.string_value();
        let arg = CefStringUtf16::from(&arg);
        let arg = arg.to_string();
        res.data = Some(TransferData::String(arg));
      }

      if arg.is_array_buffer() == 1 {
        let arr = arg.array_buffer_data();
        let len = arg.array_buffer_byte_length();

        // copy
        let data: Vec<u8> = unsafe { slice::from_raw_parts(arr.cast(), len).to_vec() };
        res.data = Some(TransferData::Prost(data));
      }

      let mut value = cef::binary_value_create(Some(lintel_lib_utils::serde::cbor4ii_to_vec(&res)?.as_slice()));
      list.set_string(0, Some(&context_id.as_str().into()));
      list.set_binary(1, value.as_mut());
      frame.send_process_message(cef::sys::cef_process_id_t::PID_BROWSER.into(), Some(&mut message));

      cef::v8_value_create_null().ok_or_else(|| anyhow::anyhow!("cannot create null"))
    })
  }

  pub fn create_error_callback(&self) -> V8Handler {
    // 这很重要 -_-
    let frame = self.frame.clone();
    let context_id = self.context_id.clone();
    IcyV8HandlerBuilder::build(move |_, _this, args| {
      let mut res = RustInvokeResponse {
        success: false,
        ..Default::default()
      };

      if let Some(frame) = frame.as_ref()
        && let Some(mut message) = cef::process_message_create(Some(&ServicePromiseCallback::channel_key().into()))
        && let Some(list) = message.argument_list()
      {
        if let Some(Some(arg)) = args.first() {
          if arg.is_string() == 1 {
            let arg = arg.string_value();
            let arg = CefStringUtf16::from(&arg);
            let arg = arg.to_string();

            // let err = RustInvokeResponseError::Other(arg);
            res.error = Some(arg);
            // maybe send failed
          }

          if let Some(exception) = arg.exception() {
            let error = exception.fmt_message();
            res.error = Some(error);
          }
        }

        let res = lintel_lib_utils::serde::cbor4ii_to_vec(&res)?;
        let mut value = cef::binary_value_create(Some(res.as_slice()));

        list.set_string(0, Some(&context_id.as_str().into()));
        list.set_binary(1, value.as_mut());

        frame.send_process_message(cef::sys::cef_process_id_t::PID_BROWSER.into(), Some(&mut message));
      }

      cef::v8_value_create_null().ok_or_else(|| anyhow::anyhow!("cannot create null"))
    })
  }
}

#[derive(Debug, Clone)]
pub struct BrowerCreateExtraInfo {
  pub app_version: String,
  pub partition_key: String,
  pub preload_type: PreloadType,
  pub device_id: String,
}

impl BrowerCreateExtraInfo {
  pub fn new(app_version: String, partition_key: String, preload_type: PreloadType, device_id: String) -> Self {
    Self {
      app_version,
      partition_key,
      preload_type,
      device_id,
    }
  }
}

impl From<&mut DictionaryValue> for BrowerCreateExtraInfo {
  fn from(value: &mut DictionaryValue) -> Self {
    let app_version = dictionary_string(value, "version");
    let partition_key = dictionary_string(value, "partition_key");
    let preload_type = PreloadType::from(dictionary_string(value, "preload_type").as_str());
    let device_id = dictionary_string(value, "device_id");
    Self {
      app_version,
      partition_key,
      preload_type,
      device_id,
    }
  }
}

fn dictionary_string(dictionary: &DictionaryValue, key: &str) -> String {
  nullable_userfree_string(&dictionary.string(Some(&key.into())))
}

/// CEF 可用空 userfree 指针表示空字符串。先判断空值，避免把它交给要求非空的复制构造。
fn nullable_userfree_string(value: &cef::CefStringUserfree) -> String {
  let raw: Option<&sys::_cef_string_utf16_t> = value.into();
  if raw.is_none() {
    return String::new();
  }
  CefString::from(value).to_string()
}

impl From<BrowerCreateExtraInfo> for Option<DictionaryValue> {
  fn from(val: BrowerCreateExtraInfo) -> Self {
    let dict = cef::dictionary_value_create();

    #[allow(clippy::manual_inspect)]
    dict.map(|dict| {
      dict.set_string(Some(&"version".into()), Some(&val.app_version.as_str().into()));
      dict.set_string(Some(&"partition_key".into()), Some(&val.partition_key.as_str().into()));
      dict.set_string(
        Some(&"preload_type".into()),
        Some(&val.preload_type.to_string().as_str().into()),
      );
      dict.set_string(
        Some(&"device_id".into()),
        Some(&val.device_id.to_string().as_str().into()),
      );
      dict
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn empty_optional_userfree_string_is_read_without_copying_a_null_cef_pointer() {
    let value = cef::CefStringUserfree::default();
    let raw: Option<&sys::_cef_string_utf16_t> = (&value).into();
    assert!(raw.is_none());
    assert_eq!(nullable_userfree_string(&value), "");
  }

  #[test]
  fn reads_legacy_protobuf_error_with_other_fields_present() {
    // Legacy tags 3 (request), 4 (status), 5 (error) and 6 (code).
    let envelope = [0x1a, 2, b'o', b'k', 0x20, 0, 0x2a, 3, b'b', b'a', b'd', 0x30, 1];
    assert_eq!(from_bytes::<LegacyPackedError>(&envelope).unwrap().error(), "bad");
    assert_eq!(from_bytes::<LegacyPackedError>(&[0x20, 1]).unwrap().error(), "");
  }

  #[test]
  fn generic_push_roundtrip_preserves_typed_json_payload_and_outer_transfer() {
    let expected = serde_json::json!({"kind":"update", "count":7, "label":"示例 🌰"});
    let payload = TransferData::from_json(&expected).unwrap().to_bytes().unwrap();
    let push = LintelPushPayload::new("example-client".into(), payload.to_vec());
    let envelope = TransferData::from_cbor(push).unwrap();
    let wire = envelope.to_bytes().unwrap();
    let restored = TransferData::from_bytes(wire).unwrap();
    let decoded: LintelPushPayload = restored.cbor().unwrap();
    assert_eq!(decoded.client_id, "example-client");
    let actual: serde_json::Value = TransferData::from_bytes(decoded.payload.into())
      .unwrap()
      .json()
      .unwrap();
    assert_eq!(actual, expected);
  }

  #[test]
  fn generic_push_cbor_codec_preserves_binary_and_empty_payloads() {
    for payload in [Vec::new(), vec![0, 1, 127, 128, 255]] {
      let encoded =
        TransferData::cbor_encode(LintelPushPayload::new("example-client".into(), payload.clone())).unwrap();
      let decoded: LintelPushPayload = TransferData::cbor_decode(&encoded).unwrap();
      assert_eq!(decoded.client_id, "example-client");
      assert_eq!(decoded.payload, payload);
    }
  }

  #[test]
  fn generic_push_codec_rejects_truncated_or_incompatible_envelopes() {
    let encoded = TransferData::cbor_encode(LintelPushPayload::new("example-client".into(), vec![0, 255])).unwrap();
    assert!(TransferData::cbor_decode::<LintelPushPayload>(&encoded[..encoded.len() - 1]).is_err());
    let incompatible = TransferData::cbor_encode(serde_json::json!({"client_id":4, "payload":false})).unwrap();
    assert!(TransferData::cbor_decode::<LintelPushPayload>(&incompatible).is_err());
  }
}
