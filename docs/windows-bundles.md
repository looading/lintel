# Windows bundles and managed per-user installation

Lintel can assemble native Windows MSVC portable directories from an existing PE
executable on macOS or Windows. Cargo build/link integration supports these
targets when the required Rust target, MSVC linker, SDK, and dependencies are
available:

| Target | PE machine | Output architecture |
| --- | --- | --- |
| `i686-pc-windows-msvc` | I386 | `x86` |
| `x86_64-pc-windows-msvc` | AMD64 | `x86_64` |
| `aarch64-pc-windows-msvc` | ARM64 | `aarch64` |

The Rust packaging API is available on the host and does not invoke a Windows
binary while inspecting it:

```rust,ignore
let directory = lintel_build::bundle_windows_with_libraries(
  &bundle_options,
  "x86_64-pc-windows-msvc",
  &lintel_build::windows_libraries(project)?,
)?;
```

`BundleOptions` is shared with the macOS API. The Windows path rejects CEF
distributions, CEF helper/Dawn options, and the macOS updater helper with a
downcastable `WindowsBundleUnsupported` error. GNU targets are also explicitly
unsupported. Apple signing and notarization options are rejected by the Windows
CLI path. Existing macOS commands continue to use the macOS bundle implementation.
On Windows, `--protocol-scheme` records declarations in `lintel-bundle.json`.
Bundling does not change the Registry. Actual associations require the explicit
per-user install or `protocol register` command below. The old `bundle --updater`
option remains a macOS bundle operation; a Windows managed installation uses
separate stable launcher and installer tools.

## Register libraries and build

Windows libraries use a separate project registry,
`lintel.windows-libraries.json`, and immutable blobs under
`.lintel/windows-libraries/`. This keeps a project's macOS and Windows inputs
separate. On Windows the `third-party` command selects this registry by default;
on macOS use `--platform windows`:

```sh
lintel third-party --platform windows add native --path vendor/native.dll --version 1.2.0
lintel third-party --platform windows list
lintel build --target x86_64-pc-windows-msvc --release
lintel bundle --target x86_64-pc-windows-msvc --release --id dev.example.app
```

Register a `.dll` and its adjacent `<stem>.lib` to enable MSVC linking. The
registrar snapshots both files and verifies their machine types. A standalone
`.lib` is treated as a static library. A DLL without an adjacent import library
can be registered for packaging an already-built executable; Cargo linking then
returns an explicit missing-import-library error. Registry updates retain old
blobs so a build already using a snapshot is unaffected. Every registry read
verifies file checksums.

To bundle an executable without running Cargo, run this from its project so the
registered DLLs are included:

```sh
lintel bundle --executable target/x86_64-pc-windows-msvc/release/app.exe \
  --target x86_64-pc-windows-msvc --id dev.example.app --name Example \
  --bundle-version 1.2.0
```

The result is `build/windows/Example-1.2.0-x86_64/`, containing `Example.exe`,
registered runtime DLLs, and `lintel-bundle.json`. Import/static libraries remain
build inputs and are not distributed. On Windows, `lintel run` launches the
packaged executable and returns a failed child exit status. A Windows target
cannot be run by the macOS `run` path.

Additional process helpers can be supplied with `bundle_windows_with_helpers`
and `BundleHelper { name, executable }`; they are copied as `<name>.exe` and pass
the same PE-machine, executable-role, collision, and import-dependency checks.
`--crash-helper PATH` supplies a named helper, while `--crash-reporter` selects
the installed sibling executable. Copying a helper does not imply that its
runtime reporting transport supports Windows.

Windows development bundles follow the same immutable version rule. For changed
inputs, set a new `--bundle-version` or a different `--output-dir` before running.

## Validation and publication

The packager checks DOS/PE signatures, architecture and optional-header format,
the executable/DLL role, section bounds, and normal and RVA-based delayed imports.
Non-system imported DLLs must be registered and copied beside the executable.
OS DLLs and Windows API sets remain system requirements. Redistributables such
as `vcruntime140.dll` are not silently assumed to be installed: provide the
appropriate redistributable runtime or build against a static CRT. Inputs have
a 512 MiB per-image/archive inspection limit.

Names reject Windows device aliases, traversal, alternate streams, trailing
dots/spaces, and unsupported characters. Artifact basenames are compared without
ASCII case sensitivity; symlink/reparse-point inputs and output collisions are
rejected. Existing system DLL names cannot be overridden by registered inputs.

