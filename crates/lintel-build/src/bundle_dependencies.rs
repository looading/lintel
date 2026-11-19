//! Verify the relocatable dependency graph of the staged macOS application.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result, bail, ensure};

use crate::macho::{self, Dependency, Image};

/// Check each process using its own executable path and each image's load-chain runpaths.
pub(crate) fn verify_bundle(
  contents: &Path,
  main: &Path,
  helpers: &[PathBuf],
  runtime_roots: &[PathBuf],
  cef_runtime_roots: &[PathBuf],
  required: &BTreeSet<String>,
) -> Result<()> {
  ensure!(!required.is_empty(), "the staged bundle has no required architectures");
  let canonical_contents = contents
    .canonicalize()
    .context("locate the staged bundle Contents directory")?;
  let main = staged_image(contents, &canonical_contents, main)?;
  let helpers = staged_images(contents, &canonical_contents, helpers)?;
  let runtime_roots = staged_images(contents, &canonical_contents, runtime_roots)?;
  let cef_runtime_roots = staged_images(contents, &canonical_contents, cef_runtime_roots)?;

  for architecture in required {
    let mut images = HashMap::new();
    for (index, executable) in std::iter::once(&main).chain(&helpers).enumerate() {
      let mut process = Process {
        contents: &canonical_contents,
        executable,
        architecture,
        images: &mut images,
        visiting: HashSet::new(),
        visited: HashSet::new(),
        reached: HashSet::new(),
      };
      let explicit = if index == 0 { runtime_roots.as_slice() } else { &[] };
      process
        .verify(explicit.iter().chain(&cef_runtime_roots))
        .with_context(|| format!("verify {architecture} Mach-O dependencies for {}", executable.display()))?;
    }
  }
  Ok(())
}

fn staged_images(contents: &Path, canonical_contents: &Path, paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
  paths
    .iter()
    .map(|path| staged_image(contents, canonical_contents, path))
    .collect()
}

fn staged_image(contents: &Path, canonical_contents: &Path, path: &Path) -> Result<PathBuf> {
  let relative = path
    .strip_prefix(contents)
    .with_context(|| format!("staged image is outside the bundle: {}", path.display()))?;
  bundle_file(canonical_contents, &canonical_contents.join(relative))?
    .with_context(|| format!("staged Mach-O image is missing: {}", path.display()))
}

struct Process<'a> {
  contents: &'a Path,
  executable: &'a Path,
  architecture: &'a str,
  images: &'a mut HashMap<PathBuf, Rc<Image>>,
  visiting: HashSet<PathBuf>,
  visited: HashSet<(PathBuf, Vec<Runpath>)>,
  reached: HashSet<PathBuf>,
}

impl Process<'_> {
  fn verify<'a>(&mut self, explicit: impl Iterator<Item = &'a PathBuf>) -> Result<()> {
    self.walk(self.executable, &[])?;
    let executable = self.image(self.executable)?;
    let runpaths = self.runpaths(self.executable, &executable, &[]);
    for path in explicit {
      // An image already loaded by a declared dependency retains that load chain.
      if !self.reached.contains(path) {
        self
          .walk(path, &runpaths)
          .with_context(|| format!("verify explicit runtime image {}", path.display()))?;
      }
    }
    Ok(())
  }

  fn image(&mut self, path: &Path) -> Result<Rc<Image>> {
    if let Some(image) = self.images.get(path) {
      return Ok(Rc::clone(image));
    }
    let image = Rc::new(
      macho::read(path, self.architecture)
        .with_context(|| format!("read {} Mach-O image {}", self.architecture, path.display()))?,
    );
    self.images.insert(path.to_path_buf(), Rc::clone(&image));
    Ok(image)
  }

  fn walk(&mut self, path: &Path, inherited: &[Runpath]) -> Result<()> {
    if self.visiting.contains(path) {
      return Ok(());
    }
    ensure!(
      self.visiting.len() < 256,
      "Mach-O dependency chain is too deep at {}",
      path.display()
    );
    let image = self.image(path)?;
    let runpaths = self.runpaths(path, &image, inherited);
    if !self.visited.insert((path.to_path_buf(), runpaths.clone())) {
      return Ok(());
    }
    self.visiting.insert(path.to_path_buf());
    self.reached.insert(path.to_path_buf());
    for dependency in &image.dependencies {
      let resolved = self.resolve(path, dependency, &runpaths).with_context(|| {
        format!(
          "resolve Mach-O dependency {:?} from {}",
          dependency.path,
          path.display()
        )
      })?;
      match resolved {
        Some(Resolved::Bundled(child)) => self.walk(&child, &runpaths)?,
        Some(Resolved::System) => {}
        None if dependency.weak => {}
        None => bail!(
          "missing required Mach-O dependency {:?} from {} (runpaths: {runpaths:?})",
          dependency.path,
          path.display()
        ),
      }
    }
    self.visiting.remove(path);
    Ok(())
  }

  fn runpaths(&self, loader: &Path, image: &Image, inherited: &[Runpath]) -> Vec<Runpath> {
    let mut runpaths = Vec::new();
    // dyld searches each image's LC_RPATH commands in order, starting at the requesting image.
    for runpath in image
      .rpaths
      .iter()
      .filter_map(|path| self.expand(loader, path))
      .chain(inherited.iter().cloned())
    {
      if !runpaths.contains(&runpath) {
        runpaths.push(runpath);
      }
    }
    runpaths
  }

  fn expand(&self, loader: &Path, path: &str) -> Option<Runpath> {
    if let Some(suffix) = at_suffix(path, "@loader_path") {
      return Some(Runpath::Relative(loader.parent()?.join(suffix)));
    }
    if let Some(suffix) = at_suffix(path, "@executable_path") {
      return Some(Runpath::Relative(self.executable.parent()?.join(suffix)));
    }
    let path = Path::new(path);
    path.is_absolute().then(|| Runpath::Absolute(path.to_path_buf()))
  }

  fn resolve(&self, loader: &Path, dependency: &Dependency, runpaths: &[Runpath]) -> Result<Option<Resolved>> {
    if let Some(tail) = dependency.path.strip_prefix("@rpath/") {
      let tail = tail.trim_start_matches('/');
      for runpath in runpaths {
        if let Some(found) = self.probe(runpath.join(tail))? {
          return Ok(Some(found));
        }
      }
      return Ok(None);
    }
    if let Some(path) = self.expand(loader, &dependency.path) {
      if matches!(&path, Runpath::Absolute(path) if !system_path(path)) {
        if dependency.weak && !path_on_disk(path.path())? {
          return Ok(None);
        }
        bail!(
          "non-system absolute install name is not relocatable: {}",
          dependency.path
        );
      }
      return self.probe(path);
    }
    bail!("unsupported non-relocatable install name: {}", dependency.path);
  }

  fn probe(&self, candidate: Runpath) -> Result<Option<Resolved>> {
    match candidate {
      Runpath::Relative(path) => Ok(bundle_file(self.contents, &path)?.map(Resolved::Bundled)),
      Runpath::Absolute(path) if system_path(&path) => Ok(Some(Resolved::System)),
      Runpath::Absolute(path) => {
        ensure!(
          !path_on_disk(&path)?,
          "absolute runpath selects an external or non-relocatable file: {}",
          path.display()
        );
        Ok(None)
      }
    }
  }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Runpath {
  /// Absolute on disk, but derived from the current loader or executable location.
  Relative(PathBuf),
  /// The Mach-O image embeds the absolute path literally.
  Absolute(PathBuf),
}

