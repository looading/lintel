# Local native crash reports

This optional standalone crate installs Embark's `crash-handler` before the UI starts and asks a separate `minidumper` helper process to write a local native minidump. It contains no upload transport and does not depend on Lintel's UI, CEF, or internal SDKs.

```toml
[dependencies]
lintel-crash = "0.1.0"
```

Install the standalone helper for the application's target architecture:

```sh
cargo install lintel-crash --version 0.1.0 --locked --bin lintel-crash-helper
```

Install `lintel-snapshot-helper` as a separate executable to capture running macOS
arm64 processes. The deliberate abort probe and its isolation tests require the
explicit `test-capture` feature.

```rust,no_run
use lintel_crash::{CrashConfig, CrashReporter};

let reporter = CrashReporter::start(CrashConfig::new(
  app_data_directory.join("crashes"),
  bundled_helper_executable,
))?;
// Keep reporter alive until after the UI, plugins, and CEF have stopped.
reporter.shutdown()?;
# Ok::<(), anyhow::Error>(())
```

Build and bundle `lintel-crash-helper` next to the host, using the same architecture and dependency versions as the host. A host can instead call `helper_entry()` at the very beginning of `main`, immediately return when it yields `true`, and pass its own executable as `CrashConfig::helper`.

Lintel's optional `crash-reporting` feature exposes this crate as `lintel::crash` and adds `.crash_reporting(CrashConfig)` to `application`, `multi_window`, `webview_app`, and the low-level `AppBuilder`. Native builders install the guard after a secondary instance has forwarded its launch, before the GUI runtime starts. The WebView builder handles CEF helper processes and secondary launches first, then keeps the reporter alive across CEF initialization and shutdown. These guards remain on the host stack; they do not place the native handler inside a shared plugin or core context.

The `window_manager` example accepts `--crash-helper <absolute executable> --crash-directory <absolute app-owned directory>` when built with `crash-reporting`. Both flags are required together. Its normal `--smoke` scenario exercises reporter cleanup while opening, closing, and reopening native windows; the separate probe remains responsible for the real fatal-crash test.

Each reporter creates a private run directory beneath the explicit absolute app-owned report directory. It contains `report.json`, `helper.log`, and, after a crash, `report.dmp` plus `completed`. New reports also include `capture.json` when the capture time is available. Its `captured_at_unix_ms` field records when the helper began handling the request. A failed writer leaves `failure.txt` when possible. Runs without a dump or writer failure are removed during normal guard cleanup. Applications decide when retained local reports should be exported or deleted.

Supervised application services can call
`CrashReporter::start_service(config, ServiceCrashContext::new(identity, attempt)?)`.
The context validates a nonempty identity of at most 256 bytes without control
characters and a positive supervisor attempt. It adds only `process_role =
"service"` and `service: { identity, attempt }` to the existing metadata. This
is application-declared correlation data, not an authenticated identity. No
endpoint, token, request, arguments, environment snapshot or invented service
generation is recorded. The original `CrashConfig` fields and `start` API are
unchanged. Lintel's process adapter installs this reporter before loading Mojo.

Diagnostics export explicitly includes the native `report.json` alongside its
dump, retaining service identity/attempt. Binary dumps remain unmodified; the
log redaction layer does not sanitize a dump. Export and sharing stay explicit.

The IPC endpoint is fresh for every app instance, lives in a private temporary directory, and uses a 256-bit authentication token delivered through the helper's inherited stdin pipe. The helper accepts one connected client. The pipe also supervises the helper: normal guard drop or abrupt parent death closes it. Native dump writing has an independent helper watchdog, while startup and normal shutdown have explicit bounded waits. The crash callback performs only the upstream preconnected crash-context IPC operation; report file IO happens in the helper. The single unsafe callback boundary and its lifetime and signal-safety reasoning are documented at the installation call.

The implementation follows the [upstream crash-handler callback contract](https://github.com/EmbarkStudios/crash-handling/blob/main/crash-handler/src/lib.rs) and [minidumper IPC client/server example](https://github.com/EmbarkStudios/crash-handling/blob/main/minidumper/examples/diskwrite.rs). Consult the [upstream supported target matrix](https://github.com/EmbarkStudios/crash-handling#supported-targets) before enabling the feature on a new architecture. On Linux the reporter restricts ptrace permission to its helper process. macOS distribution signing and sandbox entitlements must allow the bundled helper to receive the crash context; validate the final signed bundle on its target OS.

`capture_snapshot(helper, output, timeout)` captures the calling process on macOS arm64. Pass the absolute path of the bundled `lintel-snapshot-helper` and a fresh absolute file path under an application-owned staging directory with mode `0700`. The standalone helper receives the calling process's task capability only for that request. A successful call leaves a validated thread and module minidump for the caller to publish. `capture_snapshot_until` also accepts a cooperative cancellation check. Both calls remove partial output on timeout or failure, and the other platforms return `SnapshotError::Unsupported`.

Run `cargo test --manifest-path crates/lintel-crash/Cargo.toml --locked --features test-capture` to launch the isolated `lintel-crash-probe` child. The explicit `--abort` child fixture produces a real native crash. Tests check the completed dump's bounded stream directory, exception context, capture time, private file permissions, helper reaping, and startup-failure cleanup. The macOS arm64 tests also cover repeated manual snapshots, timeout and cancellation, and fatal capture after manual snapshots.

First-party code is licensed under MIT. See [LICENSE](LICENSE); upstream crash
handling and minidump dependencies retain their own licenses.
