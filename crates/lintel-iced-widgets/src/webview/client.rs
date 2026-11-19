use crate::{
  post_task_async_io, post_task_sync_io,
  webview::{
    BrowserId, CefMessage, JsInvocation, WebviewApi,
    client::{
      download_handler::{DownloadHandlerBuilder, IcyDownloadHandler},
      focus_handler::{FocusHandlerBuilder, IcyFocusHandler},
      jsdialog_handler::{IcyJsdialogHandler, JsdialogHandlerBuilder},
      permission_handler::{IcyPermissionHandler, PermissionHandlerBuilder},
      request_handler::{IcyRequestHandler, RequestHandlerBuilder},
    },
    data::ServicePromiseCallback,
    instance::{DragData, JsCallRx},
    lifecycle::ClientLifecyeleImpl,
  },
};
use arc_swap::ArcSwapOption;
use atomic_take::AtomicTake;
use cef::{
  Client, ContextMenuHandler, DownloadHandler, ImplClient, ImplListValue, ImplProcessMessage, JsdialogHandler,
  KeyboardHandler, LifeSpanHandler, LoadHandler, PermissionHandler, RenderHandler, RequestHandler, WrapClient,
  rc::{Rc, RcImpl},
  sys,
};
use cef::{DisplayHandler, FocusHandler, ImplBinaryValue, ImplBrowser, ImplFrame};
use context_menu_handler::{ContextMenuEvent, ContextMenuHandlerBuilder, IcyContextMenuHandler};
pub use display_handler::DisplayEvent;
use keyboard_handler::{IcyKeyboardHandler, KeyboardHandlerBuilder};
pub use lifespan_handler::LifeSpanEvent;
use lifespan_handler::{IcyLifeSpanHandler, LifeSpanHandlerBuilder};
pub use load_handler::LoadEvent;
use load_handler::{IcyLoadHandler, LoadHandlerBuilder};

use lintel_lib_iced::iced;
use lintel_shared_data::TransferData;
use render_handler::{IcyRenderHandler, IcyRenderState, RenderHandlerBuilder};
use std::sync::Mutex;
use std::{
  collections::HashMap,
  ptr::null_mut,
  sync::{Arc, atomic::AtomicUsize},
  time::Duration,
};
use tokio::sync::{
  mpsc::{Receiver, UnboundedReceiver, UnboundedSender, unbounded_channel},
  oneshot,
};

use crate::webview::{
  client::display_handler::{DisplayHandlerBuilder, IcyDisplayHandler},
  instance::LaunchId,
};
mod context_menu_handler;
mod display_handler;
mod download_handler;
mod drag_handler;
mod focus_handler;
mod jsdialog_handler;
mod keyboard_handler;
mod lifespan_handler;
mod load_handler;
mod permission_handler;
mod render_handler;
mod request_handler;

pub use download_handler::DownloadEvent;
pub(crate) use render_handler::{CefView, CefViewWrapper};
pub use request_handler::RequestEvent;

/// A [`CefMessage`] annotated with whether it originated from a lifecycle
/// handler (lifespan, load, request, display, download).  The relay task uses
/// this flag to decide whether to buffer the event when no subscriber is
/// connected — lifecycle events are buffered, high-frequency render/input
/// events are silently dropped.
///
/// The `lifecycle` flag is set once, at the source, inside
/// [`ClientEventSubscriber::into_stream`], so adding a new `CefMessage`
/// variant automatically gets the correct classification without touching
/// the relay logic.
#[derive(Clone, Debug)]
pub struct RelayEvent {
  pub msg: CefMessage,
  /// `true` for events that fire at most once per navigation / browser
  /// lifetime (Loaded, Closed, TitleChanged, …) and must not be silently
  /// dropped.
  pub lifecycle: bool,
}

pub struct ClientEventSubscriber {
  pub lifespan_rx: Receiver<LifeSpanEvent>,
  pub load_rx: UnboundedReceiver<LoadEvent>,
  pub process_message_rx: UnboundedReceiver<CefIpcMessage>,
  pub display_rx: Option<UnboundedReceiver<DisplayEvent>>,
  pub request_rx: UnboundedReceiver<RequestEvent>,
  pub download_rx: UnboundedReceiver<DownloadEvent>,
  pub context_menu_rx: UnboundedReceiver<ContextMenuEvent>,
  pub drag_rx: Receiver<(BrowserId, DragData)>,
  pub ime_composition_rx: Receiver<(BrowserId, f32, f32)>,
}

impl std::fmt::Debug for ClientEventSubscriber {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ClientEventSubscriber").finish_non_exhaustive()
  }
}

/// Depositor side of the subscriber handoff: `launch_webview` calls
/// `.take().send(subscriber)` after the CEF browser is created.
pub type SubscriberDepositor = Arc<Mutex<Option<oneshot::Sender<ClientEventSubscriber>>>>;

/// 每次 browser 创建时由可信 Rust 宿主选择，绝不从 JS 请求中读取。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeResponsePolicy {
  Legacy(Duration),
  /// 上层已拥有期限、取消和一次性回复；relay 只等待该 owner 的结果。
  Managed,
}

