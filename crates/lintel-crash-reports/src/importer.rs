use std::{
  fs::{self, File},
  io::{self, Read, Seek, SeekFrom},
  path::{Path, PathBuf},
  time::UNIX_EPOCH,
};

use cap_primitives::fs::{FollowSymlinks, open_dir_nofollow};
use cap_std::{
  ambient_authority,
  fs::{Dir, OpenOptions},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::{ReportError, ReportId, ReportKind, ReportSource, ReportSourceConfig, SourceKind};

pub(crate) const SNAPSHOT_SOURCE_ID: &str = "manual-snapshot";
const NATIVE_FORMAT: &str = "lintel-native-crash-v1";
const MAX_METADATA_BYTES: u64 = 8 * 1024;
const MAX_DUMP_STREAMS: u32 = 4096;
const MINIDUMP_SYSTEM_INFO_SIZE: u32 = 56;
const MINIDUMP_OS_MACOS: u32 = 0x8101;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct UploadMetadata {
  pub schema_version: u32,
  pub report_id: ReportId,
  pub application_id: String,
  pub source_id: String,
  pub source: ReportSource,
  pub kind: ReportKind,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub captured_at_unix_ms: Option<u64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub application_version: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub platform: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub architecture: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Manifest {
  pub backend_id: String,
  pub retention_time: u64,
  pub size_bytes: u64,
  pub metadata: UploadMetadata,
}

pub(crate) struct Candidate {
  pub dump: File,
  pub manifest: Manifest,
}

#[derive(Deserialize)]
struct NativeMetadata {
  format: String,
  application_id: String,
  version: String,
  os: String,
  architecture: String,
  process_role: Option<String>,
  service: Option<ServiceMetadata>,
}

#[derive(Deserialize)]
struct ServiceMetadata {
  identity: String,
  attempt: u64,
}

#[derive(Deserialize)]
struct CaptureMetadata {
  captured_at_unix_ms: u64,
}

pub(crate) fn scan(
  source: &ReportSourceConfig,
  application_id: &str,
  keep_scanning: impl Fn() -> bool,
  mut accept: impl FnMut(Candidate) -> Result<bool, ReportError>,
) -> Result<(), ReportError> {
  let root = match open_root(&source.directory) {
    Ok(root) => root,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
    Err(error) => return Err(error.into()),
  };
  match source.kind {
    SourceKind::Native | SourceKind::MojoService => {
      for entry in root.entries()?.flatten() {
        if !keep_scanning() {
          return Ok(());
        }
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| name.starts_with("report-")) else {
          continue;
        };
        if let Ok(candidate) = read_native(&root, name, source, application_id)
          && !accept(candidate)?
        {
          break;
        }
      }
    }
    SourceKind::Cef if cfg!(target_os = "macos") => {
      for state in ["pending", "completed"] {
        let Ok(directory) = open_child(&root, Path::new(state)) else {
          continue;
        };
        for entry in directory.entries()?.flatten() {
          if !keep_scanning() {
            return Ok(());
          }
          let name = entry.file_name();
          let Some(name) = name.to_str() else { continue };
          if let Ok(candidate) = read_cef(&directory, name, source, application_id)
            && !accept(candidate)?
          {
            return Ok(());
          }
        }
      }
    }
    SourceKind::Cef => return Err(ReportError::Unsupported),
  }
  Ok(())
}

fn read_native(
  root: &Dir,
  name: &str,
  source: &ReportSourceConfig,
  application_id: &str,
) -> Result<Candidate, ReportError> {
  let directory = open_child(root, Path::new(name))?;
  let mut marker = Vec::new();
  regular_file(&directory, Path::new("completed"))?
    .take(64)
    .read_to_end(&mut marker)?;
  if marker != b"lintel-native-crash-v1\n" {
    return Err(ReportError::InvalidReport);
  }
  let metadata: NativeMetadata = read_json(&directory, "report.json")?;
  if metadata.format != NATIVE_FORMAT || !valid_text(&metadata.os, 32) || !valid_text(&metadata.architecture, 32) {
    return Err(ReportError::InvalidReport);
  }
  let report_source = match (source.kind, metadata.service) {
    (SourceKind::Native, None) if metadata.application_id == application_id && metadata.process_role.is_none() => {
      ReportSource::Native
    }
    (SourceKind::MojoService, Some(service))
      if metadata.process_role.as_deref() == Some("service")
        && valid_text(&service.identity, 256)
        && service.attempt > 0 =>
    {
      ReportSource::MojoService {
        service_id: service.identity,
        attempt: service.attempt,
      }
    }
    _ => return Err(ReportError::InvalidReport),
  };
  let captured_at = read_json::<CaptureMetadata>(&directory, "capture.json")
    .ok()
    .map(|capture| capture.captured_at_unix_ms)
    .filter(|time| *time > 0 && *time <= crate::store::now_ms());
  let dump = regular_file(&directory, Path::new("report.dmp"))?;
  let file_metadata = dump.metadata()?;
  Ok(Candidate {
    dump,
    manifest: Manifest {
      backend_id: name.to_owned(),
      retention_time: captured_at.unwrap_or(file_time(&file_metadata)?),
      size_bytes: file_metadata.len(),
      metadata: UploadMetadata {
        schema_version: 1,
        report_id: ReportId(Uuid::new_v4()),
        application_id: application_id.to_owned(),
        source_id: source.id.clone(),
        source: report_source,
        kind: ReportKind::Fatal,
        captured_at_unix_ms: captured_at,
        application_version: valid_text(&metadata.version, 128).then_some(metadata.version),
        platform: Some(metadata.os),
        architecture: Some(metadata.architecture),
      },
    },
  })
}

fn read_cef(
  directory: &Dir,
  name: &str,
  source: &ReportSourceConfig,
  application_id: &str,
) -> Result<Candidate, ReportError> {
  let id = name.strip_suffix(".dmp").ok_or(ReportError::InvalidReport)?;
  let uuid = Uuid::parse_str(id).map_err(|_| ReportError::InvalidReport)?;
  if !uuid.hyphenated().to_string().eq_ignore_ascii_case(id) {
    return Err(ReportError::InvalidReport);
  }
  let mut dump = regular_file(directory, Path::new(name))?;
  let file_metadata = dump.metadata()?;
  let info = inspect_dump_info(&mut dump, file_metadata.len(), ReportKind::Fatal).ok();
  let captured_at = info
    .as_ref()
    .and_then(|info| info.cef_capture_time(crate::store::now_ms()));
  let platform = info.as_ref().filter(|info| info.platform_id == Some(MINIDUMP_OS_MACOS));
  // The queue checks the copied dump's structure even when this metadata read fails.
  dump.rewind()?;
  Ok(Candidate {
    dump,
    manifest: Manifest {
      backend_id: uuid.hyphenated().to_string(),
      retention_time: match captured_at {
        Some(time) => time,
        None => file_time(&file_metadata)?,
      },
      size_bytes: file_metadata.len(),
      metadata: UploadMetadata {
        schema_version: 1,
        report_id: ReportId(Uuid::new_v4()),
        application_id: application_id.to_owned(),
        source_id: source.id.clone(),
        source: ReportSource::Cef,
        kind: ReportKind::Fatal,
        captured_at_unix_ms: captured_at,
        application_version: None,
        platform: platform.map(|_| "macos".to_owned()),
        architecture: None,
      },
    },
  })
}

pub(crate) fn snapshot(
  staging: &Path,
  path: &Path,
  application_id: &str,
  version: &str,
) -> Result<Candidate, ReportError> {
  if !valid_text(version, 128) {
    return Err(ReportError::InvalidReport);
  }
  let relative = path.strip_prefix(staging).map_err(|_| ReportError::InvalidReport)?;
  // The facade can use a private per-request directory inside staging.
  let components: Vec<_> = relative.components().collect();
  let kind = |component| match component {
    std::path::Component::Normal(name) => Ok(PathBuf::from(name)),
    _ => Err(ReportError::InvalidReport),
  };
  let root = open_root(staging)?;
  let dump = match components.as_slice() {
    [file] => regular_file(&root, &kind(*file)?)?,
    [directory, file] => regular_file(&open_child(&root, &kind(*directory)?)?, &kind(*file)?)?,
    _ => return Err(ReportError::InvalidReport),
  };
  let size_bytes = dump.metadata()?.len();
  let time = crate::store::now_ms();
  let id = ReportId(Uuid::new_v4());
  Ok(Candidate {
    dump,
    manifest: Manifest {
      backend_id: id.to_string(),
      retention_time: time,
      size_bytes,
      metadata: UploadMetadata {
        schema_version: 1,
        report_id: id,
        application_id: application_id.to_owned(),
        source_id: SNAPSHOT_SOURCE_ID.to_owned(),
        source: ReportSource::Native,
        kind: ReportKind::ManualSnapshot,
        captured_at_unix_ms: Some(time),
        application_version: Some(version.to_owned()),
        platform: Some(std::env::consts::OS.to_owned()),
        architecture: Some(std::env::consts::ARCH.to_owned()),
      },
    },
  })
}

pub(crate) fn open_root(path: &Path) -> io::Result<Dir> {
  let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
  let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
  let parent = Dir::open_ambient_dir(parent, ambient_authority())?;
  open_child(&parent, Path::new(name))
}

fn open_child(root: &Dir, relative: &Path) -> io::Result<Dir> {
  let root = root.try_clone()?.into_std_file();
  let directory = open_dir_nofollow(&root, relative)?;
  check_owner(&directory.metadata()?)?;
  Ok(Dir::from_std_file(directory))
}

pub(crate) fn regular_file(directory: &Dir, path: &Path) -> io::Result<File> {
  if !directory.symlink_metadata(path)?.is_file() {
    return Err(io::ErrorKind::InvalidData.into());
  }
  let mut options = OpenOptions::new();
  options.read(true)._cap_fs_ext_follow(FollowSymlinks::No);
  #[cfg(unix)]
  {
    use cap_std::fs::OpenOptionsExt as _;
    options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
  }
  let file = directory.open_with(path, &options)?.into_std();
  let metadata = file.metadata()?;
  if !metadata.is_file() {
    return Err(io::ErrorKind::InvalidData.into());
  }
  check_owner(&metadata)?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt as _;
    if metadata.nlink() != 1 {
      return Err(io::ErrorKind::PermissionDenied.into());
    }
  }
  Ok(file)
}

