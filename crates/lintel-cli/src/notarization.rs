//! Apple notarization commands use references to credentials stored in Keychain.

use std::path::PathBuf;

use anyhow::{Result, ensure};
use clap::{Args, Subcommand};
use lintel_build::{
  NotarizeOptions, NotaryCredentials, NotarySubmission, notarization_info, notarization_log, notarize_bundle,
  submit_notarization, wait_for_notarization,
};

const DEFAULT_TIMEOUT: u64 = 600;

#[derive(Args)]
struct CredentialsArgs {
  /// Existing notarytool credentials profile in macOS Keychain.
  #[arg(long, value_name = "PROFILE")]
  keychain_profile: String,
  /// Keychain containing the profile; defaults to the user's search list.
  #[arg(long)]
  keychain: Option<PathBuf>,
}

impl CredentialsArgs {
  fn into_credentials(self) -> Result<NotaryCredentials> {
    validate_profile(&self.keychain_profile)?;
    Ok(NotaryCredentials {
      keychain_profile: self.keychain_profile,
      keychain: self.keychain,
    })
  }
}

#[derive(Args)]
pub(crate) struct NotarizeArgs {
  /// Developer ID signed macOS application to upload to Apple.
  bundle: PathBuf,
  #[command(flatten)]
  credentials: CredentialsArgs,
  /// Submit only and return its ID; use notarization info/wait/log to resume.
  #[arg(long)]
  no_wait: bool,
  /// Maximum polling time in seconds (default 600). A timeout leaves the remote job running.
  #[arg(long = "timeout", value_parser = clap::value_parser!(u64).range(1..), conflicts_with = "no_wait")]
  timeout_seconds: Option<u64>,
}

impl NotarizeArgs {
  pub(crate) fn run(self) -> Result<()> {
    let credentials = self.credentials.into_credentials()?;
    let submission = if self.no_wait {
      submit_notarization(&self.bundle, &credentials)?
    } else {
      notarize_bundle(
        &self.bundle,
        &NotarizeOptions {
          credentials,
          timeout_seconds: self.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT),
        },
      )?
    };
    print_submission(&submission);
    Ok(())
  }
}

#[derive(Args)]
pub(crate) struct NotarizationArgs {
  #[command(subcommand)]
  command: NotarizationCommand,
}

#[derive(Subcommand)]
enum NotarizationCommand {
  /// Query a submission's current processing status.
  Info(JobArgs),
  /// Wait for a submission and succeed only if Apple accepts it. Does not staple an app.
  Wait(WaitArgs),
  /// Print Apple's original JSON diagnostic log, including warnings and rejection details.
  Log(JobArgs),
}

#[derive(Args)]
struct JobArgs {
  /// Submission UUID returned by Apple.
  id: String,
  #[command(flatten)]
  credentials: CredentialsArgs,
}

#[derive(Args)]
struct WaitArgs {
  #[command(flatten)]
  job: JobArgs,
  /// Maximum polling time in seconds; timeout does not cancel the submission.
  #[arg(long = "timeout", default_value_t = DEFAULT_TIMEOUT, value_parser = clap::value_parser!(u64).range(1..))]
  timeout_seconds: u64,
}

impl NotarizationArgs {
  pub(crate) fn run(self) -> Result<()> {
    match self.command {
      NotarizationCommand::Info(job) => {
        let submission = notarization_info(&job.id, &job.credentials.into_credentials()?)?;
        print_submission(&submission);
      }
      NotarizationCommand::Wait(args) => {
        let submission = wait_for_notarization(
          &args.job.id,
          &NotarizeOptions {
            credentials: args.job.credentials.into_credentials()?,
            timeout_seconds: args.timeout_seconds,
          },
        )?;
        print_submission(&submission);
      }
      NotarizationCommand::Log(job) => {
        println!("{}", notarization_log(&job.id, &job.credentials.into_credentials()?)?);
      }
    }
    Ok(())
  }
}

#[derive(Args, Default)]
pub(crate) struct BundleNotarization {
  /// Notarize and staple before publishing, using this existing Keychain profile.
  /// Requires --sign and automatically enables Hardened Runtime and secure timestamps.
  #[arg(long = "notarize", value_name = "PROFILE", requires = "sign")]
  profile: Option<String>,
  /// Keychain containing the notarization profile.
  #[arg(long, requires = "profile")]
  notary_keychain: Option<PathBuf>,
  /// Maximum notarization polling time in seconds (default 600).
  #[arg(long, value_parser = clap::value_parser!(u64).range(1..), requires = "profile")]
  notary_timeout: Option<u64>,
}

