# lintel-desktop

Native desktop capabilities for Lintel applications. Enable macOS/Windows native backends with `features = ["native"]`, add `linux` (or its individual features) for GTK-free Linux tray/shortcuts, or select individual capabilities. Default builds keep the API and activation/protocol support available while returning `DesktopError::Unsupported` for native features that were not compiled.

```toml
lintel-desktop = { path = "../lintel/crates/lintel-desktop", features = ["native"] }
```

The package is an independent workspace. It does not pull in iced, CEF, or Lintel's internal windowing fork.

## Ownership and event loop

`DesktopRuntime` owns menu, tray, shortcut-manager, and clipboard objects and is **neither `Send` nor `Sync`**. `DesktopService` is a cloneable `Send + Sync` command sender. Commands carry owned data and typed resource IDs; native objects never enter an application service store. On macOS, constructing a native UI runtime from a worker fails with `MainThreadRequired`.

1. Create `(runtime, service)` with `DesktopRuntime::new(wake)` on the application UI thread. The callback sends a wake event through the host event-loop proxy.
2. Keep the runtime in the UI application state. Register a clone of the service in the host's service registry if desired.
3. After the native event loop starts, call `runtime.drain(64)` in the wake-event handler. A full batch schedules another wake, so command traffic does not monopolize the event loop. The command queue is bounded at 128 entries and reports `QueueFull` under pressure.
4. Forward existing native events with `runtime.menu_event`, `runtime.tray_event`, and `runtime.shortcut_event`. Each returns `Option<DesktopEvent>`; foreign event IDs are left unhandled. The package installs no global event handlers and never consumes the host's global native event receivers.
5. With a Linux feature, call `runtime.take_events(64)` after commands and route those typed events on the host thread; report any `take_event_overflow_count()`. Background callbacks only queue and wake.
6. Drop the runtime on its owning thread before the OS event loop exits. Shortcuts are unregistered, the application menu detached, and trays removed before their menus.

Native tray creation must happen **after the event loop is running** (for winit, `resumed` or a subsequent event). A runtime constructor does not create a tray or shortcut manager. On Windows, use the same thread as the Win32 message loop for construction, draining, and drop.

Each service method returns `Result<Pending<T>>`: the outer error covers validation, shutdown, or a full queue; the pending result contains the actual native operation's outcome. `Pending<T>` is also a `Future<Output = Result<T>>` and may be awaited directly by an executor while the native UI loop keeps running. Replies use oneshot wakeups, including runtime/job drop; there is no polling timer and no Tokio dependency. Workers may still call `wait` or `wait_timeout`. The dispatch thread must use nonblocking await or drain then `try_recv`; a premature synchronous wait returns `WouldBlockMainThread`. Blocking an executor on that same UI thread while waiting for it to drain commands is still incorrect. Dropping/timing out a pending result does not cancel a queued native side effect. Scoped window registration is a deliberate exception: dropping its result drops the lease and schedules cleanup.

```rust,no_run
use lintel_desktop::{DesktopRuntime, MenuEntry, MenuSpec};

# fn host() -> lintel_desktop::Result<()> {
let (mut runtime, service) = DesktopRuntime::new(|| {
    // host_event_loop_proxy.send_event(DesktopWake);
})?;
let created = service.create_menu(MenuSpec {
    items: vec![MenuEntry::item("open", "Open")],
})?;
// In the UI event handler, after the native event loop has started:
runtime.drain(64);
let menu = created.try_recv().expect("command drained")?;
# let _ = menu;
# Ok(())
# }
```

Menus support normal items, check items, separators, nested submenus, accelerators, enabled-state updates, and checked-state updates. Their action IDs must be nonempty and unique within one menu. The same menu can back multiple tray icons. On macOS/Windows, removing a menu detaches it from trays and the macOS application menu. Linux rejects removal while a tray still refers to the menu. Icons use validated RGBA buffers instead of an implicit image decoder.

The root `lintel::menu::menu_bar` adapter also provides a [client-rendered window-local menubar](docs/client-menubar.md), including Linux, using this package's dependency-free `client_menu::{MenuModel, MenuSession}` controller. It is distinct from native/global menu attachment and preserves ordinary business Message/Task dispatch.

