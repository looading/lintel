# lintel-process

Lintel's optional multiprocess foundation over the published `mojo-ipc` and
`mojo-service` SDKs, both pinned to `=0.3.0`. It reuses their real Mojo Core
bootstrap, typed Mojom interfaces, service directory, bounded calls, readiness,
restart budgets and health monitoring. It has no dependency on the GUI or core
application crates and creates no second async executor.

`ProcessHost::start(HostConfig)` starts one named OS thread that initializes the
broker runtime and owns the upstream `ProcessSupervisor`. All potentially blocking
bootstrap, polling, health supervision, graceful stop and child reap operations run
there. Startup returns after manager initialization, before service readiness;
inspect `snapshots()` / `subscribe_events()` or use directory discovery. Event
receivers must handle `Lagged` by reading a fresh snapshot.

For ordinary application code, use `host.wait_ready("echo-worker", timeout).await`
instead of writing an event loop. It subscribes before inspecting the snapshot,
handles event-ring lag by inspecting a fresh snapshot, and returns the Ready
`ProcessSnapshot`. To reconnect explicitly after a failed provider attempt, use
`host.wait_ready_after("echo-worker", previous.attempts, timeout).await`; it requires
a newer attempt and never replays business requests. The future owns its inputs
and is `Send + 'static`, suitable for the application's existing UI Task.

Readiness waits require a positive timeout up to 300 seconds and use the existing
Tokio timer with a fixed deadline from invocation. Unknown identities return
`NotFound`; terminal Failed/Stopped, host shutdown or closed event streams return
`BrokenPipe`; duplicate identities or invalid inputs return `InvalidInput`; an
expired deadline returns `TimedOut`. A registration identity should be unique.
Ready is a point-in-time snapshot, not a promise that the process stays alive.
With no required service or health configuration, upstream Ready means admission
only; require the actual service key/version when business readiness is needed.

Configure host and process permissions separately with `AccessPolicy`. Both deny
by default and grant exact service keys for publish/connect. Permission revocation
does not retract a business endpoint already transferred by the upstream SDK.
These are trusted processes, not an operating-system sandbox or an authenticated
network service. Business requests are never automatically replayed after crash.

## Lifecycle and limits

- At most 16 process registrations per host lifetime. Stopping an entry does not
  remove it from this limit. Initial entries and dynamically added entries share
  the same bound.
- Commands use `try_send` into a bounded queue, default 32 and maximum 256. A full
  queue reports `WouldBlock`; admission does not silently wait or drop a command.
- Events use a bounded broadcast ring, default 128 and maximum 4096. Cached process
  snapshots remain bounded by the registration limit.
- One directory client allows at most 32 unfinished control responses. Timeouts
  and dropped awaiters end only local waiting; the quota remains held until the
  response or disconnection. Explicit timeouts must be positive and at most 300s.
- A worker retains at most 64 published factory bindings. Its ordinary main can
  use `publish_blocking` and `wait` without constructing a Tokio runtime. Native
  business handlers and factory callbacks still run on the SDK dispatch sequence.
- `health_blocking` retains an explicit health binding for processes whose
  `ProcessSpec` enables health checking. Checks must be short and nonblocking;
  returning true only establishes that the selected check responded.

Dropping the final host handle requests shutdown without blocking the caller.
`request_shutdown()` is also nonblocking. **Only awaiting `shutdown()` establishes
that supervisor cleanup has completed**; cleanup errors and a panicked supervision
thread return errors rather than successful completion. The future can be awaited
on any executor; it does not require Tokio timers. Directory async methods do use
the application's existing Tokio timer.

Directory `connect`, `list` and `watch` synchronously submit their control request
and return a `DirectoryRequest<T>` that is explicitly `Send + 'static`. Their
deadline starts at submission, including a caller that delays the first poll.
Callbacks arriving after that deadline cannot return success. A response completed
in time can still be collected by a later poll. Dropping the returned future does
not retract the submitted operation, and its control-response quota stays reserved
until the reply or disconnection.

`ServiceClient<T>` adapts the upstream typed client for ordinary `Send` UI Tasks.
Its synchronous `call` sends one response-bearing RPC and returns a
`ServiceCall<R>` that does not carry the interface type. This future keeps its
client owner alive without forcing Mojom trait-object projections into the task
state. It preserves upstream timeout, cancellation and outstanding-response
limits. Shared/reentrant send attempts use `try_lock` and return `WouldBlock`
instead of blocking a UI/dispatch thread; a panicking send closure leaves the
client terminal rather than silently recovering partially modified state.
`is_connected`, `outstanding_requests` and `pending_calls` report errors for busy
or poisoned clients, so contention is not confused with a disconnected pipe.
Already-sent callbacks keep their original slots until reply/disconnect if the
client is otherwise retained. There is no unsafe Send implementation or change to
upstream generated bindings.

