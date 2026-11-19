# Signed Windows managed distribution directories

Lintel can produce a real offline directory containing a verified portable
application, same-target standalone `lintel-launcher.exe` and `lintel-updater.exe`,
a pinned update public key, and an Ed25519-signed complete file manifest. Directory
production and read-only verification work on macOS without executing a PE file.
Installation uses the existing protected current-user Windows installer.

This directory is managed-install input, not an MSI/NSIS installer, a self-running
setup EXE, or an Authenticode/SmartScreen approval. A receiving machine needs a
compatible installed Lintel CLI or an installer application using the Rust API.
The CLI binary is deliberately not copied implicitly. The producer private key is
read only for `stage --execute`, stays outside the artifact, and is never printed.
Planning/inspection do not read private keys. These operations do not download an
SDK, register protocols, or delete business data. Windows CEF packaging remains
behind its existing gate.

## Produce the final inputs, then stage

Build the portable application and the launcher/updater tools for the same MSVC
target. The tools must have only supported OS DLL imports, as required by the
existing managed installer. If your release policy uses Authenticode, sign and
verify those final binaries **before** distribution planning/staging. See
[windows-signing.md](windows-signing.md). Staging preserves exact PE bytes; it
does not run SignTool or claim certificate-chain validation.

Use the existing Ed25519 key format from `lintel updater keygen`. The producer
public key must match the application's pinned updater configuration. Keep the
private seed file outside all distribution directories, with protected access.

```sh
mkdir -p dist
lintel windows distribution stage build/windows/Example-1.2.0-x86_64 \
  --launcher signed-tools/lintel-launcher.exe \
  --updater signed-tools/lintel-updater.exe \
  --public-key updater.pub --channel stable --output-dir dist --dry-run

# Review the exact sources, targets, roles, sizes and SHA256 snapshots first.
lintel windows distribution stage build/windows/Example-1.2.0-x86_64 \
  --launcher signed-tools/lintel-launcher.exe \
  --updater signed-tools/lintel-updater.exe \
  --public-key updater.pub --channel stable --output-dir dist \
  --execute --private-key release.key
```

Planning is the default and never reads the private key or creates an artifact.
`--execute` requires the matching signing key. The source directory, tool files,
and output parent must be real paths; traversal, symlink/reparse ancestors and
unsupported Windows network/device paths are rejected. Native Windows production
reuses protected staging DACLs and output namespace pins from the signing layer;
the output ancestor chain is checked for untrusted mutation/replacement rights,
without rewriting parent ACLs. Existing system namespaces recognize the exact
TrustedInstaller SID as an OS principal alongside SYSTEM/Administrators; arbitrary
service SIDs and friendly service names are not trusted. New private staging DACLs
remain limited to the current SID, SYSTEM and Administrators. Shared build folders can therefore be refused;
choose a private workspace. Files are created and copied as
byte streams, so source ACLs are not imported into protected staging.

The versioned output is immutable:

```text
dist/Example-1.2.0-x86_64-managed/
  bundle/
    Example.exe
    ...                         registered portable EXE/DLL closure
    lintel-bundle.json
  tools/
    lintel-launcher.exe
    lintel-updater.exe
  updater.pub
  distribution.json
```

Every listed file is hashed from its final bytes. The portable manifest's own
hash is also covered. A private same-volume directory is checked, signed, checked
again, then exclusively published under the output lock. Exact repeats return
only an identically verified existing artifact; changed bytes, key, channel or
other policy with the same output version are refused. Failures never replace an
existing artifact. No recovery path deletes an old output to make publication
succeed. Inputs are not modified.

## Verify against an independent trust anchor

```sh
lintel windows distribution inspect \
  dist/Example-1.2.0-x86_64-managed --public-key independently-trusted-updater.pub
```

The receiving public key must come from a separately trusted channel or compiled
configuration. Using the package's own `updater.pub` as the only authority merely
accepts its self-identification; that is not publisher authentication. Verification
requires the included pin to equal the caller's trusted key.

The signed envelope has schema `1` and domain
`lintel.windows-managed-distribution.v1`. Ed25519 signs the exact canonical payload
bytes prefixed by `Lintel.WindowsManagedDistribution.v1\0`; updater-release
signatures cannot be reinterpreted as distribution signatures. The payload fixes
application identity, version, target, channel, approved protocol schemes, and
**every payload file's permitted relative path, role, byte length and SHA256**.
`distribution.json` is the canonical signed envelope itself, excluded from the
circular self-hash map; its complete digest is returned for optional review binding.
Canonical encoding
rejects ambiguous/duplicate JSON map content. The fixed layout admits no extra
files/directories, case aliases, traversal, drive/UNC/backslash paths, ADS, device
names, links, or unknown roles. All portable PE/import/identity checks and tool
architecture checks run again. Protocol declarations additionally share the native
Windows registration static policy: system schemes and Windows-unsafe names such
as `http`, `CON`, and `custom.` are refused before staging or inspection succeeds.
This does not probe or promise the availability of a scheme association on a
particular machine.

