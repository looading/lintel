//! Standalone host for the imported CEF initialization and renderer process.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use cef::{ImplBrowser, ImplBrowserHost};
use lintel_iced_widgets::webview::{
  BrowserId, BrowserProcessMessage, init_cef, init_cef_with_root_cache, pre_init_cef,
  renderer_process::ChildProcessApp, task::cef_post_task,
};
use tokio::sync::mpsc::UnboundedReceiver;

mod crash_reports;
#[cfg(target_os = "macos")]
mod macos_app;
pub use crash_reports::CefCrashReports;
pub(crate) use crash_reports::validate_before_load as validate_crash_reports;

/// Runs the application inside the official CEF 143 Windows GUI bootstrap.
/// Export the SDK's five-argument RunWinMain entry with `windows_cef_entry!`.
///
/// # Safety
/// Pointers must be the live instance, sandbox services and aligned version record
/// passed by the matching official bootstrap.exe. Run synchronously on that entry
/// thread; do not fabricate sandbox state or retain it beyond the callback.
#[cfg(target_os = "windows")]
pub unsafe fn windows_sandbox_main(
  instance: *mut std::ffi::c_void,
  sandbox_info: *mut std::ffi::c_void,
  version_info: *const std::ffi::c_void,
  application: impl FnOnce() -> Result<()>,
) -> i32 {
  use std::io::Write;
  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    // SAFETY: This adapter forwards the exact SDK callback contract to its
    // same-thread RAII scope. No ownership of the foreign services is taken.
    unsafe {
      crate::webview::windows_bootstrap::with_windows_bootstrap(
        instance,
        sandbox_info,
        version_info.cast(),
        application,
      )
    }
  }));
  match result {
    Ok(Ok(())) => 0,
    Ok(Err(error)) => {
      let _ = writeln!(std::io::stderr(), "Lintel CEF startup failed: {error:#}");
      1
    }
    Err(_) => {
      let _ = writeln!(
        std::io::stderr(),
        "Lintel CEF application unwound at the bootstrap boundary"
      );
      1
    }
  }
}

/// Exports the pinned five-argument GUI entry for an application's Windows cdylib.
/// Package with the matching official bootstrap.exe, not a Rust ordinary EXE.
#[cfg(target_os = "windows")]
#[macro_export]
macro_rules! windows_cef_entry {
  ($application:path) => {
    #[unsafe(no_mangle)]
    /// # Safety
    /// Called only by the matching official CEF 143 bootstrap with its live SDK arguments.
    pub unsafe extern "C" fn RunWinMain(
      instance: *mut ::std::ffi::c_void,
      _command_line: *mut u16,
      _command_show: i32,
      sandbox_info: *mut ::std::ffi::c_void,
      version_info: *const ::std::ffi::c_void,
    ) -> i32 {
      // SAFETY: The matching official bootstrap is the only supported caller
      // of this exported ABI. The adapter catches unwinding panics and resets TLS.
      unsafe { $crate::cef::windows_sandbox_main(instance, sandbox_info, version_info, $application) }
    }
  };
}

/// The process that returned from the CEF host entry point.
#[derive(Debug, PartialEq, Eq)]
pub enum Process {
  Browser,
  Helper,
}

/// Run CEF with an isolated local profile and forward its pump to the app runner.
/// The browser callback must wait for browser close notifications before returning.
pub fn run(
  app_version: &'static str,
  run_application: impl FnOnce(&Path, UnboundedReceiver<BrowserProcessMessage>) -> Result<()>,
) -> Result<Process> {
  run_configured(app_version, None, run_application)
}

/// Uses CEF's own local-only Crashpad handler for the browser and sandboxed children.
pub fn run_with_crash_reports(
  app_version: &'static str,
  reports: CefCrashReports,
  run_application: impl FnOnce(&Path, UnboundedReceiver<BrowserProcessMessage>) -> Result<()>,
) -> Result<Process> {
  run_configured(app_version, Some(reports), run_application)
}