impl Default for NativeResponsePolicy {
  fn default() -> Self {
    Self::Legacy(Duration::from_secs(15))
  }
}

async fn await_native_response(
  response: oneshot::Receiver<anyhow::Result<bytes::Bytes>>,
  policy: NativeResponsePolicy,
) -> anyhow::Result<bytes::Bytes> {
  let result = match policy {
    NativeResponsePolicy::Legacy(timeout) => tokio::time::timeout(timeout, response)
      .await
      .map_err(|_| anyhow::anyhow!("LINTEL_TIMEOUT"))?,
    NativeResponsePolicy::Managed => response.await,
  };
  result.map_err(|_| anyhow::anyhow!("LINTEL_CANCELLED"))?
}

#[cfg(test)]
mod native_response_policy_tests {
  use super::*;

  #[test]
  fn native_response_policy_default_keeps_the_legacy_fifteen_second_watchdog() {
    assert_eq!(
      NativeResponsePolicy::default(),
      NativeResponsePolicy::Legacy(Duration::from_secs(15))
    );
  }

  #[tokio::test]
  async fn native_response_policy_managed_wait_keeps_a_slow_owned_response() {
    let (send, receive) = oneshot::channel();
    let sender = tokio::spawn(async move {
      tokio::time::sleep(Duration::from_millis(20)).await;
      send.send(Ok(bytes::Bytes::from_static(b"owned response"))).unwrap();
    });
    let response = tokio::time::timeout(
      Duration::from_secs(2),
      await_native_response(receive, NativeResponsePolicy::Managed),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response, b"owned response"[..]);
    sender.await.unwrap();
  }

  #[tokio::test]
  async fn native_response_policy_legacy_expiry_has_a_stable_timeout_and_suppresses_late_send() {
    let (send, receive) = oneshot::channel();
    let error = tokio::time::timeout(
      Duration::from_secs(2),
      await_native_response(receive, NativeResponsePolicy::Legacy(Duration::from_millis(5))),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.to_string(), "LINTEL_TIMEOUT");
    assert!(send.is_closed());
    assert!(send.send(Ok(bytes::Bytes::from_static(b"late response"))).is_err());
  }

  #[tokio::test]
  async fn native_response_policy_managed_preserves_the_owner_timeout_and_sender_cancellation() {
    let (send, receive) = oneshot::channel();
    send.send(Err(anyhow::anyhow!("LINTEL_TIMEOUT"))).unwrap();
    let error = await_native_response(receive, NativeResponsePolicy::Managed)
      .await
      .unwrap_err();
    assert_eq!(native_error_code(&error), "LINTEL_TIMEOUT");
    let (send, receive) = oneshot::channel();
    drop(send);
    assert_eq!(
      await_native_response(receive, NativeResponsePolicy::Managed)
        .await
        .unwrap_err()
        .to_string(),
      "LINTEL_CANCELLED"
    );
  }

  #[tokio::test]
  async fn native_response_policy_cancelled_relay_closes_only_its_response_receiver() {
    let (send, receive) = oneshot::channel();
    let mut response = Box::pin(await_native_response(receive, NativeResponsePolicy::Managed));
    assert!(futures::poll!(response.as_mut()).is_pending());
    drop(response);
    assert!(send.is_closed());
    assert!(send.send(Ok(bytes::Bytes::from_static(b"late response"))).is_err());
  }
}

/// 只接受最外层的固定错误码，不把业务错误链、请求值或回溯交给 renderer。
fn native_error_code(error: &anyhow::Error) -> &'static str {
  match error.to_string().as_str() {
    "LINTEL_CANCELLED" => "LINTEL_CANCELLED",
    "LINTEL_INVALID_INPUT" => "LINTEL_INVALID_INPUT",
    "LINTEL_BUSY" => "LINTEL_BUSY",
    "LINTEL_DENIED" => "LINTEL_DENIED",
    "LINTEL_UNKNOWN_COMMAND" => "LINTEL_UNKNOWN_COMMAND",
    "LINTEL_DUPLICATE_REQUEST" => "LINTEL_DUPLICATE_REQUEST",
    "LINTEL_TIMEOUT" => "LINTEL_TIMEOUT",
    "LINTEL_SERVICE_UNAVAILABLE" => "LINTEL_SERVICE_UNAVAILABLE",
    "LINTEL_UNAVAILABLE" => "LINTEL_UNAVAILABLE",
    "LINTEL_INVALID_RESPONSE" => "LINTEL_INVALID_RESPONSE",
    _ => "LINTEL_COMMAND_FAILED",
  }
}

/// 成功和失败都使用相同的 TransferData CBOR 外壳；String 内容是原错误码而非 JSON 字符串。
fn native_error_wire(error: &anyhow::Error) -> anyhow::Result<(&'static str, bytes::Bytes)> {
  let code = native_error_code(error);
  Ok((code, TransferData::String(code.to_owned()).to_bytes()?))
}

#[cfg(test)]
mod native_error_wire_tests {
  use super::*;

