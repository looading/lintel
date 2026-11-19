//! The pinned CEF 143 Windows GUI-bootstrap contract. No broker is synthesized here.

use anyhow::{Result, ensure};

/// Exact CEF 143 `cef_version_info_t` layout from `include/cef_version_info.h`.
/// The pinned Rust bindings omit this bootstrap-only record.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CefBootstrapVersion {
  pub size: usize,
  pub cef_version_major: i32,
  pub cef_version_minor: i32,
  pub cef_version_patch: i32,
  pub cef_commit_number: i32,
  pub chrome_version_major: i32,
  pub chrome_version_minor: i32,
  pub chrome_version_build: i32,
  pub chrome_version_patch: i32,
}

impl CefBootstrapVersion {
  fn validate(&self) -> Result<()> {
    ensure!(
      self.size >= std::mem::size_of::<Self>(),
      "CEF bootstrap version record is truncated"
    );
    // CEF commit dd46a37, matching the checked public 143.0.14 SDK headers.
    ensure!(
      self.cef_version_major == cef::sys::CEF_VERSION_MAJOR
        && self.cef_version_minor == cef::sys::CEF_VERSION_MINOR
        && self.cef_version_patch == cef::sys::CEF_VERSION_PATCH
        && self.cef_commit_number == 3340
        && self.chrome_version_major == cef::sys::CHROME_VERSION_MAJOR
        && self.chrome_version_minor == cef::sys::CHROME_VERSION_MINOR
        && self.chrome_version_build == cef::sys::CHROME_VERSION_BUILD
        && self.chrome_version_patch == cef::sys::CHROME_VERSION_PATCH,
      "CEF bootstrap does not match the pinned public CEF 143.0.14 runtime"
    );
    Ok(())
  }
}

fn validate_sandbox_switches(mut has_switch: impl FnMut(&str) -> bool) -> Result<()> {
  for switch in ["no-sandbox", "disable-gpu-sandbox"] {
    ensure!(
      !has_switch(switch),
      "public Windows CEF rejects --{switch}; the official bootstrap sandbox must stay enabled"
    );
  }
  Ok(())
}

#[cfg(target_os = "windows")]
mod scope {
  use super::*;
  use anyhow::Context;
  use cef::ImplCommandLine as _;
  use std::{cell::Cell, ffi::c_void, ptr::NonNull};

  #[derive(Clone, Copy)]
  struct Bootstrap {
    instance: NonNull<c_void>,
    sandbox: NonNull<c_void>,
  }

  thread_local! {
    static BOOTSTRAP: Cell<Option<Bootstrap>> = const { Cell::new(None) };
  }

  struct ResetScope;
  impl Drop for ResetScope {
    fn drop(&mut self) {
      let _ = BOOTSTRAP.try_with(|slot| slot.set(None));
    }
  }

  /// Executes the host synchronously within an official GUI-bootstrap callback.
  ///
  /// # Safety
  /// All pointers must come from the same invocation of the matching CEF 143
  /// `bootstrap.exe` RunWinMain callback. `version` must be aligned and readable
  /// for a full CefBootstrapVersion. The sandbox must remain valid for this call.
  /// Run CEF on this thread and do not retain/bootstrap CEF work after `run` returns.
  /// The SDK creates/owns/destroys sandbox services in the executable, never this DLL.
  pub unsafe fn with_windows_bootstrap<R>(
    instance: *mut c_void,
    sandbox: *mut c_void,
    version: *const CefBootstrapVersion,
    run: impl FnOnce() -> Result<R>,
  ) -> Result<R> {
    let instance = NonNull::new(instance).context("CEF bootstrap instance handle is null")?;
    let sandbox = NonNull::new(sandbox).context("CEF bootstrap sandbox information is null")?;
    ensure!(
      !version.is_null() && (version as usize) % std::mem::align_of::<CefBootstrapVersion>() == 0,
      "CEF bootstrap version pointer is null or misaligned"
    );
    // SAFETY: The caller supplies an aligned live SDK record. Read its size
    // prefix before copying any version fields, so a shorter SDK record is
    // rejected before accessing fields beyond the advertised record.
    let size = unsafe { version.cast::<usize>().read() };
    ensure!(
      size >= std::mem::size_of::<CefBootstrapVersion>(),
      "CEF bootstrap version record is truncated"
    );
    // SAFETY: The caller guarantees the record is readable, and the prefix
    // above confirms it advertises all fields in this pinned layout.
    let version = unsafe { version.read() };
    version.validate()?;
    run_in_scope(Bootstrap { instance, sandbox }, run)
  }

  fn run_in_scope<R>(bootstrap: Bootstrap, run: impl FnOnce() -> Result<R>) -> Result<R> {
    BOOTSTRAP.with(|slot| {
      ensure!(
        slot.get().is_none(),
        "nested CEF Windows bootstrap scopes are not supported"
      );
      slot.set(Some(bootstrap));
      let _reset = ResetScope;
      run()
    })
  }

