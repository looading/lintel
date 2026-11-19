use super::*;
use crate::windows::tests::pe;

struct Fixture {
  _temporary: tempfile::TempDir,
  root: PathBuf,
  source: PathBuf,
  output: PathBuf,
  options: AuthenticodeOptions,
}
fn make_fixture() -> Fixture {
  let temporary = tempfile::tempdir().unwrap();
  let root = temporary.path().canonicalize().unwrap();
  let input = root.join("source.exe");
  let tool = root.join("signtool.exe");
  pe(&input, Architecture::X64, false, &[]);
  pe(&tool, Architecture::X64, false, &[]);
  let source = crate::bundle_windows(
    &crate::BundleOptions {
      executable: input,
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
  let output = root.join("signed");
  fs::create_dir(&output).unwrap();
  let options = AuthenticodeOptions::new(tool, "ab".repeat(20), "https://timestamp.example.test/rfc3161").unwrap();
  Fixture {
    _temporary: temporary,
    root,
    source,
    output,
    options,
  }
}
#[derive(Default)]
struct FakeRunner {
  fail: Option<String>,
  mutate_code: bool,
  mutate_manifest: bool,
  mutate_notice: bool,
  trace: Vec<String>,
}
impl Runner for FakeRunner {
  fn run(&mut self, _tool: &Path, args: &[String], phase: &str, _timeout: Duration) -> Result<()> {
    self.trace.push(phase.into());
    ensure!(
      self.fail.as_deref() != Some(phase),
      "simulated SignTool warning/failure exit 2"
    );
    let path = Path::new(args.last().unwrap());
    if phase == "sign" {
      let parsed = image(path)?;
      let mut bytes = fs::read(path)?;
      bytes.truncate(parsed.prefix_len);
      while bytes.len() % 8 != 0 {
        bytes.push(0);
      }
      let offset = bytes.len();
      bytes.extend(16_u32.to_le_bytes());
      bytes.extend(0x200_u16.to_le_bytes());
      bytes.extend(2_u16.to_le_bytes());
      bytes.extend(*b"MODEL001");
      bytes[parsed.security..parsed.security + 4].copy_from_slice(&(offset as u32).to_le_bytes());
      bytes[parsed.security + 4..parsed.security + 8].copy_from_slice(&16_u32.to_le_bytes());
      if self.mutate_code {
        bytes[0x800] ^= 1;
      }
      fs::write(path, bytes)?;
      if self.mutate_manifest {
        fs::write(path.parent().unwrap().join("lintel-bundle.json"), b"{}")?;
      }
      if self.mutate_notice {
        fs::write(path.parent().unwrap().join("mojo-LICENSE"), b"changed notice")?;
      }
    } else if phase == "timestamp" {
      let mut bytes = fs::read(path)?;
      *bytes.last_mut().unwrap() = b'T';
      fs::write(path, bytes)?;
    }
    Ok(())
  }
}
#[test]
fn plan_is_read_only_uses_exact_store_selector_sha256_rfc3161_and_strict_verify() {
  let fixture = make_fixture();
  let plan = plan_windows_authenticode_bundle(&fixture.source, &fixture.output, fixture.options).unwrap();
  assert!(!plan.destination.exists());
  assert!(fs::read_dir(&fixture.output).unwrap().next().is_none());
  assert_eq!(plan.commands.len(), 3);
  assert_eq!(plan.commands[0].arguments[..3], ["sign", "/fd", "SHA256"]);
  assert!(plan.commands[0].arguments.contains(&"/sha1".into()) && !plan.commands[0].arguments.contains(&"/a".into()));
  assert_eq!(plan.commands[2].arguments[..4], ["verify", "/pa", "/all", "/tw"]);
  let json = plan.to_json().unwrap();
  assert!(json.contains("/td") && json.contains("Ed25519") && !json.contains("/p\""));
}
#[test]
fn rejects_ambiguous_credentials_insecure_timestamp_and_invalid_timeout() {
  for thumb in ["", "auto", "1234", &"a".repeat(41)] {
    assert!(AuthenticodeOptions::new("signtool.exe", thumb, "https://tsa.example.test").is_err());
  }
  for url in [
    "http://tsa.example.test",
    "https://user:secret@tsa.example.test",
    "https://tsa.example.test/?token=secret",
    "https://tsa.example.test/#secret",
    "https://tsa.example.test/\n",
  ] {
    assert!(AuthenticodeOptions::new("signtool.exe", "a".repeat(40), url).is_err());
  }
  let options = AuthenticodeOptions::new("signtool.exe", "a".repeat(40), "https://tsa.example.test").unwrap();
  assert!(options.clone().timeout(Duration::ZERO).is_err());
  assert!(options.certificate_store("My /p secret").is_err());
}
#[test]
fn successful_model_signs_copy_updates_manifest_and_leaves_original_and_release_order_intact() {
  let fixture = make_fixture();
  let original = crate::validate_windows_bundle(&fixture.source).unwrap();
  let before = fs::read(fixture.source.join("Fixture.exe")).unwrap();
  let plan = plan_windows_authenticode_bundle(&fixture.source, &fixture.output, fixture.options).unwrap();
  let receipt = execute_with(&plan, &mut FakeRunner::default(), false).unwrap();
  assert!(!receipt.native_execution && receipt.completed_phases == 3 && receipt.release_must_be_created_after_signing);
  assert_eq!(fs::read(fixture.source.join("Fixture.exe")).unwrap(), before);
  assert_eq!(crate::validate_windows_bundle(&fixture.source).unwrap(), original);
  let final_info = crate::validate_windows_bundle(&receipt.destination).unwrap();
  assert_ne!(final_info.manifest_sha256, original.manifest_sha256);
  assert_eq!(final_info.version, original.version);
}

#[test]
fn mojo_notices_are_preserved_unsigned_and_tampering_prevents_publication() {
  for mutate in [false, true] {
    let fixture = make_fixture();
    crate::windows::tests::add_mojo_notice_fixture(&fixture.source);
    let original = fs::read(fixture.source.join("mojo-LICENSE")).unwrap();
    let plan = plan_windows_authenticode_bundle(&fixture.source, &fixture.output, fixture.options).unwrap();
    assert_eq!(plan.inputs.len(), 2);
    assert_eq!(plan.commands.len(), 6);
    assert!(
      plan
        .inputs
        .iter()
        .all(|input| !crate::service_processes::is_windows_mojo_notice(&input.name))
    );
    let mut runner = FakeRunner {
      mutate_notice: mutate,
      ..Default::default()
    };
    let result = execute_with(&plan, &mut runner, false);
    if mutate {
      assert!(result.is_err());
      assert!(!plan.destination.exists());
    } else {
      let receipt = result.unwrap();
      assert_eq!(fs::read(receipt.destination.join("mojo-LICENSE")).unwrap(), original);
      crate::validate_windows_bundle(&receipt.destination).unwrap();
    }
    assert_eq!(fs::read(fixture.source.join("mojo-LICENSE")).unwrap(), original);
  }
}
#[test]
fn every_tool_phase_failure_including_warning_prevents_publication_and_preserves_source() {
  for phase in ["sign", "timestamp", "verify"] {
    let fixture = make_fixture();
    let before = crate::validate_windows_bundle(&fixture.source).unwrap();
    let plan = plan_windows_authenticode_bundle(&fixture.source, &fixture.output, fixture.options).unwrap();
    let mut runner = FakeRunner {
      fail: Some(phase.into()),
      ..Default::default()
    };
    let error = execute_with(&plan, &mut runner, false).unwrap_err();
    assert!(format!("{error:#}").contains(phase));
    assert!(!plan.destination.exists());
    assert_eq!(crate::validate_windows_bundle(&fixture.source).unwrap(), before);
  }
}
#[test]
fn snapshot_tool_or_payload_change_and_destination_collision_are_rejected_before_signing() {
  let fixture = make_fixture();
  let plan = plan_windows_authenticode_bundle(&fixture.source, &fixture.output, fixture.options.clone()).unwrap();
  fs::write(&plan.options.signtool, b"replaced SDK tool").unwrap();
  let mut runner = FakeRunner::default();
  assert!(execute_with(&plan, &mut runner, false).is_err());
  assert!(runner.trace.is_empty());
  let fixture = make_fixture();
  let plan = plan_windows_authenticode_bundle(&fixture.source, &fixture.output, fixture.options.clone()).unwrap();
  fs::write(fixture.source.join("Fixture.exe"), b"changed input").unwrap();
  assert!(execute_with(&plan, &mut FakeRunner::default(), false).is_err());
  let fixture = make_fixture();
  let plan = plan_windows_authenticode_bundle(&fixture.source, &fixture.output, fixture.options).unwrap();
  fs::create_dir(&plan.destination).unwrap();
  fs::write(plan.destination.join("user.txt"), b"keep").unwrap();
  assert!(execute_with(&plan, &mut FakeRunner::default(), false).is_err());
  assert_eq!(fs::read(plan.destination.join("user.txt")).unwrap(), b"keep");
}
#[test]
fn code_injection_or_manifest_changes_by_tool_are_not_rehashed_into_a_valid_package() {
  for (mutate_code, mutate_manifest) in [(true, false), (false, true)] {
    let fixture = make_fixture();
    let plan = plan_windows_authenticode_bundle(&fixture.source, &fixture.output, fixture.options).unwrap();
    assert!(
      execute_with(
        &plan,
        &mut FakeRunner {
          mutate_code,
          mutate_manifest,
          ..Default::default()
        },
        false
      )
      .is_err()
    );
    assert!(!plan.destination.exists());
  }
}
#[test]
fn standalone_signed_tools_publish_without_overwrite_and_remain_machine_checked() {
  let fixture = make_fixture();
  let input = fixture.root.join("signtool.exe");
  let plan = plan_windows_authenticode_file(
    &input,
    &fixture.output,
    "x86_64-pc-windows-msvc",
    fixture.options.clone(),
  )
  .unwrap();
  let receipt = execute_with(&plan, &mut FakeRunner::default(), false).unwrap();
  assert!(receipt.bundle.is_none());
  assert!(crate::validate_windows_tool(&receipt.destination, "x86_64-pc-windows-msvc").is_ok());
  assert!(plan_windows_authenticode_file(&input, &fixture.output, "i686-pc-windows-msvc", fixture.options).is_err());
}
#[cfg(unix)]
#[test]
fn rejects_linked_tool_and_parent_before_plan_has_any_output_effect() {
  use std::os::unix::fs::symlink;
  let fixture = make_fixture();
  let link = fixture.root.join("linked.exe");
  symlink(&fixture.options.signtool, &link).unwrap();
  let options = AuthenticodeOptions::new(link, "a".repeat(40), "https://tsa.example.test").unwrap();
  assert!(plan_windows_authenticode_bundle(&fixture.source, &fixture.output, options).is_err());
  let linked_parent = fixture.root.join("linked-parent");
  symlink(&fixture.output, &linked_parent).unwrap();
  assert!(plan_windows_authenticode_bundle(&fixture.source, &linked_parent, fixture.options).is_err());
  assert!(fs::read_dir(&fixture.output).unwrap().next().is_none());
}

fn two_file_bundle(fixture: &Fixture) -> PathBuf {
  let helper = fixture.root.join("ZHelper.exe");
  pe(&helper, Architecture::X64, false, &[]);
  crate::bundle_windows_with_helpers(
    &crate::BundleOptions {
      executable: fixture.root.join("source.exe"),
      name: "Fixture".into(),
      identifier: "dev.lintel.fixture".into(),
      version: "1.0.0".into(),
      output_dir: fixture.root.join("two-file-bundles"),
      cef_dir: None,
      helper: None,
      dawn_root: None,
      updater_helper: None,
    },
    "x86_64-pc-windows-msvc",
    &[],
    &[crate::BundleHelper {
      name: "ZHelper".into(),
      executable: helper,
    }],
  )
  .unwrap()
}

#[test]
fn later_tool_phase_cannot_change_code_or_certificates_of_an_earlier_file() {
  struct CrossFileRunner {
    normal: FakeRunner,
    phase: &'static str,
    certificate: bool,
    corrupted: bool,
  }
  impl Runner for CrossFileRunner {
    fn run(&mut self, tool: &Path, args: &[String], phase: &str, timeout: Duration) -> Result<()> {
      self.normal.run(tool, args, phase, timeout)?;
      let target = Path::new(args.last().unwrap());
      if phase == self.phase && target.file_name().unwrap() == "ZHelper.exe" {
        let earlier = target.parent().unwrap().join("Fixture.exe");
        let mut bytes = fs::read(&earlier)?;
        let index = if self.certificate { bytes.len() - 1 } else { 0x800 };
        bytes[index] ^= 1;
        fs::write(earlier, bytes)?;
        self.corrupted = true;
      }
      Ok(())
    }
  }
  for phase in ["sign", "verify"] {
    for certificate in [false, true] {
      let fixture = make_fixture();
      let source = two_file_bundle(&fixture);
      let before = crate::validate_windows_bundle(&source).unwrap();
      let plan = plan_windows_authenticode_bundle(&source, &fixture.output, fixture.options).unwrap();
      let mut runner = CrossFileRunner {
        normal: FakeRunner::default(),
        phase,
        certificate,
        corrupted: false,
      };
      let error = execute_with(&plan, &mut runner, false).unwrap_err();
      assert!(runner.corrupted && format!("{error:#}").contains("unexpected input Fixture.exe"));
      assert!(!plan.destination.exists());
      assert_eq!(crate::validate_windows_bundle(&source).unwrap(), before);
    }
  }
}

#[test]
fn all_file_signing_and_timestamp_writes_precede_the_final_full_verification_pass() {
  let fixture = make_fixture();
  let source = two_file_bundle(&fixture);
  let plan = plan_windows_authenticode_bundle(&source, &fixture.output, fixture.options).unwrap();
  let mut runner = FakeRunner::default();
  let receipt = execute_with(&plan, &mut runner, false).unwrap();
  assert_eq!(
    runner.trace,
    ["sign", "timestamp", "sign", "timestamp", "verify", "verify"]
  );
  assert_eq!(receipt.completed_phases, 6);
  assert_eq!(receipt.files.len(), 2);
  assert!(crate::validate_windows_bundle(&receipt.destination).is_ok());
}

#[test]
fn trusted_os_principal_policy_recognizes_only_the_exact_trustedinstaller_service_sid() {
  let exact = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
  assert!(trusted_system_principal(exact));
  assert!(trusted_system_principal("S-1-5-18") && trusted_system_principal("S-1-5-32-544"));
  for untrusted in [
    "NT SERVICE\\TrustedInstaller",
    "TrustedInstaller",
    "S-1-5-80",
    "S-1-5-80-1234-5678",
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478465",
  ] {
    assert!(!trusted_system_principal(untrusted), "{untrusted}");
  }
}
