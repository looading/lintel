//! Windows portable bundles and project-local MSVC runtime libraries.

use std::{
  collections::{BTreeMap, BTreeSet},
  fmt,
  fs::{self, File, OpenOptions},
  io::{self, Read},
  path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{BundleExtensions, BundleHelper, BundleOptions, Library, LibraryKind};

const BUNDLE_MANIFEST: &str = "lintel-bundle.json";

/// Features whose Windows bootstrap or distribution contract is not implemented.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum WindowsBundleUnsupported {
  Target(String),
  Cef,
  Updater,
  ProtocolAssociation,
  Library(String),
}

impl fmt::Display for WindowsBundleUnsupported {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Target(target) => write!(formatter, "unsupported Windows target {target:?}; use i686, x86_64, or aarch64-pc-windows-msvc"),
      Self::Cef => formatter.write_str("Windows CEF packaging is unsupported until subprocess bootstrap, sandbox, and runtime resource layout are verified"),
      Self::Updater => formatter.write_str("the bundled updater currently supports macOS only"),
      Self::ProtocolAssociation => formatter.write_str("Windows protocol association requires a verified installer integration and is unsupported by portable bundles"),
      Self::Library(name) => write!(formatter, "Windows runtime library {name:?} must be a .dll; Apple frameworks and dylibs are unsupported"),
    }
  }
}

impl std::error::Error for WindowsBundleUnsupported {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Architecture {
  X86,
  X64,
  Arm64,
}

impl Architecture {
  pub(crate) fn target(target: &str) -> Result<Self> {
    match target {
      "i686-pc-windows-msvc" => Ok(Self::X86),
      "x86_64-pc-windows-msvc" => Ok(Self::X64),
      "aarch64-pc-windows-msvc" => Ok(Self::Arm64),
      _ => Err(WindowsBundleUnsupported::Target(target.to_owned()).into()),
    }
  }

  fn machine(self) -> u16 {
    match self {
      Self::X86 => 0x014c,
      Self::X64 => 0x8664,
      Self::Arm64 => 0xaa64,
    }
  }

  fn name(self) -> &'static str {
    match self {
      Self::X86 => "x86",
      Self::X64 => "x86_64",
      Self::Arm64 => "aarch64",
    }
  }
}

/// Stage and atomically publish an immutable, versioned Windows portable directory.
pub fn bundle_windows(options: &BundleOptions, target: &str) -> Result<PathBuf> {
  bundle_windows_with_libraries(options, target, &[])
}

/// Copy verified DLLs beside the executable; static/import libraries are not distributed.
pub fn bundle_windows_with_libraries(options: &BundleOptions, target: &str, libraries: &[Library]) -> Result<PathBuf> {
  bundle_windows_with_helpers(options, target, libraries, &[])
}

/// Include additional same-machine standalone executables before validating and publishing.
pub fn bundle_windows_with_helpers(
  options: &BundleOptions,
  target: &str,
  libraries: &[Library],
  helpers: &[BundleHelper],
) -> Result<PathBuf> {
  bundle_windows_with_extensions(
    options,
    target,
    libraries,
    &BundleExtensions {
      helpers: helpers.to_vec(),
      protocol_schemes: vec![],
      ..BundleExtensions::default()
    },
  )
}

/// Protocol declarations are metadata only; HKCU changes require the explicit installer/register API.
pub fn bundle_windows_with_extensions(
  options: &BundleOptions,
  target: &str,
  libraries: &[Library],
  extensions: &BundleExtensions,
) -> Result<PathBuf> {
  let helpers = &extensions.helpers;
  crate::validate_helpers(helpers)?;
  crate::validate_protocol_schemes(&extensions.protocol_schemes)?;
  let architecture = Architecture::target(target)?;
  if options.cef_dir.is_some()
    || options.helper.is_some()
    || options.dawn_root.is_some()
    || extensions.cef_crash_reports
  {
    return Err(WindowsBundleUnsupported::Cef.into());
  }
  if options.updater_helper.is_some() {
    return Err(WindowsBundleUnsupported::Updater.into());
  }
  safe_component(&options.name)?;
  safe_component(&options.version)?;
  ensure!(
    options.identifier.contains('.')
      && options.identifier.len() <= 253
      && options
        .identifier
        .split('.')
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')),
    "invalid Windows application identifier {:?}",
    options.identifier
  );
  let executable = regular_file(&options.executable)?;
  inspect_pe(&executable, architecture, false)?;
  let mut names = BTreeSet::from([
    format!("{}.exe", options.name).to_ascii_lowercase(),
    BUNDLE_MANIFEST.to_owned(),
  ]);
  let mut inputs = vec![(format!("{}.exe", options.name), executable, false)];
  for helper in helpers {
    safe_component(&helper.name)?;
    let name = format!("{}.exe", helper.name);
    ensure!(
      names.insert(name.to_ascii_lowercase()),
      "Windows helper filename collision: {name}"
    );
    let source = regular_file(&helper.executable)?;
    inspect_pe(&source, architecture, false)?;
    inputs.push((name, source, false));
  }
  for library in libraries {
    if library.kind == LibraryKind::Static {
      continue;
    }
    let name = library
      .path
      .file_name()
      .and_then(|name| name.to_str())
      .context("DLL requires a UTF-8 basename")?;
    safe_component(name)?;
    if library.kind != LibraryKind::Dylib || !name.to_ascii_lowercase().ends_with(".dll") {
      return Err(WindowsBundleUnsupported::Library(name.to_owned()).into());
    }
    if is_cef(name) {
      return Err(WindowsBundleUnsupported::Cef.into());
    }
    ensure!(
      !system_dll(&name.to_ascii_lowercase()),
      "Windows system DLL {name:?} must not be bundled"
    );
    ensure!(
      names.insert(name.to_ascii_lowercase()),
      "Windows bundle filename collision: {name}"
    );
    let source = regular_file(&library.path)?;
    inspect_pe(&source, architecture, true)?;
    inputs.push((name.to_owned(), source, true));
  }

  if options.output_dir.exists() {
    ensure!(real_directory(&options.output_dir)?, "output must be a real directory");
  }
  fs::create_dir_all(&options.output_dir)?;
  let output = options.output_dir.canonicalize()?;
  let name = format!("{}-{}-{}", options.name, options.version, architecture.name());
  safe_component(&name)?;
  let stage = tempfile::Builder::new().prefix(".lintel-bundle-").tempdir_in(&output)?;
  let staged = stage.path().join(&name);
  fs::create_dir(&staged)?;
  let mut files = BTreeMap::new();
  let mut imports = BTreeSet::new();
  for (name, source, dll) in inputs {
    let destination = staged.join(&name);
    fs::copy(source, &destination).with_context(|| format!("stage Windows binary {name}"))?;
    imports.extend(inspect_pe(&destination, architecture, dll)?);
    files.insert(name, digest_file(&destination)?);
  }
  for library in libraries
    .iter()
    .filter(|library| library.kind == LibraryKind::Dylib && crate::service_processes::is_mojo_runtime(library))
  {
    for notice in crate::service_processes::copy_mojo_notices(library, &staged, true)? {
      let name = notice
        .file_name()
        .and_then(|name| name.to_str())
        .context("Mojo notice has no filename")?;
      ensure!(
        !names.contains(&name.to_ascii_lowercase()) && !files.contains_key(name),
        "Mojo notice filename collision"
      );
      files.insert(name.into(), digest_file(&notice)?);
    }
  }
  for dependency in &imports {
    if is_cef(dependency) {
      return Err(WindowsBundleUnsupported::Cef.into());
    }
    ensure!(
      names.contains(dependency) || system_dll(dependency),
      "missing runtime DLL {dependency:?}; register it before packaging"
    );
  }
  let manifest = BundleManifest {
    schema: 1,
    name: options.name.clone(),
    identifier: options.identifier.clone(),
    version: options.version.clone(),
    target: target.to_owned(),
    files,
    system_imports: imports.into_iter().filter(|name| !names.contains(name)).collect(),
    protocol_schemes: extensions
      .protocol_schemes
      .iter()
      .map(|scheme| scheme.to_ascii_lowercase())
      .collect(),
  };
  fs::write(staged.join(BUNDLE_MANIFEST), serde_json::to_vec_pretty(&manifest)?)?;
  validate_windows_bundle(&staged).context("verify staged Windows portable bundle")?;
  let _lock = lock_file(&output.join(format!(".{name}.lock")))?;
  let destination = output.join(name);
  match fs::symlink_metadata(&destination) {
    Ok(_) => {
      verify_existing(&destination, &manifest)?;
      return Ok(destination);
    }
    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
    Err(error) => return Err(error.into()),
  }
  // 同卷目录重命名一次发布；不以删除旧目录的方式模拟原子替换。
  publish_new_directory(&staged, &destination).context("publish Windows portable bundle")?;
  Ok(destination)
}

