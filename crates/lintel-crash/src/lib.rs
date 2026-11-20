//! Local native crash capture using Embark's crash-handler and an out-of-process minidumper.
//!
//! Install once, before starting the UI or third-party runtimes. Keep the returned guard alive.
//! The helper writes only to a fresh private directory under the application's chosen report root.

use std::{
  fs::{self, File, OpenOptions},
  io::{Read, Write},
  path::{Path, PathBuf},
  process::{Child, ChildStdin, Command, ExitStatus, Stdio},
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
  },
  thread,
  time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use crash_handler::{CrashEventResult, CrashHandler};
use minidumper::{Client, LoopAction, MinidumpBinary, Server, ServerHandler, SocketName};

mod snapshot;
pub use snapshot::{SnapshotError, capture_snapshot, capture_snapshot_until, snapshot_helper_entry};

pub const HELPER_SWITCH: &str = "--lintel-crash-helper";
const AUTHENTICATE: u32 = 1;
const TOKEN_BYTES: usize = 32;
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// App-owned report storage and the bundled helper executable. No network transport is present.
#[derive(Clone, Debug)]
pub struct CrashConfig {
  pub directory: PathBuf,
  pub helper: PathBuf,
  pub application_id: String,
  pub version: String,
  pub startup_timeout: Duration,
  pub shutdown_timeout: Duration,
  pub dump_timeout: Duration,
}

impl CrashConfig {
  pub fn new(directory: impl Into<PathBuf>, helper: impl Into<PathBuf>) -> Self {
    Self {
      directory: directory.into(),
      helper: helper.into(),
      application_id: "lintel.application".into(),
      version: env!("CARGO_PKG_VERSION").into(),
      startup_timeout: Duration::from_secs(5),
      shutdown_timeout: Duration::from_secs(2),
      dump_timeout: Duration::from_secs(20),
    }
  }
}

/// 服务自行声明的固定身份与监管启动次数；不包含通信端点或业务请求。
/// 该上下文用于关联本地报告，不代表经过认证的服务身份。
#[derive(Clone, Debug)]
pub struct ServiceCrashContext {
  identity: String,
  attempt: u64,
}

impl ServiceCrashContext {
  pub fn new(identity: impl Into<String>, attempt: u64) -> Result<Self> {
    let identity = identity.into();
    ensure!(
      !identity.is_empty() && identity.len() <= 256 && !identity.chars().any(char::is_control) && attempt > 0,
      "valid service identity and positive supervisor attempt required"
    );
    Ok(Self { identity, attempt })
  }

  pub fn identity(&self) -> &str {
    &self.identity
  }

  pub fn attempt(&self) -> u64 {
    self.attempt
  }
}

fn report_metadata(config: &CrashConfig, service: Option<&ServiceCrashContext>) -> serde_json::Value {
  let mut metadata = serde_json::json!({
    "format": "lintel-native-crash-v1", "application_id": config.application_id,
    "version": config.version, "process_id": std::process::id(),
    "os": std::env::consts::OS, "architecture": std::env::consts::ARCH,
  });
  if let Some(service) = service {
    metadata["process_role"] = "service".into();
    metadata["service"] = serde_json::json!({"identity": service.identity(), "attempt": service.attempt()});
  }
  metadata
}

struct ActiveReporter;
impl ActiveReporter {
  fn acquire() -> Result<Self> {
    ensure!(
      ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok(),
      "a native crash reporter is already installed in this process"
    );
    Ok(Self)
  }
}
impl Drop for ActiveReporter {
  fn drop(&mut self) {
    ACTIVE.store(false, Ordering::Release);
  }
}

/// A process-lifetime native crash hook and its supervised helper process.
/// Drop detaches the hook, closes the helper's control pipe, then waits for bounded cleanup.
/// Other libraries must not replace the process crash handler while this guard is alive.
pub struct CrashReporter {
  handler: Option<CrashHandler>,
  helper: HelperProcess,
  report: ReportDirectory,
  _ipc: tempfile::TempDir,
  _active: ActiveReporter,
}

impl CrashReporter {
  pub fn start(config: CrashConfig) -> Result<Self> {
    Self::start_inner(config, None)
  }

  /// 在原有本地报告中增加固定服务上下文，复用同一个捕获及 helper 生命周期。
  pub fn start_service(config: CrashConfig, service: ServiceCrashContext) -> Result<Self> {
    Self::start_inner(config, Some(service))
  }

