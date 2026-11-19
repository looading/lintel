use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use cargo_metadata::{Artifact, BuildScript, CrateType, Message, Metadata, MetadataCommand, Package, Target};
use serde_json::Value;

use crate::third_party::{Library, libraries};

/// Which executable target to build.
#[derive(Debug, Clone, Default)]
pub enum TargetSelection {
  #[default]
  Default,
  Bin(String),
  Example(String),
}

/// Cargo options for the application being built.
#[derive(Debug, Clone)]
pub struct BuildOptions {
  pub manifest_path: PathBuf,
  pub package: Option<String>,
  pub selection: TargetSelection,
  pub target: Option<String>,
  pub release: bool,
  pub locked: bool,
  pub features: Vec<String>,
  /// Lintel profile configuration, loaded with Cargo's nightly alternate lockfile support.
  pub cargo_config: Option<PathBuf>,
  pub cef_path: Option<PathBuf>,
}

/// The selected Cargo executable and any CEF distribution it built against.
#[derive(Debug, Clone)]
pub struct BuildArtifact {
  pub executable: PathBuf,
  pub package_name: String,
  /// Directory containing the selected package's Cargo manifest.
  pub package_dir: PathBuf,
  pub version: String,
  pub cef_dir: Option<PathBuf>,
  pub target_name: String,
  /// Managed library snapshot used to link this artifact.
  pub third_party: Vec<Library>,
  lintel_metadata: Value,
}

/// Optional bundle defaults from the selected package's `[package.metadata.lintel]` table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BundleMetadata {
  pub id: Option<String>,
  pub name: Option<String>,
}

impl BuildArtifact {
  /// Read declared application-owned helper binaries and the explicit Mojo runtime path.
  pub fn service_metadata(&self) -> Result<crate::ServiceMetadata> {
    crate::ServiceMetadata::parse(&self.lintel_metadata)
  }

  /// Read bundle defaults when packaging or launching the built executable.
  pub fn bundle_metadata(&self) -> Result<BundleMetadata> {
    if self.lintel_metadata.is_null() {
      return Ok(BundleMetadata::default());
    }
    let table = self.lintel_metadata.as_object().with_context(|| {
      format!(
        "[package.metadata.lintel] must be a table in Cargo package {}",
        self.package_name
      )
    })?;
    let read_string = |key| match table.get(key) {
      None | Some(Value::Null) => Ok(None),
      Some(Value::String(value)) => Ok(Some(value.clone())),
      Some(_) => bail!(
        "package.metadata.lintel.{key} must be a string in Cargo package {}",
        self.package_name
      ),
    };
    Ok(BundleMetadata {
      id: read_string("id")?,
      name: read_string("name")?,
    })
  }
}

/// Build a single executable using the application's Cargo workspace and configuration.
pub fn build(options: &BuildOptions) -> Result<BuildArtifact> {
  let resolved = resolve_build(options)?;
  let package = select_package(&resolved.metadata, &resolved.manifest, options.package.as_deref())?;
  let target = select_target(package, &options.selection)?;
  let linking = resolve_linking(options, package)?;
  build_resolved(options, &resolved, package, target, &linking)
}

/// Build an application and its declared services after checking every service target.
/// Invalid declarations fail before Cargo compiles the application or runs build scripts.
/// Cargo remains responsible for feature resolution and target required-features checks.
pub fn build_with_service_processes(options: &BuildOptions) -> Result<(BuildArtifact, Vec<crate::BundleHelper>)> {
  let resolved = resolve_build(options)?;
  let package = select_package(&resolved.metadata, &resolved.manifest, options.package.as_deref())?;
  let application_target = select_target(package, &options.selection)?;
  let metadata = crate::ServiceMetadata::parse(package.metadata.get("lintel").unwrap_or(&Value::Null))?;
  let services = metadata
    .processes
    .iter()
    .map(|process| {
      ensure!(
        !application_target.is_bin() || !process.bin.eq_ignore_ascii_case(&application_target.name),
        "the application binary cannot also be declared as a service process"
      );
      let target = select_target(package, &TargetSelection::Bin(process.bin.clone()))
        .with_context(|| format!("validate service process {:?}", process.name))?;
      Ok((process.name.clone(), target))
    })
    .collect::<Result<Vec<_>>>()?;
  let linking = resolve_linking(options, package)?;
  let application = build_resolved(options, &resolved, package, application_target, &linking)?;
  let helpers = services
    .into_iter()
    .map(|(name, target)| {
      let artifact = build_resolved(options, &resolved, package, target, &linking)
        .with_context(|| format!("build service process {name:?}"))?;
      Ok(crate::service_processes::helper_from_artifact(name, artifact))
    })
    .collect::<Result<Vec<_>>>()?;
  Ok((application, helpers))
}

