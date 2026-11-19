# Linux desktop backends

`linux-tray`, `linux-x11-shortcuts`, and `linux-portal-shortcuts` are implemented opt-in backends. `linux` enables all three. They do not initialize GTK, start an unrelated GTK event loop, or interpret `DISPLAY` as permission to use X11 from a Wayland application. `native` keeps the existing macOS/Windows selection; Linux applications add the desired Linux features explicitly.

The code remains safe Rust under the public crate's `forbid(unsafe_code)`. Dependencies are `ksni 0.3.7` (async-io), `zbus =5.12.0` (async-io), `x11rb 0.13`, `async-lock`, `async-channel`, and `async-global-executor`. No application Tokio runtime is required. The package remains Rust 1.85; the direct zbus version is pinned to a compatible API/MSRV.

## Runtime and service integration

Create and retain `DesktopRuntime` as before. `DesktopService::linux()` exposes the explicit Linux services on Unix when a Linux feature is selected. Menu/tray commands on a Linux target use this service automatically; ordinary `register_shortcut` remains unsupported on Linux because the host must explicitly choose X11 or a user-mediated portal session.

Linux menu/tray operations and X11 connection admission use their own bounded async FIFO (128 in-flight/queued operations). D-Bus I/O never blocks `DesktopRuntime::drain` or a native UI thread. Each command completes its existing `Pending<T>` only after the operation's actual result; D-Bus method waits are bounded to ten seconds. Await replies rather than blocking the UI. Dropping a normal menu/tray pending reply does not cancel its side effect. On runtime drop, new work is rejected, queued commands are disconnected/failed, trays are shut down, and registered X11/portal controllers receive a close signal. Cleanup does not join a thread indefinitely.

Background callbacks send typed `DesktopEvent` values into a separate queue of 128 entries, then call the host wake callback. After draining commands, the host calls `runtime.take_events(64)` and routes events through its existing handler. `take_event_overflow_count()` returns and clears the number of dropped events; a host should log nonzero values. A full drain batch schedules another wake. No D-Bus callback executes application state code directly.

## StatusNotifierItem tray and DBusMenu

`linux-tray` translates `MenuSpec` normal items, controlled check items, nested submenus, separators, enabled state and labels to `com.canonical.dbusmenu`. IDs map back to `DesktopEvent::Menu`. Literal underscores are escaped for DBusMenu mnemonic rules. Menu shortcut metadata is a display hint; it does not install a global grab. Use an explicit shortcut backend for activation.

Tray icons are validated RGBA, converted to the SNI-required network-order ARGB bytes. macOS template-icon semantics do not apply on Linux. `set_tray_visible(false)` publishes SNI `Passive`; visibility remains a desktop-shell policy. Menu removal is rejected while that menu is attached to a tray; detach/remove its trays first.

`create_tray` waits for registration with `org.kde.StatusNotifierWatcher` and a registered host. Missing watcher, missing host, denied name ownership, and D-Bus errors remain errors. The backend does not enable ksni's `assume_sni_available` fallback. After startup, `LinuxEvent::TrayAvailability` reports watcher loss/recovery, and ksni re-registers with a returning watcher. A live watcher is a protocol availability signal; it cannot prove a shell renders the icon. `TrayActivated` preserves SNI primary/secondary activation and screen-coordinate hints without inventing mouse-release/double-click events. Shell context menus are provided through DBusMenu.

A compatible user session bus and an SNI-capable shell are required. GNOME may require a shell extension. This backend currently owns its SNI well-known name; sandbox policies that forbid that name need a separate explicit adapter and have not been accepted here.

## Explicit X11 global shortcuts

Call `service.linux().x11_shortcuts(display).await` after deciding that the real host is X11. The independent winit example uses `ActiveEventLoopExtX11::is_x11()`. `None` selects the connection's default display only after this explicit X11 choice. The API never silently falls back to XWayland for a Wayland host.

Retain the returned `X11Shortcuts`; `register("Ctrl+Shift+L")` and `unregister(id)` return awaitable `Pending` results. Supported keys include letters, digits, single characters present in the current unshifted keymap, F1–F35, Space/Tab/Enter/Escape, arrows, Home/End/Insert/Delete/Backspace/PageUp/PageDown. Modifiers are Ctrl/Control, Shift, Alt and Super/Meta/Cmd/Command. Ambiguous/missing server modifier assignments and absent keysyms are errors. This is a documented key set, not every accelerator spelling accepted by the macOS/Windows backend.

The worker connects before reporting readiness. Every passive `GrabKey` is checked for X11 errors, including conflicts; partial lock-mask combinations are rolled back on failure. CapsLock, NumLock and ScrollLock combinations are derived from the server's modifier map. Press/release events use existing `DesktopEvent::Shortcut`; legacy X11 key repeat can produce repeated protocol events. The worker polls its connection fairly and services its bounded command queue every ten milliseconds. Last controller drop or runtime shutdown closes its dedicated connection and releases all grabs. Connection loss or a keymap change emits `BackendStopped`, closes that controller, and requires explicit reconnect/re-registration.

The upstream global-hotkey 0.7/0.10 X11 worker can return `Ok` after its connection worker has died. This backend uses x11rb directly so connection failures cannot be reported as successful registrations.

## User-mediated portal global shortcuts