The full package is validated inside a temporary directory on the output volume.
Publication holds an output lock and renames the staged directory once. Versioned
output directories are immutable: an exact repeat verifies all hashes and returns
the existing path; changed inputs with the same name/version/architecture are
rejected. Choose a new version or output directory for changed contents. Failed
validation leaves an existing published bundle intact.

`validate_windows_bundle` rechecks all hashes, PE roles/machines, import closure,
identity, and declarations. `copy_windows_bundle_verified` additionally pins the
manifest snapshot captured from authenticated content, preventing a self-consistent
replacement of the files and their unsigned manifest during an update wait.

## Managed per-user installation

The Windows implementation uses a stable launcher plus immutable versions. Build
the CLI and all tools for the same MSVC target, with a static CRT so the stable
tools do not require unbundled redistributable DLLs. An MSVC linker/SDK is required
to produce executable binaries; a cross-target `cargo check` alone does not do so.

```powershell
$env:RUSTFLAGS = '-Ctarget-feature=+crt-static'
cargo build --manifest-path crates/lintel-cli/Cargo.toml --release --bins
lintel windows install 'build/windows/Example-1.2.0-x86_64' `
  --public-key updater.pub --scheme example
```

This is an explicit local initial installation of the selected package. It pins
the Ed25519 public key for future releases; it does not claim an Authenticode check
of the initial package. The private release key must never be shipped to users.
The public key must match the one embedded in the application's update client.
`--channel` defaults to `stable`; other channels must also match the client.

The default layout is:

```text
%LOCALAPPDATA%/LintelApplications/dev.example.app/
  lintel-launcher.exe
  tools/lintel-updater.exe
  lintel-install.json
  active.json
  managed-versions.json
  rollback.json                       # previous successful selection, after an update
  version-receipts/...                 # signed historical ownership envelopes
  versions/Example-1.2.0-x86_64/...
  protocol-ownership.json             # when schemes were registered
```

`--launcher` and `--installer` can specify tools explicitly; otherwise the CLI
uses installed sibling executables. The Rust API is `install_windows_app` with
`WindowsInstallOptions`. `--root` selects a new local-drive directory inside the
current user's LocalAppData. Existing roots are never replaced by initial install.
Ordinary/extended Windows paths and case variants are normalized through an
existing canonical parent, with a case-insensitive component boundary check.
Every existing directory below LocalAppData is checked for a private current-user
ACL and reparse points; a shared outer ancestor is rejected even if its immediate
child is private. Existing ACLs are not rewritten. New directories/files receive
protected current-user ACLs. UNC/network roots are refused; the installer never
requests elevation or rewrites an existing parent's ACL.

The launcher verifies the selected version before spawning it. A protocol launch
accepts only `--lintel-protocol URL` with exactly one Unicode URL from the installed
allowlist; extra arguments, literal quotes, control characters, and other schemes
are rejected. URLs are passed as a single `Command` argument, never through
`cmd.exe` or PowerShell. Normal launches preserve application arguments.

The updater verifies the signed release and archive again in its own process,
hashes the compressed bytes actually consumed by extraction, waits for the
retained parent process handle and bundle helpers to exit, then rechecks state
under the install lock. It selects the new version by atomically replacing
`active.json`; running executables and old version files are not overwritten.
An immediate process-spawn failure restores the old selection only while the
active record still equals this update's candidate. See [updater.md](updater.md)
for the client and release workflow.

## Managed maintenance and optional startup readiness

The additive `inspect_windows_installation`, `prune_windows_versions`, and
`uninstall_windows_app` APIs require an expected app ID and return a JSON-capable
`WindowsMaintenanceReport`. Prune/uninstall are native Windows operations run from
an external maintenance process after this installation's applications, launchers,
and brokers have exited. They acquire the same lifecycle lease and install lock
as the launcher/updater. Inspection is read-only and makes no native liveness claim.

```powershell
lintel windows inspect $root --app-id dev.example.app
lintel windows prune $root --app-id dev.example.app --keep-recent 2 --dry-run
lintel windows uninstall $root --app-id dev.example.app --dry-run
# Remove --dry-run to apply the reviewed operation.
```

The managed ledger records explicit initial selection and subsequent authenticated
updates. Signed historical receipts remain usable for ownership after expiry;
expiry is still enforced for every new install. Old installations without a ledger
adopt only their verified active record, never arbitrary pre-existing directories.
Pruning preserves active, rollback, both pending-journal versions, recent records,
unregistered/modified/unverified directories, and files in use. It uses Restart
Manager only to query users, then reserves all batch files with exclusive read and
DELETE handles, hashes the reserved bytes, and requests an image-section check
before deleting through those handles. It never calls RmShutdown, uses POSIX
image deletion, or schedules reboot deletion. Unsupported filesystems fail closed.

Uninstall removes only owned, unmodified version files and stable tools. A
protocol receipt must match this installation's identity, launcher, and scheme
policy, and each Registry tree must still exactly match its owner token. Foreign
edits are retained. Unknown tools, version directories, and business data remain.
An `uninstall.json` tombstone blocks future launcher execution. The root, metadata,
receipts, locks, and small audit files remain; the root is never recursively deleted.
Busy or partially removed items are reported and can be retried. In a dry-run,
`removed` lists candidates, not completed deletions. Inspection/dry-run do not
promise future liveness or atomic all-files cleanup.

`PreparedUpdate::with_windows_startup_health(WindowsStartupHealth::new(timeout,
minimum_uptime)?)` optionally changes the next update's relaunch contract.
Existing updates keep the old default without a handshake. Only enable it when
the candidate calls `confirm_windows_startup()` at its **business-ready callback**.
No marker means immediate `false` without consuming stdin; CEF `--type` helpers
also return immediately. Never wait for this challenge globally before CEF
subprocess dispatch or before `webview_app.run()`.

The dedicated candidate pipes bind a random attempt to the retained Child's PID,
creation time and manifest digest. An exit code of zero does not confirm readiness.
Readiness scanning is bounded to 64 KiB total/4 KiB per line and an explicit
100 ms..5 minute deadline, with an optional minimum uptime. On failure the broker
may terminate only the exact Child it spawned under this opt-in contract; surviving
candidate helpers defer recovery and new launches. Recovery never kills a stored
PID or overwrites another selected version. A confirmed child's stdout is drained
by the broker after it releases lifecycle locks. See [updater.md](updater.md) for
the SDK call, private-stdio restrictions and interruption semantics. These native
Windows behaviors have source/model/target-check evidence, not Windows execution
acceptance.

## Protocol registration and removal

Managed install uses declared schemes unless overridden by explicit `--scheme`
arguments. Standalone registration is available for an executable that implements
the reserved single-URL launcher mode:

```powershell
lintel protocol register --id dev.example.app `
  --executable 'C:\Users\me\AppData\Local\LintelApplications\dev.example.app\lintel-launcher.exe' `
  --scheme example --receipt protocol-ownership.json
