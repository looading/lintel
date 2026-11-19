//! Assemble native and CEF-backed macOS application bundles.

use std::{
  collections::BTreeSet,
  ffi::CString,
  fs::{self, OpenOptions},
  io::{self, Read, Seek},
  os::unix::{
    ffi::OsStrExt,
    fs::{OpenOptionsExt, PermissionsExt},
  },
  path::{Path, PathBuf},
  process::{Command, Output},
};

use anyhow::{Context, Result, ensure};
use plist::{Dictionary, Value};

use crate::third_party::Library;
use crate::{BundleExtensions, BundleOptions, CEF_HELPER_ROLES as HELPER_KINDS, NotarizeOptions, SignOptions};

const FRAMEWORK: &str = "Chromium Embedded Framework.framework";
const CEF_BINARY: &str = "Chromium Embedded Framework";
const CEF_SANDBOX: &str = "Libraries/libcef_sandbox.dylib";
const DAWN_LIBRARY: &str = "libwebgpu_dawn.dylib";

/// Assemble the app in a staging directory on the same volume, then publish it atomically.
pub(crate) fn bundle(
  options: &BundleOptions,
  libraries: &[Library],
  signing: Option<&SignOptions>,
  notarization: Option<&NotarizeOptions>,
  extensions: &BundleExtensions,
) -> Result<PathBuf> {
  validate_options(options)?;
  let executable = checked_executable(&options.executable)?;
  let architectures = binary_architectures(&executable)?;

  fs::create_dir_all(&options.output_dir)
    .with_context(|| format!("create output directory {}", options.output_dir.display()))?;
  let output_dir = options.output_dir.canonicalize()?;
  let stage = tempfile::Builder::new()
    .prefix(".lintel-bundle-")
    .tempdir_in(&output_dir)?;
  let app_name = format!("{}.app", options.name);
  let staged = stage.path().join(&app_name);
  let destination = output_dir.join(&app_name);
  let contents = staged.join("Contents");
  let staged_main = contents.join("MacOS").join(&options.name);
  let frameworks = contents.join("Frameworks");

  make_app(
    &contents,
    &executable,
    &options.name,
    &options.identifier,
    &options.version,
    false,
  )?;
  if extensions.cef_crash_reports {
    write_cef_crash_config(&contents)?;
  }
  if !extensions.protocol_schemes.is_empty() {
    let plist_path = contents.join("Info.plist");
    let mut info = Value::from_file(&plist_path)?;
    let dictionary = info
      .as_dictionary_mut()
      .context("main Info.plist is not a dictionary")?;
    let mut url_type = Dictionary::new();
    url_type.insert("CFBundleURLName".to_owned(), Value::String(options.identifier.clone()));
    url_type.insert(
      "CFBundleURLSchemes".to_owned(),
      Value::Array(
        extensions
          .protocol_schemes
          .iter()
          .map(|scheme| Value::String(scheme.to_ascii_lowercase()))
          .collect(),
      ),
    );
    dictionary.insert(
      "CFBundleURLTypes".to_owned(),
      Value::Array(vec![Value::Dictionary(url_type)]),
    );
    info.to_file_xml(&plist_path)?;
  }
  let mut cef_runtime_roots = Vec::new();
  let mut helpers = if let Some(cef_dir) = &options.cef_dir {
    let helpers = bundle_cef(
      options,
      &contents,
      &executable,
      cef_dir,
      &architectures,
      extensions.cef_crash_reports,
    )?;
    let framework = frameworks.join(FRAMEWORK);
    cef_runtime_roots.push(framework.join(CEF_BINARY));
    let sandbox = framework.join(CEF_SANDBOX);
    if sandbox.is_file() {
      cef_runtime_roots.push(sandbox);
    }
    helpers
  } else {
    Vec::new()
  };
  if let Some(helper) = &options.updater_helper {
    let helper = checked_executable(helper)?;
    require_architectures(&helper, &architectures)?;
    let destination = contents.join("Helpers/lintel-updater");
    fs::create_dir_all(destination.parent().context("update helper has no parent")?)?;
    fs::copy(helper, &destination)?;
    helpers.push(destination);
  }
  for helper in &extensions.helpers {
    ensure!(
      options.updater_helper.is_none() || !helper.name.eq_ignore_ascii_case("lintel-updater"),
      "standalone helper collides with the bundled updater"
    );
    let metadata = fs::symlink_metadata(&helper.executable)?;
    ensure!(
      metadata.is_file() && !metadata.is_symlink(),
      "standalone helper must be a real file"
    );
    let source = checked_executable(&helper.executable)?;
    crate::signing_components::require_executable_image(&source)?;
    require_architectures(&source, &architectures)?;
    let destination = contents.join("Helpers").join(&helper.name);
    fs::create_dir_all(destination.parent().context("standalone helper has no parent")?)?;
    ensure!(!destination.exists(), "standalone helper destination already exists");
    fs::copy(source, &destination)?;
    crate::signing_components::require_executable_image(&destination)?;
    require_architectures(&destination, &architectures)?;
    helpers.push(destination);
  }
  let runtime_roots = crate::native_libraries::bundle(libraries, &frameworks, &architectures)?;
  crate::bundle_dependencies::verify_bundle(
    &contents,
    &staged_main,
    &helpers,
    &runtime_roots,
    &cef_runtime_roots,
    &architectures,
  )
  .context("verify packaged Mach-O dependencies")?;
  // Resource links also affect relocation, even when no signing was requested.
  // Reuse the read-only structure scan; this never signs or changes the inputs.
  crate::signing_components::components(&staged).context("verify staged application structure and resource links")?;
  if let Some(signing) = signing {
    crate::signing::sign_staged(&staged, signing)?;
  }
  if let Some(notarization) = notarization {
    crate::notarization::notarize_staged(&staged, notarization)?;
  }

  publish(
    &staged,
    &destination,
    &output_dir.join(format!(".{app_name}.lock")),
    &options.identifier,
  )?;
  Ok(destination)
}

