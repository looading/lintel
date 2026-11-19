/// Add the CEF Framework search paths to the binaries of a macOS Cargo application.
///
/// Call this from the application's `build.rs` before building the executable
/// that will be placed in the main app and its Helper bundles.
pub fn configure_macos_cef() {
  println!("cargo::rerun-if-env-changed=CARGO_CFG_TARGET_OS");
  if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
    return;
  }

  for path in [
    "@executable_path/../Frameworks",
    "@executable_path/../Frameworks/Chromium Embedded Framework.framework/Libraries",
    "@executable_path/../../../../Frameworks",
  ] {
    println!("cargo::rustc-link-arg-bins=-Wl,-rpath,{path}");
  }
}
