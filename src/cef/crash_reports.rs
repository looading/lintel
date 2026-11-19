//! Opt-in local CEF Crashpad reporting. The signed bundle supplies the static policy.

use std::{
  collections::BTreeMap,
  ffi::OsString,
  fs,
  io::Read,
  path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

const MAX_CONFIG_BYTES: u64 = 64 * 1024;

/// Local-only CEF Crashpad storage. The bundle must contain an approved
/// `crash_reporter.cfg`; use the CLI's `--cef-crash-reports` opt-in when packaging.
/// This backend replaces the browser's native crash hook and cannot run alongside
/// the Embark host reporter. Currently accepted only for macOS CEF bundles.
///
/// The persistent root must be exclusive to one active CEF process (CEF 120+ uses
/// a profile singleton lock). Choose a different directory for simultaneous app instances.
#[derive(Clone, Debug)]
pub struct CefCrashReports {
  directory: PathBuf,
  application_id: String,
}

impl CefCrashReports {
  pub fn new(directory: impl Into<PathBuf>) -> Self {
    Self {
      directory: directory.into(),
      application_id: "lintel".into(),
    }
  }

  pub fn directory(&self) -> &Path {
    &self.directory
  }

  pub(crate) fn with_application_id(mut self, application_id: String) -> Self {
    self.application_id = application_id;
    self
  }

  pub(crate) fn prepare_directory(&self) -> Result<PathBuf> {
    ensure!(
      self.directory.is_absolute() && self.directory.to_str().is_some(),
      "CEF crash report directory must be absolute and valid UTF-8"
    );
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
      use std::os::unix::fs::DirBuilderExt;
      builder.mode(0o700);
    }
    builder
      .create(&self.directory)
      .context("cannot create CEF crash report directory")?;
    let metadata = fs::symlink_metadata(&self.directory)?;
    ensure!(
      metadata.is_dir() && !metadata.file_type().is_symlink(),
      "CEF crash report directory must be a real directory"
    );
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "CEF crash report directory must be private (0700)"
      );
    }
    self
      .directory
      .canonicalize()
      .context("cannot resolve CEF crash report directory")
  }

  pub(crate) fn annotate_browser(&self) {
    ::cef::set_crash_key_value(Some(&"lintel_app".into()), Some(&self.application_id.as_str().into()));
    ::cef::set_crash_key_value(
      Some(&"lintel_instance".into()),
      Some(&std::process::id().to_string().as_str().into()),
    );
  }
}

pub(crate) fn config_path(executable: &Path) -> Result<PathBuf> {
  let directory = executable
    .parent()
    .context("cannot find executable directory for CEF crash configuration")?;
  #[cfg(target_os = "macos")]
  if directory.file_name().is_some_and(|name| name == "MacOS")
    && directory
      .parent()
      .and_then(Path::file_name)
      .is_some_and(|name| name == "Contents")
  {
    return Ok(directory.parent().unwrap().join("Resources/crash_reporter.cfg"));
  }
  Ok(directory.join("crash_reporter.cfg"))
}

pub(crate) fn validate_before_load(reports: Option<&CefCrashReports>, native_reporter: bool) -> Result<()> {
  let executable = std::env::current_exe()?;
  validate_for_executable(&executable, reports, native_reporter, |key| std::env::var_os(key))
}

fn validate_for_executable(
  executable: &Path,
  reports: Option<&CefCrashReports>,
  native_reporter: bool,
  environment: impl Fn(&str) -> Option<OsString>,
) -> Result<()> {
  let path = config_path(executable)?;
  let present = path.try_exists().context("cannot inspect CEF crash configuration")?;
  if reports.is_none() && !present {
    return Ok(());
  }
  ensure!(
    !native_reporter,
    "CEF Crashpad configuration cannot be combined with the Embark native reporter"
  );
  let reports = reports
    .context("bundle contains crash_reporter.cfg; configure .cef_crash_reports(absolute_directory) explicitly")?;
  ensure!(
    cfg!(target_os = "macos"),
    "CEF local Crashpad directory integration currently supports macOS bundles only"
  );
  ensure!(
    reports.directory.is_absolute() && reports.directory.to_str().is_some(),
    "CEF crash report directory must be absolute and valid UTF-8"
  );
  ensure!(
    present,
    "CEF Crashpad requires bundled crash_reporter.cfg (package with --cef-crash-reports)"
  );
  for key in [
    "CEF_CRASH_REPORTER_SERVER_URL",
    "CEF_CRASH_REPORTER_RATE_LIMIT_ENABLED",
    "BREAKPAD_DUMP_LOCATION",
  ] {
    ensure!(
      environment(key).is_none_or(|value| value.is_empty()),
      "CEF crash reporting rejects inherited {key} override"
    );
  }
  let metadata = fs::symlink_metadata(&path)?;
  ensure!(
    metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= MAX_CONFIG_BYTES,
    "CEF crash configuration must be a small regular file"
  );
  let mut bytes = Vec::new();
  fs::File::open(&path)?
    .take(MAX_CONFIG_BYTES + 1)
    .read_to_end(&mut bytes)?;
  ensure!(
    bytes.len() as u64 <= MAX_CONFIG_BYTES,
    "CEF crash configuration exceeds its size limit"
  );
  let text = std::str::from_utf8(&bytes).context("CEF crash configuration must be UTF-8")?;
  validate_local_config(text)
}

