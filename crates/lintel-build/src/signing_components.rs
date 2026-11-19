//! Discover the nested code that must be signed before the enclosing application.

use std::collections::{BTreeSet, VecDeque};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use plist::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComponentKind {
  MainApp,
  HelperApp,
  Framework,
  Executable,
  MachO,
}

#[derive(Debug)]
pub(crate) struct Component {
  pub path: PathBuf,
  pub kind: ComponentKind,
  pub bundle_version: Option<String>,
}

struct LocatedComponent {
  component: Component,
  scope: PathBuf,
}

#[derive(Default)]
struct Tree {
  directories: Vec<PathBuf>,
  code: BTreeSet<PathBuf>,
  executables: BTreeSet<PathBuf>,
}

/// Validate the complete bundle and return a deterministic signing order with the main app last.
pub(crate) fn components(bundle: &Path) -> Result<Vec<Component>> {
  let bundle: PathBuf = bundle.components().collect();
  ensure!(
    fs::symlink_metadata(&bundle)?.is_dir(),
    "application bundle is not a real directory: {}",
    bundle.display()
  );
  let bundle = bundle.canonicalize().context("locate the application bundle to sign")?;
  let mut tree = Tree::default();
  scan(&bundle, &bundle, &mut tree)?;

  let mut result = Vec::new();
  let mut main_images = BTreeSet::new();
  for directory in &tree.directories {
    if directory == &bundle {
      let executable = app_executable(directory)?.context("the main application has no CFBundleExecutable")?;
      add_container(
        &mut result,
        &mut main_images,
        &tree.code,
        LocatedComponent {
          scope: directory.clone(),
          component: Component {
            path: directory.clone(),
            kind: ComponentKind::MainApp,
            bundle_version: None,
          },
        },
        executable,
      )?;
    } else {
      match directory.extension().and_then(|extension| extension.to_str()) {
        Some("app" | "xpc") => {
          if let Some(executable) = app_executable(directory)? {
            add_container(
              &mut result,
              &mut main_images,
              &tree.code,
              LocatedComponent {
                scope: directory.clone(),
                component: Component {
                  path: directory.clone(),
                  kind: ComponentKind::HelperApp,
                  bundle_version: None,
                },
              },
              executable,
            )?;
          }
        }
        Some("framework") => framework_components(directory, &tree.code, &mut result, &mut main_images)?,
        _ => {}
      }
    }
  }
  for path in tree.code.difference(&main_images) {
    result.push(LocatedComponent {
      scope: path.clone(),
      component: Component {
        path: path.clone(),
        kind: if tree.executables.contains(path) {
          ComponentKind::Executable
        } else {
          ComponentKind::MachO
        },
        bundle_version: None,
      },
    });
  }
  result.sort_by(|left, right| {
    right
      .scope
      .components()
      .count()
      .cmp(&left.scope.components().count())
      .then_with(|| left.scope.cmp(&right.scope))
  });
  Ok(result.into_iter().map(|entry| entry.component).collect())
}

fn scan(bundle: &Path, path: &Path, tree: &mut Tree) -> Result<()> {
  let metadata = fs::symlink_metadata(path).with_context(|| format!("inspect signing input {}", path.display()))?;
  if metadata.is_dir() {
    tree.directories.push(path.to_path_buf());
    let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
      scan(bundle, &entry.path(), tree)?;
    }
  } else if metadata.is_file() {
    let kind = image_kind(path).with_context(|| format!("inspect Mach-O code in {}", path.display()))?;
    if kind != ImageKind::NotCode {
      tree.code.insert(path.to_path_buf());
      if kind == ImageKind::Executable {
        tree.executables.insert(path.to_path_buf());
      }
    }
  } else if metadata.is_symlink() {
    validate_link(bundle, path)?;
  } else {
    anyhow::bail!("unsupported file type in application bundle: {}", path.display());
  }
  Ok(())
}

fn app_executable(bundle: &Path) -> Result<Option<PathBuf>> {
  let contents = bundle.join("Contents");
  let Some(name) = executable_name(&contents.join("Info.plist"))? else {
    return Ok(None);
  };
  Ok(Some(contents.join("MacOS").join(name)))
}

