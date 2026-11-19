//! Explicit SDK SignTool plans. Passwords/PFX ingestion and automatic certificate selection are absent.
#[cfg(windows)]
#[path = "windows_signing_native.rs"]
pub(crate) mod native;

use crate::{
  WindowsBundleInfo,
  windows::{self, Architecture},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
  collections::BTreeMap,
  fs,
  io::Read,
  path::{Component, Path, PathBuf},
  time::Duration,
};

// Fixed OS principals only. A service-SID prefix or a friendly service name is not a trust rule.
pub(crate) fn trusted_system_principal(sid: &str) -> bool {
  matches!(
    sid,
    "S-1-5-18" | "S-1-5-32-544" | "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
  )
}

/// Public certificate selector, not a private key. The key remains in the Windows certificate provider.
#[derive(Clone, Debug, Serialize)]
pub struct AuthenticodeOptions {
  signtool: PathBuf,
  certificate_sha1: String,
  certificate_store: String,
  machine_store: bool,
  timestamp_url: String,
  timeout_seconds: u64,
}
impl AuthenticodeOptions {
  pub fn new(
    signtool: impl Into<PathBuf>,
    certificate_sha1: impl Into<String>,
    timestamp_url: impl Into<String>,
  ) -> Result<Self> {
    let thumbprint: String = certificate_sha1
      .into()
      .chars()
      .filter(|value| !value.is_ascii_whitespace())
      .collect();
    ensure!(
      thumbprint.len() == 40 && thumbprint.bytes().all(|value| value.is_ascii_hexdigit()),
      "certificate selector must be an exact 40-hex SHA1 thumbprint (not the file digest)"
    );
    let timestamp_url = timestamp_url.into();
    ensure!(
      !timestamp_url.chars().any(char::is_control),
      "timestamp URL contains control characters"
    );
    let url = url::Url::parse(&timestamp_url).context("invalid RFC3161 timestamp URL")?;
    ensure!(
      url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none(),
      "RFC3161 timestamp URL must be HTTPS without credentials, query tokens or fragment"
    );
    Ok(Self {
      signtool: signtool.into(),
      certificate_sha1: thumbprint.to_ascii_uppercase(),
      certificate_store: "My".into(),
      machine_store: false,
      timestamp_url: url.to_string(),
      timeout_seconds: 300,
    })
  }
  pub fn machine_store(mut self, enabled: bool) -> Self {
    self.machine_store = enabled;
    self
  }
  pub fn certificate_store(mut self, store: &str) -> Result<Self> {
    ensure!(
      !store.is_empty()
        && store.len() <= 64
        && store
          .bytes()
          .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'-' | b'_')),
      "invalid certificate store name"
    );
    self.certificate_store = store.into();
    Ok(self)
  }
  pub fn timeout(mut self, timeout: Duration) -> Result<Self> {
    ensure!(
      timeout.subsec_nanos() == 0 && (1..=600).contains(&timeout.as_secs()),
      "SignTool timeout must be 1..600 whole seconds per phase"
    );
    self.timeout_seconds = timeout.as_secs();
    Ok(self)
  }
}

