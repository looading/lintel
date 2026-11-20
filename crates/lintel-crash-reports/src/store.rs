use std::{
  collections::HashSet,
  fs::{self, File, OpenOptions, TryLockError},
  io::{self, Read, Seek, Write},
  path::{Path, PathBuf},
  time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, TransactionBehavior, named_params, params};
use sha2::{Digest, Sha256};

use crate::{
  FailureReason, PolicyStatus, ReportError, ReportId, ReportKind, ReportQuery, ReportState, ReportStatus,
  ReportSummary, ReportsConfig, UploadMode, UploadPolicy, UploadReceipt, UploaderStatus,
  importer::{self, Candidate, Manifest, SNAPSHOT_SOURCE_ID, UploadMetadata},
};

const IMPORT_LOCK: &str = "import.lock";
const SEND_LOCK: &str = "send.lock";
const SNAPSHOT_LEASE: &str = "capture.lock";
const MAX_PAGE: u32 = 500;
const MAX_CONFIGURATION_BYTES: usize = 64 * 1024;
const MAX_POLICY_WAIT: Duration = Duration::from_secs(30);
const SCHEMA_VERSION: u32 = 1;

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS settings (
        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
        application_id TEXT NOT NULL,
        revision TEXT NOT NULL,
        target_id TEXT,
        policy TEXT NOT NULL,
        version INTEGER NOT NULL,
        effective_at INTEGER NOT NULL,
        applied_version INTEGER
    );
    CREATE TABLE IF NOT EXISTS reports (
        id TEXT PRIMARY KEY,
        source_id TEXT NOT NULL,
        backend_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        captured_at INTEGER,
        retention_time INTEGER NOT NULL,
        state TEXT NOT NULL,
        size INTEGER NOT NULL,
        failure TEXT,
        metadata TEXT NOT NULL,
        UNIQUE (source_id, backend_id)
    );
    CREATE TABLE IF NOT EXISTS uploads (
        report_id TEXT NOT NULL REFERENCES reports(id) ON DELETE CASCADE,
        target_id TEXT NOT NULL,
        manual INTEGER NOT NULL,
        state TEXT NOT NULL,
        attempts INTEGER NOT NULL DEFAULT 0,
        next_at INTEGER NOT NULL DEFAULT 0,
        result TEXT,
        PRIMARY KEY (report_id, target_id)
    );
    CREATE INDEX IF NOT EXISTS uploads_due ON uploads(target_id, state, next_at);
    CREATE INDEX IF NOT EXISTS reports_retention ON reports(retention_time);
";

const DISPLAY_STATE: &str = "CASE
    WHEN u.state = 'uploaded' THEN 'uploaded'
    WHEN r.state = 'expired' OR (r.retention_time < :cutoff AND u.state IS NOT 'uploading') THEN 'expired'
    WHEN r.state = 'failed' THEN 'failed'
    ELSE coalesce(u.state, 'local') END";

// Reused for automatic registration and claiming. The caller binds the authorized source IDs.
const AUTOMATIC_ELIGIBILITY: &str = "
    :automatic AND r.source_id IN (SELECT value FROM json_each(:sources))
    AND (:snapshots OR r.kind = 'fatal')
    AND (:historical OR r.captured_at > :effective_at)
";

#[derive(Clone)]
pub struct ReportStore {
  pub(crate) config: ReportsConfig,
  revision: String,
  target: Option<String>,
}

/// A private per-request directory kept alive while the snapshot helper and registration run.
pub struct SnapshotStaging {
  _lease: File,
  directory: tempfile::TempDir,
}

impl SnapshotStaging {
  pub fn path(&self) -> &Path {
    self.directory.path()
  }
}

pub(crate) struct Settings {
  revision: String,
  target: Option<String>,
  pub policy: UploadPolicy,
  pub version: u64,
  effective_at: u64,
  applied_version: Option<u64>,
}

pub(crate) struct UploadAttempt {
  pub id: ReportId,
  pub metadata: String,
  pub size: u64,
  pub attempt: u32,
}

pub(crate) enum UploadOutcome {
  Delivered(UploadReceipt),
  Failed(FailureReason),
  Retry(Duration),
  Interrupted,
}

impl ReportStore {
  pub fn open(mut config: ReportsConfig) -> Result<Self, ReportError> {
    validate_config(&mut config)?;
    create_private_dir(&config.directory)?;
    config.directory = config.directory.canonicalize()?;
    create_private_dir(&config.directory.join("objects"))?;
    create_private_dir(&config.directory.join("snapshot-staging"))?;
    let store = Self::from_config(config)?;
    let lock = store.lock_file(IMPORT_LOCK)?;
    lock.lock()?;
    let mut connection = store.connection()?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    let schema_version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match schema_version {
      0 => {
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(SCHEMA)?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
      }
      SCHEMA_VERSION => {}
      _ => return Err(ReportError::InvalidConfiguration),
    }
    connection.execute(
      "INSERT OR IGNORE INTO settings VALUES (1, ?1, ?2, ?3, ?4, 1, ?5, NULL)",
      params![
        store.config.application_id,
        store.revision,
        store.target,
        serde_json::to_string(&store.config.policy)?,
        now_ms()
      ],
    )?;
    let application_id: String =
      connection.query_row("SELECT application_id FROM settings WHERE singleton=1", [], |row| {
        row.get(0)
      })?;
    if application_id != store.config.application_id {
      return Err(ReportError::InvalidConfiguration);
    }
    let saved = read_settings(&connection)?;
    if saved.target != store.target
      || (store.config.policy.mode == UploadMode::Disabled && saved.policy != store.config.policy)
    {
      connection.execute(
        "UPDATE settings SET revision=?1, target_id=?2, policy=?3, version=version+1,
            effective_at=?4, applied_version=NULL WHERE singleton=1",
        params![
          store.revision,
          store.target,
          serde_json::to_string(&store.config.policy)?,
          now_ms()
        ],
      )?;
    } else if saved.revision != store.revision {
      connection.execute(
        "UPDATE settings SET revision=?1, version=version+1, applied_version=NULL WHERE singleton=1",
        [&store.revision],
      )?;
    }
    store.recover_objects(&connection, || true)?;
    store.recover_snapshot_staging(|| true)?;
    store.expire(&mut connection)?;
    Ok(store)
  }

  pub(crate) fn open_child(mut config: ReportsConfig) -> Result<Self, ReportError> {
    validate_config(&mut config)?;
    let store = Self::from_config(config)?;
    store.ensure_current(&read_settings(&store.connection()?)?)?;
    Ok(store)
  }

  fn from_config(config: ReportsConfig) -> Result<Self, ReportError> {
    let target = config.upload.as_ref().map(|target| hash(target.url.as_bytes()));
    let credential_generation = config.upload.as_ref().map(|target| target.credential_generation);
    let revision = hash(serde_json::to_vec(&(
      &config.application_id,
      &config.sources,
      &config.limits,
      &target,
      credential_generation,
    ))?);
    Ok(Self {
      config,
      revision,
      target,
    })
  }

