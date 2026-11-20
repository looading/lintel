//! Application-owned service executables and explicit Mojo runtime packaging.

use std::{
  collections::BTreeSet,
  io::Read,
  path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::{BuildArtifact, BuildOptions, BundleHelper, Library, LibraryKind, TargetSelection};

/// A helper executable from the selected application's Cargo package.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServiceProcess {
  pub name: String,
  pub bin: String,
}

/// Declarative service packaging. Paths are resolved relative to the selected package.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceMetadata {
  pub processes: Vec<ServiceProcess>,
  pub mojo_runtime: Option<PathBuf>,
}

impl ServiceMetadata {
  pub(crate) fn parse(metadata: &serde_json::Value) -> Result<Self> {
    if metadata.is_null() {
      return Ok(Self::default());
    }
    let table = metadata
      .as_object()
      .context("package.metadata.lintel must be a table")?;
    let processes: Vec<ServiceProcess> = match table.get("service_processes") {
      None => Vec::new(),
      Some(value) => serde_json::from_value(value.clone())
        .context("package.metadata.lintel.service_processes must be an array of { name, bin } tables")?,
    };
    ensure!(processes.len() <= 16, "at most 16 service processes may be bundled");
    let mut names = BTreeSet::new();
    let mut bins = BTreeSet::new();
    for process in &processes {
      validate_service_name(&process.name)?;
      validate_bin_name(&process.bin)?;
      ensure!(
        names.insert(process.name.to_ascii_lowercase()),
        "duplicate service process name {:?}",
        process.name
      );
      ensure!(
        bins.insert(process.bin.to_ascii_lowercase()),
        "duplicate service process bin {:?}",
        process.bin
      );
    }
    let mojo_runtime = match table.get("mojo_runtime") {
      None => None,
      Some(value) => {
        let value = value
          .as_str()
          .filter(|value| !value.is_empty())
          .context("package.metadata.lintel.mojo_runtime must be a nonempty path string")?;
        Some(PathBuf::from(value))
      }
    };
    Ok(Self {
      processes,
      mojo_runtime,
    })
  }
}

/// Validate a portable logical helper name, excluding framework-owned executables.
pub fn validate_service_name(name: &str) -> Result<()> {
  ensure!(
    name.len() <= 100
      && !name.is_empty()
      && name.as_bytes()[0].is_ascii_alphanumeric()
      && name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
    "service process name must be at most 100 ASCII letters, digits, '-' or '_', starting with a letter or digit"
  );
  validate_bin_name(name)?;
  ensure!(
    ![
      "lintel-updater",
      "lintel-crash-helper",
      "lintel-worker-crash-helper",
      "lintel-crash-uploader",
      "lintel-snapshot-helper",
      "lintel-cef-helper",
      "cef-helper",
      "crashpad_handler"
    ]
    .iter()
    .any(|reserved| name.eq_ignore_ascii_case(reserved)),
    "service process name {name:?} is reserved by the framework"
  );
  Ok(())
}

fn validate_bin_name(name: &str) -> Result<()> {
  crate::validate_helpers(&[BundleHelper {
    name: name.to_owned(),
    executable: PathBuf::new(),
  }])?;
  crate::validate_windows_filename(&format!("{name}.exe"))?;
  Ok(())
}

/// Build all declared service bins with the main application's package, target and profile.
/// The existing `build` API still selects one executable; callers opt into this composition.
pub fn build_service_processes(options: &BuildOptions, application: &BuildArtifact) -> Result<Vec<BundleHelper>> {
  let metadata = application.service_metadata()?;
  ensure!(
    metadata
      .processes
      .iter()
      .all(|process| !process.bin.eq_ignore_ascii_case(&application.target_name)),
    "the application binary cannot also be declared as a service process"
  );
  metadata
    .processes
    .into_iter()
    .map(|process| {
      let mut options = options.clone();
      options.package = Some(application.package_name.clone());
      options.selection = TargetSelection::Bin(process.bin.clone());
      let artifact = crate::build(&options).with_context(|| format!("build service process {:?}", process.name))?;
      Ok(helper_from_artifact(process.name, artifact))
    })
    .collect()
}

pub(crate) fn helper_from_artifact(name: String, artifact: BuildArtifact) -> BundleHelper {
  // Dependency discovery does not assign a process role. A WebView application's
  // worker shares Cargo features and may report cef_dir without running CEF.
  // The existing bundle publisher verifies the actual helper image/dependencies.
  BundleHelper {
    name,
    executable: artifact.executable,
  }
}

