use std::{
  fs::{self, File, OpenOptions},
  io::Write,
  path::{Path, PathBuf},
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
  time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use lintel_core::{
  context::AppContext,
  event_loop::event::LintelEvent,
  plugin::{Plugin, PluginLifecycle},
  tasks::TaskScope,
};
use serde_json::{Map, Value, json};
use tracing::{
  Event, Subscriber,
  field::{Field, Visit},
};
use tracing_subscriber::{Layer, layer::SubscriberExt};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

#[derive(Clone)]
pub struct DiagnosticConfig {
  pub directory: Option<PathBuf>,
  /// 由应用显式授权导出的崩溃根目录。
  pub crash_directory: Option<PathBuf>,
  pub max_crash_bytes: u64,
  pub max_file_bytes: u64,
  pub retained_files: usize,
  /// 仅替换结构化日志中的字段内容；显式导出的二进制 dump 保持原样。
  pub redact: Vec<String>,
  pub install_tracing: bool,
  pub capture_panics: bool,
  pub ui_timeout: Duration,
}

impl Default for DiagnosticConfig {
  fn default() -> Self {
    Self {
      directory: None,
      crash_directory: None,
      max_crash_bytes: 128 * 1024 * 1024,
      max_file_bytes: 2 * 1024 * 1024,
      retained_files: 3,
      redact: Vec::new(),
      install_tracing: true,
      capture_panics: true,
      ui_timeout: Duration::from_secs(2),
    }
  }
}

struct LogFile {
  file: File,
  size: u64,
}

/// 本地日志、卡顿观察和显式导出的诊断包。
pub struct Diagnostics {
  config: DiagnosticConfig,
  directory: PathBuf,
  log: Mutex<LogFile>,
  started: Instant,
  active: AtomicBool,
}

impl Diagnostics {
  pub fn new(config: DiagnosticConfig, default_directory: &Path) -> Result<Self> {
    ensure!(
      config.max_file_bytes >= 256 && (1..=32).contains(&config.retained_files),
      "invalid diagnostic retention limits"
    );
    ensure!(!config.ui_timeout.is_zero(), "UI watchdog timeout must be positive");
    let directory = config.directory.as_deref().unwrap_or(default_directory).to_path_buf();
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(&directory)?;
    #[cfg(unix)]
    ensure!(
      fs::symlink_metadata(&directory)?.is_dir() && fs::metadata(&directory)?.permissions().mode() & 0o077 == 0,
      "diagnostic directory must be private to its owner"
    );
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(directory.join("lintel.0.jsonl"))?;
    let size = file.metadata()?.len();
    Ok(Self {
      config,
      directory,
      log: Mutex::new(LogFile { file, size }),
      started: Instant::now(),
      active: AtomicBool::new(true),
    })
  }

  pub fn record(&self, kind: &str, details: Value) -> Result<()> {
    if !self.active.load(Ordering::Acquire) {
      return Ok(());
    }
    let mut event = json!({
      "time_ms": SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
      "uptime_ms": self.started.elapsed().as_millis(),
      "kind": kind,
      "details": details,
    });
    redact_value(&mut event, &self.config.redact);
    let mut line = serde_json::to_string(&event)?;
    ensure!(
      line.len() as u64 + 1 <= self.config.max_file_bytes,
      "diagnostic event exceeds log limit"
    );
    line.push('\n');
    let mut log = self
      .log
      .lock()
      .map_err(|_| anyhow::anyhow!("diagnostic log lock poisoned"))?;
    if log.size + line.len() as u64 > self.config.max_file_bytes {
      log.file.flush()?;
      for index in (1..self.config.retained_files).rev() {
        let previous = self.directory.join(format!("lintel.{}.jsonl", index - 1));
        if previous.exists() {
          fs::rename(previous, self.directory.join(format!("lintel.{index}.jsonl")))?;
        }
      }
      let mut options = OpenOptions::new();
      options.create(true).write(true).truncate(true);
      #[cfg(unix)]
      options.mode(0o600);
      log.file = options.open(self.directory.join("lintel.0.jsonl"))?;
      log.size = 0;
    }
    log.file.write_all(line.as_bytes())?;
    log.file.flush()?;
    log.size += line.len() as u64;
    Ok(())
  }

  pub fn layer(self: &Arc<Self>) -> DiagnosticLayer {
    DiagnosticLayer(Arc::clone(self))
  }

  fn export_crashes(&self, crash_root: &Path, destination: &Path) -> Result<()> {
    let access = super::FileAccess::read_only(crash_root, self.config.max_crash_bytes)?;
    let mut copied = 0_u64;
    let mut records = 0;
    let mut copy_file = |relative: &Path, minidump: bool| -> Result<bool> {
      let Ok(bytes) = access.read(relative) else {
        return Ok(false);
      };
      // Recognize Crashpad dump files without including its database/settings/attachments.
      // A damaged dump remains useful for diagnosis; full stream validation is separate.
      if minidump && !bytes.starts_with(b"MDMP") {
        return Ok(false);
      }
      copied = copied
        .checked_add(bytes.len() as u64)
        .context("diagnostic size overflow")?;
      ensure!(
        copied <= self.config.max_crash_bytes,
        "crash reports exceed diagnostic export limit"
      );
      let target = destination.join("crashes").join(relative);
      let mut directories = fs::DirBuilder::new();
      directories.recursive(true);
      #[cfg(unix)]
      directories.mode(0o700);
      directories.create(target.parent().context("invalid report path")?)?;
      let mut options = OpenOptions::new();
      options.write(true).create_new(true);
      #[cfg(unix)]
      options.mode(0o600);
      options.open(target)?.write_all(&bytes)?;
      Ok(true)
    };
    let mut reports = fs::read_dir(crash_root)?.collect::<std::io::Result<Vec<_>>>()?;
    reports.sort_by_key(|entry| entry.file_name());
    for entry in reports.into_iter().rev() {
      if records == 16 {
        break;
      }
      let name = entry.file_name();
      if !name.to_string_lossy().starts_with("report-") || !entry.file_type()?.is_dir() {
        continue;
      }
      records += 1;
      for file in ["report.dmp", "report.json", "metadata.json", "result.json"] {
        copy_file(&Path::new(&name).join(file), false)?;
      }
    }
    for state in ["pending", "completed"] {
      let directory = crash_root.join(state);
      match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => continue,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
        Err(error) => return Err(error.into()),
      }
      let mut dumps = fs::read_dir(&directory)?.collect::<std::io::Result<Vec<_>>>()?;
      dumps.sort_by_key(|entry| entry.file_name());
      for dump in dumps.into_iter().rev() {
        if records == 16 {
          break;
        }
        if !dump.file_type()?.is_file() || !crashpad_dump_name(&dump.file_name()) {
          continue;
        }
        if copy_file(&Path::new(state).join(dump.file_name()), true)? {
          records += 1;
        }
      }
    }
    Ok(())
  }

  pub fn export(&self, destination: &Path) -> Result<PathBuf> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    builder
      .create(destination)
      .context("diagnostic output directory must be new")?;
    let result = (|| -> Result<()> {
      let mut log = self
        .log
        .lock()
        .map_err(|_| anyhow::anyhow!("diagnostic log lock poisoned"))?;
      log.file.flush()?;
      for index in 0..self.config.retained_files {
        let name = format!("lintel.{index}.jsonl");
        let source = self.directory.join(&name);
        if source.is_file() && !fs::symlink_metadata(&source)?.file_type().is_symlink() {
          fs::copy(source, destination.join(name))?;
        }
      }
      // Crash reports can be much larger than logs; release the logging lock first.
      drop(log);
      if let Some(crash_root) = &self.config.crash_directory {
        self.export_crashes(crash_root, destination)?;
      }
      let snapshot = lintel_lib_iced::ui_perf::snapshot();
      let entries: Vec<_> = snapshot
        .entries
        .iter()
        .map(|entry| {
          json!({
            "kind": entry.kind, "name": entry.name, "browser_id": entry.browser_id,
            "count": entry.count, "average_ms": entry.avg_ms, "max_ms": entry.max_ms,
            "slow_count": entry.slow_count,
          })
        })
        .collect();
      fs::write(
        destination.join("manifest.json"),
        serde_json::to_vec_pretty(&json!({
          "lintel_version": env!("CARGO_PKG_VERSION"), "os": std::env::consts::OS,
          "architecture": std::env::consts::ARCH, "uptime_ms": self.started.elapsed().as_millis(),
          "performance_enabled": snapshot.enabled, "dropped_samples": snapshot.dropped_samples,
          "entries": entries,
        }))?,
      )?;
      Ok(())
    })();
    if let Err(error) = result {
      let _ = fs::remove_dir_all(destination);
      return Err(error);
    }
    Ok(destination.to_path_buf())
  }
}

