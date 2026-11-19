//! Experimental Windows CEF cdylib. App.exe must be the matching official GUI bootstrap.
//! This source does not create or emulate sandbox services. See WINDOWS-CEF.md.
#![allow(non_snake_case)]

#[cfg(not(target_os = "windows"))]
compile_error!("This starter is Windows-only. Use cargo check --lib --target x86_64-pc-windows-msvc --features api-only for type checks on another host.");

#[cfg(target_os = "windows")]
fn app_main() -> lintel::Result<()> {
  // The official bootstrap calls this DLL on its application thread. Lintel keeps the
  // borrowed sandbox context scoped to this callback and performs CEF subprocess dispatch.
  lintel::webview_app()
    .id(concat!("dev.example.", env!("CARGO_PKG_NAME")))
    .title("Lintel Windows CEF (Experimental)")
    .local_page(include_str!("../ui/index.html"))
    .run(env!("CARGO_PKG_VERSION"))
}

// Exports exactly the pinned five-argument C GUI entry. Do not replace it with a
// conventional Rust main(), fabricate sandbox_info, or launch another helper EXE.
#[cfg(target_os = "windows")]
lintel::windows_cef_entry!(app_main);
