use anyhow::{Context, Result, ensure};
use std::{
  fs, io,
  path::{Component, Path, PathBuf},
};

/// 创建完整应用更新包，保留 Framework 相对符号链接。
pub fn create_archive(bundle: &Path, output: &Path) -> Result<()> {
  if bundle.join("lintel-bundle.json").is_file() {
    let info = lintel_build::validate_windows_bundle(bundle)?;
    return create_windows_archive(bundle, &info, output);
  }
  lintel_build::verify_signature(bundle)?;
  let bundle = bundle.canonicalize()?;
  write_archive(&bundle, output)
}

pub(crate) fn create_windows_archive(
  bundle: &Path,
  expected: &lintel_build::WindowsBundleInfo,
  output: &Path,
) -> Result<()> {
  let root = tempfile::tempdir()?;
  let snapshot = lintel_build::copy_windows_bundle_verified(bundle, root.path(), expected)?;
  write_archive(&snapshot, output)
}

fn write_archive(bundle: &Path, output: &Path) -> Result<()> {
  let file = fs::OpenOptions::new().write(true).create_new(true).open(output)?;
  let result = (|| {
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    archive.follow_symlinks(false);
    archive.append_dir_all(bundle.file_name().context("app has no name")?, &bundle)?;
    archive.into_inner()?.finish()?.sync_all()?;
    Ok(())
  })();
  if result.is_err() {
    let _ = fs::remove_file(output);
  }
  result
}

pub fn extract(archive: &Path, root: &Path) -> Result<PathBuf> {
  extract_layout(archive, root, false)
}

pub(crate) fn extract_windows(archive: &Path, root: &Path) -> Result<PathBuf> {
  extract_layout(archive, root, true)
}

fn extract_layout(archive: &Path, root: &Path, windows: bool) -> Result<PathBuf> {
  extract_layout_reader(fs::File::open(archive)?, root, windows)
}

pub(crate) fn extract_windows_reader(reader: impl io::Read, root: &Path) -> Result<PathBuf> {
  extract_layout_reader(reader, root, true)
}

fn extract_layout_reader(reader: impl io::Read, root: &Path, windows: bool) -> Result<PathBuf> {
  let decoder = flate2::read::GzDecoder::new(reader);
  let mut archive = tar::Archive::new(HeaderGuard::new(decoder));
  let mut app_name = None;
  let mut total = 0u64;
  let mut count = 0usize;
  let mut windows_names = std::collections::BTreeSet::new();
  for entry in archive.entries()? {
    let mut entry = entry?;
    let path = entry.path()?.into_owned();
    let parts = safe_parts(&path)?;
    let first = parts.first().context("empty archive path")?;
    if windows {
      ensure!(
        parts.len() <= 2,
        "Windows update archives contain one flat portable directory"
      );
      for part in &parts {
        lintel_build::validate_windows_filename(part.to_str().context("Windows archive names must be Unicode")?)?;
      }
      let name = parts
        .iter()
        .map(|part| {
          part
            .to_str()
            .context("Windows archive path is not Unicode")
            .map(str::to_ascii_lowercase)
        })
        .collect::<Result<Vec<_>>>()?
        .join("\\");
      ensure!(
        windows_names.insert(name),
        "case-colliding/duplicate Windows archive entry"
      );
    } else {
      ensure!(
        Path::new(first).extension().is_some_and(|ext| ext == "app"),
        "update archive root must be one .app"
      );
    }
    if let Some(name) = &app_name {
      ensure!(name == first, "update archive contains multiple applications");
    } else {
      app_name = Some(first.clone());
    }
    count += 1;
    ensure!(count <= 100000, "update archive contains too many entries");
    total = total.checked_add(entry.size()).context("archive size overflow")?;
    ensure!(total <= 8 * 1024 * 1024 * 1024, "unpacked update exceeds 8 GiB");
    let destination = root.join(&path);
    let mut parent = root.to_path_buf();
    for part in &parts[..parts.len() - 1] {
      parent.push(part);
      if parent.exists() || parent.is_symlink() {
        ensure!(
          fs::symlink_metadata(&parent)?.is_dir(),
          "archive entry traverses a symlink or file"
        );
      } else {
        fs::create_dir(&parent)?;
      }
    }
    let kind = entry.header().entry_type();
    if kind.is_dir() {
      if destination.exists() {
        ensure!(
          fs::symlink_metadata(&destination)?.is_dir(),
          "duplicate archive entry type"
        );
      } else {
        fs::create_dir(&destination)?;
      }
    } else if kind.is_file() {
      let mut file = fs::OpenOptions::new().create_new(true).write(true).open(&destination)?;
      io::copy(&mut entry, &mut file)?;
      #[cfg(unix)]
      {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&destination, fs::Permissions::from_mode(entry.header().mode()? & 0o777))?;
      }
    } else if kind.is_symlink() {
      ensure!(!windows, "Windows update symlinks/reparse points are prohibited");
      let target = entry.link_name()?.context("symlink has no target")?.into_owned();
      ensure!(!target.is_absolute(), "absolute update symlink");
      let mut depth = parts.len() - 1;
      for part in target.components() {
        match part {
          Component::Normal(_) => depth += 1,
          Component::CurDir => {}
          Component::ParentDir => {
            ensure!(depth > 1, "update symlink escapes application");
            depth -= 1;
          }
          _ => anyhow::bail!("invalid update symlink"),
        }
      }
      #[cfg(unix)]
      std::os::unix::fs::symlink(target, destination)?;
      #[cfg(not(unix))]
      anyhow::bail!("symlink archives are unsupported on this platform");
    } else {
      anyhow::bail!("unsupported archive entry type (hard links and special files are prohibited)");
    }
  }
  Ok(root.join(app_name.context("update archive is empty")?))
}