The limits are 512 MiB per input file, 2 GiB total signed inputs, 1 MiB canonical
payload and 4 MiB envelope. These are explicit resource limits, not a claim about
Windows loader compatibility. There is no expiry or automatic downgrade selection
in this initial-install directory protocol; an existing managed root is never
replaced by initial installation. Signed online updater releases retain their
separate expiry/newer-version policy.

Inspection reports `ed25519_manifest_verified=true` only after checking the
explicit trust anchor and the complete directory. It reports
`authenticode_chain_verified=false` and `native_installation_executed=false`.
These checks do not substitute for native certificate, provider, revocation,
Windows edition/runtime dependencies, or sandbox validation.

## Install without changing the signed policy

```powershell
# Review only; works on a non-Windows host too.
lintel windows install-distribution 'Example-1.2.0-x86_64-managed' `
  --public-key independently-trusted-updater.pub --dry-run

# Windows native installation. Optional --root retains the original LocalAppData rules.
lintel windows install-distribution 'Example-1.2.0-x86_64-managed' `
  --public-key independently-trusted-updater.pub `
  --expected-manifest-sha256 $reviewedManifestSha256 --execute
```

The optional manifest digest binds execution to the exact artifact reviewed in an
earlier invocation. The CLI accepts no channel, scheme or tool overrides for a
signed distribution. Those values are taken from the verified manifest. The
ordinary `windows install ...` command remains compatible.

Native consumption acquires no-write/no-delete handles on the verified source
files and pins their parent namespaces, then copies **only the signed file list**
into a new protected private snapshot. The original signed envelope is copied and
its exact digest checked. The full private snapshot is reverified, then sealed
with read-only handles. Later modifications or extra files in the original source
cannot become installation input. Private snapshots are owned by an opaque lease
and cleaned when it drops; OS-locked files can require later private cleanup.

The installer consumes that same lease throughout the existing fresh-root,
LocalAppData, ACL, PE/hash, channel/version, protocol ownership and rollback
protection path. The target is first resolved from its canonical existing ancestor plus strictly
validated new components. An install destination overlapping the original
distribution or private snapshot is refused **before any new directory is created**,
so a multi-level new tail cannot leave parent directories in signed inputs. The
final canonical root is checked again before payload copying, retaining the
existing fresh-root cleanup. Ownership hashes are captured from the final installed bytes.
Existing installations and business data are not replaced or removed.

Rust entry points are `WindowsDistributionOptions`, `plan_windows_distribution`,
`stage_windows_distribution`, `inspect_windows_distribution`, and
`lease_windows_distribution` from `lintel-build`, plus
`install_windows_distribution` / `install_verified_windows_distribution` from
`lintel-updater`. Plans and leases have private fields and cannot be deserialized
or constructed as unchecked execution tokens. A native installer UI can obtain a
lease, inspect `lease.info()`, and consume that **same** verified snapshot after
its normal user decision; it need not reopen mutable source paths.

Generate any Ed25519 online update release/archive only after all relevant
Authenticode signing has finished. Never modify binaries after producing the
signed distribution manifest and reuse the old envelope. Ed25519 authenticity,
Authenticode chain trust, and native execution acceptance are separate results.
The signing/staging account and privileged SYSTEM/Administrators are trusted;
this is not isolation from malicious code running as that same account.

## Current verification boundary

Tests on macOS create real distribution folders with synthetic PE files and real
Ed25519 signatures. They exercise complete-tree verification, independent keys,
policy/content/role/identity changes, protocol-domain separation, unsafe names,
case aliases, unknown entries, links, immutable repeat/collision behavior,
injected failures, final-byte snapshots and original-input preservation. The
existing installer file/layout core is exercised with those verified fixtures.
Windows x64/i686 checks cover the native lease/DACL/installer code types only.

No Windows machine is available for native execution. Real source/file-sharing
races, protected DACL creation, temp/filesystem behavior, LocalAppData installation,
Registry/protocol effects, actual launcher/updater processes and signed Windows
binaries still require Windows acceptance. Source/model/target-check evidence must
not be reported as a native installation pass.
