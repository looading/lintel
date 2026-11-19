//! Project-scoped registration of native libraries.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Linkage and packaging rules for a registered library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LibraryKind {
  Static,
  Dylib,
  Framework,
}

/// One active library and its absolute, content-addressed artifact path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
  pub name: String,
  pub kind: LibraryKind,
  pub path: PathBuf,
  pub link_name: String,
  pub version: Option<String>,
}

const REGISTRY_FILE: &str = "lintel.third-party.json";

#[cfg(target_os = "windows")]
pub use crate::windows::{
  add_windows_library as add_library, remove_windows_library as remove_library,
  update_windows_library as update_library, windows_libraries as libraries,
};
#[cfg(target_os = "macos")]
pub use macos::{add_library, libraries, remove_library, update_library};
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub use unsupported::{add_library, libraries, remove_library, update_library};

#[cfg(target_os = "macos")]
mod macos {
  use std::collections::BTreeSet;
  use std::ffi::CString;
  use std::fs::{self, File, OpenOptions};
  use std::io::{self, Write};
  use std::os::unix::ffi::OsStrExt;
  use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
  use std::path::{Component, Path, PathBuf};

  use anyhow::{Context, Result, bail, ensure};
  use serde::{Deserialize, Serialize};
  use sha2::{Digest, Sha256};

  use super::{Library, LibraryKind, REGISTRY_FILE};

  const STORAGE: &str = ".lintel/third-party";
  const SCHEMA: u32 = 1;

  #[derive(Serialize, Deserialize)]
  #[serde(deny_unknown_fields)]
  struct Registry {
    schema: u32,
    libraries: Vec<Record>,
  }

  #[derive(Serialize, Deserialize)]
  #[serde(deny_unknown_fields)]
  struct Record {
    name: String,
    kind: LibraryKind,
    artifact: String,
    sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
  }

  /// Read the active registry and verify each artifact against its recorded digest.
  pub fn libraries(project: &Path) -> Result<Vec<Library>> {
    let project = package_directory(project)?;
    let registry = read_registry(&project)?;
    if !registry.libraries.is_empty() {
      check_directory(&project.join(".lintel"))?;
      check_directory(&project.join(STORAGE))?;
    }
    registry
      .libraries
      .into_iter()
      .map(|entry| {
        let library = entry.library(&project)?;
        check_blob(&library.path, library.kind, &library.link_name, &entry.sha256)
          .with_context(|| format!("verify registered library {}", entry.name))?;
        Ok(library)
      })
      .collect()
  }

  /// Copy a new library into project storage and make it active.
  pub fn add_library(project: &Path, name: &str, source: &Path, version: Option<&str>) -> Result<Library> {
    register(project, name, source, version, false)
  }

  /// Replace an active entry while keeping the previous artifact available to in-flight builds.
  pub fn update_library(project: &Path, name: &str, source: &Path, version: Option<&str>) -> Result<Library> {
    register(project, name, source, version, true)
  }

  /// Remove an active entry from the registry while preserving immutable artifacts.
  pub fn remove_library(project: &Path, name: &str) -> Result<()> {
    validate_name(name)?;
    let project = package_directory(project)?;
    let _lock = lock_project(&project)?;
    let mut registry = read_registry(&project)?;
    let index = entry_index(&registry, name).with_context(|| format!("library {name:?} is not registered"))?;
    registry.libraries.remove(index);
    write_registry(&project, &registry)
  }