  pub fn collect(&self) -> Result<(), ReportError> {
    self.collect_until(|| true)
  }

  pub(crate) fn collect_until(&self, keep_scanning: impl Fn() -> bool) -> Result<(), ReportError> {
    let _lock = match self.import_lock_until(|| !keep_scanning()) {
      Ok(lock) => lock,
      Err(ReportError::Cancelled) => return Ok(()),
      Err(error) => return Err(error),
    };
    let mut connection = self.connection()?;
    self.ensure_current(&read_settings(&connection)?)?;
    self.recover_objects(&connection, &keep_scanning)?;
    self.recover_snapshot_staging(&keep_scanning)?;
    if !keep_scanning() {
      return Ok(());
    }
    self.expire(&mut connection)?;
    // A bad source should not prevent the other authorized sources from being scanned.
    let mut source_error = None;
    for source in &self.config.sources {
      if !keep_scanning() {
        break;
      }
      let result = importer::scan(source, &self.config.application_id, &keep_scanning, |candidate| {
        if !keep_scanning() {
          return Ok(false);
        }
        match self.import(&mut connection, candidate, || !keep_scanning()) {
          Err(ReportError::Cancelled) => Ok(false),
          Ok(_)
          | Err(
            ReportError::FileTooLarge | ReportError::QuotaExceeded | ReportError::InvalidReport | ReportError::Expired,
          ) => Ok(true),
          Err(error) => Err(error),
        }
      });
      if let Err(error) = result {
        source_error.get_or_insert(error);
      }
    }
    source_error.map_or(Ok(()), Err)
  }

  pub fn snapshot_staging_directory(&self) -> PathBuf {
    self.config.directory.join("snapshot-staging")
  }

  /// Create a capture destination protected from recovery by an operating-system lease.
  pub fn stage_snapshot_until(&self, is_cancelled: impl Fn() -> bool) -> Result<SnapshotStaging, ReportError> {
    let _lock = self.import_lock_until(is_cancelled)?;
    let mut builder = tempfile::Builder::new();
    builder.prefix("snapshot-");
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt as _;
      builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let directory = builder.tempdir_in(self.snapshot_staging_directory())?;
    let lease = private_open(&directory.path().join(SNAPSHOT_LEASE), true)?;
    lease.lock()?;
    Ok(SnapshotStaging {
      _lease: lease,
      directory,
    })
  }

  pub fn register_snapshot(&self, path: &Path, version: &str) -> Result<ReportSummary, ReportError> {
    self.register_snapshot_until(path, version, || false)
  }

  /// Register a completed snapshot unless cancellation is observed before publication.
  pub fn register_snapshot_until(
    &self,
    path: &Path,
    version: &str,
    is_cancelled: impl Fn() -> bool,
  ) -> Result<ReportSummary, ReportError> {
    let _lock = self.import_lock_until(&is_cancelled)?;
    let mut connection = self.connection()?;
    self.ensure_current(&read_settings(&connection)?)?;
    self.recover_objects(&connection, || !is_cancelled())?;
    if is_cancelled() {
      return Err(ReportError::Cancelled);
    }
    self.expire(&mut connection)?;
    let candidate = importer::snapshot(
      &self.snapshot_staging_directory(),
      path,
      &self.config.application_id,
      version,
    )?;
    let id = self.import(&mut connection, candidate, is_cancelled)?;
    self.status_on(&connection, id)
  }

  pub fn list_reports(&self, query: ReportQuery) -> Result<Vec<ReportSummary>, ReportError> {
    let connection = self.connection()?;
    let statement = format!(
      "
            SELECT r.metadata, r.size, {DISPLAY_STATE} AS display_state,
                CASE WHEN u.state='uploaded' THEN u.result ELSE coalesce(r.failure, u.result) END
            FROM reports r LEFT JOIN uploads u ON u.report_id=r.id AND u.target_id=:target
            WHERE (:source IS NULL OR r.source_id=:source) AND (:kind IS NULL OR r.kind=:kind)
                AND (:status IS NULL OR display_state=:status)
            ORDER BY r.retention_time DESC, r.id DESC LIMIT :limit OFFSET :offset
        "
    );
    let mut statement = connection.prepare(&statement)?;
    let reports = statement.query_map(
      named_params! {
        ":target": self.target,
        ":source": query.source_id,
        ":kind": query.kind.map(kind_text),
        ":status": query.status.map(state_text),
        ":limit": query.limit.min(MAX_PAGE),
        ":offset": query.offset,
        ":cutoff": self.retention_cutoff(),
      },
      summary,
    )?;
    Ok(reports.collect::<Result<_, _>>()?)
  }

  pub fn status(&self, id: ReportId) -> Result<ReportSummary, ReportError> {
    self.status_on(&self.connection()?, id)
  }

  fn status_on(&self, connection: &Connection, id: ReportId) -> Result<ReportSummary, ReportError> {
    let id = id.to_string();
    connection
      .query_row(
        &format!(
          "SELECT r.metadata, r.size, {DISPLAY_STATE},
                CASE WHEN u.state='uploaded' THEN u.result ELSE coalesce(r.failure, u.result) END
                FROM reports r LEFT JOIN uploads u ON u.report_id=r.id AND u.target_id=:target WHERE r.id=:id"
        ),
        named_params! { ":target": self.target, ":id": id, ":cutoff": self.retention_cutoff() },
        summary,
      )
      .optional()?
      .ok_or(ReportError::NotFound)
  }

  pub fn request_upload(&self, id: ReportId) -> Result<ReportSummary, ReportError> {
    let mut connection = self.connection()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let settings = read_settings(&transaction)?;
    self.ensure_current(&settings)?;
    if settings.policy.mode == UploadMode::Disabled {
      return Err(ReportError::Disabled);
    }
    let target = self.target.as_deref().ok_or(ReportError::MissingTarget)?;
    let before = self.status_on(&transaction, id)?;
    match before.status {
      ReportStatus::Uploaded(_) => return Ok(before),
      ReportStatus::Expired => return Err(ReportError::Expired),
      ReportStatus::Failed(FailureReason::FileTooLarge) => return Err(ReportError::FileTooLarge),
      ReportStatus::Failed(FailureReason::QueueFull) => return Err(ReportError::QuotaExceeded),
      ReportStatus::Failed(FailureReason::InvalidDump) => return Err(ReportError::InvalidReport),
      ReportStatus::Failed(FailureReason::ContentConflict) => return Err(ReportError::PermanentFailure),
      _ => {}
    }
    transaction.execute(
      "INSERT INTO uploads(report_id,target_id,manual,state) VALUES(?1,?2,1,'queued')
             ON CONFLICT(report_id,target_id) DO UPDATE SET manual=1,
                state=CASE WHEN uploads.state='failed' THEN 'queued' ELSE uploads.state END,
                next_at=CASE WHEN uploads.state='failed' THEN 0 ELSE uploads.next_at END,
                attempts=CASE WHEN uploads.state='failed' THEN 0 ELSE uploads.attempts END,
                result=CASE WHEN uploads.state='failed' THEN NULL ELSE uploads.result END",
      params![id.to_string(), target],
    )?;
    let after = self.status_on(&transaction, id)?;
    transaction.commit()?;
    Ok(after)
  }

