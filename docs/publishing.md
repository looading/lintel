# Registry release staging

The development workspace keeps its verified vendored Iced adapter and public Git pins. Registry releases are prepared in a separate directory. `scripts/registry-release.py` copies source files and converts Cargo manifests there; it does not edit the development workspace, run Cargo or Git, access the network, read registry credentials, or publish packages.

The primary package is `lintel-framework`. Application manifests can keep the short Rust dependency name:

```toml
[dependencies]
lintel = { package = "lintel-framework", version = "0.1" }
```

## Package identities

The full release graph currently has 50 packages: 21 framework/tool packages, 18 Iced components, 7 WGPU components, Winit, AccessKit's Winit adapter, the CEF wrapper, and Cryoglyph. Excluding `lintel-cli` produces the 49-package framework graph.

Forks use `lintel-` followed by the original package name with underscores replaced by hyphens. The script preserves the original Rust dependency keys and library target names. For example, `iced_core` becomes a dependency on package `lintel-iced-core`, while Rust code still refers to `iced_core`. Framework packages keep their existing package names. Fork versions retain their upstream versions, and framework packages retain `0.1.0`.

The stager flattens workspace package metadata, dependencies, additive features, and lints. Local dependencies point to adjacent staged packages and also declare exact registry versions. Git selectors and development `[patch]` tables are removed. An explicit registry override replaces a verified equivalent dependency with its exact official version. Unknown Git/path dependencies fail before output is published; they are not silently converted to a guessed registry version.

Winit must have one identity across the framework, Iced, the desktop platform crate and AccessKit. WGPU must likewise have one identity across its implementation, Iced, CEF and Cryoglyph. Publishing only a renamed facade while leaving these adapters on an unrelated official crate does not preserve the existing types.

## Source map

Create an external JSON file with the explicit source and registry decisions:

```json
{
  "schema_version": 1,
  "sources": {
    "wgpu": {
      "path": "/verified/cache/wgpu",
      "revision": "a6d9d3254e8d5ed7de08634cd9ddd7b63956f7f3"
    },
    "winit": {
      "path": "/verified/cache/winit",
      "revision": "48116469f15d3bc52b5e56bb516694be8882870d"
    },
    "cef": {
      "path": "/verified/cache/cef-rs",
      "revision": "348fef293fd48b053b6157cecc3e05c36d71d0ca"
    },
    "cryoglyph": {
      "path": "/verified/cache/cryoglyph",
      "revision": "3836ca7a17f410d30871c2254dcc0f2400876636"
    },
    "accesskit_winit": {
      "path": "/verified/registry/accesskit_winit-0.34.0",
      "registry_version": "0.34.0"
    }
  },
  "registry_overrides": {
    "dpi": { "version": "=0.1.1" },
    "naga": { "version": "=28.0.0" },
    "wgpu-types": { "version": "=28.0.0" },
    "cef-dll-sys": { "version": "=143.7.1+143.0.14" },
    "download-cef": { "version": "=2.3.0" },
    "gpu-allocator": { "version": "=0.28.0" }
  },
  "package_names": {},
  "omitted_dev_dependencies": {
    "lintel-process": ["lintel-process-test-protocol"],
    "winit": ["winit"]
  },
  "first_party_license": "MIT"
}
```

`package_names` can explicitly replace a target package name if the default name is unavailable. `registry_overrides` can also specify `package` when its registry name differs. Neither option reserves a name or changes ownership in the registry.

Iced comes from the repository's `vendor/iced`. The script checks its recorded revision, patch hash and the 24 files listed by the patched-file marker. This is a partial adapter check, not authentication of all 583 files. The release operator must separately audit the complete vendored tree against the public revision and adapter changes.

Cached Git sources must report the exact selected revision in their local metadata. Every copied tracked file is checked against the current Git index blob hash. This binds copied bytes to that index; it does not prove that the index belongs to the selected commit. Before staging, the release operator must verify both that HEAD is the selected pin and that the index and working tree match HEAD, retaining that independent receipt. The script reads local metadata directly and never invokes Git or fetches missing objects. Registry snapshots must have matching package versions and valid `.cargo_vcs_info.json`; the maintainer must separately verify their download provenance and registry checksums.

Overrides record equivalence decisions, rather than proving them. WGPU 28.0.0's facade source matches the selected 28.0.1 facade, but its core and HAL omit existing fixes. Cryoglyph 0.1.0 targets older WGPU/cosmic-text APIs, and the official CEF wrapper omits the selected Metal changes. Keep those functional implementations in their forks. Naga, WGPU types, DPI, CEF sys, the downloader and allocator use the separately verified official versions shown above.

Only explicitly listed development dependencies may be omitted. The process test protocol is a repository test fixture, not a runtime dependency or a separately published framework package. Winit also has an upstream self-referential test-only dev dependency on `winit`; explicitly omit that edge to avoid requiring its own not-yet-published registry version. These omissions do not remove production dependencies or change implementation files. They mean the staged registry packages do not claim to run those repository-only test configurations. Keep the repository regression results separate from package verification.

All other internal dev dependencies participate in the publication order. `cargo package` still resolves them from the registry, even when verification does not compile the tests. For example, the WGPU HAL dev dependency on Winit requires `lintel-winit` to be published first. The planner rejects remaining cycles rather than guessing which test dependencies to discard; any additional omission must be supplied explicitly and reviewed.