  fn register(project: &Path, name: &str, source: &Path, version: Option<&str>, update: bool) -> Result<Library> {
    validate_name(name)?;
    validate_version(version)?;
    let source: PathBuf = source.components().collect();
    let artifact = source
      .file_name()
      .and_then(|name| name.to_str())
      .context("library has no UTF-8 filename")?;
    let (kind, link_name) = artifact_info(artifact)?;
    check_artifact_type(&source, kind)?;
    let project = package_directory(project)?;
    let _lock = lock_project(&project)?;
    let mut registry = read_registry(&project)?;
    let existing = entry_index(&registry, name);
    match (update, existing) {
      (true, None) => bail!("library {name:?} is not registered"),
      (false, Some(_)) => bail!("library {name:?} is already registered"),
      _ => {}
    }
    for (index, entry) in registry.libraries.iter().enumerate() {
      if Some(index) != existing {
        ensure!(
          !entry.artifact.eq_ignore_ascii_case(artifact),
          "artifact basename {artifact:?} is already used by library {:?}",
          entry.name
        );
      }
    }

    let storage = project.join(STORAGE);
    let stage = tempfile::Builder::new().prefix(".stage-").tempdir_in(&storage)?;
    let blob = stage.path().join("blob");
    fs::create_dir(&blob)?;
    let staged = blob.join(artifact);
    copy_tree(&source, &staged).with_context(|| format!("stage library {}", source.display()))?;
    let digest = artifact_digest(&staged, kind, link_name)?;
    crate::native_libraries::validate_input(&Library {
      name: name.to_owned(),
      kind,
      path: staged,
      link_name: link_name.to_owned(),
      version: version.map(str::to_owned),
    })
    .with_context(|| format!("validate native library {name:?}"))?;
    let entry = Record {
      name: name.to_owned(),
      kind,
      artifact: artifact.to_owned(),
      sha256: digest,
      version: version.map(str::to_owned),
    };
    let library = entry.library(&project)?;
    let published = storage.join(&entry.sha256);
    match fs::symlink_metadata(&published) {
      Ok(_) => check_blob(&library.path, kind, link_name, &entry.sha256)?,
      Err(error) if error.kind() == io::ErrorKind::NotFound => publish_blob(&blob, &published)?,
      Err(error) => return Err(error).with_context(|| format!("inspect library storage {}", published.display())),
    }

    if let Some(index) = existing {
      registry.libraries[index] = entry;
    } else {
      registry.libraries.push(entry);
    }
    registry.libraries.sort_by(|left, right| left.name.cmp(&right.name));
    write_registry(&project, &registry)?;
    Ok(library)
  }

  impl Record {
    fn library(&self, project: &Path) -> Result<Library> {
      let (_, link_name) = artifact_info(&self.artifact)?;
      Ok(Library {
        name: self.name.clone(),
        kind: self.kind,
        path: project.join(STORAGE).join(&self.sha256).join(&self.artifact),
        link_name: link_name.to_owned(),
        version: self.version.clone(),
      })
    }
  }

  fn package_directory(project: &Path) -> Result<PathBuf> {
    let project = project
      .canonicalize()
      .with_context(|| format!("locate Cargo package {}", project.display()))?;
    ensure!(
      project.join("Cargo.toml").is_file(),
      "no Cargo.toml in {}",
      project.display()
    );
    Ok(project)
  }

  fn validate_name(name: &str) -> Result<()> {
    ensure!(
      valid_component(name),
      "invalid library name {name:?}; use ASCII letters, digits, dots, hyphens or underscores and start with a letter or digit"
    );
    Ok(())
  }

  fn validate_version(version: Option<&str>) -> Result<()> {
    if let Some(version) = version {
      ensure!(
        !version.trim().is_empty() && version.len() <= 128 && !version.chars().any(char::is_control),
        "invalid library version label"
      );
    }
    Ok(())
  }

  fn valid_component(value: &str) -> bool {
    value.len() <= 128
      && value.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
      && value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
  }

  fn artifact_info(artifact: &str) -> Result<(LibraryKind, &str)> {
    let (kind, link_name) = if let Some(name) = artifact.strip_suffix(".framework") {
      (LibraryKind::Framework, name)
    } else if let Some(name) = artifact
      .strip_prefix("lib")
      .and_then(|name| name.strip_suffix(".dylib"))
    {
      (LibraryKind::Dylib, name)
    } else if let Some(name) = artifact.strip_prefix("lib").and_then(|name| name.strip_suffix(".a")) {
      (LibraryKind::Static, name)
    } else {
      bail!("unsupported library {artifact:?}; expected lib*.a, lib*.dylib or *.framework");
    };
    ensure!(
      valid_component(link_name),
      "invalid linker name {link_name:?} in {artifact:?}"
    );
    Ok((kind, link_name))
  }

  fn entry_index(registry: &Registry, name: &str) -> Option<usize> {
    registry
      .libraries
      .iter()
      .position(|entry| entry.name.eq_ignore_ascii_case(name))
  }

