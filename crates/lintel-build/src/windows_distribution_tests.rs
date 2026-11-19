use super::*;
use crate::windows::tests::pe;
struct Fixture {
  _temp: tempfile::TempDir,
  root: PathBuf,
  bundle: PathBuf,
  launcher: PathBuf,
  updater: PathBuf,
  output: PathBuf,
  private: PathBuf,
  public: String,
}
fn make_fixture() -> Fixture {
  let temp = tempfile::tempdir().unwrap();
  let root = temp.path().canonicalize().unwrap();
  let source = root.join("application.exe");
  let launcher = root.join("launcher.exe");
  let updater = root.join("updater.exe");
  for file in [&source, &launcher, &updater] {
    pe(file, Architecture::X64, false, &[]);
  }
  let bundle = crate::bundle_windows(
    &crate::BundleOptions {
      executable: source,
      name: "Fixture".into(),
      identifier: "dev.lintel.fixture".into(),
      version: "1.0.0".into(),
      output_dir: root.join("bundles"),
      cef_dir: None,
      helper: None,
      dawn_root: None,
      updater_helper: None,
    },
    "x86_64-pc-windows-msvc",
  )
  .unwrap();
  let output = root.join("distributions");
  fs::create_dir(&output).unwrap();
  let seed = [7_u8; 32];
  let signing = SigningKey::from_bytes(&seed);
  let public = STANDARD.encode(signing.verifying_key().as_bytes());
  let private = root.join("private.key");
  let mut options = fs::OpenOptions::new();
  options.write(true).create_new(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  options
    .open(&private)
    .unwrap()
    .write_all(STANDARD.encode(seed).as_bytes())
    .unwrap();
  Fixture {
    _temp: temp,
    root,
    bundle,
    launcher,
    updater,
    output,
    private,
    public,
  }
}
impl Fixture {
  fn plan(&self) -> WindowsDistributionPlan {
    plan_windows_distribution(&WindowsDistributionOptions {
      bundle: self.bundle.clone(),
      launcher: self.launcher.clone(),
      updater: self.updater.clone(),
      public_key: self.public.clone(),
      channel: "stable".into(),
      output_dir: self.output.clone(),
    })
    .unwrap()
  }
}

#[test]
fn mojo_notices_have_fixed_roles_and_are_covered_by_the_distribution_signature() {
  let fixture = make_fixture();
  crate::windows::tests::add_mojo_notice_fixture(&fixture.bundle);
  let plan = fixture.plan();
  for name in crate::service_processes::MOJO_NOTICE_FILES {
    assert_eq!(
      plan.manifest.files[&format!("bundle/mojo-{name}")].role,
      Role::LicenseNotice
    );
  }
  let info = stage_windows_distribution(&plan, &fixture.private).unwrap();
  assert_eq!(
    inspect_windows_distribution(&info.directory, &fixture.public).unwrap(),
    info
  );
  fs::write(info.directory.join("bundle/mojo-LICENSE"), b"changed notice").unwrap();
  assert!(inspect_windows_distribution(&info.directory, &fixture.public).is_err());
}
#[test]
fn plan_stage_verify_produces_a_real_complete_signed_directory_without_private_key_or_trust_claims() {
  let fixture = make_fixture();
  let plan = fixture.plan();
  assert!(fs::read_dir(&fixture.output).unwrap().next().is_none());
  let json = plan.to_json().unwrap();
  assert!(!json.contains("private.key") && !json.contains(&STANDARD.encode([7_u8; 32])));
  let info = stage_windows_distribution(&plan, &fixture.private).unwrap();
  assert!(info.ed25519_manifest_verified && !info.authenticode_chain_verified && !info.native_installation_executed);
  for path in [
    "distribution.json",
    "updater.pub",
    "bundle/lintel-bundle.json",
    "bundle/Fixture.exe",
    LAUNCHER,
    UPDATER,
  ] {
    assert!(info.directory.join(path).is_file(), "{path}");
  }
  assert_eq!(
    inspect_windows_distribution(&info.directory, &fixture.public).unwrap(),
    info
  );
  assert!(
    info
      .directory
      .read_dir()
      .unwrap()
      .all(|entry| entry.unwrap().file_name() != "private.key")
  );
  assert_eq!(stage_windows_distribution(&plan, &fixture.private).unwrap(), info);
}
#[test]
fn signature_requires_an_independent_caller_key_and_covers_policy_and_all_bytes() {
  let fixture = make_fixture();
  let info = stage_windows_distribution(&fixture.plan(), &fixture.private).unwrap();
  let wrong = STANDARD.encode(SigningKey::from_bytes(&[9_u8; 32]).verifying_key().as_bytes());
  assert!(inspect_windows_distribution(&info.directory, &wrong).is_err());
  let original = fs::read(info.directory.join(MANIFEST)).unwrap();
  let mut envelope: Envelope = serde_json::from_slice(&original).unwrap();
  let mut manifest: Manifest = serde_json::from_slice(&STANDARD.decode(&envelope.payload).unwrap()).unwrap();
  manifest.channel = "beta".into();
  envelope.payload = STANDARD.encode(serde_json::to_vec(&manifest).unwrap());
  fs::write(info.directory.join(MANIFEST), envelope_bytes(&envelope).unwrap()).unwrap();
  assert!(inspect_windows_distribution(&info.directory, &fixture.public).is_err());
  fs::write(info.directory.join(MANIFEST), original).unwrap();
  fs::write(info.directory.join(LAUNCHER), b"changed binary").unwrap();
  assert!(inspect_windows_distribution(&info.directory, &fixture.public).is_err());
}
#[test]
fn rejects_unknown_files_directories_and_role_or_target_mismatches() {
  for extra in [
    "unknown.exe",
    "bundle/unknown.exe",
    "tools/unknown.exe",
    "tools/extra/deeper",
  ] {
    let fixture = make_fixture();
    let info = stage_windows_distribution(&fixture.plan(), &fixture.private).unwrap();
    let path = info.directory.join(extra);
    if extra.ends_with("deeper") {
      fs::create_dir_all(path).unwrap();
    } else {
      fs::write(path, b"extra").unwrap();
    }
    assert!(inspect_windows_distribution(&info.directory, &fixture.public).is_err());
  }
  let fixture = make_fixture();
  pe(&fixture.updater, Architecture::X86, false, &[]);
  let options = WindowsDistributionOptions {
    bundle: fixture.bundle,
    launcher: fixture.launcher,
    updater: fixture.updater,
    public_key: fixture.public,
    channel: "stable".into(),
    output_dir: fixture.output,
  };
  assert!(plan_windows_distribution(&options).is_err());
}
#[test]
fn changed_plan_inputs_wrong_key_and_injected_failure_never_replace_existing_outputs() {
  let fixture = make_fixture();
  let plan = fixture.plan();
  let original = stage_windows_distribution(&plan, &fixture.private).unwrap();
  let bytes = fs::read(original.directory.join(MANIFEST)).unwrap();
  fs::write(&fixture.launcher, b"new bytes").unwrap();
  assert!(stage_windows_distribution(&plan, &fixture.private).is_err());
  assert_eq!(fs::read(original.directory.join(MANIFEST)).unwrap(), bytes);
  let fixture = make_fixture();
  let plan = fixture.plan();
  let error = stage_with(&plan, &fixture.private, |_| bail!("injected stage failure")).unwrap_err();
  assert!(format!("{error:#}").contains("injected"));
  assert!(!plan.destination.exists());
  let wrong = fixture.root.join("wrong.key");
  let mut options = fs::OpenOptions::new();
  options.write(true).create_new(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  options
    .open(&wrong)
    .unwrap()
    .write_all(STANDARD.encode([9_u8; 32]).as_bytes())
    .unwrap();
  assert!(stage_windows_distribution(&plan, &wrong).is_err());
}
#[test]
fn signed_manifest_cannot_authorize_unsafe_paths_case_aliases_or_cross_protocol_signatures() {
  let fixture = make_fixture();
  let info = stage_windows_distribution(&fixture.plan(), &fixture.private).unwrap();
  let bytes = fs::read(info.directory.join(MANIFEST)).unwrap();
  let signing = SigningKey::from_bytes(&[7_u8; 32]);
  for path in [
    "../escape",
    "/absolute.exe",
    "C:/drive.exe",
    "bundle/a\\b.exe",
    "bundle/file:ads.exe",
    "bundle/CON.exe",
    "bundle/a/extra.exe",
    "tools/another.exe",
  ] {
    let mut envelope: Envelope = serde_json::from_slice(&bytes).unwrap();
    let mut manifest: Manifest = serde_json::from_slice(&STANDARD.decode(&envelope.payload).unwrap()).unwrap();
    let record = manifest.files[LAUNCHER].clone();
    manifest.files.insert(path.into(), record);
    let payload = serde_json::to_vec(&manifest).unwrap();
    envelope.payload = STANDARD.encode(&payload);
    envelope.signature = STANDARD.encode(signing.sign(&signing_bytes(&payload)).to_bytes());
    fs::write(info.directory.join(MANIFEST), envelope_bytes(&envelope).unwrap()).unwrap();
    assert!(
      inspect_windows_distribution(&info.directory, &fixture.public).is_err(),
      "{path}"
    );
  }
  let mut envelope: Envelope = serde_json::from_slice(&bytes).unwrap();
  let payload = STANDARD.decode(&envelope.payload).unwrap();
  envelope.signature = STANDARD.encode(signing.sign(&payload).to_bytes());
  fs::write(info.directory.join(MANIFEST), envelope_bytes(&envelope).unwrap()).unwrap();
  assert!(
    inspect_windows_distribution(&info.directory, &fixture.public).is_err(),
    "update-style raw-payload signature is not distribution-domain signature"
  );
}
#[cfg(unix)]
#[test]
fn linked_inputs_and_distribution_reparse_aliases_are_not_followed() {
  use std::os::unix::fs::symlink;
  let fixture = make_fixture();
  let plan = fixture.plan();
  let info = stage_windows_distribution(&plan, &fixture.private).unwrap();
  let launcher = info.directory.join(LAUNCHER);
  fs::remove_file(&launcher).unwrap();
  symlink(&fixture.launcher, launcher).unwrap();
  assert!(inspect_windows_distribution(&info.directory, &fixture.public).is_err());
  let alias = fixture.root.join("linked-output");
  symlink(&fixture.output, &alias).unwrap();
  let options = WindowsDistributionOptions {
    bundle: fixture.bundle,
    launcher: fixture.launcher,
    updater: fixture.updater,
    public_key: fixture.public,
    channel: "stable".into(),
    output_dir: alias,
  };
  assert!(plan_windows_distribution(&options).is_err());
}

#[test]
fn private_consumption_snapshot_copies_only_verified_bytes_and_survives_source_tampering() {
  let fixture = make_fixture();
  let info = stage_windows_distribution(&fixture.plan(), &fixture.private).unwrap();
  let (_, manifest) = decode(&info.directory, &fixture.public).unwrap();
  let snapshot = fixture.root.join("consumption-snapshot");
  let sealed = copy_verified_snapshot(&info, &manifest, &snapshot, &fixture.public).unwrap();
  assert_eq!(sealed.manifest_sha256, info.manifest_sha256);
  assert_eq!(sealed.bundle, info.bundle);
  fs::write(info.directory.join("bundle/Fixture.exe"), b"changed after verification").unwrap();
  fs::write(info.directory.join("tools/extra.exe"), b"not signed").unwrap();
  assert!(inspect_windows_distribution(&info.directory, &fixture.public).is_err());
  assert_eq!(
    inspect_windows_distribution(&snapshot, &fixture.public).unwrap(),
    sealed
  );
  assert!(!snapshot.join("tools/extra.exe").exists());
}

#[test]
fn changing_a_previous_version_key_or_policy_cannot_replace_an_immutable_distribution() {
  let fixture = make_fixture();
  let plan = fixture.plan();
  let original = stage_windows_distribution(&plan, &fixture.private).unwrap();
  let envelope = fs::read(original.directory.join(MANIFEST)).unwrap();
  let code = fixture.bundle.join("Fixture.exe");
  let mut bytes = fs::read(&code).unwrap();
  bytes[0x800] ^= 1;
  fs::write(&code, bytes).unwrap();
  let meta = fixture.bundle.join("lintel-bundle.json");
  let mut bundle: serde_json::Value = serde_json::from_slice(&fs::read(&meta).unwrap()).unwrap();
  bundle["files"]["Fixture.exe"] = serde_json::Value::String(windows::digest_file(&code).unwrap());
  fs::write(meta, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();
  let newer_plan = fixture.plan();
  assert!(stage_windows_distribution(&newer_plan, &fixture.private).is_err());
  assert_eq!(fs::read(original.directory.join(MANIFEST)).unwrap(), envelope);
  assert_eq!(
    inspect_windows_distribution(&original.directory, &fixture.public).unwrap(),
    original
  );
  let policy = make_fixture();
  let original = stage_windows_distribution(&policy.plan(), &policy.private).unwrap();
  let envelope = fs::read(original.directory.join(MANIFEST)).unwrap();
  for rotate_key in [false, true] {
    let new_key = SigningKey::from_bytes(&[9_u8; 32]);
    let public = if rotate_key {
      STANDARD.encode(new_key.verifying_key().as_bytes())
    } else {
      policy.public.clone()
    };
    let private = if rotate_key {
      let path = policy.root.join("rotated.key");
      let mut options = fs::OpenOptions::new();
      options.write(true).create_new(true);
      #[cfg(unix)]
      {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
      }
      options
        .open(&path)
        .unwrap()
        .write_all(STANDARD.encode([9_u8; 32]).as_bytes())
        .unwrap();
      path
    } else {
      policy.private.clone()
    };
    let plan = plan_windows_distribution(&WindowsDistributionOptions {
      bundle: policy.bundle.clone(),
      launcher: policy.launcher.clone(),
      updater: policy.updater.clone(),
      public_key: public,
      channel: if rotate_key { "stable" } else { "beta" }.into(),
      output_dir: policy.output.clone(),
    })
    .unwrap();
    assert!(stage_windows_distribution(&plan, &private).is_err());
    assert_eq!(fs::read(original.directory.join(MANIFEST)).unwrap(), envelope);
  }
}

#[test]
fn even_a_trusted_signature_cannot_bypass_case_role_or_identity_constraints() {
  let fixture = make_fixture();
  let info = stage_windows_distribution(&fixture.plan(), &fixture.private).unwrap();
  let original = fs::read(info.directory.join(MANIFEST)).unwrap();
  let signing = SigningKey::from_bytes(&[7_u8; 32]);
  for case in ["case-alias", "role", "target", "version"] {
    let mut envelope: Envelope = serde_json::from_slice(&original).unwrap();
    let mut manifest: Manifest = serde_json::from_slice(&STANDARD.decode(&envelope.payload).unwrap()).unwrap();
    match case {
      "case-alias" => {
        let mut record = manifest.files["bundle/Fixture.exe"].clone();
        record.role = Role::ApplicationHelper;
        manifest.files.insert("bundle/FIXTURE.EXE".into(), record);
      }
      "role" => manifest.files.get_mut(LAUNCHER).unwrap().role = Role::Updater,
      "target" => manifest.bundle.target = "i686-pc-windows-msvc".into(),
      _ => manifest.bundle.version = "9.0.0".into(),
    }
    let bytes = serde_json::to_vec(&manifest).unwrap();
    envelope.payload = STANDARD.encode(&bytes);
    envelope.signature = STANDARD.encode(signing.sign(&signing_bytes(&bytes)).to_bytes());
    fs::write(info.directory.join(MANIFEST), envelope_bytes(&envelope).unwrap()).unwrap();
    let error = inspect_windows_distribution(&info.directory, &fixture.public).unwrap_err();
    if case == "case-alias" {
      assert!(format!("{error:#}").contains("case-colliding"));
    }
  }
}

#[test]
fn mutations_and_added_files_during_staging_are_not_signed_into_the_output() {
  for phase in [1, 2] {
    let fixture = make_fixture();
    let plan = fixture.plan();
    let mut count = 0;
    let result = stage_with(&plan, &fixture.private, |stage| {
      count += 1;
      if count == phase {
        fs::write(stage.join("bundle/extra.exe"), b"not in signed manifest")?;
      }
      Ok(())
    });
    assert!(result.is_err());
    assert!(!plan.destination.exists());
  }
}

#[test]
fn real_signed_directory_fixtures_cover_all_supported_msvc_architectures() {
  let fixture = make_fixture();
  for (arch, target) in [
    (Architecture::X86, "i686-pc-windows-msvc"),
    (Architecture::X64, "x86_64-pc-windows-msvc"),
    (Architecture::Arm64, "aarch64-pc-windows-msvc"),
  ] {
    let source = fixture.root.join(format!("{target}-app.exe"));
    let launcher = fixture.root.join(format!("{target}-launcher.exe"));
    let updater = fixture.root.join(format!("{target}-updater.exe"));
    for path in [&source, &launcher, &updater] {
      pe(path, arch, false, &[]);
    }
    let bundle = crate::bundle_windows(
      &crate::BundleOptions {
        executable: source,
        name: "Architecture".into(),
        identifier: "dev.lintel.architecture".into(),
        version: "1.0.0".into(),
        output_dir: fixture.root.join("architecture-bundles"),
        cef_dir: None,
        helper: None,
        dawn_root: None,
        updater_helper: None,
      },
      target,
    )
    .unwrap();
    let plan = plan_windows_distribution(&WindowsDistributionOptions {
      bundle,
      launcher,
      updater,
      public_key: fixture.public.clone(),
      channel: "stable".into(),
      output_dir: fixture.output.clone(),
    })
    .unwrap();
    let info = stage_windows_distribution(&plan, &fixture.private).unwrap();
    assert_eq!(info.bundle.target, target);
    assert_eq!(
      inspect_windows_distribution(&info.directory, &fixture.public).unwrap(),
      info
    );
  }
}

#[test]
fn destination_created_just_before_publication_is_preserved_without_replace_or_recovery_delete() {
  let fixture = make_fixture();
  let plan = fixture.plan();
  let mut count = 0;
  let result = stage_with(&plan, &fixture.private, |_| {
    count += 1;
    if count == 2 {
      fs::create_dir(&plan.destination)?;
      fs::write(plan.destination.join("user-data.txt"), b"keep")?;
    }
    Ok(())
  });
  assert!(result.is_err());
  assert_eq!(fs::read(plan.destination.join("user-data.txt")).unwrap(), b"keep");
  assert!(!plan.destination.join(MANIFEST).exists());
}

#[cfg(unix)]
#[test]
fn manifest_creation_does_not_follow_an_injected_link_or_overwrite_an_existing_file() {
  use std::os::unix::fs::symlink;
  let fixture = make_fixture();
  let plan = fixture.plan();
  let external = fixture.root.join("keep.txt");
  fs::write(&external, b"keep").unwrap();
  let mut count = 0;
  let result = stage_with(&plan, &fixture.private, |stage| {
    count += 1;
    if count == 1 {
      symlink(&external, stage.join(MANIFEST))?;
    }
    Ok(())
  });
  assert!(result.is_err());
  assert_eq!(fs::read(&external).unwrap(), b"keep");
  assert!(!plan.destination.exists());
}

#[test]
fn output_cannot_add_staging_or_lock_files_to_the_portable_source_bundle() {
  let fixture = make_fixture();
  let original = crate::validate_windows_bundle(&fixture.bundle).unwrap();
  let result = plan_windows_distribution(&WindowsDistributionOptions {
    bundle: fixture.bundle.clone(),
    launcher: fixture.launcher,
    updater: fixture.updater,
    public_key: fixture.public,
    channel: "stable".into(),
    output_dir: fixture.bundle.clone(),
  });
  assert!(result.is_err());
  assert_eq!(crate::validate_windows_bundle(&fixture.bundle).unwrap(), original);
}

#[test]
fn distribution_schemes_share_windows_registration_static_policy_before_signing_or_install() {
  for schemes in [vec![], vec!["lintel-example".to_owned()]] {
    assert!(crate::windows_protocol::validate_windows_protocol_schemes(&schemes).is_ok());
  }
  for scheme in ["http", "HTTPS", "file", "CON", "custom.", "ms-settings"] {
    let fixture = make_fixture();
    let path = fixture.bundle.join("lintel-bundle.json");
    let mut metadata: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    metadata["protocol_schemes"] = serde_json::json!([scheme]);
    fs::write(&path, serde_json::to_vec_pretty(&metadata).unwrap()).unwrap();
    assert!(
      crate::validate_windows_bundle(&fixture.bundle).is_ok(),
      "portable metadata alone permits {scheme}"
    );
    let result = plan_windows_distribution(&WindowsDistributionOptions {
      bundle: fixture.bundle.clone(),
      launcher: fixture.launcher.clone(),
      updater: fixture.updater.clone(),
      public_key: fixture.public.clone(),
      channel: "stable".into(),
      output_dir: fixture.output.clone(),
    });
    assert!(
      result.is_err(),
      "distribution must apply Windows association policy to {scheme}"
    );
    assert!(fs::read_dir(&fixture.output).unwrap().next().is_none());
  }
  let fixture = make_fixture();
  let info = stage_windows_distribution(&fixture.plan(), &fixture.private).unwrap();
  let mut envelope: Envelope = serde_json::from_slice(&fs::read(info.directory.join(MANIFEST)).unwrap()).unwrap();
  let mut manifest: Manifest = serde_json::from_slice(&STANDARD.decode(&envelope.payload).unwrap()).unwrap();
  // A trusted signature does not waive the shared static installability policy.
  manifest.bundle.protocol_schemes = vec!["http".into()];
  let payload = serde_json::to_vec(&manifest).unwrap();
  envelope.payload = STANDARD.encode(&payload);
  envelope.signature = STANDARD.encode(
    SigningKey::from_bytes(&[7_u8; 32])
      .sign(&signing_bytes(&payload))
      .to_bytes(),
  );
  fs::write(info.directory.join(MANIFEST), envelope_bytes(&envelope).unwrap()).unwrap();
  let error = inspect_windows_distribution(&info.directory, &fixture.public).unwrap_err();
  assert!(format!("{error:#}").contains("system protocol scheme"));
}