pub(crate) fn check_owner(metadata: &fs::Metadata) -> io::Result<()> {
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt as _;
    if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o022 != 0 {
      return Err(io::ErrorKind::PermissionDenied.into());
    }
  }
  #[cfg(not(unix))]
  let _ = metadata;
  Ok(())
}

pub(crate) fn read_json<T: DeserializeOwned>(directory: &Dir, name: &str) -> Result<T, ReportError> {
  let file = regular_file(directory, Path::new(name))?;
  if file.metadata()?.len() > MAX_METADATA_BYTES {
    return Err(ReportError::InvalidReport);
  }
  let mut bytes = Vec::new();
  file.take(MAX_METADATA_BYTES + 1).read_to_end(&mut bytes)?;
  if bytes.len() as u64 > MAX_METADATA_BYTES {
    return Err(ReportError::InvalidReport);
  }
  Ok(serde_json::from_slice(&bytes)?)
}

fn file_time(metadata: &fs::Metadata) -> io::Result<u64> {
  metadata
    .modified()?
    .duration_since(UNIX_EPOCH)
    .ok()
    .and_then(|duration| u64::try_from(duration.as_millis()).ok())
    .map(|time| time.min(crate::store::now_ms()))
    .ok_or_else(|| io::ErrorKind::InvalidData.into())
}