  #[test]
  fn native_error_wire_preserves_known_outer_code_in_the_renderer_cbor_envelope() {
    for code in [
      "LINTEL_DENIED",
      "LINTEL_CANCELLED",
      "LINTEL_INVALID_INPUT",
      "LINTEL_BUSY",
      "LINTEL_UNKNOWN_COMMAND",
      "LINTEL_DUPLICATE_REQUEST",
      "LINTEL_TIMEOUT",
      "LINTEL_SERVICE_UNAVAILABLE",
      "LINTEL_UNAVAILABLE",
      "LINTEL_INVALID_RESPONSE",
      "LINTEL_COMMAND_FAILED",
    ] {
      let error = anyhow::anyhow!("private cause request-body=secret-token").context(code);
      let (classified, bytes) = native_error_wire(&error).unwrap();
      assert_eq!(classified, code);
      assert!(matches!(TransferData::from_bytes(bytes).unwrap(), TransferData::String(value) if value == code));
    }
    // 旧的原始 UTF-8 不是 renderer 解码的 TransferData 外壳。
    assert!(TransferData::from_bytes(bytes::Bytes::from_static(b"LINTEL_DENIED")).is_err());
  }

  #[test]
  fn native_error_wire_never_searches_nested_causes_or_copies_user_details() {
    for error in [
      anyhow::anyhow!("private request payload secret-token"),
      anyhow::anyhow!("LINTEL_DENIED").context("private outer context secret-token"),
      anyhow::anyhow!("LINTEL_DENIED secret-token"),
      anyhow::anyhow!("LINTEL_NOT_A_PUBLIC_CODE"),
    ] {
      let (classified, bytes) = native_error_wire(&error).unwrap();
      assert_eq!(classified, "LINTEL_COMMAND_FAILED");
      assert!(
        !bytes
          .windows(b"secret-token".len())
          .any(|window| window == b"secret-token"),
        "private details entered the wire payload"
      );
      assert!(matches!(
        TransferData::from_bytes(bytes).unwrap(),
        TransferData::String(value) if value == "LINTEL_COMMAND_FAILED"
      ));
    }
  }
}