/// Describe an explicitly supplied runtime image without loading it or mutating a project registry.
/// The existing bundle publisher verifies image type, architecture and dependencies before publication.
pub fn mojo_runtime_library(path: &Path, target: Option<&str>) -> Result<Library> {
  let filename =
    if target.is_some_and(|target| target.contains("-windows-")) || (target.is_none() && cfg!(target_os = "windows")) {
      "mojo_rs_native.dll"
    } else if target.is_some_and(|target| target.ends_with("-apple-darwin"))
      || (target.is_none() && cfg!(target_os = "macos"))
    {
      "libmojo_rs_native.dylib"
    } else {
      anyhow::bail!("Mojo runtime bundling supports macOS or Windows targets only");
    };
  ensure!(
    path.file_name().and_then(|name| name.to_str()) == Some(filename),
    "Mojo runtime must be named {filename}"
  );
  let metadata = std::fs::symlink_metadata(path).context("inspect explicit Mojo runtime")?;
  ensure!(
    metadata.is_file() && !metadata.is_symlink(),
    "Mojo runtime must be a real file, not a symlink"
  );
  Ok(Library {
    name: "mojo-ipc".into(),
    kind: LibraryKind::Dylib,
    path: path.canonicalize().context("locate explicit Mojo runtime")?,
    link_name: "mojo_rs_native".into(),
    version: Some("0.3.0".into()),
  })
}

pub(crate) const MOJO_NOTICE_FILES: [&str; 3] = ["LICENSE", "CHROMIUM_LICENSE", "THIRD_PARTY_NOTICES.txt"];
pub(crate) const MAX_MOJO_NOTICE_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) fn is_mojo_runtime(library: &Library) -> bool {
  library.name.eq_ignore_ascii_case("mojo-ipc") || library.link_name.eq_ignore_ascii_case("mojo_rs_native")
}

pub(crate) fn is_windows_mojo_notice(name: &str) -> bool {
  MOJO_NOTICE_FILES.iter().any(|notice| name == format!("mojo-{notice}"))
}

