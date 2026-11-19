use anyhow::Result;
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Args)]
pub(crate) struct UpdaterArgs {
  #[command(subcommand)]
  command: UpdaterCommand,
}

#[derive(Subcommand)]
enum UpdaterCommand {
  /// Generate a private release key and its public verification key without overwriting files.
  Keygen {
    #[arg(long)]
    private_key: PathBuf,
    #[arg(long)]
    public_key: PathBuf,
  },
  /// Create a complete signed application archive and an authenticated update manifest.
  Release {
    /// Completed, signed application bundle (staple before generating release archives).
    bundle: PathBuf,
    #[arg(long)]
    url: String,
    #[arg(long)]
    private_key: PathBuf,
    /// New directory to receive update.tar.gz and latest.json.
    #[arg(long)]
    output_dir: PathBuf,
    #[arg(long, default_value = "stable")]
    channel: String,
    #[arg(long, default_value = "")]
    notes: String,
    #[arg(long, default_value_t=604800, value_parser=clap::value_parser!(u64).range(1..))]
    expires_in: u64,
    /// Target platform; must be present in the signed app's architectures.
    #[arg(long)]
    target: Option<String>,
  },
}

impl UpdaterArgs {
  pub(crate) fn run(self) -> Result<()> {
    match self.command {
      UpdaterCommand::Keygen {
        private_key,
        public_key,
      } => {
        lintel_updater::generate_keypair(&private_key, &public_key)?;
        println!("{}", public_key.display());
      }
      UpdaterCommand::Release {
        bundle,
        url,
        private_key,
        output_dir,
        channel,
        notes,
        expires_in,
        target,
      } => {
        let result = lintel_updater::publish(&lintel_updater::ReleaseOptions {
          bundle,
          archive_url: url,
          channel,
          notes,
          expires_in,
          private_key,
          output_dir,
          target,
        })?;
        println!("{}", result.display());
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
  fn updater_bundle_requires_an_explicit_signing_identity() {
    assert!(Cli::try_parse_from(["lintel", "bundle", "--updater"]).is_err());
    assert!(Cli::try_parse_from(["lintel", "bundle", "--updater", "--sign", "-"]).is_ok());
  }

  #[test]
  fn release_commands_require_keys_and_a_positive_lifetime() {
    assert!(Cli::try_parse_from(["lintel", "updater", "keygen", "--private-key", "key"]).is_err());
    assert!(
      Cli::try_parse_from([
        "lintel",
        "updater",
        "keygen",
        "--private-key",
        "key",
        "--public-key",
        "public"
      ])
      .is_ok()
    );
    assert!(
      Cli::try_parse_from([
        "lintel",
        "updater",
        "release",
        "App.app",
        "--private-key",
        "key",
        "--url",
        "https://example.test/update.tar.gz",
        "--output-dir",
        "dist",
        "--expires-in",
        "0"
      ])
      .is_err()
    );
  }
}