## Capabilities and platform status

`availability(Capability)` distinguishes compiled availability, a disabled Cargo feature, and an unsupported platform. Availability does not assert that OS permissions, a graphical session, or external system services are present; native errors are propagated.

| Capability / feature | macOS | Windows | Linux |
| --- | --- | --- | --- |
| Menus, tray, global shortcuts / `native-ui` | Native AppKit; main thread and live event loop | Native Win32; owning message-loop thread | Use explicit Linux features below |
| GTK-free tray / `linux-tray` | Unix protocol fixtures only | Unsupported | StatusNotifierItem + DBusMenu; watcher/host required |
| X11 shortcuts / `linux-x11-shortcuts` | Unix protocol/logic checks only | Unsupported | Explicit x11rb connection, checked grabs, scoped cleanup |
| Portal shortcuts / `linux-portal-shortcuts` | Private D-Bus fixture only | Unsupported | Explicit app session and consent, actual subset/remapping, cancel/close |
| Application menu / `native-ui` | Global native menu via `muda` | Use `windows-menu` window attachment | Unsupported |
| Window menu / `windows-menu` | Unsupported; macOS uses its global menu | Retained owned window, typed target/lease, explicit message hook | Unsupported; needs a real GTK host adapter |
| Clipboard text / `clipboard` | Native pasteboard | Native clipboard | arboard X11 backend; display and clipboard ownership required |
| File dialogs / `dialogs` | Native panels | Native dialogs | `rfd` XDG portal backend; desktop portal service required |
| Notifications / `notifications` | Installed bundle ID required; process-wide identity | Installed AppUserModelID and OS toast setup required | Desktop D-Bus notification service |
| URL/file opener / `opener` | System associated application | System associated application | System opener installed and usable |
| Secure credentials / `credentials` | OS Keychain via `keyring` apple-native | Credential Manager via `keyring` windows-native | Persistent D-Bus Secret Service with Rust crypto; provider/session required |
| Single-instance launch forwarding | Owner-only Unix socket and OS file lock | ACL-private discovery + authenticated loopback + OS file lock | Owner-only Unix socket and OS file lock |
| Accessibility provider / `accessibility` | AccessKit native NSAccessibility adapter | AccessKit UI Automation adapter | AccessKit AT-SPI adapter; desktop accessibility bus required |
| System permission API / `permissions` | Native notification status/decision plus AX, screen recording, input monitoring status/explicit guidance | Toast notification status; no universal toast request API; other TCC-style gates NotRequired | Universal status/request unsupported; host must use actual portals/session policy |
| Protocol URL routing | Explicit scheme allowlist; host/installer supplies association and delivery | Same routing contract; host/installer supplies delivery | Same routing contract; desktop entry/installer supplies delivery |

A notification contains an explicit `application_id`, title, and body. On macOS, configure one installed bundle identity for the process; another identity is rejected. The backend returns OS submission errors, but a successful call does not establish that notification permissions were granted or that a banner was visible. The package does not request notification permission, install a shortcut, or impersonate a different installed application.

File dialogs return an optional `FileSelection`. `None` means `rfd` returned no selection, usually cancellation. **The upstream synchronous `rfd` API does not distinguish cancellation from native dialog failures.** File filters are a convenience; validate selected paths/content in the host. The dialogs are modal and run on the UI thread.

External URL opening accepts only `http`, `https`, and `mailto`. Custom application URLs go through `ProtocolRouter`; routing does not execute another program. `open_path` canonicalizes an existing path before passing it to the system opener. A webview bridge must separately authorize requests; this crate is not a permission policy.

## Secure credentials

`CredentialStore::new("com.example.application")` names a service namespace. `set(account, &Secret)`, `get(account)`, and `delete(account)` use the real secure OS store. A missing entry is `None`/`false`; access failures remain errors. The store can prompt or block, so use it from a worker.

`Secret` redacts `Debug` output and zeroes its owned string on drop. `expose_secret` intentionally returns a borrowed string for use by the host. The host is responsible for copies, logs, protocol payloads, and JS exposure. There is no secure credential fallback when a backend is unavailable. On Linux, credentials explicitly instantiate the persistent Secret Service builder even if Cargo feature unification enabled another keyring backend. The desktop session needs a Secret Service provider; missing/locked/unavailable services return errors, and explicit credential operations may require provider unlock UI. See [Linux integration paths](docs/linux-backends.md).