#[derive(Clone, Debug, Serialize)]
struct Image {
  prefix_sha256: String,
  prefix_len: usize,
  checksum: usize,
  security: usize,
}
#[derive(Clone, Debug, Serialize)]
struct Input {
  name: String,
  sha256: String,
  image: Image,
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind")]
enum Payload {
  Bundle { info: WindowsBundleInfo },
  File { target: String },
}
#[derive(Clone, Debug, Serialize)]
pub struct AuthenticodeCommand {
  pub phase: String,
  pub file: String,
  pub arguments: Vec<String>,
}
/// A read-only snapshot. Private fields prevent reconstructing an unchecked/deserialized execution plan.
#[derive(Clone, Debug, Serialize)]
pub struct AuthenticodePlan {
  schema: u32,
  source: PathBuf,
  output: PathBuf,
  destination: PathBuf,
  options: AuthenticodeOptions,
  signtool_sha256: String,
  inputs: Vec<Input>,
  payload: Payload,
  commands: Vec<AuthenticodeCommand>,
  release_order: &'static str,
}
impl AuthenticodePlan {
  pub fn to_json(&self) -> Result<String> {
    Ok(serde_json::to_string_pretty(self)?)
  }
  pub fn destination(&self) -> &Path {
    &self.destination
  }
}
#[derive(Debug, Serialize)]
pub struct AuthenticodeReceipt {
  pub destination: PathBuf,
  pub native_execution: bool,
  pub completed_phases: usize,
  pub signtool_sha256: String,
  pub certificate_sha1: String,
  pub file_digest: &'static str,
  pub timestamp_digest: &'static str,
  pub embedded_verification: &'static str,
  pub files: BTreeMap<String, String>,
  pub bundle: Option<WindowsBundleInfo>,
  pub release_must_be_created_after_signing: bool,
}
impl AuthenticodeReceipt {
  pub fn to_json(&self) -> Result<String> {
    Ok(serde_json::to_string_pretty(self)?)
  }
}

/// Refuses symlinks/reparse points at every existing component, including input/tool parents.
pub(crate) fn checked_path(path: &Path, directory: bool) -> Result<PathBuf> {
  let absolute = if path.is_absolute() {
    path.to_owned()
  } else {
    std::env::current_dir()?.join(path)
  };
  ensure!(absolute.is_absolute(), "signing paths cannot be drive-relative");
  #[cfg(windows)]
  {
    use std::path::Prefix;
    ensure!(
      matches!(absolute.components().next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))),
      "signing/staging requires a local-drive path, not UNC/device/network paths"
    );
  }
  ensure!(
    !absolute.components().any(|part| matches!(part, Component::ParentDir)),
    "parent traversal is not accepted for signing/staging paths"
  );
  let mut current = PathBuf::new();
  for part in absolute.components() {
    current.push(part.as_os_str());
    let metadata = fs::symlink_metadata(&current)?;
    #[cfg(windows)]
    {
      use std::os::windows::fs::MetadataExt;
      ensure!(
        metadata.file_attributes() & 0x400 == 0,
        "reparse point in signing/staging path"
      );
    }
    ensure!(!metadata.is_symlink(), "symlink in signing/staging path");
    if current != absolute {
      ensure!(metadata.is_dir(), "non-directory path ancestor");
    }
  }
  let metadata = fs::symlink_metadata(&absolute)?;
  ensure!(
    if directory {
      metadata.is_dir()
    } else {
      metadata.is_file()
    },
    "unexpected signing/staging path type"
  );
  Ok(absolute.canonicalize()?)
}
fn output_destination(output: &Path, name: &str) -> Result<(PathBuf, PathBuf)> {
  windows::validate_windows_filename(name)?;
  let output = checked_path(output, true)?;
  let destination = output.join(name);
  match fs::symlink_metadata(&destination) {
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
    _ => bail!("signing destination already exists or cannot be inspected; choose a new output directory"),
  }
  Ok((output, destination))
}
fn tool_options(mut options: AuthenticodeOptions) -> Result<(AuthenticodeOptions, String)> {
  options.signtool = checked_path(&options.signtool, false)?;
  windows::checked_protocol_executable(&options.signtool)
    .context("SignTool must be an explicit real Windows PE executable")?;
  let hash = windows::digest_file(&options.signtool)?;
  Ok((options, hash))
}
fn commands(options: &AuthenticodeOptions, inputs: &[Input]) -> Vec<AuthenticodeCommand> {
  let mut commands = Vec::new();
  for input in inputs {
    let mut sign = vec![
      "sign".into(),
      "/fd".into(),
      "SHA256".into(),
      "/sha1".into(),
      options.certificate_sha1.clone(),
      "/s".into(),
      options.certificate_store.clone(),
    ];
    if options.machine_store {
      sign.push("/sm".into());
    }
    sign.push(format!("<staged:{}>", input.name));
    commands.push(AuthenticodeCommand {
      phase: "sign".into(),
      file: input.name.clone(),
      arguments: sign,
    });
    commands.push(AuthenticodeCommand {
      phase: "timestamp".into(),
      file: input.name.clone(),
      arguments: vec![
        "timestamp".into(),
        "/tr".into(),
        options.timestamp_url.clone(),
        "/td".into(),
        "SHA256".into(),
        format!("<staged:{}>", input.name),
      ],
    });
  }
  // No writable signing/timestamp operation follows this final verification pass.
  for input in inputs {
    commands.push(AuthenticodeCommand {
      phase: "verify".into(),
      file: input.name.clone(),
      arguments: vec![
        "verify".into(),
        "/pa".into(),
        "/all".into(),
        "/tw".into(),
        format!("<staged:{}>", input.name),
      ],
    });
  }
  commands
}
/// Plan on any host, with no signing/network/output mutation. Output directory must already exist.
pub fn plan_windows_authenticode_bundle(
  source: &Path,
  output: &Path,
  options: AuthenticodeOptions,
) -> Result<AuthenticodePlan> {
  let source = checked_path(source, true)?;
  let info = crate::validate_windows_bundle(&source)?;
  let architecture = Architecture::target(&info.target)?;
  let name = source
    .file_name()
    .and_then(|name| name.to_str())
    .context("bundle directory name must be Unicode")?;
  let (output, destination) = output_destination(output, name)?;
  let mut inputs = Vec::new();
  for entry in fs::read_dir(&source)? {
    let entry = entry?;
    let name = entry
      .file_name()
      .into_string()
      .map_err(|_| anyhow::anyhow!("non-Unicode code filename"))?;
    if name == "lintel-bundle.json" || crate::service_processes::is_windows_mojo_notice(&name) {
      continue;
    }
    let path = checked_path(&entry.path(), false)?;
    windows::inspect_pe(&path, architecture, name.to_ascii_lowercase().ends_with(".dll"))?;
    inputs.push(Input {
      name,
      sha256: windows::digest_file(&path)?,
      image: image(&path)?,
    });
  }
  inputs.sort_by(|left, right| left.name.cmp(&right.name));
  let (options, signtool_sha256) = tool_options(options)?;
  let commands = commands(&options, &inputs);
  Ok(AuthenticodePlan {
    schema: 1,
    source,
    output,
    destination,
    options,
    signtool_sha256,
    inputs,
    payload: Payload::Bundle { info },
    commands,
    release_order: "Authenticode -> refreshed portable manifest -> Ed25519 release archive/envelope; prior release metadata cannot authenticate changed bytes",
  })
}
/// Sign standalone launcher/updater/installer EXE or DLL inputs, preserving their filename.
pub fn plan_windows_authenticode_file(
  source: &Path,
  output: &Path,
  target: &str,
  options: AuthenticodeOptions,
) -> Result<AuthenticodePlan> {
  let source = checked_path(source, false)?;
  let name = source
    .file_name()
    .and_then(|name| name.to_str())
    .context("code filename must be Unicode")?
    .to_owned();
  let lower = name.to_ascii_lowercase();
  ensure!(
    lower.ends_with(".exe") || lower.ends_with(".dll"),
    "standalone signing supports PE EXE/DLL only"
  );
  windows::inspect_pe(&source, Architecture::target(target)?, lower.ends_with(".dll"))?;
  let (output, destination) = output_destination(output, &name)?;
  let inputs = vec![Input {
    name,
    sha256: windows::digest_file(&source)?,
    image: image(&source)?,
  }];
  let (options, signtool_sha256) = tool_options(options)?;
  let commands = commands(&options, &inputs);
  Ok(AuthenticodePlan {
    schema: 1,
    source,
    output,
    destination,
    options,
    signtool_sha256,
    inputs,
    payload: Payload::File { target: target.into() },
    commands,
    release_order: "Sign standalone tools before installer staging/ownership hash capture",
  })
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16> {
  Ok(u16::from_le_bytes(
    bytes
      .get(at..at + 2)
      .context("truncated PE signing header")?
      .try_into()?,
  ))
}
fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
  Ok(u32::from_le_bytes(
    bytes
      .get(at..at + 4)
      .context("truncated PE signing header")?
      .try_into()?,
  ))
}
/// Conservative signing subset: certificate table must be after raw data and at EOF; no ambiguous overlay.
fn image(path: &Path) -> Result<Image> {
  ensure!(
    fs::metadata(path)?.len() <= 512 * 1024 * 1024,
    "PE exceeds signing inspection limit"
  );
  let bytes = fs::read(path)?;
  let pe = u32_at(&bytes, 0x3c)? as usize;
  let optional = pe.checked_add(24).context("PE signing offset overflow")?;
  let (minimum, count_offset) = match u16_at(&bytes, optional)? {
    0x10b => (96, 92),
    0x20b => (112, 108),
    _ => bail!("unsupported PE optional magic"),
  };
  ensure!(
    u16_at(&bytes, pe + 20)? as usize >= minimum + 40 && u32_at(&bytes, optional + count_offset)? >= 5,
    "PE does not reserve a certificate data directory"
  );
  let checksum = optional + 64;
  let security = optional + minimum + 32;
  let start = u32_at(&bytes, security)? as usize;
  let size = u32_at(&bytes, security + 4)? as usize;
  let sections = optional + u16_at(&bytes, pe + 20)? as usize;
  let mut raw_end = u32_at(&bytes, optional + 60)? as usize;
  for index in 0..u16_at(&bytes, pe + 6)? as usize {
    let at = sections + index * 40;
    raw_end = raw_end.max(
      (u32_at(&bytes, at + 20)? as usize)
        .checked_add(u32_at(&bytes, at + 16)? as usize)
        .context("PE raw section overflow")?,
    );
  }
  let prefix_len = if start == 0 && size == 0 {
    bytes.len()
  } else {
    ensure!(
      start >= raw_end
        && start % 8 == 0
        && (8..=16 * 1024 * 1024).contains(&size)
        && start.checked_add(size) == Some(bytes.len()),
      "unsupported certificate table placement/size; expected aligned table after code at EOF"
    );
    let mut offset = start;
    let mut entries = 0;
    while offset < bytes.len() {
      let length = u32_at(&bytes, offset)? as usize;
      ensure!(
        length > 8
          && length <= bytes.len() - offset
          && u16_at(&bytes, offset + 4)? == 0x200
          && u16_at(&bytes, offset + 6)? == 2,
        "invalid WIN_CERTIFICATE PKCS7 entry"
      );
      offset = offset
        .checked_add((length + 7) & !7)
        .context("certificate table overflow")?;
      entries += 1;
      ensure!(entries <= 64, "too many certificate entries");
    }
    ensure!(offset == bytes.len(), "certificate alignment exceeds table");
    start
  };
  ensure!(
    security + 8 <= prefix_len && checksum + 4 <= prefix_len,
    "certificate table overlaps headers"
  );
  let mut prefix = bytes[..prefix_len].to_vec();
  prefix[checksum..checksum + 4].fill(0);
  prefix[security..security + 8].fill(0);
  Ok(Image {
    prefix_sha256: format!("{:x}", Sha256::digest(prefix)),
    prefix_len,
    checksum,
    security,
  })
}
fn unchanged_image(path: &Path, expected: &Image) -> Result<()> {
  let actual = image(path)?;
  ensure!(
    actual.checksum == expected.checksum
      && actual.security == expected.security
      && actual.prefix_len >= expected.prefix_len
      && actual.prefix_len - expected.prefix_len <= 7,
    "signer changed PE header/code layout"
  );
  let mut bytes = fs::read(path)?;
  ensure!(
    bytes[expected.prefix_len..actual.prefix_len]
      .iter()
      .all(|value| *value == 0),
    "signer appended non-padding image data"
  );
  bytes[expected.checksum..expected.checksum + 4].fill(0);
  bytes[expected.security..expected.security + 8].fill(0);
  ensure!(
    format!("{:x}", Sha256::digest(&bytes[..expected.prefix_len])) == expected.prefix_sha256,
    "signer changed executable image bytes outside checksum/security/certificate table"
  );
  Ok(())
}
fn signature_present(path: &Path) -> Result<()> {
  let bytes = fs::read(path)?;
  let parsed = image(path)?;
  ensure!(
    parsed.prefix_len < bytes.len(),
    "SignTool did not embed a certificate table"
  );
  Ok(())
}

