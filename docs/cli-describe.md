# CLI discovery and application working guides

Use the installed CLI to discover its embedded framework contract:

```sh
lintel describe
lintel describe --json
```

`--json` writes exactly one JSON object followed by a newline on stdout. The
object has `schema_version: 1` and `kind: "lintel.framework"`. It comes from the
same `lintel.project.json` compiled into this CLI; the human summary reads that
catalog too. No separate CLI-owned feature, command or platform catalog is
maintained.

Running this command does not read a project from the current directory, inspect
environment values, look for SDKs, start Cargo, download dependencies, build,
launch an application or contact a service. A local `lintel.project.json` cannot
override the embedded contract. The repository's `scripts/lintel.sh` development
wrapper can build the CLI before invoking it; use an already-built or installed
binary for a read-only discovery operation.

## Schema version 1

The top-level fields are:

| Field | Meaning |
| --- | --- |
| `schema_version` | Machine-contract version, currently integer `1` |
| `kind` | Contract identity, `lintel.framework` |
| `framework` | Name, version, summary and repository-relative root |
| `entry_points` | Public symbols, source locations, needed features and implementation status where applicable |
| `features` | Optional feature names, supported platform scope and explicit validation boundaries |
| `crate_areas` | Focused first-party source locations |
| `templates` | Starter entry/source paths, validation commands, optional platform-scoped run command and boundaries |
| `commands` | Discoverable CLI commands, descriptive effects and verification limits |
| `verification` | AI Dev Loop default, platform limits and relevant guides |

Consumers should check the version/kind, accept additional fields within the
version, and preserve the distinction between implementation and platform
acceptance. A catalog status or a `read-only` effect description does not grant
execution permission. Discovery does not replace a host doctor, SDK link check,
native sandbox test, user permission or completed application behavior.

Breaking the machine contract requires a schema-version change. Human formatting
is intended for reading and is not the machine interface. The embedded contract
describes this CLI build; an application's pinned framework can be a different
revision.

## Generated applications

`lintel new` keeps the existing native, webview and windows-webview choices and
creates consumer-root `AGENTS.md` and `llms.txt`. These are small application
entry guides. The generator reads each template's `app_entry`, `source_paths`,
`validation_commands`, optional `run_command` and `validation_boundary` from the
same catalog, so the instructions follow the selected starter.

Portable applications carry their framework under `.lintel/framework/`, including
the framework's own `AGENTS.md`, `llms.txt` and `lintel.project.json`. The consumer
guides link to those pinned files. Checkout-based projects resolve the actual
`lintel` dependency path through `Cargo.toml`; arbitrary absolute checkout paths
are not interpolated into instruction Markdown. The existing `EmbeddedStarter`
API also accepts older archives: when discovery documents are absent, the
consumer guide points to the real pinned manifest rather than broken links.

The generated working agreement keeps AI Dev Loop opt-in, separates checks from
running/publishing/installing, protects secrets, and keeps application-owned
tool grants and task scopes explicit. It does not copy the framework's complete
internal verification workflow into application development.

For the experimental Windows WebView starter, the catalog lists the API-only
cross-check and its limits. The generated guides do not add a default native
run command or imply that CEF linking, bootstrap/sandbox execution or packaging
has been accepted. Native and macOS WebView starters retain their existing source
files, features and build behavior.
