//! App-facing crash capture, report queries and upload policies.

use std::{
  path::PathBuf,
  sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
  },
  time::Duration,
};

use anyhow::ensure;
use lintel_core::plugin::{Plugin, PluginLifecycle};
use lintel_crash_reports::{ReportStore, UploaderProcess};
use tokio::sync::Semaphore;

use crate::{AppContext, TaskScope, runtime::PluginCreator};

pub use lintel_crash_reports::{
  FailureReason, PolicyStatus, QueueLimits, ReportError, ReportId, ReportKind, ReportQuery, ReportSource,
  ReportSourceConfig, ReportState, ReportStatus, ReportSummary, ReportsConfig, SourceKind, UploadMode, UploadPolicy,
  UploadReceipt, UploadTarget, UploaderStatus,
};

/// Selects the fatal capture owner for the application process.
#[derive(Clone, Debug, Default)]
pub enum FatalCapture {
  /// No automatic capture in the application process. Imported sources and manual snapshots remain available.
  #[default]
  Off,
  /// Uses Lintel's existing native crash helper.
  Native(lintel_crash::CrashConfig),
  /// Uses local Crashpad in a WebView bundle built with `--cef-crash-reports`.
  Cef(PathBuf),
}

/// Setup-time configuration for the crash report plugin.
pub struct CrashReportsConfig {
  pub reports: ReportsConfig,
  pub application_version: String,
  pub fatal_capture: FatalCapture,
  /// Absolute path to the optional `lintel-snapshot-helper` from the application bundle.
  pub snapshot_helper: Option<PathBuf>,
  /// Absolute path to `lintel-crash-uploader` when an upload target is configured.
  pub uploader: Option<PathBuf>,
  pub snapshot_timeout: Duration,
  pub post_exit_upload_grace: Duration,
}

impl CrashReportsConfig {
  /// Creates a local report service. Choose a fatal backend and enable manual snapshots or uploads as needed.
  pub fn new(
    application_id: impl Into<String>,
    application_version: impl Into<String>,
    directory: impl Into<PathBuf>,
  ) -> Self {
    Self {
      reports: ReportsConfig {
        directory: directory.into(),
        application_id: application_id.into(),
        sources: Vec::new(),
        limits: QueueLimits::default(),
        upload: None,
        policy: UploadPolicy::default(),
      },
      application_version: application_version.into(),
      fatal_capture: FatalCapture::Off,
      snapshot_helper: None,
      uploader: None,
      snapshot_timeout: Duration::from_secs(10),
      post_exit_upload_grace: Duration::from_secs(15),
    }
  }

  pub(crate) fn take_capture(&mut self) -> crate::Result<FatalCapture> {
    let mut capture = std::mem::take(&mut self.fatal_capture);
    let source = match &mut capture {
      FatalCapture::Off => return Ok(capture),
      FatalCapture::Native(config) => {
        config.application_id = self.reports.application_id.clone();
        config.version = self.application_version.clone();
        ReportSourceConfig {
          id: "native-host".into(),
          directory: config.directory.clone(),
          kind: SourceKind::Native,
        }
      }
      FatalCapture::Cef(directory) => ReportSourceConfig {
        id: "cef".into(),
        directory: directory.clone(),
        kind: SourceKind::Cef,
      },
    };
    ensure!(
      self
        .reports
        .sources
        .iter()
        .all(|configured| configured.id != source.id && configured.directory != source.directory),
      "the fatal capture backend already registers its crash report directory"
    );
    self.reports.sources.push(source);
    Ok(capture)
  }
}