fn executable_name(plist: &Path) -> Result<Option<String>> {
  if !plist.exists() {
    return Ok(None);
  }
  let info = Value::from_file(plist).with_context(|| format!("read code bundle metadata {}", plist.display()))?;
  let dictionary = info
    .as_dictionary()
    .with_context(|| format!("code bundle metadata is not a dictionary: {}", plist.display()))?;
  let Some(value) = dictionary.get("CFBundleExecutable") else {
    return Ok(None);
  };
  let name = value
    .as_string()
    .with_context(|| format!("CFBundleExecutable is not a string in {}", plist.display()))?;
  ensure!(
    !name.is_empty()
      && name != "."
      && name != ".."
      && !name.contains(['/', '\\'])
      && !name.chars().any(char::is_control),
    "invalid CFBundleExecutable in {}",
    plist.display()
  );
  Ok(Some(name.to_owned()))
}

fn framework_components(
  framework: &Path,
  code: &BTreeSet<PathBuf>,
  result: &mut Vec<LocatedComponent>,
  mains: &mut BTreeSet<PathBuf>,
) -> Result<()> {
  let stem = framework
    .file_stem()
    .and_then(|name| name.to_str())
    .context("framework has no UTF-8 name")?;
  let versions = framework.join("Versions");
  if fs::symlink_metadata(&versions).is_ok_and(|metadata| metadata.is_dir()) {
    let mut entries = fs::read_dir(&versions)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
      if entry.file_type()?.is_dir() {
        let version = entry
          .file_name()
          .into_string()
          .map_err(|_| anyhow::anyhow!("framework version is not UTF-8"))?;
        let scope = entry.path();
        let name = executable_name(&scope.join("Resources/Info.plist"))?.unwrap_or_else(|| stem.to_owned());
        let executable = scope.join(name);
        if executable.exists() {
          add_container(
            result,
            mains,
            code,
            LocatedComponent {
              scope,
              component: Component {
                path: framework.to_path_buf(),
                kind: ComponentKind::Framework,
                bundle_version: Some(version),
              },
            },
            executable,
          )?;
        }
      }
    }
  } else {
    let name = executable_name(&framework.join("Resources/Info.plist"))?.unwrap_or_else(|| stem.to_owned());
    let executable = framework.join(name);
    if executable.exists() {
      add_container(
        result,
        mains,
        code,
        LocatedComponent {
          scope: framework.to_path_buf(),
          component: Component {
            path: framework.to_path_buf(),
            kind: ComponentKind::Framework,
            bundle_version: None,
          },
        },
        executable,
      )?;
    }
  }
  Ok(())
}

fn add_container(
  result: &mut Vec<LocatedComponent>,
  mains: &mut BTreeSet<PathBuf>,
  code: &BTreeSet<PathBuf>,
  container: LocatedComponent,
  executable: PathBuf,
) -> Result<()> {
  let scope = &container.scope;
  let executable = executable
    .canonicalize()
    .with_context(|| format!("locate main executable in {}", scope.display()))?;
  ensure!(
    executable.starts_with(scope),
    "main executable leaves its code bundle: {}",
    scope.display()
  );
  ensure!(
    code.contains(&executable),
    "code bundle main executable is not a loadable Mach-O image: {}",
    executable.display()
  );
  ensure!(
    mains.insert(executable.clone()),
    "multiple code bundles share the main executable {}",
    executable.display()
  );
  result.push(container);
  Ok(())
}

