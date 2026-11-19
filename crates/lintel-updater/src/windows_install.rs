//! Managed per-user Windows installs: immutable versions and a stable authenticated launcher.

use std::{
  ffi::OsString,
  fs,
  io::Write,
  path::{Path, PathBuf},
  process::{Command, Stdio},
  time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use lintel_build::{WindowsBundleInfo, WindowsProtocolRegistration};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[cfg(windows)]
#[path = "windows_native.rs"]
mod native;

const META: &str = "lintel-install.json";
const ACTIVE: &str = "active.json";
const JOURNAL: &str = "pending-update.json";
const RESULT: &str = "last-update.json";

#[path = "windows_maintenance.rs"]
mod maintenance;
#[path = "windows_versions.rs"]
mod versions;
pub use maintenance::{
  WindowsMaintenanceReport, WindowsRetainedPath, inspect_windows_installation, prune_windows_versions,
  uninstall_windows_app,
};
#[path = "windows_health.rs"]
mod health;
pub use health::{WindowsStartupHealth, confirm_windows_startup};
#[path = "windows_distribution_install.rs"]
mod distribution;
pub use distribution::{install_verified_windows_distribution, install_windows_distribution};

/// Initial installation is explicit and pins the Ed25519 key used by subsequent releases.
pub struct WindowsInstallOptions {
  pub bundle: PathBuf,
  pub install_root: Option<PathBuf>,
  pub public_key: String,
  pub launcher: PathBuf,
  pub installer: PathBuf,
  pub protocol_schemes: Vec<String>,
  pub channel: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Installation {
  schema: u32,
  installation_id: String,
  app_id: String,
  target: String,
  public_key: String,
  launcher_sha256: String,
  installer_sha256: String,
  protocol_schemes: Vec<String>,
  channel: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActiveVersion {
  directory: String,
  manifest_sha256: String,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingSwitch {
  installation_id: String,
  old: ActiveVersion,
  new: ActiveVersion,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  health: Option<health::Attempt>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
  schema: u32,
  parent: u32,
  installation_id: String,
  old: ActiveVersion,
  new_version: String,
  relaunch: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  health: Option<WindowsStartupHealth>,
}

pub(crate) struct PreparedWindowsUpdate {
  root: PathBuf,
  stage: tempfile::TempDir,
  request: Request,
}

/// Install only into the current user's LocalAppData namespace; never requests elevation.
pub fn install_windows_app(options: &WindowsInstallOptions) -> Result<PathBuf> {
  install_windows_app_checked(options, |_| Ok(()))
}

// Shared initial-install protection; the distribution adapter may veto an overlapping fresh
// destination before any directory creation. Existing callers retain exactly the same guard path.
fn install_windows_app_checked(
  options: &WindowsInstallOptions,
  check_root: impl Fn(&Path) -> Result<()>,
) -> Result<PathBuf> {
  #[cfg(windows)]
  {
    let info = lintel_build::validate_windows_bundle(&options.bundle)?;
    crate::manifest::verifying_key(&options.public_key)?;
    let root = native::create_install_root_checked(options.install_root.as_deref(), &info.identifier, &check_root)?;
    let result = check_root(&root).and_then(|_| install_files(options, &root));
    if let Err(error) = result {
      // This call created the private root and has not returned it to an application yet.
      let cleanup = fs::remove_dir_all(&root);
      return Err(error).context(format!("initial Windows installation failed; cleanup={cleanup:?}"));
    }
    Ok(root)
  }
  #[cfg(not(windows))]
  {
    let _ = (options, check_root);
    bail!("native per-user Windows installation requires Windows");
  }
}

// The native caller validates/canonicalizes the existing ancestor and every new component first.
// Keep veto ordering cross-host testable: no create callback may run before the full target passes.
fn create_checked_install_tail(
  ancestor: &Path,
  tail: &[OsString],
  check_root: impl FnOnce(&Path) -> Result<()>,
  mut create: impl FnMut(&Path, bool) -> Result<()>,
) -> Result<PathBuf> {
  ensure!(!tail.is_empty(), "installation root must have a new tail");
  let candidate = tail.iter().fold(ancestor.to_owned(), |mut path, component| {
    path.push(component);
    path
  });
  check_root(&candidate)?;
  let mut current = ancestor.to_owned();
  for (index, component) in tail.iter().enumerate() {
    current.push(component);
    create(&current, index + 1 == tail.len())?;
  }
  Ok(current)
}

fn install_files(options: &WindowsInstallOptions, root: &Path) -> Result<()> {
  let info = lintel_build::validate_windows_bundle(&options.bundle)?;
  let version = semver::Version::parse(&info.version)?;
  ensure!(
    !options.channel.is_empty()
      && options
        .channel
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
    "invalid installation update channel"
  );
  ensure!(
    options.channel != "stable" || version.pre.is_empty(),
    "stable installation cannot start with a prerelease"
  );
  let key = crate::manifest::verifying_key(&options.public_key)?;
  let launcher_sha256 = lintel_build::validate_windows_tool(&options.launcher, &info.target)?;
  let installer_sha256 = lintel_build::validate_windows_tool(&options.installer, &info.target)?;
  let mut nonce = [0_u8; 32];
  getrandom::fill(&mut nonce).map_err(|error| anyhow::anyhow!("create installation ID: {error}"))?;
  let protocols = if options.protocol_schemes.is_empty() {
    info.protocol_schemes.clone()
  } else {
    options.protocol_schemes.clone()
  };
  let installation = Installation {
    schema: 1,
    installation_id: nonce.iter().map(|byte| format!("{byte:02x}")).collect(),
    app_id: info.identifier.clone(),
    target: info.target.clone(),
    public_key: STANDARD.encode(key.as_bytes()),
    launcher_sha256,
    installer_sha256,
    protocol_schemes: protocols,
    channel: options.channel.clone(),
  };
  fs::create_dir(root.join("tools"))?;
  fs::create_dir(root.join("versions"))?;
  fs::copy(&options.launcher, root.join("lintel-launcher.exe"))?;
  fs::copy(&options.installer, root.join("tools/lintel-updater.exe"))?;
  #[cfg(windows)]
  {
    native::protect_new(&root.join("tools"))?;
    native::protect_new(&root.join("versions"))?;
    native::protect_new(&root.join("lintel-launcher.exe"))?;
    native::protect_new(&root.join("tools/lintel-updater.exe"))?;
  }
  ensure!(
    lintel_build::validate_windows_tool(&root.join("lintel-launcher.exe"), &info.target)?
      == installation.launcher_sha256,
    "launcher changed while installing"
  );
  ensure!(
    lintel_build::validate_windows_tool(&root.join("tools/lintel-updater.exe"), &info.target)?
      == installation.installer_sha256,
    "installer changed while installing"
  );
  let version = lintel_build::copy_windows_bundle(&options.bundle, &root.join("versions"))?;
  protect_version(&version)?;
  let active = ActiveVersion {
    directory: version
      .file_name()
      .context("version has no directory name")?
      .to_str()
      .context("version directory is not Unicode")?
      .to_owned(),
    manifest_sha256: info.manifest_sha256,
  };
  write_json(&root.join(META), &installation)?;
  write_json(&root.join(ACTIVE), &active)?;
  versions::initial(
    root,
    &installation,
    &active,
    &lintel_build::validate_windows_bundle(&version)?,
  )?;
  #[cfg(windows)]
  if !installation.protocol_schemes.is_empty() {
    let registration = WindowsProtocolRegistration::prepare(
      &installation.app_id,
      &root.join("lintel-launcher.exe"),
      &installation.protocol_schemes,
    )?;
    registration.write_receipt(&root.join("protocol-ownership.json"))?;
    lintel_build::register_windows_protocols(&registration)?;
  }
  Ok(())
}

fn regular_file(path: &Path) -> Result<()> {
  let metadata = fs::symlink_metadata(path)?;
  ensure!(
    metadata.is_file() && !metadata.is_symlink(),
    "expected a real installation file: {}",
    path.display()
  );
  #[cfg(windows)]
  {
    native::check_private(path, false)?;
  }
  Ok(())
}

fn protect_version(directory: &Path) -> Result<()> {
  #[cfg(windows)]
  {
    native::protect_new(directory)?;
    for entry in fs::read_dir(directory)? {
      native::protect_new(&entry?.path())?;
    }
  }
  #[cfg(not(windows))]
  let _ = directory;
  Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
  regular_file(path)?;
  ensure!(fs::metadata(path)?.len() <= 65536, "installation JSON exceeds 64 KiB");
  Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
  if fs::symlink_metadata(path).is_ok() {
    regular_file(path)?;
  }
  let parent = path.parent().context("installation file has no parent")?;
  let mut pending = tempfile::NamedTempFile::new_in(parent)?;
  pending.write_all(&serde_json::to_vec_pretty(value)?)?;
  pending.as_file().sync_all()?;
  pending.persist(path).map_err(|error| error.error)?;
  Ok(())
}

fn installation(root: &Path) -> Result<Installation> {
  ensure!(
    fs::symlink_metadata(root)?.is_dir(),
    "installation root cannot be a symlink"
  );
  #[cfg(windows)]
  native::check_private(root, true)?;
  let installation: Installation = read_json(&root.join(META))?;
  ensure!(
    installation.schema == 1
      && installation.installation_id.len() == 64
      && installation
        .installation_id
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit()),
    "invalid Windows installation receipt"
  );
  crate::manifest::verifying_key(&installation.public_key)?;
  Ok(installation)
}

fn lock(root: &Path) -> Result<fs::File> {
  lock_named(root, "install.lock", true)
}

fn lock_named(root: &Path, name: &str, wait: bool) -> Result<fs::File> {
  let path = root.join(name);
  if fs::symlink_metadata(&path).is_ok() {
    regular_file(&path)?;
  }
  let mut options = fs::OpenOptions::new();
  options.read(true).write(true).create(true).truncate(false);
  #[cfg(windows)]
  {
    use std::os::windows::fs::OpenOptionsExt;
    options.custom_flags(0x00200000);
  }
  let file = options.open(&path)?;
  regular_file(&path)?;
  if wait {
    file.lock()?;
  } else {
    file
      .try_lock()
      .map_err(|error| anyhow::anyhow!("another Windows update handoff is already active: {error:?}"))?;
  }
  Ok(file)
}

fn validate_active(
  root: &Path,
  installation: &Installation,
  active: &ActiveVersion,
) -> Result<(PathBuf, WindowsBundleInfo)> {
  lintel_build::validate_windows_filename(&active.directory)?;
  ensure!(
    active.manifest_sha256.len() == 64 && active.manifest_sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
    "invalid active version digest"
  );
  let versions = root.join("versions");
  ensure!(
    fs::symlink_metadata(&versions)?.is_dir(),
    "version root must be a real directory"
  );
  let directory = versions.join(&active.directory);
  #[cfg(windows)]
  {
    native::check_private(&versions, true)?;
    native::check_private(&directory, true)?;
  }
  let info = lintel_build::validate_windows_bundle(&directory)?;
  ensure!(
    info.identifier == installation.app_id
      && info.target == installation.target
      && info.manifest_sha256 == active.manifest_sha256,
    "active version identity/hash differs from installation receipt"
  );
  #[cfg(windows)]
  for entry in fs::read_dir(&directory)? {
    native::check_private(&entry?.path(), false)?;
  }
  Ok((directory, info))
}

fn recover(root: &Path, installation: &Installation) -> Result<()> {
  let path = root.join(JOURNAL);
  if !path.try_exists()? {
    return Ok(());
  }
  let pending: PendingSwitch = read_json(&path)?;
  ensure!(
    pending.installation_id == installation.installation_id,
    "version journal belongs to another installation"
  );
  validate_active(root, installation, &pending.old)?;
  validate_active(root, installation, &pending.new)?;
  let active: ActiveVersion = read_json(&root.join(ACTIVE))?;
  ensure!(
    active == pending.old || active == pending.new,
    "active version changed outside pending transaction; refusing recovery overwrite"
  );
  if pending.health.is_some() {
    #[cfg(windows)]
    let quiet = native::processes_in_bundle(&root.join("versions").join(&pending.new.directory))?.is_empty();
    #[cfg(not(windows))]
    let quiet = false;
    return health::recover(root, installation, &pending, quiet);
  }
  // Both complete records are valid: retain whichever atomic write became visible.
  if active == pending.new {
    versions::set_rollback(root, installation, &pending.old)?;
  }
  fs::remove_file(path)?;
  Ok(())
}

fn switch(
  root: &Path,
  installation: &Installation,
  old: &ActiveVersion,
  new: &ActiveVersion,
  launch: impl FnOnce(&Path, &WindowsBundleInfo) -> Result<()>,
) -> Result<()> {
  ensure!(
    read_json::<ActiveVersion>(&root.join(ACTIVE))? == *old,
    "active version changed before update; no switch performed"
  );
  let (candidate, info) = validate_active(root, installation, new)?;
  validate_active(root, installation, old)?;
  ensure!(
    !root.join(JOURNAL).try_exists()?,
    "another pending version switch exists"
  );
  let pending = PendingSwitch {
    installation_id: installation.installation_id.clone(),
    old: old.clone(),
    new: new.clone(),
    health: None,
  };
  write_json(&root.join(JOURNAL), &pending)?;
  write_json(&root.join(ACTIVE), new)?;
  if let Err(error) = launch(&candidate, &info) {
    if read_json::<ActiveVersion>(&root.join(ACTIVE))? != *new {
      if read_json::<PendingSwitch>(&root.join(JOURNAL))? == pending {
        fs::remove_file(root.join(JOURNAL))?;
      }
      bail!("launch failed but another active version is now selected; refusing to roll back that update");
    }
    write_json(&root.join(ACTIVE), old)?;
    fs::remove_file(root.join(JOURNAL))?;
    return Err(error).context("new process could not start; previous active version restored");
  }
  versions::set_rollback(root, installation, old)?;
  fs::remove_file(root.join(JOURNAL))?;
  Ok(())
}

fn protocol_arguments(args: Vec<OsString>, schemes: &[String]) -> Result<Vec<OsString>> {
  if args.first().is_some_and(|argument| argument == "--lintel-protocol") {
    ensure!(
      args.len() == 2,
      "protocol activation must contain exactly one quoted URL argument"
    );
    let value = args[1].to_str().context("protocol URL must be Unicode")?;
    ensure!(
      value.len() <= 65536 && !value.chars().any(char::is_control) && !value.contains(['\"', '\'']),
      "invalid protocol URL"
    );
    let url = reqwest::Url::parse(value)?;
    ensure!(
      schemes.iter().any(|scheme| scheme.eq_ignore_ascii_case(url.scheme())),
      "protocol scheme is not owned by this installation"
    );
    return Ok(vec![args[1].clone()]);
  }
  Ok(args)
}

fn switch_with_health<T>(
  root: &Path,
  installation: &Installation,
  old: &ActiveVersion,
  new: &ActiveVersion,
  options: WindowsStartupHealth,
  launch: impl FnOnce(&mut PendingSwitch, &Path, &WindowsBundleInfo) -> Result<T>,
  quiet: impl FnOnce() -> Result<bool>,
) -> Result<T> {
  ensure!(
    read_json::<ActiveVersion>(&root.join(ACTIVE))? == *old,
    "active version changed before health switch"
  );
  validate_active(root, installation, old)?;
  let (directory, info) = validate_active(root, installation, new)?;
  ensure!(!root.join(JOURNAL).try_exists()?, "another pending switch exists");
  let mut pending = PendingSwitch {
    installation_id: installation.installation_id.clone(),
    old: old.clone(),
    new: new.clone(),
    health: Some(health::Attempt::new(options)?),
  };
  write_json(&root.join(JOURNAL), &pending)?;
  write_json(&root.join(ACTIVE), new)?;
  match launch(&mut pending, &directory, &info) {
    Ok(value) => {
      ensure!(
        read_json::<PendingSwitch>(&root.join(JOURNAL))? == pending,
        "health journal changed outside this attempt"
      );
      ensure!(
        pending.health.as_ref().is_some_and(health::Attempt::accepted),
        "process launch/exit does not constitute startup readiness"
      );
      health::recover(root, installation, &pending, false)?;
      Ok(value)
    }
    Err(error) => {
      ensure!(
        read_json::<PendingSwitch>(&root.join(JOURNAL))? == pending,
        "health journal changed; refusing recovery overwrite"
      );
      match health::recover(root, installation, &pending, quiet()?) {
        Ok(()) => Err(error).context("candidate readiness failed; old active version restored"),
        Err(recovery) => Err(error).context(format!("candidate readiness failed; recovery deferred: {recovery:#}")),
      }
    }
  }
}

#[cfg(windows)]
fn health_switch(
  root: &Path,
  installation: &Installation,
  old: &ActiveVersion,
  new: &ActiveVersion,
  options: WindowsStartupHealth,
  healthy_child: &mut Option<std::process::Child>,
) -> Result<()> {
  switch_with_health(
    root,
    installation,
    old,
    new,
    options,
    |pending, directory, info| {
      let child = health::launch(root, installation, pending, directory, info)?;
      // Keep the broker/pipe alive even if the following rollback-record/result write fails.
      *healthy_child = Some(child);
      Ok(())
    },
    || Ok(native::processes_in_bundle(&root.join("versions").join(&new.directory))?.is_empty()),
  )
}

/// Stable launcher entry. A protocol URL never enters a command shell or option parser.
pub fn run_windows_launcher() -> Result<()> {
  #[cfg(windows)]
  {
    let executable = std::env::current_exe()?.canonicalize()?;
    let root = executable.parent().context("launcher has no installation root")?;
    let installation = installation(root)?;
    ensure!(
      executable.file_name().is_some_and(|name| name == "lintel-launcher.exe")
        && lintel_build::validate_windows_tool(&executable, &installation.target)? == installation.launcher_sha256,
      "launcher differs from the installation receipt"
    );
    let args = protocol_arguments(std::env::args_os().skip(1).collect(), &installation.protocol_schemes)?;
    let lease = lock_named(root, "update-running.lock", false)?;
    let guard = lock(root)?;
    maintenance::ensure_live(root)?;
    recover(root, &installation)?;
    let active = read_json(&root.join(ACTIVE))?;
    let (directory, info) = validate_active(root, &installation, &active)?;
    let mut child = Command::new(directory.join(info.executable))
      .env_remove("LINTEL_UPDATE_HEALTH")
      .args(args)
      .spawn()
      .context("launch selected Windows application")?;
    drop(guard);
    drop(lease);
    ensure!(child.wait()?.success(), "selected application exited unsuccessfully");
    Ok(())
  }
  #[cfg(not(windows))]
  {
    bail!("the managed Windows launcher must execute on Windows");
  }
}

pub(crate) fn current_identity() -> Result<(String, String)> {
  let (root, metadata, active, info) = current_installation()?;
  let _ = (root, metadata, active);
  Ok((info.identifier, info.version))
}

fn current_installation() -> Result<(PathBuf, Installation, ActiveVersion, WindowsBundleInfo)> {
  let executable = std::env::current_exe()?.canonicalize()?;
  let directory = executable.parent().context("application executable has no parent")?;
  let versions = directory.parent().context("application has no version root")?;
  ensure!(
    versions.file_name().is_some_and(|name| name == "versions"),
    "Windows updates require a managed per-user installation"
  );
  let root = versions.parent().context("application has no install root")?.to_owned();
  let metadata = installation(&root)?;
  maintenance::ensure_live(&root)?;
  let active = read_json(&root.join(ACTIVE))?;
  let (selected, info) = validate_active(&root, &metadata, &active)?;
  ensure!(
    selected.join(&info.executable).canonicalize()? == executable,
    "only the active main executable may schedule an update"
  );
  Ok((root, metadata, active, info))
}

fn authenticated_release(stage: &Path, installation: &Installation) -> Result<crate::Release> {
  let manifest_path = stage.join("release.json");
  regular_file(&manifest_path)?;
  ensure!(
    fs::metadata(&manifest_path)?.len() <= 65536,
    "signed release exceeds limit"
  );
  let release = crate::manifest::decode(
    &fs::read(manifest_path)?,
    &crate::manifest::verifying_key(&installation.public_key)?,
  )?;
  ensure!(
    release.app_id == installation.app_id
      && release.target == installation.target
      && release.channel == installation.channel,
    "signed update identity/target/channel mismatch"
  );
  ensure!(
    installation.channel != "stable" || semver::Version::parse(&release.version)?.pre.is_empty(),
    "stable update cannot install a prerelease"
  );
  let mut archive = checked_archive(stage, release.size)?;
  std::io::copy(&mut archive, &mut std::io::sink())?;
  archive.verify(&release)?;
  Ok(release)
}

struct CheckedArchive<R> {
  input: R,
  hash: sha2::Sha256,
  count: u64,
  expected: u64,
}

impl<R: std::io::Read> std::io::Read for CheckedArchive<R> {
  fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
    use sha2::Digest;
    let length = buffer.len().min((self.expected + 1 - self.count) as usize);
    if length == 0 && !buffer.is_empty() {
      return Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "archive exceeds signed size",
      ));
    }
    let count = self.input.read(&mut buffer[..length])?;
    self.count += count as u64;
    if self.count > self.expected {
      return Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "archive exceeds signed size",
      ));
    }
    self.hash.update(&buffer[..count]);
    Ok(count)
  }
}

