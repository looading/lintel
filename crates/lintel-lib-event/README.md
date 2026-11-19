# lintel-lib-event

Synchronous, thread-safe event subscriptions with owned payloads and removable
callback IDs. This library has no GUI, native runtime, or async executor dependency.

```toml
[dependencies]
lintel-lib-event = "0.1.0"
anyhow = "1"
```

```rust
use std::{any::Any, sync::Arc};
use lintel_lib_event::{EventCallback, EventCallbackImpl, EventManager, EventPayload};

struct Count(u32);
impl EventPayload for Count {
    fn get_data(&self) -> Box<dyn Any> { Box::new(self.0) }
}

struct Increment;
impl EventCallbackImpl for Increment {
    fn call(&self, payload: Arc<dyn EventPayload>) -> anyhow::Result<Box<dyn Any>> {
        let value = payload.get_data().downcast::<u32>()
            .map_err(|_| anyhow::anyhow!("unexpected event payload"))?;
        Ok(Box::new(*value + 1))
    }
}

fn main() -> anyhow::Result<()> {
    let events = EventManager::new();
    let id = events.on("count", EventCallback::new(Arc::new(Increment)));
    let results = events.emit("count", Arc::new(Count(41)));
    let result = results.into_iter().next().unwrap()?.downcast::<u32>().unwrap();
    assert_eq!(*result, 42);
    events.off("count", &id);
    Ok(())
}
```

Callbacks execute on the emitting thread. The registry lock is released before
callbacks run, allowing a callback to query or change subscriptions. Multiple
emitting threads can execute a callback concurrently, so callback state must
follow the `Send + Sync` contract. Listener ordering is unspecified. Callback
errors are returned individually in the result vector; this is not a task queue
or a panic boundary.

Use `off`, `off_all`, or the returned IDs to manage subscription lifetime. The
library does not install a global bus or choose application permissions.

Licensed under MIT. See [LICENSE](LICENSE).