  pub fn set_upload_policy(&self, policy: UploadPolicy, timeout: Duration) -> Result<PolicyStatus, ReportError> {
    validate_policy(&self.config, &policy)?;
    let mut connection = self.connection()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = read_settings(&transaction)?;
    self.ensure_current(&current)?;
    let version = if current.policy != policy {
      transaction.execute(
        "UPDATE settings SET policy=?1, version=version+1, effective_at=?2 WHERE singleton=1",
        params![serde_json::to_string(&policy)?, now_ms()],
      )?;
      current.version + 1
    } else {
      current.version
    };
    transaction.commit()?;
    let wait_started = Instant::now();
    loop {
      let state = self.upload_policy()?;
      if state.uploader == UploaderStatus::Stopped || state.applied_version.is_some_and(|applied| applied >= version) {
        return Ok(state);
      }
      let remaining = timeout.min(MAX_POLICY_WAIT).saturating_sub(wait_started.elapsed());
      if remaining.is_zero() {
        return Err(ReportError::PolicyTimeout);
      }
      std::thread::sleep(remaining.min(Duration::from_millis(25)));
    }
  }

  pub fn upload_policy(&self) -> Result<PolicyStatus, ReportError> {
    // Check ownership before reading the acknowledgement. A newly acquired sender lock
    // resets the acknowledgement before starting any request.
    let idle_lock = self.try_sender_lock()?;
    let settings = read_settings(&self.connection()?)?;
    let uploader = if idle_lock.is_some() {
      UploaderStatus::Stopped
    } else {
      UploaderStatus::Running
    };
    Ok(PolicyStatus {
      policy: settings.policy,
      version: settings.version,
      applied_version: settings.applied_version,
      uploader,
    })
  }