fn validate_options(options: &BundleOptions) -> Result<()> {
  let name = &options.name;
  ensure!(
    !name.is_empty()
      && name.trim() == name
      && name != "."
      && name != ".."
      && !name.contains(['/', '\\', ':'])
      && !name.chars().any(char::is_control),
    "invalid macOS application name {name:?}"
  );
  let identifier = &options.identifier;
  ensure!(
    identifier.contains('.')
      && identifier
        .split('.')
        .all(|part| { !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') }),
    "invalid macOS bundle identifier {identifier:?}; use a dotted identifier such as dev.lintel.example"
  );
  ensure!(
    !options.version.trim().is_empty() && !options.version.chars().any(char::is_control),
    "invalid macOS bundle version"
  );
  ensure!(
    options.cef_dir.is_some() || (options.helper.is_none() && options.dawn_root.is_none()),
    "a helper executable or Dawn root requires a CEF distribution"
  );
  Ok(())
}

fn write_cef_crash_config(contents: &Path) -> Result<()> {
  use std::io::Write;
  let destination = contents.join("Resources/crash_reporter.cfg");
  let mut file = OpenOptions::new()
    .write(true)
    .create_new(true)
    .open(&destination)
    .context("create local-only CEF crash config without overwriting resources")?;
  file.write_all(crate::CEF_LOCAL_CRASH_REPORTER_CONFIG.as_bytes())?;
  Ok(())
}

#[cfg(test)]
mod cef_crash_config_tests {
  #[test]
  fn local_only_config_has_no_upload_url_and_never_overwrites_existing_resources() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("Resources")).unwrap();
    super::write_cef_crash_config(root.path()).unwrap();
    let path = root.path().join("Resources/crash_reporter.cfg");
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes, crate::CEF_LOCAL_CRASH_REPORTER_CONFIG.as_bytes());
    assert!(std::str::from_utf8(&bytes).unwrap().contains("ServerURL=\n"));
    assert!(super::write_cef_crash_config(root.path()).is_err());
    assert_eq!(std::fs::read(path).unwrap(), bytes);
  }

  #[test]
  fn crashpad_background_flag_is_applied_only_to_explicit_cef_helpers() {
    let root = tempfile::tempdir().unwrap();
    let binary = root.path().join("binary");
    std::fs::write(&binary, b"layout fixture only").unwrap();
    let main = root.path().join("Main.app/Contents");
    let helper = root.path().join("Helper.app/Contents");
    super::make_app(&main, &binary, "Main", "dev.fixture.main", "1.0.0", false).unwrap();
    super::make_app(&helper, &binary, "Helper", "dev.fixture.helper", "1.0.0", true).unwrap();
    let default = plist::Value::from_file(helper.join("Info.plist")).unwrap();
    assert!(!default.as_dictionary().unwrap().contains_key("LSBackgroundOnly"));
    super::mark_background_helper(&helper).unwrap();
    let enabled = plist::Value::from_file(helper.join("Info.plist")).unwrap();
    assert_eq!(
      enabled.as_dictionary().unwrap()["LSBackgroundOnly"].as_boolean(),
      Some(true)
    );
    let main_info = plist::Value::from_file(main.join("Info.plist")).unwrap();
    assert!(!main_info.as_dictionary().unwrap().contains_key("LSBackgroundOnly"));
    assert!(!main.join("Resources/crash_reporter.cfg").exists());
  }
}

