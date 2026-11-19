use std::{
  io::{Read, Write},
  path::{Component, Path},
  sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, ensure};
use cap_std::{
  ambient_authority,
  fs::{Dir, OpenOptions},
};

/// 只访问获准目录，以目录句柄约束符号链接和相对路径。
pub struct FileAccess {
  directory: Dir,
  writable: bool,
  max_bytes: u64,
}

impl FileAccess {
  pub fn read_only(root: impl AsRef<Path>, max_bytes: u64) -> Result<Self> {
    Ok(Self {
      directory: Dir::open_ambient_dir(root, ambient_authority())?,
      writable: false,
      max_bytes,
    })
  }

  pub fn read_write(root: impl AsRef<Path>, max_bytes: u64) -> Result<Self> {
    let mut access = Self::read_only(root, max_bytes)?;
    access.writable = true;
    Ok(access)
  }

  pub fn read(&self, relative: impl AsRef<Path>) -> Result<Vec<u8>> {
    let relative = relative.as_ref();
    validate_relative(relative)?;
    let mut bytes = Vec::new();
    self
      .directory
      .open(relative)?
      .take(self.max_bytes.saturating_add(1))
      .read_to_end(&mut bytes)?;
    ensure!(
      bytes.len() as u64 <= self.max_bytes,
      "file exceeds configured size limit"
    );
    Ok(bytes)
  }

  /// 先同步临时文件，再在同一目录中替换目标文件。
  pub fn write(&self, relative: impl AsRef<Path>, bytes: &[u8]) -> Result<()> {
    ensure!(self.writable, "file capability is read-only");
    ensure!(
      bytes.len() as u64 <= self.max_bytes,
      "file exceeds configured size limit"
    );
    let relative = relative.as_ref();
    validate_relative(relative)?;
    let parent = relative
      .parent()
      .filter(|path| !path.as_os_str().is_empty())
      .unwrap_or(Path::new("."));
    let directory = self.directory.open_dir(parent)?;
    let filename = relative.file_name().context("missing file name")?;
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let temporary = format!(
      ".lintel-write-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut created = false;
    let result = (|| -> Result<()> {
      let mut file = directory.open_with(&temporary, &options)?;
      created = true;
      file.write_all(bytes)?;
      file.sync_all()?;
      drop(file);
      directory.rename(&temporary, &directory, filename)?;
      Ok(())
    })();
    if created && result.is_err() {
      let _ = directory.remove_file(&temporary);
    }
    result
  }
}

fn validate_relative(path: &Path) -> Result<()> {
  ensure!(
    !path.as_os_str().is_empty() && path.components().all(|part| matches!(part, Component::Normal(_))),
    "file path must remain relative to its granted directory"
  );
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn files_are_bounded_and_atomic_with_explicit_write_access() {
    let root = tempfile::tempdir().unwrap();
    let files = FileAccess::read_write(root.path(), 8).unwrap();
    files.write("notes", b"before").unwrap();
    assert!(files.write("notes", b"too large to save").is_err());
    assert_eq!(files.read("notes").unwrap(), b"before");
    files.write("notes", b"after").unwrap();
    assert_eq!(files.read("notes").unwrap(), b"after");
    assert!(files.read("../notes").is_err());
    assert!(files.read(root.path().join("notes")).is_err());
    assert!(
      FileAccess::read_only(root.path(), 8)
        .unwrap()
        .write("notes", b"x")
        .is_err()
    );
  }

  #[cfg(unix)]
  #[test]
  fn links_cannot_escape_the_granted_directory() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), b"secret").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("outside")).unwrap();
    let files = FileAccess::read_write(root.path(), 64).unwrap();
    assert!(files.read("outside/secret").is_err());
    assert!(files.write("outside/secret", b"changed").is_err());
    assert_eq!(std::fs::read(outside.path().join("secret")).unwrap(), b"secret");
  }
}