  pub fn should_start_uploader(&self) -> Result<bool, ReportError> {
    let connection = self.connection()?;
    let settings = read_settings(&connection)?;
    self.ensure_current(&settings)?;
    let Some(target) = &self.target else { return Ok(false) };
    match settings.policy.mode {
      UploadMode::Disabled => Ok(false),
      UploadMode::Automatic => Ok(true),
      UploadMode::ManualOnly => Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM uploads u JOIN reports r ON r.id=u.report_id
                 WHERE u.target_id=?1 AND u.manual=1 AND u.state IN ('queued','uploading') AND r.state='ready' AND r.retention_time>=?2)",
        params![target, self.retention_cutoff()],
        |row| row.get(0),
      )?),
    }
  }

  pub(crate) fn try_sender_lock(&self) -> Result<Option<File>, ReportError> {
    let lock = self.lock_file(SEND_LOCK)?;
    match lock.try_lock() {
      Ok(()) => Ok(Some(lock)),
      Err(TryLockError::WouldBlock) => Ok(None),
      Err(TryLockError::Error(error)) => Err(error.into()),
    }
  }

  pub(crate) fn sender_started(&self) -> Result<(), ReportError> {
    let mut connection = self.connection()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    self.ensure_current(&read_settings(&transaction)?)?;
    transaction.execute("UPDATE settings SET applied_version=NULL WHERE singleton=1", [])?;
    transaction.execute(
      "UPDATE uploads SET state='queued', next_at=0, attempts=max(0,attempts-1) WHERE state='uploading'",
      [],
    )?;
    transaction.commit()?;
    Ok(())
  }

  pub(crate) fn settings(&self) -> Result<Settings, ReportError> {
    let settings = read_settings(&self.connection()?)?;
    self.ensure_current(&settings)?;
    Ok(settings)
  }

  pub(crate) fn acknowledge(&self, version: u64) -> Result<(), ReportError> {
    self.connection()?.execute(
      "UPDATE settings SET applied_version=?1 WHERE singleton=1 AND version=?1 AND revision=?2",
      params![version, self.revision],
    )?;
    Ok(())
  }

  pub(crate) fn claim(&self, version: u64) -> Result<Option<UploadAttempt>, ReportError> {
    let mut connection = self.connection()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let settings = read_settings(&transaction)?;
    self.ensure_current(&settings)?;
    if settings.version != version || settings.policy.mode == UploadMode::Disabled {
      return Ok(None);
    }
    let target = self.target.as_deref().ok_or(ReportError::MissingTarget)?;
    let automatic = settings.policy.mode == UploadMode::Automatic;
    let sources = self.automatic_sources(&settings.policy)?;
    let cutoff = self.retention_cutoff();
    let values: &[(&str, &dyn rusqlite::ToSql)] = &[
      (":automatic", &automatic),
      (":sources", &sources),
      (":snapshots", &settings.policy.include_manual_snapshots),
      (":historical", &settings.policy.include_existing),
      (":effective_at", &settings.effective_at),
      (":target", &target),
      (":cutoff", &cutoff),
    ];
    if automatic {
      transaction.execute(
        &format!(
          "INSERT OR IGNORE INTO uploads(report_id,target_id,manual,state)
                    SELECT r.id,:target,0,'queued' FROM reports r WHERE r.state='ready' AND r.retention_time>=:cutoff
                    AND ({AUTOMATIC_ELIGIBILITY})"
        ),
        values,
      )?;
    }
    let now = now_ms();
    let mut claim_values = values.to_vec();
    claim_values.push((":now", &now));
    let attempt = transaction
      .query_row(
        &format!(
          "SELECT r.id, r.metadata, r.size, u.attempts FROM uploads u
                JOIN reports r ON u.report_id=r.id WHERE u.target_id=:target AND r.state='ready'
                AND r.retention_time>=:cutoff AND u.state='queued' AND u.next_at<=:now
                AND (u.manual OR ({AUTOMATIC_ELIGIBILITY}))
                ORDER BY u.next_at, r.retention_time, r.id LIMIT 1"
        ),
        claim_values.as_slice(),
        |row| {
          Ok(UploadAttempt {
            id: parse_id(row.get(0)?)?,
            metadata: row.get(1)?,
            size: row.get(2)?,
            attempt: row.get::<_, u32>(3)? + 1,
          })
        },
      )
      .optional()?;
    if let Some(attempt) = &attempt {
      transaction.execute(
        "UPDATE uploads SET state='uploading', attempts=?1 WHERE report_id=?2 AND target_id=?3",
        params![attempt.attempt, attempt.id.to_string(), target],
      )?;
    }
    transaction.commit()?;
    Ok(attempt)
  }

  pub(crate) fn complete(&self, attempt: &UploadAttempt, outcome: UploadOutcome) -> Result<(), ReportError> {
    let target = self.target.as_deref().ok_or(ReportError::MissingTarget)?;
    let (state, attempts, next_at, result) = match outcome {
      UploadOutcome::Delivered(receipt) => ("uploaded", attempt.attempt, 0, Some(serde_json::to_string(&receipt)?)),
      UploadOutcome::Failed(reason) => ("failed", attempt.attempt, 0, Some(serde_json::to_string(&reason)?)),
      UploadOutcome::Retry(delay) if attempt.attempt < self.config.limits.max_attempts => (
        "queued",
        attempt.attempt,
        now_ms().saturating_add(duration_ms(delay)),
        None,
      ),
      UploadOutcome::Retry(_) => (
        "failed",
        attempt.attempt,
        0,
        Some(serde_json::to_string(&FailureReason::AttemptsExhausted)?),
      ),
      UploadOutcome::Interrupted => ("queued", attempt.attempt.saturating_sub(1), 0, None),
    };
    self.connection()?.execute(
            "UPDATE uploads SET state=?1, attempts=?2, next_at=?3, result=?4 WHERE report_id=?5 AND target_id=?6 AND state='uploading'",
            params![state, attempts, next_at, result, attempt.id.to_string(), target],
        )?;
    Ok(())
  }

  pub(crate) fn open_upload_dump(&self, id: ReportId) -> Result<File, ReportError> {
    let object = importer::open_root(&self.objects().join(id.to_string()))?;
    Ok(importer::regular_file(&object, Path::new("dump.dmp"))?)
  }

  fn automatic_sources(&self, policy: &UploadPolicy) -> Result<String, ReportError> {
    let sources: Vec<_> = self
      .config
      .sources
      .iter()
      .map(|source| source.id.as_str())
      .chain(std::iter::once(SNAPSHOT_SOURCE_ID))
      .filter(|id| policy.sources.is_empty() || policy.sources.iter().any(|source| source == id))
      .collect();
    Ok(serde_json::to_string(&sources)?)
  }

  fn import(
    &self,
    connection: &mut Connection,
    mut candidate: Candidate,
    is_cancelled: impl Fn() -> bool,
  ) -> Result<ReportId, ReportError> {
    if is_cancelled() {
      return Err(ReportError::Cancelled);
    }
    let incoming = &mut candidate.manifest;
    if incoming.retention_time < self.retention_cutoff() {
      return Err(ReportError::Expired);
    }
    let previous: Option<(String, String, Option<String>)> = connection
      .query_row(
        "SELECT id, state, failure FROM reports WHERE source_id=?1 AND backend_id=?2",
        params![incoming.metadata.source_id, incoming.backend_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
      )
      .optional()?;
    if let Some((id, state, failure)) = previous {
      incoming.metadata.report_id = parse_id(id)?;
      // Capacity may return, and the configured per-report limit may increase.
      let failure = failure.as_deref().map(decode_json).transpose()?;
      if state != "failed" || failure == Some(FailureReason::InvalidDump) {
        return Ok(incoming.metadata.report_id);
      }
    }
    let id = incoming.metadata.report_id;
    if incoming.size_bytes > self.config.limits.max_report_bytes {
      self.record_import_failure(connection, incoming, FailureReason::FileTooLarge)?;
      return Err(ReportError::FileTooLarge);
    }
    if !self.make_room(connection, incoming.size_bytes)? {
      self.record_import_failure(connection, incoming, FailureReason::QueueFull)?;
      return Err(ReportError::QuotaExceeded);
    }
    let objects = self.objects();
    let staging = tempfile::Builder::new().prefix(".stage-").tempdir_in(&objects)?;
    let mut dump = new_file(&staging.path().join("dump.dmp"))?;
    let copied = io::copy(&mut (&mut candidate.dump).take(incoming.size_bytes + 1), &mut dump)?;
    if copied != incoming.size_bytes {
      self.record_import_failure(connection, incoming, FailureReason::InvalidDump)?;
      return Err(ReportError::InvalidReport);
    }
    dump.rewind()?;
    match importer::inspect_dump(&mut dump, incoming.size_bytes, incoming.metadata.kind) {
      Ok(Some(architecture)) if incoming.metadata.architecture.is_none() => {
        incoming.metadata.architecture = Some(architecture)
      }
      Ok(_) => {}
      Err(ReportError::InvalidReport) => {
        self.record_import_failure(connection, incoming, FailureReason::InvalidDump)?;
        return Err(ReportError::InvalidReport);
      }
      Err(error) => return Err(error),
    }
    dump.sync_all()?;
    drop(dump);
    let mut manifest = new_file(&staging.path().join("report.json"))?;
    manifest.write_all(&serde_json::to_vec(incoming)?)?;
    manifest.sync_all()?;
    drop(manifest);
    sync_directory(staging.path())?;
    if is_cancelled() {
      return Err(ReportError::Cancelled);
    }
    fs::rename(staging.path(), objects.join(id.to_string()))?;
    sync_directory(&objects)?;
    record_manifest(connection, incoming, "ready", None)?;
    Ok(id)
  }

  fn recover_objects(&self, connection: &Connection, keep_scanning: impl Fn() -> bool) -> Result<(), ReportError> {
    for entry in fs::read_dir(self.objects())? {
      if !keep_scanning() {
        break;
      }
      let entry = entry?;
      let name = entry.file_name();
      let Some(name) = name.to_str() else { continue };
      if !entry.file_type()?.is_dir() {
        continue;
      }
      if name.starts_with(".stage-") {
        fs::remove_dir_all(entry.path())?;
        continue;
      }
      let Ok(id) = name.parse::<ReportId>() else { continue };
      let state: Option<String> = connection
        .query_row("SELECT state FROM reports WHERE id=?1", [name], |row| row.get(0))
        .optional()?;
      match state.as_deref() {
        Some("ready") => continue,
        Some("expired") => {
          fs::remove_dir_all(entry.path())?;
          continue;
        }
        _ => {}
      }
      let object = importer::open_root(&entry.path())?;
      let manifest: Manifest = importer::read_json(&object, "report.json")?;
      if manifest.metadata.report_id != id
        || manifest.metadata.application_id != self.config.application_id
        || manifest.metadata.schema_version != 1
        || importer::regular_file(&object, Path::new("dump.dmp"))?
          .metadata()?
          .len()
          != manifest.size_bytes
      {
        return Err(ReportError::InvalidReport);
      }
      record_manifest(connection, &manifest, "ready", None)?;
    }
    Ok(())
  }

  fn recover_snapshot_staging(&self, keep_scanning: impl Fn() -> bool) -> Result<(), ReportError> {
    for entry in fs::read_dir(self.snapshot_staging_directory())? {
      if !keep_scanning() {
        break;
      }
      let entry = entry?;
      let name = entry.file_name();
      if !name.to_str().is_some_and(|name| name.starts_with("snapshot-")) || !entry.file_type()?.is_dir() {
        continue;
      }
      let lease = match private_open(&entry.path().join(SNAPSHOT_LEASE), false) {
        Ok(lease) => lease,
        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
        Err(error) => return Err(error.into()),
      };
      match lease.try_lock() {
        Ok(()) => {
          drop(lease);
          match fs::remove_dir_all(entry.path()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
          }
        }
        Err(TryLockError::WouldBlock) => {}
        Err(TryLockError::Error(error)) => return Err(error.into()),
      }
    }
    Ok(())
  }

  fn expire(&self, connection: &mut Connection) -> Result<(), ReportError> {
    let cutoff = self.retention_cutoff();
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let expired = {
      let mut statement = transaction.prepare(
                "UPDATE reports SET state='expired', failure=NULL WHERE state!='expired' AND retention_time<?1
                    AND NOT EXISTS (SELECT 1 FROM uploads WHERE report_id=reports.id AND state='uploading') RETURNING id",
            )?;
      statement
        .query_map([cutoff], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
    };
    transaction.commit()?;
    for id in expired {
      self.remove_object(&id)?;
    }
    // Retain the expiry tombstone and the receiver receipt for an additional retention period.
    let tombstone_cutoff = cutoff.saturating_sub(duration_ms(self.config.limits.retention));
    connection.execute(
      "DELETE FROM reports WHERE state='expired' AND retention_time<?1",
      [tombstone_cutoff],
    )?;
    Ok(())
  }

  fn make_room(&self, connection: &mut Connection, size: u64) -> Result<bool, ReportError> {
    let mut occupied: u64 = connection.query_row(
      "SELECT coalesce(sum(size),0) FROM reports WHERE state='ready'",
      [],
      |row| row.get(0),
    )?;
    while occupied.saturating_add(size) > self.config.limits.max_total_bytes {
      let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
      let evict: Option<(String, u64)> = transaction
        .query_row(
          "SELECT r.id, r.size FROM reports r WHERE r.state='ready'
                 AND EXISTS (SELECT 1 FROM uploads u WHERE u.report_id=r.id AND u.state='uploaded')
                 AND NOT EXISTS (SELECT 1 FROM uploads u WHERE u.report_id=r.id AND u.state IN ('uploading','queued'))
                 ORDER BY r.retention_time, r.id LIMIT 1",
          [],
          |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
      let Some((id, size)) = evict else { return Ok(false) };
      transaction.execute("UPDATE reports SET state='expired' WHERE id=?1", [&id])?;
      transaction.commit()?;
      self.remove_object(&id)?;
      occupied = occupied.saturating_sub(size);
    }
    Ok(true)
  }

  fn remove_object(&self, id: &str) -> Result<(), ReportError> {
    // Only IDs generated by the queue can name object directories.
    let id: ReportId = parse_id(id.to_owned())?;
    match fs::remove_dir_all(self.objects().join(id.to_string())) {
      Ok(()) => Ok(()),
      Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
      Err(error) => Err(error.into()),
    }
  }

  fn record_import_failure(
    &self,
    connection: &Connection,
    manifest: &Manifest,
    reason: FailureReason,
  ) -> Result<(), ReportError> {
    // An unpublished manual request has no stable registration until it succeeds.
    if manifest.metadata.kind == ReportKind::Fatal {
      record_manifest(connection, manifest, "failed", Some(reason))?;
    }
    Ok(())
  }

  fn ensure_current(&self, settings: &Settings) -> Result<(), ReportError> {
    if settings.revision == self.revision {
      Ok(())
    } else {
      Err(ReportError::StaleConfiguration)
    }
  }

  fn retention_cutoff(&self) -> u64 {
    now_ms().saturating_sub(duration_ms(self.config.limits.retention))
  }
  fn objects(&self) -> PathBuf {
    self.config.directory.join("objects")
  }

  fn import_lock_until(&self, is_cancelled: impl Fn() -> bool) -> Result<File, ReportError> {
    let lock = self.lock_file(IMPORT_LOCK)?;
    loop {
      if is_cancelled() {
        return Err(ReportError::Cancelled);
      }
      match lock.try_lock() {
        Ok(()) => return Ok(lock),
        Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(20)),
        Err(TryLockError::Error(error)) => return Err(error.into()),
      }
    }
  }

  fn lock_file(&self, name: &str) -> Result<File, ReportError> {
    let file = private_open(&self.config.directory.join(name), false)?;
    Ok(file)
  }

  fn connection(&self) -> Result<Connection, ReportError> {
    let path = self.config.directory.join("queue.sqlite3");
    private_open(&path, false)?;
    let connection = Connection::open_with_flags(
      path,
      OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_secs(2))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    Ok(connection)
  }
}

fn read_settings(connection: &Connection) -> Result<Settings, ReportError> {
  Ok(connection.query_row(
    "SELECT revision, target_id, policy, version, effective_at, applied_version FROM settings WHERE singleton=1",
    [],
    |row| {
      Ok(Settings {
        revision: row.get(0)?,
        target: row.get(1)?,
        policy: decode_json(&row.get::<_, String>(2)?)?,
        version: row.get(3)?,
        effective_at: row.get(4)?,
        applied_version: row.get(5)?,
      })
    },
  )?)
}

fn record_manifest(
  connection: &Connection,
  manifest: &Manifest,
  state: &str,
  failure: Option<FailureReason>,
) -> Result<(), ReportError> {
  let metadata = &manifest.metadata;
  let failure = failure.map(|reason| serde_json::to_string(&reason)).transpose()?;
  connection.execute(
        "INSERT INTO reports (id,source_id,backend_id,kind,captured_at,retention_time,state,size,failure,metadata)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(id) DO UPDATE SET state=excluded.state,size=excluded.size,failure=excluded.failure,metadata=excluded.metadata",
        params![metadata.report_id.to_string(), metadata.source_id, manifest.backend_id, kind_text(metadata.kind), metadata.captured_at_unix_ms, manifest.retention_time, state, manifest.size_bytes, failure, serde_json::to_string(metadata)?],
    )?;
  Ok(())
}

fn summary(row: &Row<'_>) -> rusqlite::Result<ReportSummary> {
  let metadata: UploadMetadata = decode_json(&row.get::<_, String>(0)?)?;
  let status = match row.get::<_, String>(2)?.as_str() {
    "local" => ReportStatus::Local,
    "queued" => ReportStatus::Queued,
    "uploading" => ReportStatus::Uploading,
    "uploaded" => ReportStatus::Uploaded(decode_json(&row.get::<_, String>(3)?)?),
    "failed" => ReportStatus::Failed(decode_json(&row.get::<_, String>(3)?)?),
    "expired" => ReportStatus::Expired,
    _ => return Err(rusqlite::Error::InvalidQuery),
  };
  Ok(ReportSummary {
    id: metadata.report_id,
    source_id: metadata.source_id,
    source: metadata.source,
    kind: metadata.kind,
    captured_at_unix_ms: metadata.captured_at_unix_ms,
    application_version: metadata.application_version,
    size_bytes: row.get(1)?,
    status,
  })
}

fn decode_json<T: serde::de::DeserializeOwned>(value: &str) -> rusqlite::Result<T> {
  serde_json::from_str(value)
    .map_err(|error| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error)))
}