trait Runner {
  fn run(&mut self, tool: &Path, arguments: &[String], phase: &str, timeout: Duration) -> Result<()>;
}
/// Only Windows can execute the SDK tool. Plans and model tests work on other hosts.
pub fn execute_windows_authenticode(plan: &AuthenticodePlan) -> Result<AuthenticodeReceipt> {
  #[cfg(windows)]
  {
    let _pins = native_pins(plan)?;
    return execute_with(plan, &mut NativeRunner, true);
  }
  #[cfg(not(windows))]
  {
    let _ = plan;
    bail!(
      "native Authenticode execution requires Windows and a trusted Windows SDK SignTool; use plan-only on this host"
    );
  }
}
fn execute_with(plan: &AuthenticodePlan, runner: &mut impl Runner, native: bool) -> Result<AuthenticodeReceipt> {
  ensure!(
    checked_path(&plan.source, matches!(plan.payload, Payload::Bundle { .. }))? == plan.source
      && checked_path(&plan.output, true)? == plan.output,
    "signing path changed after planning"
  );
  ensure!(
    checked_path(&plan.options.signtool, false)? == plan.options.signtool
      && windows::digest_file(&plan.options.signtool)? == plan.signtool_sha256,
    "SignTool changed after planning"
  );
  let lock_name = format!(
    ".{}.authenticode.lock",
    plan
      .destination
      .file_name()
      .context("missing signing destination")?
      .to_string_lossy()
  );
  let _lock = windows::lock_file(&plan.output.join(lock_name))?;
  ensure!(
    fs::symlink_metadata(&plan.destination).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
    "signing destination appeared after planning"
  );
  #[cfg(windows)]
  let stage = native::stage(&plan.output)?;
  #[cfg(not(windows))]
  let stage = tempfile::Builder::new()
    .prefix(".lintel-authenticode-")
    .tempdir_in(&plan.output)?;
  let (payload, bundle_manifest) = match &plan.payload {
    Payload::Bundle { info } => {
      ensure!(
        crate::validate_windows_bundle(&plan.source)? == *info,
        "bundle changed after signing plan"
      );
      let copy = stage.path().join("payload");
      fs::create_dir(&copy)?;
      for entry in fs::read_dir(&plan.source)? {
        let entry = entry?;
        copy_new_bytes(&checked_path(&entry.path(), false)?, &copy.join(entry.file_name()))?;
      }
      ensure!(
        crate::validate_windows_bundle(&copy)? == *info,
        "bundle changed while copying the signing snapshot"
      );
      let bytes = fs::read(copy.join("lintel-bundle.json"))?;
      (copy, Some(bytes))
    }
    Payload::File { .. } => {
      let path = stage.path().join(&plan.inputs[0].name);
      copy_new_bytes(&plan.source, &path)?;
      (stage.path().to_path_buf(), None)
    }
  };
  for input in &plan.inputs {
    ensure!(
      windows::digest_file(&payload.join(&input.name))? == input.sha256,
      "code input changed while staging"
    );
  }
  let mut completed = 0;
  for command in &plan.commands {
    ensure!(
      windows::digest_file(&plan.options.signtool)? == plan.signtool_sha256,
      "SignTool changed between signing phases"
    );
    let path = checked_path(&payload.join(&command.file), false)?;
    let before = input_hashes(&payload, &plan.inputs)?;
    let mut args = command.arguments.clone();
    *args.last_mut().context("empty signing command")? = path.to_str().context("code path must be Unicode")?.into();
    runner
      .run(
        &plan.options.signtool,
        &args,
        &command.phase,
        Duration::from_secs(plan.options.timeout_seconds),
      )
      .with_context(|| {
        format!(
          "Authenticode {} failed for {}; signed output was not published",
          command.phase, command.file
        )
      })?;
    let input = plan
      .inputs
      .iter()
      .find(|input| input.name == command.file)
      .context("unplanned signing target")?;
    unchanged_image(&path, &input.image)?;
    signature_present(&path)?;
    let after = input_hashes(&payload, &plan.inputs)?;
    for input in &plan.inputs {
      ensure!(
        command.phase != "verify" && input.name == command.file || before[&input.name] == after[&input.name],
        "SignTool {} modified an unexpected input {}; output was not published",
        command.phase,
        input.name
      );
      unchanged_image(&payload.join(&input.name), &input.image)?;
    }
    completed += 1;
  }
  for input in &plan.inputs {
    let path = checked_path(&payload.join(&input.name), false)?;
    unchanged_image(&path, &input.image)?;
    signature_present(&path)?;
  }
  let hashes = input_hashes(&payload, &plan.inputs)?;
  let bundle = if let Payload::Bundle { info } = &plan.payload {
    ensure!(
      fs::read(payload.join("lintel-bundle.json"))? == bundle_manifest.context("missing bundle manifest snapshot")?,
      "signing tool modified portable identity/manifest"
    );
    let updated = refresh_manifest(&payload, &hashes)?;
    ensure!(
      updated.identifier == info.identifier
        && updated.version == info.version
        && updated.target == info.target
        && updated.protocol_schemes == info.protocol_schemes,
      "signed bundle identity changed"
    );
    Some(updated)
  } else {
    None
  };
  match &plan.payload {
    Payload::Bundle { .. } => windows::publish_new_directory(&payload, &plan.destination)?,
    Payload::File { target } => {
      windows::inspect_pe(
        &payload.join(&plan.inputs[0].name),
        Architecture::target(target)?,
        plan.inputs[0].name.to_ascii_lowercase().ends_with(".dll"),
      )?;
      fs::hard_link(payload.join(&plan.inputs[0].name), &plan.destination)
        .context("publish standalone signed file without replacement (requires same-volume hard-link support)")?;
    }
  }
  if let Some(expected) = &bundle {
    ensure!(
      crate::validate_windows_bundle(&plan.destination)? == *expected,
      "published signed bundle failed final validation"
    );
  }
  Ok(AuthenticodeReceipt {
    destination: plan.destination.clone(),
    native_execution: native,
    completed_phases: completed,
    signtool_sha256: plan.signtool_sha256.clone(),
    certificate_sha1: plan.options.certificate_sha1.clone(),
    file_digest: "SHA256",
    timestamp_digest: "SHA256",
    embedded_verification: "signtool verify /pa /all /tw; exit 0 required, warnings rejected",
    files: hashes,
    bundle,
    release_must_be_created_after_signing: true,
  })
}
fn copy_new_bytes(source: &Path, destination: &Path) -> Result<()> {
  let mut source = fs::File::open(source)?;
  let mut destination = fs::File::create_new(destination)?;
  std::io::copy(&mut source, &mut destination)?;
  destination.sync_all()?;
  Ok(())
}