impl<R> CheckedArchive<R> {
  fn verify(self, release: &crate::Release) -> Result<()> {
    use sha2::Digest;
    ensure!(
      self.count == release.size && format!("{:x}", self.hash.finalize()).eq_ignore_ascii_case(&release.sha256),
      "signed Windows update archive size/hash mismatch"
    );
    Ok(())
  }
}

fn checked_archive(stage: &Path, size: u64) -> Result<CheckedArchive<fs::File>> {
  use sha2::Digest;
  ensure!(
    (1..=2 * 1024 * 1024 * 1024).contains(&size),
    "signed archive size exceeds Windows update limit"
  );
  let path = stage.join("update.tar.gz");
  regular_file(&path)?;
  let mut options = fs::OpenOptions::new();
  options.read(true);
  #[cfg(windows)]
  {
    use std::os::windows::fs::OpenOptionsExt;
    options.custom_flags(0x00200000).share_mode(1);
  }
  let file = options.open(&path)?;
  ensure!(
    file.metadata()?.is_file() && file.metadata()?.len() == size,
    "signed Windows archive length mismatch"
  );
  #[cfg(windows)]
  {
    use std::os::windows::fs::MetadataExt;
    ensure!(
      file.metadata()?.file_attributes() & 0x400 == 0,
      "archive handle is a reparse point"
    );
  }
  Ok(CheckedArchive {
    input: file,
    hash: sha2::Sha256::new(),
    count: 0,
    expected: size,
  })
}