lintel protocol unregister --receipt protocol-ownership.json
```

Keep the receipt. The CLI writes it before registration without overwriting an
existing file; retries reuse its exact owner token. The native API
`WindowsProtocolRegistration` plus `register_windows_protocols`/
`unregister_windows_protocols` is also available. A managed launcher's allowlist
comes from its install receipt, so standalone registration must stay within that
already-approved policy; it does not broaden that policy.

Registration uses a Windows Registry transaction and only
`HKCU\Software\Classes`. It refuses foreign/modified trees, machine associations,
explicit user-choice conflicts, Registry links, system/device schemes, and unsafe
commands. A complete batch is checked before modification; failures roll back
the transaction. Removal requires the exact tree and owner token, preserving
another application's edits. `UserChoice` is read for conflicts and never written.
If transactional Registry support is unavailable, the operation fails without a
nontransactional fallback or an elevation prompt.

## Experimental Windows CEF source starter

An explicit starter generates the Windows GUI bootstrap client as a cdylib:

```sh
lintel new my-windows-app --template windows-webview
cd my-windows-app
cargo check --lib --target x86_64-pc-windows-msvc --features api-only
```

Project generation currently runs on macOS, as with the existing starters. The
result contains `src/lib.rs`, `ui/index.html`, `WINDOWS-CEF.md`, pinned public
framework/configuration, and `[lib] name = "App", crate-type = ["cdylib"]`.
There is no ordinary EXE `main`, automatic SDK fetch, bootstrap copy, native
sandbox claim, or enabled Windows CEF packager/run path. Native and macOS Webview
starter defaults remain unchanged. The additive Rust API is
`create_project(path, ProjectTemplate::WindowsWebview, checkout)` or
`create_portable_project` with the existing `EmbeddedStarter` fields.

`api-only` explicitly enables the pinned `cef/dox` dependency feature for
`cargo check`; it is absent from defaults and must not be used for native builds
or distribution. A non-Windows host build of this Windows-only library is
rejected. On an actual Windows toolchain, supply the verified matching SDK as
`CEF_PATH` and run `cargo build --lib --release --target x86_64-pc-windows-msvc`
without `api-only`. This is a prerequisite command, not a recorded successful
native link. The generated guide lists MSVC/Windows SDK/CMake/Ninja conditions,
SDK version/provenance, the five-argument entry and same-basename official
`App.exe`/`App.dll` layout, signing review, and the remaining native test scenarios.

## CEF layout assessment

Windows CEF packaging remains explicitly disabled. The read-only assessor checks
the CEF143/7499 runtime DLL PE machines, resources, default locale and dependencies.
It also reports a same-basename application DLL with the exact undecorated
`RunWinMain` code export, an application EXE byte-identical to the supplied SDK's
`bootstrap.exe`, matching import-library machine, and pinned SDK archive/header
records:

```sh
lintel windows inspect-cef --cef-path path/to/cef --executable Application.exe \
  --target x86_64-pc-windows-msvc
