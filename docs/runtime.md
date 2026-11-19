# Runtime and lifecycle

`lintel::runtime::AppBuilder` owns the winit event loop, the application context,
Tokio executors, iced, and optional native desktop services. Run it on the
application's main thread. Its `run` method returns startup, event-loop, and
replacement-process errors to the caller.

## Names and configuration

Framework types, environment variables, bridge attributes and default paths use
`Lintel`, `lintel` and `LINTEL_*`. Set an explicit reverse-DNS application ID to
keep each application's directories independent. The fallback organization is
`lintel`; runtime configuration is read only from `lintel-runtime.yml`.

Environment values are `development`, `staging` and `production`, with production
as the default. Removed application-specific configuration fields and names are
not accepted as compatibility aliases. This is an intentional early API/schema
cleanup. Generic proxy, UI performance and application-owned tool configuration
remain available; token fields are API inputs, not embedded credentials.

The published source and CLI archive use the same vendored public renderer and
pinned public dependency graph. `public-iced`, `public-cef` and `webview-cef`
remain capability/compatibility entry points; no private backend is selected by
turning one of them off. Low-level Windows CEF initialization always requires the
official bootstrap and sandbox context.

## Plugins

Implement `Plugin::name` and `PluginLifecycle`. Names must be nonempty and unique;
the runtime validates the complete list before calling any startup hook.

`on_start(&AppContext) -> anyhow::Result<()>` runs in registration order. Its
default implementation calls the existing `on_init` hook, so existing plugins
continue to work. Callbacks execute outside the plugin-manager mutex and service
table lock; they can query plugins, register services, and resolve services.

If startup fails, the failing plugin and previously attempted plugins receive
`on_stop` in reverse order. The failing hook participates because it may already
have acquired resources. `on_stop` defaults to the existing `on_exit` hook.
Registered plugins then receive `on_drop` in reverse order. Normal shutdown uses
the same once-only cleanup. A failed or stopped manager cannot restart.

Prefer `AppContext::start_plugins` over locking the public legacy manager and
calling hooks manually. Register plugins before startup. Plugin implementations
remain `Send + Sync`; thread-affine native objects belong to the runtime owner,
and plugins should keep the thread-safe `DesktopService` sender.

`AppContextBuilder::try_build` reports missing fields and native initialization
errors. The old `build` method remains available and panics on a build error.

## Scoped async work

Use `AppContext::spawn_task` for work that should end with an application, window,
or page. Direct use of the legacy public Tokio handle bypasses lifecycle tracking.

```rust,ignore
use lintel::core::tasks::TaskScope;

let scope_id = windows.scope_id(window_id)
  .ok_or_else(|| anyhow::anyhow!("window already closed"))?;
let task = context.spawn_task(TaskScope::page(scope_id, "settings"), async {
  load_settings().await
})?;
let cancellation = task.cancellation_handle();

context.cancel_tasks(&TaskScope::Window(scope_id));
let result = task.join().await; // JoinError::is_cancelled identifies cancellation.
assert!(cancellation.is_finished());
```

`Application` owns all tasks. `Window(u64)` includes that window's `Page` scopes;
cancelling one page does not cancel its siblings or its window-level work. The
managed window APIs cancel their window scope during close. Dropping a task
handle does not detach it from its owner.

Iced window IDs are opaque; use `WindowManager::scope_id` to obtain the managed
task scope key instead of converting the native window ID yourself.

`TaskHandle` provides `cancel`, `cancellation_handle`, `is_finished`, and `join`.
`TaskTracker::cancel_scope` requests cancellation; `wait_scope` waits until those
Futures have been destroyed. `shutdown` permanently rejects new tasks, and
`wait_idle` waits for all Future cleanup. Page cancellation allows a later visit
to start new tasks in the same logical page scope.

Async tasks must yield. Tokio cancellation cannot interrupt a synchronous native
function or an already-running `spawn_blocking` closure. Keep synchronous command
handlers bounded and hold any concurrency permit inside the blocking closure.
After UI/CEF cleanup, executor destruction waits at most two seconds per Tokio
runtime for remaining blocking work. A timed-out blocking function can continue
on its worker thread; this timeout does not forcibly terminate it.

`AppContext::shutdown().await` cancels tracked tasks, waits for their Future
destructors, then stops plugins and releases resources/services. Repeated and
concurrent calls wait for cleanup completion. The synchronous `destroy` method
is a compatibility entry point that requests task cancellation without awaiting
Future cleanup. Avoid awaiting `shutdown` from a task it is responsible for
cancelling.

## Ordered program messages