fn input_hashes(directory: &Path, inputs: &[Input]) -> Result<BTreeMap<String, String>> {
  inputs
    .iter()
    .map(|input| {
      let path = checked_path(&directory.join(&input.name), false)?;
      Ok((input.name.clone(), windows::digest_file(&path)?))
    })
    .collect()
}

fn refresh_manifest(directory: &Path, hashes: &BTreeMap<String, String>) -> Result<WindowsBundleInfo> {
  let path = directory.join("lintel-bundle.json");
  let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
  let files = manifest
    .get_mut("files")
    .and_then(serde_json::Value::as_object_mut)
    .context("invalid portable file map")?;
  ensure!(
    files.len()
      == hashes.len()
        + files
          .keys()
          .filter(|name| crate::service_processes::is_windows_mojo_notice(name))
          .count()
      && files
        .keys()
        .all(|name| hashes.contains_key(name) || crate::service_processes::is_windows_mojo_notice(name)),
    "signing file set changed"
  );
  for (name, hash) in hashes {
    files.insert(name.clone(), serde_json::Value::String(hash.clone()));
  }
  fs::write(&path, serde_json::to_vec_pretty(&manifest)?)?;
  crate::validate_windows_bundle(directory)
}

#[cfg(windows)]
struct NativeRunner;
#[cfg(windows)]
impl Runner for NativeRunner {
  fn run(&mut self, tool: &Path, arguments: &[String], phase: &str, timeout: Duration) -> Result<()> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(tool)
      .args(arguments)
      .current_dir(tool.parent().context("SignTool has no parent")?)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .context("start explicit Windows SDK SignTool (tool output is not echoed)")?;
    let deadline = std::time::Instant::now() + timeout;
    let result = (|| {
      loop {
        if let Some(status) = child.try_wait()? {
          ensure!(
            status.code() == Some(0),
            "SignTool {phase} returned {:?}; nonzero including warning exit 2 is a failure (output suppressed)",
            status.code()
          );
          return Ok(());
        }
        ensure!(
          std::time::Instant::now() < deadline,
          "SignTool {phase} exceeded the per-phase timeout"
        );
        std::thread::sleep(Duration::from_millis(25));
      }
    })();
    if result.is_err() && !matches!(child.try_wait(), Ok(Some(_))) {
      child
        .kill()
        .context("terminate retained failed/timed-out SignTool child")?;
      child.wait().context("wait for retained SignTool child cleanup")?;
    }
    result
  }
}
#[cfg(windows)]
fn native_pins(plan: &AuthenticodePlan) -> Result<Vec<fs::File>> {
  use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
  native::trusted_writers(&plan.output)?;
  native::trusted_writers(plan.options.signtool.parent().context("SignTool has no parent")?)?;
  native::trusted_writers(&plan.options.signtool)?;
  let mut pins = Vec::new();
  let mut paths = std::collections::BTreeSet::new();
  for directory in [
    plan.options.signtool.parent().context("SignTool has no parent")?,
    plan.output.as_path(),
  ] {
    for parent in directory.ancestors() {
      paths.insert(parent.to_owned());
    }
  }
  for path in paths {
    checked_path(&path, true)?;
    let file = fs::OpenOptions::new()
      .access_mode(0x80)
      .share_mode(3)
      .custom_flags(0x02000000 | 0x00200000)
      .open(path)?;
    ensure!(
      file.metadata()?.is_dir() && file.metadata()?.file_attributes() & 0x400 == 0,
      "directory changed to a reparse point"
    );
    pins.push(file);
  }
  let mut tool = fs::OpenOptions::new()
    .read(true)
    .share_mode(1)
    .custom_flags(0x00200000)
    .open(&plan.options.signtool)?;
  ensure!(
    tool.metadata()?.is_file() && tool.metadata()?.file_attributes() & 0x400 == 0,
    "SignTool became a reparse point"
  );
  let mut hash = Sha256::new();
  let mut bytes = [0_u8; 65536];
  loop {
    let count = tool.read(&mut bytes)?;
    if count == 0 {
      break;
    }
    hash.update(&bytes[..count]);
  }
  ensure!(
    format!("{:x}", hash.finalize()) == plan.signtool_sha256,
    "SignTool handle hash changed after plan"
  );
  pins.push(tool);
  // Audit again after namespace/file handles are retained. A writable outer parent could
  // otherwise swap a public, byte-identical SDK directory between the first audit and open.
  native::trusted_writers(&plan.output)?;
  native::trusted_writers(plan.options.signtool.parent().context("SignTool has no parent")?)?;
  native::trusted_writers(&plan.options.signtool)?;
  Ok(pins)
}

#[cfg(test)]
#[path = "windows_signing_tests.rs"]
mod tests;