impl ClientEventSubscriber {
  /// Convert the subscriber into a merged async stream of [`RelayEvent`]s.
  ///
  /// Each handler stream is tagged with `lifecycle: true/false` at the source
  /// so the relay task knows which events to buffer during subscriber gaps.
  pub(crate) fn into_stream(
    self,
    response_policy: NativeResponsePolicy,
  ) -> impl tokio_stream::Stream<Item = RelayEvent> + Send + 'static {
    let ClientEventSubscriber {
      lifespan_rx,
      load_rx,
      mut process_message_rx,
      display_rx,
      request_rx,
      download_rx,
      context_menu_rx,
      drag_rx,
      ime_composition_rx,
    } = self;
    let (inner_tx, inner_rx) = tokio::sync::mpsc::unbounded_channel();
    let (js_call_rust_tx, js_call_rust_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
      while let Some(msg) = process_message_rx.recv().await {
        match msg {
          crate::webview::client::CefIpcMessage::JsCallRust {
            browser_id,
            frame_id,
            context_id,
            fn_name,
            params,
            host_generation,
          } => {
            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();

            let ret = js_call_rust_tx.send(JsInvocation {
              browser_id: browser_id.into(),
              frame_id: frame_id.clone(),
              context_id: context_id.clone(),
              fn_name: fn_name.clone(),
              params,
              host_generation,
              resp_tx: std::sync::Arc::new(AtomicTake::new(resp_tx)),
            });
            use cef::ImplListValue;
            tokio::spawn(async move {
              let result = if let Err(err) = ret {
                Err(err.into())
              } else {
                await_native_response(resp_rx, response_policy).await
              };
              #[derive(Copy, Clone, Debug)]
              enum IpcSent {
                Ok,
                Err,
              }
              let mut notified_renderer = IpcSent::Err;
              match result {
                Ok(resp) => {
                  if let Some(frame) = cef::browser_host_get_browser_by_identifier(browser_id)
                    .and_then(|b| b.frame_by_identifier(Some(&frame_id.as_str().into())))
                    && let Some(mut message) = cef::process_message_create(Some(&"__rsAPI__.response".into()))
                    && let Some(args) = message.argument_list()
                  {
                    args.set_string(0, Some(&context_id.as_str().into()));
                    args.set_string(1, Some(&fn_name.as_str().into()));
                    args.set_binary(2, cef::binary_value_create(Some(&resp)).as_mut());
                    args.set_bool(3, true as _);
                    frame.send_process_message(cef::sys::cef_process_id_t::PID_RENDERER.into(), Some(&mut message));
                    notified_renderer = IpcSent::Ok;
                  }
                }
                Err(err) => {
                  let (code, payload) = match native_error_wire(&err) {
                    Ok(payload) => payload,
                    Err(_) => {
                      tracing::error!("cannot encode JS bridge failure response");
                      return;
                    }
                  };
                  tracing::warn!(context_id, %fn_name, error_code = code, "cannot handle js call");
                  if let Some(frame) = cef::browser_host_get_browser_by_identifier(browser_id)
                    .and_then(|b| b.frame_by_identifier(Some(&frame_id.as_str().into())))
                    && let Some(mut message) = cef::process_message_create(Some(&"__rsAPI__.response".into()))
                    && let Some(args) = message.argument_list()
                  {
                    args.set_string(0, Some(&context_id.as_str().into()));
                    args.set_string(1, Some(&fn_name.as_str().into()));
                    args.set_binary(2, cef::binary_value_create(Some(&payload)).as_mut());
                    args.set_bool(3, false as _);
                    frame.send_process_message(cef::sys::cef_process_id_t::PID_RENDERER.into(), Some(&mut message));
                    notified_renderer = IpcSent::Ok;
                  }
                }
              }
              match notified_renderer {
                IpcSent::Ok => tracing::trace!(fn_name, context_id, "js call notify renderer success"),
                IpcSent::Err => {
                  tracing::warn!(fn_name, context_id, "js call notify renderer failed")
                }
              }
            });
          }
          crate::webview::client::CefIpcMessage::CaretPositionChanged { browser_id, offset } => {
            if let Err(err) = inner_tx.send(CefMessage::UpdateCaretOffset(browser_id.into(), offset)) {
              tracing::error!(?err, "failed to send caret offset update");
            }
          }
          crate::webview::client::CefIpcMessage::FocusedNodeChanged {
            browser_id,
            x,
            y,
            width,
            height,
            is_editable,
          } => {
            if let Err(err) = inner_tx.send(CefMessage::FocusedNodeChanged(
              browser_id.into(),
              iced::Rectangle {
                x: x as _,
                y: y as _,
                width: width as _,
                height: height as _,
              },
              is_editable,
            )) {
              tracing::error!(?err, "failed to send editable node focused message");
            }
          }
          crate::webview::client::CefIpcMessage::RustInvokeResponse {
            browser_id: _,
            context_id,
            params,
          } => {
            // spawn 出去， 避免阻塞 webview ipc 事件循环
            tokio::task::spawn(async move {
              WebviewApi::notify_invoke_response(context_id.as_str(), params).await;
            });
          }
        }
      }
      tokio::task::yield_now().await;
    });
    use tokio_stream::StreamExt;
    let js_call_rx = JsCallRx::new(Some(js_call_rust_rx));

    /// Helper: wrap a CefMessage stream, tagging every item with a fixed
    /// `lifecycle` flag that reflects the originating handler.
    fn tag_stream(
      stream: std::pin::Pin<Box<dyn tokio_stream::Stream<Item = CefMessage> + Send>>,
      lifecycle: bool,
    ) -> std::pin::Pin<Box<dyn tokio_stream::Stream<Item = RelayEvent> + Send>> {
      Box::pin(stream.map(move |msg| RelayEvent { msg, lifecycle }))
    }

    type BoxedStream = std::pin::Pin<Box<dyn tokio_stream::Stream<Item = RelayEvent> + Send>>;

    let mut streams: Vec<BoxedStream> = vec![
      // — Lifecycle handlers (lifecycle: true) —
      tag_stream(
        Box::pin(
          tokio_stream::wrappers::ReceiverStream::new(lifespan_rx).map(move |event| match event {
            LifeSpanEvent::Closed { browser_id } => CefMessage::Closed(browser_id),
            LifeSpanEvent::Created {
              launch_id,
              browser_id,
              is_popup,
            } => CefMessage::Created {
              launch_id,
              browser_id,
              js_call_rx: if is_popup {
                JsCallRx::new(None)
              } else {
                js_call_rx.clone()
              },
            },
            LifeSpanEvent::CreatePopup { browser_id, target_url } => CefMessage::CreatePopup(browser_id, target_url),
          }),
        ),
        true,
      ),
      tag_stream(
        Box::pin(
          tokio_stream::wrappers::UnboundedReceiverStream::new(load_rx).map(move |msg| match msg {
            LoadEvent::Start {
              browser_id,
              frame_id,
              transition_type,
            } => CefMessage::LoadStart {
              browser_id,
              frame_id,
              transition_type,
            },
            LoadEvent::Changed {
              browser_id,
              is_loading,
              can_go_forward,
              can_go_back,
            } => CefMessage::LoadChanged {
              browser_id,
              is_loading,
              can_go_forward,
              can_go_back,
            },
            LoadEvent::End {
              browser_id,
              frame_id,
              http_status_code,
            } => CefMessage::Loaded {
              browser_id,
              frame_id,
              http_status_code,
            },
            LoadEvent::Error {
              browser_id,
              frame_id,
              error_code,
              error_text,
              failed_url,
            } => CefMessage::LoadError {
              browser_id,
              frame_id,
              error_code,
              error_text,
              failed_url,
            },
          }),
        ),
        true,
      ),
      tag_stream(
        Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(request_rx).map(CefMessage::Request)),
        true,
      ),
      tag_stream(
        Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(download_rx).map(CefMessage::Download)),
        true,
      ),
      // — Non-lifecycle / high-frequency handlers (lifecycle: false) —
      tag_stream(
        Box::pin(
          tokio_stream::wrappers::UnboundedReceiverStream::new(context_menu_rx)
            .map(|event| CefMessage::ContextMenuRequested(event.request)),
        ),
        false,
      ),
      tag_stream(
        Box::pin(tokio_stream::wrappers::UnboundedReceiverStream::new(inner_rx)),
        false,
      ),
      tag_stream(
        Box::pin(
          tokio_stream::wrappers::ReceiverStream::new(drag_rx).map(|(id, data)| CefMessage::DragOperation(id, data)),
        ),
        false,
      ),
      tag_stream(
        Box::pin(
          tokio_stream::wrappers::ReceiverStream::new(ime_composition_rx)
            .map(|(id, x, y)| CefMessage::CompositionPositionChanged(id, x, y)),
        ),
        false,
      ),
    ];
    if let Some(display_rx) = display_rx {
      // TitleChanged / UrlChanged are lifecycle; CursorChanged is not.
      // Tag per-event rather than per-stream.
      streams.push(Box::pin(
        tokio_stream::wrappers::UnboundedReceiverStream::new(display_rx).map(move |msg| match msg {
          DisplayEvent::TitleChanged { browser_id, title } => RelayEvent {
            msg: CefMessage::TitleChanged(browser_id, title),
            lifecycle: true,
          },
          DisplayEvent::UrlChanged { browser_id, url } => RelayEvent {
            msg: CefMessage::UrlChanged(browser_id, url),
            lifecycle: true,
          },
          DisplayEvent::CursorChanged {
            browser_id,
            cursor_type,
          } => RelayEvent {
            msg: CefMessage::CursorChanged(browser_id, cursor_type),
            lifecycle: false,
          },
        }),
      ));
    }

    futures::stream::select_all(streams)
  }
}

