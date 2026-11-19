//! Explicit offline distribution production/inspection and the existing protected Windows installer.
use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use std::{
  fs,
  io::Read,
  path::{Path, PathBuf},
};

#[derive(Args)]
pub(super) struct DistributionArgs {
  #[command(subcommand)]
  command: DistributionCommand,
}
#[derive(Subcommand)]
enum DistributionCommand {
  /// Plan by default; --execute produces a real signed managed-input directory.
  Stage {
    bundle: PathBuf,
    #[arg(long)]
    launcher: PathBuf,
    #[arg(long)]
    updater: PathBuf,
    /// Pinned update/distribution public key from the producer's trusted configuration.
    #[arg(long)]
    public_key: PathBuf,
    #[arg(long)]
    output_dir: PathBuf,
    #[arg(long, default_value = "stable")]
    channel: String,
    /// Read only during --execute; never copied into a distribution or printed in the plan.
    #[arg(long, requires = "execute")]
    private_key: Option<PathBuf>,
    #[arg(long, conflicts_with = "execute")]
    dry_run: bool,
    #[arg(long, requires = "private_key")]
    execute: bool,
  },
  /// Verify the complete signed tree against an independently trusted caller public key.
  Inspect {
    directory: PathBuf,
    #[arg(long)]
    public_key: PathBuf,
  },
}
#[derive(Args)]
pub(super) struct InstallDistributionArgs {
  directory: PathBuf,
  /// Trust anchor from a separate trusted channel/configuration, not package self-identification.
  #[arg(long)]
  public_key: PathBuf,
  #[arg(long)]
  root: Option<PathBuf>,
  /// Optional binding to the exact manifest reviewed by an earlier inspect/plan invocation.
  #[arg(long)]
  expected_manifest_sha256: Option<String>,
  #[arg(long, conflicts_with = "execute")]
  dry_run: bool,
  /// Actual installation requires Windows; otherwise this command only inspects inputs.
  #[arg(long)]
  execute: bool,
}
fn public_key(path: &Path) -> Result<String> {
  // The caller owns this external trust anchor. Bound reading and reject final symlink/reparse.
  let metadata = fs::symlink_metadata(path)?;
  ensure!(
    metadata.is_file() && !metadata.is_symlink(),
    "trust anchor must be a real file"
  );
  #[cfg(windows)]
  {
    use std::os::windows::fs::MetadataExt;
    ensure!(
      metadata.file_attributes() & 0x400 == 0,
      "trust anchor cannot be a reparse point"
    );
  }
  let mut bytes = Vec::new();
  fs::File::open(path)?.take(4097).read_to_end(&mut bytes)?;
  ensure!(bytes.len() <= 4096, "public key file exceeds 4 KiB");
  Ok(String::from_utf8(bytes)?)
}
impl DistributionArgs {
  pub(super) fn run(self) -> Result<()> {
    match self.command {
      DistributionCommand::Stage {
        bundle,
        launcher,
        updater,
        public_key: key_file,
        output_dir,
        channel,
        private_key,
        dry_run,
        execute,
      } => {
        let plan = lintel_build::plan_windows_distribution(&lintel_build::WindowsDistributionOptions {
          bundle,
          launcher,
          updater,
          public_key: public_key(&key_file)?,
          channel,
          output_dir,
        })?;
        let _ = dry_run;
        if execute {
          println!(
            "{}",
            lintel_build::stage_windows_distribution(
              &plan,
              private_key
                .as_deref()
                .context("distribution execution requires --private-key")?
            )?
            .to_json()?
          );
        } else {
          println!("{}", plan.to_json()?);
        }
      }
      DistributionCommand::Inspect {
        directory,
        public_key: key_file,
      } => println!(
        "{}",
        lintel_build::inspect_windows_distribution(&directory, &public_key(&key_file)?)?.to_json()?
      ),
    }
    Ok(())
  }
}
impl InstallDistributionArgs {
  pub(super) fn run(self) -> Result<()> {
    let key = public_key(&self.public_key)?;
    let expected = self.expected_manifest_sha256.as_deref();
    if let Some(expected) = expected {
      ensure!(
        expected.len() == 64 && expected.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "expected manifest SHA256 must contain 64 hex digits"
      );
    }
    let _ = self.dry_run;
    if self.execute {
      // The exact opaque verified snapshot previewed here is consumed by the protected installer.
      let lease = lintel_build::lease_windows_distribution(&self.directory, &key)?;
      ensure!(
        expected.is_none_or(|expected| lease.info().manifest_sha256.eq_ignore_ascii_case(expected)),
        "distribution differs from the reviewed manifest snapshot"
      );
      println!(
        "{}",
        lintel_updater::install_verified_windows_distribution(lease, self.root.as_deref())?.display()
      );
    } else {
      let info = lintel_build::inspect_windows_distribution(&self.directory, &key)?;
      ensure!(
        expected.is_none_or(|expected| info.manifest_sha256.eq_ignore_ascii_case(expected)),
        "distribution differs from the reviewed manifest snapshot"
      );
      println!("{}", info.to_json()?);
    }
    Ok(())
  }
}
#[cfg(test)]
mod tests {
  use crate::Cli;
  use clap::Parser;
  fn stage() -> Vec<&'static str> {
    vec![
      "lintel",
      "windows",
      "distribution",
      "stage",
      "App",
      "--launcher",
      "launcher.exe",
      "--updater",
      "updater.exe",
      "--public-key",
      "updater.pub",
      "--output-dir",
      "dist",
    ]
  }
  #[test]
  fn distribution_cli_separates_review_stage_and_native_install_without_overrides() {
    assert!(Cli::try_parse_from(stage()).is_ok());
    let mut execute = stage();
    execute.push("--execute");
    assert!(Cli::try_parse_from(execute).is_err());
    let mut signed = stage();
    signed.extend(["--execute", "--private-key", "release.key"]);
    assert!(Cli::try_parse_from(signed).is_ok());
    let mut both = stage();
    both.extend(["--dry-run", "--execute", "--private-key", "release.key"]);
    assert!(Cli::try_parse_from(both).is_err());
    assert!(
      Cli::try_parse_from([
        "lintel",
        "windows",
        "distribution",
        "inspect",
        "dist",
        "--public-key",
        "trusted.pub"
      ])
      .is_ok()
    );
    assert!(
      Cli::try_parse_from([
        "lintel",
        "windows",
        "install-distribution",
        "dist",
        "--public-key",
        "trusted.pub",
        "--execute"
      ])
      .is_ok()
    );
    assert!(
      Cli::try_parse_from([
        "lintel",
        "windows",
        "install-distribution",
        "dist",
        "--public-key",
        "trusted.pub",
        "--channel",
        "beta"
      ])
      .is_err()
    );
    assert!(Cli::try_parse_from(["lintel", "windows", "install-distribution", "dist"]).is_err());
  }
}