impl BundleNotarization {
  pub(crate) fn options(&self) -> Result<Option<NotarizeOptions>> {
    self
      .profile
      .as_ref()
      .map(|profile| {
        validate_profile(profile)?;
        if let Some(keychain) = &self.notary_keychain {
          ensure!(
            keychain.is_file(),
            "Keychain path is not a file: {}",
            keychain.display()
          );
        }
        Ok(NotarizeOptions {
          credentials: NotaryCredentials {
            keychain_profile: profile.clone(),
            keychain: self.notary_keychain.clone(),
          },
          timeout_seconds: self.notary_timeout.unwrap_or(DEFAULT_TIMEOUT),
        })
      })
      .transpose()
  }
}

fn validate_profile(profile: &str) -> Result<()> {
  ensure!(
    !profile.trim().is_empty() && !profile.chars().any(char::is_control),
    "provide a nonempty notarytool Keychain profile name"
  );
  Ok(())
}

fn print_submission(submission: &NotarySubmission) {
  println!("{}\t{}", submission.id, submission.status);
}

#[cfg(test)]
mod tests {
  use clap::{CommandFactory, Parser};

  use crate::{Cli, Command};

  #[test]
  fn command_tree_is_consistent() {
    Cli::command().debug_assert();
    assert!(
      Cli::try_parse_from([
        "lintel",
        "bundle",
        "--sign",
        "Developer ID Application: Fixture (TEAM)",
        "--notarize",
        "release",
        "--notary-keychain",
        "/tmp/fixture.keychain-db",
        "--notary-timeout",
        "45"
      ])
      .is_ok()
    );
    assert!(Cli::try_parse_from(["lintel", "bundle", "--notary-keychain", "/tmp/fixture.keychain-db"]).is_err());
  }

  #[test]
  fn notarization_requires_profile_and_positive_timeout() {
    assert!(Cli::try_parse_from(["lintel", "notarize", "Fixture.app"]).is_err());
    assert!(
      Cli::try_parse_from([
        "lintel",
        "notarize",
        "Fixture.app",
        "--keychain-profile",
        "release",
        "--timeout",
        "0"
      ])
      .is_err()
    );
    assert!(
      Cli::try_parse_from([
        "lintel",
        "notarize",
        "Fixture.app",
        "--keychain-profile",
        "release",
        "--no-wait",
        "--timeout",
        "60"
      ])
      .is_err()
    );
  }

  #[test]
  fn bundle_notarization_enables_distribution_signing_and_rejects_adhoc() {
    for identity in ["Developer ID Application: Fixture (TEAM)", "-"] {
      let cli = Cli::try_parse_from([
        "lintel",
        "bundle",
        "--sign",
        identity,
        "--notarize",
        "release",
        "--notary-timeout",
        "45",
      ])
      .unwrap();
      let Command::Bundle(args) = cli.command else {
        panic!("expected bundle command");
      };
      let notary = args.notarization.options().unwrap().unwrap();
      assert_eq!(notary.timeout_seconds, 45);
      let signing = args.bundle.distribution_signing_options(true);
      if identity == "-" {
        assert!(signing.is_err());
      } else {
        let signing = signing.unwrap().unwrap();
        assert!(signing.timestamp && signing.hardened_runtime);
      }
    }
    assert!(Cli::try_parse_from(["lintel", "bundle", "--notarize", "release"]).is_err());
    assert!(Cli::try_parse_from(["lintel", "bundle", "--notary-timeout", "45"]).is_err());
  }

  #[test]
  fn submission_only_and_resume_commands_parse() {
    assert!(
      Cli::try_parse_from([
        "lintel",
        "notarize",
        "Fixture.app",
        "--keychain-profile",
        "release",
        "--no-wait"
      ])
      .is_ok()
    );
    for command in ["info", "wait", "log"] {
      assert!(
        Cli::try_parse_from([
          "lintel",
          "notarization",
          command,
          "d0701e18-2090-4a66-827c-e1e7a803184f",
          "--keychain-profile",
          "release"
        ])
        .is_ok()
      );
    }
    for command in ["staple", "verify-notarization"] {
      assert!(Cli::try_parse_from(["lintel", command, "Fixture.app"]).is_ok());
    }
  }
}
