//! Link and bundle native libraries from the application's local registry.

use std::ffi::OsString;

use anyhow::Result;

use crate::third_party::Library;

pub(crate) fn rustc_args(libraries: &[Library], target: Option<&str>) -> Result<Vec<OsString>> {
  if libraries.is_empty() {
    return Ok(Vec::new());
  }
  if let Some(target) = target
    && target.contains("-windows-")
  {
    return crate::windows::rustc_args(libraries, target);
  }
  #[cfg(target_os = "windows")]
  if target.is_none() {
    let architecture = if std::env::consts::ARCH == "x86" {
      "i686"
    } else {
      std::env::consts::ARCH
    };
    return crate::windows::rustc_args(libraries, &format!("{architecture}-pc-windows-msvc"));
  }
  #[cfg(target_os = "macos")]
  return darwin::rustc_args(libraries, target);
  #[cfg(not(target_os = "macos"))]
  {
    let _ = target;
    anyhow::bail!("managed native libraries require a supported macOS or Windows MSVC target");
  }
}

#[cfg(target_os = "macos")]
pub(crate) use darwin::{bundle, validate_input};

#[cfg(target_os = "macos")]
mod darwin {
  use std::{
    collections::BTreeSet,
    fs, io,
    path::{Component, Path, PathBuf},
    process::Command,
  };

  use anyhow::{Context, ensure};

  use super::{Library, OsString, Result};
  use crate::{
    macos::{binary_architectures, require_architectures, run_tool},
    third_party::LibraryKind,
  };

  const BUNDLE_RPATHS: [&str; 2] = ["@loader_path/../Frameworks", "@loader_path/../../../../Frameworks"];

  pub(crate) fn validate_input(library: &Library) -> Result<()> {
    let binary = library_binary(library);
    let architectures = binary_architectures(&binary)?;
    if library.kind != LibraryKind::Static {
      runtime_install(library, &architectures)?;
    }
    Ok(())
  }

  pub(super) fn rustc_args(libraries: &[Library], target: Option<&str>) -> Result<Vec<OsString>> {
    let architecture = target_architecture(target)?;
    let required = BTreeSet::from([architecture.to_owned()]);
    let mut args = Vec::new();
    let mut search_paths = BTreeSet::new();
    let mut link_names = BTreeSet::new();
    let mut install_names = BTreeSet::new();
    let mut needs_rpath = false;
    for library in libraries {
      let parent = library
        .path
        .parent()
        .with_context(|| format!("library {} has no parent directory", library.name))?;
      let (search_kind, link_kind, conventional_name) = match library.kind {
        LibraryKind::Static => ("native", "static", format!("lib{}.a", library.link_name)),
        LibraryKind::Dylib => ("native", "dylib", format!("lib{}.dylib", library.link_name)),
        LibraryKind::Framework => ("framework", "framework", format!("{}.framework", library.link_name)),
      };
      let conventional = parent.join(&conventional_name);
      ensure!(
        conventional.canonicalize().ok().as_deref() == Some(library.path.canonicalize()?.as_path()),
        "library {} must be linkable as {} in {}",
        library.name,
        conventional_name,
        parent.display()
      );
      if library.kind == LibraryKind::Static {
        require_architectures(&library.path, &required)?;
      } else {
        let install = runtime_install(library, &required)?;
        ensure!(
          install_names.insert(install.id.clone()),
          "duplicate Mach-O install name {}",
          install.id
        );
        needs_rpath = true;
      }
      let mut search = OsString::from(format!("{search_kind}="));
      search.push(parent.as_os_str());
      if search_paths.insert(search.clone()) {
        args.extend([OsString::from("-L"), search]);
      }
      let link = format!("{link_kind}={}", library.link_name);
      ensure!(link_names.insert(link.clone()), "duplicate native link target {link}");
      args.extend([OsString::from("-l"), OsString::from(link)]);
    }
    if needs_rpath {
      for path in BUNDLE_RPATHS {
        args.extend([
          OsString::from("-C"),
          OsString::from(format!("link-arg=-Wl,-rpath,{path}")),
        ]);
      }
    }
    Ok(args)
  }

  fn target_architecture(target: Option<&str>) -> Result<&str> {
    let arch = if let Some(target) = target {
      target
        .strip_suffix("-apple-darwin")
        .with_context(|| format!("managed native libraries do not support target {target}"))?
    } else {
      std::env::consts::ARCH
    };
    match arch {
      "aarch64" | "arm64" => Ok("arm64"),
      "x86_64" => Ok("x86_64"),
      _ => anyhow::bail!("unsupported macOS architecture {arch}"),
    }
  }

  struct RuntimeInstall {
    id: String,
    relative: PathBuf,
  }

