//! One direct lintel dependency: contextual services, policy-bound tools and managed cleanup.
use std::{
  path::PathBuf,
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
};

use lintel::{
  AppContextApi, TaskScope,
  agent::{
    Invocation, InvocationCancellation, ToolEffect, ToolErrorCode, ToolGrant, ToolPolicy, ToolRegistry, ToolSession,
    ToolSpec, Value, json,
  },
  iced::{
    Element, Subscription, Task,
    widget::{button, column, text},
  },
  services::{Store, StoreConfig},
};

#[derive(Default)]
struct Proof {
  task_dropped: Arc<AtomicBool>,
  stored: AtomicBool,
  pending: Mutex<Option<InvocationCancellation>>,
}
struct DropProof(Arc<AtomicBool>);
impl Drop for DropProof {
  fn drop(&mut self) {
    self.0.store(true, Ordering::Release);
  }
}
#[derive(Debug)]
struct RequestedBootFailure;
impl std::fmt::Display for RequestedBootFailure {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("requested contextual initialization failure")
  }
}
impl std::error::Error for RequestedBootFailure {}

struct State {
  tools: Arc<ToolSession>,
  store: Arc<Store>,
  count: u64,
  status: String,
  smoke: bool,
  started: bool,
  ticks: u32,
  pending: Option<Invocation>,
  proof: Arc<Proof>,
}
#[derive(Clone, Debug)]
enum Message {
  Add,
  Result(lintel::agent::ToolResult<Value>),
  Tick,
  Quit,
}
fn add(state: &mut State) -> Task<Message> {
  match state.tools.invoke_json("counter.add", b"2") {
    Ok(invocation) => Task::perform(invocation.join(), Message::Result),
    Err(error) => {
      state.status = error.message().into();
      Task::none()
    }
  }
}
fn update(state: &mut State, message: Message) -> Task<Message> {
  match message {
    Message::Add => return add(state),
    Message::Result(Ok(value)) => {
      let Some(count) = value.as_u64() else {
        state.status = "Unexpected tool result".into();
        return Task::none();
      };
      state.count = count;
      state.status = "Saved through the approved tool".into();
      let persisted = state.store.get("count").and_then(|value| value.as_u64()) == Some(count);
      state.proof.stored.store(persisted, Ordering::Release);
      println!("LINTEL_AI_APP_TOOL_COMPLETED count={count} persisted={persisted}");
      if state.smoke {
        match state.tools.invoke_json("counter.wait", b"null") {
          Ok(invocation) => {
            *state.proof.pending.lock().unwrap() = Some(invocation.cancellation_handle());
            state.pending = Some(invocation);
            return lintel::iced::exit();
          }
          Err(error) => state.status = error.message().into(),
        }
      }
    }
    Message::Result(Err(error)) => state.status = error.message().into(),
    Message::Tick => {
      state.ticks += 1;
      if state.smoke && !state.started {
        state.started = true;
        return add(state);
      }
      if state.smoke && state.ticks >= 200 {
        return lintel::iced::exit();
      }
    }
    Message::Quit => return lintel::iced::exit(),
  }
  Task::none()
}
fn view(state: &State) -> Element<'_, Message> {
  column![
    text(format!("Counter: {}", state.count)),
    text(&state.status),
    button("Add two through a tool").on_press(Message::Add),
    button("Quit").on_press(Message::Quit),
  ]
  .spacing(16)
  .padding(32)
  .into()
}