`portal_session(PortalOptions::new("com.example.App")).await` creates a dedicated session connection. Constructing the future alone has no D-Bus side effect. Unsandboxed apps register that connection through `org.freedesktop.host.portal.Registry.Register` before using portals; `app_id` must correspond to an installed `<app_id>.desktop` visible to the portal. Older providers without Registry remain supported (`RegistryStatus::Unavailable`), while other registration failures are propagated. Set `sandboxed = true` only when the sandbox supplies the actual app identity; no host Registry call is then made.

Only an explicit `session.bind(shortcuts).await` requests binding and any consent UI. Each session permits one bind attempt. `parent_window` accepts an exported portal window identifier, or an empty string for unparented UI; the backend does not manufacture a Wayland export handle. Requests contain action IDs, descriptions and optional preferred triggers. The portal may return a subset, including none, and user-remapped `trigger_description` values. The returned values and `PortalBindingsChanged` are authoritative. There is no invented restore token or blanket global permission status.

The implementation subscribes to `Response` before invoking the method, preserving an early response. Its independent request task retains the actual method reply when the caller disappears, then sends `Request.Close` after cancellation/timeout. Canceling a binding future also closes its session. `PortalSession::close(self)` consumes the session and awaits its single cleanup result; ordinary drop schedules the same cleanup. Cleanup sends `Session.Close` and then closes the dedicated peer connection, including when the provider refuses or times out. No second cleanup request races an explicit close. Explicit response codes use `DesktopError::PortalResponse` (1 user canceled, 2 other failure). A configurable interaction deadline defaults to five minutes. Method and cleanup waits are separately bounded. Activated/Deactivated events are filtered to the retained session and known bindings, preserve timestamps and activation tokens, and are forwarded as `LinuxEvent::PortalActivated`. Provider/session closure emits `BackendStopped`; create a new session explicitly to reconnect and re-register its app identity.

No portal session/binding is created by runtime startup or by a status check. There is no universal Linux screen/input/accessibility permission: those remain accurately unsupported in the global permission facade until a particular portal/AT-SPI session is requested.

## Linux window menus and credentials

A client-rendered window-local alternative is now available as root `lintel::menu::menu_bar` with `public-iced,desktop`; it reuses MenuSpec and ordinary application Message/Task dispatch without GTK/CEF dependencies. See [client menubar](client-menubar.md) for keyboard/modal lifecycle and verification boundaries. It does not turn the native window-menu capability into a GNOME/KDE/global menu.

Linux window/application menubars remain typed unsupported for the winit host. `muda`'s Linux menu attachment needs a real GTK window/container and an active GLib host; it cannot safely attach a GTK menubar to an arbitrary iced/winit X11 or Wayland window. DBusMenu tray support does not change that boundary.

`credentials` uses keyring 3.6's explicit persistent `SsCredential` with Rust crypto and vendored D-Bus. A user bus and Secret Service provider are required. Missing services and locked/unavailable stores return errors, with no plaintext or mock fallback. Provider unlock UI can occur only in an explicitly requested credential operation. Actual Linux Secret Service roundtrip remains external acceptance.

## Verification and external acceptance

The private-session fixture starts `dbus-daemon` with a project-local socket/config, re-executes its child with only that private bus address, and shuts it down afterward. It never changes the user's session/system bus or opens consent UI. It verifies actual D-Bus calls, signal dispatch and cleanup, not a GNOME/KDE shell:

- SNI watcher absence, host absence, successful registration, ARGB bytes, activation, DBusMenu GetLayout/Event, label/status updates, watcher loss/re-registration and shutdown.
- Portal host registration, response-before-method-return ordering, bound subset/user remapping, signal ID filtering, user rejection, cancellation, timeout, Request.Close/Session.Close and provider loss.
- Pure tests cover menu translation, X11 parsing/modifier resolution/lock masks, failed X11 connection, async FIFO admission/order, bounded event overflow and runtime shutdown.

```sh
cargo test --manifest-path crates/lintel-desktop/Cargo.toml --features linux --lib --test linux_dbus -- --nocapture
cargo check --manifest-path crates/lintel-desktop/Cargo.toml --target x86_64-unknown-linux-gnu --features linux,example-winit --lib --example linux_desktop
# On an actual Linux host, explicit capabilities:
cargo run --manifest-path crates/lintel-desktop/Cargo.toml --features linux,example-winit --example linux_desktop -- --tray --x11-shortcut
# This command intentionally requests portal binding UI; install the matching .desktop first:
cargo run --manifest-path crates/lintel-desktop/Cargo.toml --features linux,example-winit --example linux_desktop -- --tray --portal-app-id com.example.App
```

Actual X11 key activation/conflict/cleanup, Wayland compositor consent/remapping, GNOME/KDE tray rendering/menu interaction and a Linux Secret Service roundtrip still require Linux desktop runners. Protocol fixtures and cross-compilation must not be reported as those desktop acceptance results. Evidence is saved under `build/checklist-platform/`.

Primary references: [GlobalShortcuts portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.GlobalShortcuts.html), [host Registry](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.host.portal.Registry.html), [portal API identity guidance](https://flatpak.github.io/xdg-desktop-portal/docs/api-reference.html), [ksni 0.3.7](https://docs.rs/crate/ksni/0.3.7), [ksni registration implementation](https://docs.rs/ksni/0.3.7/src/ksni/service.rs.html), [Xlib passive-grab semantics](https://www.x.org/releases/current/doc/libX11/libX11/libX11.html#XGrabKey).