struct ResolvedBuild {
  manifest: PathBuf,
  app_dir: PathBuf,
  config: Option<PathBuf>,
  cef_path: Option<PathBuf>,
  metadata: Metadata,
}

struct BuildLinking {
  third_party: Vec<Library>,
  args: Vec<OsString>,
}

fn resolve_build(options: &BuildOptions) -> Result<ResolvedBuild> {
  let manifest = options
    .manifest_path
    .canonicalize()
    .context("locate the application manifest")?;
  let app_dir = manifest
    .parent()
    .context("the application manifest has no parent directory")?;
  let config = options
    .cargo_config
    .as_deref()
    .map(Path::canonicalize)
    .transpose()
    .context("locate the supplied Cargo configuration")?;
  let cef_path = options
    .cef_path
    .as_deref()
    .map(Path::canonicalize)
    .transpose()
    .context("locate the supplied CEF distribution")?;

  // Resolve Cargo through PATH in the application's directory so rustup can select its toolchain.
  let mut metadata_command = MetadataCommand::new()
    .cargo_path("cargo")
    .manifest_path(&manifest)
    .current_dir(app_dir)
    .no_deps()
    .cargo_command();
  configure_cargo(&mut metadata_command, options, config.as_deref(), cef_path.as_deref());
  let metadata_output = metadata_command
    .stderr(Stdio::inherit())
    .output()
    .context("run cargo metadata for the application")?;
  ensure!(
    metadata_output.status.success(),
    "cargo metadata failed ({})",
    metadata_output.status
  );
  let metadata = MetadataCommand::parse(std::str::from_utf8(&metadata_output.stdout)?)
    .context("read Cargo metadata for the application")?;
  Ok(ResolvedBuild {
    app_dir: app_dir.to_path_buf(),
    manifest,
    config,
    cef_path,
    metadata,
  })
}

fn package_directory(package: &Package) -> Result<&Path> {
  package
    .manifest_path
    .as_std_path()
    .parent()
    .context("the selected Cargo package has no parent directory")
}

fn resolve_linking(options: &BuildOptions, package: &Package) -> Result<BuildLinking> {
  let package_dir = package_directory(package)?;
  let configured_target = std::env::var("CARGO_BUILD_TARGET").ok();
  let selected_target_triple = options.target.as_deref().or(configured_target.as_deref());
  let third_party = if selected_target_triple.is_some_and(|target| target.contains("-windows-"))
    || (selected_target_triple.is_none() && cfg!(target_os = "windows"))
  {
    crate::windows::windows_libraries(package_dir)?
  } else {
    libraries(package_dir)?
  };
  let link_args =
    crate::native_libraries::rustc_args(&third_party, options.target.as_deref().or(configured_target.as_deref()))?;
  Ok(BuildLinking {
    third_party,
    args: link_args,
  })
}