## Unified permissions

`permissions::support(PermissionKind)` reports **status** and **request** support separately. `status(kind, &PermissionContext).await` is passive. Only an explicit `request(kind, &context).await` may ask the OS to display permission UI; constructing and dropping its future is lazy and does not request anything. Runtime setup and automated smoke probes never call request.

| Permission | macOS status / request | Windows status / request | Linux |
| --- | --- | --- | --- |
| Notifications | Native NotDetermined/Denied/Granted/Provisional/Ephemeral/Unknown; native completion decision | ToastNotifier.Setting using installed AppUserModelID; request explicitly Unsupported | No universal grant API; use actual notification/session backend |
| ScreenRecording | Preflight observation; explicit native prompt request | No comparable per-app TCC gate (NotRequired) | Portal/session-specific, Unsupported here |
| InputMonitoring | Native granted/denied/not-yet-asked; explicit request | NotRequired for the comparable gate; protected resources still restricted | Portal/session-specific, Unsupported here |
| Accessibility | Current client trust; explicit guidance request | NotRequired for the comparable gate; elevation/UIAccess may still be needed | AT-SPI/session-specific, Unsupported here |

Accessibility and screen recording expose only a boolean observation on macOS. A false result is `NotGranted`, not an invented Denied/NotDetermined state. Screen recording grants can require restarting the process. Input monitoring and notification APIs expose richer states, which are preserved. `PromptRequested { current }` reports submission and the current observation; the OS may suppress a previously decided dialog, so it does not promise that UI appeared or access was granted. A notification request returns `Completed(actual_status)` after the native completion and a fresh settings query.

Mac notification status/request uses the process's actual identified `.app` bundle and never installs/replaces UNUserNotificationCenter.delegate or application identity. A plain CLI binary returns a precondition error rather than invoking a bundle-only center. Windows notification status requires the installed AppUserModelID in `PermissionContext.application_id`; macOS does not impersonate that identifier. The context timeout bounds asynchronous notification callbacks, not a native synchronous function that has already started. Timeout/cancellation does not dismiss OS UI already submitted. The macOS permission functions require macOS 10.15+.

Compatibility `accessibility_status` and `request_accessibility` remain available. The former never prompts; the latter is an explicit synchronous compatibility call. Publishing the application's own accessibility tree does not need the separate Accessibility **client** trust grant.

## Windows owned window menus

Enable `windows-menu` (included by `native`). Register a retained owned `Arc<W>` where `W: HasWindowHandle + Send + Sync + 'static + ?Sized`. Trait-object owners are supported without trait upcasting. Native HWND/process/UI-thread validation occurs on the dispatch thread; the business-facing result is `WindowMenuTarget`.

`DesktopRuntime::register_windows_window` is the synchronous host entry. `DesktopService::register_windows_window(owner)` queues the same operation and returns an awaitable pending result. `set_window_menu(target, Some(menu))` mounts a declared menu; `None` detaches it. Explicit registrations require explicit `unregister_windows_window` before native owner release. A second active registration of the same HWND in that runtime is rejected; reuse the existing target to update the menu.

Framework hosts should use `register_windows_window_scoped(owner)` and retain the returned `WindowMenuLease` in their logical window state. `.target()` supplies the token. The runtime retains only a Weak reference to the lease; dropping the last clone wakes UI cleanup directly without entering the bounded command queue. Canceling a registration's pending result therefore cannot permanently retain its native owner. Runtime draining sweeps expired leases before/after command batches, and before another registration. Runtime destruction clears remaining native registrations. Cleanup errors are bounded and available via `take_window_menu_cleanup_errors`.

Drop the scoped lease on logical close/CloseRequested or the framework's logical Closed notification. Waiting for native WM_DESTROY can be circular because the registration itself keeps the window owner alive. Detach happens before releasing the retained Arc.