pub(crate) fn run_configured(
  app_version: &'static str,
  reports: Option<CefCrashReports>,
  run_application: impl FnOnce(&Path, UnboundedReceiver<BrowserProcessMessage>) -> Result<()>,
) -> Result<Process> {
  let process_type = process_type(std::env::args_os().skip(1))?;
  #[cfg(target_os = "windows")]
  let args = cef::args::Args::from(crate::webview::windows_bootstrap::main_args()?);
  #[cfg(not(target_os = "windows"))]
  let args = cef::args::Args::new();
  #[cfg(target_os = "windows")]
  let windows_sandbox_info = crate::webview::windows_bootstrap::sandbox_info()?;
  #[cfg(not(target_os = "windows"))]
  let windows_sandbox_info = std::ptr::null_mut();
  if process_type.is_none() {
    validate_crash_reports(reports.as_ref(), false)?;
  }
  #[cfg(target_os = "macos")]
  let _sandbox = process_type.as_ref().map(|_| {
    // CEF requires initializing the helper sandbox before loading the Framework.
    let mut sandbox = cef::sandbox::Sandbox::new();
    sandbox.initialize(args.as_main_args());
    sandbox
  });
  let _library = pre_init_cef()?;

  if let Some(kind) = process_type {
    // Preserve app/src/process_helper.rs: only the renderer receives the renderer app.
    let mut app = (kind == "renderer").then(|| ChildProcessApp::new(app_version));
    let code = cef::execute_process(Some(args.as_main_args()), app.as_mut(), windows_sandbox_info);
    ensure!(code == 0, "CEF helper {kind} exited with {code}");
    return Ok(Process::Helper);
  }

  #[cfg(target_os = "macos")]
  macos_app::install()?;

  let profile = tempfile::Builder::new().prefix("lintel-cef-").tempdir()?;
  let profile_dir = profile.path().canonicalize()?;
  let report_root = reports.as_ref().map(CefCrashReports::prepare_directory).transpose()?;
  let runtime_cache = report_root
    .as_ref()
    .map(|root| {
      let mut builder = tempfile::Builder::new();
      builder.prefix("runtime-");
      #[cfg(unix)]
      {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
      }
      builder.tempdir_in(root)
    })
    .transpose()?;
  let cache_dir = runtime_cache
    .as_ref()
    .map(|cache| cache.path().to_path_buf())
    .unwrap_or_else(|| profile_dir.join("cef"));
  let download_dir = profile_dir.join("downloads");
  std::fs::create_dir_all(cache_dir.join("logs/applogs"))?;
  std::fs::create_dir(&download_dir)?;
  lintel_lib_env::LintelEnv::init(
    0,
    app_version.parse()?,
    &std::env::current_exe()?.canonicalize()?,
    &profile_dir,
    &cache_dir,
    0,
    &download_dir,
  )?;

  let initialized = match &report_root {
    Some(root) => init_cef_with_root_cache(app_version, None, Some(&cache_dir), root)?,
    None => init_cef(app_version, None, Some(&cache_dir))?,
  };
  let (_app, pump, _gpu_info) = initialized.context("expected CEF browser process")?;
  let result = match &reports {
    Some(_) if cef::crash_reporting_enabled() != 1 => Err(anyhow::anyhow!(
      "CEF Crashpad was not actually enabled; verify the bundled crash_reporter.cfg and helper bootstrap"
    )),
    Some(reports) => {
      reports.annotate_browser();
      tracing::info!(directory = ?report_root, uploads = false, "CEF local Crashpad reporting enabled");
      run_application(&profile_dir, pump)
    }
    None => run_application(&profile_dir, pump),
  };
  shutdown()?;
  result?;
  Ok(Process::Browser)
}

fn process_type(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Option<String>> {
  let mut arguments = arguments.into_iter();
  while let Some(argument) = arguments.next() {
    if argument == "--type" {
      let value = arguments.next().context("CEF --type requires an ASCII value")?;
      return decode_process_type(value.as_encoded_bytes()).map(Some);
    }
    if let Some(value) = argument.as_encoded_bytes().strip_prefix(b"--type=") {
      return decode_process_type(value).map(Some);
    }
  }
  Ok(None)
}

fn decode_process_type(bytes: &[u8]) -> Result<String> {
  ensure!(
    !bytes.is_empty() && bytes.is_ascii(),
    "CEF --type requires a nonempty ASCII value"
  );
  Ok(std::str::from_utf8(bytes)?.to_owned())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn helper_dispatch_precedes_report_policy_and_supports_crashpad_handler() {
    assert_eq!(
      process_type(["--type=renderer".into()]).unwrap().as_deref(),
      Some("renderer")
    );
    assert_eq!(
      process_type(["--type".into(), "crashpad-handler".into()])
        .unwrap()
        .as_deref(),
      Some("crashpad-handler")
    );
    assert_eq!(process_type(["--application-argument".into()]).unwrap(), None);
    assert!(process_type(["--type".into()]).is_err());
    assert!(process_type(["--type=".into()]).is_err());
  }

  #[cfg(unix)]
  #[test]
  fn cef_type_selector_skips_unrelated_non_unicode_arguments_and_rejects_invalid_type() {
    use std::os::unix::ffi::OsStringExt;
    let unrelated = std::ffi::OsString::from_vec(vec![0xff, b'X']);
    assert_eq!(
      process_type([unrelated.clone(), "--type=renderer".into()])
        .unwrap()
        .as_deref(),
      Some("renderer")
    );
    assert_eq!(
      process_type([unrelated.clone(), "--type".into(), "crashpad-handler".into()])
        .unwrap()
        .as_deref(),
      Some("crashpad-handler")
    );
    assert_eq!(process_type([unrelated.clone()]).unwrap(), None);
    assert!(process_type(["--type".into(), unrelated]).is_err());
    assert!(process_type([std::ffi::OsString::from_vec(b"--type=\xff".to_vec())]).is_err());
  }
}

/// Request close while retaining the browser state until its CEF close event arrives.
pub fn close_browser(id: BrowserId) -> Result<()> {
  cef_post_task(cef::sys::cef_thread_id_t::TID_UI.into(), move || {
    if let Some(host) = cef::browser_host_get_browser_by_identifier(id.inner()).and_then(|browser| browser.host()) {
      host.close_browser(1);
    }
  })?;
  Ok(())
}

fn shutdown() -> Result<()> {
  #[cfg(any(target_os = "macos", target_os = "windows"))]
  // CefShutdown belongs to the main application/initialization thread, even
  // when CEF owns a separate UI thread via multi_threaded_message_loop.
  cef::shutdown();

  tracing::info!("CEF runtime shut down");
  Ok(())
}
