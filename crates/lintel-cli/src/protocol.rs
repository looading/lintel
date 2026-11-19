use std::path::PathBuf;

use anyhow::{Result, ensure};
use clap::{Args, Subcommand};
use lintel_build::{WindowsProtocolRegistration, register_windows_protocols, unregister_windows_protocols};

#[derive(Args)]
pub(crate) struct ProtocolArgs {
  #[command(subcommand)]
  command: ProtocolCommand,
}

#[derive(Subcommand)]
enum ProtocolCommand {
  /// Register explicit custom schemes for this user; preserve foreign associations.
  Register {
    #[arg(long = "id")]
    app_id: String,
    /// Stable launcher accepting --lintel-protocol URL.
    #[arg(long)]
    executable: PathBuf,
    #[arg(long = "scheme", required = true, value_parser = crate::parse_protocol_scheme)]
    schemes: Vec<String>,
    /// Ownership receipt saved before the atomic registry transaction; keep it for removal.
    #[arg(long)]
    receipt: PathBuf,
  },
  /// Remove only the exact, unchanged HKCU associations authorized by this receipt.
  Unregister {
    #[arg(long)]
    receipt: PathBuf,
  },
}

impl ProtocolArgs {
  pub(crate) fn run(self) -> Result<()> {
    match self.command {
      ProtocolCommand::Register {
        app_id,
        executable,
        schemes,
        receipt,
      } => {
        let registration = if receipt.try_exists()? {
          let registration = WindowsProtocolRegistration::read_receipt(&receipt)?;
          ensure!(
            registration.app_id() == app_id
              && registration.schemes() == schemes
              && registration.executable() == executable.canonicalize()?,
            "existing receipt belongs to a different registration"
          );
          registration
        } else {
          let registration = WindowsProtocolRegistration::prepare(&app_id, &executable, &schemes)?;
          registration.write_receipt(&receipt)?;
          registration
        };
        register_windows_protocols(&registration)?;
        println!("{}", receipt.display());
      }
      ProtocolCommand::Unregister { receipt } => {
        unregister_windows_protocols(&WindowsProtocolRegistration::read_receipt(&receipt)?)?;
        println!(
          "removed owned protocol associations; receipt retained at {}",
          receipt.display()
        );
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
  fn protocol_registration_requires_explicit_identity_schemes_and_receipt() {
    assert!(
      Cli::try_parse_from([
        "lintel",
        "protocol",
        "register",
        "--id",
        "dev.example.app",
        "--executable",
        "app.exe",
        "--scheme",
        "example",
        "--receipt",
        "ownership.json"
      ])
      .is_ok()
    );
    assert!(
      Cli::try_parse_from([
        "lintel",
        "protocol",
        "register",
        "--id",
        "dev.example.app",
        "--executable",
        "app.exe"
      ])
      .is_err()
    );
    assert!(Cli::try_parse_from(["lintel", "protocol", "unregister", "--receipt", "ownership.json"]).is_ok());
  }
}