Windows accelerators additionally need message-loop preprocessing. The standalone example demonstrates `WindowsMenuMessageHook::install(&mut EventLoopBuilder)` before building the loop, then `bind(runtime.windows_menu_dispatcher())` before running. The hook copies a trusted winit MSG into an owned `WindowsMessage` and processes menu accelerators before normal dispatch. A host that mounts a menu without installing this hook must not claim keyboard accelerators work. Additional toolkit/CEF input loops need their own forwarding; a window-menu mounting helper alone does not provide that integration.

The public crate remains `forbid(unsafe_code)`. The small unpublished `native-platform` path dependency contains the audited native completion/Win32 boundary; see [its safety notes](native-platform/SAFETY.md). Source distributions must include that directory's manifest and sources. Both manifests retain Rust 1.85 and avoid trait-upcasting/async-trait requirements.

See [Linux backends](docs/linux-backends.md) for the Linux feature/API choices, ownership, protocol fixtures, standalone winit example and remaining real desktop acceptance. Linux window/application menubars still need a GTK host and remain unsupported here.

## Single instance and protocol activation

Call `instance::acquire(instance_directory, wake)` before constructing the main application. `instance::namespace_directory(base, application_namespace)` derives a stable compact SHA-256 transport directory (with the Unix UID) so long application IDs do not exceed Unix socket path limits. Use a dedicated application directory under the current user's runtime/data directory. On Unix, new directories are created with mode 0700; existing directories and lock files must be owned by the current UID without group/other access. Symlinked final paths and lock files are rejected. On Windows, the directory and files must be owned by the current process user, receive a protected DACL granting access only to that user, and reject reparse points. Use a dedicated instance directory because the Windows backend restricts that directory's ACL.

- `InstanceRole::Primary(primary)`: retain the guard until shutdown. On its wake event, drain `primary.try_recv()` and apply host activation rules (for example, show a window or route an allowed URL).
- `InstanceRole::Secondary(secondary)`: call `secondary.activate(&activation)` and exit after it succeeds. Success means the primary queued and acknowledged the launch. An unavailable primary, full activation queue, malformed frame, or timeout returns an error.

The lock is an OS advisory file lock. The file is intentionally not removed on exit, which avoids an inode race when simultaneous launches open it. Only the lock holder may remove a stale socket. Frames are limited to 64 KiB, the activation queue has 64 slots, reads have a total deadline, and drop terminates the listener even if the socket path disappeared. Unix launch IPC authenticates through the owner-only directory/socket. Windows binds only `127.0.0.1` on an OS-assigned port and publishes discovery data atomically in the ACL-private directory. A new random 256-bit token is generated for every primary launch. Fresh client/server nonces and HMAC-SHA256 authenticate the server before launch data is transmitted, then authenticate the bounded request and acknowledgment. Authentication failures do not retry a transmitted activation. The trust boundary is the current OS user; another process running under that same user can read discovery material and send launch data. Windows transport/security code is cross-compiled here, and the shared real TCP protocol is tested on macOS; validation on an actual Windows machine remains required.

`Activation` carries arguments, an absolute working directory, and URL strings. It is data, never a command to execute. Run every protocol URL through an explicit router before interpreting its action:

```rust
use lintel_desktop::protocol::ProtocolRouter;
let router = ProtocolRouter::new(["lintel-demo"])?;
let url = router.route("lintel-demo://open/document?id=42")?;
assert_eq!(url.url().scheme(), "lintel-demo");
# Ok::<(), lintel_desktop::DesktopError>(())
```

This package does not silently register system URL handlers. A macOS bundle declares `CFBundleURLTypes`/`CFBundleURLSchemes` and forwards the URL received by its application delegate; Windows registration and Linux `.desktop` association belong to the installer. Cold launches can extract URLs from argv and forward them over single-instance activation. For a running macOS application, argv alone does not handle OS URL events. Lintel's high-level runtime forwards the selected public/legacy winit native `received_url` callback through its scheme policy; another host must forward its native callback explicitly.

## Accessibility and keyboard focus

The optional `accessibility` feature provides `accessibility::AccessibilityWindow`, a real AccessKit/winit adapter. It uses native NSAccessibility on macOS, UI Automation on Windows, and AT-SPI on Linux. It is `!Send`/`!Sync` and remains with its native window on the UI thread. The module reexports its exact `winit` and `accesskit` types to make version boundaries explicit.