fn build_resolved(
  options: &BuildOptions,
  resolved: &ResolvedBuild,
  package: &Package,
  target: &Target,
  linking: &BuildLinking,
) -> Result<BuildArtifact> {
  let mut cargo = Command::new("cargo");
  cargo
    .current_dir(&resolved.app_dir)
    .arg(if linking.args.is_empty() { "build" } else { "rustc" })
    .arg("--manifest-path")
    .arg(&resolved.manifest)
    .arg("--package")
    .arg(package.name.as_str())
    .arg(if target.is_example() { "--example" } else { "--bin" })
    .arg(&target.name)
    .arg("--message-format=json")
    .stdout(Stdio::piped());
  configure_cargo(
    &mut cargo,
    options,
    resolved.config.as_deref(),
    resolved.cef_path.as_deref(),
  );
  if options.release {
    cargo.arg("--release");
  }
  if let Some(triple) = &options.target {
    cargo.arg("--target").arg(triple);
  }
  if !options.features.is_empty() {
    cargo.arg("--features").arg(options.features.join(","));
  }
  if !linking.args.is_empty() {
    cargo.arg("--").args(&linking.args);
  }

  let mut child = cargo.spawn().context("start cargo build for the application")?;
  let stdout = child.stdout.take().context("open cargo build output")?;
  let messages = read_build_output(BufReader::new(stdout), package, target);
  let status = child.wait().context("wait for cargo build")?;
  ensure!(status.success(), "cargo build failed ({status})");
  let messages = messages?;
  let executable = match messages.executables.len() {
    0 => bail!("Cargo reported no executable for {} in {}", target.name, package.name),
    1 => messages
      .executables
      .into_iter()
      .next()
      .context("read the Cargo executable path")?,
    _ => bail!(
      "Cargo reported multiple executables for {} in {}",
      target.name,
      package.name
    ),
  };
  let cef_dir = cef_distribution(
    &messages.cef_artifacts,
    &messages.build_scripts,
    &executable,
    target,
    resolved.cef_path.clone(),
  )?;

  Ok(BuildArtifact {
    executable,
    package_name: package.name.to_string(),
    package_dir: package_directory(package)?.to_path_buf(),
    version: package.version.to_string(),
    cef_dir,
    target_name: target.name.clone(),
    third_party: linking.third_party.clone(),
    lintel_metadata: package.metadata.get("lintel").cloned().unwrap_or_default(),
  })
}

fn configure_cargo(command: &mut Command, options: &BuildOptions, config: Option<&Path>, cef_path: Option<&Path>) {
  if let Some(config) = config {
    command.args(["-Z", "lockfile-path", "--config"]).arg(config);
  }
  if options.locked {
    command.arg("--locked");
  }
  if let Some(cef_path) = cef_path {
    command.env("CEF_PATH", cef_path);
  }
}

fn select_package<'a>(metadata: &'a Metadata, manifest: &Path, package: Option<&str>) -> Result<&'a Package> {
  let packages = metadata.workspace_packages();
  if let Some(name) = package {
    return packages
      .into_iter()
      .find(|package| package.name.as_str() == name)
      .with_context(|| format!("Cargo workspace has no package named {name}"));
  }
  if let Some(package) = packages
    .iter()
    .find(|package| package.manifest_path.as_std_path() == manifest)
  {
    return Ok(package);
  }
  if let [package] = metadata.workspace_default_packages().as_slice() {
    return Ok(package);
  }
  bail!("Cargo workspace has no single default package. Specify --package.");
}

fn select_target<'a>(package: &'a Package, selection: &TargetSelection) -> Result<&'a Target> {
  let bin = |name: &str| {
    package
      .targets
      .iter()
      .find(|target| target.is_bin() && target.name == name)
  };
  match selection {
    TargetSelection::Bin(name) => {
      bin(name).with_context(|| format!("package {} has no binary named {name}", package.name))
    }
    TargetSelection::Example(name) => package
      .targets
      .iter()
      .find(|target| target.is_example() && target.name == *name && target.crate_types.contains(&CrateType::Bin))
      .with_context(|| format!("package {} has no executable example named {name}", package.name)),
    TargetSelection::Default => {
      if let Some(name) = &package.default_run {
        return bin(name).with_context(|| format!("package {} has no default binary named {name}", package.name));
      }
      let mut bins = package.targets.iter().filter(|target| target.is_bin());
      match (bins.next(), bins.next()) {
        (Some(target), None) => Ok(target),
        _ => bail!(
          "package {} has no single default binary. Specify --bin or --example.",
          package.name
        ),
      }
    }
  }
}

