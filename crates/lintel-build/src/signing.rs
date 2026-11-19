//! Sign nested macOS code before atomically publishing the completed application.

use std::{
  collections::BTreeMap,
  ffi::OsString,
  fs,
  path::{Path, PathBuf},
  process::Command,
};

use anyhow::{Context, Result, ensure};
use plist::Value;

use crate::{
  CEF_HELPER_ROLES, SignOptions,
  macos::{lock_output, publish_locked, run_tool},
  signing_components::{Component, ComponentKind, components},
};

/// Sign a same-volume copy and replace the original only after all signatures verify.
pub fn sign_bundle(bundle: &Path, options: &SignOptions) -> Result<PathBuf> {
  let bundle = checked_bundle(bundle)?;
  validate_options(options)?;
  let parent = bundle.parent().context("application bundle has no parent directory")?;
  let name = bundle.file_name().context("application bundle has no filename")?;
  let mut lock_name = OsString::from(".");
  lock_name.push(name);
  lock_name.push(".lock");
  // Use the publisher's lock before taking our snapshot so another operation cannot
  // replace a newer bundle with the older copy we are about to sign.
  let lock = lock_output(&parent.join(lock_name))?;
  let identifier = bundle_identifier(&bundle)?;
  let stage = tempfile::Builder::new().prefix(".lintel-sign-").tempdir_in(parent)?;
  let staged = stage.path().join(name);

  run_tool(Command::new("/usr/bin/ditto").arg(&bundle).arg(&staged))
    .with_context(|| format!("stage application for signing {}", bundle.display()))?;
  ensure!(
    bundle_identifier(&staged)? == identifier,
    "staged application has a different bundle identifier"
  );
  sign_staged(&staged, options)?;

  publish_locked(&staged, &bundle, &identifier, &lock)?;
  Ok(bundle)
}

/// Sign an already private staging directory, then verify every discovered component.
pub(crate) fn sign_staged(bundle: &Path, options: &SignOptions) -> Result<()> {
  let bundle = checked_bundle(bundle)?;
  bundle_identifier(&bundle)?;
  validate_options(options)?;
  let entitlements = normalized_entitlements(options.entitlements.as_deref())?;
  let helper_entitlements = normalized_entitlements(options.helper_entitlements.as_deref())?;
  let normalized_overrides = options
    .helper_entitlements_overrides
    .iter()
    .map(|(role, path)| {
      let normalized = normalized_entitlements(Some(path))?.context("Helper entitlement override has no plist")?;
      Ok((role.clone(), normalized))
    })
    .collect::<Result<BTreeMap<_, _>>>()?;
  let override_paths = normalized_overrides
    .iter()
    .map(|(role, file)| (role.clone(), file.path().to_path_buf()))
    .collect();
  let components = components(&bundle)?;
  let helper_roles = requested_helper_roles(&components, &options.helper_entitlements_overrides)?;
  if options.identity == "-" {
    let mut versions = BTreeMap::new();
    for component in &components {
      if component.kind == ComponentKind::Framework && component.bundle_version.is_some() {
        let count = versions.entry(&component.path).or_insert(0);
        *count += 1;
        ensure!(
          *count == 1,
          "ad-hoc signing cannot give a shared default designated requirement to multiple versions of {}; use a certificate-backed identity",
          component.path.display()
        );
      }
    }
  }
  for component in &components {
    run_tool(&mut signing_command(
      component,
      options,
      entitlements.as_ref().map(tempfile::NamedTempFile::path),
      selected_helper_entitlements(
        component,
        helper_entitlements.as_ref().map(tempfile::NamedTempFile::path),
        &override_paths,
        &helper_roles,
      ),
    ))
    .with_context(|| component_context("sign", component))?;
  }
  verify_components(&bundle, &components)
}

/// Verify every nested component and every real framework version, then the whole app.
pub fn verify_signature(bundle: &Path) -> Result<()> {
  let bundle = checked_bundle(bundle)?;
  bundle_identifier(&bundle)?;
  let components = components(&bundle)?;
  verify_components(&bundle, &components)
}