pub struct IcyClient {
  pub state: Option<IcyClientState>,
  pub subscribers: ClientEventSubscriber,
  pub lifecycle: Option<Arc<dyn ClientLifecyeleImpl>>,
}

#[derive(Clone, Debug)]
pub(crate) struct IcyClientState {
  pub render: IcyRenderState,
}

pub(crate) struct IcyClientConfig {
  pub launch_id: LaunchId,
  pub device_scale_factor: f32,
  pub view_rect: cef::Rect,
  /// Just run JavaScript code, no interaction with the user and renderering
  pub as_js_runner: bool,
  pub screen: Option<iced::window::ScreenInfo>,
  pub lifecycle: Option<Arc<dyn ClientLifecyeleImpl>>,
  pub partition_key: Option<String>,
  pub hw_view: Arc<ArcSwapOption<CefView>>,
  pub popup_view: Arc<ArcSwapOption<CefView>>,
  pub native_window: Option<isize>,
}

impl IcyClient {
  pub fn new(config: IcyClientConfig) -> (Self, IcyClientHandlers) {
    let IcyClientConfig {
      launch_id,
      device_scale_factor,
      view_rect,
      as_js_runner,
      screen,
      lifecycle,
      partition_key,
      hw_view,
      popup_view,
      native_window,
    } = config;
    let (load_handler, load_rx) = IcyLoadHandler::with_lifecycle(lifecycle.clone());
    let (lifespan_handler, lifespan_rx) = IcyLifeSpanHandler::new(launch_id);
    let (download_handler, download_rx) = IcyDownloadHandler::new(partition_key.clone());

    let (process_message_tx, process_message_rx) = unbounded_channel();
    let (render_handler, _, drag_rx, ime_composition_rx) = IcyRenderHandler::new(
      device_scale_factor,
      view_rect.clone(),
      screen.clone(),
      hw_view.clone(),
      popup_view.clone(),
      native_window,
    );

    let (request_handler, request_rx) = IcyRequestHandler::new(lifecycle.clone(), partition_key.clone());
    if as_js_runner {
      return (
        Self {
          state: None,
          subscribers: ClientEventSubscriber {
            lifespan_rx,
            load_rx,
            process_message_rx,
            display_rx: None,
            request_rx,
            download_rx,
            context_menu_rx: unbounded_channel().1,
            drag_rx,
            ime_composition_rx,
          },
          lifecycle,
        },
        IcyClientHandlers {
          load_handler,
          request_handler,
          lifespan_handler,
          render_handler: render_handler.into(),
          context_menu_handler: None,
          display_handler: None,
          keyboard_handler: None,
          process_message_tx,
          download_handler: Some(download_handler),
          permission_handler: Some(IcyPermissionHandler::new()),
          jsdialog_handler: Some(IcyJsdialogHandler::new()),
        },
      );
    }

    let (context_menu_handler, context_menu_rx) = IcyContextMenuHandler::new();
    let (display_handler, display_rx) = IcyDisplayHandler::new();
    let keyboard_handler = IcyKeyboardHandler::new();
    let (render_handler, render_state, drag_rx, ime_composition_rx) = IcyRenderHandler::new(
      device_scale_factor,
      view_rect,
      screen,
      hw_view,
      popup_view,
      native_window,
    );
    let state = IcyClientState { render: render_state };
    let subscribers = ClientEventSubscriber {
      lifespan_rx,
      load_rx,
      display_rx: Some(display_rx),
      process_message_rx,
      request_rx,
      download_rx,
      context_menu_rx,
      drag_rx,
      ime_composition_rx,
    };
    let handlers = IcyClientHandlers {
      load_handler,
      lifespan_handler,
      request_handler,
      render_handler: Some(render_handler),
      context_menu_handler: Some(context_menu_handler),
      display_handler: Some(display_handler),
      keyboard_handler: Some(keyboard_handler),
      process_message_tx,
      download_handler: Some(download_handler),
      permission_handler: Some(IcyPermissionHandler::new()),
      jsdialog_handler: Some(IcyJsdialogHandler::new()),
    };
    (
      Self {
        state: Some(state),
        subscribers,
        lifecycle,
      },
      handlers,
    )
  }
}

