# Experimental Windows CEF cdylib starter

This project generates the source for `App.dll`. It does not generate a GUI
bootstrap EXE, download an SDK, install protocol associations, or enable Lintel's
Windows CEF packager/run path. Native SDK linking, distribution signing, sandbox
execution and UI acceptance remain required on Windows. The ordinary Native and
macOS Webview starters are unchanged.

## API-only check from macOS

Generate using the explicit starter, including from an empty directory:

```sh
lintel new my-windows-app --template windows-webview
cd my-windows-app
cargo check --lib --target x86_64-pc-windows-msvc --features api-only
```

The generated project pins the public framework sources/patches and toolchain in
the same way as the existing starters. `api-only` enables the matching `cef/dox`
feature solely to bypass SDK-native linking during **cargo check**. It does not
verify exports, create a runnable DLL, validate a Windows SDK, or provide a
sandbox. Do not use `api-only` for native builds or distribution. An ordinary
host check is deliberately rejected because this is a Windows-only library.

The CLI's project generation currently runs on macOS; the generated directory
and its embedded framework can then be copied to a Windows build environment.
A checkout-based project generated with `--lintel-path` instead retains its
explicit external checkout dependency.

## Native build conditions

Use the exact SDK selected by the framework:

- CEF **143.0.14**, commit **dd46a37**, commit number **3340**.
- Chromium **143.0.7499.193**, pinned `cef-rs` revision
  **348fef293fd48b053b6157cecc3e05c36d71d0ca**.
- Matching target architecture and official SDK `archive.json` provenance,
  `include/cef_version.h`, `include/cef_sandbox_win.h`,
  `include/cef_version_info.h`, `libcef.lib`, `libcef.dll`, `chrome_elf.dll`,
  official GUI `bootstrap.exe`, and complete runtime/resource closure.
- MSVC C/C++ tools, Windows SDK, the matching Rust MSVC target, and CMake/Ninja
  for the SDK wrapper build. The wrapper uses `/MT`; keep the bootstrap, DLL,
  import libraries and runtime architecture consistent.

For an actual native build, explicitly provide a verified SDK as `CEF_PATH`,
remove `--features api-only`, and build the library on Windows:

```powershell
$env:CEF_PATH = 'C:\verified-sdk\cef'
$env:RUSTFLAGS = '-Ctarget-feature=+crt-static'
cargo build --lib --release --target x86_64-pc-windows-msvc
```

This command is a build prerequisite, not a recorded successful native link.
Upstream CEF's own build tooling controls SDK lookup; do not rely on an automatic
SDK download as provenance validation. `cef/dox` is intentionally absent from the
default feature set. ARM64/x86 builds require their own exact matching SDK and
separate target/runtime acceptance.

## Official bootstrap layout

The library name is deliberately `App`, so a successfully linked target produces
`App.dll`. In a separately reviewed distribution directory, place the matching
official `bootstrap.exe` under the same basename `App.exe`:

```text
App.exe                 official same-version GUI bootstrap.exe, renamed
App.dll                 this cdylib, exporting exact undecorated RunWinMain
libcef.dll
chrome_elf.dll
...                     complete SDK runtime/resource closure
locales/*.pak
```

All browser/renderer/GPU processes use that same official `App.exe`.
`browser_subprocess_path` must remain empty. The exact pinned entry is:

```text
extern "C" int RunWinMain(HINSTANCE, LPWSTR, int,
                         void* sandbox_info, cef_version_info_t* version_info);
```

`lintel::windows_cef_entry!(app_main)` exports this five-argument ABI. It establishes
a same-thread TLS/RAII scope for the real bootstrap instance, sandbox services,
and validated version record throughout `app_main`, including CEF child dispatch.
It does not fabricate broker pointers or disable the sandbox. Calling the DLL
through a generic DLL runner, a normal Rust EXE, or the stale four-argument Rust
example is unsupported and fails the required bootstrap contract. The initial
starter does not support `bootstrapc.exe`/`RunConsoleMain`.

M138+ sandbox libraries require the Chromium toolchain. Presence of an unrelated
`cef_sandbox.lib` does not replace the official bootstrap. Review the SDK's
bootstrap/client/`chrome_elf.dll` signature relationship and sign distribution
binaries consistently; renaming a local fixture is not a signing or provenance
check. No copy/sign/publish command is performed by this starter.

The read-only command below may inspect prepared inputs, with packaging still
blocked:

```sh
lintel windows inspect-cef --cef-path path/to/verified-sdk --executable App.exe \
  --target x86_64-pc-windows-msvc
```

Inspect the final PE for exact **undecorated, case-sensitive `RunWinMain`** and
matching machine types after native link. Layout checks and API-only checks do
not verify the calling ABI or sandbox execution. Before distribution, validate
startup from an unrelated working directory, actual CPU-rendered frames,
input/IME, popup placement, resizing, renderer recovery, complete shutdown, and
native subprocess sandbox state. `packaging_enabled` remains false until the
full distribution/runtime contract has been separately accepted.

Sources: [pinned CEF sandbox ABI](https://github.com/chromiumembedded/cef/blob/dd46a37/include/cef_sandbox_win.h),
[pinned GUI bootstrap](https://github.com/chromiumembedded/cef/blob/dd46a37/libcef_dll/bootstrap/bootstrap_win.cc),
[official sandbox requirements](https://chromiumembedded.github.io/cef/sandbox_setup).