fn parse_id(value: String) -> rusqlite::Result<ReportId> {
  value
    .parse()
    .map_err(|error| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error)))
}

fn state_text(state: ReportState) -> &'static str {
  match state {
    ReportState::Local => "local",
    ReportState::Queued => "queued",
    ReportState::Uploading => "uploading",
    ReportState::Uploaded => "uploaded",
    ReportState::Failed => "failed",
    ReportState::Expired => "expired",
  }
}

fn kind_text(kind: ReportKind) -> &'static str {
  match kind {
    ReportKind::Fatal => "fatal",
    ReportKind::ManualSnapshot => "manual_snapshot",
  }
}

pub(crate) fn now_ms() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(duration_ms)
    .unwrap_or(0)
}

pub(crate) fn duration_ms(duration: Duration) -> u64 {
  u64::try_from(duration.as_millis())
    .unwrap_or(u64::MAX)
    .min(i64::MAX as u64)
}

pub(crate) fn hash(value: impl AsRef<[u8]>) -> String {
  format!("{:x}", Sha256::digest(value.as_ref()))
}

fn validate_config(config: &mut ReportsConfig) -> Result<(), ReportError> {
  let limits = &config.limits;
  if !config.directory.is_absolute()
    || !importer::valid_text(&config.application_id, 253)
    || limits.max_report_bytes == 0
    || limits.max_total_bytes < limits.max_report_bytes
    || limits.max_total_bytes > i64::MAX as u64
    || duration_ms(limits.retention) == 0
    || limits.max_attempts == 0
    || limits.max_attempts > 1000
    || limits.request_timeout.is_zero()
    || limits.request_timeout > Duration::from_secs(120)
    || limits.max_retry_delay.is_zero()
    || limits.max_retry_delay > Duration::from_secs(7 * 24 * 60 * 60)
    || config.sources.len() > 64
  {
    return Err(ReportError::InvalidConfiguration);
  }
  let mut sources = HashSet::new();
  for source in &config.sources {
    if !source.directory.is_absolute()
      || source.id == SNAPSHOT_SOURCE_ID
      || !importer::valid_text(&source.id, 128)
      || !sources.insert(&source.id)
    {
      return Err(ReportError::InvalidConfiguration);
    }
  }
  validate_policy(config, &config.policy)?;
  if let Some(target) = &mut config.upload {
    let url = reqwest::Url::parse(&target.url).map_err(|_| ReportError::InvalidConfiguration)?;
    if url.scheme() != "https"
      || url.host().is_none()
      || !url.username().is_empty()
      || url.password().is_some()
      || url.fragment().is_some()
      || target.url.len() > 4096
      || target.bearer_token.as_ref().is_some_and(|token| {
        !importer::valid_text(token, 4096)
          || !token.is_ascii()
          || token.bytes().any(|byte| byte.is_ascii_whitespace())
          || target.credential_generation == 0
      })
    {
      return Err(ReportError::InvalidConfiguration);
    }
    target.url = url.to_string();
  }
  let config_len = serde_json::to_vec(config)
    .map_err(|_| ReportError::InvalidConfiguration)?
    .len();
  if config_len > MAX_CONFIGURATION_BYTES / 2 {
    return Err(ReportError::InvalidConfiguration);
  }
  Ok(())
}

