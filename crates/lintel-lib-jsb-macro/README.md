# lintel-lib-jsb-macro

Typed bridge message declarations and sender methods for local Lintel
integrations. The macros do not require a GUI runtime. Methods keep their
existing snake_case names and send the corresponding tuple variants through the
application-selected sender.

```toml
[dependencies]
bridge = { package = "lintel-lib-jsb-macro", version = "0.1.0" }
tokio = { version = "1", features = ["sync"] }
```

The dependency may be renamed. There is no need to directly depend on `paste` or
`tracing`, or to import `ResponseTx` for generated items to resolve.

```rust
use tokio::sync::{mpsc, oneshot};

bridge::define_api! {
    enum Message {
        Double(value: u32) -> u32,
        Notify(text: String),
    }

    struct Api {
        sender: mpsc::UnboundedSender<Message>,
    }
}

fn main() {
    let (sender, mut messages) = mpsc::unbounded_channel();
    let api = Api::new(sender);
    let (reply, mut result) = oneshot::channel();
    api.double(21, bridge::ResponseTx::new("request-1".into(), reply));
    match messages.try_recv().unwrap() {
        Message::Double(value, response) => response.send(Ok(value * 2)),
        _ => unreachable!(),
    }
    assert_eq!(result.try_recv().unwrap().unwrap(), 42);
    api.notify("updated".into());
}
```

`define_api!` declares the enum, a cloneable sender wrapper, `new`, and the
per-variant methods. `define_api_method!` remains available for a caller-owned
enum and wrapper. A sender must offer the synchronous `send(message) -> Result`
shape, such as Tokio's unbounded sender; the macros do not await a bounded
channel's async `send` method.

Methods return unit. Failed message sends log the operation without dumping the
message payload, and their messages are dropped;
for a response-bearing request this closes its oneshot sender. `ResponseTx::send`
delivers an `anyhow::Result<T>`, while `execute` runs a synchronous handler and
forwards that result. A dropped response receiver is logged without panicking.
There is no automatic retry, business replay, scheduler, or remote transport.

The hidden helper reexports are macro implementation details. Applications use
the public macros and `ResponseTx` rather than those helper paths. The regression
suite expands a real API with shadowed helper names and exercises returning,
notification, closed-channel and response-error behavior using local channels.

Part of [Lintel](https://github.com/looading/lintel). Licensed under MIT; see
[LICENSE](LICENSE).