fn validate_link(bundle: &Path, link: &Path) -> Result<()> {
  let mut pending = VecDeque::from([link.strip_prefix(bundle)?.as_os_str().to_owned()]);
  let mut current = bundle.to_path_buf();
  let mut traversed = 0;
  while let Some(path) = pending.pop_front() {
    let components: Vec<_> = Path::new(&path)
      .components()
      .map(|part| part.as_os_str().to_owned())
      .collect();
    for (index, part) in components.iter().enumerate() {
      if part == ".." {
        ensure!(
          current != bundle,
          "symlink escapes the application bundle: {}",
          link.display()
        );
        current.pop();
      } else if part != "." {
        current.push(part);
        if fs::symlink_metadata(&current)?.is_symlink() {
          traversed += 1;
          ensure!(traversed <= 40, "cyclic or excessive symlinks in {}", link.display());
          let target = fs::read_link(&current)?;
          ensure!(
            !target.is_absolute(),
            "absolute symlink in application bundle: {}",
            current.display()
          );
          current.pop();
          let remaining: PathBuf = components[index + 1..].iter().collect();
          pending.push_front(remaining.into_os_string());
          pending.push_front(target.into_os_string());
          break;
        }
      }
    }
  }
  let metadata = fs::metadata(&current)?;
  ensure!(
    metadata.is_file() || metadata.is_dir(),
    "symlink targets an unsupported file type: {}",
    link.display()
  );
  Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageKind {
  Executable,
  OtherCode,
  NotCode,
}

pub(crate) fn require_executable_image(path: &Path) -> Result<()> {
  ensure!(
    image_kind(path)? == ImageKind::Executable,
    "standalone helper must be a Mach-O executable: {}",
    path.display()
  );
  Ok(())
}

fn image_kind(path: &Path) -> Result<ImageKind> {
  let mut file = File::open(path).with_context(|| format!("inspect code header {}", path.display()))?;
  let length = file.metadata()?.len();
  if length < 4 {
    return Ok(ImageKind::NotCode);
  }
  let mut magic = [0; 4];
  file.read_exact(&mut magic)?;
  if let Some(code) = thin_code(&mut file, 0, length, magic)? {
    return Ok(code);
  }
  let (little, wide) = match magic {
    [0xca, 0xfe, 0xba, 0xbe] => (false, false),
    [0xbe, 0xba, 0xfe, 0xca] => (true, false),
    [0xca, 0xfe, 0xba, 0xbf] => (false, true),
    [0xbf, 0xba, 0xfe, 0xca] => (true, true),
    _ => return Ok(ImageKind::NotCode),
  };
  let mut count = [0; 4];
  if file.read_exact(&mut count).is_err() {
    return Ok(ImageKind::NotCode);
  }
  let count = number32(count, little) as u64;
  let entry_size = if wide { 32 } else { 20 };
  // CAFEBABE also identifies Java class files. Require a valid universal slice table.
  if count == 0 || count > 128 || 8 + count * entry_size > length {
    return Ok(ImageKind::NotCode);
  }
  let mut kind = None;
  for index in 0..count {
    file.seek(SeekFrom::Start(8 + index * entry_size + 8))?;
    let mut fields = [0; 16];
    let size = if wide { 16 } else { 8 };
    file.read_exact(&mut fields[..size])?;
    let (offset, size) = if wide {
      (
        number64(fields[..8].try_into()?, little),
        number64(fields[8..].try_into()?, little),
      )
    } else {
      (
        u64::from(number32(fields[..4].try_into()?, little)),
        u64::from(number32(fields[4..8].try_into()?, little)),
      )
    };
    if size < 4 || offset < 8 + count * entry_size || offset.checked_add(size).is_none_or(|end| end > length) {
      return Ok(ImageKind::NotCode);
    }
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut magic)?;
    let Some(code) = thin_code(&mut file, offset, size, magic)? else {
      return Ok(ImageKind::NotCode);
    };
    ensure!(
      kind.is_none_or(|previous| previous == code),
      "universal Mach-O slices have different executable file types"
    );
    kind = Some(code);
  }
  Ok(kind.unwrap_or(ImageKind::NotCode))
}

fn thin_code(file: &mut File, offset: u64, length: u64, magic: [u8; 4]) -> Result<Option<ImageKind>> {
  let (little, header_size) = match magic {
    [0xfe, 0xed, 0xfa, 0xce] => (false, 28),
    [0xce, 0xfa, 0xed, 0xfe] => (true, 28),
    [0xfe, 0xed, 0xfa, 0xcf] => (false, 32),
    [0xcf, 0xfa, 0xed, 0xfe] => (true, 32),
    _ => return Ok(None),
  };
  ensure!(length >= header_size, "truncated Mach-O header");
  file.seek(SeekFrom::Start(offset + 12))?;
  let mut kind = [0; 4];
  file.read_exact(&mut kind)?;
  Ok(Some(match number32(kind, little) {
    2 => ImageKind::Executable,
    6 | 7 | 8 => ImageKind::OtherCode,
    _ => ImageKind::NotCode,
  }))
}

fn number32(bytes: [u8; 4], little: bool) -> u32 {
  if little {
    u32::from_le_bytes(bytes)
  } else {
    u32::from_be_bytes(bytes)
  }
}

