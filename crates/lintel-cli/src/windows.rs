use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};

#[path = "windows_distribution.rs"]
mod distribution;
#[path = "windows_signing.rs"]
mod signing;

#[derive(Args)]
pub(crate) struct WindowsArgs {
  #[command(subcommand)]
  command: WindowsCommand,
}

#[derive(Subcommand)]
enum WindowsCommand {
  /// Produce or inspect a real signed offline managed-input directory (not MSI/NSIS).
  Distribution(distribution::DistributionArgs),
  /// Inspect by default, or explicitly install the verified immutable distribution snapshot.
  InstallDistribution(distribution::InstallDistributionArgs),
  /// Plan or explicitly execute strict SHA256/RFC3161 Authenticode signing on copies.
  Sign(signing::SignArgs),
  /// Read-only ownership/retention report (also usable on portable test fixtures).
  Inspect {
    root: PathBuf,
    #[arg(long)]
    app_id: String,
  },
  /// Explicit external-process cleanup; active/rollback/pending/unknown versions stay protected.
  Prune {
    root: PathBuf,
    #[arg(long)]
    app_id: String,
    #[arg(long, default_value_t = 2)]
    keep_recent: usize,
    #[arg(long)]
    dry_run: bool,
  },
  /// Remove only this installation's authenticated versions/tools and still-owned protocols.
  /// Run from outside the managed root, after applications and launchers exit. Business data stays.
  Uninstall {
    root: PathBuf,
    #[arg(long)]
    app_id: String,
    #[arg(long)]
    dry_run: bool,
  },
  /// Inspect CEF143 PE/resource/helper layout without enabling an unverified bootstrap.
  InspectCef {
    #[arg(long)]
    cef_path: PathBuf,
    #[arg(long)]
    executable: PathBuf,
    #[arg(long)]
    helper: Option<PathBuf>,
    #[arg(long)]
    target: String,
    /// Verify actual official minimal archive bytes and extracted SDK SHA256 pins.
    /// This does not execute Windows code or enable packaging.
    #[arg(long)]
    archive: Option<PathBuf>,
  },
  /// Explicit current-user installation with a stable launcher and pinned update key.
  Install {
    bundle: PathBuf,
    /// New directory inside current-user LOCALAPPDATA; defaults to LintelApplications/<id>.
    #[arg(long)]
    root: Option<PathBuf>,
    #[arg(long)]
    public_key: PathBuf,
    /// Defaults to the installed sibling lintel-launcher.exe.
    #[arg(long)]
    launcher: Option<PathBuf>,
    /// Defaults to the installed sibling lintel-updater.exe.
    #[arg(long)]
    installer: Option<PathBuf>,
    /// Optional custom schemes; otherwise uses declarations in the portable bundle.
    #[arg(long = "scheme", value_parser = crate::parse_protocol_scheme)]
    schemes: Vec<String>,
    #[arg(long, default_value = "stable")]
    channel: String,
  },
}

impl WindowsArgs {
  pub(crate) fn run(self) -> Result<()> {
    match self.command {
      WindowsCommand::Distribution(args) => args.run()?,
      WindowsCommand::InstallDistribution(args) => args.run()?,
      WindowsCommand::Sign(args) => args.run()?,
      WindowsCommand::Inspect { root, app_id } => println!(
        "{}",
        lintel_updater::inspect_windows_installation(&root, &app_id)?.to_json()?
      ),
      WindowsCommand::Prune {
        root,
        app_id,
        keep_recent,
        dry_run,
      } => println!(
        "{}",
        lintel_updater::prune_windows_versions(&root, &app_id, keep_recent, dry_run)?.to_json()?
      ),
      WindowsCommand::Uninstall { root, app_id, dry_run } => println!(
        "{}",
        lintel_updater::uninstall_windows_app(&root, &app_id, dry_run)?.to_json()?
      ),
      WindowsCommand::InspectCef {
        cef_path,
        executable,
        helper,
        target,
        archive,
      } => {
        let assessment = match archive {
          Some(archive) => lintel_build::inspect_windows_cef_layout_with_archive(
            &cef_path,
            &executable,
            helper.as_deref(),
            &target,
            &archive,
          )?,
          None => lintel_build::inspect_windows_cef_layout(&cef_path, &executable, helper.as_deref(), &target)?,
        };
        println!("{}", assessment.to_json()?);
      }
      WindowsCommand::Install {
        bundle,
        root,
        public_key,
        launcher,
        installer,
        schemes,
        channel,
      } => {
        ensure!(cfg!(windows), "native per-user Windows installation requires Windows");
        let executable = std::env::current_exe()?;
        let siblings = executable.parent().context("lintel has no installation directory")?;
        let root = lintel_updater::install_windows_app(&lintel_updater::WindowsInstallOptions {
          bundle,
          install_root: root,
          public_key: std::fs::read_to_string(public_key)?,
          launcher: launcher.unwrap_or_else(|| siblings.join("lintel-launcher.exe")),
          installer: installer.unwrap_or_else(|| siblings.join("lintel-updater.exe")),
          protocol_schemes: schemes,
          channel,
        })?;
        println!("{}", root.display());
      }
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use crate::Cli;
  use clap::Parser;
  #[test]
  fn managed_windows_install_requires_explicit_pinned_key() {
    assert!(
      Cli::try_parse_from([
        "lintel",
        "windows",
        "install",
        "App-1.0.0-x86_64",
        "--public-key",
        "updater.pub",
        "--scheme",
        "example"
      ])
      .is_ok()
    );
    assert!(Cli::try_parse_from(["lintel", "windows", "install", "App"]).is_err());
  }
  #[test]
  fn maintenance_requires_explicit_identity_and_accepts_preview() {
    for action in ["inspect", "prune", "uninstall"] {
      assert!(
        Cli::try_parse_from([
          "lintel",
          "windows",
          action,
          "C:\\Application",
          "--app-id",
          "dev.example.app"
        ])
        .is_ok()
      );
      assert!(Cli::try_parse_from(["lintel", "windows", action, "C:\\Application"]).is_err());
    }
    assert!(
      Cli::try_parse_from([
        "lintel",
        "windows",
        "prune",
        "C:\\Application",
        "--app-id",
        "dev.example.app",
        "--keep-recent",
        "3",
        "--dry-run"
      ])
      .is_ok()
    );
  }
}
