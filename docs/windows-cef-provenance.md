# Windows CEF archive provenance

Lintel can verify the actual compressed bytes of the pinned official CEF 143
Windows minimal archives on macOS, Linux, or Windows. It then compares the
complete extracted SDK against the recorded file hashes and rejects unknown
files/directories. This proves agreement with the reviewed distribution bytes.
It does not execute Windows code, check Authenticode trust, prove native ABI
linkage or sandbox operation, or enable Windows CEF packaging.

The CEF project identifies the [Spotify CEF CDN as its official download
service](https://chromiumembedded.github.io/cef/). The records below were retrieved
from its [release index](https://cef-builds.spotifycdn.com/index.json), and each
download's actual SHA1 also matched the corresponding `.tar.bz2.sha1` sidecar.
SHA256 values are Lintel's measured pins of those verified downloads; the index
publishes SHA1, not these SHA256 values. The checked index snapshot SHA256 is
`af3a6bc54b6b57c1bcd6f98e1e86d97f687d53559627a4c5f36d6919c886d399`.

All three records select CEF `143.0.14+gdd46a37`, commit
`dd46a3761b5389255e1824c81ae1b131459f7cd0`, commit number `3340`, Chromium
`143.0.7499.193`, stable channel. They were published on January 8, 2026. The
verification is intentionally pinned to this historical release.

| Target | Exact official minimal download | Compressed bytes | Official SHA1 | Measured SHA256 |
| --- | --- | ---: | --- | --- |
| `i686-pc-windows-msvc` | [windows32](https://cef-builds.spotifycdn.com/cef_binary_143.0.14%2Bgdd46a37%2Bchromium-143.0.7499.193_windows32_minimal.tar.bz2) | 129230189 | `6a36fce4a63c129c8d9ec8325c7caf5fc38218e9` | `b1a4e9649dc8aefedbf1aa5797376b1fe12ef29d14c7f722a3a58fa95150a496` |
| `x86_64-pc-windows-msvc` | [windows64](https://cef-builds.spotifycdn.com/cef_binary_143.0.14%2Bgdd46a37%2Bchromium-143.0.7499.193_windows64_minimal.tar.bz2) | 150087605 | `f75c50c0710a2fc07b13941bab8a02f7b6bc0442` | `a2d4c52075faa3e82cc0fefb1825d66b122dffaa240ee840b6c964f5933f515e` |
| `aarch64-pc-windows-msvc` | [windowsarm64](https://cef-builds.spotifycdn.com/cef_binary_143.0.14%2Bgdd46a37%2Bchromium-143.0.7499.193_windowsarm64_minimal.tar.bz2) | 148503950 | `d7c8108cccd33019f514852497c1a9408e8ef347` | `872c546f8dad168e9e8f39e6e7f6d7e11eca8bcf12ff42c4db2cf3322d853898` |

The fixed manifest is
`crates/lintel-build/src/windows_cef/cef143_minimal_manifest.json`. It contains
962, 964, and 964 original files respectively, including headers, build inputs,
bootstrap, import library, runtime DLLs, resources and every locale. Changing a
pin requires a new reviewed official download and evidence; no network fetch or
caller-supplied manifest can replace the verifier's built-in pins.

## Read-only CLI and Rust API

Keep the downloaded archive outside a fresh, complete extracted SDK directory.
The original archive remains unchanged; inspection never extracts or executes it.
For a linked application, keep the candidate `App.exe` and its `App.dll` outside
the SDK directory too:

```sh
lintel windows inspect-cef \
  --cef-path /absolute/path/to/extracted-windows64-sdk \
  --archive /absolute/path/to/cef_binary_143.0.14+gdd46a37+chromium-143.0.7499.193_windows64_minimal.tar.bz2 \
  --executable /absolute/path/to/candidate/App.exe \
  --target x86_64-pc-windows-msvc
```

The additive host-independent Rust API is:

```rust,ignore
let report = lintel_build::inspect_windows_cef_layout_with_archive(
  sdk,
  app_exe,
  None, // CEF children use the same official bootstrap, not a separate helper.
  "x86_64-pc-windows-msvc",
  compressed_archive,
)?;
println!("{}", report.to_json()?);
```

The old `inspect_windows_cef_layout` entry and CLI invocation without `--archive`
remain available, but they cannot set `sdk_archive_verified` or the new byte
provenance flags. The stronger archive API currently supports the three minimal
releases above. The layout-only metadata check recognizes the official standard
archive filename without an invented `_standard` suffix; this does not add a
standard-archive byte pin.

For an SDK-only audit before linking a client DLL, `--executable` may point to
the SDK's `Release/bootstrap.exe`. Its provenance and PE machine can be checked,
but `client_entry_export_verified` and `bootstrap_inputs_valid` remain false
because no same-basename application DLL has been linked. Do not create a fake
client DLL to make that report appear complete.

Inspect the JSON fields separately:

| Field | Meaning |
| --- | --- |
| `sdk_archive_verified` | The actual compressed file has the expected length and pinned SHA256 |
| `sdk_files_match_archive` | All original SDK files match their byte pins, with no unapproved extra files/directories |
| `archive_verification.bootstrap_file_verified` | The supplied SDK GUI bootstrap matches its recorded official bytes |
| `bootstrap_archive_provenance_verified` | The candidate application EXE byte-matches that verified official GUI bootstrap |
| `client_entry_export_verified` | The same-basename DLL has the exact, non-forwarded `RunWinMain` code export |
| `bootstrap_verified`, `native_execution_verified`, `packaging_enabled` | Remain false; source bytes and static layout do not establish native execution |

The command returns an assessment, including failed checks; automation must
inspect the relevant boolean fields and diagnostics. Successful JSON generation
alone is not validation. The SDK audit is a point-in-time, read-only snapshot,
not a future file-identity lease or an installation/publication authorization.

## Strict extracted-SDK policy

The verifier checks all pinned original files and enumerates the complete SDK
tree. Unknown DLLs, resources, files and even unknown empty directories reject
complete SDK provenance. Symlinks/reparse points and unsupported file types are
rejected. Enumeration is bounded to 8192 entries and 32 levels, and it never
descends into an unknown directory. Each file is size-bounded before streaming
hashing, with growth during hashing also detected. The raw `Release`/`Resources`
layout and a complete cef-rs layout with those two directories flattened are
recognized. Builds that add objects or generated files inside the SDK should
audit a fresh source extraction instead.

`archive.json` is **not included in these official archives**. It is download or
repack bookkeeping added by other tooling. The original raw SDK needs no such
file when actual archive bytes and all SDK files verify. A root-level
`archive.json` is the only extra-file exception, and if present its exact pinned
version, platform, kind and published SHA1 must validate. A self-reported SHA1,
even copied from the official index, never substitutes for actual byte hashes.

The real minimal distributions also do not contain `cef_sandbox.lib`. The
[official M138+ sandbox design](https://chromiumembedded.github.io/cef/sandbox_setup)
uses prebuilt `bootstrap.exe`/`bootstrapc.exe`; missing the old sandbox archive is
not a missing GUI-bootstrap input. Lintel's initial entry uses the five-argument
GUI ABI from the
[pinned sandbox header](https://github.com/chromiumembedded/cef/blob/dd46a37/include/cef_sandbox_win.h).
The complete runtime and resource directories remain required by the
[CEF distribution layout](https://chromiumembedded.github.io/cef/general_usage.html).

## Recorded evidence and remaining acceptance

The real archives were downloaded over certificate-verified HTTPS on a Mac host.
The index snapshot, response headers, official SHA1 sidecars, actual compressed
hashes, safe extraction tool, full file manifest and bootstrap static records are
under `build/checklist-platform/`. The official bootstrap PE certificate tables
are empty for all three downloaded architectures; no Authenticode trust result
is claimed. This exact-byte audit applies before re-signing or modifying a
bootstrap. A later signed/modified artifact needs its own explicitly reviewed
receipt rather than silently bypassing the source-byte check.

Real CEF also imports the Windows Bluetooth module `bthprops.cpl`; Microsoft's
[BluetoothFindFirstRadio requirements](https://learn.microsoft.com/en-us/windows/win32/api/bluetoothapis/nf-bluetoothapis-bluetoothfindfirstradio)
identify that exact system DLL. The PE parser accepts that specific system
module and continues to reject arbitrary `.cpl` imports.

Windows native MSVC/SDK linking, the actual client ABI, matching signing
relationships, loader startup, CPU-rendered UI/input/IME/popups/resize, renderer
recovery, sandbox process inspection and shutdown still require a Windows host.
Nothing in this archive verifier opens the packaging gate or replaces that
acceptance with `API_ONLY` output.
