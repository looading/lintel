# lintel-desktop-platform

Target-specific native permission and window-menu adapters used by
[`lintel-desktop`](https://docs.rs/lintel-desktop). Applications normally depend
on that higher-level crate and choose its explicit `permissions` or
`windows-menu` feature.

```toml
[dependencies]
lintel-desktop-platform = "0.1.0"
```

The macOS module integrates notification authorization through the application's
native bundle context. The Windows module adapts native menu ownership and
message dispatch to an application's existing window/message loop. Other targets
do not acquire a native backend merely by depending on this crate.

This package does not create an iced or CEF runtime, request a permission at
startup, or provide an operating-system sandbox. Native owners and callbacks
must follow the thread, lifetime, and cleanup contracts in [SAFETY.md](SAFETY.md).
The crate denies unsafe code by default and isolates its necessary FFI boundary
inside its audited target modules.

Licensed under MIT. See [LICENSE](LICENSE).
