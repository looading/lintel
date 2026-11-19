# Lintel CLI

Create, build, run and package applications using the Lintel desktop framework.
The command supports native, WebView and multiprocess service starters. Native
dependencies, signing credentials and execution permissions remain explicit.

```sh
cargo install lintel-cli
lintel describe --json
lintel new --template native my-application
```

The registry package contains an audited framework source archive and its
SHA-256. Its build reads only package-local resources and writes generated views
into Cargo's `OUT_DIR`; it does not recover sources from a publisher's checkout,
download a replacement archive, or execute archive contents.

For maintainers, prepare publication from a complete matching checkout:

1. Run `python3 crates/lintel-cli/resources/prepare-release.py --sync` to sync the
   generator's package-local resources and MIT licenses from canonical sources.
2. Build the normal CLI in that checkout. Its build script regenerates the full
   framework archive after checking source policy and resource consistency.
3. Run `python3 crates/lintel-cli/resources/prepare-release.py --archive PATH`,
   where PATH is that build's `OUT_DIR/lintel-framework.tar.gz`.
4. Run the script with `--check`, then inspect and verify the actual Cargo package
   before publishing. The generated archive and checksum stay ignored in Git,
   while Cargo's explicit include list puts them into the registry package.

The preparation script verifies every source member and the complete expected
member set against the current checkout. A stale archive is rejected. Archive
integrity, source-policy validation and native platform acceptance are separate
checks. SHA-256 inside a package is an integrity check, not independent publisher
authentication.

See the [Lintel repository](https://github.com/looading/lintel) for platform
requirements and current acceptance limits. Preserve all upstream notices.
