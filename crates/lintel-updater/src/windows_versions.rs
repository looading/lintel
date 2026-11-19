//! Local ownership attestations written only after explicit initial install or authenticated extraction.
use super::*;
use std::collections::BTreeMap;

pub(super) const LEDGER: &str = "managed-versions.json";
pub(super) const ROLLBACK: &str = "rollback.json";
const PROOFS: &str = "version-receipts";

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Origin {
  InitialUserSelection,
  Signed { envelope: String, sha256: String },
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct VersionRecord {
  pub active: ActiveVersion,
  pub version: String,
  pub sequence: u64,
  origin: Origin,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Ledger {
  schema: u32,
  installation_id: String,
  pub records: BTreeMap<String, VersionRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Rollback {
  installation_id: String,
  active: ActiveVersion,
}

pub(super) fn read(root: &Path, metadata: &Installation) -> Result<Ledger> {
  match fs::symlink_metadata(root.join(LEDGER)) {
    Ok(_) => {
      let ledger: Ledger = read_json(&root.join(LEDGER))?;
      ensure!(
        ledger.schema == 1 && ledger.installation_id == metadata.installation_id && ledger.records.len() <= 128,
        "invalid managed version ledger"
      );
      for (directory, record) in &ledger.records {
        lintel_build::validate_windows_filename(directory)?;
        ensure!(
          *directory == record.active.directory,
          "version ledger directory mismatch"
        );
        semver::Version::parse(&record.version)?;
      }
      Ok(ledger)
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      // Existing installations know only their active record. Never adopt unrecorded older directories.
      let active: ActiveVersion = read_json(&root.join(ACTIVE))?;
      let (_, info) = validate_active(root, metadata, &active)?;
      Ok(Ledger {
        schema: 1,
        installation_id: metadata.installation_id.clone(),
        records: BTreeMap::from([(
          active.directory.clone(),
          VersionRecord {
            active,
            version: info.version,
            sequence: 0,
            origin: Origin::InitialUserSelection,
          },
        )]),
      })
    }
    Err(error) => Err(error.into()),
  }
}

fn save(root: &Path, ledger: &Ledger) -> Result<()> {
  ensure!(
    ledger.records.len() <= 128 && serde_json::to_vec_pretty(ledger)?.len() <= 65536,
    "managed version ledger is full; prune old versions before updating"
  );
  write_json(&root.join(LEDGER), ledger)
}

pub(super) fn initial(
  root: &Path,
  metadata: &Installation,
  active: &ActiveVersion,
  info: &WindowsBundleInfo,
) -> Result<()> {
  ensure!(
    !root.join(LEDGER).try_exists()?,
    "initial version ledger already exists"
  );
  let ledger = Ledger {
    schema: 1,
    installation_id: metadata.installation_id.clone(),
    records: BTreeMap::from([(
      active.directory.clone(),
      VersionRecord {
        active: active.clone(),
        version: info.version.clone(),
        sequence: 0,
        origin: Origin::InitialUserSelection,
      },
    )]),
  };
  save(root, &ledger)
}

pub(super) fn signed(
  root: &Path,
  metadata: &Installation,
  active: &ActiveVersion,
  info: &WindowsBundleInfo,
  envelope: &[u8],
) -> Result<()> {
  let release = crate::manifest::decode(envelope, &crate::manifest::verifying_key(&metadata.public_key)?)?;
  ensure!(
    release.app_id == metadata.app_id
      && release.target == metadata.target
      && release.channel == metadata.channel
      && release.version == info.version
      && info.manifest_sha256 == active.manifest_sha256,
    "authenticated version attestation mismatch"
  );
  let mut ledger = read(root, metadata)?;
  let sequence = ledger
    .records
    .values()
    .map(|record| record.sequence)
    .max()
    .unwrap_or(0)
    .checked_add(1)
    .context("version sequence exhausted")?;
  let proofs = root.join(PROOFS);
  if proofs.exists() {
    ensure!(
      fs::symlink_metadata(&proofs)?.is_dir(),
      "version receipt root cannot be linked"
    );
    #[cfg(windows)]
    native::check_private(&proofs, true)?;
  } else {
    fs::create_dir(&proofs)?;
    #[cfg(windows)]
    native::protect_new(&proofs)?;
  }
  let name = format!("{}.json", active.manifest_sha256);
  lintel_build::validate_windows_filename(&name)?;
  let path = proofs.join(&name);
  if path.exists() {
    regular_file(&path)?;
    ensure!(fs::read(&path)? == envelope, "existing signed version receipt differs");
  } else {
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
    file.write_all(envelope)?;
    file.sync_all()?;
  }
  let (_, digest) = crate::archive_digest(&path)?;
  ledger.records.insert(
    active.directory.clone(),
    VersionRecord {
      active: active.clone(),
      version: info.version.clone(),
      sequence,
      origin: Origin::Signed {
        envelope: name,
        sha256: digest,
      },
    },
  );
  save(root, &ledger)
}

pub(super) fn authenticate(
  root: &Path,
  metadata: &Installation,
  record: &VersionRecord,
) -> Result<(PathBuf, WindowsBundleInfo)> {
  let (directory, info) = validate_active(root, metadata, &record.active)?;
  ensure!(
    info.version == record.version,
    "managed version differs from ownership ledger"
  );
  if let Origin::Signed { envelope, sha256 } = &record.origin {
    lintel_build::validate_windows_filename(envelope)?;
    let proofs = root.join(PROOFS);
    ensure!(fs::symlink_metadata(&proofs)?.is_dir(), "receipt directory is not real");
    #[cfg(windows)]
    native::check_private(&proofs, true)?;
    let path = proofs.join(envelope);
    regular_file(&path)?;
    ensure!(
      fs::metadata(&path)?.len() <= 65536 && crate::archive_digest(&path)?.1 == *sha256,
      "signed version receipt changed"
    );
    let release =
      crate::manifest::decode_history(&fs::read(path)?, &crate::manifest::verifying_key(&metadata.public_key)?)?;
    ensure!(
      release.app_id == metadata.app_id
        && release.target == metadata.target
        && release.channel == metadata.channel
        && release.version == info.version,
      "version receipt signature/identity mismatch"
    );
  }
  Ok((directory, info))
}

pub(super) fn remove(root: &Path, ledger: &mut Ledger, directory: &str) -> Result<()> {
  ledger.records.remove(directory);
  save(root, ledger)
}

pub(super) fn rollback(root: &Path, metadata: &Installation) -> Result<Option<ActiveVersion>> {
  if !root.join(ROLLBACK).try_exists()? {
    return Ok(None);
  }
  let record: Rollback = read_json(&root.join(ROLLBACK))?;
  ensure!(
    record.installation_id == metadata.installation_id,
    "rollback record belongs to another installation"
  );
  validate_active(root, metadata, &record.active)?;
  Ok(Some(record.active))
}

pub(super) fn set_rollback(root: &Path, metadata: &Installation, active: &ActiveVersion) -> Result<()> {
  validate_active(root, metadata, active)?;
  write_json(
    &root.join(ROLLBACK),
    &Rollback {
      installation_id: metadata.installation_id.clone(),
      active: active.clone(),
    },
  )
}

pub(super) fn files(root: &Path, record: &VersionRecord) -> Result<Vec<(PathBuf, String)>> {
  let directory = root.join("versions").join(&record.active.directory);
  let manifest: serde_json::Value = serde_json::from_slice(&fs::read(directory.join("lintel-bundle.json"))?)?;
  let files = manifest["files"]
    .as_object()
    .context("portable manifest has no file map")?;
  let mut result = Vec::new();
  for (name, hash) in files {
    lintel_build::validate_windows_filename(name)?;
    result.push((
      directory.join(name),
      hash.as_str().context("invalid manifest hash")?.to_owned(),
    ));
  }
  result.push((
    directory.join("lintel-bundle.json"),
    record.active.manifest_sha256.clone(),
  ));
  Ok(result)
}
