# Public source and namespace cleanup plan

**Goal:** Use the Lintel namespace consistently, remove application-specific code and embedded credentials, and make source and CLI-generated projects use the same pinned public dependencies.

**Constraints:** Preserve the existing runtime/window/plugin/rendering implementation. Preserve third-party copyright and license notices. Do not alter the separate application repository, global Git identity/security hooks or system credential stores. Do not commit, publish, create remotes or enable AI Dev Loop. Existing private materials and caches are moved outside the project for reversible isolation.

## Source and dependency ownership

- [x] Rename framework events, environment, bridge data and header/JS/config names to `Lintel`, `lintel` and `LINTEL_*` consistently across first-party callers.
- [x] Remove unused application protocol, URL registry, signed product requests, remote time lookup, product launcher and encoded product fixtures rather than shipping neutral-looking wrappers around them.
- [x] Use `lintel` as the default directory organization while retaining explicit application ID behavior. Read only `lintel-runtime.yml`; no hidden fallback to an application-specific config.
- [x] Keep only the already verified public iced/winit/wgpu/CEF paths. Vendor the same public iced revision and functional patch without Git metadata or obsolete empty organization features. Remove private Git sources and regenerate public lock graphs without changing the selected upstream revisions.
- [x] Remove private renderer callbacks, switches and unused platform stubs. Preserve OSR, native bootstrap, sandbox, window ownership and scoped cancellation behavior.

## Distribution and repository boundary

- [x] Package the same public source used by the root checkout. Remove build-time product-specific rewriting and enforce generic Git-source, sensitive-file and credential checks before archive publication.
- [x] Supply additional release deny rules through an external private validation file; do not encode the removed identifiers or secrets into shipped tests or scanner code. Report only rule IDs and locations.
- [x] Move old SDKs, generated consumers, validation snapshots, cached checkouts, dumps and compiled artifacts outside the repository; retain only source, public vendored code, required assets and licenses.
- [x] Rebuild validation artifacts outside the source tree and ensure final source/archive inventories have no private URLs, credentials, local user paths or embedded Git directories. Keep machine/global settings outside the cleanup boundary.

## Verification

- [x] Run source/privacy scans and manifest/lock checks; distinguish real secrets from API fields and deterministic test fixtures without printing credential values.
- [x] Run affected configuration/core/bridge tests, generator/CLI/source-policy tests and relevant native framework checks using one Cargo operation at a time.
- [x] Generate a fresh independent app from the rebuilt CLI, compare its public source and verify native multiwindow and WebView behavior, then check optional process/crash integration.
- [x] Perform scoped independent review, document intentional low-level naming/API changes, and report platform checks separately from native acceptance.