fn validate_policy(config: &ReportsConfig, policy: &UploadPolicy) -> Result<(), ReportError> {
  let mut selected = HashSet::new();
  for source in &policy.sources {
    if !selected.insert(source)
      || (source != SNAPSHOT_SOURCE_ID && !config.sources.iter().any(|allowed| allowed.id == *source))
    {
      return Err(ReportError::InvalidConfiguration);
    }
  }
  Ok(())
}

fn create_private_dir(path: &Path) -> Result<(), ReportError> {
  let parent = path.parent().ok_or(ReportError::InvalidConfiguration)?;
  fs::create_dir_all(parent)?;
  let mut builder = fs::DirBuilder::new();
  #[cfg(unix)]
  {
    use std::os::unix::fs::DirBuilderExt as _;
    builder.mode(0o700);
  }
  match builder.create(path) {
    Ok(()) => {}
    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
    Err(error) => return Err(error.into()),
  }
  let directory = importer::open_root(path)?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt as _;
    if directory.into_std_file().metadata()?.mode() & 0o077 != 0 {
      return Err(ReportError::InvalidConfiguration);
    }
  }
  #[cfg(not(unix))]
  drop(directory);
  Ok(())
}

fn new_file(path: &Path) -> io::Result<File> {
  private_open(path, true)
}

fn private_open(path: &Path, create_new: bool) -> io::Result<File> {
  let mut options = OpenOptions::new();
  options.read(true).write(true).create(true).create_new(create_new);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt as _;
    options
      .mode(0o600)
      .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32);
  }
  #[cfg(windows)]
  {
    use std::os::windows::fs::OpenOptionsExt as _;
    // FILE_FLAG_OPEN_REPARSE_POINT
    options.custom_flags(0x0020_0000);
  }
  let file = options.open(path)?;
  let metadata = file.metadata()?;
  if !metadata.is_file() {
    return Err(io::ErrorKind::InvalidData.into());
  }
  importer::check_owner(&metadata)?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt as _;
    if metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 {
      return Err(io::ErrorKind::PermissionDenied.into());
    }
  }
  Ok(file)
}