#[derive(Debug, thiserror::Error)]
pub enum CrashReportsError {
  #[error("the crash report service is unavailable")]
  Unavailable,
  #[error("manual snapshots are disabled")]
  SnapshotDisabled,
  #[error("manual snapshots are unsupported on this platform")]
  Unsupported,
  #[error("another manual snapshot is in progress")]
  Busy,
  #[error("manual snapshot timed out")]
  TimedOut,
  #[error("manual snapshot was cancelled")]
  Cancelled,
  #[error("manual snapshot failed")]
  SnapshotFailed(#[source] anyhow::Error),
  #[error(transparent)]
  Reports(#[from] ReportError),
  #[error("the crash report background task stopped")]
  BackgroundTask(#[from] tokio::task::JoinError),
}

type Result<T> = std::result::Result<T, CrashReportsError>;

/// Typed application service. Obtain it through [`AppContext::get_service`] after plugin startup.
#[derive(Clone)]
pub struct CrashReports(Arc<Inner>);

struct Inner {
  store: ReportStore,
  application_version: String,
  snapshot_supported: bool,
  snapshot_helper: Option<PathBuf>,
  snapshot_timeout: Duration,
  snapshot_slot: Arc<Semaphore>,
  runtime: OnceLock<tokio::runtime::Handle>,
  active: AtomicBool,
  uploader_path: Option<PathBuf>,
  uploader_grace: Duration,
  uploader: Mutex<Option<UploaderProcess>>,
  uploader_start_failed: AtomicBool,
}

impl CrashReports {
  /// Captures a nonfatal snapshot of the current application process and registers the complete report.
  pub async fn capture_now(&self) -> Result<ReportSummary> {
    self.0.check_active()?;
    let runtime = self.0.runtime.get().ok_or(CrashReportsError::Unavailable)?;
    if !self.0.snapshot_supported {
      return Err(CrashReportsError::Unsupported);
    }
    let helper = self
      .0
      .snapshot_helper
      .clone()
      .ok_or(CrashReportsError::SnapshotDisabled)?;
    let slot = self
      .0
      .snapshot_slot
      .clone()
      .try_acquire_owned()
      .map_err(|_| CrashReportsError::Busy)?;
    let cancellation = SnapshotCancellation(Arc::new(AtomicBool::new(false)));
    let cancelled = cancellation.0.clone();
    let inner = self.0.clone();
    let result = runtime
      .spawn_blocking(move || {
        let _slot = slot;
        let is_cancelled = || cancelled.load(Ordering::Acquire) || !inner.active.load(Ordering::Acquire);
        let staging = inner
          .store
          .stage_snapshot_until(&is_cancelled)
          .map_err(snapshot_queue_error)?;
        let dump = staging.path().join("report.dmp");
        lintel_crash::capture_snapshot_until(&helper, &dump, inner.snapshot_timeout, &is_cancelled)
          .map_err(snapshot_error)?;
        if is_cancelled() {
          return Err(CrashReportsError::Cancelled);
        }
        let report = inner
          .store
          .register_snapshot_until(&dump, &inner.application_version, &is_cancelled)
          .map_err(snapshot_queue_error)?;
        inner.maintain_uploader();
        Ok(report)
      })
      .await?;
    drop(cancellation);
    result
  }

  /// Discovers reports from the application-configured sources and returns a bounded page.
  pub async fn list_reports(&self, query: ReportQuery) -> Result<Vec<ReportSummary>> {
    self
      .on_worker(move |inner| {
        inner.store.collect()?;
        Ok(inner.store.list_reports(query)?)
      })
      .await
  }

  pub async fn status(&self, id: ReportId) -> Result<ReportSummary> {
    self.on_worker(move |inner| Ok(inner.store.status(id)?)).await
  }

  /// Persists a send request. An uploaded status requires the receiver's matching receipt.
  pub async fn request_upload(&self, id: ReportId) -> Result<ReportSummary> {
    self
      .on_worker(move |inner| {
        let report = inner.store.request_upload(id)?;
        inner.maintain_uploader();
        Ok(report)
      })
      .await
  }

  /// Stores the new policy and waits briefly for any active uploader to apply it.
  pub async fn set_upload_policy(&self, policy: UploadPolicy) -> Result<PolicyStatus> {
    self
      .on_worker(move |inner| {
        inner.store.set_upload_policy(policy, Duration::from_secs(3))?;
        inner.maintain_uploader();
        inner.policy_status()
      })
      .await
  }

  pub async fn upload_policy(&self) -> Result<PolicyStatus> {
    self.on_worker(|inner| inner.policy_status()).await
  }

  async fn on_worker<T: Send + 'static>(
    &self,
    operation: impl FnOnce(&Inner) -> Result<T> + Send + 'static,
  ) -> Result<T> {
    self.0.check_active()?;
    let runtime = self.0.runtime.get().ok_or(CrashReportsError::Unavailable)?;
    let inner = self.0.clone();
    runtime
      .spawn_blocking(move || {
        inner.check_active()?;
        operation(&inner)
      })
      .await?
  }
}

struct SnapshotCancellation(Arc<AtomicBool>);

impl Drop for SnapshotCancellation {
  fn drop(&mut self) {
    self.0.store(true, Ordering::Release);
  }
}

fn snapshot_error(error: anyhow::Error) -> CrashReportsError {
  match error.downcast_ref::<lintel_crash::SnapshotError>() {
    Some(lintel_crash::SnapshotError::Unsupported) => CrashReportsError::Unsupported,
    Some(lintel_crash::SnapshotError::TimedOut) => CrashReportsError::TimedOut,
    Some(lintel_crash::SnapshotError::Cancelled) => CrashReportsError::Cancelled,
    _ => CrashReportsError::SnapshotFailed(error),
  }
}

fn snapshot_queue_error(error: ReportError) -> CrashReportsError {
  match error {
    ReportError::Cancelled => CrashReportsError::Cancelled,
    error => error.into(),
  }
}

impl Inner {
  fn check_active(&self) -> Result<()> {
    if self.active.load(Ordering::Acquire) {
      Ok(())
    } else {
      Err(CrashReportsError::Unavailable)
    }
  }

  fn policy_status(&self) -> Result<PolicyStatus> {
    let mut status = self.store.upload_policy()?;
    if self.uploader_start_failed.load(Ordering::Acquire) && status.uploader == UploaderStatus::Stopped {
      status.uploader = UploaderStatus::Unavailable;
    }
    Ok(status)
  }

  fn maintain_uploader(&self) {
    let Ok(mut child) = self.uploader.lock() else { return };
    if !self.active.load(Ordering::Acquire) {
      return;
    }
    let should_start = self.store.should_start_uploader();
    if let Some(process) = child.as_mut() {
      if process.is_running() {
        if matches!(should_start, Ok(false)) {
          process.finish();
          child.take();
        } else {
          process.wake();
        }
        return;
      }
      child.take();
    }
    match should_start {
      Ok(true) => {
        let process = self
          .uploader_path
          .as_deref()
          .ok_or(ReportError::UploaderUnavailable)
          .and_then(|path| self.store.spawn_uploader(path, self.uploader_grace));
        match process {
          Ok(process) => {
            *child = Some(process);
            self.uploader_start_failed.store(false, Ordering::Release);
          }
          Err(_) => self.uploader_start_failed.store(true, Ordering::Release),
        }
      }
      Ok(false) => self.uploader_start_failed.store(false, Ordering::Release),
      Err(_) => self.uploader_start_failed.store(true, Ordering::Release),
    }
  }
}

struct CrashReportsPlugin(CrashReports);

impl Plugin for CrashReportsPlugin {
  fn name(&self) -> &str {
    "lintel.crash-reports"
  }
}

impl PluginLifecycle for CrashReportsPlugin {
  fn on_start(&self, context: &AppContext) -> crate::Result<()> {
    self
      .0
      .0
      .runtime
      .set(context.runtime.clone())
      .map_err(|_| CrashReportsError::Unavailable)?;
    let service = Arc::new(self.0.clone());
    context.register_service(move || service.clone());
    let inner = self.0.0.clone();
    context.spawn_task(TaskScope::Application, async move {
      loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let inner = inner.clone();
        if tokio::task::spawn_blocking(move || inner.maintain_uploader())
          .await
          .is_err()
        {
          break;
        }
      }
    })?;
    Ok(())
  }

  fn on_stop(&self, context: &AppContext) {
    let inner = &self.0.0;
    inner.active.store(false, Ordering::Release);
    if let Ok(mut child) = inner.uploader.lock()
      && let Some(mut process) = child.take()
    {
      process.finish();
    }
    context.remove_service::<CrashReports>();
  }
}

pub(crate) fn configure_native(
  legacy: Option<lintel_crash::CrashConfig>,
  mut reports: Option<CrashReportsConfig>,
) -> crate::Result<(Option<lintel_crash::CrashConfig>, Option<CrashReportsConfig>)> {
  let Some(config) = reports.as_mut() else {
    return Ok((legacy, None));
  };
  ensure!(
    legacy.is_none(),
    "choose the native capture backend in CrashReportsConfig"
  );
  let crash = match config.take_capture()? {
    FatalCapture::Off => None,
    FatalCapture::Native(config) => Some(config),
    FatalCapture::Cef(_) => anyhow::bail!("CEF crash capture requires the WebView builder"),
  };
  Ok((crash, reports))
}

pub(crate) fn install(config: CrashReportsConfig, native_app: bool) -> crate::Result<PluginCreator> {
  ensure!(
    !config.application_version.is_empty() && config.application_version.len() <= 128,
    "invalid crash report application version"
  );
  ensure!(
    config.snapshot_helper.as_ref().is_none_or(|path| path.is_absolute())
      && config.uploader.as_ref().is_none_or(|path| path.is_absolute()),
    "crash report helper paths must be absolute"
  );
  ensure!(
    !config.snapshot_timeout.is_zero()
      && config.snapshot_timeout <= Duration::from_secs(120)
      && config.post_exit_upload_grace <= Duration::from_secs(120),
    "invalid crash report helper timeout"
  );
  ensure!(
    config.reports.upload.is_none() || config.uploader.is_some(),
    "an upload target requires a bundled crash uploader"
  );
  let inner = Arc::new(Inner {
    store: ReportStore::open(config.reports)?,
    application_version: config.application_version,
    snapshot_supported: native_app && cfg!(all(target_os = "macos", target_arch = "aarch64")),
    snapshot_helper: config.snapshot_helper,
    snapshot_timeout: config.snapshot_timeout,
    snapshot_slot: Arc::new(Semaphore::new(1)),
    runtime: OnceLock::new(),
    active: AtomicBool::new(true),
    uploader_path: config.uploader,
    uploader_grace: config.post_exit_upload_grace,
    uploader: Mutex::new(None),
    uploader_start_failed: AtomicBool::new(false),
  });
  inner.maintain_uploader();
  Ok(Box::new(move |context| {
    context.register_plugin(Arc::new(CrashReportsPlugin(CrashReports(inner))));
  }))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn plugin_native_capture_registers_source_and_application_release() {
    let root = std::env::temp_dir();
    let mut config = CrashReportsConfig::new("dev.example.crash", "2.3.4", root.join("queue"));
    let directory = root.join("native");
    config.fatal_capture = FatalCapture::Native(lintel_crash::CrashConfig::new(&directory, root.join("helper")));

    let (crash, config) = configure_native(None, Some(config)).unwrap();
    let crash = crash.unwrap();
    assert_eq!(crash.application_id, "dev.example.crash");
    assert_eq!(crash.version, "2.3.4");
    let sources = config.unwrap().reports.sources;
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].directory, directory);
    assert!(matches!(sources[0].kind, SourceKind::Native));
  }

  #[test]
  fn plugin_off_keeps_historical_sources_without_installing_capture() {
    let root = std::env::temp_dir();
    let mut config = CrashReportsConfig::new("dev.example.crash", "2.3.4", root.join("queue"));
    config.reports.sources.push(ReportSourceConfig {
      id: "existing".into(),
      directory: root.join("existing"),
      kind: SourceKind::Native,
    });

    let (capture, config) = configure_native(None, Some(config)).unwrap();
    assert!(capture.is_none());
    assert_eq!(config.unwrap().reports.sources.len(), 1);
  }

  #[test]
  fn legacy_capture_remains_available_and_conflicting_config_fails() {
    let root = std::env::temp_dir();
    let legacy = lintel_crash::CrashConfig::new(root.join("native"), root.join("helper"));
    assert!(configure_native(Some(legacy.clone()), None).unwrap().0.is_some());
    let config = CrashReportsConfig::new("dev.example.crash", "2.3.4", root.join("queue"));
    assert!(configure_native(Some(legacy), Some(config)).is_err());
  }
}