#[derive(Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleManifest {
  schema: u32,
  name: String,
  identifier: String,
  version: String,
  target: String,
  files: BTreeMap<String, String>,
  system_imports: Vec<String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  protocol_schemes: Vec<String>,
}

/// Identity from a fully hash- and PE-validated portable bundle.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowsBundleInfo {
  pub name: String,
  pub identifier: String,
  pub version: String,
  pub target: String,
  pub executable: String,
  pub manifest_sha256: String,
  pub protocol_schemes: Vec<String>,
}

pub fn validate_windows_bundle(directory: &Path) -> Result<WindowsBundleInfo> {
  ensure!(real_directory(directory)?, "portable bundle must be a real directory");
  let manifest_path = regular_file(&directory.join(BUNDLE_MANIFEST))?;
  ensure!(
    fs::metadata(&manifest_path)?.len() <= 1024 * 1024,
    "portable manifest exceeds 1 MiB"
  );
  let manifest: BundleManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
  ensure!(
    manifest.schema == 1 && manifest.files.len() <= 4096,
    "invalid portable bundle schema/file count"
  );
  safe_component(&manifest.name)?;
  safe_component(&manifest.version)?;
  ensure!(
    manifest.identifier.contains('.')
      && manifest
        .identifier
        .split('.')
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')),
    "invalid portable application ID"
  );
  crate::validate_protocol_schemes(&manifest.protocol_schemes)?;
  let architecture = Architecture::target(&manifest.target)?;
  let executable = format!("{}.exe", manifest.name);
  ensure!(
    manifest.files.contains_key(&executable),
    "portable bundle is missing its main executable"
  );
  let mut names = BTreeSet::new();
  let mut code_names = BTreeSet::new();
  let mut imports = BTreeSet::new();
  for (name, digest) in &manifest.files {
    safe_component(name)?;
    ensure!(
      valid_digest(digest) && names.insert(name.to_ascii_lowercase()),
      "invalid digest or case-colliding portable filename"
    );
    let lower = name.to_ascii_lowercase();
    if crate::service_processes::is_windows_mojo_notice(name) {
      let path = regular_file(&directory.join(name))?;
      ensure!(
        fs::metadata(&path)?.len() > 0 && fs::metadata(&path)?.len() <= crate::service_processes::MAX_MOJO_NOTICE_BYTES,
        "invalid Mojo notice size"
      );
      ensure!(digest_file(&path)? == *digest, "portable file digest mismatch: {name}");
      continue;
    }
    let dll = lower.ends_with(".dll");
    ensure!(
      dll || lower.ends_with(".exe"),
      "unsupported portable code file {name:?}"
    );
    ensure!(!dll || !system_dll(&lower), "portable bundle overrides a system DLL");
    if is_cef(name) {
      return Err(WindowsBundleUnsupported::Cef.into());
    }
    let path = regular_file(&directory.join(name))?;
    ensure!(digest_file(&path)? == *digest, "portable file digest mismatch: {name}");
    code_names.insert(lower);
    imports.extend(inspect_pe(&path, architecture, dll)?);
  }
  let notice_count = manifest
    .files
    .keys()
    .filter(|name| crate::service_processes::is_windows_mojo_notice(name))
    .count();
  ensure!(
    notice_count
      == if code_names.contains("mojo_rs_native.dll") {
        3
      } else {
        0
      },
    "Mojo runtime requires the exact three upstream notice files"
  );
  for dependency in &imports {
    if is_cef(dependency) {
      return Err(WindowsBundleUnsupported::Cef.into());
    }
    ensure!(
      code_names.contains(dependency) || system_dll(dependency),
      "missing portable runtime DLL {dependency}"
    );
  }
  let system_imports: Vec<_> = imports.into_iter().filter(|name| !code_names.contains(name)).collect();
  ensure!(
    system_imports == manifest.system_imports,
    "portable import manifest does not match the PE files"
  );
  verify_existing(directory, &manifest)?;
  Ok(WindowsBundleInfo {
    name: manifest.name,
    identifier: manifest.identifier,
    version: manifest.version,
    target: manifest.target,
    executable,
    manifest_sha256: digest_file(&manifest_path)?,
    protocol_schemes: manifest.protocol_schemes,
  })
}

/// Copy a checked bundle to an immutable version directory without replacing existing content.
pub fn copy_windows_bundle(source: &Path, output: &Path) -> Result<PathBuf> {
  let info = validate_windows_bundle(source)?;
  copy_windows_snapshot(source, output, info)
}

/// Preserve the manifest captured from authenticated content, including across a shutdown wait.
pub fn copy_windows_bundle_verified(source: &Path, output: &Path, expected: &WindowsBundleInfo) -> Result<PathBuf> {
  let info = validate_windows_bundle(source)?;
  ensure!(
    &info == expected,
    "portable bundle no longer matches the authenticated manifest snapshot"
  );
  copy_windows_snapshot(source, output, info)
}

fn copy_windows_snapshot(source: &Path, output: &Path, info: WindowsBundleInfo) -> Result<PathBuf> {
  if output.exists() {
    ensure!(real_directory(output)?, "version output cannot be linked");
  }
  fs::create_dir_all(output)?;
  let output = output.canonicalize()?;
  let stage = tempfile::Builder::new()
    .prefix(".lintel-version-")
    .tempdir_in(&output)?;
  let name = format!(
    "{}-{}-{}",
    info.name,
    info.version,
    Architecture::target(&info.target)?.name()
  );
  safe_component(&name)?;
  let staged = stage.path().join(&name);
  fs::create_dir(&staged)?;
  for entry in fs::read_dir(source)? {
    let entry = entry?;
    fs::copy(regular_file(&entry.path())?, staged.join(entry.file_name()))?;
  }
  ensure!(
    validate_windows_bundle(&staged)? == info,
    "portable bundle changed while copying"
  );
  let _lock = lock_file(&output.join(format!(".{name}.lock")))?;
  let destination = output.join(name);
  match fs::symlink_metadata(&destination) {
    Ok(_) => {
      ensure!(
        validate_windows_bundle(&destination)? == info,
        "immutable version directory already has different content"
      );
    }
    Err(error) if error.kind() == io::ErrorKind::NotFound => publish_new_directory(&staged, &destination)?,
    Err(error) => return Err(error.into()),
  }
  Ok(destination)
}

/// Stable launch/update tools must have the app's machine and only OS DLL imports.
pub fn validate_windows_tool(path: &Path, target: &str) -> Result<String> {
  let path = regular_file(path)?;
  let imports = inspect_pe(&path, Architecture::target(target)?, false)?;
  ensure!(
    imports.iter().all(|name| system_dll(name)),
    "standalone Windows tool has non-system runtime DLL dependencies"
  );
  digest_file(&path)
}

fn verify_existing(directory: &Path, expected: &BundleManifest) -> Result<()> {
  ensure!(
    real_directory(directory)?,
    "cannot replace a non-directory or linked bundle"
  );
  let manifest = regular_file(&directory.join(BUNDLE_MANIFEST))?;
  ensure!(
    fs::metadata(&manifest)?.len() <= 1024 * 1024,
    "bundle manifest is too large"
  );
  let actual: BundleManifest = serde_json::from_slice(&fs::read(manifest)?)?;
  ensure!(
    &actual == expected,
    "versioned Windows bundle already exists with different contents or identity; use a new version/output directory"
  );
  let mut seen = BTreeSet::new();
  for entry in fs::read_dir(directory)? {
    let entry = entry?;
    let name = entry
      .file_name()
      .into_string()
      .map_err(|_| anyhow::anyhow!("non-UTF-8 file in Windows bundle"))?;
    if name == BUNDLE_MANIFEST {
      continue;
    }
    let expected = expected
      .files
      .get(&name)
      .context("existing bundle contains an unexpected file")?;
    ensure!(
      &digest_file(&regular_file(&entry.path())?)? == expected,
      "existing bundle file {name} was modified"
    );
    seen.insert(name);
  }
  ensure!(seen.len() == expected.files.len(), "existing bundle is missing files");
  Ok(())
}