fn update_identity(app: &Path) -> Result<(String, String, PathBuf)> {
  ensure!(
    fs::symlink_metadata(app)?.is_dir() && app.extension().is_some_and(|ext| ext == "app"),
    "update path must be a real .app"
  );
  let info = Value::from_file(app.join("Contents/Info.plist"))?;
  let info = info.as_dictionary().context("invalid application Info.plist")?;
  let string = |key| {
    info
      .get(key)
      .and_then(Value::as_string)
      .with_context(|| format!("missing {key}"))
  };
  let name = string("CFBundleExecutable")?;
  ensure!(
    !name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != "..",
    "invalid app executable"
  );
  Ok((
    string("CFBundleIdentifier")?.to_owned(),
    string("CFBundleShortVersionString")?.to_owned(),
    app.join("Contents/MacOS").join(name),
  ))
}

pub(crate) fn validate_update(
  candidate: &Path,
  installed: &Path,
  id: &str,
  old_version: &str,
  new_version: &str,
) -> Result<()> {
  let (current_id, current_version, current_exe) = update_identity(installed)?;
  let (next_id, next_version, next_exe) = update_identity(candidate)?;
  ensure!(current_id == id && next_id == id, "update application ID mismatch");
  ensure!(
    current_version == old_version && next_version == new_version,
    "update application version changed or does not match signed manifest"
  );
  require_architectures(&next_exe, &binary_architectures(&current_exe)?)?;
  crate::signing::verify_signature(candidate).context("verify update signature")?;
  crate::signing::verify_signature(installed).context("verify installed signature")?;
  Ok(())
}

pub(crate) fn install_update(
  candidate: &Path,
  installed: &Path,
  id: &str,
  old_version: &str,
  new_version: &str,
  relaunch: bool,
  before_publish: impl FnOnce() -> Result<()>,
) -> Result<()> {
  ensure!(
    fs::symlink_metadata(installed)?.is_dir(),
    "installed application cannot be a symlink"
  );
  let installed = installed.canonicalize()?;
  let parent = installed.parent().context("installed app has no parent")?;
  let name = installed.file_name().context("installed app has no name")?;
  let lock = lock_output(&parent.join(format!(".{}.lock", name.to_string_lossy())))?;
  validate_update(candidate, &installed, id, old_version, new_version)?;
  let stage = tempfile::Builder::new()
    .prefix(".lintel-install-")
    .tempdir_in(parent)
    .context("create update staging directory beside installed application (no elevation is requested)")?;
  let staged = stage.path().join(name);
  run_tool(Command::new("/usr/bin/ditto").arg(candidate).arg(&staged))?;
  validate_update(&staged, &installed, id, old_version, new_version)?;
  let (_, _, next_exe) = update_identity(&staged)?;
  let executable_name = next_exe.file_name().context("update executable has no name")?;
  before_publish().context("application reopened before atomic update publication; original bundle retained")?;
  publish_locked(&staged, &installed, id, &lock)?;
  if relaunch {
    let spawn = |executable: &Path| {
      let mut command = Command::new(executable);
      command
        .current_dir(parent)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
      for name in [
        "CEF_PATH",
        "DAWN_ROOT",
        "DYLD_LIBRARY_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "DYLD_FALLBACK_FRAMEWORK_PATH",
        "DYLD_INSERT_LIBRARIES",
      ] {
        command.env_remove(name);
      }
      command.spawn()
    };
    if let Err(error) = spawn(&installed.join("Contents/MacOS").join(executable_name)) {
      publish_locked(&staged, &installed, id, &lock).context("restore original application after failed relaunch")?;
      let (_, _, restored) = update_identity(&installed)?;
      spawn(&restored).context("original application restored but could not be restarted")?;
      return Err(error).context("relaunch failed; original application restored and restarted");
    }
  }
  Ok(())
}

fn checked_executable(path: &Path) -> Result<PathBuf> {
  let path = path
    .canonicalize()
    .with_context(|| format!("locate executable {}", path.display()))?;
  let metadata = fs::metadata(&path)?;
  ensure!(
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
    "not an executable file: {}",
    path.display()
  );
  Ok(path)
}

fn make_app(contents: &Path, binary: &Path, name: &str, identifier: &str, version: &str, helper: bool) -> Result<()> {
  let executable_dir = contents.join("MacOS");
  fs::create_dir_all(&executable_dir)?;
  fs::create_dir_all(contents.join("Resources"))?;
  fs::copy(binary, executable_dir.join(name)).with_context(|| format!("copy executable {}", binary.display()))?;

  let mut info = Dictionary::new();
  for (key, value) in [
    ("CFBundleExecutable", name),
    ("CFBundleIdentifier", identifier),
    ("CFBundleName", name),
    ("CFBundleDisplayName", name),
    ("CFBundlePackageType", "APPL"),
    ("CFBundleVersion", "1"),
    ("CFBundleShortVersionString", version),
  ] {
    info.insert(key.into(), Value::String(value.into()));
  }
  info.insert("LSUIElement".into(), Value::Boolean(helper));
  Value::Dictionary(info)
    .to_file_xml(contents.join("Info.plist"))
    .with_context(|| format!("write Info.plist in {}", contents.display()))?;
  Ok(())
}

