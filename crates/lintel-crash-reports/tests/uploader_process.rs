#![cfg(unix)]

use std::{
  fs::{self, DirBuilder},
  io::{self, Read},
  net::TcpListener,
  os::unix::fs::DirBuilderExt as _,
  path::Path,
  thread,
  time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use lintel_crash_reports::{
  QueueLimits, ReportError, ReportQuery, ReportSourceConfig, ReportStatus, ReportStore, ReportsConfig, SourceKind,
  UploadMode, UploadPolicy, UploadTarget, UploaderStatus,
};

const APP_ID: &str = "dev.lintel.queue-process-test";

fn publish_fatal(root: &Path) {
  let staging = root.join("staging");
  DirBuilder::new().mode(0o700).create(&staging).unwrap();
  fs::write(
    staging.join("report.json"),
    serde_json::to_vec(&serde_json::json!({
        "format": "lintel-native-crash-v1",
        "application_id": APP_ID,
        "version": "1.2.3",
        "os": "macos",
        "architecture": "aarch64",
    }))
    .unwrap(),
  )
  .unwrap();
  let captured_at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
  fs::write(
    staging.join("capture.json"),
    serde_json::to_vec(&serde_json::json!({"captured_at_unix_ms": captured_at})).unwrap(),
  )
  .unwrap();
  let mut dump = vec![0_u8; 44 + 168];
  dump[..4].copy_from_slice(b"MDMP");
  for (offset, value) in [(4, 0xa793_u32), (8, 1), (12, 32), (32, 6), (36, 168), (40, 44)] {
    dump[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
  }
  fs::write(staging.join("report.dmp"), dump).unwrap();
  fs::write(staging.join("completed"), b"lintel-native-crash-v1\n").unwrap();
  fs::rename(&staging, root.join("report-late")).unwrap();
}

fn eventually<T>(mut probe: impl FnMut() -> Option<T>) -> T {
  let deadline = Instant::now() + Duration::from_secs(5);
  loop {
    if let Some(value) = probe() {
      return value;
    }
    assert!(
      Instant::now() < deadline,
      "uploader process did not reach the expected state"
    );
    thread::sleep(Duration::from_millis(20));
  }
}

#[test]
fn independent_uploader_discovers_a_late_dump_after_eof_and_honors_revocation() {
  let root = tempfile::tempdir().unwrap();
  let sources = root.path().join("native");
  DirBuilder::new().mode(0o700).create(&sources).unwrap();
  // Hold a loopback TLS handshake open to observe cancellation of real socket work.
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let store = ReportStore::open(ReportsConfig {
    directory: root.path().join("queue"),
    application_id: APP_ID.to_owned(),
    sources: vec![ReportSourceConfig {
      id: "native".to_owned(),
      directory: sources.clone(),
      kind: SourceKind::Native,
    }],
    limits: QueueLimits::default(),
    upload: Some(UploadTarget::new(format!(
      "https://{}/reports",
      listener.local_addr().unwrap()
    ))),
    policy: UploadPolicy {
      mode: UploadMode::Automatic,
      ..UploadPolicy::default()
    },
  })
  .unwrap();
  let child = store
    .spawn_uploader(
      Path::new(env!("CARGO_BIN_EXE_lintel-crash-uploader")),
      Duration::from_secs(6),
    )
    .unwrap();
  eventually(|| (store.upload_policy().unwrap().uploader == UploaderStatus::Running).then_some(()));
  drop(child);
  publish_fatal(&sources);
  let mut peer = eventually(|| match listener.accept() {
    Ok((peer, _)) => Some(peer),
    Err(error) if error.kind() == io::ErrorKind::WouldBlock => None,
    Err(error) => panic!("loopback listener failed: {error}"),
  });
  let report = store.list_reports(ReportQuery::default()).unwrap().pop().unwrap();
  assert!(matches!(report.status, ReportStatus::Uploading));

  let status = store
    .set_upload_policy(
      UploadPolicy {
        mode: UploadMode::Disabled,
        ..UploadPolicy::default()
      },
      Duration::from_secs(3),
    )
    .unwrap();
  assert_eq!(status.policy.mode, UploadMode::Disabled);
  assert!(matches!(store.request_upload(report.id), Err(ReportError::Disabled)));
  peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
  let mut buffer = [0_u8; 4096];
  loop {
    match peer.read(&mut buffer) {
      Ok(0) => break,
      Ok(_) => continue,
      Err(error) if error.kind() == io::ErrorKind::ConnectionReset => break,
      Err(error) => panic!("revoked uploader did not close the connection: {error}"),
    }
  }
  assert!(!matches!(
    store.status(report.id).unwrap().status,
    ReportStatus::Uploaded(_)
  ));
  eventually(|| (store.upload_policy().unwrap().uploader != UploaderStatus::Running).then_some(()));
}