`IcedAppPluginApi::try_dispatch(Arc<dyn AnyMessage>)` admits a message through the
same FIFO as program creation and close commands. `IcedAppPluginApiExt::dispatch`
is the typed convenience method. Both return `ProgramCommandError` for a full
queue, stopped renderer, stopping application, or unavailable dispatch support.
Success means admission; application shutdown can still cancel queued work.
The compatibility `create`/`close` methods remain available.

High-level activation and desktop callbacks use this FIFO rather than sending
an independent event-loop message. This preserves Create-before-update ordering
even when channel capacity is exhausted or the subscription has not started.
Before application setup, at most 64 activation events are buffered; after setup,
they enter the ordered program path. This covers cold macOS URL delivery and the
initial-launch message as well as later secondary processes.

## Main-thread work and native desktop events

`run_on_app_main_thread` and `send_task_event` submit nonblocking event-loop
callbacks. At most 1024 callbacks may be pending; a full queue or a shutting-down
application returns an error. A dropped or executed callback releases its slot.

With the `desktop` feature, `AppBuilder::with_desktop` registers
`Arc<lintel::desktop::DesktopService>` in the context. The `!Send` native owner
stays on the event-loop thread. Use `with_desktop_event_handler` to receive
logical menu, tray, and shortcut events; it also enables desktop services.
Native command submission is bounded. `Pending<T>` implements `Future`, so normal
iced tasks can await it without blocking native dispatch. Blocking `wait` and
`wait_timeout` on the dispatch thread are rejected; synchronous workers may use
them, or a host may use `try_recv` after draining commands. A dropped runtime/job
wakes pending futures with a closed-runtime error.

Managed multi-window boot callbacks can obtain the registered service through
`windows.desktop()`. The public iced adapter additionally provides
`with_native_window(id, callback)` after `Opened`, with an owned native window
instead of a borrowed platform handle. Release that owner after detaching its
integration. On Windows, `bind_menu` combines this task with scoped registration
and menu attachment. Keep the returned lease until the manager's logical `Closed`
event, then drop it; do not wait for Win32 destruction of the retained window.
The desktop runtime sweeps expired leases on a direct wake, including cancellation
or a full command queue. The high-level Windows `.desktop()` runtime installs accelerator preprocessing
before event-loop creation and binds its dispatcher after creating desktop services.
Independent Windows hosts connect `WindowsMenuMessageHook` themselves.

Linux services enqueue typed events and wake the owner; the runtime drains up to
64 background events per turn and reports the explicit overflow counter. Linux
startup does not construct the legacy implicit X11 hotkey manager. Applications
select an X11 controller or a portal session explicitly through the optional
Linux service; portal binding is never a startup side effect.

## Native control semantics

With `desktop` and `public-iced`, `.accessibility()` installs a native factory on
the existing runner. The factory creates AccessKit before showing each native
window. A targeted widget operation collects the real layout after redraw;
native input/action callbacks only queue plain requests and wake the UI thread.
The runner releases the adapter before the window's last owner, including normal
close, explicit event-loop exit and terminal renderer errors. Native integration
errors are returned by the caller-owned runner instead of being silently dropped.

The `a11y` helpers retain ordinary typed iced messages. Stable keys identify the
controls, nested scroll offsets and clipping constrain their bounds, and disabled
or clipped controls leave the keyboard focus order. Focus identity and revision
share one locked token so an old blur cannot overwrite a newer same-node Focus.
Text inputs synchronize their real focus after handling input. Unknown custom
widgets require declared semantics; password values are omitted from snapshots.

## Quit, exit, restart, and activation

`quit`, `exit(code)`, and `relaunch(args)` follow `Init -> Waiting -> Done`.
Duplicate initial requests do not run cleanup again. Restart creates the
replacement process after cleanup, event-loop return, and native-owner release.
`None` preserves captured application arguments; `Some` replaces all arguments
after the executable name. On macOS, the current bundle's `CFBundleExecutable`
selects the executable after an update. Spawn failures are returned to the host.

Managed CEF windows register a `QuitCoordinator` service. The runtime consults it
before accepting an initial quit, exit, or restart. `Defer` leaves the event loop,
CEF pump, and task tracker active while browsers close. The coordinator resends
the original initial event after the final browser and native window close.
Only `Proceed` starts application cleanup; a coordinator error leaves the
application running. This also covers direct calls to `context.quit()`.