  fn start_inner(config: CrashConfig, service: Option<ServiceCrashContext>) -> Result<Self> {
    ensure!(
      config.directory.is_absolute() && config.helper.is_absolute(),
      "crash report and helper paths must be absolute"
    );
    ensure!(
      !config.startup_timeout.is_zero() && config.startup_timeout <= Duration::from_secs(30),
      "invalid crash helper startup timeout"
    );
    ensure!(
      !config.shutdown_timeout.is_zero() && config.shutdown_timeout <= Duration::from_secs(10),
      "invalid crash helper shutdown timeout"
    );
    ensure!(
      !config.dump_timeout.is_zero() && config.dump_timeout <= Duration::from_secs(120),
      "invalid native dump timeout"
    );
    ensure!(
      !config.application_id.is_empty() && config.application_id.len() <= 253 && config.version.len() <= 128,
      "invalid crash reporter application metadata"
    );
    let executable = config
      .helper
      .canonicalize()
      .context("cannot find the bundled crash helper")?;
    ensure!(executable.is_file(), "crash helper is not a regular executable file");
    let active = ActiveReporter::acquire()?;
    fs::create_dir_all(&config.directory).context("cannot create the application's crash report directory")?;
    let root = config.directory.canonicalize()?;
    let report = ReportDirectory(private_tempdir(&root, "report-")?.keep());
    ensure_private_directory(&report.0)?;
    let metadata = report_metadata(&config, service.as_ref());
    write_private(&report.0.join("report.json"), &serde_json::to_vec_pretty(&metadata)?)?;
    // Unix-domain sockets have a short path limit. Keep only the private IPC directory
    // in the system temporary root; minidumps remain under the app-owned report root.
    #[cfg(unix)]
    let ipc_root = PathBuf::from("/tmp");
    #[cfg(not(unix))]
    let ipc_root = std::env::temp_dir();
    let ipc = private_tempdir(&ipc_root, "lc-")?;
    ensure_private_directory(ipc.path())?;
    let endpoint = ipc.path().join("s");
    ensure!(
      endpoint.to_str().is_some_and(|path| path.len() < 104),
      "crash IPC endpoint path exceeds the platform socket limit"
    );
    let mut token = [0_u8; TOKEN_BYTES];
    getrandom::fill(&mut token)
      .map_err(|error| anyhow::anyhow!("cannot generate crash helper authentication: {error}"))?;
    let log = private_file(&report.0.join("helper.log"))?;
    let child = Command::new(executable)
      .arg(HELPER_SWITCH)
      .arg(&endpoint)
      .arg(&report.0)
      .arg(std::process::id().to_string())
      .arg(config.dump_timeout.as_millis().to_string())
      .stdin(Stdio::piped())
      .stdout(Stdio::null())
      .stderr(Stdio::from(log))
      .spawn()
      .context("cannot launch the bundled crash helper")?;
    let mut helper = HelperProcess {
      child,
      input: None,
      timeout: config.shutdown_timeout,
      stopping: false,
    };
    helper.input = helper.child.stdin.take();
    helper
      .input
      .as_mut()
      .context("crash helper control pipe is unavailable")?
      .write_all(&token)?;
    let client = connect_helper(&endpoint, token, config.startup_timeout, &mut helper)?;
    // SAFETY: Everything required by the callback is created before installation.
    // The callback only borrows the supplied live CrashContext and invokes the upstream
    // minidumper Client's intended crash-context IPC path. It allocates no application
    // data, takes no application locks, logs nothing, never unwraps, and never spawns.
    // The owned Client remains captured until CrashHandler detaches. request_dump is
    // synchronous, keeping Windows exception pointers valid until the helper replies.
    // The independent helper watchdog bounds dump work if the writer stops responding.
    #[allow(unsafe_code)]
    let callback = unsafe {
      crash_handler::make_crash_event(move |context| CrashEventResult::Handled(client.request_dump(context).is_ok()))
    };
    let handler = CrashHandler::attach(callback).context("cannot install the native crash handler")?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    handler.set_ptracer(Some(helper.child.id()));
    Ok(Self {
      handler: Some(handler),
      helper,
      report,
      _ipc: ipc,
      _active: active,
    })
  }

  pub fn directory(&self) -> &Path {
    &self.report.0
  }
  pub fn helper_pid(&self) -> u32 {
    self.helper.child.id()
  }