Create the native window with `with_visible(false)`, create the adapter using the active event loop/window and a proxy for events implementing `From<AccessibilityEvent>`, then show the window. Forward every native `WindowEvent` through `adapter.process_event` before ordinary handlers. On `AccessibilityWindowEvent::InitialTreeRequested`, publish a complete `accesskit::TreeUpdate` using `update_if_active`; publish later semantic/focus changes the same way. Honor `ActionRequested` against the actual widget state, then publish the resulting state. The adapter supplies OS transport and action delivery, not invented semantics for controls the host did not describe.

`FocusOrder` stores the host's visible, enabled, focusable node IDs. It supports next/previous traversal with optional wrapping, direct focus requests, empty orders, and control removal while preserving focus when possible. Map Tab/Shift+Tab to `FocusDirection::Next`/`Previous`, apply the returned ID to the widgets, and use `focused_or_root` for every accessibility update's focus field. The helper never moves OS focus independently of the widget tree.

`permissions::accessibility_status()` checks macOS Accessibility client trust without prompting. `request_accessibility()` is a separate explicit setup action that can display the system guidance; its return value is the current state, not an eventual grant. These calls concern accessing/automating other applications. Publishing the application's own accessibility tree does not require that client grant. Windows reports `NotRequired` for the comparable per-app grant, while protected/elevated targets can still require elevation or UIAccess. Linux returns a typed unsupported error for this generic permission probe rather than assuming its session policy.

## Runnable example and verification

```bash
cargo run --manifest-path crates/lintel-desktop/Cargo.toml \
  --example desktop --features native,example-winit

cargo test --manifest-path crates/lintel-desktop/Cargo.toml --no-default-features
cargo test --manifest-path crates/lintel-desktop/Cargo.toml --features native
cargo clippy --manifest-path crates/lintel-desktop/Cargo.toml --features native --all-targets -- -D warnings
```

Add `-- --smoke` to create a hidden window with a native accessibility adapter, menu, tray, and shortcut, update a menu item, read and discard clipboard text, inspect the non-prompting Accessibility client status, then exit and release the native resources. The smoke reports setup failures as a nonzero exit code.

The example starts a real native event loop, installs an application menu on macOS, an owned window menu with the accelerator hook on Windows, and a tray menu, and lets you explicitly trigger clipboard read, file selection, and an external URL from menu items. The action entry logs only event types/IDs; file results log `None` or `Selected` without paths. Cmd/Ctrl+O demonstrates the file-menu accelerator, and Cmd/Ctrl+Shift+L is the separately registered global shortcut. It does not overwrite the user's clipboard, write credentials, request notification access, or launch a browser on startup.

Automated tests cover command dispatch and shutdown, queue bounds, dispatch-thread deadlock prevention, compile-time native thread confinement, protocol rejection, secret redaction, invalid icon/menu inputs, permission checks, actual local activation delivery, and lock/socket cleanup. These tests do not modify the user's clipboard or Keychain, show native dialogs, or send notifications. The native feature build checks actual backend integration without making those OS-visible changes.

A separate opt-in real credential smoke uses a fresh random service namespace and verifies cleanup:

```bash
cargo run --manifest-path crates/lintel-desktop/Cargo.toml \
  --example credential_smoke --features credentials
```

On macOS it disables Keychain interaction in its own process with a scoped guard, so a locked or unavailable Keychain returns an error without presenting an authorization prompt. It refuses an unexpectedly existing test namespace, sets/reads the disposable credential, deletes it, and verifies the entry is absent. An RAII cleanup guard removes that exact unique test entry if an intermediate assertion fails. It does not modify any application credential namespace.

Passive permission snapshot (no requests):

```bash
cargo run --manifest-path crates/lintel-desktop/Cargo.toml \
  --example permissions_smoke --features permissions
```

A plain macOS CLI reports the notification bundle precondition error. Run that executable from an identified signed `.app` with `--require-notifications` to require a successful passive notification-status query. The JSON includes `permission_requests_executed: 0`; no request future is polled. A returned authorization snapshot is the calling process's current execution context, not a claim that all deployed copies have the same grants.