pub(crate) fn valid_text(value: &str, max_len: usize) -> bool {
  !value.is_empty() && value.len() <= max_len && !value.chars().any(char::is_control)
}

struct DumpInfo {
  architecture: Option<&'static str>,
  platform_id: Option<u32>,
  timestamp_seconds: u32,
}

impl DumpInfo {
  fn cef_capture_time(&self, now_ms: u64) -> Option<u64> {
    // Chromium 143's Crashpad writes SnapshotTime().tv_sec. Use the beginning of that second
    // so an automatic policy cutoff inside the second leaves the report local.
    let time = u64::from(self.timestamp_seconds) * 1_000;
    (self.platform_id == Some(MINIDUMP_OS_MACOS) && time > 0 && time <= now_ms).then_some(time)
  }
}

pub(crate) fn inspect_dump(dump: &mut File, size: u64, kind: ReportKind) -> Result<Option<String>, ReportError> {
  Ok(inspect_dump_info(dump, size, kind)?.architecture.map(str::to_owned))
}

fn inspect_dump_info(dump: &mut (impl Read + Seek), size: u64, kind: ReportKind) -> Result<DumpInfo, ReportError> {
  let mut header = [0; 32];
  dump.read_exact(&mut header).map_err(|_| ReportError::InvalidReport)?;
  if &header[..4] != b"MDMP" || header[4..6] != [0x93, 0xa7] {
    return Err(ReportError::InvalidReport);
  }
  let stream_count = read_u32(&header[8..12]);
  let directory = u64::from(read_u32(&header[12..16]));
  if !(1..=MAX_DUMP_STREAMS).contains(&stream_count)
    || directory < 32
    || directory + 12 * u64::from(stream_count) > size
  {
    return Err(ReportError::InvalidReport);
  }
  dump.seek(SeekFrom::Start(directory))?;
  let mut exception = false;
  let mut modules = false;
  let mut threads = false;
  let mut system_info = None;
  let mut system_info_count = 0;
  for _ in 0..stream_count {
    let mut entry = [0; 12];
    dump.read_exact(&mut entry).map_err(|_| ReportError::InvalidReport)?;
    let stream_type = read_u32(&entry[..4]);
    let length = read_u32(&entry[4..8]);
    let offset = u64::from(read_u32(&entry[8..12]));
    if offset + u64::from(length) > size || (length > 0 && offset < 32) {
      return Err(ReportError::InvalidReport);
    }
    match stream_type {
      3 | 8 => threads |= length >= 4,
      4 => modules |= length >= 4,
      6 => exception |= length >= 168,
      7 => {
        system_info_count += 1;
        if length >= 2 {
          system_info = Some((offset, length));
        }
      }
      _ => {}
    }
  }
  if (kind == ReportKind::Fatal && !exception) || (kind == ReportKind::ManualSnapshot && !(modules && threads)) {
    return Err(ReportError::InvalidReport);
  }
  let mut platform_id = None;
  let architecture = if let Some((offset, length)) = system_info {
    dump.seek(SeekFrom::Start(offset))?;
    let mut bytes = [0; 24];
    if system_info_count == 1 && length >= MINIDUMP_SYSTEM_INFO_SIZE {
      dump.read_exact(&mut bytes)?;
      platform_id = Some(read_u32(&bytes[20..24]));
    } else {
      dump.read_exact(&mut bytes[..2])?;
    }
    match u16::from_le_bytes([bytes[0], bytes[1]]) {
      0 => Some("x86"),
      5 => Some("arm"),
      9 => Some("x86_64"),
      12 | 0x8003 => Some("aarch64"),
      _ => None,
    }
  } else {
    None
  };
  dump.rewind()?;
  Ok(DumpInfo {
    architecture,
    platform_id,
    timestamp_seconds: read_u32(&header[20..24]),
  })
}

