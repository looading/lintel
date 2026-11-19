//! Explicit local export of logs and native/CEF crash dumps; never uploads reports.
#[cfg(feature = "services")]
fn main() -> lintel::Result<()> {
  use lintel::services::{DiagnosticConfig, Diagnostics};
  let arguments: Vec<_> = std::env::args_os().skip(1).collect();
  anyhow::ensure!(
    arguments.len() == 2,
    "usage: diagnostic_export CRASH_DIRECTORY NEW_OUTPUT_DIRECTORY"
  );
  let logs = tempfile::tempdir()?;
  let diagnostics = Diagnostics::new(
    DiagnosticConfig {
      crash_directory: Some(arguments[0].clone().into()),
      install_tracing: false,
      capture_panics: false,
      ..Default::default()
    },
    &logs.path().join("logs"),
  )?;
  diagnostics.record("export", serde_json::json!({ "source": "explicit-local-example" }))?;
  let output = diagnostics.export(std::path::Path::new(&arguments[1]))?;
  println!("LINTEL_DIAGNOSTIC_EXPORT_OK {}", output.display());
  Ok(())
}

#[cfg(not(feature = "services"))]
fn main() -> lintel::Result<()> {
  anyhow::bail!("enable the services feature")
}
