//! Explicit external-process maintenance; unknown files and altered ownership records are retained.
use super::*;
use std::collections::BTreeSet;

pub(super) const TOMBSTONE: &str = "uninstall.json";

#[derive(Clone, Debug, Serialize)]
pub struct WindowsRetainedPath {
  pub path: String,
  pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct WindowsMaintenanceReport {
  pub app_id: String,
  pub operation: String,
  pub dry_run: bool,
  pub removed: Vec<String>,
  pub retained: Vec<WindowsRetainedPath>,
  pub protected_versions: Vec<String>,
  pub protocols_removed: Vec<String>,
  pub protocols_retained: Vec<String>,
}

impl WindowsMaintenanceReport {
  pub fn to_json(&self) -> Result<String> {
    Ok(serde_json::to_string_pretty(self)?)
  }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UninstallState {
  schema: u32,
  installation_id: String,
  app_id: String,
}

pub(super) fn ensure_live(root: &Path) -> Result<()> {
  ensure!(
    fs::symlink_metadata(root.join(TOMBSTONE)).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
    "managed installation is uninstalled or pending removal"
  );
  Ok(())
}

fn report(metadata: &Installation, operation: &str, dry_run: bool) -> WindowsMaintenanceReport {
  WindowsMaintenanceReport {
    app_id: metadata.app_id.clone(),
    operation: operation.into(),
    dry_run,
    removed: vec![],
    retained: vec![],
    protected_versions: vec![],
    protocols_removed: vec![],
    protocols_retained: vec![],
  }
}

fn retain(report: &mut WindowsMaintenanceReport, path: impl Into<String>, reason: impl Into<String>) {
  report.retained.push(WindowsRetainedPath {
    path: path.into(),
    reason: reason.into(),
  });
}

fn check_directories(root: &Path) -> Result<()> {
  for name in ["versions", "tools"] {
    let path = root.join(name);
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
      metadata.is_dir() && !metadata.is_symlink(),
      "maintenance directory is linked or not a directory"
    );
    #[cfg(windows)]
    native::check_private(&path, true)?;
  }
  Ok(())
}

fn protected(root: &Path, metadata: &Installation, ledger: &versions::Ledger, keep: usize) -> Result<BTreeSet<String>> {
  let active: ActiveVersion = read_json(&root.join(ACTIVE))?;
  validate_active(root, metadata, &active)?;
  let mut protected = BTreeSet::from([active.directory]);
  if let Some(rollback) = versions::rollback(root, metadata)? {
    protected.insert(rollback.directory);
  }
  if root.join(JOURNAL).try_exists()? {
    let pending: PendingSwitch = read_json(&root.join(JOURNAL))?;
    ensure!(
      pending.installation_id == metadata.installation_id,
      "pending switch belongs to another installation"
    );
    validate_active(root, metadata, &pending.old)?;
    validate_active(root, metadata, &pending.new)?;
    protected.insert(pending.old.directory);
    protected.insert(pending.new.directory);
  }
  let mut recent: Vec<_> = ledger.records.values().collect();
  recent.sort_by_key(|record| std::cmp::Reverse(record.sequence));
  protected.extend(
    recent
      .into_iter()
      .take(keep)
      .map(|record| record.active.directory.clone()),
  );
  Ok(protected)
}

/// Read-only inspection also works for fixtures on other hosts; it makes no native liveness claim.
pub fn inspect_windows_installation(root: &Path, expected_app_id: &str) -> Result<WindowsMaintenanceReport> {
  let metadata = installation(root)?;
  ensure!(
    metadata.app_id == expected_app_id,
    "installation belongs to a different app ID"
  );
  let ledger = versions::read(root, &metadata)?;
  let mut report = report(&metadata, "inspect", true);
  if !root.join(TOMBSTONE).try_exists()? {
    report.protected_versions = protected(root, &metadata, &ledger, 0)?.into_iter().collect();
  }
  for entry in fs::read_dir(root.join("versions"))? {
    let entry = entry?;
    let name = entry
      .file_name()
      .into_string()
      .map_err(|_| anyhow::anyhow!("non-Unicode version directory"))?;
    match ledger.records.get(&name) {
      Some(record) => {
        if let Err(error) = versions::authenticate(root, &metadata, record) {
          retain(
            &mut report,
            format!("versions/{name}"),
            format!("unverified/modified: {error:#}"),
          );
        }
      }
      None => retain(
        &mut report,
        format!("versions/{name}"),
        "unregistered directory; not owned by cleanup",
      ),
    }
  }
  Ok(report)
}

/// Run from an external maintenance process after the installation's applications have exited.
pub fn prune_windows_versions(
  root: &Path,
  expected_app_id: &str,
  keep_recent: usize,
  dry_run: bool,
) -> Result<WindowsMaintenanceReport> {
  #[cfg(windows)]
  {
    let root = native::validate_existing_install_root(root)?;
    let metadata = installation(&root)?;
    ensure!(
      metadata.app_id == expected_app_id,
      "installation belongs to another app ID"
    );
    let _lease = lock_named(&root, "update-running.lock", false)?;
    let _guard = lock(&root)?;
    ensure_live(&root)?;
    ensure!(
      native::processes_in_bundle(&root)?.is_empty(),
      "close all installation applications/launchers before pruning"
    );
    prune_with(&root, &metadata, keep_recent, dry_run, |files| {
      native::remove_owned_files(files)
    })
  }
  #[cfg(not(windows))]
  {
    let _ = (root, expected_app_id, keep_recent, dry_run);
    bail!("native Windows version pruning requires Windows");
  }
}

pub(super) fn prune_with(
  root: &Path,
  metadata: &Installation,
  keep_recent: usize,
  dry_run: bool,
  mut remove: impl FnMut(&[(PathBuf, String)]) -> Result<()>,
) -> Result<WindowsMaintenanceReport> {
  check_directories(root)?;
  ensure!(keep_recent <= 128, "keep_recent exceeds ledger bound");
  let mut ledger = versions::read(root, metadata)?;
  let protected = protected(root, metadata, &ledger, keep_recent)?;
  let mut report = report(metadata, "prune", dry_run);
  report.protected_versions = protected.iter().cloned().collect();
  let entries = fs::read_dir(root.join("versions"))?.collect::<std::io::Result<Vec<_>>>()?;
  for entry in entries {
    let name = entry
      .file_name()
      .into_string()
      .map_err(|_| anyhow::anyhow!("non-Unicode version directory"))?;
    if protected.contains(&name) {
      retain(
        &mut report,
        format!("versions/{name}"),
        "active/rollback/pending/recent version",
      );
      continue;
    }
    let Some(record) = ledger.records.get(&name).cloned() else {
      retain(&mut report, format!("versions/{name}"), "unregistered directory");
      continue;
    };
    if let Err(error) = versions::authenticate(root, metadata, &record) {
      retain(
        &mut report,
        format!("versions/{name}"),
        format!("unverified/modified: {error:#}"),
      );
      continue;
    }
    let files = versions::files(root, &record)?;
    if !dry_run {
      if let Err(error) = remove(&files) {
        retain(
          &mut report,
          format!("versions/{name}"),
          format!("in use or could not reserve owned files: {error:#}"),
        );
        continue;
      }
      if let Err(error) = fs::remove_dir(entry.path()) {
        retain(
          &mut report,
          format!("versions/{name}"),
          format!("directory retained: {error}"),
        );
        continue;
      }
      versions::remove(root, &mut ledger, &name)?;
    }
    report.removed.push(format!("versions/{name}"));
  }
  Ok(report)
}

/// Leaves unknown/business data and a small ownership/tombstone audit record; never remove_dir_all.
pub fn uninstall_windows_app(root: &Path, expected_app_id: &str, dry_run: bool) -> Result<WindowsMaintenanceReport> {
  #[cfg(windows)]
  {
    let root = native::validate_existing_install_root(root)?;
    let metadata = installation(&root)?;
    ensure!(
      metadata.app_id == expected_app_id,
      "installation belongs to another app ID"
    );
    let _lease = lock_named(&root, "update-running.lock", false)?;
    let _guard = lock(&root)?;
    ensure!(
      native::processes_in_bundle(&root)?.is_empty(),
      "close all installation applications/launchers before uninstalling"
    );
    ensure!(
      !root.join(JOURNAL).try_exists()?,
      "resolve pending update/health state before uninstalling"
    );
    uninstall_with(
      &root,
      &metadata,
      dry_run,
      |files| native::remove_owned_files(files),
      |registration| lintel_build::unregister_matching_windows_protocols(registration),
    )
  }
  #[cfg(not(windows))]
  {
    let _ = (root, expected_app_id, dry_run);
    bail!("native current-user Windows uninstall requires Windows");
  }
}

pub(super) fn uninstall_with(
  root: &Path,
  metadata: &Installation,
  dry_run: bool,
  mut remove: impl FnMut(&[(PathBuf, String)]) -> Result<()>,
  mut protocols: impl FnMut(&WindowsProtocolRegistration) -> Result<lintel_build::WindowsProtocolRemoval>,
) -> Result<WindowsMaintenanceReport> {
  check_directories(root)?;
  let mut ledger = versions::read(root, metadata)?;
  let mut report = report(metadata, "uninstall", dry_run);
  let mut owned_versions = Vec::new();
  for entry in fs::read_dir(root.join("versions"))? {
    let entry = entry?;
    let name = entry
      .file_name()
      .into_string()
      .map_err(|_| anyhow::anyhow!("non-Unicode version directory"))?;
    match ledger.records.get(&name).cloned() {
      Some(record) => match versions::authenticate(root, metadata, &record) {
        Ok(_) => owned_versions.push((name, versions::files(root, &record)?)),
        Err(error) => retain(
          &mut report,
          format!("versions/{name}"),
          format!("modified/unverified: {error:#}"),
        ),
      },
      None => retain(
        &mut report,
        format!("versions/{name}"),
        "unregistered directory/business data preserved",
      ),
    }
  }
  let mut tools = Vec::new();
  for (name, expected) in [
    ("lintel-launcher.exe", &metadata.launcher_sha256),
    ("tools/lintel-updater.exe", &metadata.installer_sha256),
  ] {
    let path = root.join(name);
    if !path.try_exists()? {
      continue;
    }
    match lintel_build::validate_windows_tool(&path, &metadata.target) {
      Ok(hash) if hash == *expected => tools.push((path, hash)),
      _ => retain(&mut report, name, "tool differs from the installation ownership hash"),
    }
  }
  let receipt_path = root.join("protocol-ownership.json");
  if receipt_path.try_exists()? {
    regular_file(&receipt_path)?;
    let registration = WindowsProtocolRegistration::read_receipt(&receipt_path)?;
    let executable = root.join("lintel-launcher.exe");
    let same_path = registration
      .executable()
      .canonicalize()
      .ok()
      .zip(executable.canonicalize().ok())
      .is_some_and(|(left, right)| left == right);
    let matching = registration.app_id() == metadata.app_id
      && same_path
      && registration.schemes().iter().collect::<BTreeSet<_>>()
        == metadata.protocol_schemes.iter().collect::<BTreeSet<_>>();
    if matching {
      if !dry_run {
        let result = protocols(&registration)?;
        report.protocols_removed = result.removed;
        report.protocols_retained = result.retained_foreign;
      } else {
        report.protocols_removed = registration.schemes().to_vec();
      }
    } else {
      retain(
        &mut report,
        "protocol-ownership.json",
        "receipt belongs to another executable/root/policy; associations preserved",
      );
    }
  }
  if !dry_run {
    write_json(
      &root.join(TOMBSTONE),
      &UninstallState {
        schema: 1,
        installation_id: metadata.installation_id.clone(),
        app_id: metadata.app_id.clone(),
      },
    )?;
  }
  for (name, files) in owned_versions {
    if !dry_run {
      match remove(&files).and_then(|_| fs::remove_dir(root.join("versions").join(&name)).map_err(Into::into)) {
        Ok(()) => versions::remove(root, &mut ledger, &name)?,
        Err(error) => {
          retain(
            &mut report,
            format!("versions/{name}"),
            format!("owned files busy/retained: {error:#}"),
          );
          continue;
        }
      }
    }
    report.removed.push(format!("versions/{name}"));
  }
  if !dry_run && !tools.is_empty() {
    if let Err(error) = remove(&tools) {
      for (path, _) in &tools {
        retain(
          &mut report,
          path.strip_prefix(root)?.to_string_lossy(),
          format!("tool busy/retained: {error:#}"),
        );
      }
    } else {
      report.removed.extend(
        tools
          .iter()
          .map(|(path, _)| path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned()),
      );
    }
  } else {
    report.removed.extend(
      tools
        .iter()
        .map(|(path, _)| path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned()),
    );
  }
  let known: BTreeSet<&str> = [
    META,
    ACTIVE,
    JOURNAL,
    RESULT,
    versions::LEDGER,
    versions::ROLLBACK,
    TOMBSTONE,
    "install.lock",
    "update-running.lock",
    "versions",
    "tools",
    "version-receipts",
    "protocol-ownership.json",
    "lintel-launcher.exe",
  ]
  .into_iter()
  .collect();
  for entry in fs::read_dir(root)? {
    let entry = entry?;
    let name = entry.file_name().to_string_lossy().into_owned();
    if !known.contains(name.as_str()) {
      retain(&mut report, name, "unknown/business data preserved");
    }
  }
  for entry in fs::read_dir(root.join("tools"))? {
    let entry = entry?;
    if entry.file_name() != "lintel-updater.exe" {
      retain(
        &mut report,
        format!("tools/{}", entry.file_name().to_string_lossy()),
        "unregistered tool/business data preserved",
      );
    }
  }
  retain(
    &mut report,
    META,
    "ownership/tombstone/lock audit records are deliberately retained; installation root is not recursively deleted",
  );
  Ok(report)
}
