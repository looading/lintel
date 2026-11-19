# Service crash report integration plan

**Goal:** Give supervised Mojo workers explicit local native crash capture with service identity/attempt metadata, reusing the existing reporter, helper, publication and diagnostic export paths.

**Architecture:** An optional process `crash-reporting` feature provides `ProcessWorker::connect_with_crash_reporting(library, identity, config)`. It returns `CrashReportingWorker`, with the ordinary worker declared before its reporter so native service owners drop first. Existing `ProcessWorker`, native app reporters and CEF Crashpad retain their behavior. A separate packaged `lintel-worker-crash-helper` role permits worker capture alongside browser Crashpad.

**Constraints:** Independent Lintel checkout only. No AI Dev Loop, commits, remotes, upload transport, arbitrary metadata, new executor, unsafe Send/Sync or changes to upstream Mojo. Keep `CrashConfig`, `ServiceProcess`, `BuildOptions` and `BundleExtensions` public fields compatible. Native acceptance is macOS arm64; target checks are separate evidence.

## Reporter and worker

- [x] Add validated, private-field `ServiceCrashContext::new(identity, attempt)` and `CrashReporter::start_service(config, context)`. Preserve ordinary metadata; service reports add only `process_role` and `{identity, attempt}`. Startup has no directory generation yet, so omit it.
- [x] Add optional dependency through Cargo, wire `lintel-process/crash-reporting`, and forward the root crash feature through `lintel-process?/crash-reporting` without enabling multiprocess by default.
- [x] Validate the upstream attempt using the SDK constant, reserve parent stdin via `watch_supervisor`, start the existing reporter, then load/connect Mojo through the old entry. Reject invalid attempt without reflecting environment values.
- [x] Preserve the old worker's Send/Sync and owned typed-call contracts. New wrapper exposes only Deref, crash report directory and helper PID; startup errors release the local reporter.
- [x] Test fixed metadata keys, invalid identity/attempt and source compatibility. Run existing native crash isolation tests and default/opt-in process tests sequentially.

## CLI and starter

- [x] Add `--worker-crash-helper PATH`, copied as `lintel-worker-crash-helper`, through existing helper checks/signing/atomic publication. Keep old browser reporter/CEF Crashpad conflicts; reserve the worker helper name against service collisions.
- [x] Both multiprocess starters expose optional `service-crash-reports = ["lintel/crash-reporting"]`. A supplied absolute `--service-crash-directory` selects worker capture; compile feature alone does not install a reporter. Parent only parses/forwards owned path data.
- [x] Share a small data-only argument parser across parent and worker. Reject missing values, duplicates, relative paths and a requested service report directory without the feature. No directory keeps the existing worker path.
- [x] WebView starter can explicitly select existing CEF Crashpad with `--cef-crash-directory`; helper role dispatch remains free of reporter side effects.
- [x] Generator and embedded CLI archive include the parser and feature. Cover parser, reserved role, role combination and ordinary starter compatibility.

## Export and native acceptance

- [x] Add existing native `report.json` to the diagnostic export filename allowlist so identity/attempt survive export; keep all existing record/byte/file-capability limits and exclude other run files.
- [x] A private real Mojo fixture aborts only on explicit test requests. Verify MDMP header/streams, completed marker, worker PID, identity/attempt, distinct restart reports and explicit reconnect to a newer generation.
- [x] Separately verify normal cleanup, helper-start failure before native load, failure after reporter startup, and broker death. SDK stdin EOF calls process exit without Rust Drop; test the crash helper's independent EOF exit rather than claiming RAII or a dump on that path.
- [x] Verify actual native and CEF starter build/bundle/run with the dedicated helper role. Keep test faults outside shipping templates and test-only mock keychain out of defaults.
- [x] Run scoped independent review, affected package/target checks, final CLI generation/source matching, document/catalog updates and owned-process cleanup audit. Save evidence under `build/mojo-crash/`.

Reports remain local. This does not establish OS sandboxing, automatic upload/symbolication, SIGKILL/OOM capture, strong identity attestation or Windows/Linux native acceptance.