  fn current() -> Result<Bootstrap> {
    BOOTSTRAP.with(|slot| slot.get()).context(
      "public Windows CEF requires the matching official bootstrap.exe and a RunWinMain client DLL; ordinary EXE startup has no sandbox context",
    )
  }

  /// Returns only the caller-owned opaque pointer; no sandbox layout is copied.
  pub fn sandbox_info() -> Result<*mut u8> {
    Ok(current()?.sandbox.as_ptr().cast())
  }

  pub fn main_args() -> Result<cef::MainArgs> {
    Ok(cef::MainArgs {
      instance: cef::sys::HINSTANCE(current()?.instance.as_ptr().cast()),
    })
  }

  /// Reads the native UTF-16 process command line without rebuilding it through
  /// UTF-8 argv or changing its Windows quoting/argument boundaries.
  pub fn command_line() -> Result<cef::CommandLine> {
    let _ = current()?;
    // SAFETY: GetCommandLineW returns the process-owned, NUL-terminated command
    // line for the process lifetime. It is read-only here; no pointer escapes.
    let native = unsafe { windows::Win32::System::Environment::GetCommandLineW() };
    ensure!(!native.is_null(), "Windows process command line is unavailable");
    // SAFETY: The OS contract above supplies a valid terminated UTF-16 buffer.
    let units = unsafe { native.as_wide() };
    let raw = cef::sys::_cef_string_utf16_t {
      str_: native.0.cast_mut(),
      length: units.len(),
      dtor: None,
    };
    let value = cef::CefString::from(raw);
    let line = cef::command_line_create().context("cannot create the CEF command-line object")?;
    line.init_from_string(Some(&value));
    validate_sandbox_switches(|switch| line.has_switch(Some(&switch.into())) == 1)?;
    Ok(line)
  }

  #[cfg(test)]
  mod tests {
    use super::*;

    #[test]
    fn bootstrap_scope_policy_restores_tls_on_error_and_unwind_without_native_calls() {
      // Scope-policy test only: these opaque markers are never dereferenced or
      // passed to CEF. They do not simulate a functioning sandbox broker.
      let marker = Bootstrap {
        instance: NonNull::dangling(),
        sandbox: NonNull::dangling(),
      };
      assert!(current().is_err());
      run_in_scope(marker, || {
        assert!(current().is_ok());
        assert!(run_in_scope(marker, || Ok(())).is_err());
        assert!(current().is_ok());
        Ok(())
      })
      .unwrap();
      assert!(current().is_err());
      assert!(run_in_scope(marker, || Err::<(), _>(anyhow::anyhow!("test failure"))).is_err());
      assert!(current().is_err());
      let result =
        std::panic::catch_unwind(|| run_in_scope(marker, || -> Result<()> { panic!("scope-policy fixture") }));
      assert!(result.is_err());
      assert!(current().is_err());
    }
  }
}

#[cfg(target_os = "windows")]
pub use scope::{command_line, main_args, sandbox_info, with_windows_bootstrap};

#[cfg(test)]
mod tests {
  use super::*;

  fn pinned() -> CefBootstrapVersion {
    CefBootstrapVersion {
      size: std::mem::size_of::<CefBootstrapVersion>(),
      cef_version_major: 143,
      cef_version_minor: 0,
      cef_version_patch: 14,
      cef_commit_number: 3340,
      chrome_version_major: 143,
      chrome_version_minor: 0,
      chrome_version_build: 7499,
      chrome_version_patch: 193,
    }
  }

  #[test]
  fn bootstrap_header_layout_and_version_require_exact_pinned_sdk() {
    assert_eq!(
      std::mem::offset_of!(CefBootstrapVersion, cef_version_major),
      std::mem::size_of::<usize>()
    );
    assert_eq!(
      std::mem::size_of::<CefBootstrapVersion>(),
      std::mem::size_of::<usize>() + 8 * std::mem::size_of::<i32>()
    );
    pinned().validate().unwrap();
    let mut truncated = pinned();
    truncated.size -= 1;
    assert!(truncated.validate().is_err());
    let mut wrong = pinned();
    wrong.cef_version_patch += 1;
    assert!(wrong.validate().is_err());
    let mut wrong = pinned();
    wrong.chrome_version_build += 1;
    assert!(wrong.validate().is_err());
  }

  #[test]
  fn public_sandbox_rejects_explicit_bypasses_but_allows_cef_service_policy() {
    for forbidden in ["no-sandbox", "disable-gpu-sandbox"] {
      let switches = [forbidden, "type", "service-sandbox-type"];
      let error = validate_sandbox_switches(|name| switches.contains(&name)).unwrap_err();
      assert!(error.to_string().contains(forbidden));
    }
    // Chromium legitimately passes service-sandbox-type=none for some services.
    let permitted = ["type", "service-sandbox-type", "disable-gpu", "lang"];
    validate_sandbox_switches(|name| permitted.contains(&name)).unwrap();
    validate_sandbox_switches(|_| false).unwrap();
  }

  #[cfg(target_os = "windows")]
  #[test]
  fn ordinary_executable_has_no_implicitly_created_sandbox_context() {
    assert!(sandbox_info().is_err());
    assert!(main_args().is_err());
  }
}
