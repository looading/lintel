use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
  fs,
  path::{Path, PathBuf},
  process::{Command, Stdio},
  time::{Duration, Instant},
};

use crate::DownloadedUpdate;

/// 已解压和验证、尚未交给安装助手的更新。
pub struct PreparedUpdate {
  inner: PreparedKind,
}

enum PreparedKind {
  Mac(MacPreparedUpdate),
  #[cfg(windows)]
  Windows(crate::windows_install::PreparedWindowsUpdate),
}

struct MacPreparedUpdate {
  root: tempfile::TempDir,
  request: Request,
}

/// 下次启动时可读取的安装结果路径。
pub struct InstallReceipt {
  pub result_path: PathBuf,
  pub installer_pid: u32,
}

#[derive(Debug, Deserialize)]
pub struct InstallOutcome {
  pub success: bool,
  pub version: String,
  pub error: Option<String>,
}

impl InstallReceipt {
  pub fn read_result(&self) -> Result<Option<InstallOutcome>> {
    match fs::read(&self.result_path) {
      Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
      Err(error) => Err(error.into()),
    }
  }
}

pub(crate) fn current_identity() -> Result<(String, String)> {
  #[cfg(target_os = "macos")]
  {
    let app = installed_application()?;
    let info = plist::Value::from_file(app.join("Contents/Info.plist"))?;
    let info = info.as_dictionary().context("invalid app metadata")?;
    let read = |name| {
      info
        .get(name)
        .and_then(plist::Value::as_string)
        .map(str::to_owned)
        .context("missing application identity")
    };
    Ok((read("CFBundleIdentifier")?, read("CFBundleShortVersionString")?))
  }
  #[cfg(windows)]
  {
    return crate::windows_install::current_identity();
  }
  #[cfg(not(any(target_os = "macos", windows)))]
  anyhow::bail!("automatic application identity currently requires macOS");
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
  parent: u32,
  candidate: PathBuf,
  installed: PathBuf,
  app_id: String,
  old_version: String,
  new_version: String,
  result: PathBuf,
  relaunch: bool,
}

pub(crate) fn prepare(download: DownloadedUpdate) -> Result<PreparedUpdate> {
  #[cfg(windows)]
  {
    return Ok(PreparedUpdate {
      inner: PreparedKind::Windows(crate::windows_install::prepare(download)?),
    });
  }
  #[cfg(not(windows))]
  {
    ensure!(
      download.release.expires_at > crate::manifest::now()?,
      "update manifest has expired"
    );
    let installed = installed_application()?;
    let parent = installed.parent().context("installed application has no parent")?;
    let root = private_stage(parent)
      .context("prepare update beside installed application; install location must be writable")?;
    let candidate = crate::archive::extract(download.archive.path(), root.path())?;
    let request = Request {
      parent: std::process::id(),
      candidate,
      installed: installed.clone(),
      app_id: download.release.app_id.clone(),
      old_version: download.settings.current.to_string(),
      new_version: download.release.version,
      result: parent.join(format!(".{}.update-result.json", download.release.app_id)),
      relaunch: true,
    };
    lintel_build::validate_macos_update(
      &request.candidate,
      &installed,
      &request.app_id,
      &request.old_version,
      &request.new_version,
    )?;
    ensure!(
      installed.join("Contents/Helpers/lintel-updater").is_file(),
      "application has no installer helper; package it with lintel bundle --updater"
    );
    Ok(PreparedUpdate {
      inner: PreparedKind::Mac(MacPreparedUpdate { root, request }),
    })
  }
}

impl PreparedUpdate {
  /// Opt in only when the candidate app calls confirm_windows_startup at business readiness.
  /// Timeout terminates only the exact child spawned by this attempt, then attempts safe rollback.
  pub fn with_windows_startup_health(mut self, options: crate::WindowsStartupHealth) -> Result<Self> {
    match &mut self.inner {
      #[cfg(windows)]
      PreparedKind::Windows(prepared) => prepared.startup_health(options)?,
      _ => {
        let _ = options;
        anyhow::bail!("Windows startup health requires a managed Windows update");
      }
    }
    Ok(self)
  }
  /// 交接后调用应用自身的正常退出流程；助手不会强制结束进程。
  pub async fn install_on_exit(self, relaunch: bool) -> Result<InstallReceipt> {
    tokio::task::spawn_blocking(move || match self.inner {
      PreparedKind::Mac(prepared) => prepared.handoff(relaunch),
      #[cfg(windows)]
      PreparedKind::Windows(prepared) => prepared.handoff(relaunch),
    })
    .await
    .context("installer handoff task failed")?
  }
}

