fn main() {
  if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
    return;
  }

  // The same example binary runs from the main app and its CEF helper bundles.
  for path in [
    "@executable_path/../Frameworks",
    "@executable_path/../Frameworks/Chromium Embedded Framework.framework/Libraries",
    "@executable_path/../../../../Frameworks",
  ] {
    println!("cargo::rustc-link-arg-examples=-Wl,-rpath,{path}");
  }
}
