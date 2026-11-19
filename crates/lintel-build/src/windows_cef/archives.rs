//! Offline byte verification against reviewed official CEF 143 minimal archives.
//! No archive is extracted, downloaded, executed, or trusted via archive.json here.

use crate::windows::{real_directory, regular_file};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
  collections::BTreeSet,
  fs::{self, File},
  io::Read,
  path::{Component, Path, PathBuf},
};

const MANIFEST: &str = include_str!("cef143_minimal_manifest.json");
const MAX_DIAGNOSTICS: usize = 64;
const MAX_SDK_ENTRIES: usize = 8192;
const MAX_SDK_DEPTH: usize = 32;

/// An offline comparison with exact archived bytes retrieved from CEF's official
/// download service. It does not verify Authenticode, native linkage or sandbox execution.
#[derive(Debug, Serialize)]
pub struct WindowsCefArchiveVerification {
  pub target: String,
  pub archive_name: String,
  pub archive_url: String,
  pub index_url: String,
  pub index_snapshot_sha256: String,
  /// Official index SHA1 recorded when the stronger SHA256 pin was established.
  pub official_index_sha1: String,
  pub pinned_archive_sha256: String,
  pub observed_archive_sha256: Option<String>,
  pub archive_size: u64,
  pub archive_bytes_verified: bool,
  pub sdk_files_verified: bool,
  pub bootstrap_file_verified: bool,
  pub expected_sdk_files: usize,
  pub checked_sdk_files: usize,
  pub invalid_file_count: usize,
  pub unknown_file_count: usize,
  pub unknown_directory_count: usize,
  /// At most 64 diagnostics, with the total retained in invalid_file_count.
  pub invalid_files: Vec<String>,
}

#[derive(Deserialize)]
struct Manifest {
  index_url: String,
  index_sha256: String,
  archives: Vec<ArchivePin>,
}

#[derive(Deserialize)]
struct ArchivePin {
  target: String,
  name: String,
  url: String,
  official_sha1: String,
  sha256: String,
  size: u64,
  files: Vec<FilePin>,
}

#[derive(Deserialize)]
struct FilePin {
  path: String,
  size: u64,
  sha256: String,
}

pub(super) fn verify(cef: &Path, archive: &Path, target: &str) -> Result<WindowsCefArchiveVerification> {
  let manifest: Manifest = serde_json::from_str(MANIFEST).context("invalid embedded CEF archive pins")?;
  let pin = manifest
    .archives
    .iter()
    .find(|pin| pin.target == target)
    .context("official minimal archive verification supports only pinned Windows MSVC targets")?;
  ensure!(
    !pin.files.is_empty() && pin.files.len() < MAX_SDK_ENTRIES,
    "invalid embedded SDK file count"
  );
  verify_pin(cef, archive, pin, &manifest.index_url, &manifest.index_sha256)
}

fn verify_pin(
  cef: &Path,
  archive: &Path,
  pin: &ArchivePin,
  index_url: &str,
  index_sha256: &str,
) -> Result<WindowsCefArchiveVerification> {
  let mut report = WindowsCefArchiveVerification {
    target: pin.target.clone(),
    archive_name: pin.name.clone(),
    archive_url: pin.url.clone(),
    index_url: index_url.into(),
    index_snapshot_sha256: index_sha256.into(),
    official_index_sha1: pin.official_sha1.clone(),
    pinned_archive_sha256: pin.sha256.clone(),
    observed_archive_sha256: None,
    archive_size: pin.size,
    archive_bytes_verified: false,
    sdk_files_verified: false,
    bootstrap_file_verified: false,
    expected_sdk_files: pin.files.len(),
    checked_sdk_files: 0,
    invalid_file_count: 0,
    unknown_file_count: 0,
    unknown_directory_count: 0,
    invalid_files: vec![],
  };
  match digest_sized_file(archive, pin.size) {
    Ok(digest) => {
      report.archive_bytes_verified = digest == pin.sha256;
      report.observed_archive_sha256 = Some(digest);
      if !report.archive_bytes_verified {
        report.invalid("compressed archive SHA256 differs from the reviewed official archive".into());
      }
    }
    Err(error) => report.invalid(format!("compressed archive: {error:#}")),
  }
  // An unauthenticated compressed file cannot authenticate an extracted SDK.
  if !report.archive_bytes_verified {
    return Ok(report);
  }
  if !real_directory(cef).unwrap_or(false) {
    report.invalid("SDK root must be a real directory, not a symlink/reparse point".into());
    return Ok(report);
  }
  let root = cef.canonicalize().context("resolve SDK root")?;
  for file in &pin.files {
    let result = sdk_path(&root, &file.path).and_then(|path| digest_sized_file(&path, file.size));
    match result {
      Ok(digest) if digest == file.sha256 => {
        report.checked_sdk_files += 1;
        if file.path == "Release/bootstrap.exe" {
          report.bootstrap_file_verified = true;
        }
      }
      Ok(_) => report.invalid(format!("{}: SHA256 differs from official archive", file.path)),
      Err(error) => report.invalid(format!("{}: {error:#}", file.path)),
    }
  }
  inspect_extra_entries(&root, pin, &mut report);
  report.sdk_files_verified = report.invalid_file_count == 0 && report.checked_sdk_files == pin.files.len();
  Ok(report)
}

