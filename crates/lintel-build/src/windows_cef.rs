//! Read-only CEF 143 (branch 7499) layout audit. It never enables unreviewed Windows bootstrap.

use crate::windows::{
  Architecture, archive_architecture, inspect_pe, pe_code_exports, real_directory, regular_file, system_dll,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{collections::BTreeSet, fs, path::Path};

mod archives;
pub use archives::WindowsCefArchiveVerification;

#[derive(Debug, Serialize)]
pub struct WindowsCefAssessment {
  pub target: String,
  pub layout_valid: bool,
  pub checked_files: Vec<String>,
  pub missing_files: Vec<String>,
  pub invalid_files: Vec<String>,
  pub sandbox_archive_machine_verified: bool,
  pub sdk_version_metadata_matches: bool,
  pub sdk_import_library_machine_verified: bool,
  pub client_entry_export_verified: bool,
  pub bootstrap_matches_sdk: bool,
  pub bootstrap_inputs_valid: bool,
  pub sdk_archive_verified: bool,
  /// All original minimal SDK files match embedded byte pins. Additional local
  /// files (including repack bookkeeping) are not authenticated by this flag.
  pub sdk_files_match_archive: bool,
  /// The application EXE matches the byte-verified official GUI bootstrap.
  /// This is provenance only; it is not a native ABI, Authenticode or sandbox result.
  pub bootstrap_archive_provenance_verified: bool,
  pub archive_verification: Option<WindowsCefArchiveVerification>,
  pub bootstrap_verified: bool,
  pub native_execution_verified: bool,
  pub packaging_enabled: bool,
  pub blockers: Vec<String>,
}

impl WindowsCefAssessment {
  pub fn to_json(&self) -> Result<String> {
    Ok(serde_json::to_string_pretty(self)?)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::windows::tests::{archive, pe};

  #[test]
  fn complete_matching_layout_remains_explicitly_unverified_for_bootstrap_and_execution() {
    let root = tempfile::tempdir().unwrap();
    let cef = root.path().join("cef");
    for name in [
      "libcef.dll",
      "chrome_elf.dll",
      "d3dcompiler_47.dll",
      "libEGL.dll",
      "libGLESv2.dll",
      "vk_swiftshader.dll",
      "vulkan-1.dll",
      "dxil.dll",
      "dxcompiler.dll",
    ] {
      pe(&cef.join("Release").join(name), Architecture::X64, true, &[]);
    }
    for name in ["v8_context_snapshot.bin", "vk_swiftshader_icd.json"] {
      fs::write(cef.join("Release").join(name), b"layout fixture only").unwrap();
    }
    fs::create_dir_all(cef.join("Resources/locales")).unwrap();
    for name in [
      "icudtl.dat",
      "chrome_100_percent.pak",
      "chrome_200_percent.pak",
      "resources.pak",
      "locales/en-US.pak",
    ] {
      fs::write(cef.join("Resources").join(name), b"layout fixture only").unwrap();
    }
    archive(&cef.join("Release/cef_sandbox.lib"), Architecture::X64);
    let main = root.path().join("app.exe");
    let helper = root.path().join("helper.exe");
    pe(&main, Architecture::X64, false, &[]);
    pe(&helper, Architecture::X64, false, &[]);
    let assessment = inspect_windows_cef_layout(&cef, &main, Some(&helper), "x86_64-pc-windows-msvc").unwrap();
    assert!(assessment.layout_valid && assessment.sandbox_archive_machine_verified);
    assert!(!assessment.bootstrap_verified && !assessment.native_execution_verified && !assessment.packaging_enabled);
    fs::remove_file(cef.join("Resources/icudtl.dat")).unwrap();
    pe(&helper, Architecture::X86, false, &[]);
    let invalid = inspect_windows_cef_layout(&cef, &main, Some(&helper), "x86_64-pc-windows-msvc").unwrap();
    assert!(!invalid.layout_valid);
    assert!(invalid.missing_files.contains(&"icudtl.dat".into()));
    assert!(invalid.invalid_files.iter().any(|error| error.starts_with("helper:")));
  }
}

/// Audit PE machines/roles and a complete conventional runtime/resource layout on any host.
/// Presence of cef_sandbox.lib does not prove it is linked or initialized by the executable.
pub fn inspect_windows_cef_layout(
  cef: &Path,
  executable: &Path,
  helper: Option<&Path>,
  target: &str,
) -> Result<WindowsCefAssessment> {
  inspect_windows_cef_layout_inner(cef, executable, helper, target, None)
}

/// In addition to the layout audit, verifies actual compressed archive bytes and
/// the extracted SDK against fixed SHA256 pins for the official CEF 143 minimal
/// releases. archive.json alone is never accepted as provenance. This is an
/// offline, read-only snapshot and never enables packaging or native execution.
pub fn inspect_windows_cef_layout_with_archive(
  cef: &Path,
  executable: &Path,
  helper: Option<&Path>,
  target: &str,
  archive: &Path,
) -> Result<WindowsCefAssessment> {
  inspect_windows_cef_layout_inner(cef, executable, helper, target, Some(archive))
}

fn inspect_windows_cef_layout_inner(
  cef: &Path,
  executable: &Path,
  helper: Option<&Path>,
  target: &str,
  archive: Option<&Path>,
) -> Result<WindowsCefAssessment> {
  let architecture = Architecture::target(target)?;
  let binary = if cef.join("Release").is_dir() {
    cef.join("Release")
  } else {
    cef.to_owned()
  };
  let resources = if cef.join("Resources").is_dir() {
    cef.join("Resources")
  } else {
    cef.to_owned()
  };
  let mut report = WindowsCefAssessment {
    target: target.into(),
    layout_valid: false,
    checked_files: vec![],
    missing_files: vec![],
    invalid_files: vec![],
    sandbox_archive_machine_verified: false,
    sdk_version_metadata_matches: false,
    sdk_import_library_machine_verified: false,
    client_entry_export_verified: false,
    bootstrap_matches_sdk: false,
    bootstrap_inputs_valid: false,
    sdk_archive_verified: false,
    sdk_files_match_archive: false,
    bootstrap_archive_provenance_verified: false,
    archive_verification: None,
    bootstrap_verified: false,
    native_execution_verified: false,
    packaging_enabled: false,
    blockers: vec![
      "Windows CEF 143 sandbox/bootstrap entrypoints and libcef delayed loading are not proven by file layout".into(),
      "Native Windows browser/renderer/GPU and sandbox execution have not been validated".into(),
    ],
  };
  if let Some(archive) = archive {
    let verification = archives::verify(cef, archive, target)?;
    report.sdk_archive_verified = verification.archive_bytes_verified;
    report.sdk_files_match_archive = verification.sdk_files_verified;
    if !verification.archive_bytes_verified || !verification.sdk_files_verified {
      report.blockers.push(format!(
        "official archive/SDK byte verification failed ({} diagnostics): {}",
        verification.invalid_file_count,
        verification.invalid_files.join("; ")
      ));
    }
    report.archive_verification = Some(verification);
  }
  let mut names = BTreeSet::new();
  let mut imports = BTreeSet::new();
  let client = executable.with_extension("dll");
  match regular_file(&client).and_then(|path| pe_code_exports(&path, architecture)) {
    Ok(exports) if exports.contains("RunWinMain") => {
      report.client_entry_export_verified = true;
      report
        .checked_files
        .push("same-basename client DLL with undecorated RunWinMain code export".into());
      imports.extend(inspect_pe(&client, architecture, true)?);
    }
    Ok(_) => report
      .blockers
      .push("client DLL lacks exact undecorated executable RunWinMain export".into()),
    Err(error) => report.blockers.push(format!("same-basename client DLL: {error:#}")),
  }
  let bootstrap = binary.join("bootstrap.exe");
  match regular_file(&bootstrap).and_then(|path| {
    inspect_pe(&path, architecture, false)?;
    Ok(fs::read(path)?)
  }) {
    Ok(bytes) => {
      report.bootstrap_matches_sdk = regular_file(executable)
        .and_then(|path| Ok(fs::read(path)?))
        .is_ok_and(|actual| actual == bytes);
    }
    Err(error) => report.blockers.push(format!("SDK GUI bootstrap: {error:#}")),
  }
  if !report.bootstrap_matches_sdk {
    report
      .blockers
      .push("application EXE does not byte-match the supplied SDK bootstrap.exe".into());
  }
  report.bootstrap_archive_provenance_verified = report.bootstrap_matches_sdk
    && report
      .archive_verification
      .as_ref()
      .is_some_and(|proof| proof.archive_bytes_verified && proof.bootstrap_file_verified);
  match regular_file(&binary.join("libcef.lib")).and_then(|path| archive_architecture(&path)) {
    Ok(machine) if machine == architecture => report.sdk_import_library_machine_verified = true,
    _ => report
      .blockers
      .push("matching-machine libcef.lib import library is missing/invalid".into()),
  }
  let metadata = if report.sdk_files_match_archive && !cef.join("archive.json").exists() {
    // Official raw archives do not contain archive.json. A byte-authenticated
    // SDK is valid without cef-rs download/repack bookkeeping.
    sdk_headers(cef)
  } else {
    sdk_metadata(cef, target)
  };
  match metadata {
    Ok(()) => report.sdk_version_metadata_matches = true,
    Err(error) => report
      .blockers
      .push(format!("SDK pinned metadata/header contract: {error:#}")),
  }
  if helper.is_some() {
    report.blockers.push("separate CEF subprocess helper is not the public Windows bootstrap contract; leave browser_subprocess_path empty".into());
  }
  report.bootstrap_inputs_valid = report.client_entry_export_verified
    && report.bootstrap_matches_sdk
    && report.sdk_version_metadata_matches
    && report.sdk_import_library_machine_verified
    && (archive.is_none() || (report.sdk_archive_verified && report.sdk_files_match_archive))
    && helper.is_none();
  for (label, path) in std::iter::once(("main", executable)).chain(helper.map(|path| ("helper", path))) {
    match regular_file(path).and_then(|path| inspect_pe(&path, architecture, false)) {
      Ok(actual) => {
        imports.extend(actual);
        report.checked_files.push(label.into());
      }
      Err(error) => report.invalid_files.push(format!("{label}: {error:#}")),
    }
  }
  if !real_directory(&binary).unwrap_or(false) || !real_directory(&resources).unwrap_or(false) {
    report
      .invalid_files
      .push("runtime/resource directory is missing or is a link/reparse point".into());
    return Ok(report);
  }
  for file in [
    "libcef.dll",
    "chrome_elf.dll",
    "d3dcompiler_47.dll",
    "libEGL.dll",
    "libGLESv2.dll",
    "vk_swiftshader.dll",
    "vulkan-1.dll",
  ] {
    let path = binary.join(file);
    if !path.exists() {
      report.missing_files.push(file.into());
      continue;
    }
    match regular_file(&path).and_then(|path| inspect_pe(&path, architecture, true)) {
      Ok(actual) => {
        names.insert(file.to_ascii_lowercase());
        imports.extend(actual);
        report.checked_files.push(file.into());
      }
      Err(error) => report.invalid_files.push(format!("{file}: {error:#}")),
    }
  }
  if target.starts_with("x86_64-") || target.starts_with("aarch64-") {
    for file in ["dxil.dll", "dxcompiler.dll"] {
      let path = binary.join(file);
      match regular_file(&path).and_then(|path| inspect_pe(&path, architecture, true)) {
        Ok(actual) => {
          names.insert(file.into());
          imports.extend(actual);
          report.checked_files.push(file.into());
        }
        Err(error) => report.invalid_files.push(format!("{file}: {error:#}")),
      }
    }
  }
  for (base, file) in [
    (&binary, "v8_context_snapshot.bin"),
    (&binary, "vk_swiftshader_icd.json"),
    (&resources, "icudtl.dat"),
    (&resources, "chrome_100_percent.pak"),
    (&resources, "chrome_200_percent.pak"),
    (&resources, "resources.pak"),
  ] {
    let path = base.join(file);
    match regular_file(&path).and_then(|path| Ok(fs::metadata(path)?.len())) {
      Ok(size) if size > 0 => report.checked_files.push(file.into()),
      _ => report.missing_files.push(file.into()),
    }
  }
  let locales = resources.join("locales");
  if real_directory(&locales).unwrap_or(false) && regular_file(&locales.join("en-US.pak")).is_ok() {
    report.checked_files.push("locales/en-US.pak".into());
  } else {
    report.missing_files.push("locales/en-US.pak".into());
  }
  let sandbox = binary.join("cef_sandbox.lib");
  match regular_file(&sandbox).and_then(|path| archive_architecture(&path)) {
    Ok(machine) if machine == architecture => {
      report.sandbox_archive_machine_verified = true;
      report.checked_files.push("cef_sandbox.lib".into());
    }
    Ok(_) => report
      .invalid_files
      .push("cef_sandbox.lib has a different machine".into()),
    // M138+ official minimal archives use the prebuilt GUI bootstrap rather
    // than shipping cef_sandbox.lib. Its absence is not a missing bootstrap input.
    Err(error) if sandbox.exists() => report.blockers.push(format!(
      "supplied legacy sandbox archive cannot be validated: {error:#}"
    )),
    Err(_) => {}
  }
  for dependency in imports {
    if !names.contains(&dependency) && !system_dll(&dependency) {
      report
        .invalid_files
        .push(format!("unresolved runtime import: {dependency}"));
    }
  }
  report.layout_valid = report.missing_files.is_empty() && report.invalid_files.is_empty();
  report.bootstrap_inputs_valid &= report.layout_valid;
  Ok(report)
}

fn sdk_metadata(cef: &Path, target: &str) -> Result<()> {
  let platform = match target {
    "i686-pc-windows-msvc" => "windows32",
    "x86_64-pc-windows-msvc" => "windows64",
    "aarch64-pc-windows-msvc" => "windowsarm64",
    _ => unreachable!(),
  };
  let record: serde_json::Value = serde_json::from_str(&read_metadata(&cef.join("archive.json"))?)?;
  let name = record["name"].as_str().context("archive.json has no name")?;
  let kind = record["type"].as_str().context("archive.json has no type")?;
  ensure!(["minimal", "standard"].contains(&kind), "unsupported SDK archive type");
  let expected_name = if kind == "standard" {
    format!("cef_binary_143.0.14+gdd46a37+chromium-143.0.7499.193_{platform}.tar.bz2")
  } else {
    format!("cef_binary_143.0.14+gdd46a37+chromium-143.0.7499.193_{platform}_{kind}.tar.bz2")
  };
  ensure!(
    name == expected_name,
    "SDK archive name/version/platform does not match pinned CEF"
  );
  let sha = record["sha1"].as_str().context("archive.json has no SHA1")?;
  ensure!(
    sha.len() == 40 && sha.bytes().all(|value| value.is_ascii_hexdigit()),
    "invalid SDK archive SHA1 record (not independently authenticated)"
  );
  // A copied official SHA1 record is still not a proof of compressed bytes.
  // Requiring the published value only checks this metadata contract.
  let official_sha1 = match (platform, kind) {
    ("windows32", "minimal") => "6a36fce4a63c129c8d9ec8325c7caf5fc38218e9",
    ("windows32", "standard") => "bcd3669064f70886389cbadff6f30dc697960911",
    ("windows64", "minimal") => "f75c50c0710a2fc07b13941bab8a02f7b6bc0442",
    ("windows64", "standard") => "4912e99b3522500d38f075150095a7a786c35f3b",
    ("windowsarm64", "minimal") => "d7c8108cccd33019f514852497c1a9408e8ef347",
    ("windowsarm64", "standard") => "0bd63deae3a7f80c11639ace994fc3ab11706764",
    _ => unreachable!(),
  };
  ensure!(
    sha.eq_ignore_ascii_case(official_sha1),
    "SDK metadata SHA1 differs from the published official record"
  );
  sdk_headers(cef)
}

fn read_metadata(path: &Path) -> Result<String> {
  use std::io::Read as _;
  const LIMIT: u64 = 1024 * 1024;
  let path = regular_file(path)?;
  let file = fs::File::open(path)?;
  ensure!(file.metadata()?.len() <= LIMIT, "SDK metadata exceeds 1 MiB");
  let mut value = String::new();
  file.take(LIMIT + 1).read_to_string(&mut value)?;
  ensure!(
    value.len() as u64 <= LIMIT,
    "SDK metadata grew beyond 1 MiB while reading"
  );
  Ok(value)
}

fn sdk_headers(cef: &Path) -> Result<()> {
  let header = read_metadata(&cef.join("include/cef_version.h"))?;
  for (key, value) in [
    ("CEF_VERSION_MAJOR", "143"),
    ("CEF_VERSION_MINOR", "0"),
    ("CEF_VERSION_PATCH", "14"),
    ("CEF_COMMIT_NUMBER", "3340"),
    ("CEF_COMMIT_HASH", "\"dd46a3761b5389255e1824c81ae1b131459f7cd0\""),
    ("CHROME_VERSION_MAJOR", "143"),
    ("CHROME_VERSION_MINOR", "0"),
    ("CHROME_VERSION_BUILD", "7499"),
    ("CHROME_VERSION_PATCH", "193"),
  ] {
    let matches: Vec<_> = header
      .lines()
      .map(str::split_whitespace)
      .filter_map(|mut words| {
        (words.next() == Some("#define") && words.next() == Some(key)).then(|| words.collect::<Vec<_>>())
      })
      .collect();
    ensure!(
      matches.len() == 1 && matches[0] == [value],
      "SDK version define {key} differs or is ambiguous"
    );
  }
  read_metadata(&cef.join("include/cef_sandbox_win.h"))?;
  read_metadata(&cef.join("include/cef_version_info.h"))?;
  Ok(())
}

#[cfg(test)]
mod bootstrap_tests {
  use super::*;
  use crate::windows::tests::{archive, pe};
  fn exported(path: &Path, name: &str, forwarded: bool) {
    pe(path, Architecture::X64, true, &[]);
    let mut bytes = fs::read(path).unwrap();
    let optional = 0x98;
    let section = optional + 240;
    bytes[optional + 112..optional + 116].copy_from_slice(&0x1000_u32.to_le_bytes());
    bytes[optional + 116..optional + 120].copy_from_slice(&0x80_u32.to_le_bytes());
    bytes[section + 36..section + 40].copy_from_slice(&0x20000000_u32.to_le_bytes());
    for (offset, value) in [(20, 1_u32), (24, 1), (28, 0x1040), (32, 0x1044), (36, 0x1048)] {
      bytes[0x200 + offset..0x204 + offset].copy_from_slice(&value.to_le_bytes());
    }
    bytes[0x240..0x244].copy_from_slice(&(if forwarded { 0x1060_u32 } else { 0x1180_u32 }).to_le_bytes());
    bytes[0x244..0x248].copy_from_slice(&0x1060_u32.to_le_bytes());
    bytes[0x248..0x24a].copy_from_slice(&0_u16.to_le_bytes());
    bytes[0x260..0x260 + name.len()].copy_from_slice(name.as_bytes());
    bytes[0x260 + name.len()] = 0;
    fs::write(path, bytes).unwrap();
  }
  #[test]
  fn only_exact_undecorated_code_export_satisfies_public_gui_entry_contract() {
    let root = tempfile::tempdir().unwrap();
    let client = root.path().join("Application.dll");
    exported(&client, "RunWinMain", false);
    assert!(
      pe_code_exports(&client, Architecture::X64)
        .unwrap()
        .contains("RunWinMain")
    );
    exported(&client, "_RunWinMain@20", false);
    assert!(
      !pe_code_exports(&client, Architecture::X64)
        .unwrap()
        .contains("RunWinMain")
    );
    exported(&client, "RunWinMain", true);
    assert!(
      !pe_code_exports(&client, Architecture::X64)
        .unwrap()
        .contains("RunWinMain")
    );
    exported(&client, "RunWinMain", false);
    let mut bytes = fs::read(&client).unwrap();
    bytes[0x244..0x248].copy_from_slice(&0xfffffff0_u32.to_le_bytes());
    fs::write(&client, bytes).unwrap();
    assert!(pe_code_exports(&client, Architecture::X64).is_err());
  }
  #[test]
  fn sdk_records_check_exact_pinned_version_platform_and_deny_ambiguous_defines() {
    let root = tempfile::tempdir().unwrap();
    let cef = root.path();
    fs::create_dir(cef.join("include")).unwrap();
    fs::write(cef.join("archive.json"),serde_json::to_vec(&serde_json::json!({"name":"cef_binary_143.0.14+gdd46a37+chromium-143.0.7499.193_windows64_minimal.tar.bz2","type":"minimal","sha1":"f75c50c0710a2fc07b13941bab8a02f7b6bc0442"})).unwrap()).unwrap();
    let header = "#define CEF_VERSION_MAJOR 143\n#define CEF_VERSION_MINOR 0\n#define CEF_VERSION_PATCH 14\n#define CEF_COMMIT_NUMBER 3340\n#define CEF_COMMIT_HASH \"dd46a3761b5389255e1824c81ae1b131459f7cd0\"\n#define CHROME_VERSION_MAJOR 143\n#define CHROME_VERSION_MINOR 0\n#define CHROME_VERSION_BUILD 7499\n#define CHROME_VERSION_PATCH 193\n";
    fs::write(cef.join("include/cef_version.h"), header).unwrap();
    for name in ["cef_sandbox_win.h", "cef_version_info.h"] {
      fs::write(cef.join("include").join(name), b"fixture, not ABI proof").unwrap();
    }
    assert!(sdk_metadata(cef, "x86_64-pc-windows-msvc").is_ok());
    assert!(sdk_metadata(cef, "i686-pc-windows-msvc").is_err());
    let record = fs::read(cef.join("archive.json")).unwrap();
    fs::write(
      cef.join("archive.json"),
      serde_json::to_vec(&serde_json::json!({
        "name":"cef_binary_143.0.14+gdd46a37+chromium-143.0.7499.193_windows64.tar.bz2",
        "type":"standard", "sha1":"4912e99b3522500d38f075150095a7a786c35f3b"
      }))
      .unwrap(),
    )
    .unwrap();
    assert!(sdk_metadata(cef, "x86_64-pc-windows-msvc").is_ok());
    fs::write(
      cef.join("archive.json"),
      serde_json::to_vec(&serde_json::json!({
        "name":"cef_binary_143.0.14+gdd46a37+chromium-143.0.7499.193_windows64_minimal.tar.bz2",
        "type":"minimal", "sha1":"a".repeat(40)
      }))
      .unwrap(),
    )
    .unwrap();
    assert!(sdk_metadata(cef, "x86_64-pc-windows-msvc").is_err());
    fs::write(cef.join("archive.json"), record).unwrap();
    fs::write(
      cef.join("include/cef_version.h"),
      format!("{header}#define CEF_VERSION_MAJOR 143\n"),
    )
    .unwrap();
    assert!(sdk_metadata(cef, "x86_64-pc-windows-msvc").is_err());
    // A supplied import archive proves only its machine; no fake fixture enables packaging.
    archive(&cef.join("libcef.lib"), Architecture::X64);
    let executable = cef.join("Application.exe");
    pe(&executable, Architecture::X64, false, &[]);
    let report = inspect_windows_cef_layout(cef, &executable, None, "x86_64-pc-windows-msvc").unwrap();
    assert!(!report.packaging_enabled && !report.bootstrap_verified && !report.sdk_archive_verified);
  }

  #[test]
  #[ignore = "requires the real official downloads produced by build/checklist-platform/verify_cef143_archives.py"]
  fn real_official_archives_and_all_sdk_inputs_verify_without_windows_execution() {
    let evidence = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/checklist-platform");
    let mut failures = vec![];
    for (platform, target, files) in [
      ("windows32", "i686-pc-windows-msvc", 962),
      ("windows64", "x86_64-pc-windows-msvc", 964),
      ("windowsarm64", "aarch64-pc-windows-msvc", 964),
    ] {
      let cef = evidence.join("cef143-official-sdk").join(platform);
      let archive = evidence.join(format!(
        "cef_binary_143.0.14+gdd46a37+chromium-143.0.7499.193_{platform}_minimal.tar.bz2"
      ));
      let report =
        inspect_windows_cef_layout_with_archive(&cef, &cef.join("Release/bootstrap.exe"), None, target, &archive)
          .unwrap();
      fs::write(
        evidence.join(format!("cef143-{platform}-assessment.json")),
        report.to_json().unwrap(),
      )
      .unwrap();
      assert!(
        report.sdk_archive_verified && report.sdk_files_match_archive,
        "{platform}: {:?}",
        report.blockers
      );
      assert!(
        report.bootstrap_archive_provenance_verified,
        "{platform}: {:?}",
        report.blockers
      );
      assert_eq!(report.archive_verification.as_ref().unwrap().checked_sdk_files, files);
      if !report.layout_valid {
        failures.push(format!("{platform}: {:?}", report.invalid_files));
      }
      assert!(report.sdk_version_metadata_matches && report.sdk_import_library_machine_verified);
      // The SDK bootstrap is not an application client; no matching bootstrap.dll
      // has been linked in this experiment, and no PE is run on the Mac host.
      assert!(!report.client_entry_export_verified);
      assert!(!report.bootstrap_inputs_valid && !report.bootstrap_verified);
      assert!(!report.native_execution_verified && !report.packaging_enabled);
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
  }
}
