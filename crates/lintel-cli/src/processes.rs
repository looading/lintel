//! Explicit service helper and Mojo runtime inputs for existing bundle publishers.

use std::{
  collections::BTreeSet,
  path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use clap::Args;
use lintel_build::{BundleHelper, Library, mojo_runtime_library, validate_service_name};

#[derive(Args, Default)]
pub(crate) struct ProcessBundleArgs {
  /// Trusted Mojo SDK 0.3.0 runtime image, copied into the application bundle without loading it.
  /// Overrides package.metadata.lintel.mojo_runtime.
  #[arg(long)]
  pub(crate) mojo_runtime: Option<PathBuf>,

  /// Additional prebuilt service helper, NAME=PATH; repeat for each service executable.
  #[arg(long = "service-process", value_name = "NAME=PATH", value_parser = parse_service)]
  pub(crate) services: Vec<BundleHelper>,
}

pub(crate) fn parse_service(value: &str) -> Result<BundleHelper, String> {
  let (name, path) = value.split_once('=').ok_or("expected NAME=PATH")?;
  validate_service_name(name).map_err(|error| error.to_string())?;
  if path.is_empty() {
    return Err("service executable path must not be empty".into());
  }
  Ok(BundleHelper {
    name: name.into(),
    executable: PathBuf::from(path),
  })
}

impl ProcessBundleArgs {
  pub(crate) fn append_helpers(&self, helpers: &mut Vec<BundleHelper>, built: Vec<BundleHelper>) -> Result<bool> {
    ensure!(
      self.services.len() + built.len() <= 16,
      "at most 16 service processes may be bundled"
    );
    let has_services = !self.services.is_empty() || !built.is_empty();
    let mut names: BTreeSet<_> = helpers.iter().map(|helper| helper.name.to_ascii_lowercase()).collect();
    for helper in self.services.iter().cloned().chain(built) {
      validate_service_name(&helper.name)?;
      ensure!(
        names.insert(helper.name.to_ascii_lowercase()),
        "duplicate service/helper name {:?}",
        helper.name
      );
      helpers.push(helper);
    }
    Ok(has_services)
  }

  pub(crate) fn append_runtime(
    &self,
    libraries: &mut Vec<Library>,
    package_dir: &Path,
    metadata_runtime: Option<PathBuf>,
    target: Option<&str>,
    has_services: bool,
  ) -> Result<()> {
    let path = self
      .mojo_runtime
      .clone()
      .or_else(|| metadata_runtime.map(|path| package_dir.join(path)));
    ensure!(
      !has_services || path.is_some(),
      "service processes require --mojo-runtime PATH or package.metadata.lintel.mojo_runtime"
    );
    if let Some(path) = path {
      let library = mojo_runtime_library(&path, target).context("prepare explicit Mojo runtime for bundling")?;
      ensure!(
        !libraries
          .iter()
          .any(|other| other.name.eq_ignore_ascii_case(&library.name)
            || other
              .path
              .file_name()
              .zip(library.path.file_name())
              .is_some_and(|(a, b)| a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()))),
        "explicit Mojo runtime collides with an already registered library"
      );
      libraries.push(library);
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn explicit_helper_parse_preserves_paths_and_rejects_reserved_or_traversing_names() {
    let helper = parse_service("echo-worker=/path with space/a=b").unwrap();
    assert_eq!(helper.name, "echo-worker");
    assert_eq!(helper.executable, Path::new("/path with space/a=b"));
    for input in [
      "no-path",
      "worker=",
      "../worker=path",
      "NUL=path",
      "lintel-updater=path",
      "worker.exe=path",
    ] {
      assert!(parse_service(input).is_err(), "{input}");
    }
  }

  #[test]
  fn metadata_and_explicit_helpers_share_one_collision_domain_and_need_a_runtime() {
    let options = ProcessBundleArgs {
      mojo_runtime: None,
      services: vec![parse_service("Worker=a").unwrap()],
    };
    let mut helpers = vec![parse_service("worker=b").unwrap()];
    assert!(options.append_helpers(&mut helpers, Vec::new()).is_err());
    let mut helpers = Vec::new();
    assert!(options.append_helpers(&mut helpers, Vec::new()).unwrap());
    assert!(
      options
        .append_runtime(
          &mut Vec::new(),
          Path::new("."),
          None,
          Some("aarch64-apple-darwin"),
          true
        )
        .is_err()
    );
    assert!(
      options
        .append_runtime(
          &mut Vec::new(),
          Path::new("."),
          None,
          Some("aarch64-apple-darwin"),
          false
        )
        .is_ok()
    );
  }
}