impl WindowsCefArchiveVerification {
  fn invalid(&mut self, message: String) {
    self.invalid_file_count += 1;
    if self.invalid_files.len() < MAX_DIAGNOSTICS {
      self.invalid_files.push(message);
    }
  }
}

fn digest_sized_file(path: &Path, expected_size: u64) -> Result<String> {
  ensure!(
    expected_size <= 512 * 1024 * 1024,
    "CEF pinned file exceeds the 512 MiB inspection limit"
  );
  let path = regular_file(path)?;
  let file = File::open(path)?;
  ensure!(
    file.metadata()?.len() == expected_size,
    "file length differs from official archive"
  );
  // Bound a concurrent growth race too. The read-only report is a snapshot,
  // never an installation lock, publication grant or future file-identity lease.
  let mut source = file.take(expected_size + 1);
  let mut hasher = Sha256::new();
  let mut read = 0_u64;
  let mut buffer = [0_u8; 64 * 1024];
  loop {
    let count = source.read(&mut buffer)?;
    if count == 0 {
      break;
    }
    read += count as u64;
    hasher.update(&buffer[..count]);
  }
  ensure!(read == expected_size, "file changed length while hashing");
  Ok(format!("{:x}", hasher.finalize()))
}

fn sdk_relative_path(root: &Path, archived: &str) -> Result<PathBuf> {
  ensure!(
    !archived.contains(['\\', ':'])
      && Path::new(archived)
        .components()
        .all(|part| matches!(part, Component::Normal(_))),
    "invalid embedded SDK relative path"
  );
  // cef-rs can export Release/Resources contents into one root. The rest of
  // the complete minimal SDK, including build inputs, retains its relative paths.
  Ok(if let Some(rest) = archived.strip_prefix("Release/") {
    if has_sdk_directory(root, "Release")? {
      PathBuf::from(archived)
    } else {
      PathBuf::from(rest)
    }
  } else if let Some(rest) = archived.strip_prefix("Resources/") {
    if has_sdk_directory(root, "Resources")? {
      PathBuf::from(archived)
    } else {
      PathBuf::from(rest)
    }
  } else {
    PathBuf::from(archived)
  })
}