  /// Explicit bounded shutdown; Drop performs the same cleanup as a best effort fallback.
  pub fn shutdown(mut self) -> Result<()> {
    drop(self.handler.take());
    self.helper.stop()
  }
}

impl Drop for CrashReporter {
  fn drop(&mut self) {
    // The socket captured by the callback must remain live until the handler is detached.
    drop(self.handler.take());
    let _ = self.helper.stop();
  }
}

struct ReportDirectory(PathBuf);
impl Drop for ReportDirectory {
  fn drop(&mut self) {
    // Only this fresh, private run directory is removable. Retain dumps and failures.
    if !self.0.join("report.dmp").exists() && !self.0.join("failure.txt").exists() {
      let _ = fs::remove_dir_all(&self.0);
    }
  }
}

struct HelperProcess {
  child: Child,
  input: Option<ChildStdin>,
  timeout: Duration,
  stopping: bool,
}
impl HelperProcess {
  fn stop(&mut self) -> Result<()> {
    // Explicit shutdown, CrashReporter::drop, and field drop share one deadline.
    if std::mem::replace(&mut self.stopping, true) {
      return Ok(());
    }
    drop(self.input.take());
    if wait_until(&mut self.child, Instant::now() + self.timeout)?.is_some() {
      return Ok(());
    }
    self
      .child
      .kill()
      .context("cannot stop the crash helper after its shutdown deadline")?;
    ensure!(
      wait_until(&mut self.child, Instant::now() + Duration::from_secs(1))?.is_some(),
      "crash helper did not exit after being terminated"
    );
    Ok(())
  }
}
impl Drop for HelperProcess {
  fn drop(&mut self) {
    let _ = self.stop();
  }
}

fn wait_until(child: &mut Child, deadline: Instant) -> Result<Option<ExitStatus>> {
  loop {
    if let Some(status) = child.try_wait()? {
      return Ok(Some(status));
    }
    if Instant::now() >= deadline {
      return Ok(None);
    }
    thread::sleep(Duration::from_millis(10));
  }
}

fn connect_helper(
  endpoint: &Path,
  token: [u8; TOKEN_BYTES],
  timeout: Duration,
  helper: &mut HelperProcess,
) -> Result<Client> {
  let (tx, rx) = mpsc::sync_channel(1);
  let endpoint = endpoint.to_path_buf();
  // Some platforms include a synchronous acknowledgement in Client::with_name.
  // Supervise the whole connect/auth/ping sequence, rather than just connection retries.
  thread::Builder::new()
    .name("lintel-crash-connect".into())
    .spawn(move || {
      let started = Instant::now();
      let result = loop {
        match Client::with_name(SocketName::Path(&endpoint)) {
          Ok(client) => {
            break client
              .send_message(AUTHENTICATE, token)
              .and_then(|()| client.ping())
              .map(|()| client);
          }
          Err(error) if started.elapsed() >= timeout => break Err(error),
          Err(_) => thread::sleep(Duration::from_millis(10)),
        }
      };
      let _ = tx.send(result);
    })?;
  let deadline = Instant::now() + timeout;
  loop {
    match rx.recv_timeout(Duration::from_millis(10)) {
      Ok(result) => return result.context("cannot connect to the crash helper"),
      Err(mpsc::RecvTimeoutError::Disconnected) => anyhow::bail!("crash helper connection worker stopped"),
      Err(mpsc::RecvTimeoutError::Timeout) => {}
    }
    ensure!(
      helper.child.try_wait()?.is_none(),
      "crash helper exited before initialization completed"
    );
    ensure!(Instant::now() < deadline, "crash helper initialization timed out");
  }
}

fn private_file(path: &Path) -> std::io::Result<File> {
  let mut options = OpenOptions::new();
  options.write(true).create_new(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  options.open(path)
}

fn private_tempdir(root: &Path, prefix: &str) -> std::io::Result<tempfile::TempDir> {
  let mut builder = tempfile::Builder::new();
  builder.prefix(prefix).rand_bytes(12);
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    builder.permissions(fs::Permissions::from_mode(0o700));
  }
  builder.tempdir_in(root)
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
  let mut file = private_file(path)?;
  file.write_all(bytes)?;
  file.sync_all()?;
  Ok(())
}
fn ensure_private_directory(path: &Path) -> Result<()> {
  let metadata = fs::symlink_metadata(path)?;
  ensure!(
    metadata.is_dir() && !metadata.file_type().is_symlink(),
    "crash directory must be a real directory"
  );
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    ensure!(
      metadata.permissions().mode() & 0o077 == 0,
      "crash directory must be private to its owner"
    );
  }
  Ok(())
}