#[derive(Clone)]
pub struct IcyClientHandlers {
  load_handler: IcyLoadHandler,
  request_handler: IcyRequestHandler,
  lifespan_handler: IcyLifeSpanHandler,
  process_message_tx: UnboundedSender<CefIpcMessage>,
  render_handler: Option<IcyRenderHandler>,
  context_menu_handler: Option<IcyContextMenuHandler>,
  keyboard_handler: Option<IcyKeyboardHandler>,
  display_handler: Option<IcyDisplayHandler>,
  download_handler: Option<IcyDownloadHandler>,
  permission_handler: Option<IcyPermissionHandler>,
  jsdialog_handler: Option<IcyJsdialogHandler>,
}

pub enum CefIpcMessage {
  FocusedNodeChanged {
    browser_id: i32,
    x: f32,
    y: f32,
    width: i32,
    height: i32,
    is_editable: bool,
  },
  CaretPositionChanged {
    browser_id: i32,
    offset: i32,
  },
  JsCallRust {
    browser_id: i32,
    frame_id: String,
    context_id: String,
    fn_name: String,
    // serialized json
    params: bytes::Bytes,
    /// CEF 原生入口捕获的文档代号，不属于 renderer 可写的请求数据。
    host_generation: Option<u64>,
  },
  RustInvokeResponse {
    browser_id: i32,
    context_id: String,
    // serialized json
    params: bytes::Bytes,
  },
}

pub(crate) struct ClientBuilder {
  object: *mut RcImpl<sys::cef_client_t, Self>,
  load_handler: LoadHandler,
  request_handler: RequestHandler,
  lifespan_handler: LifeSpanHandler,
  render_handler: Option<RenderHandler>,
  context_menu_handler: Option<ContextMenuHandler>,
  display_handler: Option<DisplayHandler>,
  keyboard_handler: Option<KeyboardHandler>,
  focus_handler: Option<FocusHandler>,
  permission_handler: Option<PermissionHandler>,
  jsdialog_handler: Option<JsdialogHandler>,
  drag_handler: Option<cef::DragHandler>,
  process_message_tx: UnboundedSender<CefIpcMessage>,
  lifecycle: Option<Arc<dyn ClientLifecyeleImpl>>,
  download_handler: Option<DownloadHandler>,
}

impl ClientBuilder {
  pub(crate) fn build(client_handlers: IcyClientHandlers) -> Client {
    let IcyClientHandlers {
      load_handler,
      lifespan_handler,
      render_handler,
      context_menu_handler,
      keyboard_handler,
      display_handler,
      process_message_tx,
      request_handler,
      download_handler,
      permission_handler,
      jsdialog_handler,
    } = client_handlers;
    let load_handler = LoadHandlerBuilder::build(load_handler);
    let lifespan_handler = LifeSpanHandlerBuilder::build(lifespan_handler);
    let lifecycle = request_handler.lifecycle.clone();
    let request_handler = RequestHandlerBuilder::build(request_handler);
    let render_handler = render_handler.map(RenderHandlerBuilder::build);
    let context_menu_handler = context_menu_handler.map(ContextMenuHandlerBuilder::build);
    let keyboard_handler = keyboard_handler.map(KeyboardHandlerBuilder::build);
    let display_handler = display_handler.map(DisplayHandlerBuilder::build);
    let download_handler = download_handler.map(DownloadHandlerBuilder::build);
    let permission_handler = permission_handler.map(PermissionHandlerBuilder::build);
    let jsdialog_handler = jsdialog_handler.map(JsdialogHandlerBuilder::build);
    let drag_handler = Some(drag_handler::IcyDragHandlerBuilder::build(
      drag_handler::IcyDragHandler {},
    ));
    Client::new(Self {
      object: null_mut(),
      load_handler,
      lifespan_handler,
      display_handler,
      request_handler,
      render_handler,
      context_menu_handler,
      keyboard_handler,
      focus_handler: FocusHandlerBuilder::build(IcyFocusHandler::new()).into(),
      permission_handler,
      jsdialog_handler,
      process_message_tx,
      lifecycle,
      download_handler,
      drag_handler,
    })
  }
}

impl Rc for ClientBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl WrapClient for ClientBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::cef_client_t, Self>) {
    self.object = object;
  }
}