impl Runpath {
  fn path(&self) -> &Path {
    match self {
      Self::Relative(path) | Self::Absolute(path) => path,
    }
  }

  fn join(&self, suffix: &str) -> Self {
    match self {
      Self::Relative(path) => Self::Relative(path.join(suffix)),
      Self::Absolute(path) => Self::Absolute(path.join(suffix)),
    }
  }
}

enum Resolved {
  Bundled(PathBuf),
  System,
}

fn at_suffix<'a>(value: &'a str, token: &str) -> Option<&'a str> {
  let rest = value.strip_prefix(token)?;
  if rest.is_empty() {
    return Some(rest);
  }
  rest.strip_prefix('/').map(|suffix| suffix.trim_start_matches('/'))
}

fn system_path(path: &Path) -> bool {
  !path.components().any(|component| component == Component::ParentDir)
    && [
      "/usr/lib",
      "/System/Library",
      "/System/iOSSupport/usr/lib",
      "/System/iOSSupport/System/Library",
      "/System/DriverKit",
    ]
    .iter()
    .any(|root| {
      path
        .strip_prefix(root)
        .is_ok_and(|relative| !relative.as_os_str().is_empty())
    })
}

fn path_on_disk(path: &Path) -> Result<bool> {
  match fs::metadata(path) {
    Ok(metadata) => Ok(metadata.is_file()),
    Err(error) if missing(&error) => Ok(false),
    Err(error) => Err(error).with_context(|| format!("inspect Mach-O dependency candidate {}", path.display())),
  }
}

fn missing(error: &io::Error) -> bool {
  matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::NotADirectory)
}

fn bundle_file(contents: &Path, candidate: &Path) -> Result<Option<PathBuf>> {
  let path = match candidate.canonicalize() {
    Ok(path) => path,
    Err(error) if missing(&error) => return Ok(None),
    Err(error) => return Err(error).with_context(|| format!("resolve staged path {}", candidate.display())),
  };
  if !path_on_disk(&path)? {
    return Ok(None);
  }
  ensure!(
    path.starts_with(contents),
    "dependency path escapes the staged bundle: {} -> {}",
    candidate.display(),
    path.display()
  );
  verify_links(contents, candidate)?;
  Ok(Some(path))
}

fn verify_links(contents: &Path, candidate: &Path) -> Result<()> {
  let relative = candidate
    .strip_prefix(contents)
    .context("dependency candidate starts outside the staged bundle")?;
  let mut parts: VecDeque<_> = relative.components().map(|part| part.as_os_str().to_owned()).collect();
  let mut current = contents.to_path_buf();
  let mut traversed = 0;
  while let Some(part) = parts.pop_front() {
    if part == ".." {
      ensure!(
        current != contents,
        "dependency path leaves the staged bundle: {}",
        candidate.display()
      );
      current.pop();
    } else if part != "." {
      current.push(part);
      if fs::symlink_metadata(&current)?.is_symlink() {
        traversed += 1;
        ensure!(traversed <= 40, "too many symlinks resolving {}", candidate.display());
        let target = fs::read_link(&current)?;
        ensure!(
          !target.is_absolute(),
          "staged dependency uses an absolute symlink: {}",
          current.display()
        );
        current.pop();
        for part in target.components().rev() {
          parts.push_front(part.as_os_str().to_owned());
        }
      }
    }
  }
  Ok(())
}