/// Handles the private helper switch and returns true after the helper loop exits.
/// A host using its own executable as the helper must call this before UI/CEF startup
/// and return from main immediately when it returns true.
pub fn helper_entry() -> Result<bool> {
  let args: Vec<_> = std::env::args_os().skip(1).collect();
  if args.first().is_none_or(|argument| argument != HELPER_SWITCH) {
    return Ok(false);
  }
  ensure!(args.len() == 5, "invalid crash helper invocation");
  let endpoint = PathBuf::from(&args[1]);
  let directory = PathBuf::from(&args[2]);
  let parent: u32 = args[3].to_str().context("invalid crash parent PID")?.parse()?;
  let timeout_ms: u64 = args[4].to_str().context("invalid native dump timeout")?.parse()?;
  ensure!(
    parent > 0 && timeout_ms > 0 && timeout_ms <= 120_000,
    "invalid crash helper limits"
  );
  ensure!(
    endpoint.is_absolute() && endpoint.file_name().is_some_and(|name| name == "s"),
    "invalid crash IPC endpoint"
  );
  ensure_private_directory(endpoint.parent().context("missing crash IPC directory")?)?;
  ensure!(directory.is_absolute(), "crash report path must be absolute");
  ensure_private_directory(&directory)?;
  let metadata: serde_json::Value = serde_json::from_slice(&fs::read(directory.join("report.json"))?)?;
  ensure!(
    metadata["format"] == "lintel-native-crash-v1" && metadata["process_id"] == parent,
    "crash helper does not match the report's application instance"
  );
  let mut token = [0_u8; TOKEN_BYTES];
  std::io::stdin()
    .read_exact(&mut token)
    .context("missing crash helper authentication")?;
  run_helper(endpoint, directory, token, Duration::from_millis(timeout_ms))?;
  Ok(true)
}

fn run_helper(endpoint: PathBuf, directory: PathBuf, token: [u8; TOKEN_BYTES], timeout: Duration) -> Result<()> {
  let shutdown = Arc::new(AtomicBool::new(false));
  let control_shutdown = shutdown.clone();
  thread::Builder::new()
    .name("lintel-crash-control".into())
    .spawn(move || {
      // EOF is the only control operation. The inherited private pipe identifies the
      // supervising app and also closes automatically if the app dies without a hook.
      let mut byte = [0_u8; 1];
      let _ = std::io::stdin().read(&mut byte);
      control_shutdown.store(true, Ordering::Release);
    })?;
  let started = Arc::new(Mutex::new(None::<DumpStart>));
  let watcher_started = started.clone();
  let watcher_shutdown = shutdown.clone();
  thread::Builder::new()
    .name("lintel-crash-deadline".into())
    .spawn(move || {
      while !watcher_shutdown.load(Ordering::Acquire) {
        if watcher_started.lock().is_ok_and(|started| {
          started
            .as_ref()
            .is_some_and(|started| started.instant.elapsed() > timeout)
        }) {
          // This is the isolated helper process, never the monitored application.
          std::process::exit(3);
        }
        thread::sleep(Duration::from_millis(50));
      }
    })?;
  let handler = DumpHandler {
    directory,
    token,
    authenticated: AtomicBool::new(false),
    shutdown: shutdown.clone(),
    started,
  };
  let result = Server::with_name(SocketName::Path(&endpoint))?.run(Box::new(handler), &shutdown, None);
  shutdown.store(true, Ordering::Release);
  let _ = fs::remove_file(&endpoint);
  if let Some(parent) = endpoint.parent() {
    let _ = fs::remove_dir(parent);
  }
  result.context("native minidump helper failed")
}

struct DumpHandler {
  directory: PathBuf,
  token: [u8; TOKEN_BYTES],
  authenticated: AtomicBool,
  shutdown: Arc<AtomicBool>,
  started: Arc<Mutex<Option<DumpStart>>>,
}

struct DumpStart {
  instant: Instant,
  captured_at_unix_ms: Option<u64>,
}

fn write_capture_time(directory: &Path, captured_at: u64) -> Result<()> {
  let mut sidecar = tempfile::NamedTempFile::new_in(directory)?;
  serde_json::to_writer(&mut sidecar, &serde_json::json!({"captured_at_unix_ms": captured_at}))?;
  sidecar.as_file().sync_all()?;
  sidecar.persist(directory.join("capture.json"))?;
  Ok(())
}

