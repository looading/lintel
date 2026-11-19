# lintel-build

Build, scaffold, package and verify Lintel desktop applications. The library
reuses Cargo and the framework's existing native package publishers. Application
build options, helper declarations and signing remain explicit.

The published crate contains the real generator catalog, consumer guides and
template resources under `resources/`. It does not read a publisher's original
checkout to compile. `create_project` still requires an explicitly selected
complete framework checkout; the portable generator consumes the framework
archive supplied by its caller.

Source maintenance uses the repository's canonical templates and catalog. Before
packaging, run `python3 crates/lintel-cli/resources/prepare-release.py --sync`,
then its `--check` mode after the CLI framework archive has been prepared. The
checkout CLI build also verifies these small resources against their canonical
sources. Preserve bundled upstream copyright and license notices.

See the [Lintel repository](https://github.com/looading/lintel) for supported
platforms, integration guides and native verification boundaries.