fn signing_command(
  component: &Component,
  options: &SignOptions,
  entitlements: Option<&Path>,
  helper_entitlements: Option<&Path>,
) -> Command {
  let mut command = Command::new("/usr/bin/codesign");
  command.args(["--force", "--sign"]).arg(&options.identity);
  command.arg(if options.timestamp {
    "--timestamp"
  } else {
    "--timestamp=none"
  });

  let process_entitlements = match component.kind {
    ComponentKind::MainApp => entitlements,
    ComponentKind::HelperApp => helper_entitlements,
    ComponentKind::Framework | ComponentKind::MachO | ComponentKind::Executable => None,
  };
  if options.hardened_runtime
    && matches!(
      component.kind,
      ComponentKind::MainApp | ComponentKind::HelperApp | ComponentKind::Executable
    )
  {
    command.args(["--options", "runtime"]);
  }
  if let Some(entitlements) = process_entitlements {
    command.arg("--entitlements").arg(entitlements);
  }
  if let Some(version) = &component.bundle_version {
    command.arg("--bundle-version").arg(version);
  }
  command.arg(&component.path);
  command
}

fn verify_components(bundle: &Path, components: &[Component]) -> Result<()> {
  for component in components {
    let mut command = Command::new("/usr/bin/codesign");
    command.args(["--verify", "--strict"]);
    if let Some(version) = &component.bundle_version {
      command.arg("--bundle-version").arg(version);
    }
    command.arg(&component.path);
    run_tool(&mut command).with_context(|| component_context("verify signature of", component))?;
  }
  run_tool(
    Command::new("/usr/bin/codesign")
      .args(["--verify", "--deep", "--strict"])
      .arg(bundle),
  )
  .with_context(|| format!("verify complete application signature {}", bundle.display()))?;
  Ok(())
}

fn component_context(action: &str, component: &Component) -> String {
  match &component.bundle_version {
    Some(version) => format!("{action} {} (framework version {version})", component.path.display()),
    None => format!("{action} {}", component.path.display()),
  }
}

fn requested_helper_roles(
  components: &[Component],
  requested: &BTreeMap<String, PathBuf>,
) -> Result<BTreeMap<PathBuf, String>> {
  let mut roles = BTreeMap::new();
  if requested.is_empty() {
    return Ok(roles);
  }
  for component in components
    .iter()
    .filter(|component| component.kind == ComponentKind::HelperApp)
  {
    let info = Value::from_file(component.path.join("Contents/Info.plist"))
      .with_context(|| format!("read Helper role in {}", component.path.display()))?;
    let role = info
      .as_dictionary()
      .and_then(|info| info.get("CFBundleIdentifier"))
      .and_then(Value::as_string)
      .and_then(|identifier| identifier.rsplit_once(".helper."))
      .map(|(_, role)| role)
      .filter(|role| CEF_HELPER_ROLES.contains(role));
    if let Some(role) = role {
      roles.insert(component.path.clone(), role.to_owned());
    }
  }
  for role in requested.keys() {
    ensure!(
      roles.values().any(|found| found == role),
      "Helper entitlement override for role {role} matches no CEF Helper CFBundleIdentifier in this application"
    );
  }
  Ok(roles)
}

fn selected_helper_entitlements<'a>(
  component: &Component,
  fallback: Option<&'a Path>,
  overrides: &'a BTreeMap<String, PathBuf>,
  roles: &BTreeMap<PathBuf, String>,
) -> Option<&'a Path> {
  if component.kind != ComponentKind::HelperApp {
    return None;
  }
  roles
    .get(&component.path)
    .and_then(|role| overrides.get(role))
    .map(PathBuf::as_path)
    .or(fallback)
}

fn validate_options(options: &SignOptions) -> Result<()> {
  ensure!(
    !options.identity.trim().is_empty() && !options.identity.chars().any(char::is_control),
    "an explicit signing identity is required; use '-' for local ad-hoc signing"
  );
  ensure!(
    options.identity != "-" || !options.timestamp,
    "secure timestamps require a certificate-backed signing identity"
  );
  for role in options.helper_entitlements_overrides.keys() {
    ensure!(
      CEF_HELPER_ROLES.contains(&role.as_str()),
      "unknown CEF Helper role {role:?}; expected {}",
      CEF_HELPER_ROLES.join(", ")
    );
  }
  Ok(())
}

fn normalized_entitlements(path: Option<&Path>) -> Result<Option<tempfile::NamedTempFile>> {
  path
    .map(|path| {
      ensure!(path.is_file(), "entitlements path is not a file: {}", path.display());
      let value = Value::from_file(path).with_context(|| format!("read entitlements file {}", path.display()))?;
      ensure!(
        value.as_dictionary().is_some(),
        "entitlements plist must contain a dictionary: {}",
        path.display()
      );
      // codesign accepts XML on every supported macOS version. Keep this owned
      // temporary file alive until the last component has been signed.
      let mut normalized = tempfile::Builder::new()
        .prefix("lintel-entitlements-")
        .suffix(".plist")
        .tempfile()?;
      value
        .to_writer_xml(normalized.as_file_mut())
        .with_context(|| format!("normalize entitlements file {}", path.display()))?;
      Ok(normalized)
    })
    .transpose()
}