fn number64(bytes: [u8; 8], little: bool) -> u64 {
  if little {
    u64::from_le_bytes(bytes)
  } else {
    u64::from_be_bytes(bytes)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn thin(kind: u32, little: bool, wide: bool) -> Vec<u8> {
    let mut image = vec![0; if wide { 32 } else { 28 }];
    let magic: u32 = if wide { 0xfeedfacf } else { 0xfeedface };
    image[..4].copy_from_slice(&if little {
      magic.to_le_bytes()
    } else {
      magic.to_be_bytes()
    });
    image[12..16].copy_from_slice(&if little { kind.to_le_bytes() } else { kind.to_be_bytes() });
    image
  }

  fn fat(kinds: &[u32], little: bool, wide: bool) -> Vec<u8> {
    let entry_size = if wide { 32 } else { 20 };
    let header_size = 8 + kinds.len() * entry_size;
    let mut image = vec![0; header_size];
    let magic: u32 = if wide { 0xcafebabf } else { 0xcafebabe };
    image[..4].copy_from_slice(&if little {
      magic.to_le_bytes()
    } else {
      magic.to_be_bytes()
    });
    let count = kinds.len() as u32;
    image[4..8].copy_from_slice(&if little {
      count.to_le_bytes()
    } else {
      count.to_be_bytes()
    });
    for (index, kind) in kinds.iter().copied().enumerate() {
      let start = 8 + index * entry_size + 8;
      let offset = image.len() as u64;
      let slice = thin(kind, index % 2 == 0, true);
      if wide {
        let size = slice.len() as u64;
        image[start..start + 8].copy_from_slice(&if little {
          offset.to_le_bytes()
        } else {
          offset.to_be_bytes()
        });
        image[start + 8..start + 16].copy_from_slice(&if little { size.to_le_bytes() } else { size.to_be_bytes() });
      } else {
        let offset = offset as u32;
        let size = slice.len() as u32;
        image[start..start + 4].copy_from_slice(&if little {
          offset.to_le_bytes()
        } else {
          offset.to_be_bytes()
        });
        image[start + 4..start + 8].copy_from_slice(&if little { size.to_le_bytes() } else { size.to_be_bytes() });
      }
      image.extend(slice);
    }
    image
  }

  #[test]
  fn distinguishes_executable_and_library_headers_for_thin_and_universal_images() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("image");
    for little in [false, true] {
      for wide in [false, true] {
        for (kind, expected) in [
          (2, ImageKind::Executable),
          (6, ImageKind::OtherCode),
          (8, ImageKind::OtherCode),
          (1, ImageKind::NotCode),
        ] {
          fs::write(&path, thin(kind, little, wide)).unwrap();
          assert_eq!(image_kind(&path).unwrap(), expected);
          fs::write(&path, fat(&[kind, kind], little, wide)).unwrap();
          assert_eq!(image_kind(&path).unwrap(), expected);
        }
      }
    }
    fs::write(&path, b"!<arch>\nstatic archive").unwrap();
    assert_eq!(image_kind(&path).unwrap(), ImageKind::NotCode);
    fs::write(&path, fat(&[2, 6], false, false)).unwrap();
    assert!(image_kind(&path).is_err());
    fs::write(&path, [0xcf, 0xfa, 0xed, 0xfe]).unwrap();
    assert!(image_kind(&path).is_err());
  }

  #[test]
  fn discovers_standalone_processes_without_classifying_libraries_as_processes() {
    let directory = tempfile::tempdir().unwrap();
    let app = directory.path().join("Fixture.app");
    let binaries = app.join("Contents/MacOS");
    fs::create_dir_all(&binaries).unwrap();
    let info = plist::Dictionary::from_iter([("CFBundleExecutable".to_owned(), Value::String("Main".to_owned()))]);
    Value::Dictionary(info)
      .to_file_xml(app.join("Contents/Info.plist"))
      .unwrap();
    fs::write(binaries.join("Main"), thin(2, true, true)).unwrap();
    fs::write(binaries.join("worker"), thin(2, true, true)).unwrap();
    fs::write(binaries.join("universal-worker"), fat(&[2, 2], false, false)).unwrap();
    fs::write(binaries.join("libfixture.dylib"), thin(6, true, true)).unwrap();
    let components = components(&app).unwrap();
    let kind = |name: &str| {
      components
        .iter()
        .find(|component| component.path.file_name().unwrap() == name)
        .unwrap()
        .kind
    };
    assert_eq!(kind("Fixture.app"), ComponentKind::MainApp);
    assert_eq!(kind("worker"), ComponentKind::Executable);
    assert_eq!(kind("universal-worker"), ComponentKind::Executable);
    assert_eq!(kind("libfixture.dylib"), ComponentKind::MachO);
    assert!(
      !components
        .iter()
        .any(|component| component.path.file_name().unwrap() == "Main")
    );
    assert_eq!(components.last().unwrap().kind, ComponentKind::MainApp);
  }
}