fn safe_component(value: &str) -> Result<()> {
  let stem = value.split('.').next().unwrap_or("").to_ascii_uppercase();
  let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
    || stem
      .strip_prefix("COM")
      .or_else(|| stem.strip_prefix("LPT"))
      .is_some_and(|digit| digit.len() == 1 && matches!(digit.as_bytes()[0], b'1'..=b'9'));
  ensure!(
    !reserved
      && !value.is_empty()
      && value.len() <= 200
      && value.trim() == value
      && !value.ends_with('.')
      && value.as_bytes()[0].is_ascii_alphanumeric()
      && value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'.' | b'_' | b'-' | b'+')),
    "unsafe Windows filename component {value:?}"
  );
  Ok(())
}

pub fn validate_windows_filename(value: &str) -> Result<()> {
  safe_component(value)
}

pub(crate) fn regular_file(path: &Path) -> Result<PathBuf> {
  let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
  ensure!(
    metadata.is_file() && !linked(&metadata),
    "expected a real file: {}",
    path.display()
  );
  path
    .canonicalize()
    .with_context(|| format!("resolve {}", path.display()))
}

pub(crate) fn real_directory(path: &Path) -> Result<bool> {
  let metadata = fs::symlink_metadata(path)?;
  Ok(metadata.is_dir() && !linked(&metadata))
}

fn linked(metadata: &fs::Metadata) -> bool {
  #[cfg(windows)]
  {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
  }
  #[cfg(not(windows))]
  {
    metadata.file_type().is_symlink()
  }
}

pub(crate) fn lock_file(path: &Path) -> Result<File> {
  #[cfg(target_os = "macos")]
  {
    return crate::macos::lock_output(path);
  }
  #[cfg(not(target_os = "macos"))]
  {
    if let Ok(metadata) = fs::symlink_metadata(path) {
      ensure!(metadata.is_file() && !linked(&metadata), "lock must be a real file");
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
      use std::os::windows::fs::OpenOptionsExt;
      options.custom_flags(0x00200000);
    }
    let lock = options.open(path)?;
    ensure!(!linked(&lock.metadata()?), "lock cannot be a reparse point");
    lock.lock()?;
    Ok(lock)
  }
}

pub(crate) fn digest_file(path: &Path) -> Result<String> {
  let mut file = File::open(path)?;
  let mut hasher = Sha256::new();
  let mut buffer = [0_u8; 64 * 1024];
  loop {
    let count = file.read(&mut buffer)?;
    if count == 0 {
      break;
    }
    hasher.update(&buffer[..count]);
  }
  Ok(format!("{:x}", hasher.finalize()))
}

pub(crate) fn publish_new_directory(staged: &Path, destination: &Path) -> Result<()> {
  #[cfg(target_os = "macos")]
  {
    crate::macos::publish_new_directory(staged, destination)
  }
  #[cfg(windows)]
  {
    fs::rename(staged, destination).map_err(Into::into)
  }
  #[cfg(target_os = "linux")]
  {
    rustix::fs::renameat_with(
      rustix::fs::CWD,
      staged,
      rustix::fs::CWD,
      destination,
      rustix::fs::RenameFlags::NOREPLACE,
    )
    .context("publish directory exclusively with Linux RENAME_NOREPLACE")
  }
  #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
  {
    let _ = (staged, destination);
    bail!("exclusive Windows-directory publication requires macOS, Windows or Linux")
  }
}

fn is_cef(name: &str) -> bool {
  name.eq_ignore_ascii_case("libcef.dll") || name.eq_ignore_ascii_case("chrome_elf.dll")
}

pub(crate) fn system_dll(name: &str) -> bool {
  name.starts_with("api-ms-win-")
    || name.starts_with("ext-ms-win-")
    || [
      "ktmw32.dll",
      "schannel.dll",
      "ncrypt.dll",
      "comdlg32.dll",
      "credui.dll",
      "cryptui.dll",
      "d3d9.dll",
      "dxva2.dll",
      "esent.dll",
      "imagehlp.dll",
      "msi.dll",
      "oleacc.dll",
      "rstrtmgr.dll",
      "uiautomationcore.dll",
      "urlmon.dll",
      "wer.dll",
      "wevtapi.dll",
      "wsock32.dll",
      "dhcpcsvc.dll",
      "kernel32.dll",
      "kernelbase.dll",
      "ntdll.dll",
      "user32.dll",
      "gdi32.dll",
      "gdi32full.dll",
      "advapi32.dll",
      "bcrypt.dll",
      "bcryptprimitives.dll",
      "crypt32.dll",
      "cryptbase.dll",
      "cryptsp.dll",
      "ole32.dll",
      "oleaut32.dll",
      "combase.dll",
      "comctl32.dll",
      "shell32.dll",
      "shlwapi.dll",
      "shcore.dll",
      "version.dll",
      "ws2_32.dll",
      "mswsock.dll",
      "ucrtbase.dll",
      "msvcrt.dll",
      "rpcrt4.dll",
      "secur32.dll",
      "sspicli.dll",
      "d3d11.dll",
      "d3d12.dll",
      "dxgi.dll",
      "dwmapi.dll",
      "dwrite.dll",
      "d2d1.dll",
      "opengl32.dll",
      "glu32.dll",
      "imm32.dll",
      "winmm.dll",
      "winspool.drv",
      "usp10.dll",
      "iphlpapi.dll",
      "setupapi.dll",
      "propsys.dll",
      "winhttp.dll",
      "wininet.dll",
      "dbghelp.dll",
      "msimg32.dll",
      "psapi.dll",
      "wtsapi32.dll",
      "wintrust.dll",
      "normaliz.dll",
      "userenv.dll",
      "powrprof.dll",
      "cfgmgr32.dll",
      "netapi32.dll",
      "wldap32.dll",
      "dnsapi.dll",
      "nsi.dll",
      "dinput8.dll",
      "avrt.dll",
      "dsound.dll",
      "mmdevapi.dll",
      "audioses.dll",
      "cabinet.dll",
      "win32u.dll",
      "msctf.dll",
      "profapi.dll",
      "winusb.dll",
      "uxtheme.dll",
      "hid.dll",
      "winrnr.dll",
      "bthprops.cpl",
      "dcomp.dll",
      "fontsub.dll",
      "mf.dll",
      "mfplat.dll",
      "mfreadwrite.dll",
      "ndfapi.dll",
      "pdh.dll",
    ]
    .contains(&name)
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
  Ok(u16::from_le_bytes(
    bytes
      .get(offset..offset + 2)
      .context("truncated PE/COFF header")?
      .try_into()?,
  ))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
  Ok(u32::from_le_bytes(
    bytes
      .get(offset..offset + 4)
      .context("truncated PE/COFF header")?
      .try_into()?,
  ))
}

