#![allow(private_interfaces)]

use ::lintel_lib_jsb_macro as renamed_bridge;
use tokio::sync::{mpsc, oneshot};

// 故意遮蔽调用处名字，证明展开使用定义方路径，而不是测试包碰巧拥有这些依赖。
mod paste {}
mod tracing {}
mod lintel_lib_jsb_macro {}
#[allow(dead_code)]
struct ResponseTx;

renamed_bridge::define_api! {
  enum TestMessage {
    RequestValue(value: u32) -> u32,
    FireAndForget(text: String),
    ReadValue -> u32,
    Wake,
  }

  struct TestApi {
    sender: mpsc::UnboundedSender<TestMessage>,
  }
}

#[test]
fn renamed_macro_routes_returning_and_notification_messages_without_helper_imports() {
  let (sender, mut messages) = mpsc::unbounded_channel();
  let api = TestApi::new(sender);
  let (reply, mut result) = oneshot::channel();
  api.request_value(41, renamed_bridge::ResponseTx::new("request".into(), reply));
  match messages.try_recv().unwrap() {
    TestMessage::RequestValue(value, response) => response.execute(|| Ok(value + 1)),
    _ => panic!("returning request changed its message shape"),
  }
  assert_eq!(result.try_recv().unwrap().unwrap(), 42);

  api.fire_and_forget("changed".into());
  match messages.try_recv().unwrap() {
    TestMessage::FireAndForget(text) => assert_eq!(text, "changed"),
    _ => panic!("notification changed its message shape"),
  }
  api.wake();
  assert!(matches!(messages.try_recv().unwrap(), TestMessage::Wake()));

  let (reply, mut result) = oneshot::channel();
  api.read_value(renamed_bridge::ResponseTx::new("read".into(), reply));
  match messages.try_recv().unwrap() {
    TestMessage::ReadValue(response) => response.send(Ok(7)),
    _ => panic!("parameterless request changed its message shape"),
  }
  assert_eq!(result.try_recv().unwrap().unwrap(), 7);
}

#[test]
fn closed_message_receiver_drops_response_sender_and_keeps_unit_send_api() {
  let (sender, messages) = mpsc::unbounded_channel();
  let api = TestApi::new(sender);
  drop(messages);
  let (reply, mut result) = oneshot::channel::<Result<u32, _>>();
  let _: () = api.request_value(1, renamed_bridge::ResponseTx::new("closed".into(), reply));
  assert!(matches!(result.try_recv(), Err(oneshot::error::TryRecvError::Closed)));
  // 无返回值分支也沿用发送失败记录日志、返回 unit 的既有协议。
  let _: () = api.fire_and_forget("discarded".into());
  let _: () = api.wake();
}

#[test]
fn response_execution_preserves_handler_error_and_accepts_a_dropped_waiter() {
  let (reply, mut result) = oneshot::channel();
  renamed_bridge::ResponseTx::<u32>::new("business-error".into(), reply)
    .execute(|| Err(std::io::Error::other("expected business failure").into()));
  assert_eq!(
    result.try_recv().unwrap().unwrap_err().to_string(),
    "expected business failure"
  );

  let (reply, result) = oneshot::channel();
  drop(result);
  let _: () = renamed_bridge::ResponseTx::<u32>::new("dropped-waiter".into(), reply).send(Ok(1));
}