impl MacPreparedUpdate {
  fn handoff(mut self, relaunch: bool) -> Result<InstallReceipt> {
    self.request.relaunch = relaunch;
    let plan = self.root.path().join("request.json");
    fs::write(&plan, serde_json::to_vec(&self.request)?)?;
    let log = fs::File::create(self.root.path().join("installer.log"))?;
    let mut child = Command::new(self.request.installed.join("Contents/Helpers/lintel-updater"))
      .arg("--apply")
      .arg(&plan)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(log)
      .spawn()
      .context("launch isolated update installer")?;
    let stdout = child.stdout.take().context("installer has no readiness pipe")?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
      use std::io::BufRead;
      let mut line = String::new();
      let result = std::io::BufReader::new(stdout).read_line(&mut line).map(|_| line);
      let _ = sender.send(result);
    });
    let ready = receiver.recv_timeout(Duration::from_secs(120));
    if !matches!(ready, Ok(Ok(ref line)) if line.trim() == "LINTEL_UPDATER_READY") {
      let _ = child.kill();
      let _ = child.wait();
      anyhow::bail!(
        "installer did not accept update handoff: {}",
        fs::read_to_string(self.root.path().join("installer.log")).unwrap_or_default()
      );
    }
    let result_path = self.request.result.clone();
    let installer_pid = child.id();
    if let Err(error) = fs::remove_file(&result_path) {
      if error.kind() != std::io::ErrorKind::NotFound {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error).context("reset prior installer outcome");
      }
    }
    // The helper owns this private directory after a successful spawn.
    let _ = self.root.keep();
    Ok(InstallReceipt {
      result_path,
      installer_pid,
    })
  }
}

/// 独立助手入口；应用业务无须分发安装命令或自行退出。
pub fn run_installer_helper() -> Result<()> {
  let args: Vec<_> = std::env::args_os().collect();
  if args.len() == 3 && args[1] == "--apply-windows" {
    return crate::windows_install::run_helper(Path::new(&args[2]));
  }
  ensure!(
    args.len() == 3 && args[1] == "--apply",
    "usage: lintel-updater --apply PRIVATE_REQUEST_JSON"
  );
  let path = Path::new(&args[2]);
  let root = path.parent().context("update request has no parent")?.canonicalize()?;
  ensure!(
    root
      .file_name()
      .is_some_and(|n| n.to_string_lossy().starts_with(".lintel-update-")),
    "invalid private update directory"
  );
  ensure!(
    fs::symlink_metadata(path)?.is_file() && fs::metadata(path)?.len() <= 65536,
    "invalid installer request"
  );
  #[cfg(target_os = "macos")]
  {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(&root)?;
    // SAFETY: getuid has no input pointers and returns this process's real user ID.
    ensure!(
      metadata.is_dir() && metadata.uid() == unsafe { libc::getuid() } && metadata.mode() & 0o077 == 0,
      "installer request directory must be private and owned by this user"
    );
  }
  let request: Request = serde_json::from_slice(&fs::read(path)?)?;
  ensure!(
    request.app_id.contains('.')
      && request
        .app_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b)),
    "invalid update application ID"
  );
  let outcome = (|| {
    ensure!(
      request.candidate.canonicalize()?.starts_with(&root),
      "candidate escapes private update directory"
    );
    ensure!(
      request.installed.parent() == root.parent(),
      "update staging must be beside the installed app"
    );
    ensure!(
      request.result
        == request
          .installed
          .parent()
          .context("app has no parent")?
          .join(format!(".{}.update-result.json", request.app_id)),
      "unexpected update result path"
    );
    ensure!(
      semver::Version::parse(&request.new_version)?
        .cmp_precedence(&semver::Version::parse(&request.old_version)?)
        .is_gt(),
      "update must increase version"
    );
    lintel_build::validate_macos_update(
      &request.candidate,
      &request.installed,
      &request.app_id,
      &request.old_version,
      &request.new_version,
    )?;
    use std::io::Write;
    println!("LINTEL_UPDATER_READY");
    std::io::stdout().flush()?;
    wait_for_exit(&request)?;
    lintel_build::install_macos_update_guarded(
      &request.candidate,
      &request.installed,
      &request.app_id,
      &request.old_version,
      &request.new_version,
      request.relaunch,
      || {
        ensure!(
          processes_in_bundle(&request.installed)?.is_empty(),
          "application or Helper was reopened during installation"
        );
        Ok(())
      },
    )
  })();
  // Write the outcome atomically, even on failure, for the next application launch.
  let value = serde_json::json!({"success": outcome.is_ok(), "version": request.new_version,
    "error": outcome.as_ref().err().map(|error| format!("{error:#}"))});
  if let Some(parent) = request.result.parent() {
    let mut result = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    result.write_all(&serde_json::to_vec_pretty(&value)?)?;
    result.as_file().sync_all()?;
    result.persist(&request.result)?;
  }
  let _ = fs::remove_dir_all(&root);
  outcome
}