#[derive(Default)]
struct BuildOutput {
  executables: BTreeSet<PathBuf>,
  cef_artifacts: Vec<Artifact>,
  build_scripts: Vec<BuildScript>,
}

fn read_build_output(output: impl std::io::BufRead, package: &Package, target: &Target) -> Result<BuildOutput> {
  let mut build = BuildOutput::default();
  let mut diagnostics = std::io::stderr().lock();
  for message in Message::parse_stream(output) {
    match message.context("read Cargo build messages")? {
      Message::CompilerArtifact(artifact) => {
        if artifact.package_id == package.id
          && artifact.target.name == target.name
          && artifact.target.kind == target.kind
          && let Some(executable) = artifact.executable.as_ref()
        {
          build.executables.insert(executable.as_std_path().to_path_buf());
        }
        if artifact.target.name == "cef_dll_sys" && artifact.target.is_lib() {
          build.cef_artifacts.push(artifact);
        }
      }
      Message::BuildScriptExecuted(script) => build.build_scripts.push(script),
      Message::CompilerMessage(message) => match &message.message.rendered {
        Some(rendered) => write!(diagnostics, "{rendered}")?,
        None => writeln!(diagnostics, "{message}")?,
      },
      Message::TextLine(line) => writeln!(diagnostics, "{line}")?,
      _ => {}
    }
  }
  Ok(build)
}

fn cef_distribution(
  artifacts: &[Artifact],
  scripts: &[BuildScript],
  executable: &Path,
  target: &Target,
  explicit: Option<PathBuf>,
) -> Result<Option<PathBuf>> {
  let executable_dir = executable
    .parent()
    .context("the Cargo executable has no parent directory")?;
  let profile_dir = if target.is_example() {
    executable_dir
      .parent()
      .context("the Cargo example has no profile directory")?
  } else {
    executable_dir
  };
  // Separate a CEF build dependency built for the host from the selected target's runtime dependency.
  let cef_packages: BTreeSet<_> = artifacts
    .iter()
    .filter(|artifact| {
      artifact
        .filenames
        .iter()
        .any(|file| file.as_std_path().starts_with(profile_dir))
    })
    .map(|artifact| &artifact.package_id)
    .collect();
  if cef_packages.is_empty() {
    return Ok(None);
  }

  let mut matching_scripts: Vec<_> = scripts
    .iter()
    .filter(|script| cef_packages.contains(&script.package_id))
    .collect();
  if matching_scripts
    .iter()
    .any(|script| script.out_dir.as_std_path().starts_with(profile_dir))
  {
    matching_scripts.retain(|script| script.out_dir.as_std_path().starts_with(profile_dir));
  }
  let mut distributions = BTreeSet::new();
  for script in matching_scripts {
    for path in &script.linked_paths {
      if let Some(path) = path.as_str().strip_prefix("native=") {
        let path = Path::new(path);
        if has_cef_runtime(path) {
          distributions.insert(
            path
              .canonicalize()
              .with_context(|| format!("locate the CEF runtime at {}", path.display()))?,
          );
        }
      }
    }
  }

  ensure!(
    distributions.len() <= 1,
    "Cargo linked multiple CEF distributions: {distributions:?}"
  );
  if let Some(distribution) = distributions.into_iter().next() {
    if let Some(explicit) = explicit {
      ensure!(
        explicit == distribution,
        "Cargo selected CEF at {}, but --cef-path points to {}",
        distribution.display(),
        explicit.display()
      );
    }
    return Ok(Some(distribution));
  }
  if let Some(explicit) = explicit {
    ensure!(
      has_cef_runtime(&explicit),
      "no CEF runtime found at {}",
      explicit.display()
    );
    return Ok(Some(explicit));
  }
  bail!("Cargo built cef_dll_sys but reported no CEF runtime directory. Specify --cef-path with the CEF distribution.");
}

