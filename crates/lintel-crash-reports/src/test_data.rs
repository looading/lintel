use std::{fs, path::Path};

use crate::{ReportSourceConfig, ReportsConfig, SourceKind, UploadTarget};

pub(crate) fn config(root: &Path) -> ReportsConfig {
  ReportsConfig {
    directory: root.join("queue"),
    application_id: "lintel.test".to_owned(),
    sources: vec![ReportSourceConfig {
      id: "host".to_owned(),
      directory: root.join("native"),
      kind: SourceKind::Native,
    }],
    upload: Some(UploadTarget::new("https://localhost:9/crashes")),
    policy: Default::default(),
    limits: Default::default(),
  }
}

pub(crate) fn dump(streams: &[(u32, u32)]) -> Vec<u8> {
  let mut dump = vec![0; 32 + 12 * streams.len()];
  dump[..4].copy_from_slice(b"MDMP");
  dump[4..6].copy_from_slice(&0xa793u16.to_le_bytes());
  dump[8..12].copy_from_slice(&(streams.len() as u32).to_le_bytes());
  dump[12..16].copy_from_slice(&32u32.to_le_bytes());
  for (index, &(kind, size)) in streams.iter().enumerate() {
    let entry = 32 + 12 * index;
    let offset = dump.len() as u32;
    dump[entry..entry + 4].copy_from_slice(&kind.to_le_bytes());
    dump[entry + 4..entry + 8].copy_from_slice(&size.to_le_bytes());
    dump[entry + 8..entry + 12].copy_from_slice(&offset.to_le_bytes());
    dump.resize(dump.len() + size as usize, 0);
  }
  dump
}

pub(crate) fn fatal(config: &ReportsConfig, name: &str, captured_at: Option<u64>, complete: bool) {
  let directory = config.sources[0].directory.join(name);
  fs::create_dir_all(&directory).unwrap();
  let metadata = serde_json::json!({
    "format": "lintel-native-crash-v1",
    "application_id": config.application_id,
    "version": "1.2.3",
    "os": "macos",
    "architecture": "aarch64",
    "process_id": 421,
  });
  fs::write(directory.join("report.json"), metadata.to_string()).unwrap();
  fs::write(directory.join("report.dmp"), dump(&[(6, 168)])).unwrap();
  if let Some(captured_at) = captured_at {
    fs::write(
      directory.join("capture.json"),
      format!("{{\"captured_at_unix_ms\":{captured_at}}}"),
    )
    .unwrap();
  }
  if complete {
    fs::write(directory.join("completed"), b"lintel-native-crash-v1\n").unwrap();
  }
}