For ordinary application code,
`directory.connect_client::<dyn Echo>(&calls, &key, version, capacity, timeout)`
combines directory connection and typed client binding. Its owned, `Send + 'static`
future returns `ConnectedService { client, generation }`, retaining the exact
generation returned by routing. It immediately submits through the existing
directory request, preserving the submission deadline, delayed-poll and quota
semantics. It adds no timer, implicit permission, retry or request replay. Zero
capacity or an already closed call context is rejected before native submission;
binding errors retain their concrete `CallError` as the I/O error source.

Directory submission uses a nonblocking lock: contention returns `WouldBlock`,
poison fails without recovering possibly damaged state, and rejected requests
release their unsubmitted quota. Readiness remains an explicit call;
`wait_ready(identity)` does not bind key/version routing to that process identity.
The original `connect` and `ServiceClient::bind` remain available for lower-level
integration.

Shutdown is bounded by configured upstream bootstrap and shutdown-grace operations,
which cannot be interrupted by closing the command queue. Multiple processes may
consume those intervals sequentially. Stop requests do not promise to undo remote
business side effects, and process crashes cannot resolve application-level
idempotency or transaction ambiguity.

## Native runtime and packaging

Construct `TrustedRuntimeLibrary` explicitly from an absolute library path, or use
`TrustedRuntimeLibrary::bundled()` in a CLI-created app. Both constructors are
unsafe because loading native code requires a genuine trust decision. File
existence and SDK ABI/revision checks alone do not authenticate native code. There
is no fallback to cwd, PATH or a runtime-library environment variable.

The bundled resolver uses `Contents/Frameworks/libmojo_rs_native.dylib` for macOS
main/helper executables, app-root `mojo_rs_native.dll` on Windows, and a sibling
`libmojo_rs_native.so` for a Linux executable (one level above `Helpers` for a
helper). `service_executable(name)` accepts an extension-free ASCII basename;
macOS uses `Contents/Helpers`, Windows uses the existing bundle root, and the
Linux development convention uses `Helpers`. Explicit development paths remain
available through the trusted library and `ProcessSpec` configuration.

For CEF helper app entries, the macOS resolver recognizes only the existing
`outer.app/Contents/Frameworks/<name> Helper[ (role)].app/Contents/MacOS` layout and
uses the outer app's runtime and service paths. This permits constructing the
application configuration before CEF role dispatch. It does not search arbitrary
ancestors or test paths outside that bundle, and these path regressions alone do
not establish combined native CEF/Mojo acceptance.

The worker reserves stdin for the upstream parent-liveness watcher before loading
the participant runtime. It exits on parent-liveness EOF; use `wait()` or
`disconnected().await` to finish gracefully when the host closes its admission
connection. Only direct supervised children are managed. Untracked grandchildren,
arbitrary executables, independent Chromium versions and role switching are not
supported by this layer. Mojo native code, IO/dispatch threads and the selected
broker/participant role remain fixed until process exit.

The upstream source and runtime provenance are available at
[looading/mojo-ipc](https://github.com/looading/mojo-ipc). Ordinary package tests
do not load native code and must not be reported as native IPC acceptance.

## Optional worker crash reports

Enable this crate's `crash-reporting` feature, or enable both `multiprocess` and
`crash-reporting` on the Lintel facade. The latter uses weak feature forwarding:
ordinary native crash reporting does not implicitly enable multiprocess support.

```rust,ignore
let mut config = crash::CrashConfig::new(
    application_reports,
    service_executable("lintel-worker-crash-helper")?,
);
config.application_id = "dev.example.app.echo-worker".into();
config.version = env!("CARGO_PKG_VERSION").into();
let worker = ProcessWorker::connect_with_crash_reporting(
    library, "echo-worker", config,
)?;
// Existing publish/wait APIs work through Deref.
```

This opt-in entry validates the SDK's positive decimal supervisor attempt,
reserves stdin for parent liveness, installs the existing native reporter and
only then calls the original Mojo worker connection. `CrashReportingWorker`
owns the ordinary worker before its reporter; native factory/directory owners
are released before the hook/helper. The old `ProcessWorker` layout and
Send/Sync contracts are unchanged. Startup failure releases the local reporter.
`crash_report_directory()` and `crash_helper_pid()` expose local observations.

The report records the explicitly supplied service identity and supervisor
attempt, not an authenticated identity or a startup-time directory generation.
No endpoint, token, request payload or environment snapshot is included. The
normal crash report root/helper paths must be absolute and application-owned.

The SDK parent watcher exits the worker directly on stdin EOF; it does not run
Rust Drop or create a native dump for that normal exit. Its crash helper has a
separate private control-pipe EOF path and must be observed to exit independently.
A metadata-only run directory can remain after abrupt process exit; it is not
a completed crash report. Native fatal signals and the actual target's capture
support differ from forced termination, OOM and application-level errors.