fn bundle_cef(
  options: &BundleOptions,
  contents: &Path,
  executable: &Path,
  cef_dir: &Path,
  architectures: &BTreeSet<String>,
  crash_reports: bool,
) -> Result<Vec<PathBuf>> {
  for config in [
    cef_dir.join("crash_reporter.cfg"),
    cef_dir.join("Resources/crash_reporter.cfg"),
    cef_dir.join(FRAMEWORK).join("Resources/crash_reporter.cfg"),
  ] {
    ensure!(
      fs::symlink_metadata(&config).is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
      "external CEF crash_reporter.cfg conflicts with explicit local-only bundle configuration: {}",
      config.display()
    );
  }
  let helper = options.helper.as_deref().map(checked_executable).transpose()?;
  let helper = helper.as_deref().unwrap_or(executable);
  require_architectures(helper, architectures)?;

  let source = cef_dir
    .join(FRAMEWORK)
    .canonicalize()
    .with_context(|| format!("locate CEF Framework in {}", cef_dir.display()))?;
  let cef_binary = source.join(CEF_BINARY);
  require_architectures(&cef_binary, architectures)?;
  let sandbox = source.join(CEF_SANDBOX);
  if sandbox.is_file() {
    require_architectures(&sandbox, architectures)?;
  }

  // Inspect optional graphics runtime dependencies absent from the Rust graph.
  let linked = run_tool(
    Command::new("/usr/bin/otool")
      .arg("-L")
      .args([executable, helper, &cef_binary]),
  )?;
  let linked = String::from_utf8_lossy(&linked.stdout);
  let dawn = if linked.contains("@rpath/libwebgpu_dawn.dylib") {
    let root = options
      .dawn_root
      .as_ref()
      .context("this executable or CEF Framework requires a Dawn runtime root")?;
    let library = root
      .join("lib")
      .join(DAWN_LIBRARY)
      .canonicalize()
      .with_context(|| format!("locate {DAWN_LIBRARY} in {}", root.display()))?;
    require_architectures(&library, architectures)?;
    Some(library)
  } else {
    None
  };

  let frameworks = contents.join("Frameworks");
  let framework = frameworks.join(FRAMEWORK);
  fs::create_dir_all(&frameworks)?;
  // ditto preserves the versioned Framework's symlinks and bundled sandbox library.
  run_tool(Command::new("/usr/bin/ditto").arg(&source).arg(&framework))?;
  copy_cef_notices(cef_dir, &contents.join("Resources/cef-notices"))?;
  if let Some(dawn) = dawn {
    fs::copy(dawn, frameworks.join(DAWN_LIBRARY))?;
  }
  let cache = framework.join("Resources/gpu_shader_cache.bin");
  if cache.is_file() {
    fs::set_permissions(cache, fs::Permissions::from_mode(0o644))?;
  }

  let mut helpers = Vec::with_capacity(HELPER_KINDS.len());
  for kind in HELPER_KINDS {
    let name = if kind == "default" {
      format!("{} Helper", options.name)
    } else {
      format!("{} Helper ({kind})", options.name)
    };
    let identifier = format!("{}.helper.{kind}", options.identifier);
    let helper_contents = frameworks.join(format!("{name}.app/Contents"));
    make_app(&helper_contents, helper, &name, &identifier, &options.version, true)?;
    if crash_reports {
      mark_background_helper(&helper_contents)?;
    }
    helpers.push(helper_contents.join("MacOS").join(name));
  }
  Ok(helpers)
}

fn mark_background_helper(contents: &Path) -> Result<()> {
  let path = contents.join("Info.plist");
  let mut info = Value::from_file(&path)?;
  info
    .as_dictionary_mut()
    .context("invalid CEF helper Info.plist")?
    .insert("LSBackgroundOnly".into(), Value::Boolean(true));
  info.to_file_xml(path)?;
  Ok(())
}

const CEF_NOTICE_NAMES: [&str; 2] = ["LICENSE.txt", "CREDITS.html"];
const MAX_CEF_NOTICE_BYTES: u64 = 64 * 1024 * 1024;

fn open_real_file(path: &Path) -> Result<fs::File> {
  let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
  ensure!(
    metadata.is_file() && !metadata.is_symlink(),
    "input must be a real file: {}",
    path.display()
  );
  let file = OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_NOFOLLOW)
    .open(path)
    .with_context(|| format!("open real file {}", path.display()))?;
  ensure!(
    file.metadata()?.is_file(),
    "input changed to a non-file: {}",
    path.display()
  );
  Ok(file)
}