fn has_cef_runtime(path: &Path) -> bool {
  path.join("Chromium Embedded Framework.framework").is_dir()
    || path.join("libcef.dll").is_file()
    || path.join("libcef.so").is_file()
}

#[cfg(test)]
mod service_build_tests {
  use super::*;

  #[test]
  fn shared_cef_dependency_metadata_does_not_turn_a_service_into_a_cef_process_role() {
    let executable = PathBuf::from("/test/worker");
    let artifact = BuildArtifact {
      executable: executable.clone(),
      package_name: "mixed-app".into(),
      package_dir: PathBuf::from("/test"),
      version: "1.0.0".into(),
      cef_dir: Some(PathBuf::from("/test/shared-cef")),
      target_name: "worker".into(),
      third_party: Vec::new(),
      lintel_metadata: serde_json::Value::Null,
    };
    let helper = crate::service_processes::helper_from_artifact("indexer".into(), artifact);
    assert_eq!(helper.name, "indexer");
    assert_eq!(helper.executable, executable);
  }

  #[test]
  fn service_bins_build_with_the_main_package_features_and_keep_main_selection() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::write(
      root.path().join("Cargo.toml"),
      "[package]\nname=\"service-build-fixture\"\nversion=\"1.2.3\"\nedition=\"2024\"\ndefault-run=\"app\"\n\
       [features]\nmarker=[]\n\
       [[bin]]\nname=\"app\"\npath=\"src/main.rs\"\n\
       [[bin]]\nname=\"worker\"\npath=\"src/worker.rs\"\n\
       [[package.metadata.lintel.service_processes]]\nname=\"echo-worker\"\nbin=\"worker\"\n\
       [workspace]\n",
    )
    .unwrap();
    for (name, role) in [("main.rs", "main"), ("worker.rs", "worker")] {
      std::fs::write(
        root.path().join("src").join(name),
        format!("fn main() {{ println!(\"{role}:{{}}:{{}}\", cfg!(feature=\"marker\"), cfg!(debug_assertions)); }}"),
      )
      .unwrap();
    }
    let rustc = Command::new("rustc").arg("-vV").output().unwrap();
    assert!(rustc.status.success());
    let host = std::str::from_utf8(&rustc.stdout)
      .unwrap()
      .lines()
      .find_map(|line| line.strip_prefix("host: "))
      .unwrap()
      .to_owned();
    let options = BuildOptions {
      manifest_path: root.path().join("Cargo.toml"),
      package: None,
      selection: TargetSelection::Default,
      target: Some(host.clone()),
      release: true,
      locked: false,
      features: vec!["marker".into()],
      cargo_config: None,
      cef_path: None,
    };
    let (main, services) = build_with_service_processes(&options).unwrap();
    assert_eq!(main.target_name, "app");
    assert_eq!(services.len(), 1);
    assert_eq!(services[0].name, "echo-worker");
    assert_ne!(main.executable, services[0].executable);
    for (path, expected) in [
      (&main.executable, "main:true:false"),
      (&services[0].executable, "worker:true:false"),
    ] {
      let profile_dir = path.parent().unwrap();
      assert_eq!(profile_dir.file_name().unwrap(), "release");
      assert_eq!(profile_dir.parent().unwrap().file_name().unwrap(), host.as_str());
      let output = Command::new(path).output().unwrap();
      assert!(output.status.success());
      assert_eq!(std::str::from_utf8(&output.stdout).unwrap().trim(), expected);
    }
    // Choosing the worker explicitly must not cause it to be recursively rebuilt as its own helper.
    let mut worker_options = options;
    worker_options.selection = TargetSelection::Bin("worker".into());
    let worker = build(&worker_options).unwrap();
    assert_eq!(worker.target_name, "worker");
    assert!(crate::build_service_processes(&worker_options, &worker).is_err());
    assert!(build_with_service_processes(&worker_options).is_err());
  }

  fn preflight_fixture(metadata: &str) -> (tempfile::TempDir, BuildOptions) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("src")).unwrap();
    std::fs::create_dir_all(root.path().join("build-probe/src")).unwrap();
    std::fs::write(
      root.path().join("Cargo.toml"),
      format!(
        "[package]\nname=\"service-preflight-fixture\"\nversion=\"1.0.0\"\nedition=\"2024\"\ndefault-run=\"app\"\n\
         [[bin]]\nname=\"app\"\npath=\"src/main.rs\"\n\
         [[bin]]\nname=\"worker\"\npath=\"src/worker.rs\"\n\
         [build-dependencies]\npreflight-build-probe={{path=\"build-probe\"}}\n\
         {metadata}\n[workspace]\n"
      ),
    )
    .unwrap();
    for name in ["main.rs", "worker.rs"] {
      std::fs::write(root.path().join("src").join(name), "fn main() {}\n").unwrap();
    }
    std::fs::write(
      root.path().join("build.rs"),
      "fn main() { preflight_build_probe::present(); std::fs::write(\"application-build-script-ran\", b\"ran\").unwrap(); }\n",
    )
    .unwrap();
    std::fs::write(
      root.path().join("build-probe/Cargo.toml"),
      "[package]\nname=\"preflight-build-probe\"\nversion=\"1.0.0\"\nedition=\"2024\"\n",
    )
    .unwrap();
    std::fs::write(root.path().join("build-probe/src/lib.rs"), "pub fn present() {}\n").unwrap();
    std::fs::write(
      root.path().join("build-probe/build.rs"),
      "fn main() { std::fs::write(\"dependency-build-script-ran\", b\"ran\").unwrap(); }\n",
    )
    .unwrap();
    let options = BuildOptions {
      manifest_path: root.path().join("Cargo.toml"),
      package: None,
      selection: TargetSelection::Default,
      target: None,
      release: false,
      locked: false,
      features: Vec::new(),
      cargo_config: None,
      cef_path: None,
    };
    (root, options)
  }

  #[test]
  fn service_preflight_rejects_all_bad_declarations_before_any_application_or_dependency_build_script() {
    for (metadata, expected) in [
      (
        "[package.metadata.lintel]\nservice_processes=\"not an array\"\n",
        "service_processes must be an array",
      ),
      (
        "[[package.metadata.lintel.service_processes]]\nname=\"first\"\nbin=\"worker\"\n\
         [[package.metadata.lintel.service_processes]]\nname=\"later\"\nbin=\"missing-worker\"\n",
        "no binary named missing-worker",
      ),
      (
        "[[package.metadata.lintel.service_processes]]\nname=\"first\"\nbin=\"worker\"\n\
         [[package.metadata.lintel.service_processes]]\nname=\"self\"\nbin=\"app\"\n",
        "application binary cannot also be declared as a service process",
      ),
      (
        "[[package.metadata.lintel.service_processes]]\nname=\"worker\"\nbin=\"worker\"\nextra=true\n",
        "unknown field",
      ),
    ] {
      let (root, options) = preflight_fixture(metadata);
      let error = build_with_service_processes(&options).unwrap_err();
      assert!(format!("{error:#}").contains(expected), "unexpected error: {error:#}");
      assert!(!root.path().join("application-build-script-ran").exists());
      assert!(!root.path().join("build-probe/dependency-build-script-ran").exists());
    }
  }

  #[test]
  fn single_executable_build_keeps_service_metadata_out_of_its_contract() {
    let (root, options) = preflight_fixture("[package.metadata.lintel]\nservice_processes=\"not an array\"\n");
    let application = build(&options).unwrap();
    assert_eq!(application.target_name, "app");
    assert!(root.path().join("application-build-script-ran").is_file());
    assert!(root.path().join("build-probe/dependency-build-script-ran").is_file());
    assert!(application.service_metadata().is_err());
  }
}