fn main() -> lintel::Result<()> {
  let mut smoke = false;
  let mut fail_boot = false;
  let mut data_dir = PathBuf::new();
  let mut arguments = std::env::args_os().skip(1);
  while let Some(argument) = arguments.next() {
    match argument.to_str() {
      Some("--smoke") => smoke = true,
      Some("--fail-boot") => fail_boot = true,
      Some("--data-dir") => {
        data_dir = arguments
          .next()
          .map(PathBuf::from)
          .ok_or_else(|| lintel::Error::msg("--data-dir needs a path"))?;
      }
      _ => return Err(lintel::Error::msg("unknown example argument")),
    }
  }
  let proof = Arc::new(Proof::default());
  let boot_proof = proof.clone();
  let result = lintel::application_with_context(
    move |context| {
      let cleanup = DropProof(boot_proof.task_dropped.clone());
      context.spawn_task(TaskScope::Application, async move {
        let _cleanup = cleanup;
        std::future::pending::<()>().await;
      })?;
      let store = context
        .get_service::<Store>()
        .ok_or_else(|| lintel::Error::msg("store was not started"))?;
      if fail_boot {
        return Err(lintel::Error::new(RequestedBootFailure));
      }
      let mut registry = ToolRegistry::new();
      let read = store.clone();
      registry.register(
        ToolSpec::new(
          "counter.read",
          "Read the saved counter",
          json!({"type":"null"}),
          ToolEffect::ReadOnly,
        ),
        move |_, (): ()| {
          let store = read.clone();
          async move { Ok(store.get("count").and_then(|value| value.as_u64()).unwrap_or(0)) }
        },
      )?;
      let write = store.clone();
      registry.register_validated(
        ToolSpec::new(
          "counter.add",
          "Add a small positive amount",
          json!({"type":"integer","minimum":1,"maximum":10}),
          ToolEffect::LocalMutation,
        ),
        |amount: &u64| (1..=10).contains(amount),
        move |_, amount: u64| {
          let store = write.clone();
          async move {
            let count = store.get("count").and_then(|value| value.as_u64()).unwrap_or(0);
            let count = count
              .checked_add(amount)
              .ok_or_else(|| lintel::Error::msg("counter overflow"))?;
            store.set("count".into(), count.into())?;
            Ok(count)
          }
        },
      )?;
      registry.register(
        ToolSpec::new(
          "counter.wait",
          "Wait until the owning application closes",
          json!({"type":"null"}),
          ToolEffect::ReadOnly,
        ),
        |_, (): ()| async { std::future::pending::<lintel::Result<Value>>().await },
      )?;
      let registry = Arc::new(registry);
      let denied = ToolSession::new(context, TaskScope::Application, registry.clone(), ToolPolicy::default())?;
      let denial = match denied.invoke_json("counter.add", b"2") {
        Err(error) => error,
        Ok(invocation) => {
          drop(invocation);
          return Err(lintel::Error::msg(
            "default tool policy unexpectedly admitted a mutation",
          ));
        }
      };
      if denial.code != ToolErrorCode::Denied {
        return Err(lintel::Error::msg(
          "default tool policy unexpectedly allowed a mutation",
        ));
      }
      drop(denied);
      let policy = ToolPolicy::allow([
        ToolGrant::new("counter.read", ToolEffect::ReadOnly)?,
        ToolGrant::new("counter.add", ToolEffect::LocalMutation)?,
        ToolGrant::new("counter.wait", ToolEffect::ReadOnly)?,
      ])?;
      let tools = Arc::new(ToolSession::with_limits(
        context,
        TaskScope::Application,
        registry,
        policy,
        lintel::agent::ToolLimits {
          max_concurrency: 1,
          ..Default::default()
        },
      )?);
      println!(
        "LINTEL_AI_APP_READY tools={} default_deny=true",
        tools.catalog()?.tools.len()
      );
      Ok(State {
        count: store.get("count").and_then(|value| value.as_u64()).unwrap_or(0),
        tools,
        store,
        status: "Ready".into(),
        smoke,
        started: false,
        ticks: 0,
        pending: None,
        proof: boot_proof.clone(),
      })
    },
    update,
    view,
  )
  .id("dev.lintel.examples.ai-native")
  .title("Lintel AI-native app")
  .plugin(lintel::services::store(StoreConfig {
    cwd: data_dir,
    name: "agent-counter".into(),
    ..Default::default()
  }))
  .subscription(move |_: &State| {
    if smoke {
      lintel::iced::time::every(std::time::Duration::from_millis(50)).map(|_| Message::Tick)
    } else {
      Subscription::none()
    }
  })
  .run();
  if fail_boot {
    let error = result
      .err()
      .ok_or_else(|| lintel::Error::msg("boot unexpectedly succeeded"))?;
    if !error.is::<RequestedBootFailure>() || !proof.task_dropped.load(Ordering::Acquire) {
      return Err(lintel::Error::msg("boot error or managed cleanup was lost"));
    }
    println!("LINTEL_AI_APP_EXPECTED_BOOT_FAILURE_CLEANED");
    return Ok(());
  }
  result?;
  if smoke {
    let pending = proof.pending.lock().unwrap();
    if !proof.stored.load(Ordering::Acquire)
      || !proof.task_dropped.load(Ordering::Acquire)
      || !pending.as_ref().is_some_and(InvocationCancellation::is_finished)
    {
      return Err(lintel::Error::msg(
        "tool, persistence or owned task cleanup did not finish",
      ));
    }
    println!("LINTEL_AI_APP_SMOKE_OK");
  }
  Ok(())
}