pub(crate) fn inspect_pe(path: &Path, architecture: Architecture, dll: bool) -> Result<BTreeSet<String>> {
  ensure!(
    fs::metadata(path)?.len() <= 512 * 1024 * 1024,
    "PE image exceeds 512 MiB inspection limit"
  );
  let bytes = fs::read(path)?;
  ensure!(
    bytes.get(..2) == Some(b"MZ"),
    "{} is not a DOS/PE image",
    path.display()
  );
  let pe = u32_at(&bytes, 0x3c)? as usize;
  ensure!(
    pe >= 64 && pe <= bytes.len().saturating_sub(24),
    "PE header points outside the file"
  );
  ensure!(
    pe >= 64 && bytes.get(pe..pe + 4) == Some(b"PE\0\0"),
    "invalid PE signature in {}",
    path.display()
  );
  ensure!(
    u16_at(&bytes, pe + 4)? == architecture.machine(),
    "PE architecture mismatch in {}; expected {}",
    path.display(),
    architecture.name()
  );
  let sections = u16_at(&bytes, pe + 6)? as usize;
  ensure!((1..=96).contains(&sections), "invalid PE section count");
  let optional_size = u16_at(&bytes, pe + 20)? as usize;
  let characteristics = u16_at(&bytes, pe + 22)?;
  ensure!(
    characteristics & 2 != 0 && (characteristics & 0x2000 != 0) == dll,
    "wrong PE executable/DLL role in {}",
    path.display()
  );
  let optional = pe + 24;
  let (magic, minimum, directory_count) = if architecture == Architecture::X86 {
    (0x10b, 96, 92)
  } else {
    (0x20b, 112, 108)
  };
  ensure!(
    optional_size >= minimum && u16_at(&bytes, optional)? == magic,
    "invalid PE optional header"
  );
  let section_start = optional.checked_add(optional_size).context("PE header overflow")?;
  ensure!(
    section_start
      .checked_add(sections * 40)
      .is_some_and(|end| end <= bytes.len()),
    "truncated PE section table"
  );
  let section_bytes = &bytes[section_start..section_start + sections * 40];
  for section in section_bytes.chunks_exact(40) {
    let size = u32_at(section, 16)? as usize;
    let offset = u32_at(section, 20)? as usize;
    ensure!(
      offset.checked_add(size).is_some_and(|end| end <= bytes.len()),
      "PE section points outside the file"
    );
  }
  let rva_offset = |rva: u32, size: usize| -> Result<usize> {
    if rva < u32_at(&bytes, optional + 60)? {
      let offset = rva as usize;
      ensure!(
        offset.checked_add(size).is_some_and(|end| end <= bytes.len()),
        "PE header RVA outside the file"
      );
      return Ok(offset);
    }
    for section in section_bytes.chunks_exact(40) {
      let start = u32_at(section, 12)?;
      let raw_size = u32_at(section, 16)? as usize;
      if let Some(delta) = rva.checked_sub(start)
        && (delta as usize).checked_add(size).is_some_and(|end| end <= raw_size)
      {
        return Ok(u32_at(section, 20)? as usize + delta as usize);
      }
    }
    bail!("PE RVA points outside initialized section data")
  };
  let count = u32_at(&bytes, optional + directory_count)? as usize;
  ensure!(
    count <= (optional_size - minimum) / 8,
    "PE data-directory count exceeds optional header"
  );
  let mut imports = BTreeSet::new();
  for (index, descriptor_size, name_offset) in [(1, 20, 12), (13, 32, 4)] {
    if count <= index {
      continue;
    }
    let directory = optional + minimum + index * 8;
    let rva = u32_at(&bytes, directory)?;
    let size = u32_at(&bytes, directory + 4)? as usize;
    if rva == 0 && size == 0 {
      continue;
    }
    ensure!(
      rva != 0 && size >= descriptor_size && size / descriptor_size <= 4096,
      "invalid PE import directory size"
    );
    let start = rva_offset(rva, size)?;
    let mut terminated = false;
    for descriptor in bytes[start..start + size].chunks_exact(descriptor_size) {
      if descriptor.iter().all(|byte| *byte == 0) {
        terminated = true;
        break;
      }
      if index == 13 {
        ensure!(
          u32_at(descriptor, 0)? & 1 == 1,
          "VA-based delay imports are unsupported"
        );
      }
      let name = rva_offset(u32_at(descriptor, name_offset)?, 1)?;
      let rest = &bytes[name..bytes.len().min(name + 512)];
      let end = rest
        .iter()
        .position(|byte| *byte == 0)
        .context("unterminated PE import name")?;
      let name = std::str::from_utf8(&rest[..end])?.to_ascii_lowercase();
      safe_component(&name)?;
      ensure!(
        name.ends_with(".dll") || name.ends_with(".drv") || name == "bthprops.cpl",
        "unsupported PE import name {name:?}"
      );
      imports.insert(name);
    }
    ensure!(terminated, "unterminated PE import descriptor table");
  }
  Ok(imports)
}

