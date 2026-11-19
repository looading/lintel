//! Inspect loader paths and dependencies for a single Mach-O architecture.

use std::{
  fs::File,
  path::Path,
  process::{Command, Stdio},
};

use anyhow::{Context, Result, ensure};

use crate::macos::run_tool;

#[derive(Debug, Default)]
pub(crate) struct Image {
  pub rpaths: Vec<String>,
  pub dependencies: Vec<Dependency>,
}

#[derive(Debug)]
pub(crate) struct Dependency {
  pub path: String,
  pub weak: bool,
}

pub(crate) fn read(path: &Path, architecture: &str) -> Result<Image> {
  let file = File::open(path).with_context(|| format!("open Mach-O image {}", path.display()))?;
  // otool interprets a filename ending in parentheses as archive-member syntax.
  // Pass the open file instead so CEF Helper names remain untouched.
  let output = run_tool(
    Command::new("/usr/bin/otool")
      .args(["-arch", architecture, "-l", "/dev/stdin"])
      .stdin(Stdio::from(file)),
  )?;
  let text = std::str::from_utf8(&output.stdout).context("Mach-O load commands are not UTF-8")?;
  parse(text).with_context(|| format!("read {architecture} Mach-O load commands in {}", path.display()))
}

#[derive(Clone, Copy)]
enum PathCommand {
  Rpath,
  Dependency { weak: bool },
}

fn parse(text: &str) -> Result<Image> {
  let mut image = Image::default();
  let mut pending = None;
  let mut saw_command = false;

  for line in text.lines().map(str::trim) {
    if line.starts_with("Load command ") {
      saw_command = true;
      ensure!(pending.is_none(), "load command has no path");
    } else if let Some(command) = line.strip_prefix("cmd ") {
      ensure!(pending.is_none(), "load command has no path");
      pending = match command {
        "LC_RPATH" => Some(PathCommand::Rpath),
        "LC_LOAD_WEAK_DYLIB" => Some(PathCommand::Dependency { weak: true }),
        "LC_LOAD_DYLIB" | "LC_LOAD_UPWARD_DYLIB" | "LC_REEXPORT_DYLIB" | "LC_LAZY_LOAD_DYLIB" => {
          Some(PathCommand::Dependency { weak: false })
        }
        _ => None,
      };
    } else if let Some(command) = pending {
      let field = match command {
        PathCommand::Rpath => "path ",
        PathCommand::Dependency { .. } => "name ",
      };
      let Some(value) = line.strip_prefix(field) else {
        continue;
      };
      let (path, offset) = value
        .rsplit_once(" (offset ")
        .context("load command path has no offset")?;
      ensure!(
        !path.is_empty()
          && offset
            .strip_suffix(')')
            .is_some_and(|offset| offset.parse::<u32>().is_ok()),
        "invalid load command path: {value:?}"
      );
      match command {
        PathCommand::Rpath => image.rpaths.push(path.to_owned()),
        PathCommand::Dependency { weak } => image.dependencies.push(Dependency {
          path: path.to_owned(),
          weak,
        }),
      }
      pending = None;
    }
  }
  ensure!(
    saw_command,
    "otool reported no Mach-O load commands for the requested architecture"
  );
  ensure!(pending.is_none(), "load command has no path");
  Ok(image)
}

#[cfg(test)]
mod tests {
  use super::parse;

  #[test]
  fn reads_rpaths_and_dylib_load_commands_without_splitting_paths() {
    let image = parse(
      "
Load command 0
         cmd LC_ID_DYLIB
        name @rpath/libSelf.dylib (offset 24)
Load command 1
         cmd LC_RPATH
     cmdsize 48
        path @loader_path/Frameworks with spaces (offset 12)
Load command 2
         cmd LC_LOAD_DYLIB
        name @rpath/Library (offset text).framework/Library (offset 24)
Load command 3
         cmd LC_LOAD_WEAK_DYLIB
        name @rpath/libWeak.dylib (offset 24)
Load command 4
         cmd LC_LOAD_UPWARD_DYLIB
        name @rpath/libUpward.dylib (offset 24)
Load command 5
         cmd LC_REEXPORT_DYLIB
        name @rpath/libReexport.dylib (offset 24)
Load command 6
         cmd LC_LAZY_LOAD_DYLIB
        name @rpath/libLazy.dylib (offset 24)
",
    )
    .unwrap();

    assert_eq!(image.rpaths, ["@loader_path/Frameworks with spaces"]);
    let dependencies: Vec<_> = image
      .dependencies
      .iter()
      .map(|dependency| (dependency.path.as_str(), dependency.weak))
      .collect();
    assert_eq!(
      dependencies,
      [
        ("@rpath/Library (offset text).framework/Library", false),
        ("@rpath/libWeak.dylib", true),
        ("@rpath/libUpward.dylib", false),
        ("@rpath/libReexport.dylib", false),
        ("@rpath/libLazy.dylib", false),
      ]
    );
  }

  #[test]
  fn rejects_missing_and_malformed_paths_on_known_commands() {
    assert!(parse("").is_err());
    assert!(parse("Load command 0\ncmd LC_RPATH\ncmdsize 48\nLoad command 1").is_err());
    assert!(parse("Load command 0\ncmd LC_LOAD_DYLIB\nname @rpath/libBad.dylib (offset xyz)").is_err());
  }
}