fn checked_bundle(bundle: &Path) -> Result<PathBuf> {
  let metadata = fs::symlink_metadata(bundle).with_context(|| format!("inspect application {}", bundle.display()))?;
  ensure!(
    metadata.is_dir() && bundle.extension().is_some_and(|extension| extension == "app"),
    "expected an application directory ending in .app, not a symlink: {}",
    bundle.display()
  );
  bundle
    .canonicalize()
    .with_context(|| format!("resolve application {}", bundle.display()))
}

fn bundle_identifier(bundle: &Path) -> Result<String> {
  let info_path = bundle.join("Contents/Info.plist");
  let info = Value::from_file(&info_path).with_context(|| format!("read {}", info_path.display()))?;
  let identifier = info
    .as_dictionary()
    .and_then(|dictionary| dictionary.get("CFBundleIdentifier"))
    .and_then(Value::as_string)
    .with_context(|| format!("{} has no string CFBundleIdentifier", info_path.display()))?;
  ensure!(
    !identifier.is_empty()
      && identifier
        .split('.')
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')),
    "invalid CFBundleIdentifier {identifier:?} in {}",
    info_path.display()
  );
  Ok(identifier.to_owned())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn options() -> SignOptions {
    SignOptions {
      identity: "-".to_owned(),
      entitlements: None,
      helper_entitlements: None,
      helper_entitlements_overrides: BTreeMap::new(),
      hardened_runtime: true,
      timestamp: false,
    }
  }

  fn arguments(kind: ComponentKind, version: Option<&str>) -> Vec<OsString> {
    let component = Component {
      path: PathBuf::from("/Fixture.app"),
      kind,
      bundle_version: version.map(str::to_owned),
    };
    signing_command(
      &component,
      &options(),
      Some(Path::new("/main.plist")),
      Some(Path::new("/helper.plist")),
    )
    .get_args()
    .map(OsString::from)
    .collect()
  }

  #[test]
  fn process_signatures_use_separate_explicit_entitlements() {
    let main = arguments(ComponentKind::MainApp, None);
    let helper = arguments(ComponentKind::HelperApp, None);
    assert!(main.windows(2).any(|pair| pair == ["--entitlements", "/main.plist"]));
    assert!(
      helper
        .windows(2)
        .any(|pair| pair == ["--entitlements", "/helper.plist"])
    );
    assert!(main.windows(2).any(|pair| pair == ["--options", "runtime"]));
    assert!(helper.windows(2).any(|pair| pair == ["--options", "runtime"]));
    assert!(main.iter().any(|arg| arg == "--timestamp=none"));
    assert!(!main.iter().any(|arg| arg == "--deep"));
  }

  #[test]
  fn libraries_have_no_process_options_and_framework_versions_are_explicit() {
    for kind in [ComponentKind::Framework, ComponentKind::MachO] {
      let args = arguments(kind, Some("A"));
      assert!(!args.iter().any(|arg| arg == "--entitlements" || arg == "--options"));
      assert!(args.windows(2).any(|pair| pair == ["--bundle-version", "A"]));
    }
  }

  #[test]
  fn standalone_executables_enable_requested_runtime_without_inheriting_app_entitlements() {
    let args = arguments(ComponentKind::Executable, None);
    assert!(args.windows(2).any(|pair| pair == ["--options", "runtime"]));
    assert!(!args.iter().any(|arg| arg == "--entitlements"));

    let mut options = options();
    options.hardened_runtime = false;
    let component = Component {
      path: PathBuf::from("/Fixture.app/Contents/MacOS/tool"),
      kind: ComponentKind::Executable,
      bundle_version: None,
    };
    let command = signing_command(&component, &options, None, None);
    assert!(
      !command
        .get_args()
        .any(|arg| arg == "--options" || arg == "--entitlements")
    );
  }

  #[test]
  fn timestamp_and_hardened_runtime_follow_explicit_settings() {
    let mut options = options();
    options.hardened_runtime = false;
    options.timestamp = true;
    let component = Component {
      path: PathBuf::from("/Fixture.app"),
      kind: ComponentKind::MainApp,
      bundle_version: None,
    };
    let command = signing_command(&component, &options, None, None);
    let args: Vec<_> = command.get_args().collect();
    assert!(args.iter().any(|arg| *arg == "--timestamp"));
    assert!(!args.iter().any(|arg| *arg == "--options" || *arg == "--entitlements"));
  }

  #[test]
  fn normalizes_binary_entitlements_without_changing_the_source() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("entitlements.plist");
    let dictionary =
      plist::Dictionary::from_iter([("com.apple.security.cs.allow-jit".to_owned(), Value::Boolean(true))]);
    let expected = Value::Dictionary(dictionary);
    expected.to_file_binary(&source).unwrap();
    let original = fs::read(&source).unwrap();
    let normalized = normalized_entitlements(Some(&source)).unwrap().unwrap();
    assert!(fs::read(normalized.path()).unwrap().starts_with(b"<?xml"));
    assert_eq!(Value::from_file(normalized.path()).unwrap(), expected);
    assert_eq!(fs::read(&source).unwrap(), original);
  }

  #[test]
  fn rejects_missing_identity_and_entitlements_before_signing() {
    let mut options = options();
    options.identity.clear();
    assert!(validate_options(&options).is_err());
    options.identity = "-".to_owned();
    let directory = tempfile::tempdir().unwrap();
    assert!(normalized_entitlements(Some(directory.path())).is_err());
    assert!(normalized_entitlements(Some(&directory.path().join("missing.plist"))).is_err());
    let invalid = directory.path().join("invalid.plist");
    Value::Array(Vec::new()).to_file_xml(&invalid).unwrap();
    assert!(normalized_entitlements(Some(&invalid)).is_err());
  }

  fn helper(root: &Path, name: &str, identifier: &str) -> Component {
    let path = root.join(name);
    fs::create_dir_all(path.join("Contents")).unwrap();
    let info = plist::Dictionary::from_iter([("CFBundleIdentifier".to_owned(), Value::String(identifier.to_owned()))]);
    Value::Dictionary(info)
      .to_file_xml(path.join("Contents/Info.plist"))
      .unwrap();
    Component {
      path,
      kind: ComponentKind::HelperApp,
      bundle_version: None,
    }
  }

  #[test]
  fn per_role_helper_entitlements_follow_identifiers_and_preserve_explicit_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let renderer = helper(
      directory.path(),
      "Misleading Helper (GPU).app",
      "dev.lintel.app.helper.Renderer",
    );
    let gpu = helper(directory.path(), "Helper.app", "dev.lintel.app.helper.GPU");
    let helpers = [renderer, gpu];
    let overrides = BTreeMap::from([("Renderer".to_owned(), PathBuf::from("/renderer.plist"))]);
    let roles = requested_helper_roles(&helpers, &overrides).unwrap();
    let fallback = Path::new("/fallback.plist");
    let renderer = selected_helper_entitlements(&helpers[0], Some(fallback), &overrides, &roles).unwrap();
    assert_eq!(renderer, Path::new("/renderer.plist"));
    assert_eq!(
      selected_helper_entitlements(&helpers[1], Some(fallback), &overrides, &roles),
      Some(fallback)
    );
    assert_eq!(
      selected_helper_entitlements(&helpers[1], None, &overrides, &roles),
      None
    );
    let command = signing_command(&helpers[0], &options(), None, Some(renderer));
    let args: Vec<_> = command.get_args().collect();
    assert!(
      args
        .windows(2)
        .any(|pair| pair == ["--entitlements", "/renderer.plist"])
    );
    for kind in [
      ComponentKind::MainApp,
      ComponentKind::Framework,
      ComponentKind::MachO,
      ComponentKind::Executable,
    ] {
      let component = Component {
        path: helpers[0].path.clone(),
        kind,
        bundle_version: None,
      };
      assert_eq!(
        selected_helper_entitlements(&component, Some(fallback), &overrides, &roles),
        None
      );
    }
  }

  #[test]
  fn helper_override_rejects_unknown_unmatched_or_non_helper_roles() {
    let directory = tempfile::tempdir().unwrap();
    let mut renderer = helper(
      directory.path(),
      "Helper (Renderer).app",
      "dev.lintel.app.helper.Renderer",
    );
    let requested = BTreeMap::from([("GPU".to_owned(), PathBuf::from("/gpu.plist"))]);
    assert!(requested_helper_roles(&[renderer], &requested).is_err());
    renderer = helper(directory.path(), "Helper (GPU).app", "dev.lintel.app.other.GPU");
    assert!(requested_helper_roles(&[renderer], &requested).is_err());
    renderer = helper(directory.path(), "Main.app", "dev.lintel.app.helper.GPU");
    renderer.kind = ComponentKind::MainApp;
    assert!(requested_helper_roles(&[renderer], &requested).is_err());
    let mut options = options();
    options
      .helper_entitlements_overrides
      .insert("renderer".to_owned(), PathBuf::from("/renderer.plist"));
    assert!(validate_options(&options).is_err());
  }
}
