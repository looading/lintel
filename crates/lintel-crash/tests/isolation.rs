use std::{
  fs,
  path::{Path, PathBuf},
  process::{Command, ExitStatus, Stdio},
  thread,
  time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn run_probe(root: &Path, helper: &Path, mode: &str) -> (ExitStatus, String) {
  run_probe_with_snapshot(root, helper, mode, None)
}

fn run_probe_with_snapshot(root: &Path, helper: &Path, mode: &str, snapshot: Option<&Path>) -> (ExitStatus, String) {
  let mut child = Command::new(env!("CARGO_BIN_EXE_lintel-crash-probe"))
    .arg(root)
    .arg(helper)
    .arg(mode)
    .args(snapshot)
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

fn now_unix_ms() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap()
    .as_millis()
    .try_into()
    .unwrap()
}

#[test]
fn isolated_abort_produces_a_structurally_valid_native_exception_dump() {
  let root = tempfile::tempdir().unwrap();
  let helper = Path::new(env!("CARGO_BIN_EXE_lintel-crash-helper"));
  let started_at = now_unix_ms();
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
  assert_eq!(fs::read(report.join("completed")).unwrap(), b"lintel-native-crash-v1\n");
  let bytes = fs::read(report.join("report.dmp")).unwrap();
  let streams = stream_types(&bytes);
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
  assert!(metadata.get("captured_at_unix_ms").is_none());
  let capture: serde_json::Value = serde_json::from_slice(&fs::read(report.join("capture.json")).unwrap()).unwrap();
  let captured_at = capture["captured_at_unix_ms"].as_u64().unwrap();
  assert!((started_at..=now_unix_ms()).contains(&captured_at));
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    for path in [
      report.to_path_buf(),
      report.join("report.dmp"),
      report.join("capture.json"),
    ] {
      assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    }
  }
}

fn stream_types(bytes: &[u8]) -> Vec<u32> {
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
  streams
}

#[test]
fn optional_capture_time_failure_leaves_the_fatal_dump_complete() {
  let root = tempfile::tempdir().unwrap();
  let helper = Path::new(env!("CARGO_BIN_EXE_lintel-crash-helper"));
  let (status, output) = run_probe(root.path(), helper, "--abort-sidecar-failure");
  assert!(!status.success());
  let reports = reports(root.path());
  assert_eq!(reports.len(), 1);
  let report = &reports[0];
  assert!(report.join("capture.json").is_dir());
  assert_eq!(
    fs::read(report.join("completed")).unwrap(),
    b"lintel-native-crash-v1\n",
    "{output}"
  );
  assert!(!report.join("failure.txt").exists());
  assert_eq!(
    fs::read_dir(report).unwrap().count(),
    5,
    "temporary capture metadata should be removed"
  );
  assert!(stream_types(&fs::read(report.join("report.dmp")).unwrap()).contains(&6));
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn assert_manual_snapshots(root: &Path) {
  use std::os::unix::fs::PermissionsExt;

  for name in ["first.dmp", "second.dmp"] {
    let path = root.join(name);
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
    let bytes = fs::read(&path).unwrap();
    let streams = stream_types(&bytes);
    for required in [3, 4, 7] {
      assert!(streams.contains(&required), "missing manual minidump stream {required}");
    }
    assert!(
      !streams.contains(&6),
      "manual snapshot unexpectedly includes an exception stream"
    );
  }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn private_snapshot_root() -> tempfile::TempDir {
  use std::os::unix::fs::PermissionsExt;

  tempfile::Builder::new()
    .permissions(fs::Permissions::from_mode(0o700))
    .tempdir()
    .unwrap()
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn manual_snapshots_and_cleanup_work_without_a_fatal_hook() {
  let root = private_snapshot_root();
  let helper = Path::new(env!("CARGO_BIN_EXE_lintel-snapshot-helper"));
  let (status, output) = run_probe(root.path(), helper, "--snapshots-only");
  assert!(status.success(), "isolated snapshot probe failed: {output}");
  assert_manual_snapshots(root.path());
  assert!(reports(root.path()).is_empty());
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn two_manual_snapshots_leave_the_fatal_reporter_available() {
  let root = private_snapshot_root();
  let fatal = Path::new(env!("CARGO_BIN_EXE_lintel-crash-helper"));
  let snapshot = Path::new(env!("CARGO_BIN_EXE_lintel-snapshot-helper"));
  let (status, output) = run_probe_with_snapshot(root.path(), fatal, "--snapshots-abort", Some(snapshot));
  assert!(
    !status.success(),
    "the isolated child must terminate after the deliberate abort: {output}"
  );
  assert!(
    output.contains("captured both snapshots before abort"),
    "isolated snapshot probe failed: {output}"
  );
  assert_manual_snapshots(root.path());
  let reports = reports(root.path());
  assert_eq!(reports.len(), 1);
  assert!(reports[0].join("completed").is_file());
  assert!(stream_types(&fs::read(reports[0].join("report.dmp")).unwrap()).contains(&6));
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
