# Windows Authenticode signing

Lintel provides explicit Windows SDK SignTool plans for native portable bundles
and standalone PE EXE/DLL tools. Planning works on macOS without running Windows
code. Actual signing requires Windows, an explicitly selected trusted SDK tool,
and a code-signing certificate already available through the Windows certificate
provider. This implementation does not import PFX files, accept passwords, select
a certificate automatically, request elevation, produce MSI/NSIS packages, or
enable Windows CEF packaging.

## Review the plan

Prepare a separate, existing output directory. The original input is never signed
in place, and the final destination must not already exist. Use an exact public
certificate thumbprint and an approved HTTPS RFC3161 timestamp endpoint:

```powershell
lintel windows sign 'build\windows\Example-1.2.0-x86_64' `
  --output-dir 'build\signed-windows' `
  --signtool 'C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64\signtool.exe' `
  --certificate-sha1 $certificateThumbprint `
  --timestamp-url $approvedHttpsRfc3161Url --dry-run
```

Omitting both `--dry-run` and `--execute` also produces only a JSON plan. It records
the canonical input/tool/output paths, full SHA256 input and tool snapshots,
executable-image hashes, selected certificate/store, and exact phase arguments.
No output artifact or network timestamp request is made by planning. Replace the
SDK version/path with your verified installed SDK; a path or recorded hash alone
does not prove that an SDK binary is official.

The SHA1 thumbprint is a certificate identifier, not the signature digest. File
signing and RFC3161 timestamping both explicitly request **SHA256**. The default
certificate store is current-user `My`; `--certificate-store NAME` and
`--machine-store` select another store explicitly. The key stays in its installed
provider, including a supported hardware provider. This API never serializes a
private key and offers no `/p` password argument. Timestamp URLs containing
userinfo, query tokens or fragments are rejected rather than echoed in a plan.

Standalone tools use the same workflow with an explicit target:

```powershell
lintel windows sign 'tools\lintel-launcher.exe' `
  --target x86_64-pc-windows-msvc --output-dir 'build\signed-tools' `
  --signtool $verifiedSignTool --certificate-sha1 $certificateThumbprint `
  --timestamp-url $approvedHttpsRfc3161Url
```

The Rust APIs are `AuthenticodeOptions`, `plan_windows_authenticode_bundle`,
`plan_windows_authenticode_file`, and `execute_windows_authenticode`. Plans have
private fields and cannot be deserialized into unchecked executable commands.
The CLI reconstructs and validates a current snapshot on every invocation;
it does not execute arbitrary commands from JSON.

## Execute and verify

Add `--execute` on Windows after reviewing the inputs. Every code file is signed
and timestamped, then a final full verification pass runs after all signing writes.
The per-file command sequence is, without a command shell:

```text
signtool sign /fd SHA256 /sha1 EXACT_THUMBPRINT /s My STAGED_FILE
signtool timestamp /tr APPROVED_HTTPS_RFC3161_URL /td SHA256 STAGED_FILE
signtool verify /pa /all /tw STAGED_FILE
```

`/sm` is added only when requested. There is no `/a` automatic certificate
selection, append-signature mode, catalog fallback, SHA1 file-signing default,
missing-timestamp exemption, or warning-success mode. Exit **0** is required
for every phase; exit **1**, warning exit **2**, startup errors, verification
failures and timeouts stop publication. The default per-phase child-runtime
timeout is 300 seconds, configurable within 1..600 seconds. Only the retained
SignTool child may be terminated for cleanup; application processes are not
terminated. OS process creation and provider/OS behavior still need native tests.

Tool stdout/stderr are suppressed so certificate-provider diagnostic text cannot
accidentally echo credentials. Errors identify the file, phase and exit code;
they do not dump a command, environment, password or provider output. A provider
that requires interaction must be prepared for the build account; Lintel does
not provide a PIN/password prompt or bypass the provider's policy.

Input/tool paths reject traversal and every symlink/reparse ancestor. Native
execution pins the tool hash through a read handle that denies write/delete and
holds tool/output ancestor handles that deny directory replacement. The output
and SDK paths must not grant untrusted principals write/delete-child access;
existing ACLs are checked and never rewritten. The trust check recognizes the
fixed OS TrustedInstaller SID for existing owners/grants; it does not trust an
arbitrary service SID or account with the same friendly name. Staging is created with a
protected DACL at creation for the current SID, SYSTEM and Administrators. New
files are copied as byte streams into that tree, without copying source ACLs.
This does not isolate the signing key from malicious code already running as
the signing account or a privileged system administrator. Use a trusted,
dedicated signing account/SDK and preserve your provider's access restrictions.

Every phase compares hashes of all inputs: sign/timestamp may change only the
current file, while verify may change no file. Before publication, every input
must again satisfy its original PE image invariant and contain a certificate
table. The PE image invariant allows only checksum, certificate-directory fields and an aligned EOF WIN_CERTIFICATE
table to change. Code/header/resource bytes remain pinned to the input image. The
conservative parser rejects ambiguous certificate placement, unsupported overlay
arrangements, oversized tables and malformed certificate entries. It does not
implement its own PKCS7 certificate-chain or timestamp verifier: final trust and
timestamp validation are delegated to the explicitly trusted SDK SignTool using
`/pa /all /tw`. The SHA256 algorithms are enforced through the sign/timestamp
arguments. A model runner returning success is never native signing evidence.

After all files pass, a bundle's original identity/file set is checked, its full
file hashes are refreshed, and the complete portable manifest/PE/import closure
is validated again. The new artifact is published exclusively to the chosen
output directory. Standalone publication requires same-volume hard-link support
and refuses replacement; unsupported filesystems fail closed. A failed phase
leaves the source intact and produces no success receipt. OS-locked temporary
files may require later private staging cleanup; no failure is labeled complete.

## Release and managed-install order

Authenticode changes full PE bytes and therefore changes the portable manifest
hash. Perform operations in this order:

1. Build the portable application and the standalone launcher/updater/CLI tools.
2. Authenticode-sign copies and require the final SignTool verification stages.
3. Stage/install the signed inputs, so managed ownership records capture their
   final hashes.
4. Generate the Ed25519 update archive/envelope from the signed portable output.

Do not sign a payload after `lintel updater release` and then claim the old
Ed25519 envelope still validates it. Neither file hashes nor the signed archive
remain the same. Authenticode and the pinned Ed25519 updater signature serve
separate checks; one does not replace the other. Existing `windows install`
remains an explicit local initial-trust operation and does not silently acquire
an Authenticode policy. A signing receipt is not an installer or a Windows CEF
sandbox acceptance result.

## Verification boundary

Mac host tests use synthetic PE and certificate-table fixtures plus a fake runner
to exercise argument policy, stage ordering, original preservation, code/manifest
tampering rejection, reparse/path checks, collisions and every phase failure.
They do not create a valid Authenticode signature. MSVC target checks cover
Windows-specific API/ACL/process code types only. Native certificate/provider
selection, SignTool trust/revocation/timestamp behavior, protected ACL creation,
sharing/rename races and filesystem publication require actual Windows execution.
No such native execution is claimed while a Windows machine is unavailable.

Parameters, policies and exit codes follow Microsoft's primary
[SignTool reference](https://learn.microsoft.com/en-us/windows/win32/seccrypto/signtool)
and [file-signing guide](https://learn.microsoft.com/en-us/windows/win32/seccrypto/using-signtool-to-sign-a-file).