  fn read_registry(project: &Path) -> Result<Registry> {
    let path = project.join(REGISTRY_FILE);
    let file = match OpenOptions::new()
      .read(true)
      .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
      .open(&path)
    {
      Ok(file) => file,
      Err(error) if error.kind() == io::ErrorKind::NotFound => {
        return Ok(Registry {
          schema: SCHEMA,
          libraries: Vec::new(),
        });
      }
      Err(error) => return Err(error).with_context(|| format!("read library registry {}", path.display())),
    };
    ensure!(
      file.metadata()?.is_file(),
      "library registry is not a regular file: {}",
      path.display()
    );
    let registry: Registry = serde_json::from_reader(file).with_context(|| format!("parse {}", path.display()))?;
    ensure!(
      registry.schema == SCHEMA,
      "unsupported library registry schema {}",
      registry.schema
    );
    let mut names = BTreeSet::new();
    let mut artifacts = BTreeSet::new();
    for entry in &registry.libraries {
      validate_name(&entry.name)?;
      validate_version(entry.version.as_deref())?;
      let (kind, _) = artifact_info(&entry.artifact)?;
      ensure!(
        kind == entry.kind,
        "library kind does not match artifact {:?}",
        entry.artifact
      );
      ensure!(
        entry.sha256.len() == 64
          && entry
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid SHA-256 for library {:?}",
        entry.name
      );
      ensure!(
        names.insert(entry.name.to_ascii_lowercase()),
        "duplicate library name {:?}",
        entry.name
      );
      ensure!(
        artifacts.insert(entry.artifact.to_ascii_lowercase()),
        "duplicate artifact basename {:?}",
        entry.artifact
      );
    }
    Ok(registry)
  }

  fn write_registry(project: &Path, registry: &Registry) -> Result<()> {
    let path = project.join(REGISTRY_FILE);
    let mut staged = tempfile::NamedTempFile::new_in(project).context("stage the library registry")?;
    serde_json::to_writer_pretty(staged.as_file_mut(), registry)?;
    staged.write_all(b"\n")?;
    staged.as_file().sync_all()?;
    staged
      .persist(&path)
      .with_context(|| format!("publish library registry {}", path.display()))?;
    Ok(())
  }

  fn check_directory(path: &Path) -> Result<()> {
    let metadata =
      fs::symlink_metadata(path).with_context(|| format!("inspect library directory {}", path.display()))?;
    ensure!(
      metadata.is_dir(),
      "library storage path is not a directory: {}",
      path.display()
    );
    Ok(())
  }

  fn lock_project(project: &Path) -> Result<File> {
    for directory in [project.join(".lintel"), project.join(STORAGE)] {
      match fs::create_dir(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => check_directory(&directory)?,
        Err(error) => return Err(error).with_context(|| format!("create library directory {}", directory.display())),
      }
    }
    let path = project.join(STORAGE).join(".registry.lock");
    let lock = OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .truncate(false)
      .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
      .open(&path)
      .with_context(|| format!("open library registry lock {}", path.display()))?;
    ensure!(
      lock.metadata()?.is_file(),
      "library registry lock is not a regular file: {}",
      path.display()
    );
    lock.lock().context("lock the project library registry")?;
    Ok(lock)
  }

