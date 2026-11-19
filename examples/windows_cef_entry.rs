//! API-only fixture for the pinned Windows cdylib export. This binary is not a bootstrap.
#[cfg(all(target_os = "windows", feature = "public-cef"))]
fn cef_application() -> lintel::Result<()> {
  lintel::webview_app()
    .id("dev.lintel.examples.windows-cef")
    .local_page("<!doctype html><html><body>Lintel Windows CEF</body></html>")
    .run(env!("CARGO_PKG_VERSION"))
}

#[cfg(all(target_os = "windows", feature = "public-cef"))]
lintel::windows_cef_entry!(cef_application);

fn main() {
  eprintln!(
    "Build the exported entry as App.dll and use the matching official CEF bootstrap; this fixture does not execute it."
  );
}