fn read_bounded_real_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
  let file = open_real_file(path)?;
  ensure!(
    file.metadata()?.len() > 0 && file.metadata()?.len() <= limit,
    "input must be nonempty and at most {limit} bytes: {}",
    path.display()
  );
  let mut bytes = Vec::new();
  file.take(limit + 1).read_to_end(&mut bytes)?;
  ensure!(
    !bytes.is_empty() && bytes.len() as u64 <= limit,
    "input changed or exceeded its size limit: {}",
    path.display()
  );
  Ok(bytes)
}

fn archived_cef_notice(archive: &mut fs::File, entry: &str) -> Result<Vec<u8>> {
  archive.rewind()?;
  let mut child = Command::new("/usr/bin/tar")
    .args(["-xOqf", "-"])
    .arg(entry)
    .stdin(archive.try_clone()?)
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .spawn()
    .context("read CEF notice from verified archive")?;
  let mut bytes = Vec::new();
  let read = child
    .stdout
    .take()
    .context("open CEF notice output")?
    .take(MAX_CEF_NOTICE_BYTES + 1)
    .read_to_end(&mut bytes);
  if read.is_err() || bytes.len() as u64 > MAX_CEF_NOTICE_BYTES {
    let _ = child.kill();
    let _ = child.wait();
    read.context("read bounded CEF notice")?;
    anyhow::bail!("CEF notice exceeds {MAX_CEF_NOTICE_BYTES} bytes: {entry}");
  }
  ensure!(
    child.wait()?.success(),
    "CEF archive does not contain a readable notice: {entry}"
  );
  ensure!(!bytes.is_empty(), "CEF archive notice is empty: {entry}");
  Ok(bytes)
}

