# Application-owned agent tools

The opt-in `agent` feature provides provider-neutral discovery and execution of
trusted Rust tools. It uses the application's existing `AppContext::spawn_task`
and `TaskScope` lifecycle. It includes no LLM vendor SDK, chat/RAG subsystem,
network MCP server, model-selected permissions, or default outbound transfer.
Results stay with the caller until application code explicitly does something
with them.

## Register typed handlers and choose a policy

A registry is built during trusted application setup, then shared through `Arc`.
Duplicate IDs are rejected rather than replaced. Each specification advertises an
ID, description, JSON input schema and declared effect. Registry catalogs use
`schema_version: 1` and `kind: "lintel.agent.tools"`; a session's catalog contains
only tools permitted by its policy. Discovery is not an execution token: every
invocation checks the live session and its policy again.

```rust,ignore
use std::sync::Arc;
use lintel::agent::{self, ToolEffect, ToolPolicy, ToolRegistry, ToolSession, ToolSpec, json};
use lintel::core::tasks::TaskScope;

#[derive(lintel::agent::serde::Deserialize)]
#[serde(crate = "lintel::agent::serde", deny_unknown_fields)]
struct Add { a: i64, b: i64 }

let mut tools = ToolRegistry::new();
tools.register_validated::<Add, i64, _, _, _>(
  ToolSpec::new("example.add", "Add bounded integers",
    json!({"type":"object", "properties":{"a":{"type":"integer"},"b":{"type":"integer"}},
      "required":["a","b"], "additionalProperties":false}),
    ToolEffect::ReadOnly),
  |input| (-1_000_000..=1_000_000).contains(&input.a)
    && (-1_000_000..=1_000_000).contains(&input.b),
  |context, input| async move {
    context.progress(1, Some(1))?;
    Ok(input.a + input.b)
  },
)?;
let session = ToolSession::new(
  app_context, TaskScope::Window(scope_id), Arc::new(tools), ToolPolicy::read_only(),
)?;
let input = agent::to_vec(&json!({"a":40,"b":2}))?;
let result = session.invoke_json("example.add", &input)?.join().await?;
```

`register` uses typed serde decoding without an additional domain validator;
`register_validated` also calls the supplied `Fn(&Input) -> bool`. Handlers return
`lintel::Result<Output>`, and output must implement `Serialize`. The schema is
**advertised metadata**. Lintel does not evaluate arbitrary JSON Schema keywords
or infer that typed serde defaults reject unknown fields. Declare
`deny_unknown_fields` and domain validation when needed. The registry is trusted
application code, not a facility for loading arbitrary functions from a model.

`ToolPolicy::default()` / `deny_all()` permits nothing. `read_only()` explicitly
permits registered tools declared `ReadOnly`. Exact grants require both a validated
ID and its exact declared effect:

```rust,ignore
use lintel::agent::ToolGrant;
let policy = ToolPolicy::allow([
  ToolGrant::new("settings.set", ToolEffect::LocalMutation)?,
])?;
```

Effects are `ReadOnly`, `LocalMutation`, and `External`. Effect declarations and
handler code are maintained by the trusted host. This is not an OS/process
sandbox: a mislabeled Rust handler can still use its application's authority.
Existing filesystem/network/native-service permissions remain separate checks.
Native/UI effects must use existing application messages, main-thread dispatch or
Send+Sync service queues; handlers do not receive worker-thread widget mutation
access from this module.

## Scope, close and cancellation

Create a session with the actual application/window/page scope. Window cancellation
also cancels its page tasks through the existing core tracker. A small managed
scope sentinel is registered at session creation, with its drop guard constructed
before spawning. Even cancellation before its first poll ends the old session.
A later page/window owner can create a fresh session; an old session cannot start
new work after its sentinel has been cancelled.

Each invocation is a real managed task in that scope. Invocation IDs are
session-local; correlate a progress stream with the session that created it. Session admission reserves
its own active entry before spawning. Handler execution waits until cancellation
registration commits. Closing during spawn cannot leave an unowned late task,
and an entry destroyed before commit is never inserted again. The session keeps
only its own cancellation handles; it never calls `cancel_scope` to close itself.
Other business tasks and other sessions in the same scope remain independent.

`Invocation::cancel()` and its cloneable cancellation handle request abortion.
Dropping an invocation cancels it. Dropping its `join()` future, before or after
first poll, also cancels it; there is no implicit detached tool mode. A completed
result can remain available after the session later closes, since already-finished
work is not cancelled by close. An explicit invocation cancellation suppresses
its returned result when the cancellation wins that check. Cancellation cannot
undo effects that have already occurred.

