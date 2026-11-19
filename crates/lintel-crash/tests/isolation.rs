use std::{
  fs,
  path::{Path, PathBuf},
  process::{Command, ExitStatus, Stdio},
  thread,
  time::{Duration, Instant},
};

fn run_probe(root: &Path, helper: &Path, mode: &str) -> (ExitStatus, String) {
  let mut child = Command::new(env!("CARGO_BIN_EXE_lintel-crash-probe"))
    .arg(root)
    .arg(helper)
    .arg(mode)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
  let deadline = Instant::now() + Duration::from_secs(15);
  let status = loop {
    if let Some(status) = child.try_wait().unwrap() {
      break status;
    }
    if Instant::now() >= deadline {
      let _ = child.kill();
      let _ = child.wait();
      panic!("isolated crash probe exceeded its 15 second deadline");
    }
    thread::sleep(Duration::from_millis(20));
  };
  let output = child.wait_with_output().unwrap();
  (
    status,
    format!(
      "{}{}",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    ),
  )
}

fn reports(root: &Path) -> Vec<PathBuf> {
  fs::read_dir(root)
    .unwrap()
    .map(|entry| entry.unwrap().path())
    .filter(|path| path.is_dir())
    .collect()
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
  u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

#[test]
fn isolated_abort_produces_a_structurally_valid_native_exception_dump() {
  let root = tempfile::tempdir().unwrap();
  let helper = Path::new(env!("CARGO_BIN_EXE_lintel-crash-helper"));
  let (status, output) = run_probe(root.path(), helper, "--abort");
  assert!(
    !status.success(),
    "the isolated child must terminate after abort: {output}"
  );
  let reports = reports(root.path());
  assert_eq!(reports.len(), 1, "exactly one app instance must own the dump: {output}");
  let report = &reports[0];
  assert!(
    report.join("completed").is_file(),
    "helper did not finish a dump: {output}; files={:?}",
    fs::read_dir(report)
      .unwrap()
      .map(|entry| entry.unwrap().file_name())
      .collect::<Vec<_>>()
  );
  assert!(!report.join("failure.txt").exists());
  let bytes = fs::read(report.join("report.dmp")).unwrap();
  assert!(bytes.len() > 4096, "dump is too short to include native context");
  assert_eq!(&bytes[..4], b"MDMP");
  assert_eq!(u32_at(&bytes, 4) & 0xffff, 0xa793);
  let count = u32_at(&bytes, 8) as usize;
  let directory = u32_at(&bytes, 12) as usize;
  assert!((1..=1024).contains(&count));
  assert!(directory.checked_add(count * 12).is_some_and(|end| end <= bytes.len()));
  let mut streams = Vec::new();
  for index in 0..count {
    let offset = directory + index * 12;
    let kind = u32_at(&bytes, offset);
    let length = u32_at(&bytes, offset + 4) as usize;
    let rva = u32_at(&bytes, offset + 8) as usize;
    assert!(
      rva.checked_add(length).is_some_and(|end| end <= bytes.len()),
      "stream {kind} extends beyond the dump"
    );
    if kind == 6 {
      assert!(length >= 32, "native exception stream is missing its context");
      assert_ne!(u32_at(&bytes, rva + 8), 0, "native exception code must be present");
    }
    streams.push(kind);
  }
  for required in [3, 4, 6, 7] {
    assert!(streams.contains(&required), "missing minidump stream {required}");
  }
  let metadata: serde_json::Value = serde_json::from_slice(&fs::read(report.join("report.json")).unwrap()).unwrap();
  assert_eq!(metadata["format"], "lintel-native-crash-v1");
  assert_ne!(
    metadata["process_id"],
    std::process::id(),
    "the test host must never be the crashing process"
  );
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(report).unwrap().permissions().mode() & 0o077, 0);
    assert_eq!(
      fs::metadata(report.join("report.dmp")).unwrap().permissions().mode() & 0o077,
      0
    );
  }
}

#[test]
fn normal_guard_shutdown_reaps_helper_and_removes_empty_run_directory() {
  let root = tempfile::tempdir().unwrap();
  let (status, output) = run_probe(
    root.path(),
    Path::new(env!("CARGO_BIN_EXE_lintel-crash-helper")),
    "--clean",
  );
  assert!(status.success(), "normal helper shutdown failed: {output}");
  assert!(
    reports(root.path()).is_empty(),
    "normal startup must not retain empty reports"
  );
}

#[test]
fn helper_startup_failure_returns_without_crashing_or_leaking_run_directories() {
  let root = tempfile::tempdir().unwrap();
  // The test runner rejects the private helper arguments and exits; no crash hook
  // can be installed until authentication succeeds, so this remains a startup failure.
  let (status, output) = run_probe(root.path(), &std::env::current_exe().unwrap(), "--clean");
  assert!(!status.success());
  assert!(output.contains("crash helper"), "unexpected startup error: {output}");
  assert!(reports(root.path()).is_empty());
}
