# Multiprocess connection and build preflight implementation plan

> Execute this continuation in the existing independent Lintel checkout, with separate owners for process and build code and a scoped integration review. The user has already requested continued implementation and multi-agent collaboration. No AI Dev Loop, commits, remote creation or publication.

**Goal:** Remove repeated typed client binding from business applications and reject invalid service declarations before any application build script executes.

**Architecture:** Keep the existing Mojo 0.3.0 directory, typed calls, supervision thread and explicit readiness waits. Add one connection adapter over the existing directory request, use nonblocking lock admission, and compose application/service builds through a preflight using Cargo metadata. Preserve the original single-target build API and prebuilt bundle path.

**Tech stack:** Existing Rust 2024, pinned toolchain, mojo-ipc/mojo-service 0.3.0, Tokio, cargo_metadata and clap. No new dependencies or native runtime version changes.

## Constraints and design decisions

- `wait_ready(identity)` remains explicit. It is a process readiness gate; directory routing by key/version does not promise affinity to that identity.
- No automatic reconnect, replay, implicit grants, new executor, native library discovery or new unsafe implementation.
- Preserve directory submission-time deadlines and the rule that a timely response can be collected by a later first poll. Dropping an awaiter does not retract submitted control work or release its retained response slot prematurely.
- Compare two approaches: a combined readiness/reconnect client would add provider-affinity and replay policy; a typed directory connect/bind adapter removes repeated code without those new policies. Use the latter.
- Keep `build(&BuildOptions)` and `build_service_processes(...)` available. CLI application build paths opt into the new composition, while `bundle --executable` continues without Cargo execution.
- Do not approximate Cargo's feature resolver. Static preflight validates declaration shape, service target existence and application self-reference; Cargo remains authoritative for feature activation and compilation.
- Generated multiprocess applications cannot be named `lintel-worker` or `lintel-app-protocol`: these conflict with the fixed worker target and protocol package respectively. Other templates retain their existing name rules.
- Keep one Cargo operation at a time. Reuse already passed checks for unchanged paths; native macOS evidence and Windows/Linux typechecks must be reported separately.

## Process adapter

Files: `crates/lintel-process/src/directory.rs`, `calls.rs`, `lib.rs`, `tests.rs`, and the two multiprocess `src/main.rs` templates.

Public interface:

```rust,ignore
pub struct ConnectedService<T: DynMojomInterface + ?Sized + 'static> {
    pub client: ServiceClient<T>,
    pub generation: u64,
}

impl DirectoryClient {
    pub fn connect_client<T: DynMojomInterface + ?Sized + 'static>(
        &self,
        context: &CallContext,
        key: &ServiceKey,
        version: u32,
        capacity: usize,
        timeout: Duration,
    ) -> DirectoryRequest<ConnectedService<T>>;
}
```

- [x] Replace only the directory request admission lock with `try_lock`; contention returns `WouldBlock`, poison fails, and rejected admission releases its permit.
- [x] Validate zero capacity and closed call context before submitting native work. Preserve concrete `CallError` as the I/O error source when binding fails.
- [x] Submit through the existing `connect` immediately, retain runtime/context ownership across its owned `Send + 'static` future, and return the exact routed generation with the bound client.
- [x] Add focused lock/permit/failure tests and real generated Echo `Send + 'static` compile contracts. Adapt native and WebView templates to the new entry point.

## Build composition and generator

Files: `crates/lintel-build/src/cargo.rs`, `service_processes.rs`, `project.rs`, `lib.rs`; `crates/lintel-cli/src/main.rs`.

Public interface:

```rust,ignore
pub fn build_with_service_processes(
    options: &BuildOptions,
) -> anyhow::Result<(BuildArtifact, Vec<BundleHelper>)>;
```

- [x] Resolve Cargo metadata and selected application once, validate every declared service target and declaration before compiling any target, then build with the original package/target/profile/features/configuration.
- [x] Switch CLI build, run and Cargo-backed bundle to the composition. Preserve legacy single-target builds and the no-Cargo prebuilt path.
- [x] Reject the two confirmed multiprocess name collisions before publishing a new destination or writing into an existing empty destination.
- [x] Add regressions for malformed metadata, missing later service targets, application self-reference, positive multi-service compilation and destination preservation. Build-script sentinel files must remain absent on preflight failures.

## Integration and acceptance

- [x] Root runs `cargo test --manifest-path crates/lintel-process/Cargo.toml --offline --locked --lib` and `cargo test --manifest-path crates/lintel-build/Cargo.toml --offline --locked --features portable --lib`, sequentially. Run the CLI's focused affected tests and normal binary build after source/docs stabilize.
- [x] Generate native and WebView multiprocess projects with the final CLI, compare the changed framework/template files and compile both consumer paths using the prepared dependencies.
- [x] Run real macOS typed RPC through the new API, verify nonzero distinct service PID, exact returned generation, default-deny failure and normal/startup-failure cleanup. Keep fault logic outside shipping templates.
- [x] Independently review the changed functions and necessary direct callers; fix evidence-backed issues, then run only newly affected checks.
- [x] Update `docs/multiprocess.md`, `docs/webview-services.md`, process README and machine catalog. Save exact commands, logs, fixture outcomes and source consistency evidence under `build/mojo-ergonomics/`.

Existing CEF long-task, navigation and resource-publication tests remain evidence for unchanged code. This batch does not establish new Windows/Linux native acceptance, production signing trust or an OS sandbox for application service processes.
