//! Credential-free SignTool CLI adapter. No shell, PFX password flag, or automatic certificate choice.
use anyhow::{Context, Result, ensure};
use clap::Args;
use std::{path::PathBuf, time::Duration};

#[derive(Args)]
pub(super) struct SignArgs {
  /// A verified portable directory or standalone PE EXE/DLL. Inputs are never modified.
  input: PathBuf,
  /// Existing real output directory. Final destination must not already exist.
  #[arg(long)]
  output_dir: PathBuf,
  /// Explicit Windows SDK signtool.exe path; its SHA256 is pinned in the plan.
  #[arg(long)]
  signtool: PathBuf,
  /// Exact certificate SHA1 thumbprint in the selected store (public selector, not file digest).
  #[arg(long)]
  certificate_sha1: String,
  /// HTTPS RFC3161 TSA endpoint without credentials, query tokens or fragments.
  #[arg(long)]
  timestamp_url: String,
  #[arg(long, default_value = "My")]
  certificate_store: String,
  /// Select LocalMachine certificate store rather than current user. No elevation is requested.
  #[arg(long)]
  machine_store: bool,
  /// Required only for standalone files. Portable directories derive target from their manifest.
  #[arg(long)]
  target: Option<String>,
  #[arg(long,default_value_t=300,value_parser=clap::value_parser!(u64).range(1..=600))]
  phase_timeout_seconds: u64,
  /// Read-only plan is the default; this flag states that intent explicitly.
  #[arg(long, conflicts_with = "execute")]
  dry_run: bool,
  /// Explicitly run all phases on Windows and publish only after strict verification.
  #[arg(long)]
  execute: bool,
}
impl SignArgs {
  pub(super) fn run(self) -> Result<()> {
    let options = lintel_build::AuthenticodeOptions::new(self.signtool, self.certificate_sha1, self.timestamp_url)?
      .certificate_store(&self.certificate_store)?
      .machine_store(self.machine_store)
      .timeout(Duration::from_secs(self.phase_timeout_seconds))?;
    let plan = if std::fs::symlink_metadata(&self.input)?.is_dir() {
      ensure!(
        self.target.is_none(),
        "portable target comes from its manifest; omit --target"
      );
      lintel_build::plan_windows_authenticode_bundle(&self.input, &self.output_dir, options)?
    } else {
      let target = self.target.as_deref().context("standalone signing requires --target")?;
      lintel_build::plan_windows_authenticode_file(&self.input, &self.output_dir, target, options)?
    };
    let _ = self.dry_run;
    if self.execute {
      println!("{}", lintel_build::execute_windows_authenticode(&plan)?.to_json()?);
    } else {
      println!("{}", plan.to_json()?);
    }
    Ok(())
  }
}
#[cfg(test)]
mod tests {
  use crate::Cli;
  use clap::Parser;
  fn args() -> Vec<&'static str> {
    vec![
      "lintel",
      "windows",
      "sign",
      "app.exe",
      "--output-dir",
      "signed",
      "--target",
      "x86_64-pc-windows-msvc",
      "--signtool",
      "C:\\SDK\\signtool.exe",
      "--certificate-sha1",
      "0123456789012345678901234567890123456789",
      "--timestamp-url",
      "https://tsa.example.test",
    ]
  }
  #[test]
  fn signing_requires_explicit_identity_and_timestamp_and_defaults_to_plan() {
    assert!(Cli::try_parse_from(args()).is_ok());
    assert!(Cli::try_parse_from(["lintel", "windows", "sign", "app.exe"]).is_err());
    let mut both = args();
    both.extend(["--execute", "--dry-run"]);
    assert!(Cli::try_parse_from(both).is_err());
    let mut password = args();
    password.extend(["--password", "secret"]);
    assert!(Cli::try_parse_from(password).is_err());
    let mut timeout = args();
    timeout.extend(["--phase-timeout-seconds", "0"]);
    assert!(Cli::try_parse_from(timeout).is_err());
  }
}