fn crashpad_dump_name(name: &std::ffi::OsStr) -> bool {
  let Some(name) = name.to_str().and_then(|name| name.strip_suffix(".dmp")) else {
    return false;
  };
  name.len() == 36
    && name.bytes().enumerate().all(|(index, byte)| {
      if [8, 13, 18, 23].contains(&index) {
        byte == b'-'
      } else {
        byte.is_ascii_hexdigit()
      }
    })
}

pub struct DiagnosticLayer(Arc<Diagnostics>);

fn redact_value(value: &mut Value, secrets: &[String]) {
  match value {
    Value::String(text) => {
      for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        *text = text.replace(secret, "[REDACTED]");
      }
    }
    Value::Array(values) => values.iter_mut().for_each(|value| redact_value(value, secrets)),
    Value::Object(values) => values.values_mut().for_each(|value| redact_value(value, secrets)),
    _ => {}
  }
}

struct Fields(Map<String, Value>);

impl Visit for Fields {
  fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
    self
      .0
      .insert(field.name().to_owned(), Value::String(format!("{value:?}")));
  }

  fn record_str(&mut self, field: &Field, value: &str) {
    self.0.insert(field.name().to_owned(), Value::String(value.to_owned()));
  }
}

impl<S: Subscriber> Layer<S> for DiagnosticLayer {
  fn on_event(&self, event: &Event<'_>, _context: tracing_subscriber::layer::Context<'_, S>) {
    let mut fields = Fields(Map::new());
    event.record(&mut fields);
    // 此处不再使用 tracing，避免日志记录递归。
    let _ = self.0.record(
      "log",
      json!({
        "target": event.metadata().target(), "level": event.metadata().level().as_str(), "fields": fields.0,
      }),
    );
  }
}