fn safe_parts(path: &Path) -> Result<Vec<std::ffi::OsString>> {
  path
    .components()
    .map(|part| match part {
      Component::Normal(name) => Ok(name.to_owned()),
      _ => anyhow::bail!("unsafe update archive path {}", path.display()),
    })
    .collect()
}

// tar 会在交出条目前读取 GNU/PAX 元数据，因此必须在解析器之前限制这些分配。
struct HeaderGuard<R> {
  input: R,
  header: [u8; 512],
  header_position: usize,
  body_remaining: u64,
  declared_total: u64,
  metadata_total: u64,
  headers: usize,
}

impl<R: io::Read> HeaderGuard<R> {
  fn new(input: R) -> Self {
    Self {
      input,
      header: [0; 512],
      header_position: 512,
      body_remaining: 0,
      declared_total: 0,
      metadata_total: 0,
      headers: 0,
    }
  }
  fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
  }
}

impl<R: io::Read> io::Read for HeaderGuard<R> {
  fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
    if buffer.is_empty() {
      return Ok(0);
    }
    if self.header_position < 512 {
      let length = buffer.len().min(512 - self.header_position);
      buffer[..length].copy_from_slice(&self.header[self.header_position..self.header_position + length]);
      self.header_position += length;
      return Ok(length);
    }
    if self.body_remaining > 0 {
      let length = buffer.len().min(self.body_remaining.min(usize::MAX as u64) as usize);
      let read = self.input.read(&mut buffer[..length])?;
      if read == 0 {
        return Err(Self::invalid("truncated update archive entry"));
      }
      self.body_remaining -= read as u64;
      return Ok(read);
    }
    let first = self.input.read(&mut self.header[..1])?;
    if first == 0 {
      return Ok(0);
    }
    self.input.read_exact(&mut self.header[1..])?;
    self.headers += 1;
    if self.headers > 200000 {
      return Err(Self::invalid("too many raw archive headers"));
    }
    if self.header.iter().any(|byte| *byte != 0) {
      // 发布器只生成 GNU tar。拒绝会覆盖原始 size 或另读稀疏头的格式，
      // 保证此处与后续 tar 解析器对每个条目边界的理解一致。
      if !matches!(self.header[156], 0 | b'0' | b'5' | b'2' | b'L' | b'K') {
        return Err(Self::invalid(
          "unsupported update archive encoding (PAX and sparse entries are prohibited)",
        ));
      }
      let header = tar::Header::from_byte_slice(&self.header);
      let size = header.size()?;
      if size > 8 * 1024 * 1024 * 1024 {
        return Err(Self::invalid("archive entry exceeds 8 GiB"));
      }
      if matches!(self.header[156], b'L' | b'K' | b'x' | b'g') {
        self.metadata_total = self
          .metadata_total
          .checked_add(size)
          .ok_or_else(|| Self::invalid("metadata size overflow"))?;
        if size > 65536 || self.metadata_total > 16 * 1024 * 1024 {
          return Err(Self::invalid("archive extension metadata exceeds limit"));
        }
      }
      let padded = size
        .checked_add(511)
        .ok_or_else(|| Self::invalid("entry size overflow"))?
        / 512
        * 512;
      self.declared_total = self
        .declared_total
        .checked_add(padded + 512)
        .ok_or_else(|| Self::invalid("archive size overflow"))?;
      if self.declared_total > 8 * 1024 * 1024 * 1024 {
        return Err(Self::invalid("decompressed archive exceeds 8 GiB"));
      }
      self.body_remaining = padded;
    }
    self.header_position = 0;
    self.read(buffer)
  }
}
