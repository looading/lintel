# Context-aware, fallible native initialization

`application_with_context` and `multi_window_with_context` build application state after plugin startup, using the application's actual `AppContext`. They are additive native entry points: existing `application`, `multi_window`, update/view callbacks and subscriptions remain available.

Applications can read registered stores, tool registries and other services directly during initialization. They do not need to save the context in a business-owned global `OnceLock`, add a model SDK, or construct another runtime.

## Single-window application

The initializer has the shape `Fn(&AppContext) -> Result<State>`. Update still returns `Task<Message>` and the view still borrows state.

```rust,ignore
use std::sync::Arc;
use lintel::{
    AppContext, Error, Result, application_with_context,
    iced::{Element, Task, widget::text},
};

struct SettingsService {
    title: String,
}
struct State {
    settings: Arc<SettingsService>,
    count: u64,
}
#[derive(Clone, Debug)]
enum Message {
    Increment,
}

fn main() -> Result<()> {
    application_with_context(
        |context: &AppContext| {
            let settings = context.get_service::<SettingsService>()
                .ok_or_else(|| Error::msg("settings service was not registered"))?;
            Ok(State { settings, count: 0 })
        },
        |state: &mut State, Message::Increment| {
            state.count += 1;
            Task::none()
        },
        |state: &State| -> Element<'_, Message> {
            text(format!("{}: {}", state.settings.title, state.count)).into()
        },
    )
    .id("dev.example.context-counter") // Required, as with application(...).
    .plugin(|context| {
        let service = Arc::new(SettingsService { title: "Context counter".into() });
        context.register_service(move || service.clone());
    })
    .run()
}
```

A plugin can instead register a persistent Store or an application-owned AI/tool service. Its creator and synchronous `on_start` hook complete before the initializer runs. Asynchronous work spawned by that hook may still be running; the initializer is not an implicit await-all-services barrier.

The initializer is synchronous and runs on the application's UI thread during the first event-loop Init. For asynchronous tool/model work, retain service handles in state and use ordinary `Task<Message>` or `AppContext::spawn_task` with an appropriate task scope. Returning a later task failure is normal application-message handling, distinct from returning `Err` from this initializer.

## Multiple windows

The boot closure has the shape:

```text
Fn(&AppContext, WindowManager) -> Result<(State, Task<Message>)>
```

It receives both context services and the same lightweight WindowManager supplied by `multi_window`. With desktop services enabled, the manager has its existing desktop service installed before boot.

```rust,ignore
use lintel::{multi_window_with_context, AppContext, Result, WindowManager};
use lintel::iced::{Element, Task, widget::text};

struct State {
    windows: WindowManager,
    title: String,
}
#[derive(Clone, Debug)]
enum Message {
    Ready,
}

fn main() -> Result<()> {
    multi_window_with_context(
        |context: &AppContext, windows: WindowManager| {
            let title = context.app_info.name.to_string();
            windows.open(title.clone(), Default::default())?;
            Ok((State { windows, title }, Task::done(Message::Ready)))
        },
        |state: &mut State, _message: Message| {let _=&state.windows;Task::none()},
        |state: &State, _window| -> Element<'_, Message> {
            text(&state.title).into()
        },
    )
    .id("dev.example.context-windows")
    .run()
}
```

WindowManager open calls during boot queue ordinary commands/tasks. They are consumed only after boot succeeds and the program is admitted to the Iced service. If boot returns an error, its unconsumed command receiver and initial tasks are dropped; an escaped manager then reports `ManagerClosed` instead of creating a window.

Keep `.id(...)`: context-aware initialization does not supply a default application identity. Missing/invalid IDs are configuration errors and can be reported before the context or GUI is initialized.

## Startup and failure order

The native lifecycle remains shared with the existing runner:

1. Process-helper/single-instance preparation occurs as before. A forwarded secondary launch does not initialize business state.
2. AppContext is built, plugin creators run, runtime services are installed, and `start_plugins()` completes its synchronous startup transaction.
3. At `StartCause::Init`, setup calls the chosen initializer/boot once. Existing legacy callbacks use the same phase; their captures remain held until runner teardown as before.
4. Only a successful initializer reaches `IcedAppPluginApi::try_create`. The resulting program then uses the normal boot, Task, subscription and managed-window path. Buffered activation delivery becomes ready only after successful setup.

On `Err`:

- The failed initializer does not submit a program, so the facade does not create a managed window. A multi-window boot's queued commands remain unconsumed and are dropped.
- The runner stores the original `anyhow::Error` without formatting it into a new error. A typed renderer-admission failure, such as `ProgramCommandError::QueueFull`, is likewise propagated through `run()`.
- The runner stops polling Iced and stops delivering activation/native-desktop handler events. It follows the existing QuitCoordinator and Init → Waiting → Done cleanup path instead of starting a second exit loop.
- The existing main-thread Task channel, quit completion and optional CEF tick/pump path remain available for cleanup. Cancelable background work should use the existing tracked task scopes. This matters for low-level hosts that already supplied a CEF pump or a deferred quit coordinator.
- `AppContext::shutdown()` cancels and awaits tracked task cleanup, stops plugins and releases resources/services using the existing lifecycle. After the event loop returns, shutdown is awaited idempotently and `run()` returns the original initialization error before generic exit-status handling.

The contextual native entry points do not initialize CEF or add browser windows. A low-level `AppBuilder` host that already owns CEF keeps its existing pump/coordinator responsibilities. Likewise, the facade cannot undo arbitrary I/O or native UI that user plugin/initializer code performs itself; the no-window guarantee concerns programs/windows admitted by this facade after successful initialization.

If an application replaces the standard Iced service, that service must support typed `try_create` admission. An older create-only service returns `CreationUnsupported`, which now reaches the caller rather than being logged and swallowed.

## Low-level setup

`AppBuilder::try_run` accepts `Fn(&AppContext) -> Result<()>` at the same post-plugin Init phase. Its error uses the cleanup behavior above. Existing `AppBuilder::run(Fn(&AppContext))` wraps a successful result and delegates to the same runtime implementation; there is no duplicated event loop.

Initialization is one attempt per runner. A repeated Init does not call the initializer twice, and taking the stored error does not make a failed runner ready again. This contract handles returned errors; it does not convert arbitrary user panics into Result values.

## Source compatibility

- `application(...)` still returns `Application<Initialize, Update, View>` with the original initializer closure type.
- `Application` still accepts explicitly three generic parameters; `NoSubscription` remains the default fourth parameter.
- Existing `Fn() -> State` and `Fn(WindowManager) -> (State, Task<Message>)` callbacks keep working through small initialization adapters.
- Existing `.subscription`, activation, desktop-event and window configuration methods apply to the new builders too.
- Update/view signatures, borrowed view lifetimes, Message routing and ProgramId selection are unchanged.

`ContextInitialize` / `ApplicationInitializer` and `ContextBoot` / `MultiWindowBoot` are doc-hidden, nameable adapter contracts for generic builder code. Ordinary applications use the entry functions and type inference, without naming these contracts or implementing another framework trait.

## Verification boundary

Focused regression tests cover successful service access, one-time setup, original-error identity, no program admission after initialization failure, typed admission errors, unadmitted multi-window command cleanup, capture lifetimes, default subscription/type inference and old generic builder source compatibility. The cleanup regression uses the real TaskTracker and ServiceStore to check task-drop-before-service-release and waiting before returning an error.

Those headless tests do not construct a fake native AppContext or use unsafe memory. The separate macOS consumer probe uses a real application context and event loop. Existing core plugin lifecycle tests remain the evidence for reverse-order/idempotent plugin shutdown, while the new path deliberately invokes the same AppContext shutdown implementation.


Completed checks:

| Check | Result | Evidence |
| --- | --- | --- |
| Root public-profile library check | Passed | `build/ai-native/agent-context-check-after-fix.log` |
| Root `context_` regression filter | **10 passed, 0 failed; 37 filtered out** | `build/ai-native/context-after-fix-tests.log` |
| Application compatibility group | **5 passed, 0 failed**, including the legacy three-generic configuration | `build/ai-native/application-compat-tests.log` |
| Full affected root library | **127 passed, 0 failed** | `build/ai-native/root-affected-regressions.log` |
| External one-dependency consumer on macOS | Two normal runs persisted 2 then 4; owned tasks finished; requested initialization error retained its type and was returned after cleanup | `build/ai-native/consumer-native-result.json` |

The consumer's only direct dependency is `lintel`; it enables `public-iced`, `agent` and `services`. The failure probe exits successfully only after observing the requested runtime error and managed-task cleanup; its probe exit code is not a claim that runtime initialization succeeded. Windows/Linux target checks remain separate from native startup/cleanup acceptance. The contextual entry does not initialize or validate native CEF.