const REGISTRY: &str = "lintel.windows-libraries.json";
const STORAGE: &str = ".lintel/windows-libraries";

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowsRegistry {
  schema: u32,
  libraries: Vec<WindowsRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowsRecord {
  name: String,
  artifact: String,
  sha256: String,
  kind: LibraryKind,
  version: Option<String>,
  import_library: Option<ImportLibrary>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportLibrary {
  artifact: String,
  sha256: String,
}

impl WindowsRecord {
  fn blob(&self) -> String {
    let mut hash = Sha256::new();
    hash.update(self.artifact.as_bytes());
    hash.update([0]);
    hash.update(self.sha256.as_bytes());
    if let Some(import) = &self.import_library {
      hash.update([0]);
      hash.update(import.artifact.as_bytes());
      hash.update([0]);
      hash.update(import.sha256.as_bytes());
    }
    format!("{:x}", hash.finalize())
  }

  fn validate(&self) -> Result<()> {
    safe_component(&self.name)?;
    safe_component(&self.artifact)?;
    ensure!(valid_digest(&self.sha256), "invalid Windows library digest");
    let expected_kind = artifact_kind(&self.artifact)?;
    ensure!(
      self.kind == expected_kind,
      "Windows library kind does not match its filename"
    );
    if let Some(import) = &self.import_library {
      safe_component(&import.artifact)?;
      ensure!(
        self.kind == LibraryKind::Dylib
          && import
            .artifact
            .eq_ignore_ascii_case(&format!("{}.lib", stem(&self.artifact)?)),
        "wrong DLL import library filename"
      );
      ensure!(valid_digest(&import.sha256), "invalid import library digest");
    }
    validate_version(self.version.as_deref())
  }

  fn library(&self, project: &Path) -> Result<Library> {
    self.validate()?;
    let directory = project.join(STORAGE).join(self.blob());
    ensure!(
      real_directory(&directory)?,
      "Windows library blob must be a real directory"
    );
    let path = regular_file(&directory.join(&self.artifact))?;
    ensure!(
      digest_file(&path)? == self.sha256,
      "Windows library {} digest mismatch",
      self.name
    );
    if let Some(import) = &self.import_library {
      let import_path = regular_file(&directory.join(&import.artifact))?;
      ensure!(
        digest_file(&import_path)? == import.sha256,
        "Windows import library digest mismatch"
      );
    }
    let allowed = if self.import_library.is_some() { 2 } else { 1 };
    ensure!(
      fs::read_dir(&directory)?.count() == allowed,
      "unexpected files in Windows library blob"
    );
    Ok(Library {
      name: self.name.clone(),
      kind: self.kind,
      path,
      link_name: stem(&self.artifact)?.to_owned(),
      version: self.version.clone(),
    })
  }
}

fn valid_digest(value: &str) -> bool {
  value.len() == 64
    && value
      .bytes()
      .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn artifact_kind(name: &str) -> Result<LibraryKind> {
  let lower = name.to_ascii_lowercase();
  if lower.ends_with(".dll") {
    Ok(LibraryKind::Dylib)
  } else if lower.ends_with(".lib") {
    Ok(LibraryKind::Static)
  } else {
    bail!("Windows libraries must be .dll or MSVC .lib files");
  }
}

fn stem(name: &str) -> Result<&str> {
  let value = name.rsplit_once('.').context("library has no extension")?.0;
  safe_component(value)?;
  Ok(value)
}

fn project_directory(project: &Path) -> Result<PathBuf> {
  let project = project.canonicalize()?;
  ensure!(
    project.join("Cargo.toml").is_file(),
    "no Cargo.toml in {}",
    project.display()
  );
  Ok(project)
}

fn validate_version(version: Option<&str>) -> Result<()> {
  if let Some(version) = version {
    ensure!(
      !version.trim().is_empty() && version.len() <= 128 && !version.chars().any(char::is_control),
      "invalid library version label"
    );
  }
  Ok(())
}

fn read_registry(project: &Path) -> Result<WindowsRegistry> {
  let path = project.join(REGISTRY);
  let metadata = match fs::symlink_metadata(&path) {
    Ok(metadata) => metadata,
    Err(error) if error.kind() == io::ErrorKind::NotFound => {
      return Ok(WindowsRegistry {
        schema: 1,
        libraries: vec![],
      });
    }
    Err(error) => return Err(error.into()),
  };
  ensure!(
    metadata.is_file() && !linked(&metadata) && metadata.len() <= 1024 * 1024,
    "Windows registry must be a regular file of at most 1 MiB"
  );
  let registry: WindowsRegistry = serde_json::from_slice(&fs::read(path)?)?;
  ensure!(registry.schema == 1, "unsupported Windows library registry schema");
  let mut names = BTreeSet::new();
  let mut artifacts = BTreeSet::new();
  for record in &registry.libraries {
    record.validate()?;
    ensure!(
      names.insert(record.name.to_ascii_lowercase()) && artifacts.insert(record.artifact.to_ascii_lowercase()),
      "Windows registry has duplicate names or DLL basenames"
    );
  }
  Ok(registry)
}

fn write_registry(project: &Path, registry: &WindowsRegistry) -> Result<()> {
  use std::io::Write;

  let destination = project.join(REGISTRY);
  match fs::symlink_metadata(&destination) {
    Ok(_) => {
      regular_file(&destination)?;
    }
    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
    Err(error) => return Err(error.into()),
  }
  let mut file = tempfile::NamedTempFile::new_in(project)?;
  serde_json::to_writer_pretty(file.as_file_mut(), registry)?;
  file.write_all(b"\n")?;
  file.as_file().sync_all()?;
  file
    .persist(&destination)
    .map_err(|error| error.error)
    .context("publish Windows library registry")?;
  Ok(())
}

/// Read and verify the immutable DLL/import-library snapshot for a Windows Cargo build.
pub fn windows_libraries(project: &Path) -> Result<Vec<Library>> {
  let project = project_directory(project)?;
  let registry = read_registry(&project)?;
  if !registry.libraries.is_empty() {
    ensure!(
      real_directory(&project.join(".lintel"))? && real_directory(&project.join(STORAGE))?,
      "Windows library storage cannot be linked"
    );
  }
  registry
    .libraries
    .iter()
    .map(|record| record.library(&project))
    .collect()
}

/// Register a DLL and its optional adjacent MSVC import library, or a static .lib.
pub fn add_windows_library(project: &Path, name: &str, source: &Path, version: Option<&str>) -> Result<Library> {
  register_library(project, name, source, version, false)
}

/// Atomically replace an active Windows library while preserving old build snapshots.
pub fn update_windows_library(project: &Path, name: &str, source: &Path, version: Option<&str>) -> Result<Library> {
  register_library(project, name, source, version, true)
}

pub fn remove_windows_library(project: &Path, name: &str) -> Result<()> {
  safe_component(name)?;
  let project = project_directory(project)?;
  let _lock = lock_file(&project.join(".lintel-windows-libraries.lock"))?;
  let mut registry = read_registry(&project)?;
  let index = registry
    .libraries
    .iter()
    .position(|entry| entry.name.eq_ignore_ascii_case(name))
    .with_context(|| format!("Windows library {name:?} is not registered"))?;
  registry.libraries.remove(index);
  write_registry(&project, &registry)
}

fn register_library(project: &Path, name: &str, source: &Path, version: Option<&str>, update: bool) -> Result<Library> {
  safe_component(name)?;
  validate_version(version)?;
  let source = regular_file(source)?;
  let artifact = source
    .file_name()
    .and_then(|name| name.to_str())
    .context("library requires a UTF-8 filename")?;
  safe_component(artifact)?;
  let kind = artifact_kind(artifact)?;
  let architecture = if kind == LibraryKind::Dylib {
    let architecture = pe_architecture(&source)?;
    inspect_pe(&source, architecture, true)?;
    architecture
  } else {
    archive_architecture(&source)?
  };
  let import = if kind == LibraryKind::Dylib {
    let expected = format!("{}.lib", stem(artifact)?);
    let mut import = None;
    for entry in fs::read_dir(source.parent().context("library has no parent")?)? {
      let entry = entry?;
      if entry
        .file_name()
        .to_str()
        .is_some_and(|name| name.eq_ignore_ascii_case(&expected))
      {
        ensure!(import.is_none(), "case-insensitive import library filename collision");
        import = Some(regular_file(&entry.path())?);
      }
    }
    if let Some(import) = &import {
      ensure!(
        archive_architecture(import)? == architecture,
        "DLL and import library architectures differ"
      );
    }
    import
  } else {
    None
  };
  let project = project_directory(project)?;
  let _lock = lock_file(&project.join(".lintel-windows-libraries.lock"))?;
  let mut registry = read_registry(&project)?;
  let existing = registry
    .libraries
    .iter()
    .position(|entry| entry.name.eq_ignore_ascii_case(name));
  ensure!(
    existing.is_some() == update,
    "Windows library {name:?} {}",
    if update {
      "is not registered"
    } else {
      "is already registered"
    }
  );
  for (index, entry) in registry.libraries.iter().enumerate() {
    ensure!(
      Some(index) == existing || !entry.artifact.eq_ignore_ascii_case(artifact),
      "Windows artifact basename {artifact:?} is already registered"
    );
  }
  for directory in [project.join(".lintel"), project.join(STORAGE)] {
    if directory.exists() {
      ensure!(
        real_directory(&directory)?,
        "Windows storage directory cannot be linked"
      );
    } else {
      fs::create_dir(&directory)?;
    }
  }
  let stage = tempfile::Builder::new()
    .prefix(".stage-")
    .tempdir_in(project.join(STORAGE))?;
  let blob = stage.path().join("blob");
  fs::create_dir(&blob)?;
  let binary = blob.join(artifact);
  fs::copy(&source, &binary)?;
  if kind == LibraryKind::Dylib {
    inspect_pe(&binary, architecture, true)?;
  } else {
    ensure!(
      archive_architecture(&binary)? == architecture,
      "library changed while registering"
    );
  }
  let import_library = if let Some(import) = import {
    let artifact = format!("{}.lib", stem(artifact)?);
    let destination = blob.join(&artifact);
    fs::copy(import, &destination)?;
    ensure!(
      archive_architecture(&destination)? == architecture,
      "import library changed while registering"
    );
    Some(ImportLibrary {
      artifact,
      sha256: digest_file(&destination)?,
    })
  } else {
    None
  };
  let record = WindowsRecord {
    name: name.to_owned(),
    artifact: artifact.to_owned(),
    sha256: digest_file(&binary)?,
    kind,
    version: version.map(str::to_owned),
    import_library,
  };
  let destination = project.join(STORAGE).join(record.blob());
  match fs::symlink_metadata(&destination) {
    Ok(_) => {
      record.library(&project)?;
    }
    Err(error) if error.kind() == io::ErrorKind::NotFound => {
      publish_new_directory(&blob, &destination).context("publish immutable Windows library")?;
    }
    Err(error) => return Err(error.into()),
  }
  let library = record.library(&project)?;
  if let Some(index) = existing {
    registry.libraries[index] = record;
  } else {
    registry.libraries.push(record);
  }
  registry.libraries.sort_by(|left, right| left.name.cmp(&right.name));
  write_registry(&project, &registry)?;
  Ok(library)
}

fn pe_architecture(path: &Path) -> Result<Architecture> {
  use std::io::{Seek, SeekFrom};
  let mut file = File::open(path)?;
  let mut header = [0_u8; 64];
  file.read_exact(&mut header)?;
  ensure!(&header[..2] == b"MZ", "not a PE image");
  file.seek(SeekFrom::Start(u32_at(&header, 0x3c)? as u64 + 4))?;
  let mut machine = [0; 2];
  file.read_exact(&mut machine)?;
  architecture_from_machine(u16::from_le_bytes(machine))
}

pub(crate) fn checked_protocol_executable(path: &Path) -> Result<PathBuf> {
  let path = regular_file(path)?;
  inspect_pe(&path, pe_architecture(&path)?, false)?;
  Ok(path)
}

fn architecture_from_machine(machine: u16) -> Result<Architecture> {
  match machine {
    0x14c => Ok(Architecture::X86),
    0x8664 => Ok(Architecture::X64),
    0xaa64 => Ok(Architecture::Arm64),
    _ => bail!("unsupported PE/COFF machine 0x{machine:04x}"),
  }
}

pub(crate) fn archive_architecture(path: &Path) -> Result<Architecture> {
  ensure!(
    fs::metadata(path)?.len() <= 512 * 1024 * 1024,
    "COFF archive exceeds inspection limit"
  );
  let bytes = fs::read(path)?;
  ensure!(bytes.get(..8) == Some(b"!<arch>\n"), "MSVC .lib must be a COFF archive");
  let mut offset = 8_usize;
  let mut architecture = None;
  while offset < bytes.len() {
    let header = bytes
      .get(offset..offset + 60)
      .context("truncated COFF archive member")?;
    ensure!(&header[58..] == b"`\n", "invalid COFF archive member");
    let length = std::str::from_utf8(&header[48..58])?.trim().parse::<usize>()?;
    offset += 60;
    let end = offset.checked_add(length).context("COFF archive member overflow")?;
    let member = bytes
      .get(offset..end)
      .context("COFF archive member points outside the file")?;
    let name = std::str::from_utf8(&header[..16])?.trim();
    if !["/", "//", "/SYM64/"].contains(&name) {
      let machine = if member.get(..4) == Some(&[0, 0, 255, 255]) {
        u16_at(member, 6)?
      } else {
        u16_at(member, 0)?
      };
      let actual = architecture_from_machine(machine)?;
      ensure!(
        architecture.is_none_or(|prior| prior == actual),
        "mixed-machine COFF archive"
      );
      architecture = Some(actual);
    }
    offset = end.checked_add(length % 2).context("COFF archive padding overflow")?;
    ensure!(offset <= bytes.len(), "truncated COFF archive padding");
  }
  architecture.context("COFF archive has no machine-specific object members")
}

pub(crate) fn rustc_args(libraries: &[Library], target: &str) -> Result<Vec<std::ffi::OsString>> {
  use std::ffi::OsString;

  let required = Architecture::target(target)?;
  let mut paths = BTreeSet::new();
  let mut names = BTreeSet::new();
  let mut arguments = Vec::new();
  for library in libraries {
    safe_component(&library.link_name)?;
    let source = regular_file(&library.path)?;
    let parent = source.parent().context("library has no parent directory")?;
    let artifact = source
      .file_name()
      .and_then(|name| name.to_str())
      .context("library has no UTF-8 basename")?;
    let extension = if library.kind == LibraryKind::Static {
      "lib"
    } else {
      "dll"
    };
    ensure!(
      artifact.eq_ignore_ascii_case(&format!("{}.{extension}", library.link_name)),
      "MSVC library {} is not linkable by its registered name",
      library.name
    );
    let kind = match library.kind {
      LibraryKind::Static => {
        ensure!(
          archive_architecture(&source)? == required,
          "static library architecture mismatch"
        );
        "static"
      }
      LibraryKind::Dylib => {
        inspect_pe(&source, required, true)?;
        let import = regular_file(&parent.join(format!("{}.lib", library.link_name))).with_context(|| {
          format!(
            "DLL {} requires its adjacent MSVC import library for Cargo linking",
            library.name
          )
        })?;
        ensure!(
          archive_architecture(&import)? == required,
          "DLL import library architecture mismatch"
        );
        "dylib"
      }
      LibraryKind::Framework => return Err(WindowsBundleUnsupported::Library(library.name.clone()).into()),
    };
    ensure!(
      names.insert(library.link_name.to_ascii_lowercase()),
      "duplicate MSVC link target {}",
      library.link_name
    );
    if paths.insert(parent.to_owned()) {
      let mut search = OsString::from("native=");
      search.push(parent);
      arguments.extend([OsString::from("-L"), search]);
    }
    arguments.extend([
      OsString::from("-l"),
      OsString::from(format!("{kind}={}", library.link_name)),
    ]);
  }
  Ok(arguments)
}

#[cfg(test)]
pub(crate) mod tests {
  use std::io::Write;

  use super::*;

  pub(crate) fn pe(path: &Path, architecture: Architecture, dll: bool, imports: &[&str]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = vec![0_u8; 0xa00];
    bytes[..2].copy_from_slice(b"MZ");
    bytes[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
    bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
    bytes[0x84..0x86].copy_from_slice(&architecture.machine().to_le_bytes());
    bytes[0x86..0x88].copy_from_slice(&1_u16.to_le_bytes());
    let (optional_size, magic, directories) = if architecture == Architecture::X86 {
      (224_u16, 0x10b_u16, 96)
    } else {
      (240, 0x20b, 112)
    };
    bytes[0x94..0x96].copy_from_slice(&optional_size.to_le_bytes());
    bytes[0x96..0x98].copy_from_slice(&(if dll { 0x2002_u16 } else { 2_u16 }).to_le_bytes());
    let optional = 0x98;
    bytes[optional..optional + 2].copy_from_slice(&magic.to_le_bytes());
    bytes[optional + 60..optional + 64].copy_from_slice(&0x200_u32.to_le_bytes());
    bytes[optional + directories - 4..optional + directories].copy_from_slice(&16_u32.to_le_bytes());
    let section = optional + optional_size as usize;
    bytes[section..section + 6].copy_from_slice(b".rdata");
    bytes[section + 8..section + 12].copy_from_slice(&0x800_u32.to_le_bytes());
    bytes[section + 12..section + 16].copy_from_slice(&0x1000_u32.to_le_bytes());
    bytes[section + 16..section + 20].copy_from_slice(&0x800_u32.to_le_bytes());
    bytes[section + 20..section + 24].copy_from_slice(&0x200_u32.to_le_bytes());
    if !imports.is_empty() {
      let directory = optional + directories + 8;
      bytes[directory..directory + 4].copy_from_slice(&0x1000_u32.to_le_bytes());
      bytes[directory + 4..directory + 8].copy_from_slice(&(20_u32 * (imports.len() as u32 + 1)).to_le_bytes());
      for (index, name) in imports.iter().enumerate() {
        let offset = 0x100 + index * 128;
        bytes[0x200 + index * 20 + 12..0x200 + index * 20 + 16]
          .copy_from_slice(&(0x1000_u32 + offset as u32).to_le_bytes());
        bytes[0x200 + offset..0x200 + offset + name.len()].copy_from_slice(name.as_bytes());
      }
    }
    fs::write(path, bytes).unwrap();
  }

  pub(crate) fn archive(path: &Path, architecture: Architecture) {
    let mut bytes = b"!<arch>\n".to_vec();
    let header = format!(
      "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
      "member.obj/", 0, 0, 0, "100644", 20
    );
    assert_eq!(header.len(), 60);
    bytes.extend(header.as_bytes());
    let mut object = [0_u8; 20];
    object[..2].copy_from_slice(&architecture.machine().to_le_bytes());
    bytes.extend(object);
    fs::write(path, bytes).unwrap();
  }

  fn options(root: &Path) -> BundleOptions {
    BundleOptions {
      executable: root.join("source/main.exe"),
      name: "Fixture".into(),
      identifier: "dev.lintel.fixture".into(),
      version: "1.0.0".into(),
      output_dir: root.join("output"),
      cef_dir: None,
      helper: None,
      dawn_root: None,
      updater_helper: None,
    }
  }

  fn library(path: PathBuf) -> Library {
    Library {
      name: "native".into(),
      kind: LibraryKind::Dylib,
      link_name: "native".into(),
      path,
      version: None,
    }
  }

  #[test]
  fn portable_layout_places_matching_dlls_beside_the_executable_and_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let options = options(root.path());
    pe(
      &options.executable,
      Architecture::X64,
      false,
      &["kernel32.dll", "native.dll"],
    );
    let dll = root.path().join("source/native.dll");
    pe(&dll, Architecture::X64, true, &["user32.dll"]);
    let libraries = [library(dll)];
    let bundle = bundle_windows_with_libraries(&options, "x86_64-pc-windows-msvc", &libraries).unwrap();
    assert_eq!(
      bundle,
      options.output_dir.canonicalize().unwrap().join("Fixture-1.0.0-x86_64")
    );
    assert!(bundle.join("Fixture.exe").is_file() && bundle.join("native.dll").is_file());
    assert_eq!(fs::read_dir(&bundle).unwrap().count(), 3);
    assert_eq!(
      bundle_windows_with_libraries(&options, "x86_64-pc-windows-msvc", &libraries).unwrap(),
      bundle
    );
    let manifest: BundleManifest = serde_json::from_slice(&fs::read(bundle.join(BUNDLE_MANIFEST)).unwrap()).unwrap();
    assert_eq!(manifest.system_imports, ["kernel32.dll", "user32.dll"]);
  }

  #[test]
  fn all_supported_pe_machines_are_checked_against_the_explicit_target() {
    let root = tempfile::tempdir().unwrap();
    for (architecture, target) in [
      (Architecture::X86, "i686-pc-windows-msvc"),
      (Architecture::X64, "x86_64-pc-windows-msvc"),
      (Architecture::Arm64, "aarch64-pc-windows-msvc"),
    ] {
      let mut options = options(root.path());
      options.name = format!("Fixture-{}", architecture.name());
      pe(&options.executable, architecture, false, &[]);
      assert!(bundle_windows(&options, target).is_ok());
      pe(
        &options.executable,
        if architecture == Architecture::X64 {
          Architecture::X86
        } else {
          Architecture::X64
        },
        false,
        &[],
      );
      assert!(bundle_windows(&options, target).is_err());
    }
  }

  #[test]
  fn failed_validation_or_changed_same_version_preserves_the_previous_bundle() {
    let root = tempfile::tempdir().unwrap();
    let options = options(root.path());
    pe(&options.executable, Architecture::X64, false, &[]);
    let bundle = bundle_windows(&options, "x86_64-pc-windows-msvc").unwrap();
    let previous = fs::read(bundle.join("Fixture.exe")).unwrap();
    pe(&options.executable, Architecture::X64, false, &["unregistered.dll"]);
    assert!(
      bundle_windows(&options, "x86_64-pc-windows-msvc")
        .unwrap_err()
        .to_string()
        .contains("missing runtime DLL")
    );
    assert_eq!(fs::read(bundle.join("Fixture.exe")).unwrap(), previous);
    pe(&options.executable, Architecture::X64, false, &[]);
    OpenOptions::new()
      .append(true)
      .open(&options.executable)
      .unwrap()
      .write_all(b"changed")
      .unwrap();
    assert!(
      bundle_windows(&options, "x86_64-pc-windows-msvc")
        .unwrap_err()
        .to_string()
        .contains("already exists")
    );
    assert_eq!(fs::read(bundle.join("Fixture.exe")).unwrap(), previous);
    assert!(!fs::read_dir(&options.output_dir).unwrap().any(|entry| {
      entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".lintel-bundle-")
    }));
  }

  #[test]
  fn cef_and_unknown_targets_return_typed_unsupported_errors() {
    let root = tempfile::tempdir().unwrap();
    let mut options = options(root.path());
    options.cef_dir = Some(root.path().join("cef"));
    let error = bundle_windows(&options, "x86_64-pc-windows-msvc").unwrap_err();
    assert_eq!(
      error.downcast_ref::<WindowsBundleUnsupported>(),
      Some(&WindowsBundleUnsupported::Cef)
    );
    options.cef_dir = None;
    pe(&options.executable, Architecture::X64, false, &["libcef.dll"]);
    assert_eq!(
      bundle_windows(&options, "x86_64-pc-windows-msvc")
        .unwrap_err()
        .downcast_ref::<WindowsBundleUnsupported>(),
      Some(&WindowsBundleUnsupported::Cef)
    );
    assert!(matches!(
      bundle_windows(&options, "x86_64-pc-windows-gnu")
        .unwrap_err()
        .downcast_ref::<WindowsBundleUnsupported>(),
      Some(WindowsBundleUnsupported::Target(_))
    ));
  }

  #[test]
  fn unsafe_names_case_collisions_and_truncated_pe_images_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let mut options = options(root.path());
    pe(&options.executable, Architecture::X64, false, &[]);
    for invalid in [
      "../escape",
      "CON",
      "NUL.app",
      "name.",
      "name:stream",
      "LPT1",
      "trailing ",
      "a/b",
    ] {
      options.name = invalid.into();
      assert!(
        bundle_windows(&options, "x86_64-pc-windows-msvc").is_err(),
        "accepted {invalid}"
      );
    }
    options.name = "Fixture".into();
    let first = root.path().join("first/Native.dll");
    let second = root.path().join("second/native.DLL");
    pe(&first, Architecture::X64, true, &[]);
    pe(&second, Architecture::X64, true, &[]);
    assert!(
      bundle_windows_with_libraries(&options, "x86_64-pc-windows-msvc", &[library(first), library(second)])
        .unwrap_err()
        .to_string()
        .contains("collision")
    );
    fs::write(&options.executable, b"MZ").unwrap();
    assert!(bundle_windows(&options, "x86_64-pc-windows-msvc").is_err());
    let mut file = File::create(&options.executable).unwrap();
    file.write_all(&[0; 64]).unwrap();
    assert!(bundle_windows(&options, "x86_64-pc-windows-msvc").is_err());
  }

  #[cfg(unix)]
  #[test]
  fn symlink_inputs_and_existing_destination_are_not_followed_or_replaced() {
    let root = tempfile::tempdir().unwrap();
    let mut options = options(root.path());
    pe(&options.executable, Architecture::X64, false, &[]);
    let link = root.path().join("linked.exe");
    std::os::unix::fs::symlink(&options.executable, &link).unwrap();
    let original = options.executable.clone();
    options.executable = link;
    assert!(bundle_windows(&options, "x86_64-pc-windows-msvc").is_err());
    options.executable = original;
    fs::create_dir(&options.output_dir).unwrap();
    let external = root.path().join("external");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("keep"), b"untouched").unwrap();
    std::os::unix::fs::symlink(&external, options.output_dir.join("Fixture-1.0.0-x86_64")).unwrap();
    assert!(bundle_windows(&options, "x86_64-pc-windows-msvc").is_err());
    assert_eq!(fs::read(external.join("keep")).unwrap(), b"untouched");
  }

  #[test]
  fn windows_registry_preserves_snapshots_links_import_libraries_and_detects_tampering() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
      root.path().join("Cargo.toml"),
      "[package]\nname='fixture'\nversion='1.0.0'\n",
    )
    .unwrap();
    let dll = root.path().join("native.dll");
    pe(&dll, Architecture::X64, true, &[]);
    archive(&root.path().join("native.lib"), Architecture::X64);
    let old = add_windows_library(root.path(), "native", &dll, Some("1")).unwrap();
    assert!(old.path.with_extension("lib").is_file());
    assert_eq!(windows_libraries(root.path()).unwrap(), [old.clone()]);
    let arguments = rustc_args(&[old.clone()], "x86_64-pc-windows-msvc").unwrap();
    assert!(arguments.contains(&"dylib=native".into()));
    let mut mismatched = old.clone();
    mismatched.link_name = "unverified".into();
    assert!(rustc_args(&[mismatched], "x86_64-pc-windows-msvc").is_err());
    assert!(rustc_args(&[old.clone()], "aarch64-pc-windows-msvc").is_err());
    assert!(add_windows_library(root.path(), "NATIVE", &dll, None).is_err());
    OpenOptions::new()
      .append(true)
      .open(&dll)
      .unwrap()
      .write_all(b"new")
      .unwrap();
    let new = update_windows_library(root.path(), "native", &dll, Some("2")).unwrap();
    assert_ne!(old.path, new.path);
    assert!(old.path.is_file());
    fs::write(&new.path, b"corrupt").unwrap();
    assert!(windows_libraries(root.path()).is_err());
    remove_windows_library(root.path(), "native").unwrap();
    assert!(windows_libraries(root.path()).unwrap().is_empty());
    assert!(old.path.is_file());
  }

  #[test]
  fn registration_rejects_wrong_machine_import_library_before_publishing() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
      root.path().join("Cargo.toml"),
      "[package]\nname='fixture'\nversion='1.0.0'\n",
    )
    .unwrap();
    let dll = root.path().join("native.dll");
    pe(&dll, Architecture::X64, true, &[]);
    archive(&root.path().join("native.lib"), Architecture::Arm64);
    assert!(add_windows_library(root.path(), "native", &dll, None).is_err());
    assert!(!root.path().join(REGISTRY).exists());
    assert!(!root.path().join(STORAGE).exists());
  }

  #[test]
  fn standalone_helpers_have_matching_machine_and_do_not_replace_a_good_bundle_on_failure() {
    let root = tempfile::tempdir().unwrap();
    let options = options(root.path());
    pe(&options.executable, Architecture::X64, false, &[]);
    let helper = root.path().join("source/helper.exe");
    pe(&helper, Architecture::X64, false, &["kernel32.dll"]);
    let helpers = [BundleHelper {
      name: "lintel-crash-helper".into(),
      executable: helper.clone(),
    }];
    let bundle = bundle_windows_with_helpers(&options, "x86_64-pc-windows-msvc", &[], &helpers).unwrap();
    let installed = fs::read(bundle.join("lintel-crash-helper.exe")).unwrap();
    pe(&helper, Architecture::Arm64, false, &[]);
    assert!(bundle_windows_with_helpers(&options, "x86_64-pc-windows-msvc", &[], &helpers).is_err());
    assert_eq!(fs::read(bundle.join("lintel-crash-helper.exe")).unwrap(), installed);
    let collision = [BundleHelper {
      name: "fixture".into(),
      executable: options.executable.clone(),
    }];
    assert!(
      bundle_windows_with_helpers(&options, "x86_64-pc-windows-msvc", &[], &collision)
        .unwrap_err()
        .to_string()
        .contains("collision")
    );
  }

  #[test]
  fn mojo_notices_are_hash_covered_copied_and_required_before_atomic_publication() {
    let root = tempfile::tempdir().unwrap();
    let options = options(root.path());
    pe(&options.executable, Architecture::X64, false, &["mojo_rs_native.dll"]);
    let image = root.path().join("sdk/mojo_rs_native.dll");
    pe(&image, Architecture::X64, true, &[]);
    let library = crate::mojo_runtime_library(&image, Some("x86_64-pc-windows-msvc")).unwrap();
    assert!(bundle_windows_with_libraries(&options, "x86_64-pc-windows-msvc", std::slice::from_ref(&library)).is_err());
    for name in crate::service_processes::MOJO_NOTICE_FILES {
      fs::write(image.parent().unwrap().join(name), format!("upstream notice: {name}")).unwrap();
    }
    let bundle =
      bundle_windows_with_libraries(&options, "x86_64-pc-windows-msvc", std::slice::from_ref(&library)).unwrap();
    let accepted = validate_windows_bundle(&bundle).unwrap();
    let copied = copy_windows_bundle(&bundle, &root.path().join("versions")).unwrap();
    for name in crate::service_processes::MOJO_NOTICE_FILES {
      let file = format!("mojo-{name}");
      assert_eq!(
        fs::read(copied.join(&file)).unwrap(),
        fs::read(bundle.join(&file)).unwrap()
      );
    }
    fs::remove_file(image.parent().unwrap().join("LICENSE")).unwrap();
    assert!(bundle_windows_with_libraries(&options, "x86_64-pc-windows-msvc", &[library]).is_err());
    assert_eq!(validate_windows_bundle(&bundle).unwrap(), accepted);
    fs::write(copied.join("mojo-LICENSE"), b"changed notice").unwrap();
    assert!(validate_windows_bundle(&copied).is_err());
    let mut manifest: BundleManifest =
      serde_json::from_slice(&fs::read(bundle.join(BUNDLE_MANIFEST)).unwrap()).unwrap();
    fs::write(bundle.join("unknown-notice.txt"), b"unapproved").unwrap();
    manifest.files.insert(
      "unknown-notice.txt".into(),
      digest_file(&bundle.join("unknown-notice.txt")).unwrap(),
    );
    fs::write(bundle.join(BUNDLE_MANIFEST), serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(validate_windows_bundle(&bundle).is_err());
  }

  pub(crate) fn add_mojo_notice_fixture(bundle: &Path) {
    let image = bundle.join("mojo_rs_native.dll");
    pe(&image, Architecture::X64, true, &[]);
    let mut manifest: BundleManifest =
      serde_json::from_slice(&fs::read(bundle.join(BUNDLE_MANIFEST)).unwrap()).unwrap();
    manifest
      .files
      .insert("mojo_rs_native.dll".into(), digest_file(&image).unwrap());
    for name in crate::service_processes::MOJO_NOTICE_FILES {
      let name = format!("mojo-{name}");
      fs::write(bundle.join(&name), format!("exact upstream {name}")).unwrap();
      manifest
        .files
        .insert(name.clone(), digest_file(&bundle.join(name)).unwrap());
    }
    fs::write(bundle.join(BUNDLE_MANIFEST), serde_json::to_vec(&manifest).unwrap()).unwrap();
    validate_windows_bundle(bundle).unwrap();
  }

  #[cfg(any(target_os = "macos", target_os = "windows"))]
  #[test]
  fn exclusive_publication_rejects_an_empty_destination_created_after_validation() {
    let root = tempfile::tempdir().unwrap();
    let staged = root.path().join("staged");
    let destination = root.path().join("published");
    fs::create_dir(&staged).unwrap();
    fs::write(staged.join("payload"), b"keep staged").unwrap();
    fs::create_dir(&destination).unwrap();
    assert!(publish_new_directory(&staged, &destination).is_err());
    assert_eq!(fs::read(staged.join("payload")).unwrap(), b"keep staged");
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
  }
}