fn copy_cef_notices(cef_dir: &Path, destination: &Path) -> Result<()> {
  // Normalize `--cef-path .` as well as package-relative paths before finding
  // the sibling cache archive. This does not modify the selected SDK.
  let cef_dir = cef_dir.canonicalize().context("locate CEF notice distribution")?;
  let mut notices = std::collections::BTreeMap::new();
  let mut missing = Vec::new();
  for name in CEF_NOTICE_NAMES {
    let path = cef_dir.join(name);
    match fs::symlink_metadata(&path) {
      Ok(_) => {
        notices.insert(name, read_bounded_real_file(&path, MAX_CEF_NOTICE_BYTES)?);
      }
      Err(error) if error.kind() == io::ErrorKind::NotFound => missing.push(name),
      Err(error) => return Err(error).with_context(|| format!("inspect CEF notice {}", path.display())),
    }
  }
  if !missing.is_empty() {
    // cef-dll-sys keeps its checked download next to the normalized runtime.
    // Retain this supported cache layout, but never silently omit its notices.
    let metadata_path = cef_dir.join("archive.json");
    let metadata: serde_json::Value = serde_json::from_slice(
      &read_bounded_real_file(&metadata_path, 64 * 1024)
        .with_context(|| format!("CEF distribution is missing required notices: {}", missing.join(", ")))?,
    )?;
    let name = metadata["name"].as_str().context("CEF archive metadata has no name")?;
    let prefix = name
      .strip_suffix(".tar.bz2")
      .filter(|prefix| prefix.starts_with("cef_binary_"))
      .filter(|_| name.len() <= 255 && !name.contains(['/', '\\']) && !name.chars().any(char::is_control))
      .context("CEF archive metadata has an invalid distribution name")?;
    let expected = metadata["sha1"]
      .as_str()
      .filter(|sha| sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
      .context("CEF archive metadata requires a SHA-1 checksum")?;
    let mut archive = open_real_file(&cef_dir.with_file_name(name)).context("locate CEF notice fallback archive")?;
    let checksum = run_tool(
      Command::new("/usr/bin/shasum")
        .args(["-a", "1"])
        .stdin(archive.try_clone()?),
    )?;
    let actual = std::str::from_utf8(&checksum.stdout)?
      .split_whitespace()
      .next()
      .context("read CEF archive checksum")?;
    ensure!(
      actual.eq_ignore_ascii_case(expected),
      "CEF notice archive checksum does not match {}",
      metadata_path.display()
    );
    for name in missing {
      notices.insert(name, archived_cef_notice(&mut archive, &format!("{prefix}/{name}"))?);
    }
  }
  fs::create_dir_all(destination)?;
  for (name, bytes) in notices {
    use std::io::Write;
    let mut output = fs::File::create_new(destination.join(name))?;
    output.write_all(&bytes)?;
  }
  Ok(())
}

#[cfg(test)]
mod cef_packaging {
  use super::*;
  use std::{io::Write, os::unix::fs::symlink};

  struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    sdk: PathBuf,
    resources: PathBuf,
    options: BundleOptions,
  }

  fn fixture() -> Fixture {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let sdk = root.join("sdk");
    let framework = sdk.join(FRAMEWORK);
    let version = framework.join("Versions/A");
    let resources = version.join("Resources");
    fs::create_dir_all(&resources).unwrap();
    fs::write(resources.join("locale.dat"), b"CEF fixture locale").unwrap();
    symlink("A", framework.join("Versions/Current")).unwrap();
    symlink(format!("Versions/Current/{CEF_BINARY}"), framework.join(CEF_BINARY)).unwrap();
    symlink("Versions/Current/Resources", framework.join("Resources")).unwrap();
    let source = root.join("runtime.c");
    fs::write(&source, "int cef_fixture(void) { return 1; }\n").unwrap();
    assert!(
      Command::new("/usr/bin/clang")
        .arg("-dynamiclib")
        .arg(&source)
        .arg(format!("-Wl,-install_name,@rpath/{FRAMEWORK}/Versions/A/{CEF_BINARY}"))
        .arg("-o")
        .arg(version.join(CEF_BINARY))
        .status()
        .unwrap()
        .success()
    );
    fs::write(&source, "int main(void) { return 0; }\n").unwrap();
    let executable = root.join("main");
    assert!(
      Command::new("/usr/bin/clang")
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .status()
        .unwrap()
        .success()
    );
    for name in CEF_NOTICE_NAMES {
      fs::write(sdk.join(name), format!("exact CEF fixture {name}\n")).unwrap();
    }
    let options = BundleOptions {
      executable,
      name: "CEF Packaging Fixture".into(),
      identifier: "dev.lintel.cef-packaging".into(),
      version: "1.0.0".into(),
      output_dir: root.join("output"),
      cef_dir: Some(sdk.clone()),
      helper: None,
      dawn_root: None,
      updater_helper: None,
    };
    Fixture {
      _temporary: temporary,
      root,
      sdk,
      resources,
      options,
    }
  }

  fn publish(fixture: &Fixture) -> Result<PathBuf> {
    bundle(&fixture.options, &[], None, None, &BundleExtensions::default())
  }

  #[test]
  fn missing_empty_linked_and_oversized_notices_never_replace_an_existing_app() {
    let fixture = fixture();
    let accepted = publish(&fixture).unwrap();
    let main = accepted.join("Contents/MacOS/CEF Packaging Fixture");
    let original = fs::read(&main).unwrap();
    let notice = fixture.sdk.join("LICENSE.txt");
    let bytes = fs::read(&notice).unwrap();
    for mode in ["missing", "empty", "linked", "oversized"] {
      fs::remove_file(&notice).unwrap();
      match mode {
        "missing" => {}
        "empty" => fs::write(&notice, []).unwrap(),
        "linked" => symlink(fixture.sdk.join("CREDITS.html"), &notice).unwrap(),
        _ => fs::File::create(&notice)
          .unwrap()
          .set_len(MAX_CEF_NOTICE_BYTES + 1)
          .unwrap(),
      }
      assert!(publish(&fixture).is_err(), "notice mode {mode}");
      assert_eq!(fs::read(&main).unwrap(), original);
      assert_eq!(
        fs::read(accepted.join("Contents/Resources/cef-notices/LICENSE.txt")).unwrap(),
        bytes
      );
      if fs::symlink_metadata(&notice).is_ok() {
        fs::remove_file(&notice).unwrap();
      }
      fs::write(&notice, &bytes).unwrap();
    }
    assert_eq!(fs::read(&notice).unwrap(), bytes);
  }

  #[test]
  fn unsigned_bundles_preserve_version_links_and_reject_external_resource_links_atomically() {
    let fixture = fixture();
    symlink("locale.dat", fixture.resources.join("locale-alias.dat")).unwrap();
    let accepted = publish(&fixture).unwrap();
    let framework = accepted.join("Contents/Frameworks").join(FRAMEWORK);
    assert_eq!(
      fs::read_link(framework.join("Versions/Current")).unwrap(),
      Path::new("A")
    );
    assert_eq!(
      fs::read_link(framework.join("Resources")).unwrap(),
      Path::new("Versions/Current/Resources")
    );
    assert_eq!(
      fs::read(framework.join("Resources/locale-alias.dat")).unwrap(),
      b"CEF fixture locale"
    );
    assert!(!accepted.join("Contents/_CodeSignature").exists());
    let main = accepted.join("Contents/MacOS/CEF Packaging Fixture");
    let original = fs::read(&main).unwrap();
    let external = fixture.root.join("outside.dat");
    fs::write(&external, b"outside SDK data").unwrap();
    let link = fixture.resources.join("external-resource.dat");
    for target in [external.as_path(), Path::new("../../../../../../../../outside.dat")] {
      symlink(target, &link).unwrap();
      let error = publish(&fixture).unwrap_err();
      assert!(format!("{error:#}").contains("symlink"));
      assert_eq!(fs::read(&main).unwrap(), original);
      assert!(!framework.join("Resources/external-resource.dat").exists());
      fs::remove_file(&link).unwrap();
    }
    assert_eq!(fs::read(&external).unwrap(), b"outside SDK data");
  }

  fn archive_fixture(root: &Path, credits: Option<&[u8]>) -> (PathBuf, PathBuf, Vec<u8>) {
    let sdk = root.join("normalized-runtime");
    fs::create_dir_all(&sdk).unwrap();
    let name = "cef_binary_notice_fixture";
    let source = root.join(name);
    fs::create_dir_all(&source).unwrap();
    fs::write(source.join("LICENSE.txt"), b"verified CEF license\n").unwrap();
    if let Some(credits) = credits {
      fs::write(source.join("CREDITS.html"), credits).unwrap();
    }
    let archive = root.join(format!("{name}.tar.bz2"));
    assert!(
      Command::new("/usr/bin/tar")
        .arg("-cjf")
        .arg(&archive)
        .arg("-C")
        .arg(root)
        .arg(name)
        .status()
        .unwrap()
        .success()
    );
    let hash = run_tool(Command::new("/usr/bin/shasum").args(["-a", "1"]).arg(&archive)).unwrap();
    let hash = std::str::from_utf8(&hash.stdout)
      .unwrap()
      .split_whitespace()
      .next()
      .unwrap();
    let metadata = serde_json::to_vec(&serde_json::json!({"name":format!("{name}.tar.bz2"),"sha1":hash})).unwrap();
    fs::write(sdk.join("archive.json"), &metadata).unwrap();
    (sdk, archive, metadata)
  }

  #[test]
  fn normalized_cache_fallback_requires_matching_archive_and_preserves_source_bytes() {
    let root = tempfile::tempdir().unwrap();
    let (sdk, archive, metadata) = archive_fixture(root.path(), Some(b"verified CEF credits\n"));
    let original = fs::read(&archive).unwrap();
    let output = root.path().join("notices");
    copy_cef_notices(&sdk, &output).unwrap();
    assert_eq!(fs::read(output.join("LICENSE.txt")).unwrap(), b"verified CEF license\n");
    assert_eq!(
      fs::read(output.join("CREDITS.html")).unwrap(),
      b"verified CEF credits\n"
    );
    assert_eq!(fs::read(&archive).unwrap(), original);
    assert_eq!(fs::read(sdk.join("archive.json")).unwrap(), metadata);
    fs::OpenOptions::new()
      .append(true)
      .open(&archive)
      .unwrap()
      .write_all(b"tampered archive")
      .unwrap();
    assert!(copy_cef_notices(&sdk, &root.path().join("tampered")).is_err());
    assert!(!root.path().join("tampered").exists());
    fs::remove_file(&archive).unwrap();
    assert!(copy_cef_notices(&sdk, &root.path().join("missing")).is_err());
    fs::write(&archive, &original).unwrap();
    for bad in [
      b"{}".as_slice(),
      b"{\"name\":\"../outside.tar.bz2\",\"sha1\":\"bad\"}".as_slice(),
      b"not JSON".as_slice(),
    ] {
      fs::write(sdk.join("archive.json"), bad).unwrap();
      assert!(copy_cef_notices(&sdk, &root.path().join("bad-metadata")).is_err());
    }
    fs::write(sdk.join("archive.json"), &metadata).unwrap();
    let moved = root.path().join("saved-archive");
    fs::rename(&archive, &moved).unwrap();
    symlink(&moved, &archive).unwrap();
    assert!(copy_cef_notices(&sdk, &root.path().join("linked-archive")).is_err());
  }

  #[test]
  fn verified_archive_missing_or_empty_notice_is_rejected() {
    for credits in [None, Some(b"".as_slice())] {
      let root = tempfile::tempdir().unwrap();
      let (sdk, _, _) = archive_fixture(root.path(), credits);
      let output = root.path().join("notices");
      assert!(copy_cef_notices(&sdk, &output).is_err());
      assert!(!output.exists());
    }
  }

  #[test]
  fn verified_archive_oversized_notice_is_bounded_before_any_notice_is_published() {
    let root = tempfile::tempdir().unwrap();
    let (sdk, archive, _) = archive_fixture(root.path(), Some(b"small"));
    let prefix = "cef_binary_notice_fixture";
    fs::File::create(root.path().join(prefix).join("CREDITS.html"))
      .unwrap()
      .set_len(MAX_CEF_NOTICE_BYTES + 1)
      .unwrap();
    assert!(
      Command::new("/usr/bin/tar")
        .arg("-cjf")
        .arg(&archive)
        .arg("-C")
        .arg(root.path())
        .arg(prefix)
        .status()
        .unwrap()
        .success()
    );
    let hash = run_tool(Command::new("/usr/bin/shasum").args(["-a", "1"]).arg(&archive)).unwrap();
    let hash = std::str::from_utf8(&hash.stdout)
      .unwrap()
      .split_whitespace()
      .next()
      .unwrap();
    fs::write(
      sdk.join("archive.json"),
      serde_json::to_vec(&serde_json::json!({"name":format!("{prefix}.tar.bz2"),"sha1":hash})).unwrap(),
    )
    .unwrap();
    let output = root.path().join("notices");
    let error = copy_cef_notices(&sdk, &output).unwrap_err();
    assert!(format!("{error:#}").contains("exceeds"));
    assert!(!output.exists());
  }
}

