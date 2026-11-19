# Test-only Echo protocol

`echo.mojom` is the real protocol used by this crate's typed `Send + 'static`
connection and call contracts. `echo.mojom.rs` is the unchanged output of
`mojom-build = 0.3.0`, using its bundled Chromium Rust generator at revision
`bcbc09a9f8e4c3fd3ad2ed9cbad41e63c35ccfdd` and runtime path `::mojo_ipc`.

The Rust output is checked in so registry package unit tests do not depend on a
private workspace package, build-machine absolute paths, or an extra protocol
generation step. It is compiled only under `cfg(test)` and is not a public
application protocol. It retains the generated Remote/Receiver and wire codecs;
it is not a hand-written substitute interface. No native runtime is loaded by
the compile-contract tests.

The source IDL also remains in the Git checkout's private `tests/protocol`
development package. Regenerate with that package's existing `mojom-build`
configuration and copy only the resulting Rust file. The local module index
uses a relative path; do not copy Cargo configuration, parser metadata or an
absolute generated module index into the package.

The Chromium generator/template notice is retained in `LICENSE-CHROMIUM`.
