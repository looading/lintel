//! Application-owned crash report queue and independently supervised HTTPS delivery.

mod importer;
mod store;
mod uploader;

#[cfg(test)]
mod test_data;

use std::{fmt, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use store::{ReportStore, SnapshotStaging};
pub use uploader::{UploaderProcess, run_uploader};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReportsConfig {
  pub directory: PathBuf,
  pub application_id: String,
  pub sources: Vec<ReportSourceConfig>,
  pub limits: QueueLimits,
  pub upload: Option<UploadTarget>,
  /// Initial policy for a new queue or receiver. `Disabled` also revokes a saved policy on startup.
  pub policy: UploadPolicy,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReportSourceConfig {
  pub id: String,
  pub directory: PathBuf,
  pub kind: SourceKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
  Native,
  Cef,
  MojoService,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum ReportSource {
  Native,
  Cef,
  MojoService { service_id: String, attempt: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
  Fatal,
  ManualSnapshot,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ReportId(Uuid);

impl fmt::Display for ReportId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    self.0.fmt(f)
  }
}

impl std::str::FromStr for ReportId {
  type Err = uuid::Error;

  fn from_str(value: &str) -> Result<Self, Self::Err> {
    value.parse().map(Self)
  }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UploadReceipt {
  pub schema_version: u32,
  pub report_id: ReportId,
  pub receipt_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureReason {
  FileTooLarge,
  QueueFull,
  InvalidDump,
  Authentication,
  ReceiptProtocol,
  ContentConflict,
  HttpRejected,
  Redirect,
  SecureConnection,
  AttemptsExhausted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum ReportStatus {
  Local,
  Queued,
  Uploading,
  Uploaded(UploadReceipt),
  Failed(FailureReason),
  Expired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportState {
  Local,
  Queued,
  Uploading,
  Uploaded,
  Failed,
  Expired,
}

impl ReportStatus {
  pub fn state(&self) -> ReportState {
    match self {
      Self::Local => ReportState::Local,
      Self::Queued => ReportState::Queued,
      Self::Uploading => ReportState::Uploading,
      Self::Uploaded(_) => ReportState::Uploaded,
      Self::Failed(_) => ReportState::Failed,
      Self::Expired => ReportState::Expired,
    }
  }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReportSummary {
  pub id: ReportId,
  pub source_id: String,
  pub source: ReportSource,
  pub kind: ReportKind,
  pub captured_at_unix_ms: Option<u64>,
  pub application_version: Option<String>,
  pub size_bytes: u64,
  pub status: ReportStatus,
}

#[derive(Clone, Debug)]
pub struct ReportQuery {
  pub source_id: Option<String>,
  pub kind: Option<ReportKind>,
  pub status: Option<ReportState>,
  pub offset: u32,
  pub limit: u32,
}

impl Default for ReportQuery {
  fn default() -> Self {
    Self {
      source_id: None,
      kind: None,
      status: None,
      offset: 0,
      limit: 50,
    }
  }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadMode {
  Disabled,
  #[default]
  ManualOnly,
  Automatic,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct UploadPolicy {
  pub mode: UploadMode,
  pub include_manual_snapshots: bool,
  pub include_existing: bool,
  /// Stable authorized source IDs. An empty list selects all configured sources.
  pub sources: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploaderStatus {
  Stopped,
  Running,
  Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PolicyStatus {
  pub policy: UploadPolicy,
  pub version: u64,
  pub applied_version: Option<u64>,
  pub uploader: UploaderStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueueLimits {
  pub max_report_bytes: u64,
  pub max_total_bytes: u64,
  pub retention: Duration,
  pub max_attempts: u32,
  pub request_timeout: Duration,
  pub max_retry_delay: Duration,
}

impl Default for QueueLimits {
  fn default() -> Self {
    Self {
      max_report_bytes: 64 * 1024 * 1024,
      max_total_bytes: 256 * 1024 * 1024,
      retention: Duration::from_secs(7 * 24 * 60 * 60),
      max_attempts: 8,
      request_timeout: Duration::from_secs(10),
      max_retry_delay: Duration::from_secs(60 * 60),
    }
  }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct UploadTarget {
  url: String,
  bearer_token: Option<String>,
  credential_generation: u64,
}

impl UploadTarget {
  pub fn new(https_url: impl Into<String>) -> Self {
    Self {
      url: https_url.into(),
      bearer_token: None,
      credential_generation: 0,
    }
  }

  /// Advance the generation whenever the receiver's bearer token changes.
  pub fn with_bearer_token(mut self, token: impl Into<String>, generation: u64) -> Self {
    self.bearer_token = Some(token.into());
    self.credential_generation = generation;
    self
  }
}

impl fmt::Debug for UploadTarget {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("UploadTarget")
      .field("credential_generation", &self.credential_generation)
      .finish_non_exhaustive()
  }
}

#[derive(Debug, thiserror::Error)]
pub enum ReportError {
  #[error("invalid crash report configuration")]
  InvalidConfiguration,
  #[error("the crash report queue configuration was replaced by another process")]
  StaleConfiguration,
  #[error("crash report registration was cancelled")]
  Cancelled,
  #[error("crash report uploads are disabled")]
  Disabled,
  #[error("no crash report upload target is configured")]
  MissingTarget,
  #[error("the crash report does not exist")]
  NotFound,
  #[error("the crash report is no longer available for upload")]
  Expired,
  #[error("the crash report has a permanent upload failure")]
  PermanentFailure,
  #[error("the crash report is incomplete, invalid, or outside its authorized directory")]
  InvalidReport,
  #[error("the crash report exceeds its size limit")]
  FileTooLarge,
  #[error("the crash report queue is full")]
  QuotaExceeded,
  #[error("the crash report uploader is unavailable")]
  UploaderUnavailable,
  #[error("the crash report uploader did not acknowledge the policy before the deadline")]
  PolicyTimeout,
  #[error("this crash report operation is unsupported on the current platform")]
  Unsupported,
  #[error("crash report storage I/O failed")]
  Io(#[from] std::io::Error),
  #[error("crash report queue database operation failed")]
  Database(#[from] rusqlite::Error),
  #[error("crash report queue data is invalid")]
  Data(#[from] serde_json::Error),
}
