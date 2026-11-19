//! The distribution lease stays live throughout the unchanged protected initial-install path.
use super::*;

fn options(info: &lintel_build::WindowsDistributionInfo, install_root: Option<&Path>) -> WindowsInstallOptions {
  WindowsInstallOptions {
    bundle: info.directory.join("bundle"),
    install_root: install_root.map(Path::to_owned),
    public_key: info.public_key.clone(),
    launcher: info.directory.join("tools/lintel-launcher.exe"),
    installer: info.directory.join("tools/lintel-updater.exe"),
    protocol_schemes: info.bundle.protocol_schemes.clone(),
    channel: info.channel.clone(),
  }
}
fn check_target(root: &Path, original: &Path, snapshot: &Path) -> Result<()> {
  for source in [original, snapshot] {
    #[cfg(windows)]
    let overlaps = native::equal_prefix(root, source, true)
      || native::equal_prefix(root, source, false)
      || native::equal_prefix(source, root, false);
    #[cfg(not(windows))]
    let overlaps = root.starts_with(source) || source.starts_with(root);
    ensure!(
      !overlaps,
      "managed installation destination must not overlap its signed distribution inputs"
    );
  }
  Ok(())
}
/// Caller must pin a separately trusted Ed25519 public key. The package's own key cannot establish
/// publisher identity. No channel/scheme/tool overrides are accepted for these signed inputs.
pub fn install_windows_distribution(
  directory: &Path,
  trusted_public_key: &str,
  install_root: Option<&Path>,
) -> Result<PathBuf> {
  #[cfg(windows)]
  {
    let lease = lintel_build::lease_windows_distribution(directory, trusted_public_key)?;
    install_verified_windows_distribution(lease, install_root)
  }
  #[cfg(not(windows))]
  {
    let _ = (directory, trusted_public_key, install_root);
    bail!(
      "native per-user Windows distribution installation requires Windows; inspect the signed directory on this host instead"
    )
  }
}

/// Inspect `lease.info()` in a native installer UI, then consume that same opaque snapshot.
/// No mutable source paths are reopened after a caller approves this verified lease.
pub fn install_verified_windows_distribution(
  lease: lintel_build::WindowsDistributionLease,
  install_root: Option<&Path>,
) -> Result<PathBuf> {
  #[cfg(windows)]
  {
    let info = lease.info();
    let requested = options(info, install_root);
    // The opaque lease prevents source file writes/deletion and parent replacement while the
    // original installer revalidates/copies these exact paths and captures ownership hashes.
    install_windows_app_checked(&requested, |root| check_target(root, lease.source(), &info.directory))
  }
  #[cfg(not(windows))]
  {
    let _ = (lease, install_root);
    bail!(
      "native per-user Windows distribution installation requires Windows; inspect the signed directory on this host instead"
    )
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn verified_distribution_maps_frozen_policy_and_tools_to_the_existing_install_core() {
    let fixture = super::super::tests::fixture();
    let output = fixture.temporary.path().join("distribution-output");
    fs::create_dir(&output).unwrap();
    let plan = lintel_build::plan_windows_distribution(&lintel_build::WindowsDistributionOptions {
      bundle: fixture
        .root
        .join("versions")
        .join(&fixture.old.directory)
        .canonicalize()
        .unwrap(),
      launcher: fixture.root.join("lintel-launcher.exe").canonicalize().unwrap(),
      updater: fixture.root.join("tools/lintel-updater.exe").canonicalize().unwrap(),
      public_key: fixture.metadata.public_key.clone(),
      channel: "stable".into(),
      output_dir: output.canonicalize().unwrap(),
    })
    .unwrap();
    let info = lintel_build::stage_windows_distribution(&plan, &fixture.private_key.canonicalize().unwrap()).unwrap();
    let root = fixture.temporary.path().join("new-installation");
    fs::create_dir(&root).unwrap();
    #[cfg(windows)]
    native::protect_new(&root).unwrap();
    let requested = options(&info, Some(&root));
    assert_eq!(requested.public_key, fixture.metadata.public_key);
    assert_eq!(requested.channel, info.channel);
    assert_eq!(requested.protocol_schemes, info.bundle.protocol_schemes);
    // This is the existing cross-host file/layout model, not a native LocalAppData install claim.
    install_files(&requested, &root).unwrap();
    let installed = installation(&root).unwrap();
    let active = read_json::<ActiveVersion>(&root.join(ACTIVE)).unwrap();
    assert_eq!(validate_active(&root, &installed, &active).unwrap().1, info.bundle);
    assert_eq!(
      installed.launcher_sha256,
      lintel_build::validate_windows_tool(&requested.launcher, &info.bundle.target).unwrap()
    );
    assert_eq!(
      installed.installer_sha256,
      lintel_build::validate_windows_tool(&requested.installer, &info.bundle.target).unwrap()
    );
    assert!(installed.public_key == info.public_key && root.join(versions::LEDGER).is_file());
  }
  #[cfg(not(windows))]
  #[test]
  fn native_distribution_install_is_not_fabricated_on_a_non_windows_host() {
    let error = install_windows_distribution(Path::new("not-a-native-install"), "untrusted", None).unwrap_err();
    assert!(format!("{error:#}").contains("requires Windows"));
  }
  #[test]
  fn overlapping_multilevel_target_is_vetoed_before_any_source_directory_creation() {
    let fixture = super::super::tests::fixture();
    let output = fixture.temporary.path().join("overlap-distribution-output");
    fs::create_dir(&output).unwrap();
    let plan = lintel_build::plan_windows_distribution(&lintel_build::WindowsDistributionOptions {
      bundle: fixture
        .root
        .join("versions")
        .join(&fixture.old.directory)
        .canonicalize()
        .unwrap(),
      launcher: fixture.root.join("lintel-launcher.exe").canonicalize().unwrap(),
      updater: fixture.root.join("tools/lintel-updater.exe").canonicalize().unwrap(),
      public_key: fixture.metadata.public_key.clone(),
      channel: "stable".into(),
      output_dir: output.canonicalize().unwrap(),
    })
    .unwrap();
    let info = lintel_build::stage_windows_distribution(&plan, &fixture.private_key.canonicalize().unwrap()).unwrap();
    let tail = [OsString::from("new-parent"), OsString::from("new-root")];
    let mut creates = 0;
    let result = create_checked_install_tail(
      &info.directory,
      &tail,
      |candidate| check_target(candidate, &info.directory, &info.directory),
      |path, _| {
        creates += 1;
        fs::create_dir(path)?;
        Ok(())
      },
    );
    assert!(result.is_err());
    assert_eq!(creates, 0);
    assert!(!info.directory.join("new-parent").exists());
    assert_eq!(
      lintel_build::inspect_windows_distribution(&info.directory, &fixture.metadata.public_key).unwrap(),
      info
    );
  }
}