fn sync_directory(path: &Path) -> io::Result<()> {
  #[cfg(unix)]
  File::open(path)?.sync_all()?;
  #[cfg(not(unix))]
  let _ = path;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{
    UploadTarget,
    test_data::{config, dump, fatal},
  };

  #[test]
  fn automatic_policy_holds_history_unknown_times_and_unselected_sources() {
    let temporary = tempfile::tempdir().unwrap();
    let config = config(temporary.path());
    let store = ReportStore::open(config.clone()).unwrap();
    fatal(&config, "report-history", Some(now_ms() - 1_000), true);
    fatal(&config, "report-no-time", None, true);
    fatal(&config, "report-partial", None, false);
    store.collect().unwrap();
    let reports = store.list_reports(ReportQuery::default()).unwrap();
    assert_eq!(reports.len(), 2);
    assert!(reports.iter().any(|report| report.captured_at_unix_ms.is_none()));
    assert!(reports.iter().all(|report| report.status == ReportStatus::Local));

    let mut policy = UploadPolicy {
      mode: UploadMode::Automatic,
      sources: vec!["host".to_owned()],
      ..Default::default()
    };
    let state = store.set_upload_policy(policy.clone(), Duration::ZERO).unwrap();
    assert_eq!(state.uploader, UploaderStatus::Stopped);
    assert!(store.claim(state.version).unwrap().is_none());

    std::thread::sleep(Duration::from_millis(2));
    fatal(&config, "report-new", Some(now_ms()), true);
    let snapshot = store.snapshot_staging_directory().join("manual.dmp");
    fs::write(&snapshot, dump(&[(3, 4), (4, 4)])).unwrap();
    let manual = store.register_snapshot(&snapshot, "1.2.3").unwrap();
    store.collect().unwrap();
    let first = store.claim(state.version).unwrap().unwrap();
    assert!(!reports.iter().any(|report| report.id == first.id));
    assert_ne!(first.id, manual.id);
    assert!(store.claim(state.version).unwrap().is_none());
    store.complete(&first, UploadOutcome::Interrupted).unwrap();

    let manual_state = store
      .set_upload_policy(UploadPolicy::default(), Duration::ZERO)
      .unwrap();
    assert!(store.claim(manual_state.version).unwrap().is_none());
    assert_eq!(store.request_upload(first.id).unwrap().status, ReportStatus::Queued);
    assert_eq!(store.claim(manual_state.version).unwrap().unwrap().id, first.id);

    policy.include_existing = true;
    let state = store.set_upload_policy(policy, Duration::ZERO).unwrap();
    let historical = store.claim(state.version).unwrap().unwrap();
    assert!(reports.iter().any(|report| report.id == historical.id));
  }

  #[test]
  fn sender_handoff_retries_same_id_and_receipts_are_receiver_specific() {
    let temporary = tempfile::tempdir().unwrap();
    let mut config = config(temporary.path());
    fatal(&config, "report-sample", None, true);
    let store = ReportStore::open(config.clone()).unwrap();
    store.collect().unwrap();
    let id = store.list_reports(ReportQuery::default()).unwrap()[0].id;
    assert_eq!(store.request_upload(id).unwrap().status, ReportStatus::Queued);
    assert!(store.should_start_uploader().unwrap());
    let lease = store.try_sender_lock().unwrap().unwrap();
    assert!(store.try_sender_lock().unwrap().is_none());
    store.sender_started().unwrap();
    let version = store.settings().unwrap().version;
    let attempt = store.claim(version).unwrap().unwrap();
    assert_eq!(attempt.id, id);
    assert_eq!(store.request_upload(id).unwrap().status, ReportStatus::Uploading);
    drop(lease);
    drop(store);

    let store = ReportStore::open(config.clone()).unwrap();
    let _lease = store.try_sender_lock().unwrap().unwrap();
    store.sender_started().unwrap();
    let retried = store.claim(version).unwrap().unwrap();
    assert_eq!(retried.id, attempt.id);
    assert_eq!(retried.metadata, attempt.metadata);
    let receipt = UploadReceipt {
      schema_version: 1,
      report_id: id,
      receipt_id: "receipt-1".to_owned(),
    };
    store
      .complete(&retried, UploadOutcome::Delivered(receipt.clone()))
      .unwrap();
    assert_eq!(
      store.request_upload(id).unwrap().status,
      ReportStatus::Uploaded(receipt.clone())
    );
    assert!(!store.should_start_uploader().unwrap());

    config.upload = Some(UploadTarget::new("https://localhost:9/different"));
    let other = ReportStore::open(config.clone()).unwrap();
    assert!(matches!(store.settings(), Err(ReportError::StaleConfiguration)));
    assert_eq!(other.status(id).unwrap().status, ReportStatus::Local);
    assert!(!other.should_start_uploader().unwrap());
    assert_eq!(other.request_upload(id).unwrap().status, ReportStatus::Queued);

    config.upload = Some(UploadTarget::new("https://localhost:9/crashes").with_bearer_token("rotated", 2));
    let restored = ReportStore::open(config).unwrap();
    assert_eq!(restored.status(id).unwrap().status, ReportStatus::Uploaded(receipt));
  }

  #[cfg(target_os = "macos")]
  #[test]
  fn cef_move_and_supervised_worker_keep_their_authorized_source_identity() {
    use crate::{ReportSource, ReportSourceConfig, SourceKind};

    let temporary = tempfile::tempdir().unwrap();
    let mut config = config(temporary.path());
    config.sources[0].id = "worker".to_owned();
    config.sources[0].kind = SourceKind::MojoService;
    let cef = temporary.path().join("cef");
    config.sources.push(ReportSourceConfig {
      id: "browser".to_owned(),
      directory: cef.clone(),
      kind: SourceKind::Cef,
    });
    fatal(&config, "report-worker", Some(now_ms()), true);
    let worker_metadata = config.sources[0].directory.join("report-worker/report.json");
    let mut worker: serde_json::Value = serde_json::from_slice(&fs::read(&worker_metadata).unwrap()).unwrap();
    worker["application_id"] = "lintel.worker".into();
    worker["process_role"] = "service".into();
    worker["service"] = serde_json::json!({"identity":"example.worker", "attempt":3});
    fs::write(worker_metadata, worker.to_string()).unwrap();

    fs::create_dir_all(cef.join("pending")).unwrap();
    fs::create_dir_all(cef.join("completed")).unwrap();
    let file = "a77e0160-3712-4f06-8d68-a4e1bed12630.dmp";
    fs::write(cef.join("pending").join(file), dump(&[(6, 168)])).unwrap();
    let store = ReportStore::open(config).unwrap();
    store.collect().unwrap();
    let worker = store
      .list_reports(ReportQuery {
        source_id: Some("worker".to_owned()),
        ..Default::default()
      })
      .unwrap()
      .remove(0);
    assert_eq!(
      worker.source,
      ReportSource::MojoService {
        service_id: "example.worker".to_owned(),
        attempt: 3
      }
    );
    assert_eq!(worker.application_version.as_deref(), Some("1.2.3"));
    let query = ReportQuery {
      source_id: Some("browser".to_owned()),
      ..Default::default()
    };
    let browser = store.list_reports(query.clone()).unwrap().remove(0);
    assert_eq!(browser.source, ReportSource::Cef);
    assert_eq!(browser.application_version, None);
    fs::rename(cef.join("pending").join(file), cef.join("completed").join(file)).unwrap();
    store.collect().unwrap();
    assert_eq!(store.list_reports(query).unwrap(), vec![browser]);
  }

  #[test]
  fn receiver_change_needs_fresh_policy_and_disabled_startup_revokes_grant() {
    let temporary = tempfile::tempdir().unwrap();
    let mut config = config(temporary.path());
    fatal(&config, "report-previously-approved", None, true);
    let store = ReportStore::open(config.clone()).unwrap();
    store.collect().unwrap();
    let report = store.list_reports(ReportQuery::default()).unwrap().remove(0);
    let automatic = UploadPolicy {
      mode: UploadMode::Automatic,
      include_existing: true,
      ..Default::default()
    };
    let version = store
      .set_upload_policy(automatic.clone(), Duration::ZERO)
      .unwrap()
      .version;
    let pending = store.claim(version).unwrap().unwrap();
    store.complete(&pending, UploadOutcome::Interrupted).unwrap();

    config.upload = Some(UploadTarget::new("https://localhost:9/crashes").with_bearer_token("rotated", 1));
    let rotated = ReportStore::open(config.clone()).unwrap();
    assert_eq!(rotated.upload_policy().unwrap().policy, automatic);
    assert_eq!(rotated.status(report.id).unwrap().status, ReportStatus::Queued);

    config.upload = Some(UploadTarget::new("https://localhost:9/new-collector"));
    let other = ReportStore::open(config.clone()).unwrap();
    let changed = other.upload_policy().unwrap();
    assert_eq!(changed.policy.mode, UploadMode::ManualOnly);
    assert_eq!(other.status(report.id).unwrap().status, ReportStatus::Local);
    assert!(other.claim(changed.version).unwrap().is_none());

    other.set_upload_policy(automatic, Duration::ZERO).unwrap();
    config.policy.mode = UploadMode::Disabled;
    let disabled = ReportStore::open(config).unwrap();
    assert_eq!(disabled.upload_policy().unwrap().policy.mode, UploadMode::Disabled);
    assert!(!disabled.should_start_uploader().unwrap());
    assert!(matches!(disabled.request_upload(report.id), Err(ReportError::Disabled)));
  }

  #[test]
  fn manual_retry_resets_backoff_but_keeps_permanent_conflict() {
    let temporary = tempfile::tempdir().unwrap();
    let mut config = config(temporary.path());
    config.limits.max_attempts = 1;
    fatal(&config, "report-retry", None, true);
    let store = ReportStore::open(config.clone()).unwrap();
    store.collect().unwrap();
    let id = store.list_reports(ReportQuery::default()).unwrap()[0].id;
    store.request_upload(id).unwrap();
    let version = store.settings().unwrap().version;
    let attempt = store.claim(version).unwrap().unwrap();
    store
      .complete(&attempt, UploadOutcome::Retry(Duration::from_secs(60)))
      .unwrap();
    assert_eq!(
      store.status(id).unwrap().status,
      ReportStatus::Failed(FailureReason::AttemptsExhausted)
    );
    assert_eq!(store.request_upload(id).unwrap().status, ReportStatus::Queued);
    let retried = store.claim(version).unwrap().unwrap();
    assert_eq!(retried.id, id);
    assert_eq!(retried.attempt, 1);
    store
      .complete(&retried, UploadOutcome::Failed(FailureReason::ContentConflict))
      .unwrap();
    assert!(matches!(store.request_upload(id), Err(ReportError::PermanentFailure)));
  }

  #[test]
  fn import_quota_and_incomplete_publication_recover_without_new_ids() {
    let temporary = tempfile::tempdir().unwrap();
    let mut config = config(temporary.path());
    let dump_size = dump(&[(6, 168)]).len() as u64;
    config.limits.max_report_bytes = dump_size;
    config.limits.max_total_bytes = dump_size;
    let store = ReportStore::open(config.clone()).unwrap();
    fatal(&config, "report-first", None, true);
    store.collect().unwrap();
    let first = store.list_reports(ReportQuery::default()).unwrap()[0].id;
    fatal(&config, "report-second", None, true);
    store.collect().unwrap();
    let failed = store
      .list_reports(ReportQuery {
        status: Some(ReportState::Failed),
        ..Default::default()
      })
      .unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].status, ReportStatus::Failed(FailureReason::QueueFull));
    assert!(matches!(
      store.request_upload(failed[0].id),
      Err(ReportError::QuotaExceeded)
    ));

    store.request_upload(first).unwrap();
    let attempt = store.claim(store.settings().unwrap().version).unwrap().unwrap();
    let receipt = UploadReceipt {
      schema_version: 1,
      report_id: first,
      receipt_id: "good".to_owned(),
    };
    store
      .complete(&attempt, UploadOutcome::Delivered(receipt.clone()))
      .unwrap();
    store.collect().unwrap();
    assert_eq!(store.status(failed[0].id).unwrap().status, ReportStatus::Local);
    assert_eq!(store.status(first).unwrap().status, ReportStatus::Uploaded(receipt));
    assert!(!store.objects().join(first.to_string()).exists());

    // Crash after object publication and before committing its database row.
    let db = store.connection().unwrap();
    db.execute("DELETE FROM reports WHERE id=?1", [failed[0].id.to_string()])
      .unwrap();
    drop(db);
    fs::create_dir(store.objects().join(".stage-interrupted")).unwrap();
    let recovered = ReportStore::open(config).unwrap();
    assert_eq!(recovered.status(failed[0].id).unwrap().status, ReportStatus::Local);
    assert!(!recovered.objects().join(".stage-interrupted").exists());
  }

  #[test]
  fn recovery_retains_live_snapshot_leases_and_removes_abandoned_staging() {
    let temporary = tempfile::tempdir().unwrap();
    let config = config(temporary.path());
    let store = ReportStore::open(config.clone()).unwrap();
    let live = store.stage_snapshot_until(|| false).unwrap();
    #[cfg(unix)]
    {
      use std::os::unix::fs::MetadataExt as _;
      assert_eq!(fs::metadata(live.path()).unwrap().mode() & 0o077, 0);
    }
    let abandoned = store.snapshot_staging_directory().join("snapshot-abandoned");
    create_private_dir(&abandoned).unwrap();
    fs::write(abandoned.join("report.dmp"), b"partial").unwrap();
    let recovered = ReportStore::open(config).unwrap();
    recovered.collect().unwrap();
    assert!(live.path().exists());
    assert!(!abandoned.exists());
  }

  #[test]
  fn expired_work_is_not_claimed_before_maintenance() {
    let temporary = tempfile::tempdir().unwrap();
    let config = config(temporary.path());
    fatal(&config, "report-retired", None, true);
    let store = ReportStore::open(config).unwrap();
    store.collect().unwrap();
    let id = store.list_reports(ReportQuery::default()).unwrap().remove(0).id;
    store.request_upload(id).unwrap();
    store
      .connection()
      .unwrap()
      .execute("UPDATE reports SET retention_time=0", [])
      .unwrap();
    assert_eq!(store.status(id).unwrap().status, ReportStatus::Expired);
    assert!(matches!(store.request_upload(id), Err(ReportError::Expired)));
    assert!(!store.should_start_uploader().unwrap());
    assert!(store.claim(store.settings().unwrap().version).unwrap().is_none());
  }

  #[test]
  fn cancelled_or_outside_snapshot_is_never_published() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ReportStore::open(config(temporary.path())).unwrap();
    let path = store.snapshot_staging_directory().join("manual.dmp");
    fs::write(&path, dump(&[(3, 4), (4, 4)])).unwrap();
    let checks = std::cell::Cell::new(0);
    let result = store.register_snapshot_until(&path, "1.2.3", || {
      checks.set(checks.get() + 1);
      checks.get() >= 4
    });
    assert!(matches!(result, Err(ReportError::Cancelled)));
    assert!(store.list_reports(ReportQuery::default()).unwrap().is_empty());
    assert_eq!(fs::read_dir(store.objects()).unwrap().count(), 0);
    let outside = temporary.path().join("outside.dmp");
    fs::write(&outside, dump(&[(3, 4), (4, 4)])).unwrap();
    assert!(matches!(
      store.register_snapshot(&outside, "1.2.3"),
      Err(ReportError::InvalidReport)
    ));
    #[cfg(unix)]
    {
      let link = store.snapshot_staging_directory().join("link.dmp");
      std::os::unix::fs::symlink(outside, &link).unwrap();
      assert!(store.register_snapshot(&link, "1.2.3").is_err());
    }
    assert_eq!(
      store.register_snapshot(&path, "1.2.3").unwrap().kind,
      ReportKind::ManualSnapshot
    );
  }
}