  fn publish_blob(source: &Path, destination: &Path) -> Result<()> {
    let from = CString::new(source.as_os_str().as_bytes())?;
    let to = CString::new(destination.as_os_str().as_bytes())?;
    // SAFETY: Both NUL-terminated path buffers remain valid for the call.
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) } != 0 {
      return Err(io::Error::last_os_error())
        .with_context(|| format!("publish library artifact {}", destination.display()));
    }
    Ok(())
  }

  fn check_artifact_type(path: &Path, kind: LibraryKind) -> Result<()> {
    let metadata =
      fs::symlink_metadata(path).with_context(|| format!("inspect library artifact {}", path.display()))?;
    let valid = match kind {
      LibraryKind::Framework => metadata.is_dir(),
      LibraryKind::Static | LibraryKind::Dylib => metadata.is_file(),
    };
    ensure!(valid, "invalid artifact type for {}", path.display());
    Ok(())
  }

  fn regular_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
      .read(true)
      .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
      .open(path)?;
    ensure!(file.metadata()?.is_file(), "not a regular file: {}", path.display());
    Ok(file)
  }

  fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() {
      fs::create_dir(target)?;
      for entry in fs::read_dir(source)? {
        let entry = entry?;
        copy_tree(&entry.path(), &target.join(entry.file_name()))?;
      }
    } else if metadata.is_file() {
      let mut reader = regular_file(source)?;
      let mut writer = File::create_new(target)?;
      io::copy(&mut reader, &mut writer)?;
      writer.set_permissions(fs::Permissions::from_mode(metadata.permissions().mode() & 0o777))?;
    } else if metadata.is_symlink() {
      symlink(fs::read_link(source)?, target)?;
    } else {
      bail!("unsupported entry in library artifact: {}", source.display());
    }
    Ok(())
  }

  fn check_blob(path: &Path, kind: LibraryKind, link_name: &str, expected: &str) -> Result<()> {
    check_directory(path.parent().context("artifact path has no blob directory")?)?;
    ensure!(
      artifact_digest(path, kind, link_name)? == expected,
      "library artifact SHA-256 differs at {}",
      path.display()
    );
    Ok(())
  }

  fn artifact_digest(path: &Path, kind: LibraryKind, link_name: &str) -> Result<String> {
    check_artifact_type(path, kind)?;
    let root = path.canonicalize()?;
    let mut digest = Sha256::new();
    digest.update(b"lintel-library-v1\0");
    hash_field(
      &mut digest,
      path.file_name().context("artifact has no basename")?.as_bytes(),
    );
    hash_entry(&root, &root, &mut digest).with_context(|| format!("hash artifact {}", path.display()))?;
    if kind == LibraryKind::Framework {
      ensure!(
        root.join(link_name).is_file(),
        "framework has no binary named {link_name:?}: {}",
        path.display()
      );
    }
    Ok(format!("{:x}", digest.finalize()))
  }

  fn hash_field(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes);
  }

  fn hash_entry(root: &Path, path: &Path, digest: &mut Sha256) -> Result<()> {
    hash_field(digest, path.strip_prefix(root)?.as_os_str().as_bytes());
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() {
      digest.update(b"d");
      let mut entries = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
      entries.sort_by_key(fs::DirEntry::file_name);
      for entry in entries {
        hash_entry(root, &entry.path(), digest)?;
      }
    } else if metadata.is_file() {
      digest.update(b"f");
      let mut file = regular_file(path)?;
      let length = file.metadata()?.len();
      digest.update(length.to_le_bytes());
      ensure!(
        io::copy(&mut file, digest)? == length,
        "library changed while reading {}",
        path.display()
      );
    } else if metadata.is_symlink() {
      digest.update(b"l");
      let target = fs::read_link(path)?;
      validate_link(root, path, &target)?;
      hash_field(digest, target.as_os_str().as_bytes());
    } else {
      bail!("unsupported entry in library artifact: {}", path.display());
    }
    Ok(())
  }

  fn validate_link(root: &Path, path: &Path, target: &Path) -> Result<()> {
    let mut relative = path.strip_prefix(root)?.to_path_buf();
    relative.pop();
    for component in target.components() {
      let safe = match component {
        Component::Normal(part) => {
          relative.push(part);
          true
        }
        Component::CurDir => true,
        Component::ParentDir => relative.pop(),
        Component::RootDir | Component::Prefix(_) => false,
      };
      ensure!(safe, "framework symlink escapes the artifact: {}", path.display());
    }
    let resolved = path
      .canonicalize()
      .with_context(|| format!("resolve framework symlink {}", path.display()))?;
    ensure!(
      resolved.starts_with(root),
      "framework symlink escapes the artifact: {}",
      path.display()
    );
    Ok(())
  }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod unsupported {
  use std::path::Path;

  use anyhow::{Result, bail};

  use super::{Library, REGISTRY_FILE};

  pub fn libraries(project: &Path) -> Result<Vec<Library>> {
    match std::fs::symlink_metadata(project.join(REGISTRY_FILE)) {
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
      Err(error) => Err(error.into()),
      Ok(_) => bail!("registered Lintel libraries require macOS"),
    }
  }

  pub fn add_library(_project: &Path, _name: &str, _source: &Path, _version: Option<&str>) -> Result<Library> {
    bail!("registering Lintel libraries requires macOS")
  }

  pub fn update_library(_project: &Path, _name: &str, _source: &Path, _version: Option<&str>) -> Result<Library> {
    bail!("registering Lintel libraries requires macOS")
  }

  pub fn remove_library(_project: &Path, _name: &str) -> Result<()> {
    bail!("registering Lintel libraries requires macOS")
  }
}