fn read_u32(bytes: &[u8]) -> u32 {
  let mut value = [0; 4];
  value.copy_from_slice(bytes);
  u32::from_le_bytes(value)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Cursor;

  fn fatal_dump(timestamp_seconds: u32, platform_id: u32) -> Vec<u8> {
    let mut dump = vec![0; 280];
    dump[..4].copy_from_slice(b"MDMP");
    dump[4..6].copy_from_slice(&[0x93, 0xa7]);
    let mut field = |offset: usize, value: u32| dump[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    field(8, 2); // Two streams at offset 32.
    field(12, 32);
    field(20, timestamp_seconds);
    field(32, 6); // ExceptionStream, 168 bytes at offset 56.
    field(36, 168);
    field(40, 56);
    field(44, 7); // SystemInfoStream, 56 bytes at offset 224.
    field(48, MINIDUMP_SYSTEM_INFO_SIZE);
    field(52, 224);
    field(244, platform_id);
    dump[224..226].copy_from_slice(&12_u16.to_le_bytes());
    dump
  }

  fn inspect(bytes: Vec<u8>) -> Result<DumpInfo, ReportError> {
    let size = bytes.len() as u64;
    inspect_dump_info(&mut Cursor::new(bytes), size, ReportKind::Fatal)
  }

  #[test]
  fn cef_timestamp_uses_the_start_of_a_verified_macos_snapshot_second() {
    let info = inspect(fatal_dump(4, MINIDUMP_OS_MACOS)).unwrap();
    let capture = info.cef_capture_time(5_000).unwrap();
    assert_eq!(capture, 4_000);
    assert!(capture < 4_001);
    assert_eq!(info.architecture, Some("aarch64"));

    for (timestamp, platform) in [(0, MINIDUMP_OS_MACOS), (6, MINIDUMP_OS_MACOS), (4, 0x8201)] {
      assert_eq!(
        inspect(fatal_dump(timestamp, platform))
          .unwrap()
          .cef_capture_time(5_000),
        None
      );
    }

    let mut incomplete_info = fatal_dump(4, MINIDUMP_OS_MACOS);
    incomplete_info[48..52].copy_from_slice(&24_u32.to_le_bytes());
    assert_eq!(inspect(incomplete_info).unwrap().cef_capture_time(5_000), None);

    let mut invalid_header = fatal_dump(4, MINIDUMP_OS_MACOS);
    invalid_header[4] = 0;
    assert!(matches!(inspect(invalid_header), Err(ReportError::InvalidReport)));
  }
}