impl ServerHandler for DumpHandler {
  fn create_minidump_file(&self) -> std::io::Result<(File, PathBuf)> {
    if !self.authenticated.load(Ordering::Acquire) {
      return Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "unauthenticated crash client",
      ));
    }
    let instant = Instant::now();
    let captured_at_unix_ms = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .ok()
      .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok());
    *self
      .started
      .lock()
      .map_err(|_| std::io::Error::other("dump deadline lock poisoned"))? = Some(DumpStart {
      instant,
      captured_at_unix_ms,
    });
    let path = self.directory.join("report.dmp");
    Ok((private_file(&path)?, path))
  }

  fn on_minidump_created(&self, result: std::result::Result<MinidumpBinary, minidumper::Error>) -> LoopAction {
    let result = result.map_err(anyhow::Error::from).and_then(|mut dump| {
      dump.file.flush()?;
      dump.file.sync_all()?;
      ensure!(dump.file.metadata()?.len() >= 32, "native minidump is truncated");
      let captured_at = self
        .started
        .lock()
        .ok()
        .and_then(|started| started.as_ref().and_then(|started| started.captured_at_unix_ms));
      if let Some(captured_at) = captured_at {
        // Optional metadata must not keep an otherwise complete dump from being published.
        let _ = write_capture_time(&self.directory, captured_at);
      }
      write_private(&self.directory.join("completed"), b"lintel-native-crash-v1\n")
    });
    if let Err(error) = result {
      let _ = write_private(&self.directory.join("failure.txt"), format!("{error:#}\n").as_bytes());
    }
    LoopAction::Exit
  }

  fn on_message(&self, kind: u32, buffer: Vec<u8>) {
    if kind == AUTHENTICATE && buffer.as_slice() == self.token {
      self.authenticated.store(true, Ordering::Release);
    } else {
      self.shutdown.store(true, Ordering::Release);
    }
  }
  fn on_client_connected(&self, clients: usize) -> LoopAction {
    if clients == 1 {
      LoopAction::Continue
    } else {
      LoopAction::Exit
    }
  }
  fn on_client_disconnected(&self, clients: usize) -> LoopAction {
    if clients == 0 {
      LoopAction::Exit
    } else {
      LoopAction::Continue
    }
  }
}

#[cfg(test)]
mod service_context_tests {
  use super::*;

  #[test]
  fn service_context_rejects_unbounded_or_invalid_metadata() {
    for identity in [String::new(), "worker\nsecret".into(), "w".repeat(257)] {
      assert!(ServiceCrashContext::new(identity, 1).is_err());
    }
    assert!(ServiceCrashContext::new("echo-worker", 0).is_err());
    let context = ServiceCrashContext::new("w".repeat(256), u64::MAX).unwrap();
    assert_eq!(context.identity().len(), 256);
    assert_eq!(context.attempt(), u64::MAX);
  }

  #[test]
  fn native_metadata_stays_compatible_and_service_metadata_has_only_fixed_context() {
    // 显式字面量保持原 CrashConfig 字段形状的编译兼容。
    let config = CrashConfig {
      directory: PathBuf::from("/reports"),
      helper: PathBuf::from("/helpers/crash"),
      application_id: "dev.example.services".into(),
      version: "1.2.3".into(),
      startup_timeout: Duration::from_secs(5),
      shutdown_timeout: Duration::from_secs(2),
      dump_timeout: Duration::from_secs(20),
    };
    let base = report_metadata(&config, None);
    assert_eq!(base.as_object().unwrap().len(), 6);
    assert!(base.get("service").is_none());
    let context = ServiceCrashContext::new("echo-worker", 2).unwrap();
    let service = report_metadata(&config, Some(&context));
    for (name, value) in base.as_object().unwrap() {
      assert_eq!(&service[name], value);
    }
    assert_eq!(service.as_object().unwrap().len(), 8);
    assert_eq!(service["process_role"], "service");
    assert_eq!(
      service["service"],
      serde_json::json!({"identity":"echo-worker", "attempt":2})
    );
    for excluded in ["generation", "endpoint", "token", "args", "environment"] {
      assert!(service.get(excluded).is_none());
      assert!(service["service"].get(excluded).is_none());
    }
  }
}