pub(crate) fn binary_architectures(path: &Path) -> Result<BTreeSet<String>> {
  let output = run_tool(Command::new("/usr/bin/lipo").arg("-archs").arg(path))
    .with_context(|| format!("inspect Mach-O architectures in {}", path.display()))?;
  let architectures: BTreeSet<_> = std::str::from_utf8(&output.stdout)?
    .split_whitespace()
    .map(str::to_owned)
    .collect();
  ensure!(
    !architectures.is_empty(),
    "no Mach-O architectures in {}",
    path.display()
  );
  Ok(architectures)
}

pub(super) fn require_architectures(path: &Path, required: &BTreeSet<String>) -> Result<()> {
  let actual = binary_architectures(path)?;
  ensure!(
    required.is_subset(&actual),
    "{} has architectures {actual:?}, but the application requires {required:?}",
    path.display()
  );
  Ok(())
}

pub(super) fn run_tool(command: &mut Command) -> Result<Output> {
  let output = command
    .output()
    .with_context(|| format!("run {:?}", command.get_program()))?;
  ensure!(
    output.status.success(),
    "{:?} failed with {}: {}",
    command.get_program(),
    output.status,
    String::from_utf8_lossy(&output.stderr).trim()
  );
  Ok(output)
}

fn publish(staged: &Path, destination: &Path, lock_path: &Path, identifier: &str) -> Result<()> {
  let lock = lock_output(lock_path)?;
  publish_locked(staged, destination, identifier, &lock)
}