## Plan and stage

Python 3.11 or newer is required for `tomllib`. First inspect the complete plan:

```sh
python3 scripts/registry-release.py \
  --root . \
  --source-map /verified/release/source-map.json \
  --exclude lintel-cli \
  --plan
```

Create a new external stage after checking package names and source decisions:

```sh
python3 scripts/registry-release.py \
  --root . \
  --source-map /verified/release/source-map.json \
  --exclude lintel-cli \
  --stage /tmp/lintel-registry-stage
```

The destination must not exist and must be outside the source repository. Its parent must already exist. Preparation happens in a temporary sibling and is renamed into place only after conversion and copying succeed. Existing outputs are never overwritten.

The stage contains:

- `packages/<registry-name>/`: independent source packages with normalized manifests, original implementation bytes and preserved license/readme files.
- `Cargo.toml`: a local verification workspace with the development release profiles.
- `plan.json`: resolved package identities, source pins, explicit omissions, dependencies and topological publication order.
- `checksums.json`: sizes and SHA-256 checksums for the prepared files.
- `registry-patches.toml`: absolute paths to the prepared packages for local registry-resolution checks.

The receipts and local patch file stay outside package payloads. They may contain local verification paths. Regenerate the stage after moving it; the patch file is tied to its original location. Source locks are not copied into the release packages; Cargo creates a fresh registry resolution during verification. Cached targets, SDK distributions, Git metadata and nested source packages are excluded from package copying. Template manifests under package resources remain resources and are copied verbatim.

The stager does not claim that a package is registered, that its name is available, or that Cargo has accepted the archive. Its JSON explicitly reports `published: false` and an empty executed-tools list.

## License resources

Each staged package must contain a license text, rather than only an SPDX identifier. Existing upstream notices are copied unchanged from the package or its workspace. A registry snapshot can declare a license while omitting the actual text, so this check can require an additional verified legal resource.

Pass such resources through the same external `--resources-manifest` file used for CLI assets:

```json
{
  "schema_version": 1,
  "packages": {
    "accesskit_winit": {
      "licenses": [
        {
          "source": "/verified/release/licenses/accesskit-LICENSE-APACHE",
          "destination": "LICENSE-APACHE",
          "sha256": "62c7a1e35f56406896d7aa7ca52d0cc0d272ac022b5d2796e7d6905db8a3636a",
          "origin": "https://raw.githubusercontent.com/AccessKit/accesskit/42e53b0d829e7a0b34dc8803bb06012db2e80cc6/LICENSE-APACHE"
        }
      ]
    }
  }
}
```

This AccessKit file is the official license at the selected registry snapshot's revision, 9,723 bytes. It differs in formatting from other projects' Apache license files; do not substitute one based only on its SPDX name. The script verifies its SHA-256, preserves its exact bytes, rejects replacement of a different existing notice, and records the destination, hash and public origin in `plan.json` under `license_resources`. License resources are restricted to top-level legal notice filenames and do not modify implementation files.

## CLI resources

The CLI registry package needs a prepared framework archive and checksum. Generate and audit those assets separately using `crates/lintel-cli/resources/prepare-release.py`. Keep the large archive outside the Git tree and pass it through an external resources manifest:

```json
{
  "schema_version": 1,
  "packages": {
    "lintel-cli": {
      "overlays": [
        {
          "source": "/verified/release/assets/lintel-framework.tar.gz",
          "destination": "resources/lintel-framework.tar.gz"
        },
        {
          "source": "/verified/release/assets/lintel-framework.sha256",
          "destination": "resources/lintel-framework.sha256"
        }
      ]
    }
  }
}
```

Include the CLI by omitting `--exclude lintel-cli` and supplying `--resources-manifest`. Resource overlays are restricted to package resources and cannot replace Rust/shader/native implementation files. Optional manifest overlays can only adjust package file metadata (`include`, `exclude`, `readme`, `license-file`); they cannot inject dependencies or alter package identities. The CLI's `include` list must retain both prepared assets. Verify the resulting `.crate` size before upload.

The archive is an explicit input. The stager does not rewrite its contents. An archive prepared from the Git development graph still generates applications with that graph's public Git pins; a registry-only generated application requires an archive deliberately prepared from the corresponding registry release sources. Audit that choice separately from the CLI package's own dependencies.

## Verification and publication

Run the converter's primitive checks without starting build tools:

```sh
python3 scripts/registry-release.py --self-test
```

The release operator then runs Cargo from the stage, not from the development checkout. Check the normalized graph and all affected targets, inspect each `cargo package --list`, and ensure templates, headers, shaders, license files and CLI resources are included. Check packed crates in an isolated environment without relying on undeclared files outside the package. Reuse the existing implementation tests for unchanged code while recording dependency/packaging verification separately.

`plan.json` gives the publication order, including normal, build and non-omitted dev dependencies on all declared targets and optional capabilities. Root workspace commands alone do not cover the development checkout's separate desktop/platform, process, build, crash, updater and CLI workspaces; the staged plan explicitly contains them.

Use local patches only for pre-publication checks. After prerequisites are registered, verify the final package against the actual registry graph before publishing it. Publish functional prerequisites in the plan order, wait for each required version to become resolvable, and publish `lintel-framework` after its complete dependency closure. Publish the CLI after its generator archive and dependency versions are ready. Credentials and publishing commands remain the release operator's responsibility; the staging script never reads or sends them.