```

`Application.dll` must sit beside `Application.exe`. A forwarded or decorated-only
export does not satisfy the GUI entry contract. The official pinned bootstrap
uses the five-argument `RunWinMain(HINSTANCE, LPWSTR, int, void*, cef_version_info_t*)`
ABI. All CEF child processes use this same EXE; `browser_subprocess_path` stays
empty. `--helper` remains accepted for inspecting old layouts but prevents
`bootstrap_inputs_valid` from becoming true. Do not substitute the stale
four-argument Rust sample or fabricate sandbox state.

The JSON separates conventional `layout_valid`, `bootstrap_inputs_valid`,
`client_entry_export_verified`, `bootstrap_matches_sdk`,
`sdk_version_metadata_matches`, `sdk_import_library_machine_verified`, and
`sandbox_archive_machine_verified`. Local archive metadata is not independently
authenticated, so `sdk_archive_verified`, `bootstrap_verified`,
`native_execution_verified`, and `packaging_enabled` remain false. A local
SHA1 string or matching files does not establish official SDK provenance, code
signature relationships, native ABI linkage, or an operational sandbox. No report
bypasses the packaging refusal.

The matching SDK is CEF **143.0.14**, commit **dd46a37/3340**, Chromium
**143.0.7499.193**, with target-specific `archive.json`, `include/cef_version.h`,
`include/cef_sandbox_win.h`, `include/cef_version_info.h`, `libcef.lib`, official
GUI bootstrap and runtime closure. The matching-version official bootstrap is
the supported design; an arbitrary `cef_sandbox.lib` is not proof of a working
M138+ sandbox. Native MSVC/Windows SDK/CMake/Ninja linkage, consistent distribution
signing, unrelated-working-directory startup, CPU OSR interaction, sandboxed
subprocess inspection and complete shutdown still require the actual SDK and a
Windows machine. The contract follows the pinned
[CEF sandbox header](https://github.com/chromiumembedded/cef/blob/dd46a37/include/cef_sandbox_win.h),
[bootstrap implementation](https://github.com/chromiumembedded/cef/blob/dd46a37/libcef_dll/bootstrap/bootstrap_win.cc),
and [official sandbox requirements](https://chromiumembedded.github.io/cef/sandbox_setup).

## Verification status

The PE, DLL/import-library, path, registry, layout, repeat-publication, and rollback
regressions use synthetic PE/COFF fixtures on macOS. These tests do not execute a
Windows application or prove a particular Windows loader/SDK combination works.
Current-user Registry transactions, process handles/waits, filesystem replacement,
launcher execution, Restart Manager/file-handle deletion, health pipes/timeouts,
exact-child termination, and ACL behavior still require actual Windows CI or a
Windows machine. Managed installer/updater source and model tests are implemented; this is
not a native Windows installation/update acceptance result. MSI/NSIS generation
is not provided. Explicit [SignTool integration](windows-signing.md),
[signed offline distribution](windows-distribution.md), and the experimental
public CEF bootstrap/CPU OSR source are implemented; they do not establish
certificate-chain trust, native installation or sandbox/bootstrap execution.

`lintel-build --all-targets` has also been checked for both installed
`i686-pc-windows-msvc` and `x86_64-pc-windows-msvc` Rust targets from macOS. Windows
updater HTTPS uses system Schannel; non-Windows clients retain rustls. Neither
backend relaxes certificate, redirect, signed-manifest, or archive checks. ARM64
PE handling is covered by fixtures, with no ARM64 execution claim.

PE field interpretation follows Microsoft's [PE format specification](https://learn.microsoft.com/en-us/windows/win32/debug/pe-format).