With the `desktop` feature, all high-level application builders and `AppBuilder`
provide `.single_instance()` and `.protocol_schemes(["my-app"])`. The process
claims a dedicated per-user instance directory before plugins, business-state
initialization, or the GUI event loop are created. A secondary process forwards
its argv, absolute working directory, and allowed URL arguments, waits for the
primary's queue acknowledgment, and returns without creating another GUI. The
queue is bounded; failures remain errors. The primary's guard remains on the
main thread through native cleanup. Transport paths use a stable compact SHA-256
namespace under the per-user runtime/temp root (including the Unix UID), so a
long valid application ID cannot exceed the Unix socket path limit. WebView helpers take the CEF helper branch
first, and the browser's guard is additionally retained until CEF shutdown. A
WebView relaunch is handed to the outer CEF owner: finish CEF shutdown, release
the primary guard, then create the replacement, preventing it from forwarding
to the old instance and exiting.

`with_activation_handler` / `.on_activation(...)` receive the existing
`ActivationEvent::Opened { urls }` and `ActivationEvent::Reopen { has_visible_window }`,
plus `ActivationEvent::Launch { arguments, working_directory, urls, source }`.
`ActivationSource` distinguishes the initial launch from a secondary instance.
Startup messages arrive after application setup; later launches wake the event
loop directly. A launch queued before the event-loop proxy exists schedules a
wake when that proxy is attached. Activation draining is bounded and uses no
periodic polling timer.

An explicit scheme list is validated at startup, extracts matching URL arguments,
and validates complete native/forwarded URL batches before delivery. An empty or
invalid configured list is an error. Without an explicit scheme policy, the
legacy OS `Opened` event behavior is preserved, and no URLs are inferred from
argv. Arguments and file paths remain untrusted data for the application to
interpret. Non-UTF-8 argv cannot be forwarded by the String-based transport and
returns an error instead of silently changing a path.

The selected public `iced-rs/winit` revision
(`48116469f15d3bc52b5e56bb516694be8882870d`) delivers native macOS URL callbacks.
Lintel forwards its `received_url` callback into the same configured scheme
policy and activation handler. Winit owns its retained NSAppleEventManager GURL
handler; Lintel does not replace the NSApplication delegate or install a competing
AppleEvent target. For Dock reopen, the adapter adds only the missing `applicationShouldHandleReopen:hasVisibleWindows:`
selector to the known winit delegate, rejects replacing an unknown implementation,
and keeps a bounded main-thread weak subscription. Its guard removes the active
subscription on shutdown. It delivers the typed Reopen event through the
existing activation inbox; visible and hidden windows have real `open -a` probes.

A different host can use `lintel::activation::forward_urls(&context, urls)` for
an explicit delegate/portal integration; this hook requires
`.protocol_schemes(...)` and validates all URLs before queueing the event. The
unpatched registry winit API is not the public profile selected by this project.
URL associations remain bundle/installer metadata: the macOS bundler emits
CFBundleURLTypes from repeated `--protocol-scheme` arguments before signing.
No default-handler system setting is changed. Cold protocol launches delivered
through argv also pass through the single-instance preflight.

For a macOS bundle, repeat `lintel bundle --protocol-scheme lintel-example` for
each supported scheme. The packager validates scheme syntax and emits
`CFBundleURLTypes` before signing. This declares handlers in the selected bundle;
it does not change the user's system-wide default application. The additive
`BundleExtensions` Rust API combines these declarations with extra signed process
helpers while preserving existing `BundleOptions` initializers.

`examples/activation.rs` is a real two-process probe: start the binary with
`--expect-secondary`, wait for `PRIMARY_BOOT`, then launch the same binary with
`lintel-activation://open/42 --from-secondary`. The primary reports the received
argv/cwd/URL and quits; the secondary prints `PROCESS_EXIT` without `PRIMARY_BOOT`.
The primary has a ten-second timeout and reports failure if delivery never occurs.
Use the same `--app-id=org.example.unique-probe` for both processes to isolate a run.

Core lifecycle/task regression tests run with:

```sh
cargo test -p lintel-core --lib
```

For the native URL route, bundle the activation example with a unique protocol
scheme and start it with `--expect-protocol --protocol-scheme=<scheme>`. While
it is running, `open -a <unique-demo.app> <scheme>://open/42` must produce a
`PROTOCOL_ACTIVATION` marker and a clean application exit. This exercises actual
LaunchServices/AppleEvent delivery rather than argv forwarding.

For the Dock path, start the bundled activation example with `--expect-reopen`
(optionally `--show-window`), wait for the initial-state marker, then use
`open -a <demo.app>` without a URL. Expect `REOPEN_ACTIVATION` with the appropriate
visible-window flag, no `SECONDARY_ACTIVATION`, and a normal process exit.

Linux extraction uses XDG local data paths and an adjacent resources directory.
Legacy Muda/Tray event types and automatic legacy hotkey ownership are limited to
the platforms that implement them; Linux forwards the typed optional backend
events instead. The legacy process-cleanup utility is not exposed on Linux
without an actual implementation.