/// Named, initialized executable exports. Forwarded/decorated entrypoints do not prove the GUI ABI.
pub(crate) fn pe_code_exports(path: &Path, architecture: Architecture) -> Result<BTreeSet<String>> {
  inspect_pe(path, architecture, true)?;
  let bytes = fs::read(path)?;
  let pe = u32_at(&bytes, 0x3c)? as usize;
  let optional = pe + 24;
  let minimum = if architecture == Architecture::X86 { 96 } else { 112 };
  let directory_count = if architecture == Architecture::X86 { 92 } else { 108 };
  if u32_at(&bytes, optional + directory_count)? == 0 {
    return Ok(BTreeSet::new());
  }
  let start = optional + u16_at(&bytes, pe + 20)? as usize;
  let sections = &bytes[start..start + u16_at(&bytes, pe + 6)? as usize * 40];
  let offset = |rva: u32, size: usize| -> Result<usize> {
    if rva < u32_at(&bytes, optional + 60)? {
      ensure!(
        (rva as usize)
          .checked_add(size)
          .is_some_and(|end| end <= u32_at(&bytes, optional + 60).unwrap_or(0) as usize && end <= bytes.len()),
        "export header RVA outside initialized data"
      );
      return Ok(rva as usize);
    }
    for section in sections.chunks_exact(40) {
      if let Some(delta) = rva.checked_sub(u32_at(section, 12)?) {
        if (delta as usize)
          .checked_add(size)
          .is_some_and(|end| end <= u32_at(section, 16).unwrap_or(0) as usize)
        {
          return Ok(u32_at(section, 20)? as usize + delta as usize);
        }
      }
    }
    bail!("export RVA outside initialized section data")
  };
  let rva = u32_at(&bytes, optional + minimum)?;
  let size = u32_at(&bytes, optional + minimum + 4)?;
  if rva == 0 && size == 0 {
    return Ok(BTreeSet::new());
  }
  ensure!(rva != 0 && size >= 40, "invalid export directory");
  let directory = offset(rva, size as usize)?;
  let functions = u32_at(&bytes, directory + 20)? as usize;
  let names = u32_at(&bytes, directory + 24)? as usize;
  ensure!(
    functions <= 65536 && names <= functions,
    "export count exceeds inspection bound"
  );
  let addresses = offset(u32_at(&bytes, directory + 28)?, functions * 4)?;
  let pointers = offset(u32_at(&bytes, directory + 32)?, names * 4)?;
  let ordinals = offset(u32_at(&bytes, directory + 36)?, names * 2)?;
  let end = rva.checked_add(size).context("export directory RVA overflow")?;
  let mut exports = BTreeSet::new();
  for index in 0..names {
    let ordinal = u16_at(&bytes, ordinals + index * 2)? as usize;
    ensure!(ordinal < functions, "export ordinal outside address table");
    let address = u32_at(&bytes, addresses + ordinal * 4)?;
    if address == 0 || (rva..end).contains(&address) {
      continue;
    }
    let code = sections.chunks_exact(40).any(|section| {
      let start = u32_at(section, 12).unwrap_or(0);
      let length = u32_at(section, 16).unwrap_or(0);
      address.checked_sub(start).is_some_and(|delta| delta < length)
        && u32_at(section, 36).unwrap_or(0) & 0x20000000 != 0
    });
    if !code {
      continue;
    }
    offset(address, 1)?;
    let name_rva = u32_at(&bytes, pointers + index * 4)?;
    let mut name = Vec::new();
    for index in 0..512_u32 {
      let at = offset(name_rva.checked_add(index).context("export name RVA overflow")?, 1)?;
      if bytes[at] == 0 {
        break;
      }
      ensure!(bytes[at].is_ascii_graphic(), "non-ASCII export name");
      name.push(bytes[at]);
    }
    ensure!(!name.is_empty() && name.len() < 512, "unterminated export name");
    ensure!(exports.insert(String::from_utf8(name)?), "duplicate export name");
  }
  Ok(exports)
}

#[cfg(test)]
mod bluetooth_import_tests {
  use super::*;
  #[test]
  fn accepts_only_the_documented_bluetooth_system_cpl_import() {
    let root = tempfile::tempdir().unwrap();
    let image = root.path().join("bluetooth.exe");
    tests::pe(&image, Architecture::X64, false, &["bthprops.cpl"]);
    assert!(
      inspect_pe(&image, Architecture::X64, false)
        .unwrap()
        .contains("bthprops.cpl")
    );
    assert!(system_dll("bthprops.cpl"));
    tests::pe(&image, Architecture::X64, false, &["arbitrary.cpl"]);
    assert!(inspect_pe(&image, Architecture::X64, false).is_err());
    assert!(!system_dll("arbitrary.cpl"));
  }
}