fn require_newer(version: &str, current: &str) -> Result<()> {
  ensure!(
    semver::Version::parse(version)?
      .cmp_precedence(&semver::Version::parse(current)?)
      .is_gt(),
    "Windows update must increase version"
  );
  Ok(())
}

fn candidate(
  stage: &Path,
  installation: &Installation,
  release: &crate::Release,
  label: &str,
) -> Result<(PathBuf, WindowsBundleInfo)> {
  let output = stage.join(label);
  fs::create_dir(&output)?;
  // Hash the exact compressed bytes consumed by extraction, not a prior path-based read.
  let mut archive = checked_archive(stage, release.size)?;
  let candidate = crate::archive::extract_windows_reader(&mut archive, &output)?;
  std::io::copy(&mut archive, &mut std::io::sink())?;
  archive.verify(release)?;
  let info = lintel_build::validate_windows_bundle(&candidate)?;
  ensure!(
    info.identifier == installation.app_id && info.target == installation.target && info.version == release.version,
    "Windows update content differs from signed release"
  );
  Ok((candidate, info))
}

pub(crate) fn prepare(download: crate::DownloadedUpdate) -> Result<PreparedWindowsUpdate> {
  let (root, installation, old, info) = current_installation()?;
  ensure!(
    installation.public_key == STANDARD.encode(download.settings.key.as_bytes())
      && download.settings.current.to_string() == info.version
      && installation.channel == download.settings.channel,
    "updater settings differ from the pinned installation"
  );
  let stage = tempfile::Builder::new().prefix(".lintel-update-").tempdir_in(&root)?;
  #[cfg(windows)]
  native::protect_new(stage.path())?;
  fs::write(stage.path().join("release.json"), &download.signed_manifest)?;
  fs::copy(download.archive.path(), stage.path().join("update.tar.gz"))?;
  #[cfg(windows)]
  {
    native::protect_new(&stage.path().join("release.json"))?;
    native::protect_new(&stage.path().join("update.tar.gz"))?;
  }
  let release = authenticated_release(stage.path(), &installation)?;
  require_newer(&release.version, &info.version)?;
  candidate(stage.path(), &installation, &release, "preview")?;
  Ok(PreparedWindowsUpdate {
    root,
    stage,
    request: Request {
      schema: 1,
      parent: std::process::id(),
      installation_id: installation.installation_id,
      old,
      new_version: release.version,
      relaunch: true,
      health: None,
    },
  })
}

