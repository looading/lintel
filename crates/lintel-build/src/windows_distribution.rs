//! Signed, immutable inputs for the existing per-user Windows installer, not an MSI/NSIS engine.
use crate::{
  WindowsBundleInfo,
  windows::{self, Architecture},
  windows_signing::checked_path,
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
  collections::{BTreeMap, BTreeSet},
  fs,
  io::{Read, Write},
  path::{Path, PathBuf},
};

const MANIFEST: &str = "distribution.json";
const DOMAIN: &str = "lintel.windows-managed-distribution.v1";
const SIGNING_DOMAIN: &[u8] = b"Lintel.WindowsManagedDistribution.v1\0";
const MAX_JSON: usize = 4 * 1024 * 1024;
const MAX_PAYLOAD: usize = 1024 * 1024;
const MAX_FILE: u64 = 512 * 1024 * 1024;
const MAX_TOTAL: u64 = 2 * 1024 * 1024 * 1024;
const KEY_FILE: &str = "updater.pub";
const LAUNCHER: &str = "tools/lintel-launcher.exe";
const UPDATER: &str = "tools/lintel-updater.exe";

/// Authenticode-sign the final binaries before creating this snapshot. Keys are public here.
pub struct WindowsDistributionOptions {
  pub bundle: PathBuf,
  pub launcher: PathBuf,
  pub updater: PathBuf,
  pub public_key: String,
  pub channel: String,
  /// Existing output directory. Final version directories are immutable and never replaced.
  pub output_dir: PathBuf,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Role {
  Application,
  RuntimeLibrary,
  ApplicationHelper,
  PortableManifest,
  LicenseNotice,
  Launcher,
  Updater,
  UpdateKey,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileRecord {
  role: Role,
  size: u64,
  sha256: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
  schema: u32,
  kind: String,
  bundle: WindowsBundleInfo,
  channel: String,
  public_key: String,
  files: BTreeMap<String, FileRecord>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
  schema: u32,
  domain: String,
  payload: String,
  signature: String,
}
#[derive(Clone, Debug, Serialize)]
struct Source {
  path: PathBuf,
  record: FileRecord,
}
/// A read-only checked input snapshot. No private key path or bytes are retained in a plan.
#[derive(Clone, Debug, Serialize)]
pub struct WindowsDistributionPlan {
  schema: u32,
  operation: &'static str,
  bundle_source: PathBuf,
  sources: BTreeMap<String, Source>,
  output: PathBuf,
  destination: PathBuf,
  manifest: Manifest,
  authenticode_chain_verified: bool,
}
impl WindowsDistributionPlan {
  pub fn to_json(&self) -> Result<String> {
    Ok(serde_json::to_string_pretty(self)?)
  }
  pub fn destination(&self) -> &Path {
    &self.destination
  }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WindowsDistributionInfo {
  pub directory: PathBuf,
  pub bundle: WindowsBundleInfo,
  pub channel: String,
  pub public_key: String,
  pub manifest_sha256: String,
  pub file_count: usize,
  /// Verified against the explicit caller key, never inferred from the included key.
  pub ed25519_manifest_verified: bool,
  pub authenticode_chain_verified: bool,
  pub native_installation_executed: bool,
}
impl WindowsDistributionInfo {
  pub fn to_json(&self) -> Result<String> {
    Ok(serde_json::to_string_pretty(self)?)
  }
}

fn key(encoded: &str) -> Result<VerifyingKey> {
  let decoded = STANDARD
    .decode(encoded.trim())
    .context("decode distribution public key")?;
  let bytes: [u8; 32] = decoded
    .try_into()
    .map_err(|_| anyhow::anyhow!("distribution public key must contain 32 bytes"))?;
  let key = VerifyingKey::from_bytes(&bytes)?;
  ensure!(!key.is_weak(), "weak distribution public key");
  Ok(key)
}
fn validate_policy(manifest: &Manifest) -> Result<()> {
  ensure!(
    manifest.schema == 1 && manifest.kind == DOMAIN,
    "unsupported Windows distribution schema/domain"
  );
  let version = cargo_metadata::semver::Version::parse(&manifest.bundle.version)?;
  ensure!(
    !manifest.channel.is_empty()
      && manifest.channel.len() <= 64
      && manifest
        .channel
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
    "invalid distribution channel"
  );
  ensure!(
    manifest.channel != "stable" || version.pre.is_empty(),
    "stable distribution cannot contain a prerelease"
  );
  Architecture::target(&manifest.bundle.target)?;
  crate::windows_protocol::validate_windows_protocol_schemes(&manifest.bundle.protocol_schemes)?;
  windows::validate_windows_filename(&manifest.bundle.identifier)?;
  ensure!(
    (5..=4100).contains(&manifest.files.len()),
    "invalid distribution file count"
  );
  ensure!(
    STANDARD.encode(key(&manifest.public_key)?.as_bytes()) == manifest.public_key,
    "distribution key pin must be canonical base64"
  );
  let mut aliases = BTreeSet::new();
  let mut total = 0_u64;
  for (path, record) in &manifest.files {
    validate_relative(path)?;
    ensure!(
      aliases.insert(path.to_ascii_lowercase()),
      "case-colliding distribution path"
    );
    ensure!(
      record.sha256.len() == 64 && record.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()),
      "invalid distribution file digest"
    );
    ensure!(
      record.size > 0 && record.size <= MAX_FILE,
      "distribution file size outside bound"
    );
    total = total.checked_add(record.size).context("distribution size overflow")?;
    ensure!(total <= MAX_TOTAL, "distribution exceeds 2 GiB input limit");
    let expected = role(path, &manifest.bundle)?;
    ensure!(
      record.role == expected,
      "distribution file role does not match its fixed path"
    );
  }
  ensure!(
    manifest.files.contains_key(KEY_FILE)
      && manifest.files.contains_key(LAUNCHER)
      && manifest.files.contains_key(UPDATER)
      && manifest.files.contains_key("bundle/lintel-bundle.json")
      && manifest
        .files
        .contains_key(&format!("bundle/{}", manifest.bundle.executable)),
    "distribution is missing a mandatory role"
  );
  ensure!(
    manifest.files["bundle/lintel-bundle.json"].sha256 == manifest.bundle.manifest_sha256,
    "portable manifest digest is inconsistent with distribution identity"
  );
  Ok(())
}
fn validate_relative(path: &str) -> Result<()> {
  ensure!(
    !path.contains(['\\', ':']) && !path.starts_with('/') && !path.ends_with('/') && path.len() <= 220,
    "unsafe distribution relative path"
  );
  let parts: Vec<_> = path.split('/').collect();
  ensure!(
    parts.len() <= 2 && !parts.is_empty(),
    "distribution path depth exceeds fixed layout"
  );
  for component in &parts {
    windows::validate_windows_filename(component)?;
  }
  ensure!(
    parts.len() == 1 && path == KEY_FILE || parts.len() == 2 && matches!(parts[0], "bundle" | "tools"),
    "unknown distribution path outside fixed layout"
  );
  Ok(())
}
fn role(path: &str, bundle: &WindowsBundleInfo) -> Result<Role> {
  Ok(match path {
    KEY_FILE => Role::UpdateKey,
    LAUNCHER => Role::Launcher,
    UPDATER => Role::Updater,
    "bundle/lintel-bundle.json" => Role::PortableManifest,
    value
      if value
        .strip_prefix("bundle/")
        .is_some_and(crate::service_processes::is_windows_mojo_notice) =>
    {
      Role::LicenseNotice
    }
    value if value == format!("bundle/{}", bundle.executable) => Role::Application,
    value if value.starts_with("bundle/") && value.to_ascii_lowercase().ends_with(".dll") => Role::RuntimeLibrary,
    value if value.starts_with("bundle/") && value.to_ascii_lowercase().ends_with(".exe") => Role::ApplicationHelper,
    _ => bail!("unknown distribution file role/path"),
  })
}
fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>> {
  let path = checked_path(path, false)?;
  let mut bytes = Vec::new();
  fs::File::open(path)?.take(max as u64 + 1).read_to_end(&mut bytes)?;
  ensure!(bytes.len() <= max, "distribution metadata exceeds size limit");
  Ok(bytes)
}
fn record(path: &Path, role: Role) -> Result<FileRecord> {
  let path = checked_path(path, false)?;
  let mut file = fs::File::open(path)?;
  let mut hash = Sha256::new();
  let mut size = 0_u64;
  let mut buffer = [0_u8; 65536];
  loop {
    let count = file.read(&mut buffer)?;
    if count == 0 {
      break;
    }
    size = size.checked_add(count as u64).context("input size overflow")?;
    ensure!(size <= MAX_FILE, "distribution file exceeds 512 MiB");
    hash.update(&buffer[..count]);
  }
  ensure!(size > 0, "empty distribution file");
  Ok(FileRecord {
    role,
    size,
    sha256: format!("{:x}", hash.finalize()),
  })
}
fn public_key_bytes(key: &str) -> Vec<u8> {
  format!("{key}\n").into_bytes()
}
fn byte_record(bytes: &[u8], role: Role) -> FileRecord {
  FileRecord {
    role,
    size: bytes.len() as u64,
    sha256: format!("{:x}", Sha256::digest(bytes)),
  }
}
fn output_name(info: &WindowsBundleInfo) -> Result<String> {
  let arch = match Architecture::target(&info.target)? {
    Architecture::X86 => "x86",
    Architecture::X64 => "x86_64",
    Architecture::Arm64 => "aarch64",
  };
  let name = format!("{}-{}-{arch}-managed", info.name, info.version);
  windows::validate_windows_filename(&name)?;
  Ok(name)
}
fn output_overlaps_bundle(output: &Path, bundle: &Path) -> Result<bool> {
  if output.starts_with(bundle) {
    return Ok(true);
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    let output = fs::metadata(output)?;
    let bundle = fs::metadata(bundle)?;
    return Ok(output.dev() == bundle.dev() && output.ino() == bundle.ino());
  }
  #[cfg(windows)]
  {
    return native::same_directory(output, bundle);
  }
  #[cfg(not(any(unix, windows)))]
  {
    Ok(false)
  }
}
/// Read-only, cross-host planning of the exact post-Authenticode bytes to distribute.
pub fn plan_windows_distribution(options: &WindowsDistributionOptions) -> Result<WindowsDistributionPlan> {
  let bundle_source = checked_path(&options.bundle, true)?;
  let bundle = crate::validate_windows_bundle(&bundle_source)?;
  let output = checked_path(&options.output_dir, true)?;
  ensure!(
    !output_overlaps_bundle(&output, &bundle_source)?,
    "distribution output must not be inside its portable source bundle"
  );
  let destination = output.join(output_name(&bundle)?);
  let mut sources = BTreeMap::new();
  let mut files = BTreeMap::new();
  for entry in fs::read_dir(&bundle_source)? {
    let entry = entry?;
    let name = entry
      .file_name()
      .into_string()
      .map_err(|_| anyhow::anyhow!("non-Unicode distribution filename"))?;
    let relative = format!("bundle/{name}");
    relative_path_record(&mut sources, &mut files, relative, entry.path(), &bundle)?;
  }
  for (relative, path) in [(LAUNCHER, &options.launcher), (UPDATER, &options.updater)] {
    let path = checked_path(path, false)?;
    crate::validate_windows_tool(&path, &bundle.target)?;
    relative_path_record(&mut sources, &mut files, relative.into(), path, &bundle)?;
  }
  let public_key = STANDARD.encode(key(&options.public_key)?.as_bytes());
  let bytes = public_key_bytes(&public_key);
  files.insert(KEY_FILE.into(), byte_record(&bytes, Role::UpdateKey));
  let manifest = Manifest {
    schema: 1,
    kind: DOMAIN.into(),
    bundle,
    channel: options.channel.clone(),
    public_key,
    files,
  };
  validate_policy(&manifest)?;
  ensure!(
    serde_json::to_vec(&manifest)?.len() <= MAX_PAYLOAD,
    "distribution manifest payload exceeds 1 MiB"
  );
  ensure!(
    crate::validate_windows_bundle(&bundle_source)? == manifest.bundle,
    "portable inputs changed while planning"
  );
  Ok(WindowsDistributionPlan {
    schema: 1,
    operation: "plan-only: no directory produced or native trust asserted",
    bundle_source,
    sources,
    output,
    destination,
    manifest,
    authenticode_chain_verified: false,
  })
}
fn relative_path_record(
  sources: &mut BTreeMap<String, Source>,
  files: &mut BTreeMap<String, FileRecord>,
  path: String,
  source: PathBuf,
  bundle: &WindowsBundleInfo,
) -> Result<()> {
  validate_relative(&path)?;
  let source = checked_path(&source, false)?;
  let record = record(&source, role(&path, bundle)?)?;
  ensure!(
    files.insert(path.clone(), record.clone()).is_none(),
    "duplicate distribution path"
  );
  sources.insert(path, Source { path: source, record });
  Ok(())
}
fn signing_bytes(payload: &[u8]) -> Vec<u8> {
  [SIGNING_DOMAIN, payload].concat()
}
fn envelope_bytes(envelope: &Envelope) -> Result<Vec<u8>> {
  let mut bytes = serde_json::to_vec_pretty(envelope)?;
  bytes.push(b'\n');
  Ok(bytes)
}
fn read_signing_key(path: &Path, expected: &str) -> Result<SigningKey> {
  let path = checked_path(path, false)?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    ensure!(
      fs::metadata(&path)?.permissions().mode() & 0o077 == 0,
      "distribution signing key must not be accessible to group/other users"
    );
  }
  #[cfg(windows)]
  native::private_key(&path)?;
  let mut encoded = read_bounded(&path, 4096)?;
  let text = std::str::from_utf8(&encoded).context("private key must be base64 seed text")?;
  let mut seed: [u8; 32] = STANDARD
    .decode(text.trim())
    .context("decode distribution signing key")?
    .try_into()
    .map_err(|_| anyhow::anyhow!("distribution signing key must contain 32 bytes"))?;
  let signing = SigningKey::from_bytes(&seed);
  seed.fill(0);
  encoded.fill(0);
  ensure!(
    STANDARD.encode(signing.verifying_key().as_bytes()) == expected,
    "distribution signing key does not match the pinned public key"
  );
  Ok(signing)
}
fn snapshot_matches(plan: &WindowsDistributionPlan) -> Result<()> {
  ensure!(
    checked_path(&plan.bundle_source, true)? == plan.bundle_source && checked_path(&plan.output, true)? == plan.output,
    "distribution paths changed after planning"
  );
  ensure!(
    !output_overlaps_bundle(&plan.output, &plan.bundle_source)?,
    "distribution output now overlaps its portable source"
  );
  ensure!(
    crate::validate_windows_bundle(&plan.bundle_source)? == plan.manifest.bundle,
    "portable identity/bytes changed after planning"
  );
  for source in plan.sources.values() {
    ensure!(
      record(&source.path, source.record.role.clone())? == source.record,
      "distribution tool/code input changed after planning"
    );
  }
  Ok(())
}
fn copy_record(source: &Path, target: &Path, expected: &FileRecord) -> Result<()> {
  let source = checked_path(source, false)?;
  let mut input = fs::File::open(source)?;
  let mut output = fs::File::create_new(target)?;
  let mut hash = Sha256::new();
  let mut size = 0_u64;
  let mut bytes = [0_u8; 65536];
  loop {
    let count = input.read(&mut bytes)?;
    if count == 0 {
      break;
    }
    size = size.checked_add(count as u64).context("copy size overflow")?;
    ensure!(size <= expected.size, "distribution input grew during copy");
    hash.update(&bytes[..count]);
    output.write_all(&bytes[..count])?;
  }
  ensure!(
    size == expected.size && format!("{:x}", hash.finalize()) == expected.sha256,
    "distribution input bytes changed while copying"
  );
  output.sync_all()?;
  Ok(())
}
fn validate_files(directory: &Path, manifest: &Manifest, envelope_expected: bool) -> Result<()> {
  validate_policy(manifest)?;
  let mut actual_files = BTreeSet::new();
  let mut actual_dirs = BTreeSet::new();
  for entry in fs::read_dir(directory)? {
    let entry = entry?;
    let name = entry
      .file_name()
      .into_string()
      .map_err(|_| anyhow::anyhow!("non-Unicode distribution entry"))?;
    let metadata = fs::symlink_metadata(entry.path())?;
    if metadata.is_dir() {
      ensure!(
        matches!(name.as_str(), "bundle" | "tools"),
        "unknown distribution directory"
      );
      checked_path(&entry.path(), true)?;
      actual_dirs.insert(name.clone());
      for child in fs::read_dir(entry.path())? {
        let child = child?;
        let leaf = child
          .file_name()
          .into_string()
          .map_err(|_| anyhow::anyhow!("non-Unicode distribution file"))?;
        let relative = format!("{name}/{leaf}");
        validate_relative(&relative)?;
        ensure!(
          manifest.files.contains_key(&relative),
          "unknown distribution child file"
        );
        checked_path(&child.path(), false)?;
        ensure!(actual_files.insert(relative), "duplicate distribution file");
      }
    } else {
      checked_path(&entry.path(), false)?;
      ensure!(
        matches!(name.as_str(), MANIFEST | KEY_FILE),
        "unknown distribution root file"
      );
      actual_files.insert(name);
    }
  }
  ensure!(
    actual_dirs == BTreeSet::from(["bundle".into(), "tools".into()]),
    "missing distribution directories"
  );
  let expected: BTreeSet<String> = manifest
    .files
    .keys()
    .cloned()
    .chain(envelope_expected.then(|| MANIFEST.to_owned()))
    .collect();
  ensure!(actual_files == expected, "distribution contains missing/unknown files");
  for (path, expected) in &manifest.files {
    ensure!(
      record(&directory.join(path), expected.role.clone())? == *expected,
      "distribution file hash/size mismatch: {path}"
    );
  }
  ensure!(
    read_bounded(&directory.join(KEY_FILE), 4096)? == public_key_bytes(&manifest.public_key),
    "included update key differs from its signed pin"
  );
  ensure!(
    crate::validate_windows_bundle(&directory.join("bundle"))? == manifest.bundle,
    "portable identity/target/version/schemes differ from signed distribution"
  );
  for relative in [LAUNCHER, UPDATER] {
    ensure!(
      crate::validate_windows_tool(&directory.join(relative), &manifest.bundle.target)?
        == manifest.files[relative].sha256,
      "distribution tool target/hash mismatch"
    );
  }
  Ok(())
}
fn decode(directory: &Path, trusted_public_key: &str) -> Result<(WindowsDistributionInfo, Manifest)> {
  let directory = checked_path(directory, true)?;
  let bytes = read_bounded(&directory.join(MANIFEST), MAX_JSON)?;
  let envelope: Envelope = serde_json::from_slice(&bytes).context("read Windows distribution envelope")?;
  ensure!(
    envelope.schema == 1 && envelope.domain == DOMAIN,
    "wrong distribution signature domain/schema"
  );
  ensure!(
    envelope_bytes(&envelope)? == bytes,
    "distribution envelope is not canonical schema-1 JSON"
  );
  let payload = STANDARD.decode(&envelope.payload)?;
  ensure!(payload.len() <= MAX_PAYLOAD, "distribution payload exceeds limit");
  let trusted = key(trusted_public_key)?;
  let signature = Signature::from_slice(&STANDARD.decode(&envelope.signature)?)?;
  trusted
    .verify_strict(&signing_bytes(&payload), &signature)
    .context("invalid distribution signature or caller trust anchor")?;
  let manifest: Manifest = serde_json::from_slice(&payload).context("read signed Windows distribution manifest")?;
  ensure!(
    serde_json::to_vec(&manifest)? == payload,
    "distribution payload is noncanonical/ambiguous JSON"
  );
  ensure!(
    manifest.public_key == STANDARD.encode(trusted.as_bytes()),
    "included key pin is not the caller's trusted key"
  );
  validate_files(&directory, &manifest, true)?;
  let info = WindowsDistributionInfo {
    directory,
    bundle: manifest.bundle.clone(),
    channel: manifest.channel.clone(),
    public_key: manifest.public_key.clone(),
    manifest_sha256: format!("{:x}", Sha256::digest(bytes)),
    file_count: manifest.files.len(),
    ed25519_manifest_verified: true,
    authenticode_chain_verified: false,
    native_installation_executed: false,
  };
  Ok((info, manifest))
}
/// Full read-only signature + exact-tree verification against the explicit caller trust anchor.
pub fn inspect_windows_distribution(directory: &Path, trusted_public_key: &str) -> Result<WindowsDistributionInfo> {
  Ok(decode(directory, trusted_public_key)?.0)
}
/// Produce a real immutable directory. Final hashes are signed only after staged bytes are verified.
pub fn stage_windows_distribution(
  plan: &WindowsDistributionPlan,
  private_key: &Path,
) -> Result<WindowsDistributionInfo> {
  stage_with(plan, private_key, |_| Ok(()))
}
fn stage_with(
  plan: &WindowsDistributionPlan,
  private_key: &Path,
  mut checkpoint: impl FnMut(&Path) -> Result<()>,
) -> Result<WindowsDistributionInfo> {
  snapshot_matches(plan)?;
  let signing = read_signing_key(private_key, &plan.manifest.public_key)?;
  #[cfg(windows)]
  let _pins = native::pin_output(&plan.output)?;
  let _lock = windows::lock_file(&plan.output.join(format!(
      ".{}.lock",
      plan
        .destination
        .file_name()
        .context("distribution has no name")?
        .to_string_lossy()
    )))?;
  match fs::symlink_metadata(&plan.destination) {
    Ok(_) => {
      let (info, existing) = decode(&plan.destination, &plan.manifest.public_key)?;
      ensure!(
        existing == plan.manifest,
        "immutable distribution version already has different contents/key/policy"
      );
      return Ok(info);
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
    Err(error) => return Err(error.into()),
  }
  #[cfg(windows)]
  let stage = crate::windows_signing::native::stage(&plan.output)?;
  #[cfg(not(windows))]
  let stage = tempfile::Builder::new()
    .prefix(".lintel-distribution-")
    .tempdir_in(&plan.output)?;
  let payload = stage.path().join("payload");
  fs::create_dir(&payload)?;
  fs::create_dir(payload.join("bundle"))?;
  fs::create_dir(payload.join("tools"))?;
  for (relative, source) in &plan.sources {
    copy_record(&source.path, &payload.join(relative), &source.record)?;
  }
  let mut key_file = fs::File::create_new(payload.join(KEY_FILE))?;
  key_file.write_all(&public_key_bytes(&plan.manifest.public_key))?;
  key_file.sync_all()?;
  drop(key_file);
  // Validate the exact final files before signing. Envelope is deliberately created last.
  let manifest_path = payload.join(MANIFEST);
  validate_files(&payload, &plan.manifest, false)?;
  checkpoint(&payload)?;
  snapshot_matches(plan)?;
  let bytes = serde_json::to_vec(&plan.manifest)?;
  let envelope = Envelope {
    schema: 1,
    domain: DOMAIN.into(),
    payload: STANDARD.encode(&bytes),
    signature: STANDARD.encode(signing.sign(&signing_bytes(&bytes)).to_bytes()),
  };
  let mut file = fs::File::create_new(&manifest_path)?;
  file.write_all(&envelope_bytes(&envelope)?)?;
  file.sync_all()?;
  drop(file);
  let (info, manifest) = decode(&payload, &plan.manifest.public_key)?;
  ensure!(manifest == plan.manifest, "staged distribution manifest changed");
  checkpoint(&payload)?;
  ensure!(
    inspect_windows_distribution(&payload, &plan.manifest.public_key)? == info,
    "staged distribution changed before publication"
  );
  windows::publish_new_directory(&payload, &plan.destination).context("publish immutable managed distribution")?;
  let published = inspect_windows_distribution(&plan.destination, &plan.manifest.public_key)?;
  ensure!(
    published.manifest_sha256 == info.manifest_sha256,
    "published distribution differs from signed snapshot"
  );
  Ok(published)
}

fn copy_verified_snapshot(
  initial: &WindowsDistributionInfo,
  manifest: &Manifest,
  snapshot: &Path,
  trusted_public_key: &str,
) -> Result<WindowsDistributionInfo> {
  ensure!(
    inspect_windows_distribution(&initial.directory, trusted_public_key)? == *initial,
    "distribution changed after verification"
  );
  fs::create_dir(snapshot)?;
  fs::create_dir(snapshot.join("bundle"))?;
  fs::create_dir(snapshot.join("tools"))?;
  for (path, expected) in &manifest.files {
    copy_record(&initial.directory.join(path), &snapshot.join(path), expected)?;
  }
  let envelope = read_bounded(&initial.directory.join(MANIFEST), MAX_JSON)?;
  ensure!(
    format!("{:x}", Sha256::digest(&envelope)) == initial.manifest_sha256,
    "distribution envelope changed before snapshot copy"
  );
  let mut envelope_file = fs::File::create_new(snapshot.join(MANIFEST))?;
  envelope_file.write_all(&envelope)?;
  envelope_file.sync_all()?;
  drop(envelope_file);
  let verified = inspect_windows_distribution(snapshot, trusted_public_key)?;
  ensure!(
    verified.manifest_sha256 == initial.manifest_sha256,
    "private snapshot differs from the trusted distribution"
  );
  Ok(verified)
}

/// Native file/namespace guard: keep alive through the existing installer's complete consumption.
/// Unknown files can never become install inputs; all actual copies remain validated by that installer.
pub struct WindowsDistributionLease {
  source: PathBuf,
  info: WindowsDistributionInfo,
  #[cfg(windows)]
  _pins: Vec<fs::File>,
  #[cfg(windows)]
  _stage: crate::windows_signing::native::Stage,
  #[cfg(windows)]
  _output_pins: Vec<fs::File>,
}
impl WindowsDistributionLease {
  pub fn source(&self) -> &Path {
    &self.source
  }
  pub fn info(&self) -> &WindowsDistributionInfo {
    &self.info
  }
}
pub fn lease_windows_distribution(directory: &Path, trusted_public_key: &str) -> Result<WindowsDistributionLease> {
  #[cfg(windows)]
  {
    let (initial, manifest) = decode(directory, trusted_public_key)?;
    let _source_pins = native::pin_inputs(&initial.directory, &manifest)?;
    let sealed = inspect_windows_distribution(&initial.directory, trusted_public_key)?;
    ensure!(
      sealed == initial,
      "distribution changed while acquiring its consumption lease"
    );
    let temporary = checked_path(&std::env::temp_dir(), true)?;
    let output_pins = native::pin_output(&temporary)?;
    let stage = crate::windows_signing::native::stage(&temporary)?;
    let snapshot = stage.path().join("inputs");
    let verified = copy_verified_snapshot(&initial, &manifest, &snapshot, trusted_public_key)?;
    let pins = native::pin_inputs(&snapshot, &manifest)?;
    ensure!(
      inspect_windows_distribution(&snapshot, trusted_public_key)? == verified,
      "private distribution snapshot changed while sealing"
    );
    Ok(WindowsDistributionLease {
      source: initial.directory,
      info: verified,
      _pins: pins,
      _stage: stage,
      _output_pins: output_pins,
    })
  }
  #[cfg(not(windows))]
  {
    let _ = (directory, trusted_public_key);
    bail!("native Windows distribution consumption lease requires Windows; inspect remains available on this host")
  }
}

#[cfg(windows)]
#[path = "windows_distribution_native.rs"]
mod native;
#[cfg(test)]
#[path = "windows_distribution_tests.rs"]
mod tests;