`ToolSession` itself is not Clone; wrap it in `Arc` to share one owner. Dropping
that final session closes it. `close()` requests cancellation synchronously;
`shutdown().await` additionally waits until its handler futures have been
destroyed. A concurrency slot is held through actual future destruction, not
released merely when cancellation is requested. Cancellation/waiting does not
hold the session lock. Application shutdown still uses the core tracker for its
own final bookkeeping.

## Limits and progress

`ToolSession::with_limits` accepts validated `ToolLimits`. Defaults and hard caps:

| Limit | Default | Hard cap |
| --- | --- | --- |
| Concurrent unfinished invocations | 4 | 32 |
| Input serialized JSON bytes | 64 KiB | 1 MiB |
| Output serialized JSON bytes | 64 KiB | 1 MiB |
| Queued progress events per invocation | 16 | 256 |
| Successfully sent progress events per invocation | 256 | 4096 |
| Cooperative invocation timeout | 15 seconds | 300 seconds |

All limits must be positive. There is no waiting execution queue: saturation
returns `busy`. Input bytes are checked before copying/decoding. Output is encoded
through a bounded writer before constructing the returned `Value`; a handler's
own allocations are outside this wire-size limit. Registry metadata is limited
to 64 tools, 2048-byte descriptions, 16 KiB schemas, bounded schema depth/nodes,
and a 256 KiB catalog.

The deadline covers managed typed decoding, domain validation, handler factory,
handler future and output serialization. It is **cooperative**: Tokio cannot
preempt blocking Rust code or a future that never yields. Do not block a worker,
spawn untracked work, or assume an arbitrary native/synchronous closure becomes
cancellable. Keep such operations bounded and use existing lifecycle-aware host
services. Do not detach work from the handler: separately spawned tasks do not
automatically become children of this invocation, even if the core scope tracker
owns them. Await the work the tool promises to complete. Already-performed side
effects are not rolled back by timeout. A non-yielding poll cannot be stopped,
but a result returned after its deadline is still labeled `timeout`.

`ToolContext::progress(completed, total)` sends numeric-only `ProgressEvent`s.
Use `Invocation::take_progress()` for its single bounded stream. Sequence
allocation and `try_send` occur under one mutex, so successful events are delivered
in increasing sequence order. Full queues return `ProgressError::Backpressure`;
nothing is silently dropped and the caller is not blocked. Invalid totals return
`InvalidProgress`, the total event budget returns `LimitReached`, and ended or
cancelled contexts return `Closed`. A handler can handle optional progress errors
explicitly; propagating them through its `lintel::Result` becomes the ordinary
redacted `handler_failed` response. Events already queued can be drained after
completion/cancellation, but no new event is accepted from a completed context.
There is no free-text progress field, implicit tracing, input recording or
persistence in this module.

## Stable errors and trust boundary

`ToolError` serializes a fixed `code`. It never attaches serde field paths,
request bytes, handler error chains or panic payloads. Codes include `denied`,
`tool_not_found`, `invalid_input`, `input_too_large`, `invalid_output`,
`output_too_large`, `busy`, `session_closed`, `scope_closed`, `cancelled`, `timeout`,
`handler_failed`, `handler_panicked`, and bounded registry/configuration errors.
Applications choose their own user-facing copy from these codes.

Ordinary unwinding task panics map to `handler_panicked`. A user/global panic hook
can still print its own diagnostics; Lintel does not install or replace that
hook. `panic=abort` builds, aborts, stack exhaustion and process failure cannot
return a recoverable tool error. The repository's dev profile uses abort; tests
exercise unwinding task panic behavior in the test profile. This layer does not
claim process isolation, arbitrary-code safety, secret-free output from a handler
that deliberately returns secrets, or a complete JSON Schema validator.

The standalone service example is `examples/agent_tools.rs`. It registers a
bounded integer tool, chooses a read-only policy explicitly, executes through the
real application context, drains numeric progress, closes the session and uses
the application's normal quit path. It has no model client or outbound request.

## Verification

The focused `agent::` group passed 17 tests covering policy-filtered discovery,
typed/domain failures, bounded input/output/catalog/progress, cancellation,
cooperative deadlines, real task destruction, scope/session isolation,
reserve/spawn/commit races and unwinding panic redaction. The complete affected
root library passed 127 tests. These are model/managed-runtime regressions, not
model-provider or process-isolation acceptance.

`examples/ai_native_app.rs` also ran as an external generated macOS application
whose only direct dependency is `lintel`. It obtained Store after plugin startup,
rejected a default-policy mutation, executed an exact-granted typed mutation,
persisted the counter across runs, and completed task cleanup on exit. A requested
initialization failure returned its original tagged error after cleanup. See
`build/ai-native/consumer-native-result.json` for that distinct native evidence.