  fn runtime_install(library: &Library, required: &BTreeSet<String>) -> Result<RuntimeInstall> {
    let framework = library.kind == LibraryKind::Framework;
    let binary = library_binary(library);
    require_architectures(&binary, required)?;
    let mut install_id = None;
    for architecture in required {
      let output = run_tool(
        Command::new("/usr/bin/otool")
          .args(["-arch", architecture, "-D"])
          .arg(&binary),
      )?;
      let mut lines = std::str::from_utf8(&output.stdout)?
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| !line.is_empty());
      let id = lines
        .next()
        .with_context(|| format!("missing Mach-O install name in {}", binary.display()))?;
      ensure!(
        lines.next().is_none(),
        "expected one Mach-O install name in {}",
        binary.display()
      );
      if let Some(prior) = &install_id {
        ensure!(prior == id, "inconsistent Mach-O install names in {}", binary.display());
      } else {
        install_id = Some(id.to_owned());
      }
    }
    let id = install_id.context("missing architecture for native library")?;
    // The official Mojo image identifies itself with this relative LC_ID_DYLIB.
    // It is loaded through an absolute bundle path, not linked through that ID.
    // Keep its trusted bytes intact; actual LC_LOAD_* dependencies are still
    // checked independently by the staged bundle dependency graph.
    let mojo_self_id = library.kind == LibraryKind::Dylib
      && library.name.eq_ignore_ascii_case("mojo-ipc")
      && library.link_name == "mojo_rs_native"
      && library
        .path
        .file_name()
        .is_some_and(|name| name == "libmojo_rs_native.dylib")
      && id == "./libmojo_rs_native.dylib";
    let relative = if mojo_self_id {
      "libmojo_rs_native.dylib"
    } else {
      id.strip_prefix("@rpath/").with_context(|| {
        format!(
          "library {} has nonportable install name {id:?}; expected @rpath",
          library.name
        )
      })?
    };
    let relative = PathBuf::from(relative);
    ensure!(
      is_relative_path(&relative),
      "invalid install name {id:?} for {}",
      library.name
    );

    if framework {
      let framework_name = format!("{}.framework", library.link_name);
      let components: Vec<_> = relative.components().map(|part| part.as_os_str()).collect();
      let framework_dir = library.path.canonicalize()?;
      let from_install_name = relative
        .strip_prefix(&framework_name)
        .with_context(|| format!("wrong framework in install name {id:?}"))?;
      let versioned = components.len() == 4 && components[1] == "Versions";
      ensure!(
        library
          .path
          .file_name()
          .is_some_and(|name| name == framework_name.as_str())
          && (components.len() == 2 || versioned)
          && relative
            .file_name()
            .is_some_and(|name| name == library.link_name.as_str()),
        "install name {id:?} does not name the binary in {framework_name}"
      );
      let resolved = framework_dir
        .join(from_install_name)
        .canonicalize()
        .with_context(|| format!("resolve framework install name {id:?} in {}", framework_dir.display()))?;
      ensure!(
        resolved.starts_with(&framework_dir) && resolved == binary.canonicalize()?,
        "framework install name {id:?} does not resolve to {}",
        binary.display()
      );
    } else {
      ensure!(
        relative.components().count() == 1
          && relative
            .to_str()
            .is_some_and(|name| name.starts_with("lib") && name.ends_with(".dylib")),
        "invalid dylib install name {id:?}; expected @rpath/lib*.dylib"
      );
    }
    Ok(RuntimeInstall { id, relative })
  }

  fn library_binary(library: &Library) -> PathBuf {
    match library.kind {
      LibraryKind::Framework => library.path.join(&library.link_name),
      LibraryKind::Static | LibraryKind::Dylib => library.path.clone(),
    }
  }

  fn is_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|part| matches!(part, Component::Normal(_)))
  }

  pub(crate) fn bundle(libraries: &[Library], frameworks: &Path, required: &BTreeSet<String>) -> Result<Vec<PathBuf>> {
    let mut installed = Vec::new();
    for library in libraries.iter().filter(|library| library.kind != LibraryKind::Static) {
      let install = runtime_install(library, required)?;
      let bundle_name = install.relative.components().next().context("empty install name")?;
      let destination = frameworks.join(bundle_name.as_os_str());
      match fs::symlink_metadata(&destination) {
        Ok(_) => anyhow::bail!("library {} collides with {}", library.name, destination.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("inspect {}", destination.display())),
      }
      fs::create_dir_all(frameworks)?;
      if library.kind == LibraryKind::Framework {
        run_tool(
          Command::new("/usr/bin/ditto")
            .arg(library.path.canonicalize()?)
            .arg(&destination),
        )?;
        let canonical = destination.canonicalize()?;
        validate_framework_links(&canonical, &canonical)?;
      } else {
        fs::copy(&library.path, &destination).with_context(|| format!("copy library {}", library.name))?;
      }
      if crate::service_processes::is_mojo_runtime(library) {
        let contents = frameworks
          .parent()
          .context("bundle Frameworks has no Contents directory")?;
        crate::service_processes::copy_mojo_notices(library, &contents.join("Resources/licenses/mojo-ipc"), false)?;
      }
      installed.push(frameworks.join(install.relative));
    }
    Ok(installed)
  }

  fn validate_framework_links(root: &Path, directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
      let entry = entry?;
      let file_type = entry.file_type()?;
      if file_type.is_symlink() {
        let target = entry
          .path()
          .canonicalize()
          .with_context(|| format!("resolve framework symlink {}", entry.path().display()))?;
        ensure!(
          target.starts_with(root),
          "framework symlink {} escapes its bundle",
          entry.path().display()
        );
      } else if file_type.is_dir() {
        validate_framework_links(root, &entry.path())?;
      }
    }
    Ok(())
  }
}