pub fn diagnostics(config: DiagnosticConfig) -> impl FnOnce(&mut AppContext) + Send + Sync {
  move |context| {
    context.register_plugin(Arc::new(DiagnosticPlugin {
      config,
      instance: Mutex::new(None),
    }))
  }
}

struct DiagnosticPlugin {
  config: DiagnosticConfig,
  instance: Mutex<Option<Arc<Diagnostics>>>,
}

impl Plugin for DiagnosticPlugin {
  fn name(&self) -> &str {
    "lintel.diagnostics"
  }
}

impl PluginLifecycle for DiagnosticPlugin {
  fn on_start(&self, context: &AppContext) -> Result<()> {
    let diagnostic = Arc::new(Diagnostics::new(
      self.config.clone(),
      &context.app_info.project_dir.data_local_dir().join("diagnostics"),
    )?);
    if self.config.install_tracing {
      let layer = diagnostic
        .layer()
        .with_filter(tracing_subscriber::filter::LevelFilter::INFO);
      tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
        .context("logging is already installed; configure diagnostics with install_tracing=false and attach its layer to your subscriber")?;
    }
    if self.config.capture_panics {
      let previous = std::panic::take_hook();
      let capture = Arc::clone(&diagnostic);
      std::panic::set_hook(Box::new(move |panic| {
        let _ = capture.record("rust_panic", json!({ "panic": panic.to_string() }));
        previous(panic);
      }));
    }
    diagnostic.record("application_start", json!({ "application_id": context.app_info.project_dir.config_dir().file_name().map(|name| name.to_string_lossy()) }))?;
    lintel_lib_iced::ui_perf::set_enabled(true);
    let service = Arc::clone(&diagnostic);
    context.register_service(move || Arc::clone(&service));
    *self
      .instance
      .lock()
      .map_err(|_| anyhow::anyhow!("diagnostic plugin lock poisoned"))? = Some(Arc::clone(&diagnostic));
    let app = context.clone();
    let timeout = self.config.ui_timeout;
    context.spawn_task(TaskScope::Application, async move {
      loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let (reply, mut acknowledged) = tokio::sync::oneshot::channel();
        if app
          .send_user_event(LintelEvent::Task(Box::new(move || {
            let _ = reply.send(());
          })))
          .is_err()
        {
          break;
        }
        if tokio::time::timeout(timeout, &mut acknowledged).await.is_err() {
          let _ = diagnostic.record("ui_heartbeat_timeout", json!({ "timeout_ms": timeout.as_millis() }));
          // 一个未应答探针只保留一份，防止卡死期间堆积唤醒。
          if acknowledged.await.is_err() {
            break;
          }
        }
      }
    })?;
    Ok(())
  }

  fn on_stop(&self, context: &AppContext) {
    if let Ok(mut instance) = self.instance.lock()
      && let Some(diagnostic) = instance.take()
    {
      let _ = diagnostic.record("application_stop", Value::Null);
      diagnostic.active.store(false, Ordering::Release);
    }
    context.remove_service::<Diagnostics>();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn logs_rotate_redact_and_export_only_owned_files() {
    let directory = tempfile::tempdir().unwrap();
    let diagnostics = Diagnostics::new(
      DiagnosticConfig {
        max_file_bytes: 256,
        retained_files: 2,
        redact: vec!["test-secret".into()],
        ..Default::default()
      },
      &directory.path().join("logs"),
    )
    .unwrap();
    for _ in 0..8 {
      diagnostics.record("sample", json!({ "value": "test-secret" })).unwrap();
    }
    let export = directory.path().join("export");
    diagnostics.export(&export).unwrap();
    assert!(export.join("manifest.json").is_file());
    assert!(export.join("lintel.1.jsonl").is_file());
    for index in 0..2 {
      let content = fs::read_to_string(export.join(format!("lintel.{index}.jsonl"))).unwrap();
      assert!(!content.contains("test-secret"));
      assert!(content.contains("[REDACTED]"));
      for line in content.lines() {
        serde_json::from_str::<Value>(line).unwrap();
      }
      #[cfg(unix)]
      assert_eq!(
        fs::metadata(export.join(format!("lintel.{index}.jsonl")))
          .unwrap()
          .permissions()
          .mode()
          & 0o777,
        0o600
      );
    }
    assert!(diagnostics.export(&export).is_err());
  }

  #[test]
  fn crash_export_requires_explicit_directory_and_rejects_links() {
    let root = tempfile::tempdir().unwrap();
    let crashes = root.path().join("crashes");
    fs::create_dir_all(crashes.join("report-owned")).unwrap();
    fs::write(crashes.join("report-owned/report.dmp"), b"MDMP").unwrap();
    fs::create_dir_all(crashes.join("unrelated")).unwrap();
    fs::write(crashes.join("unrelated/report.dmp"), b"unrelated").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.path(), crashes.join("report-link")).unwrap();
    let diagnostic = Diagnostics::new(
      DiagnosticConfig {
        crash_directory: Some(crashes),
        ..Default::default()
      },
      &root.path().join("logs"),
    )
    .unwrap();
    let output = root.path().join("export");
    diagnostic.export(&output).unwrap();
    assert_eq!(
      fs::read(output.join("crashes/report-owned/report.dmp")).unwrap(),
      b"MDMP"
    );
    assert!(!output.join("crashes/unrelated").exists());
    assert!(!output.join("crashes/report-link").exists());
  }

  #[test]
  fn native_service_report_export_retains_fixed_context_without_other_run_files() {
    let root = tempfile::tempdir().unwrap();
    let crashes = root.path().join("crashes");
    let report = crashes.join("report-service");
    fs::create_dir_all(&report).unwrap();
    let metadata = json!({
      "format":"lintel-native-crash-v1", "process_id":42, "process_role":"service",
      "service":{"identity":"echo-worker", "attempt":2}
    });
    fs::write(report.join("report.json"), serde_json::to_vec(&metadata).unwrap()).unwrap();
    fs::write(report.join("report.dmp"), b"MDMPnative").unwrap();
    for name in ["helper.log", "credentials.json", "token"] {
      fs::write(report.join(name), b"must not export").unwrap();
    }
    let diagnostics = Diagnostics::new(
      DiagnosticConfig {
        crash_directory: Some(crashes),
        ..Default::default()
      },
      &root.path().join("logs"),
    )
    .unwrap();
    let output = diagnostics.export(&root.path().join("export")).unwrap();
    let exported = output.join("crashes/report-service");
    let actual: Value = serde_json::from_slice(&fs::read(exported.join("report.json")).unwrap()).unwrap();
    assert_eq!(actual, metadata);
    assert_eq!(fs::read(exported.join("report.dmp")).unwrap(), b"MDMPnative");
    assert_eq!(fs::read_dir(&exported).unwrap().count(), 2);
  }

  #[test]
  fn crashpad_export_includes_only_dump_records_and_shares_the_size_budget() {
    let root = tempfile::tempdir().unwrap();
    let crashes = root.path().join("crashes");
    fs::create_dir_all(crashes.join("pending")).unwrap();
    fs::create_dir_all(crashes.join("completed")).unwrap();
    let name = "12345678-abcd-1234-abcd-123456789abc.dmp";
    fs::write(crashes.join("pending").join(name), b"MDMPpending").unwrap();
    fs::write(crashes.join("completed").join(name), b"MDMPcompleted").unwrap();
    fs::write(crashes.join("pending/database.dat"), b"private settings").unwrap();
    fs::write(
      crashes.join("pending/12345678-abcd-1234-abcd-123456789abc.meta"),
      b"metadata",
    )
    .unwrap();
    fs::write(
      crashes.join("pending/12345678-abcd-1234-abcd-223456789abc.dmp"),
      b"other data",
    )
    .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
      crashes.join("pending").join(name),
      crashes.join("pending/12345678-abcd-1234-abcd-323456789abc.dmp"),
    )
    .unwrap();
    let diagnostic = Diagnostics::new(
      DiagnosticConfig {
        crash_directory: Some(crashes.clone()),
        ..Default::default()
      },
      &root.path().join("logs"),
    )
    .unwrap();
    let output = root.path().join("export");
    diagnostic.export(&output).unwrap();
    for state in ["pending", "completed"] {
      let directory = output.join("crashes").join(state);
      assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
      #[cfg(unix)]
      {
        assert_eq!(fs::metadata(&directory).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(
          fs::metadata(directory.join(name)).unwrap().permissions().mode() & 0o777,
          0o600
        );
      }
    }
    let limited = Diagnostics::new(
      DiagnosticConfig {
        crash_directory: Some(crashes),
        max_crash_bytes: 20,
        ..Default::default()
      },
      &root.path().join("limited-logs"),
    )
    .unwrap();
    let failed_export = root.path().join("too-large");
    assert!(limited.export(&failed_export).is_err());
    assert!(!failed_export.exists());
  }

  #[cfg(unix)]
  #[test]
  fn diagnostic_storage_is_private_from_creation_and_shared_roots_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("private");
    let diagnostics = Diagnostics::new(Default::default(), &directory).unwrap();
    diagnostics.record("test", Value::Null).unwrap();
    assert_eq!(fs::metadata(&directory).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(
      fs::metadata(directory.join("lintel.0.jsonl"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777,
      0o600
    );
    let shared = root.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Diagnostics::new(Default::default(), &shared).is_err());
  }
}
