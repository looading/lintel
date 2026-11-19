use std::time::Duration;

/// An integration failure; unavailable features never report success.
#[derive(Debug, thiserror::Error)]
pub enum DesktopError {
  #[error("{capability} is unavailable on {platform}: {reason}")]
  Unsupported {
    capability: &'static str,
    platform: &'static str,
    reason: &'static str,
  },
  #[error("native UI must be initialized on the process main thread")]
  MainThreadRequired,
  #[error("waiting for a desktop command on its dispatch thread would deadlock; use try_recv")]
  WouldBlockMainThread,
  #[error("the desktop runtime has stopped")]
  RuntimeClosed,
  #[error("the desktop command queue is full")]
  QueueFull,
  #[error("desktop command did not finish within {0:?}")]
  Timeout(Duration),
  #[error("invalid {field}: {reason}")]
  InvalidInput { field: &'static str, reason: String },
  #[error("unknown {kind} handle {id}")]
  UnknownHandle { kind: &'static str, id: u64 },
  #[error("{operation} failed: {message}")]
  Native { operation: &'static str, message: String },
  #[error("{operation} portal response {response} (1 = user canceled, 2 = failed)")]
  PortalResponse { operation: &'static str, response: u32 },
  #[error(transparent)]
  Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, DesktopError>;

pub(crate) fn unsupported(capability: &'static str, reason: &'static str) -> DesktopError {
  DesktopError::Unsupported {
    capability,
    platform: std::env::consts::OS,
    reason,
  }
}

pub(crate) fn invalid(field: &'static str, reason: impl Into<String>) -> DesktopError {
  DesktopError::InvalidInput {
    field,
    reason: reason.into(),
  }
}

#[allow(dead_code)]
pub(crate) fn native(operation: &'static str, error: impl std::fmt::Display) -> DesktopError {
  DesktopError::Native {
    operation,
    message: error.to_string(),
  }
}
