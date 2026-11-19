//! Opt-in, provider-neutral tools. Execution stays in the application's managed task lifecycle.
//! Catalog schemas/effects are host declarations, not a JSON Schema engine or process sandbox.
mod progress;
mod registry;
mod session;

pub use progress::{ProgressError, ProgressEvent, ProgressStream};
pub use registry::{ToolCatalog, ToolEffect, ToolGrant, ToolPolicy, ToolRegistry, ToolSpec};
pub use serde;
pub use serde_json::{Value, json, to_vec};
pub use session::{Invocation, InvocationCancellation, ToolContext, ToolLimits, ToolSession};

/// Stable response codes. Input, serde paths, handler errors and panic payloads are never attached.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolErrorCode {
  InvalidSpec,
  DuplicateTool,
  RegistryFull,
  CatalogTooLarge,
  InvalidLimits,
  ToolNotFound,
  Denied,
  InvalidInput,
  InputTooLarge,
  InvalidOutput,
  OutputTooLarge,
  Busy,
  SessionClosed,
  ScopeClosed,
  Cancelled,
  Timeout,
  HandlerFailed,
  HandlerPanicked,
  Unavailable,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct ToolError {
  pub code: ToolErrorCode,
}
pub type ToolResult<T> = std::result::Result<T, ToolError>;
impl ToolError {
  pub const fn new(code: ToolErrorCode) -> Self {
    Self { code }
  }
  pub fn message(&self) -> &'static str {
    match self.code {
      ToolErrorCode::InvalidSpec => "tool specification is invalid",
      ToolErrorCode::DuplicateTool => "tool ID is already registered",
      ToolErrorCode::RegistryFull => "tool registry capacity reached",
      ToolErrorCode::CatalogTooLarge => "tool catalog size limit reached",
      ToolErrorCode::InvalidLimits => "tool limits are invalid",
      ToolErrorCode::ToolNotFound => "tool is not registered",
      ToolErrorCode::Denied => "tool is denied by session policy",
      ToolErrorCode::InvalidInput => "input failed typed or domain validation",
      ToolErrorCode::InputTooLarge => "input JSON exceeds the byte limit",
      ToolErrorCode::InvalidOutput => "output could not be encoded as bounded JSON",
      ToolErrorCode::OutputTooLarge => "output JSON exceeds the byte limit",
      ToolErrorCode::Busy => "session concurrency limit reached",
      ToolErrorCode::SessionClosed => "tool session is closed",
      ToolErrorCode::ScopeClosed => "tool session scope has ended",
      ToolErrorCode::Cancelled => "tool invocation was cancelled",
      ToolErrorCode::Timeout => "tool invocation exceeded its cooperative deadline",
      ToolErrorCode::HandlerFailed => "tool handler failed",
      ToolErrorCode::HandlerPanicked => "tool handler panicked",
      ToolErrorCode::Unavailable => "managed tool execution is unavailable",
    }
  }
}
impl std::fmt::Display for ToolError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.message())
  }
}
impl std::error::Error for ToolError {}

struct BoundedJson {
  bytes: Vec<u8>,
  limit: usize,
  exceeded: bool,
}
impl std::io::Write for BoundedJson {
  fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
    if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
      self.exceeded = true;
      return Err(std::io::Error::other("bounded JSON size limit"));
    }
    self.bytes.extend_from_slice(bytes);
    Ok(bytes.len())
  }
  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}
fn encode<T: serde::Serialize + ?Sized>(
  value: &T,
  limit: usize,
  overflow: ToolErrorCode,
  invalid: ToolErrorCode,
) -> ToolResult<Vec<u8>> {
  let mut writer = BoundedJson {
    bytes: Vec::new(),
    limit,
    exceeded: false,
  };
  if serde_json::to_writer(&mut writer, value).is_err() {
    return Err(ToolError::new(if writer.exceeded { overflow } else { invalid }));
  }
  Ok(writer.bytes)
}

#[cfg(test)]
mod tests;