pub(crate) fn lock_output(lock_path: &Path) -> Result<std::fs::File> {
  // Keep the lock file in place so a waiting publisher never locks an unlinked inode.
  let lock = OpenOptions::new()
    .read(true)
    .write(true)
    .create(true)
    .truncate(false)
    .custom_flags(libc::O_NOFOLLOW)
    .open(lock_path)
    .with_context(|| format!("open bundle lock {}", lock_path.display()))?;
  lock
    .lock()
    .with_context(|| format!("lock bundle output {}", lock_path.display()))?;
  Ok(lock)
}

pub(crate) fn publish_locked(staged: &Path, destination: &Path, identifier: &str, _lock: &std::fs::File) -> Result<()> {
  let flags = match fs::symlink_metadata(destination) {
    Ok(metadata) => {
      ensure!(
        metadata.is_dir() && destination.join("Contents/Info.plist").is_file(),
        "cannot replace non-application path {}",
        destination.display()
      );
      let info = Value::from_file(destination.join("Contents/Info.plist"))
        .with_context(|| format!("read existing bundle identifier in {}", destination.display()))?;
      let existing_id = info
        .as_dictionary()
        .and_then(|dict| dict.get("CFBundleIdentifier"))
        .and_then(Value::as_string);
      ensure!(
        existing_id == Some(identifier),
        "cannot replace {} with a different bundle identifier; use a different name or output directory",
        destination.display()
      );
      libc::RENAME_SWAP
    }
    Err(error) if error.kind() == io::ErrorKind::NotFound => libc::RENAME_EXCL,
    Err(error) => return Err(error).with_context(|| format!("inspect {}", destination.display())),
  };

  rename_paths(staged, destination, flags)
}

/// Publish a new directory without replacing a destination created after preflight.
pub(crate) fn publish_new_directory(staged: &Path, destination: &Path) -> Result<()> {
  rename_paths(staged, destination, libc::RENAME_EXCL)
}

fn rename_paths(staged: &Path, destination: &Path, flags: u32) -> Result<()> {
  let from = CString::new(staged.as_os_str().as_bytes())?;
  let to = CString::new(destination.as_os_str().as_bytes())?;
  // SAFETY: Both paths are NUL-terminated for the duration of the call and flags are Darwin rename flags.
  // A successful swap leaves the old app inside the private staging directory for TempDir to clean up.
  if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), flags) } != 0 {
    return Err(io::Error::last_os_error()).with_context(|| format!("publish bundle {}", destination.display()));
  }
  Ok(())
}