fn private_stage(parent: &Path) -> std::io::Result<tempfile::TempDir> {
  let mut builder = tempfile::Builder::new();
  builder.prefix(".lintel-update-");
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    builder.permissions(fs::Permissions::from_mode(0o700));
  }
  builder.tempdir_in(parent)
}

#[cfg(all(test, unix))]
mod tests {
  #[test]
  fn handoff_directory_is_private_at_creation() {
    use std::os::unix::fs::PermissionsExt;
    let parent = tempfile::tempdir().unwrap();
    let stage = super::private_stage(parent.path()).unwrap();
    assert_eq!(
      std::fs::metadata(stage.path()).unwrap().permissions().mode() & 0o777,
      0o700
    );
  }
}

#[cfg(target_os = "macos")]
fn installed_application() -> Result<PathBuf> {
  let executable = std::env::current_exe()?.canonicalize()?;
  let app = executable
    .ancestors()
    .find(|p| p.extension().is_some_and(|e| e == "app"))
    .context("updater must run inside an installed .app")?;
  let info = plist::Value::from_file(app.join("Contents/Info.plist"))?;
  let name = info
    .as_dictionary()
    .and_then(|d| d.get("CFBundleExecutable"))
    .and_then(plist::Value::as_string)
    .context("app has no executable name")?;
  ensure!(
    app.join("Contents/MacOS").join(name).canonicalize()? == executable,
    "only the main application can schedule updates"
  );
  Ok(app.to_path_buf())
}

#[cfg(not(target_os = "macos"))]
fn installed_application() -> Result<PathBuf> {
  anyhow::bail!("update installation currently requires macOS");
}

#[cfg(target_os = "macos")]
fn wait_for_exit(request: &Request) -> Result<()> {
  let deadline = Instant::now() + Duration::from_secs(120);
  loop {
    // SAFETY: signal 0 performs only a liveness check; no signal is delivered.
    let parent_alive = unsafe { libc::kill(request.parent as i32, 0) } == 0;
    let active = processes_in_bundle(&request.installed)?;
    if !parent_alive && active.is_empty() {
      return Ok(());
    }
    ensure!(
      Instant::now() < deadline,
      "application or Helper did not exit within 120 seconds; original bundle retained"
    );
    std::thread::sleep(Duration::from_millis(100));
  }
}

#[cfg(not(target_os = "macos"))]
fn wait_for_exit(_: &Request) -> Result<()> {
  anyhow::bail!("update installation currently requires macOS");
}

#[cfg(target_os = "macos")]
fn processes_in_bundle(bundle: &Path) -> Result<Vec<i32>> {
  use std::os::unix::ffi::OsStrExt;
  #[link(name = "proc")]
  unsafe extern "C" {
    fn proc_listallpids(buffer: *mut std::ffi::c_void, size: i32) -> i32;
    fn proc_pidpath(pid: i32, buffer: *mut std::ffi::c_void, size: u32) -> i32;
  }
  // SAFETY: a null buffer with size zero asks libproc for an estimated PID count.
  let count = unsafe { proc_listallpids(std::ptr::null_mut(), 0) };
  ensure!(count > 0, "cannot enumerate running processes");
  let mut pids = vec![0i32; count as usize + 1024];
  // SAFETY: the initialized vector provides exactly the writable byte capacity passed to libproc.
  let used = unsafe { proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as i32) };
  ensure!(
    used >= 0 && (used as usize) < pids.len(),
    "process enumeration failed or was truncated"
  );
  pids.truncate(used as usize);
  let mut active = Vec::new();
  for pid in pids {
    if pid <= 0 || pid as u32 == std::process::id() {
      continue;
    }
    let mut bytes = [0u8; 4096];
    // SAFETY: the byte buffer is initialized, writable, and lives throughout the call.
    let length = unsafe { proc_pidpath(pid, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
    if length > 0 {
      let end = bytes
        .iter()
        .position(|b| *b == 0)
        .context("process path is not terminated")?;
      if Path::new(std::ffi::OsStr::from_bytes(&bytes[..end])).starts_with(bundle) {
        active.push(pid);
      }
    }
  }
  Ok(active)
}

#[cfg(not(target_os = "macos"))]
fn processes_in_bundle(_: &Path) -> Result<Vec<i32>> {
  anyhow::bail!("update installation currently requires macOS");
}