/// Preserve upstream notices in the staged bundle before it can be signed or published.
pub(crate) fn copy_mojo_notices(library: &Library, destination: &Path, flat_windows: bool) -> Result<Vec<PathBuf>> {
  let source = library.path.parent().context("Mojo runtime has no source directory")?;
  std::fs::create_dir_all(destination)?;
  let mut copied = Vec::new();
  for name in MOJO_NOTICE_FILES {
    let input = source.join(name);
    let metadata =
      std::fs::symlink_metadata(&input).with_context(|| format!("Mojo runtime requires adjacent {name}"))?;
    ensure!(
      metadata.is_file() && !metadata.is_symlink() && metadata.len() > 0 && metadata.len() <= MAX_MOJO_NOTICE_BYTES,
      "Mojo notice {name} must be a nonempty real file of at most 16 MiB"
    );
    let mut input = std::fs::File::open(&input)?.take(MAX_MOJO_NOTICE_BYTES + 1);
    let output = destination.join(if flat_windows {
      format!("mojo-{name}")
    } else {
      name.into()
    });
    let mut file = std::fs::File::create_new(&output)?;
    let size = std::io::copy(&mut input, &mut file)?;
    ensure!(
      size > 0 && size <= MAX_MOJO_NOTICE_BYTES,
      "Mojo notice {name} changed or exceeded its size limit"
    );
    copied.push(output);
  }
  Ok(copied)
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn metadata_preserves_package_relative_runtime_and_ordered_services() {
    let data = ServiceMetadata::parse(
      &json!({"id":"dev.example.app", "mojo_runtime":"native/libmojo_rs_native.dylib",
      "service_processes":[{"name":"indexer", "bin":"index-worker"},{"name":"search", "bin":"search-worker"}]}),
    )
    .unwrap();
    assert_eq!(data.mojo_runtime, Some(PathBuf::from("native/libmojo_rs_native.dylib")));
    assert_eq!(
      data.processes[1],
      ServiceProcess {
        name: "search".into(),
        bin: "search-worker".into()
      }
    );
    assert!(
      ServiceMetadata::parse(&serde_json::Value::Null)
        .unwrap()
        .processes
        .is_empty()
    );
  }

  #[test]
  fn metadata_rejects_traversal_case_collisions_reserved_names_and_unknown_service_options() {
    for data in [
      json!({"service_processes":[{"name":"../worker","bin":"worker"}]}),
      json!({"service_processes":[{"name":"worker","bin":"../worker"}]}),
      json!({"service_processes":[{"name":"Worker","bin":"one"},{"name":"worker","bin":"two"}]}),
      json!({"service_processes":[{"name":"one","bin":"Worker"},{"name":"two","bin":"worker"}]}),
      json!({"service_processes":[{"name":"lintel-updater","bin":"worker"}]}),
      json!({"service_processes":[{"name":"lintel-worker-crash-helper","bin":"worker"}]}),
      json!({"service_processes":[{"name":"lintel-crash-uploader","bin":"worker"}]}),
      json!({"service_processes":[{"name":"LINTEL-SNAPSHOT-HELPER","bin":"worker"}]}),
      json!({"service_processes":[{"name":"NUL","bin":"worker"}]}),
      json!({"service_processes":[{"name":"worker.exe","bin":"worker"}]}),
      json!({"service_processes":[{"name":"worker.name","bin":"worker"}]}),
      json!({"service_processes":[{"name":"worker","bin":"worker","args":[]}]}),
      json!({"mojo_runtime":false}),
      json!({"mojo_runtime":""}),
    ] {
      assert!(ServiceMetadata::parse(&data).is_err(), "{data}");
    }
    let processes: Vec<_> = (0..17)
      .map(|n| json!({"name":format!("worker-{n}"),"bin":format!("worker-{n}")}))
      .collect();
    assert!(ServiceMetadata::parse(&json!({"service_processes":processes})).is_err());
    assert!(validate_service_name(&"w".repeat(100)).is_ok());
    assert!(validate_service_name(&"w".repeat(101)).is_err());
    assert!(ServiceMetadata::parse(&json!({"service_processes":[{"name":"worker","bin":"worker.binary"}]})).is_ok());
  }

  #[test]
  fn runtime_packaging_never_loads_library_and_requires_explicit_matching_filename() {
    let root = tempfile::tempdir().unwrap();
    let library = root.path().join("mojo_rs_native.dll");
    std::fs::write(&library, b"not loaded by planning").unwrap();
    let planned = mojo_runtime_library(&library, Some("x86_64-pc-windows-msvc")).unwrap();
    assert_eq!(planned.link_name, "mojo_rs_native");
    assert!(mojo_runtime_library(&library, Some("aarch64-apple-darwin")).is_err());
    assert!(mojo_runtime_library(&library, Some("x86_64-unknown-linux-gnu")).is_err());
    assert!(mojo_runtime_library(&root.path().join("missing.dll"), Some("x86_64-pc-windows-msvc")).is_err());
  }

  #[test]
  fn notice_staging_requires_all_real_bounded_files_and_preserves_bytes() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("sdk");
    std::fs::create_dir(&source).unwrap();
    let image = source.join("mojo_rs_native.dll");
    std::fs::write(&image, b"planning only").unwrap();
    let library = mojo_runtime_library(&image, Some("x86_64-pc-windows-msvc")).unwrap();
    assert!(copy_mojo_notices(&library, &root.path().join("missing"), false).is_err());
    for name in MOJO_NOTICE_FILES {
      std::fs::write(source.join(name), format!("exact upstream {name}\n")).unwrap();
    }
    let copied = copy_mojo_notices(&library, &root.path().join("flat"), true).unwrap();
    assert_eq!(copied.len(), 3);
    for (path, name) in copied.iter().zip(MOJO_NOTICE_FILES) {
      assert!(is_windows_mojo_notice(path.file_name().unwrap().to_str().unwrap()));
      assert_eq!(std::fs::read(path).unwrap(), std::fs::read(source.join(name)).unwrap());
    }
    std::fs::File::create(source.join("LICENSE"))
      .unwrap()
      .set_len(MAX_MOJO_NOTICE_BYTES + 1)
      .unwrap();
    assert!(copy_mojo_notices(&library, &root.path().join("oversize"), false).is_err());
    #[cfg(unix)]
    {
      std::fs::remove_file(source.join("LICENSE")).unwrap();
      std::os::unix::fs::symlink(source.join("CHROMIUM_LICENSE"), source.join("LICENSE")).unwrap();
      assert!(copy_mojo_notices(&library, &root.path().join("linked"), false).is_err());
    }
  }
}
