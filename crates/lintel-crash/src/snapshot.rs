use std::{fmt, path::Path, time::Duration};

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod mac;

/// Snapshot outcomes that the caller can distinguish from helper and filesystem failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotError {
  Unsupported,
  TimedOut,
  Cancelled,
}

impl fmt::Display for SnapshotError {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(match self {
      Self::Unsupported => "native snapshots are supported on macOS arm64",
      Self::TimedOut => "native snapshot timed out",
      Self::Cancelled => "native snapshot was cancelled",
    })
  }
}

impl std::error::Error for SnapshotError {}

/// Captures the current process with the bundled `lintel-snapshot-helper` executable.
/// `output` must be a fresh path in a private staging directory owned by this application.
/// A successful call leaves a validated minidump at `output` for the caller to publish.
pub fn capture_snapshot(helper: &Path, output: &Path, timeout: Duration) -> anyhow::Result<()> {
  capture_snapshot_until(helper, output, timeout, || false)
}

/// Captures a snapshot with cooperative cancellation from an application-owned task.
/// On cancellation or timeout the helper is reaped and any partial output is removed.
pub fn capture_snapshot_until(
  helper: &Path,
  output: &Path,
  timeout: Duration,
  is_cancelled: impl Fn() -> bool,
) -> anyhow::Result<()> {
  #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
  {
    mac::capture(helper, output, timeout, is_cancelled)
  }
  #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
  {
    let _ = (helper, output, timeout, is_cancelled);
    Err(SnapshotError::Unsupported.into())
  }
}

#[doc(hidden)]
pub fn snapshot_helper_entry() -> anyhow::Result<()> {
  #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
  {
    mac::helper_entry()
  }
  #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
  {
    Err(SnapshotError::Unsupported.into())
  }
}