impl PreparedWindowsUpdate {
  pub(crate) fn startup_health(&mut self, options: WindowsStartupHealth) -> Result<()> {
    options.validate()?;
    self.request.health = Some(options);
    Ok(())
  }
  pub(crate) fn handoff(mut self, relaunch: bool) -> Result<crate::InstallReceipt> {
    ensure!(
      self.request.health.is_none() || relaunch,
      "startup health requires relaunch=true"
    );
    self.request.relaunch = relaunch;
    let installation = installation(&self.root)?;
    let helper = self.root.join("tools/lintel-updater.exe");
    ensure!(
      lintel_build::validate_windows_tool(&helper, &installation.target)? == installation.installer_sha256,
      "installed updater helper was modified"
    );
    let plan = self.stage.path().join("request.json");
    write_json(&plan, &self.request)?;
    let log = fs::File::create(self.stage.path().join("installer.log"))?;
    let mut command = Command::new(helper);
    command
      .arg("--apply-windows")
      .arg(plan)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(log);
    #[cfg(windows)]
    {
      use std::os::windows::process::CommandExt;
      command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().context("installer has no readiness pipe")?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
      use std::io::BufRead;
      let mut line = String::new();
      let ready = std::io::BufReader::new(stdout).read_line(&mut line).map(|_| line);
      let _ = sender.send(ready);
    });
    if !matches!(receiver.recv_timeout(Duration::from_secs(120)), Ok(Ok(line)) if line.trim() == "LINTEL_UPDATER_READY")
    {
      let _ = child.kill();
      let _ = child.wait();
      bail!(
        "Windows update handoff failed: {}",
        fs::read_to_string(self.stage.path().join("installer.log")).unwrap_or_default()
      );
    }
    let prior_result = self.root.join(RESULT);
    let reset = (|| -> Result<()> {
      match fs::symlink_metadata(&prior_result) {
        Ok(_) => {
          regular_file(&prior_result)?;
          fs::remove_file(&prior_result)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
      }
      Ok(())
    })();
    if let Err(error) = reset {
      let _ = child.kill();
      let _ = child.wait();
      return Err(error).context("reset prior Windows update outcome");
    }
    let installer_pid = child.id();
    let _ = self.stage.keep();
    Ok(crate::InstallReceipt {
      result_path: self.root.join(RESULT),
      installer_pid,
    })
  }
}

