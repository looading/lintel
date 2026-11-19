# Native boundary audit

The public `lintel-desktop` crate remains `forbid(unsafe_code)`. This unpublished nested crate denies unsafe code by default and allows it only in its audited macOS/Windows modules. It has no observer, process deployment, or system-permission side effects at initialization.

## macOS notifications

The module calls only UserNotifications settings and explicit authorization completion APIs. It never sets a notification delegate or categories and never changes the application identifier. The caller must run from an identified `.app` bundle; the guard rejects plain CLI binaries before `currentNotificationCenter` can enforce that precondition.

Both functions construct lazy Futures. Bundle inspection, native center access, and request submission happen only after polling. Owned native objects and the local RcBlock reference leave scope before await, keeping the returned Future Send. Apple copies the completion block; its Mutex-protected oneshot sender remains owned by that copy. Canceling the Rust receiver does not invalidate callback memory. Callbacks copy only a status integer or an NSError description; borrowed Objective-C objects never cross an await or leave the callback. A repeated callback cannot send twice.

Reading settings does not request permission. The explicit request can cause system UI; cancellation/timeout does not dismiss UI already submitted to the OS.

## Windows window menus

Registration accepts an owned Arc implementing HasWindowHandle + Send + Sync. It validates a live Win32 handle in the current process and owning UI thread. The owner is retained until unregister; detach and menu-subclass removal happen before releasing that Arc. Business-facing code receives an opaque generation token instead of a raw HWND. One host rejects a second active registration of the same HWND, including explicit/scoped registration mixtures.

Menu attachment refuses an already installed foreign menu. The upstream muda menu is retained while mounted. Cleanup preserves a foreign menu if another native host replaced ours. The host must release registrations on logical close, not wait for native destruction that retaining an Arc could defer.

The dispatcher snapshots owners/menu clones and releases all RefCell borrows before calling Win32, which can synchronously send WM_COMMAND. A snapshot is not an owned Binding and cannot detach on drop. Keyboard messages use initialized scalar MSG data. The only borrowed MSG pointer is provided by winit's documented hook and is read only during that callback; no pointer escapes. Both bindings use the Windows SDK MSG ABI.

The owned-window contract excludes foreign code destroying/replacing a HWND independently of its retained owner. If validation detects that violation during cleanup, the bridge reports an error and conservatively retains the upstream menu allocation rather than free a live subclass refcon or execute its destructor against a reused HWND. It still releases the owner; this exceptional protection is not a normal lifecycle path.

Scoped leases keep no native object. Runtime tracking is Weak; the last lease drop only wakes the UI loop, never enters the bounded command queue. A canceled registration reply drops its lease, and runtime draining detaches expired registrations. Runtime destruction always clears native registrations. Cleanup diagnostics are bounded and available to the host.