impl Clone for ClientBuilder {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc_impl = &mut *self.object;
      rc_impl.interface.add_ref();
      rc_impl
    };

    Self {
      object,
      load_handler: self.load_handler.clone(),
      request_handler: self.request_handler.clone(),
      render_handler: self.render_handler.clone(),
      lifespan_handler: self.lifespan_handler.clone(),
      context_menu_handler: self.context_menu_handler.clone(),
      keyboard_handler: self.keyboard_handler.clone(),
      display_handler: self.display_handler.clone(),
      focus_handler: self.focus_handler.clone(),
      permission_handler: self.permission_handler.clone(),
      jsdialog_handler: self.jsdialog_handler.clone(),
      process_message_tx: self.process_message_tx.clone(),
      lifecycle: self.lifecycle.clone(),
      download_handler: self.download_handler.clone(),
      drag_handler: self.drag_handler.clone(),
    }
  }
}

impl ImplClient for ClientBuilder {
  fn get_raw(&self) -> *mut sys::_cef_client_t {
    self.object.cast()
  }

  fn load_handler(&self) -> Option<cef::LoadHandler> {
    Some(self.load_handler.clone())
  }

  fn life_span_handler(&self) -> Option<cef::LifeSpanHandler> {
    Some(self.lifespan_handler.clone())
  }

  fn request_handler(&self) -> Option<RequestHandler> {
    Some(self.request_handler.clone())
  }

  fn render_handler(&self) -> Option<cef::RenderHandler> {
    self.render_handler.clone()
  }

  fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
    self.context_menu_handler.clone()
  }

  fn keyboard_handler(&self) -> Option<KeyboardHandler> {
    self.keyboard_handler.clone()
  }

  fn display_handler(&self) -> Option<DisplayHandler> {
    self.display_handler.clone()
  }

  fn focus_handler(&self) -> Option<FocusHandler> {
    self.focus_handler.clone()
  }

  fn permission_handler(&self) -> Option<PermissionHandler> {
    self.permission_handler.clone()
  }

  fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
    self.jsdialog_handler.clone()
  }

  fn download_handler(&self) -> Option<DownloadHandler> {
    self.download_handler.clone()
  }

  fn drag_handler(&self) -> Option<cef::DragHandler> {
    self.drag_handler.clone()
  }

  fn on_process_message_received(
    &self,
    browser: Option<&mut cef::Browser>,
    frame: Option<&mut cef::Frame>,
    source_process: cef::ProcessId,
    message: Option<&mut cef::ProcessMessage>,
  ) -> std::ffi::c_int {
    use cef::ImplListValue;
    let Some(browser) = browser else {
      return false as _;
    };
    let Some(frame) = frame else {
      return false as _;
    };
    let Some(message) = message else {
      return false as _;
    };
    let browser_id = browser.identifier();
    let api_name = cef::CefString::from(&message.name()).to_string();
    if "__rsAPI__.request" == &api_name
      && let Some(args) = message.argument_list()
      && let uuid = args.string(0)
      && let method = args.string(1)
      && let Some(params) = args.binary(2)
    {
      let payload_size = params.size();
      let mut host_generation = None;
      if let Some(policy) = &self.lifecycle {
        let url = cef::CefString::from(&frame.url()).to_string();
        let is_main_frame = frame.is_main() == 1 && frame.is_valid() == 1;
        let from_renderer = source_process == cef::sys::cef_process_id_t::PID_RENDERER.into();
        if !policy.allows_js_invocation(&url, is_main_frame, from_renderer, payload_size) {
          tracing::warn!(browser_id, "CEF bridge invocation denied by the page policy");
          return true as _;
        }
        host_generation = policy.bridge_generation();
      }
      let mut data = vec![0; payload_size];
      if params.data(Some(&mut data), 0) != payload_size {
        return false as _;
      }
      let frame_id = cef::CefString::from(&frame.identifier()).to_string();
      let context_id = cef::CefString::from(&uuid).to_string();
      let fn_name = cef::CefString::from(&method).to_string();
      let params = bytes::Bytes::from(data);
      // tracing::debug!(
      //   context_id,
      //   browser_id,
      //   frame_id,
      //   fn_name,
      //   "receive call from javascript"
      // );
      if self
        .process_message_tx
        .send(CefIpcMessage::JsCallRust {
          browser_id,
          frame_id: frame_id.clone(),
          context_id: context_id.clone(),
          fn_name: fn_name.clone(),
          params,
          host_generation,
        })
        .inspect_err(|err| {
          tracing::error!(%err, "failed to send invocation");
        })
        .is_err()
      {
        return false as _;
      }

      return true as _;
    }

    if ServicePromiseCallback::channel_key() == api_name
      && let Some(args) = message.argument_list()
      && let uuid = args.string(0)
      && let Some(params) = args.binary(1)
    {
      let frame_id = cef::CefString::from(&frame.identifier()).to_string();
      let context_id = cef::CefString::from(&uuid).to_string();

      let data = {
        let mut data = vec![0; params.size()];
        params.data(Some(&mut data), 0);
        data
      };

      let params = bytes::Bytes::from(data);
      tracing::trace!(
        context_id,
        browser_id,
        frame_id,
        "receive rust invoke response from javascript"
      );
      if self
        .process_message_tx
        .send(CefIpcMessage::RustInvokeResponse {
          browser_id,
          context_id: context_id.clone(),
          params,
        })
        .inspect_err(|err| {
          tracing::error!(%err, "failed to send rust invoke response");
        })
        .is_err()
      {
        return false as _;
      }

      return true as _;
    }

    if "renderer.caret_offset_changed" == &api_name
      && let Some(bound) = message.argument_list().map(|args| args.string(0))
    {
      #[derive(serde::Deserialize)]
      struct Offset {
        offset: i32,
      }
      let Ok(Offset { offset }) = serde_json::from_str(&cef::CefString::from(&bound).to_string()) else {
        return false as _;
      };
      if let Err(err) = self
        .process_message_tx
        .send(CefIpcMessage::CaretPositionChanged { browser_id, offset })
      {
        tracing::error!(?err, "cannot send ipc message event");
      }
      return true as _;
    }
    if "renderer.focused_node_changed" == &api_name
      && let Some(bound) = message.argument_list().map(|args| args.string(0))
    {
      #[derive(serde::Deserialize)]
      struct Node {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        is_editable: bool,
      }
      let Ok(Node {
        x,
        y,
        width,
        height,
        is_editable,
      }) = serde_json::from_str(&cef::CefString::from(&bound).to_string())
      else {
        return false as _;
      };
      if let Err(err) = self.process_message_tx.send(CefIpcMessage::FocusedNodeChanged {
        browser_id,
        x: x / 2.0,
        y: y / 2.0,
        width: (height / 2.0).floor() as _,
        height: (width / 2.0).floor() as _,
        is_editable,
      }) {
        tracing::error!(?err, "cannot send ipc message event");
      }
      return true as _;
    }

    if "renderer.ipc.response" == &api_name
      && let Some(resp) = message.argument_list()
      && let Some(payload) = resp.binary(0)
    {
      let ptr: *const u8 = payload.raw_data().cast();
      let size = payload.size();

      if !ptr.is_null()
        && let Ok(resp) = postcard::from_bytes::<Response>(unsafe { std::slice::from_raw_parts(ptr, size) })
      {
        ipc_response(resp.req_id, resp.response);
      }

      return true as _;
    }

    false as _
  }
}

