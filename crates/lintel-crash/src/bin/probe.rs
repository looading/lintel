//! Isolated validation fixture. It aborts only when the explicit --abort switch is supplied.
use std::{io::Write, path::PathBuf};

fn main() -> anyhow::Result<()> {
  let args: Vec<_> = std::env::args_os().skip(1).collect();
  anyhow::ensure!(
    args.len() == 3,
    "usage: lintel-crash-probe <absolute-report-root> <absolute-helper> <--abort|--clean>"
  );
  let mode = args[2].to_str().unwrap_or_default();
  anyhow::ensure!(matches!(mode, "--abort" | "--clean"), "probe mode must be explicit");
  let mut config = lintel_crash::CrashConfig::new(PathBuf::from(&args[0]), PathBuf::from(&args[1]));
  config.dump_timeout = std::time::Duration::from_secs(5);
  let guard = lintel_crash::CrashReporter::start(config.clone())?;
  anyhow::ensure!(
    lintel_crash::CrashReporter::start(config).is_err(),
    "a second reporter must be rejected"
  );
  println!("report={} helper={}", guard.directory().display(), guard.helper_pid());
  std::io::stdout().flush()?;
  if mode == "--abort" {
    std::process::abort();
  }
  guard.shutdown()?;
  Ok(())
}