pub(crate) fn run_helper(path: &Path) -> Result<()> {
  #[cfg(windows)]
  {
    let executable = std::env::current_exe()?.canonicalize()?;
    let tools = executable.parent().context("installer has no tools directory")?;
    ensure!(
      tools.file_name().is_some_and(|name| name == "tools"),
      "Windows installer must be installed in its managed tools directory"
    );
    let root = tools
      .parent()
      .context("installer has no managed root")?
      .canonicalize()?;
    let installation = installation(&root)?;
    ensure!(
      lintel_build::validate_windows_tool(&executable, &installation.target)? == installation.installer_sha256,
      "installer does not match the managed installation"
    );
    let stage = path.parent().context("request has no stage")?.canonicalize()?;
    native::check_private(&stage, true)?;
    ensure!(
      stage.parent() == Some(root.as_path())
        && stage
          .file_name()
          .is_some_and(|name| name.to_string_lossy().starts_with(".lintel-update-"))
        && path.file_name().is_some_and(|name| name == "request.json"),
      "request stage escapes the managed installation"
    );
    let request: Request = read_json(path)?;
    ensure!(
      request.schema == 1 && request.installation_id == installation.installation_id,
      "update request belongs to another installation"
    );
    ensure!(
      request.health.is_none() || request.relaunch,
      "startup health requires a relaunching update"
    );
    if let Some(options) = &request.health {
      options.validate()?;
    }
    let _lease = lock_named(&root, "update-running.lock", false)?;
    maintenance::ensure_live(&root)?;
    let mut healthy_child = None;
    let outcome = (|| {
      let release = authenticated_release(&stage, &installation)?;
      ensure!(
        release.version == request.new_version,
        "request target version differs from signed release"
      );
      let (old_directory, old_info) = validate_active(&root, &installation, &request.old)?;
      require_newer(&release.version, &old_info.version)?;
      ensure!(
        read_json::<ActiveVersion>(&root.join(ACTIVE))? == request.old,
        "active version changed before installer readiness"
      );
      let (verified, verified_info) = candidate(&stage, &installation, &release, "verified")?;
      let parent = native::Parent::open(request.parent, &old_directory.join(&old_info.executable))?;
      writeln!(std::io::stdout(), "LINTEL_UPDATER_READY")?;
      std::io::stdout().flush()?;
      parent.wait_and_check(&old_directory)?;
      let _guard = lock(&root)?;
      recover(&root, &installation)?;
      ensure!(
        native::processes_in_bundle(&old_directory)?.is_empty(),
        "application reopened before Windows version switch"
      );
      let checked = authenticated_release(&stage, &installation)?;
      ensure!(checked == release, "signed release changed during shutdown wait");
      ensure!(
        lintel_build::validate_windows_bundle(&verified)? == verified_info,
        "verified update tree changed during shutdown wait"
      );
      let published = lintel_build::copy_windows_bundle_verified(&verified, &root.join("versions"), &verified_info)?;
      protect_version(&published)?;
      let info = lintel_build::validate_windows_bundle(&published)?;
      ensure!(
        info == verified_info,
        "published version differs from authenticated update snapshot"
      );
      let new = ActiveVersion {
        directory: published
          .file_name()
          .context("published version has no name")?
          .to_str()
          .context("non-Unicode version")?
          .to_owned(),
        manifest_sha256: info.manifest_sha256,
      };
      versions::signed(
        &root,
        &installation,
        &new,
        &verified_info,
        &fs::read(stage.join("release.json"))?,
      )?;
      ensure!(
        release.expires_at > crate::manifest::now()?,
        "signed release expired before version switch"
      );
      let result = if let Some(options) = request.health.clone() {
        health_switch(&root, &installation, &request.old, &new, options, &mut healthy_child)
      } else {
        switch(&root, &installation, &request.old, &new, |directory, info| {
          if request.relaunch {
            Command::new(directory.join(&info.executable))
              .env_remove("LINTEL_UPDATE_HEALTH")
              .current_dir(&root)
              .stdin(Stdio::null())
              .stdout(Stdio::null())
              .stderr(Stdio::null())
              .spawn()
              .context("start updated Windows application")?;
          }
          Ok(())
        })
      };
      if result.is_err() && request.relaunch && read_json::<ActiveVersion>(&root.join(ACTIVE))? == request.old {
        Command::new(old_directory.join(old_info.executable))
          .env_remove("LINTEL_UPDATE_HEALTH")
          .current_dir(&root)
          .spawn()
          .context("previous version restored but could not restart")?;
      }
      result
    })();
    let result_write = write_json(
      &root.join(RESULT),
      &serde_json::json!({"success": outcome.is_ok(), "version": request.new_version, "error": outcome.as_ref().err().map(|error| format!("{error:#}"))}),
    );
    let _ = fs::remove_dir_all(&stage);
    drop(_lease);
    // A health-only broker stays alive to drain its child's dedicated stdout. It holds no lease
    // or install lock after committing the result, so a subsequent update can proceed normally.
    if let Some(mut child) = healthy_child {
      let _ = child.wait();
    }
    result_write.context("persist Windows installer outcome")?;
    outcome
  }
  #[cfg(not(windows))]
  {
    let _ = path;
    bail!("Windows update installer must execute on Windows");
  }
}

#[cfg(test)]
#[path = "windows_install_tests.rs"]
mod tests;