fn has_sdk_directory(root: &Path, name: &str) -> Result<bool> {
  let path = root.join(name);
  match fs::symlink_metadata(&path) {
    Ok(_) => {
      ensure!(
        real_directory(&path)?,
        "SDK {name} is not a real directory (links/reparse points are rejected)"
      );
      Ok(true)
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
    Err(error) => Err(error).with_context(|| format!("inspect SDK {name}")),
  }
}

fn sdk_path(root: &Path, archived: &str) -> Result<PathBuf> {
  let relative = sdk_relative_path(root, archived)?;
  let mut path = root.to_owned();
  let parts: Vec<_> = relative.components().collect();
  for (index, component) in parts.iter().enumerate() {
    path.push(component.as_os_str());
    if index + 1 < parts.len() {
      ensure!(
        real_directory(&path).unwrap_or(false),
        "SDK ancestor is missing or is a link/reparse point"
      );
    }
  }
  let resolved = regular_file(&path)?;
  ensure!(resolved.starts_with(root), "SDK file escapes its root");
  Ok(resolved)
}

fn inspect_extra_entries(root: &Path, pin: &ArchivePin, report: &mut WindowsCefArchiveVerification) {
  let mut expected_files = BTreeSet::new();
  let mut expected_directories = BTreeSet::new();
  for file in &pin.files {
    let relative = match sdk_relative_path(root, &file.path) {
      Ok(relative) => relative,
      Err(error) => {
        report.invalid(format!("invalid embedded SDK path: {error:#}"));
        return;
      }
    };
    let mut parent = relative.parent();
    while let Some(path) = parent.filter(|path| !path.as_os_str().is_empty()) {
      expected_directories.insert(path.to_owned());
      parent = path.parent();
    }
    if !expected_files.insert(relative) {
      report.invalid("embedded SDK paths collide in the selected layout".into());
      return;
    }
  }
  let mut pending = vec![(root.to_owned(), 0_usize)];
  let mut entries = 0_usize;
  while let Some((directory, depth)) = pending.pop() {
    if depth > MAX_SDK_DEPTH {
      report.invalid("SDK tree exceeds the 32-level inspection bound".into());
      return;
    }
    let listing = match fs::read_dir(&directory) {
      Ok(listing) => listing,
      Err(error) => {
        report.invalid(format!("cannot enumerate SDK directory: {error}"));
        return;
      }
    };
    for entry in listing {
      entries += 1;
      if entries > MAX_SDK_ENTRIES {
        report.invalid("SDK tree exceeds the 8192-entry inspection bound".into());
        return;
      }
      let path = match entry {
        Ok(entry) => entry.path(),
        Err(error) => {
          report.invalid(format!("cannot inspect SDK entry: {error}"));
          continue;
        }
      };
      let relative = match path.strip_prefix(root) {
        Ok(relative) => relative,
        Err(_) => {
          report.invalid("SDK enumeration escaped its root".into());
          continue;
        }
      };
      if real_directory(&path).unwrap_or(false) {
        if expected_directories.contains(relative) {
          pending.push((path, depth + 1));
        } else {
          report.unknown_directory_count += 1;
          report.invalid(format!("unexpected SDK directory: {}", relative.display()));
          // It is already untrusted. Do not traverse arbitrary unknown trees.
        }
      } else if regular_file(&path).is_ok() {
        if relative == Path::new("archive.json") {
          if let Err(error) = super::sdk_metadata(root, &pin.target) {
            report.invalid(format!("invalid optional repack metadata archive.json: {error:#}"));
          }
        } else if !expected_files.contains(relative) {
          report.unknown_file_count += 1;
          report.invalid(format!("unexpected SDK file: {}", relative.display()));
        }
      } else {
        report.invalid(format!(
          "SDK entry is not a real file/directory (links/reparse points are rejected): {}",
          relative.display()
        ));
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fixture(root: &Path) -> (PathBuf, PathBuf, ArchivePin) {
    let sdk = root.join("sdk");
    let archive = root.join("download.tar.bz2");
    fs::write(&archive, b"compressed fixture, not a CEF runtime").unwrap();
    let mut files = vec![];
    for (path, bytes) in [
      ("Release/bootstrap.exe", b"bootstrap fixture".as_slice()),
      ("Release/libcef.dll", b"runtime fixture".as_slice()),
      ("include/cef_version.h", b"header fixture".as_slice()),
    ] {
      let destination = sdk.join(path);
      fs::create_dir_all(destination.parent().unwrap()).unwrap();
      fs::write(destination, bytes).unwrap();
      files.push(FilePin {
        path: path.into(),
        size: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
      });
    }
    let size = fs::metadata(&archive).unwrap().len();
    let pin = ArchivePin {
      target: "x86_64-pc-windows-msvc".into(),
      name: "fixture.tar.bz2".into(),
      url: "fixture only".into(),
      official_sha1: "fixture is not official".into(),
      sha256: digest_sized_file(&archive, size).unwrap(),
      size,
      files,
    };
    (sdk, archive, pin)
  }

  #[test]
  fn actual_compressed_bytes_and_all_sdk_inputs_must_match_not_repack_metadata() {
    let root = tempfile::tempdir().unwrap();
    let (sdk, archive, pin) = fixture(root.path());
    let verified = verify_pin(&sdk, &archive, &pin, "fixture", "fixture").unwrap();
    assert!(verified.archive_bytes_verified && verified.sdk_files_verified && verified.bootstrap_file_verified);
    assert_eq!(verified.checked_sdk_files, 3);
    fs::write(sdk.join("archive.json"), b"self-asserted checksum metadata").unwrap();
    let mut replacement = fs::read(&archive).unwrap();
    replacement[0] ^= 1;
    fs::write(&archive, replacement).unwrap();
    let untrusted = verify_pin(&sdk, &archive, &pin, "fixture", "fixture").unwrap();
    assert!(!untrusted.archive_bytes_verified && !untrusted.sdk_files_verified && !untrusted.bootstrap_file_verified);
    assert_eq!(untrusted.checked_sdk_files, 0);
  }

  #[test]
  fn authentic_archive_does_not_vouch_for_an_edited_runtime_or_bootstrap() {
    let root = tempfile::tempdir().unwrap();
    let (sdk, archive, pin) = fixture(root.path());
    fs::write(sdk.join("Release/bootstrap.exe"), b"Bootstrap fixture").unwrap();
    fs::remove_file(sdk.join("include/cef_version.h")).unwrap();
    let changed = verify_pin(&sdk, &archive, &pin, "fixture", "fixture").unwrap();
    assert!(changed.archive_bytes_verified);
    assert!(!changed.sdk_files_verified && !changed.bootstrap_file_verified);
    assert_eq!(changed.invalid_file_count, 2);
    assert!(
      changed
        .invalid_files
        .iter()
        .any(|error| error.contains("SHA256 differs"))
    );
  }

  #[cfg(unix)]
  #[test]
  fn matching_bytes_beyond_a_symlinked_sdk_ancestor_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let (sdk, archive, pin) = fixture(root.path());
    let moved = root.path().join("outside");
    fs::rename(sdk.join("Release"), &moved).unwrap();
    std::os::unix::fs::symlink(&moved, sdk.join("Release")).unwrap();
    let linked = verify_pin(&sdk, &archive, &pin, "fixture", "fixture").unwrap();
    assert!(linked.archive_bytes_verified);
    assert!(!linked.sdk_files_verified && !linked.bootstrap_file_verified);
  }

  #[test]
  fn trailing_archive_data_and_relative_path_escapes_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let (sdk, archive, pin) = fixture(root.path());
    let mut extra = fs::read(&archive).unwrap();
    extra.extend_from_slice(b"unreviewed trailing bytes");
    fs::write(&archive, extra).unwrap();
    assert!(
      !verify_pin(&sdk, &archive, &pin, "fixture", "fixture")
        .unwrap()
        .archive_bytes_verified
    );
    fs::write(&archive, b"short file").unwrap();
    assert!(
      !verify_pin(&sdk, &archive, &pin, "fixture", "fixture")
        .unwrap()
        .archive_bytes_verified
    );
    for path in [
      "../escape",
      "/absolute",
      "Release/../escape",
      "C:/escape",
      "Release\\escape",
    ] {
      assert!(sdk_path(root.path(), path).is_err());
    }
  }

  #[test]
  fn unknown_dlls_empty_directories_and_forged_repack_records_prevent_complete_sdk_provenance() {
    let root = tempfile::tempdir().unwrap();
    let (sdk, archive, pin) = fixture(root.path());
    fs::write(sdk.join("Release/extra.dll"), b"unreviewed DLL").unwrap();
    fs::create_dir_all(sdk.join("unreviewed/nested/deep/tree")).unwrap();
    fs::write(sdk.join("archive.json"), b"self-asserted checksum metadata").unwrap();
    let changed = verify_pin(&sdk, &archive, &pin, "fixture", "fixture").unwrap();
    assert!(changed.archive_bytes_verified && changed.bootstrap_file_verified);
    assert!(!changed.sdk_files_verified);
    assert_eq!(changed.unknown_file_count, 1);
    assert_eq!(changed.unknown_directory_count, 1);
    assert!(
      changed
        .invalid_files
        .iter()
        .any(|error| error.contains("optional repack metadata"))
    );
  }
}
