use super::*;

fn pe(path: &Path, marker: u8) {
  fs::create_dir_all(path.parent().unwrap()).unwrap();
  let mut bytes = vec![0_u8; 0x400];
  bytes[..2].copy_from_slice(b"MZ");
  bytes[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
  bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
  bytes[0x84..0x86].copy_from_slice(&0x8664_u16.to_le_bytes());
  bytes[0x86..0x88].copy_from_slice(&1_u16.to_le_bytes());
  bytes[0x94..0x96].copy_from_slice(&240_u16.to_le_bytes());
  bytes[0x96..0x98].copy_from_slice(&2_u16.to_le_bytes());
  bytes[0x98..0x9a].copy_from_slice(&0x20b_u16.to_le_bytes());
  bytes[0x98 + 60..0x98 + 64].copy_from_slice(&0x200_u32.to_le_bytes());
  bytes[0x98 + 108..0x98 + 112].copy_from_slice(&16_u32.to_le_bytes());
  let section = 0x98 + 240;
  bytes[section + 8..section + 12].copy_from_slice(&0x200_u32.to_le_bytes());
  bytes[section + 12..section + 16].copy_from_slice(&0x1000_u32.to_le_bytes());
  bytes[section + 16..section + 20].copy_from_slice(&0x200_u32.to_le_bytes());
  bytes[section + 20..section + 24].copy_from_slice(&0x200_u32.to_le_bytes());
  bytes[0x200] = marker;
  fs::write(path, bytes).unwrap();
}

fn bundle(base: &Path, version: &str, marker: u8) -> PathBuf {
  let source = base.join(format!("source-{version}.exe"));
  pe(&source, marker);
  lintel_build::bundle_windows(
    &lintel_build::BundleOptions {
      executable: source,
      name: "Fixture".into(),
      identifier: "dev.lintel.fixture".into(),
      version: version.into(),
      output_dir: base.join("bundles"),
      cef_dir: None,
      helper: None,
      dawn_root: None,
      updater_helper: None,
    },
    "x86_64-pc-windows-msvc",
  )
  .unwrap()
}

pub(super) struct Fixture {
  pub(super) temporary: tempfile::TempDir,
  pub(super) root: PathBuf,
  pub(super) private_key: PathBuf,
  pub(super) metadata: Installation,
  pub(super) old: ActiveVersion,
}

pub(super) fn fixture() -> Fixture {
  let temporary = tempfile::tempdir().unwrap();
  #[cfg(windows)]
  native::protect_new(temporary.path()).unwrap();
  let private_key = temporary.path().join("release.key");
  let public_key = temporary.path().join("updater.pub");
  crate::generate_keypair(&private_key, &public_key).unwrap();
  let tool = temporary.path().join("tool.exe");
  pe(&tool, 7);
  let root = temporary.path().join("installation");
  fs::create_dir(&root).unwrap();
  #[cfg(windows)]
  native::protect_new(&root).unwrap();
  install_files(
    &WindowsInstallOptions {
      bundle: bundle(temporary.path(), "1.0.0", 1),
      install_root: Some(root.clone()),
      public_key: fs::read_to_string(public_key).unwrap(),
      launcher: tool.clone(),
      installer: tool,
      protocol_schemes: vec![],
      channel: "stable".into(),
    },
    &root,
  )
  .unwrap();
  let metadata = installation(&root).unwrap();
  let old = read_json(&root.join(ACTIVE)).unwrap();
  Fixture {
    temporary,
    root,
    private_key,
    metadata,
    old,
  }
}

fn add_version(fixture: &Fixture, version: &str, marker: u8) -> ActiveVersion {
  let source = bundle(fixture.temporary.path(), version, marker);
  let output = lintel_build::copy_windows_bundle(&source, &fixture.root.join("versions")).unwrap();
  protect_version(&output).unwrap();
  ActiveVersion {
    directory: output.file_name().unwrap().to_str().unwrap().into(),
    manifest_sha256: lintel_build::validate_windows_bundle(&output).unwrap().manifest_sha256,
  }
}

fn signed_stage(fixture: &Fixture) -> (PathBuf, crate::Release) {
  let source = bundle(fixture.temporary.path(), "2.0.0", 2);
  let stage = fixture.root.join(".lintel-update-fixture");
  fs::create_dir(&stage).unwrap();
  #[cfg(windows)]
  native::protect_new(&stage).unwrap();
  let archive = stage.join("update.tar.gz");
  crate::create_archive(&source, &archive).unwrap();
  let (size, sha256) = crate::archive_digest(&archive).unwrap();
  let release = crate::Release {
    schema: 1,
    app_id: "dev.lintel.fixture".into(),
    version: "2.0.0".into(),
    channel: "stable".into(),
    target: "x86_64-pc-windows-msvc".into(),
    url: "https://example.test/update.tar.gz".into(),
    size,
    sha256,
    expires_at: crate::manifest::now().unwrap() + 3600,
    notes: String::new(),
  };
  fs::write(
    stage.join("release.json"),
    crate::sign_release(&release, &fixture.private_key).unwrap(),
  )
  .unwrap();
  (stage, release)
}

#[test]
fn version_switch_keeps_old_files_and_rolls_back_a_failed_spawn() {
  let fixture = fixture();
  let new = add_version(&fixture, "2.0.0", 2);
  assert!(
    switch(&fixture.root, &fixture.metadata, &fixture.old, &new, |_, _| bail!(
      "injected CreateProcess failure"
    ))
    .is_err()
  );
  assert_eq!(
    read_json::<ActiveVersion>(&fixture.root.join(ACTIVE)).unwrap(),
    fixture.old
  );
  assert!(!fixture.root.join(JOURNAL).exists());
  validate_active(&fixture.root, &fixture.metadata, &fixture.old).unwrap();
  switch(&fixture.root, &fixture.metadata, &fixture.old, &new, |_, _| Ok(())).unwrap();
  assert_eq!(read_json::<ActiveVersion>(&fixture.root.join(ACTIVE)).unwrap(), new);
  assert!(fixture.root.join("versions").join(&fixture.old.directory).is_dir());
}

#[test]
fn failed_spawn_never_rolls_back_a_different_successful_selection() {
  let fixture = fixture();
  let new = add_version(&fixture, "2.0.0", 2);
  let newer = add_version(&fixture, "3.0.0", 3);
  let result = switch(&fixture.root, &fixture.metadata, &fixture.old, &new, |_, _| {
    write_json(&fixture.root.join(ACTIVE), &newer)?;
    bail!("another installer selected a different version");
  });
  assert!(result.unwrap_err().to_string().contains("refusing to roll back"));
  assert_eq!(read_json::<ActiveVersion>(&fixture.root.join(ACTIVE)).unwrap(), newer);
  assert!(!fixture.root.join(JOURNAL).exists());
}

#[test]
fn journal_recovery_handles_complete_old_or_new_records_and_refuses_corrupt_active_state() {
  let fixture = fixture();
  let new = add_version(&fixture, "2.0.0", 2);
  let pending = PendingSwitch {
    installation_id: fixture.metadata.installation_id.clone(),
    old: fixture.old.clone(),
    new: new.clone(),
    health: None,
  };
  write_json(&fixture.root.join(JOURNAL), &pending).unwrap();
  recover(&fixture.root, &fixture.metadata).unwrap();
  assert_eq!(
    read_json::<ActiveVersion>(&fixture.root.join(ACTIVE)).unwrap(),
    fixture.old
  );
  write_json(&fixture.root.join(JOURNAL), &pending).unwrap();
  write_json(&fixture.root.join(ACTIVE), &new).unwrap();
  recover(&fixture.root, &fixture.metadata).unwrap();
  assert_eq!(read_json::<ActiveVersion>(&fixture.root.join(ACTIVE)).unwrap(), new);
  write_json(&fixture.root.join(JOURNAL), &pending).unwrap();
  fs::remove_file(fixture.root.join(ACTIVE)).unwrap();
  assert!(recover(&fixture.root, &fixture.metadata).is_err());
  fs::write(fixture.root.join(ACTIVE), b"corrupt").unwrap();
  assert!(recover(&fixture.root, &fixture.metadata).is_err());
  assert_eq!(fs::read(fixture.root.join(ACTIVE)).unwrap(), b"corrupt");
  assert!(fixture.root.join(JOURNAL).is_file());
}

#[test]
fn helper_revalidates_signature_hash_identity_channel_and_expiry() {
  let fixture = fixture();
  let (stage, release) = signed_stage(&fixture);
  authenticated_release(&stage, &fixture.metadata).unwrap();
  let mut wrong_channel = fixture.metadata.clone();
  wrong_channel.channel = "beta".into();
  assert!(authenticated_release(&stage, &wrong_channel).is_err());
  let mut wrong_id = fixture.metadata.clone();
  wrong_id.app_id = "dev.other.app".into();
  assert!(authenticated_release(&stage, &wrong_id).is_err());
  let mut envelope: serde_json::Value = serde_json::from_slice(&fs::read(stage.join("release.json")).unwrap()).unwrap();
  envelope["signature"] = serde_json::Value::String(STANDARD.encode([0_u8; 64]));
  fs::write(stage.join("release.json"), serde_json::to_vec(&envelope).unwrap()).unwrap();
  assert!(authenticated_release(&stage, &fixture.metadata).is_err());
  let mut expired = release.clone();
  expired.expires_at = crate::manifest::now().unwrap() - 1;
  // The production signer correctly refuses expired releases; build this hostile fixture directly.
  use ed25519_dalek::Signer;
  let seed: [u8; 32] = STANDARD
    .decode(fs::read_to_string(&fixture.private_key).unwrap().trim())
    .unwrap()
    .try_into()
    .unwrap();
  let payload = serde_json::to_vec(&expired).unwrap();
  let signature = ed25519_dalek::SigningKey::from_bytes(&seed).sign(&payload);
  fs::write(
    stage.join("release.json"),
    serde_json::to_vec(
      &serde_json::json!({"payload": STANDARD.encode(&payload), "signature": STANDARD.encode(signature.to_bytes())}),
    )
    .unwrap(),
  )
  .unwrap();
  assert!(authenticated_release(&stage, &fixture.metadata).is_err());
  fs::write(
    stage.join("release.json"),
    crate::sign_release(&release, &fixture.private_key).unwrap(),
  )
  .unwrap();
  fs::OpenOptions::new()
    .append(true)
    .open(stage.join("update.tar.gz"))
    .unwrap()
    .write_all(b"changed")
    .unwrap();
  assert!(authenticated_release(&stage, &fixture.metadata).is_err());
  for version in ["1.0.0", "0.9.0", "1.0.0+different-build"] {
    assert!(require_newer(version, "1.0.0").is_err());
  }
}

#[test]
fn self_consistent_manifest_tampering_during_shutdown_cannot_escape_the_signed_snapshot() {
  let fixture = fixture();
  let (stage, _) = signed_stage(&fixture);
  let release = authenticated_release(&stage, &fixture.metadata).unwrap();
  let (verified, captured) = candidate(&stage, &fixture.metadata, &release, "verified").unwrap();
  let executable = verified.join(&captured.executable);
  fs::OpenOptions::new()
    .append(true)
    .open(&executable)
    .unwrap()
    .write_all(b"different executable")
    .unwrap();
  let (_, digest) = crate::archive_digest(&executable).unwrap();
  let manifest_path = verified.join("lintel-bundle.json");
  let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
  manifest["files"][&captured.executable] = serde_json::Value::String(digest);
  fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
  assert!(lintel_build::validate_windows_bundle(&verified).is_ok());
  assert!(lintel_build::copy_windows_bundle_verified(&verified, &fixture.root.join("versions"), &captured).is_err());
  assert_eq!(
    read_json::<ActiveVersion>(&fixture.root.join(ACTIVE)).unwrap(),
    fixture.old
  );
}

#[test]
fn extraction_hashes_its_own_archive_stream_after_an_authenticated_path_is_replaced() {
  let fixture = fixture();
  let (stage, _) = signed_stage(&fixture);
  let captured = authenticated_release(&stage, &fixture.metadata).unwrap();
  let forged_base = fixture.temporary.path().join("forged-source");
  fs::create_dir(&forged_base).unwrap();
  let forged = bundle(&forged_base, "2.0.0", 9);
  fs::remove_file(stage.join("update.tar.gz")).unwrap();
  crate::create_archive(&forged, &stage.join("update.tar.gz")).unwrap();
  assert!(candidate(&stage, &fixture.metadata, &captured, "after-swap").is_err());
  assert_eq!(
    read_json::<ActiveVersion>(&fixture.root.join(ACTIVE)).unwrap(),
    fixture.old
  );
}

#[test]
fn active_paths_and_protocol_arguments_cannot_become_arbitrary_paths_or_process_options() {
  let fixture = fixture();
  for directory in [
    "../outside",
    r"C:\outside",
    r"\\server\share",
    "versions/sub",
    "NUL",
    "file:stream",
  ] {
    let bad = ActiveVersion {
      directory: directory.into(),
      manifest_sha256: fixture.old.manifest_sha256.clone(),
    };
    assert!(validate_active(&fixture.root, &fixture.metadata, &bad).is_err());
  }
  let schemes = vec!["example".into()];
  assert_eq!(
    protocol_arguments(vec!["--lintel-protocol".into(), "example://open/42".into()], &schemes).unwrap(),
    [OsString::from("example://open/42")]
  );
  for args in [
    vec!["--lintel-protocol", "example://x", "--type=renderer"],
    vec!["--lintel-protocol", "example://x\" --type=renderer"],
    vec!["--lintel-protocol", "example://x'evil"],
    vec!["--lintel-protocol", "example://x\n"],
    vec!["--lintel-protocol", "file:///C:/windows"],
  ] {
    assert!(protocol_arguments(args.into_iter().map(Into::into).collect(), &schemes).is_err());
  }
}

fn hostile_archive(entries: &[(&str, bool)]) -> tempfile::NamedTempFile {
  let file = tempfile::NamedTempFile::new().unwrap();
  let encoder = flate2::write::GzEncoder::new(file.reopen().unwrap(), flate2::Compression::default());
  let mut archive = tar::Builder::new(encoder);
  for (name, symlink) in entries {
    let mut header = tar::Header::new_gnu();
    header.set_size(0);
    header.set_mode(0o600);
    header.set_entry_type(if *symlink {
      tar::EntryType::Symlink
    } else {
      tar::EntryType::Regular
    });
    header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
    if *symlink {
      header.set_link_name("target").unwrap();
    }
    header.set_cksum();
    archive.append(&header, std::io::empty()).unwrap();
  }
  archive.into_inner().unwrap().finish().unwrap();
  file
}

#[test]
fn windows_archive_guard_rejects_cross_host_drive_unc_ads_devices_aliases_and_links() {
  for entries in [
    vec![("C:/escape.exe", false)],
    vec![(r"\\server\share\escape", false)],
    vec![("App/file.dll:stream", false)],
    vec![("App/NUL.exe", false)],
    vec![("App/trailing.", false)],
    vec![("App/a/child.exe", false)],
    vec![("App/Foo.dll", false), ("App/foo.DLL", false)],
    vec![("App/link", true)],
  ] {
    let archive = hostile_archive(&entries);
    let root = tempfile::tempdir().unwrap();
    assert!(
      crate::archive::extract_windows(archive.path(), root.path()).is_err(),
      "{entries:?}"
    );
  }
}

#[test]
fn duplicate_handoff_lease_is_nonblocking_and_released_when_owner_drops() {
  let fixture = fixture();
  let first = lock_named(&fixture.root, "update-running.lock", false).unwrap();
  assert!(lock_named(&fixture.root, "update-running.lock", false).is_err());
  drop(first);
  assert!(lock_named(&fixture.root, "update-running.lock", false).is_ok());
}

fn record_version(fixture: &Fixture, active: &ActiveVersion) {
  let (_, info) = validate_active(&fixture.root, &fixture.metadata, active).unwrap();
  let release = crate::Release {
    schema: 1,
    app_id: info.identifier.clone(),
    version: info.version.clone(),
    channel: "stable".into(),
    target: info.target.clone(),
    url: "https://example.test/history.tar.gz".into(),
    size: 1,
    sha256: "a".repeat(64),
    expires_at: crate::manifest::now().unwrap() + 3600,
    notes: String::new(),
  };
  let envelope = crate::sign_release(&release, &fixture.private_key).unwrap();
  versions::signed(&fixture.root, &fixture.metadata, active, &info, &envelope).unwrap();
}

fn remove_fixture_files(files: &[(PathBuf, String)]) -> Result<()> {
  for (path, hash) in files {
    ensure!(crate::archive_digest(path)?.1 == *hash, "fixture changed");
  }
  for (path, _) in files {
    fs::remove_file(path)?;
  }
  Ok(())
}

#[test]
fn pruning_protects_active_rollback_pending_recent_unknown_and_modified_versions() {
  let fixture = fixture();
  let second = add_version(&fixture, "2.0.0", 2);
  record_version(&fixture, &second);
  let third = add_version(&fixture, "3.0.0", 3);
  record_version(&fixture, &third);
  let fourth = add_version(&fixture, "4.0.0", 4);
  record_version(&fixture, &fourth);
  let unknown = add_version(&fixture, "5.0.0", 5);
  let altered = add_version(&fixture, "6.0.0", 6);
  record_version(&fixture, &altered);
  let (directory, info) = validate_active(&fixture.root, &fixture.metadata, &altered).unwrap();
  fs::write(directory.join(info.executable), b"modified").unwrap();
  write_json(&fixture.root.join(ACTIVE), &third).unwrap();
  versions::set_rollback(&fixture.root, &fixture.metadata, &second).unwrap();
  write_json(
    &fixture.root.join(JOURNAL),
    &PendingSwitch {
      installation_id: fixture.metadata.installation_id.clone(),
      old: third.clone(),
      new: fourth.clone(),
      health: None,
    },
  )
  .unwrap();
  let preview = maintenance::prune_with(&fixture.root, &fixture.metadata, 0, true, |_| {
    panic!("preview must not delete")
  })
  .unwrap();
  assert_eq!(preview.removed, [format!("versions/{}", fixture.old.directory)]);
  assert!(fixture.root.join("versions").join(&fixture.old.directory).exists());
  let applied = maintenance::prune_with(&fixture.root, &fixture.metadata, 0, false, remove_fixture_files).unwrap();
  assert_eq!(applied.removed, preview.removed);
  for version in [&second, &third, &fourth, &unknown, &altered] {
    assert!(fixture.root.join("versions").join(&version.directory).exists());
  }
  assert!(
    applied
      .retained
      .iter()
      .any(|path| path.path.ends_with(&unknown.directory) && path.reason.contains("unregistered"))
  );
  assert!(
    applied
      .retained
      .iter()
      .any(|path| path.path.ends_with(&altered.directory) && path.reason.contains("modified"))
  );
}

#[test]
fn pruning_busy_files_and_recent_versions_are_retained_without_adopting_legacy_directories() {
  let fixture = fixture();
  let new = add_version(&fixture, "2.0.0", 2);
  record_version(&fixture, &new);
  write_json(&fixture.root.join(ACTIVE), &new).unwrap();
  let preview = maintenance::prune_with(&fixture.root, &fixture.metadata, 2, true, |_| unreachable!()).unwrap();
  assert!(preview.removed.is_empty());
  let busy = maintenance::prune_with(&fixture.root, &fixture.metadata, 0, false, |_| {
    bail!("injected resource-in-use")
  })
  .unwrap();
  assert!(busy.removed.is_empty());
  assert!(busy.retained.iter().any(|path| path.reason.contains("in use")));
  fs::remove_file(fixture.root.join(versions::LEDGER)).unwrap();
  let legacy = versions::read(&fixture.root, &fixture.metadata).unwrap();
  assert_eq!(legacy.records.len(), 1);
  assert!(legacy.records.contains_key(&new.directory));
  let report = maintenance::prune_with(&fixture.root, &fixture.metadata, 0, false, |_| {
    panic!("unrecorded old version must not be deleted")
  })
  .unwrap();
  assert!(report.removed.is_empty());
}

#[test]
fn uninstall_removes_only_owned_files_and_preserves_business_data_modified_tools_and_audit() {
  let fixture = fixture();
  let unknown = add_version(&fixture, "2.0.0", 2);
  fs::write(fixture.root.join("customer-database.sqlite"), b"user data").unwrap();
  fs::write(fixture.root.join("tools/custom-tool.exe"), b"unknown tool").unwrap();
  fs::write(fixture.root.join("lintel-launcher.exe"), b"modified launcher").unwrap();
  let preview = maintenance::uninstall_with(
    &fixture.root,
    &fixture.metadata,
    true,
    |_| panic!("preview must not delete"),
    |_| panic!("no associations"),
  )
  .unwrap();
  assert!(!fixture.root.join(maintenance::TOMBSTONE).exists());
  assert!(
    preview
      .removed
      .iter()
      .any(|path| path.ends_with(&fixture.old.directory))
  );
  let report = maintenance::uninstall_with(&fixture.root, &fixture.metadata, false, remove_fixture_files, |_| {
    panic!("no associations")
  })
  .unwrap();
  assert!(maintenance::ensure_live(&fixture.root).is_err());
  assert!(!fixture.root.join("versions").join(&fixture.old.directory).exists());
  assert!(!fixture.root.join("tools/lintel-updater.exe").exists());
  for path in [
    "lintel-launcher.exe",
    "customer-database.sqlite",
    "tools/custom-tool.exe",
    META,
    maintenance::TOMBSTONE,
  ] {
    assert!(fixture.root.join(path).exists());
  }
  assert!(fixture.root.join("versions").join(unknown.directory).exists());
  assert!(
    report
      .retained
      .iter()
      .any(|path| path.path == "customer-database.sqlite")
  );
  assert!(report.retained.iter().any(|path| path.path == "lintel-launcher.exe"));
  let repeated = maintenance::uninstall_with(
    &fixture.root,
    &fixture.metadata,
    false,
    remove_fixture_files,
    |_| unreachable!(),
  )
  .unwrap();
  assert!(repeated.removed.is_empty());
}

#[test]
fn health_timeout_and_interrupted_attempt_require_quiet_before_compare_and_swap_rollback() {
  let fixture = fixture();
  let new = add_version(&fixture, "2.0.0", 2);
  let options = WindowsStartupHealth::new(Duration::from_secs(5), Duration::from_millis(100)).unwrap();
  let error = switch_with_health::<()>(
    &fixture.root,
    &fixture.metadata,
    &fixture.old,
    &new,
    options.clone(),
    |_, _, _| bail!("injected readiness timeout"),
    || Ok(false),
  )
  .unwrap_err();
  assert!(format!("{error:#}").contains("deferred"));
  assert_eq!(read_json::<ActiveVersion>(&fixture.root.join(ACTIVE)).unwrap(), new);
  let pending = read_json::<PendingSwitch>(&fixture.root.join(JOURNAL)).unwrap();
  assert!(health::recover(&fixture.root, &fixture.metadata, &pending, false).is_err());
  let third = add_version(&fixture, "3.0.0", 3);
  write_json(&fixture.root.join(ACTIVE), &third).unwrap();
  assert!(health::recover(&fixture.root, &fixture.metadata, &pending, true).is_err());
  assert_eq!(read_json::<ActiveVersion>(&fixture.root.join(ACTIVE)).unwrap(), third);
  write_json(&fixture.root.join(ACTIVE), &new).unwrap();
  health::recover(&fixture.root, &fixture.metadata, &pending, true).unwrap();
  assert_eq!(
    read_json::<ActiveVersion>(&fixture.root.join(ACTIVE)).unwrap(),
    fixture.old
  );
  assert!(!fixture.root.join(JOURNAL).exists());
  let result = switch_with_health::<()>(
    &fixture.root,
    &fixture.metadata,
    &fixture.old,
    &new,
    options,
    |_, _, _| bail!("exited with code zero without readiness"),
    || Ok(true),
  );
  assert!(result.is_err());
  assert_eq!(
    read_json::<ActiveVersion>(&fixture.root.join(ACTIVE)).unwrap(),
    fixture.old
  );
}

#[test]
fn confirmed_health_commits_rollback_and_old_request_json_remains_compatible() {
  let fixture = fixture();
  let new = add_version(&fixture, "2.0.0", 2);
  let options = WindowsStartupHealth::new(Duration::from_secs(5), Duration::from_millis(100)).unwrap();
  switch_with_health(
    &fixture.root,
    &fixture.metadata,
    &fixture.old,
    &new,
    options,
    |pending, _, _| {
      pending.health.as_mut().unwrap().accept_for_test();
      write_json(&fixture.root.join(JOURNAL), pending)?;
      Ok(())
    },
    || panic!("confirmed health must not request failed-candidate liveness"),
  )
  .unwrap();
  assert_eq!(
    versions::rollback(&fixture.root, &fixture.metadata).unwrap(),
    Some(fixture.old)
  );
  assert_eq!(read_json::<ActiveVersion>(&fixture.root.join(ACTIVE)).unwrap(), new);
  let request:Request=serde_json::from_value(serde_json::json!({"schema":1,"parent":42,"installation_id":fixture.metadata.installation_id,"old":new,"new_version":"3.0.0","relaunch":true})).unwrap();
  assert!(request.health.is_none());
}

#[test]
fn interrupted_health_before_selection_after_selection_and_after_acceptance_recovers_conservatively() {
  let fixture = fixture();
  let new = add_version(&fixture, "2.0.0", 2);
  let mut pending = PendingSwitch {
    installation_id: fixture.metadata.installation_id.clone(),
    old: fixture.old.clone(),
    new: new.clone(),
    health: Some(
      health::Attempt::new(WindowsStartupHealth::new(Duration::from_secs(5), Duration::ZERO).unwrap()).unwrap(),
    ),
  };
  // Journal visible, old active still selected: no child may have been launched yet.
  write_json(&fixture.root.join(JOURNAL), &pending).unwrap();
  health::recover(&fixture.root, &fixture.metadata, &pending, true).unwrap();
  assert_eq!(
    read_json::<ActiveVersion>(&fixture.root.join(ACTIVE)).unwrap(),
    fixture.old
  );
  // Candidate selected before broker can create/confirm a child: return to old only while quiet.
  write_json(&fixture.root.join(JOURNAL), &pending).unwrap();
  write_json(&fixture.root.join(ACTIVE), &new).unwrap();
  assert!(health::recover(&fixture.root, &fixture.metadata, &pending, false).is_err());
  health::recover(&fixture.root, &fixture.metadata, &pending, true).unwrap();
  // Confirmation persisted before final journal cleanup: preserve confirmed candidate and rollback record.
  pending.health.as_mut().unwrap().accept_for_test();
  write_json(&fixture.root.join(JOURNAL), &pending).unwrap();
  write_json(&fixture.root.join(ACTIVE), &new).unwrap();
  health::recover(&fixture.root, &fixture.metadata, &pending, false).unwrap();
  assert_eq!(read_json::<ActiveVersion>(&fixture.root.join(ACTIVE)).unwrap(), new);
  assert_eq!(
    versions::rollback(&fixture.root, &fixture.metadata).unwrap(),
    Some(fixture.old)
  );
}

#[test]
fn copied_or_mismatched_protocol_receipt_never_authorizes_other_installation_removal() {
  let fixture = fixture();
  write_json(&fixture.root.join("protocol-ownership.json"),&serde_json::json!({"schema":1,"app_id":"dev.lintel.fixture","executable":"C:\\Users\\other\\Application\\lintel-launcher.exe","schemes":["example"],"owner":"a".repeat(64)})).unwrap();
  let report = maintenance::uninstall_with(
    &fixture.root,
    &fixture.metadata,
    true,
    |_| unreachable!(),
    |_| panic!("copied receipt cannot authorize external registry removal"),
  )
  .unwrap();
  assert!(report.protocols_removed.is_empty());
  assert!(
    report
      .retained
      .iter()
      .any(|path| path.path == "protocol-ownership.json")
  );
}

#[cfg(unix)]
#[test]
fn maintenance_rejects_linked_tool_and_version_directories_before_any_deletion() {
  use std::os::unix::fs::symlink;
  let fixture = fixture();
  let outside = tempfile::tempdir().unwrap();
  fs::remove_file(fixture.root.join("tools/lintel-updater.exe")).unwrap();
  fs::remove_dir(fixture.root.join("tools")).unwrap();
  symlink(outside.path(), fixture.root.join("tools")).unwrap();
  assert!(
    maintenance::uninstall_with(
      &fixture.root,
      &fixture.metadata,
      false,
      |_| panic!("linked directory must be rejected before delete"),
      |_| unreachable!()
    )
    .is_err()
  );
  assert!(
    maintenance::prune_with(&fixture.root, &fixture.metadata, 0, false, |_| panic!(
      "linked directory must be rejected before delete"
    ))
    .is_err()
  );
}