// ONLY lives in cef io thread
thread_local! {
    static IPC_CALL_ROUTER: std::cell::RefCell<HashMap<RequestId, (oneshot::Sender<Vec<u8>>, std::time::Instant)>> = std::cell::RefCell::new(HashMap::new());
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Eq, PartialEq, Hash, Clone)]
pub(crate) struct RequestId {
  pub browser_id: i32,
  pub req_id: usize,
  pub method: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Eq, PartialEq)]
pub(crate) struct Request {
  pub req_id: RequestId,
  pub args: Vec<u8>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Eq, PartialEq)]
pub(crate) struct Response {
  pub req_id: RequestId,
  pub response: Vec<u8>,
}

pub(crate) fn ipc_request(
  browser_id: i32,
  method: &str,
  args: impl serde::Serialize,
) -> anyhow::Result<oneshot::Receiver<Vec<u8>>> {
  static ID: AtomicUsize = AtomicUsize::new(0);
  let req_id = ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
  let req_id = RequestId {
    browser_id,
    req_id,
    method: method.to_string(),
  };
  tracing::debug!(?req_id, "send renderer ipc request");
  let req_id_1 = req_id.clone();
  let request = Request {
    req_id,
    args: postcard::to_stdvec(&args)?,
  };
  if let Some(frame) = cef::browser_host_get_browser_by_identifier(browser_id).and_then(|b| b.main_frame())
    && frame.is_valid() == 1
    && let Some(mut message) = cef::process_message_create(Some(&"renderer.ipc.request".into()))
    && let Some(mut argment) = message.argument_list()
    && let Some(mut binary) = cef::binary_value_create(postcard::to_stdvec(&request).ok().as_deref())
    && argment.set_binary(0, Some(&mut binary)) == 1
  {
    let (tx, rx) = tokio::sync::oneshot::channel();
    post_task_sync_io!(move || {
      IPC_CALL_ROUTER.with_borrow_mut(|map| {
        map.insert(req_id_1, (tx, std::time::Instant::now()));
        // trim requests waiting for more than 30s
        map.retain(|_, v| v.1.elapsed() < std::time::Duration::from_millis(1_000 * 30));
      });
    });
    frame.send_process_message(cef::sys::cef_process_id_t::PID_RENDERER.into(), Some(&mut message));
    Ok(rx)
  } else {
    anyhow::bail!("cannot send ipc call to renderer");
  }
}

fn ipc_response(req_id: RequestId, resp: Vec<u8>) {
  post_task_sync_io!(move || {
    IPC_CALL_ROUTER.with_borrow_mut(|map| {
      tracing::debug!(?req_id, "receive renderer ipc response");
      if let Some(resp_tx) = map.remove(&req_id) {
        _ = resp_tx.0.send(resp);
      }
    });
  });
}