fn validate_local_config(text: &str) -> Result<()> {
  ensure!(
    !text.starts_with('\u{feff}'),
    "CEF crash configuration must not have a BOM"
  );
  let mut section = "";
  let mut config = BTreeMap::new();
  let mut keys = BTreeMap::new();
  for line in text.lines() {
    ensure!(
      line.len() < 900,
      "CEF crash configuration line exceeds the native parser limit"
    );
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
      continue;
    }
    if line == "[Config]" || line == "[CrashKeys]" {
      section = line;
      continue;
    }
    ensure!(
      !line.starts_with('[') && !section.is_empty(),
      "unsupported CEF crash configuration section"
    );
    let (key, value) = line.split_once('=').context("invalid CEF crash configuration entry")?;
    let (key, value) = (key.trim(), value.trim());
    let entries = if section == "[Config]" { &mut config } else { &mut keys };
    ensure!(
      !key.is_empty() && entries.insert(key, value).is_none(),
      "duplicate or empty CEF crash configuration key"
    );
  }
  ensure!(
    config.get("ServerURL") == Some(&""),
    "CEF crash reporting must be local-only (empty ServerURL)"
  );
  ensure!(
    config
      .get("BrowserCrashForwardingEnabled")
      .is_some_and(|value| *value == "0" || value.eq_ignore_ascii_case("false")),
    "CEF browser crash forwarding must be disabled"
  );
  ensure!(
    config
      .get("RateLimitEnabled")
      .is_some_and(|value| *value == "1" || value.eq_ignore_ascii_case("true")),
    "CEF crash report rate limiting must remain enabled"
  );
  for key in config.keys() {
    ensure!(
      [
        "ProductName",
        "ProductVersion",
        "BrowserCrashForwardingEnabled",
        "ServerURL",
        "RateLimitEnabled",
        "MaxUploadsPerDay",
        "MaxDatabaseSizeInMb",
        "MaxDatabaseAgeInDays"
      ]
      .contains(key),
      "unsupported CEF crash configuration key {key}"
    );
  }
  for (key, maximum) in [
    ("MaxUploadsPerDay", 100),
    ("MaxDatabaseSizeInMb", 1024),
    ("MaxDatabaseAgeInDays", 365),
  ] {
    let value: u32 = config
      .get(key)
      .with_context(|| format!("CEF crash configuration is missing {key}"))?
      .parse()?;
    ensure!(
      (1..=maximum).contains(&value),
      "invalid CEF crash retention limit {key}"
    );
  }
  ensure!(
    keys.get("lintel_app") == Some(&"medium")
      && keys.get("lintel_instance") == Some(&"small")
      && keys.get("lintel_browser") == Some(&"small"),
    "CEF crash configuration is missing required Lintel crash keys"
  );
  for (key, value) in keys {
    ensure!(
      key.len() <= 64
        && key
          .bytes()
          .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        && ["small", "medium", "large"].contains(&value),
      "invalid CEF crash key"
    );
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  const LOCAL: &str = "[Config]\nProductName=Lintel\nBrowserCrashForwardingEnabled=false\nServerURL=\nRateLimitEnabled=true\nMaxUploadsPerDay=1\nMaxDatabaseSizeInMb=20\nMaxDatabaseAgeInDays=7\n[CrashKeys]\nlintel_app=medium\nlintel_instance=small\nlintel_browser=small\n";

  #[test]
  fn config_requires_local_only_and_rejects_override_or_duplicate_policy() {
    validate_local_config(LOCAL).unwrap();
    for invalid in [
      LOCAL.replace("ServerURL=\n", "ServerURL=https://example.org/upload\n"),
      LOCAL.replace("ServerURL=\n", "ServerURL=\nServerURL=\n"),
      LOCAL.replace(
        "BrowserCrashForwardingEnabled=false",
        "BrowserCrashForwardingEnabled=true",
      ),
      LOCAL.replace("MaxDatabaseSizeInMb=20", "MaxDatabaseSizeInMb=0"),
      LOCAL.replace("ProductName=Lintel", "ExternalHandler=minidumper"),
      LOCAL.replace("[CrashKeys]", "[Unknown]"),
      format!("\u{feff}{LOCAL}"),
    ] {
      assert!(validate_local_config(&invalid).is_err());
    }
  }

  #[test]
  fn accidental_bundle_config_or_native_hook_is_rejected_before_cef_load() {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("App.app/Contents/MacOS/App");
    let path = config_path(&executable).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, LOCAL).unwrap();
    let reports = CefCrashReports::new(root.path().join("reports"));
    assert!(validate_for_executable(&executable, None, false, |_| None).is_err());
    assert!(validate_for_executable(&executable, Some(&reports), true, |_| None).is_err());
    #[cfg(target_os = "macos")]
    {
      validate_for_executable(&executable, Some(&reports), false, |_| None).unwrap();
      for rejected in [
        "CEF_CRASH_REPORTER_SERVER_URL",
        "BREAKPAD_DUMP_LOCATION",
        "CEF_CRASH_REPORTER_RATE_LIMIT_ENABLED",
      ] {
        assert!(
          validate_for_executable(&executable, Some(&reports), false, |key| (key == rejected)
            .then(|| "forbidden".into()))
          .is_err()
        );
      }
    }
  }

  #[cfg(unix)]
  #[test]
  fn report_root_is_private_and_symlink_or_shared_directory_is_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = tempfile::tempdir().unwrap();
    let reports = CefCrashReports::new(root.path().join("reports"));
    let directory = reports.prepare_directory().unwrap();
    assert_eq!(fs::metadata(&directory).unwrap().permissions().mode() & 0o077, 0);
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(reports.prepare_directory().is_err());
    let link = root.path().join("linked");
    symlink(&directory, &link).unwrap();
    assert!(CefCrashReports::new(link).prepare_directory().is_err());
  }
}
