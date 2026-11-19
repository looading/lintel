use std::path::{Path, PathBuf};

use anyhow::Result;

/// The type of standalone Lintel project to generate.
#[derive(Debug, Clone, Copy)]
pub enum ProjectTemplate {
  Native,
  /// Native UI plus a typed Mojom protocol and separately built service executable.
  Multiprocess,
  Webview,
  /// CEF WebView UI calling a separately managed typed Mojo service executable.
  WebviewMultiprocess,
  /// Experimental Windows-only cdylib for the matching official CEF GUI bootstrap.
  /// Generation does not enable Windows CEF packaging or native execution acceptance.
  WindowsWebview,
}

impl ProjectTemplate {
  fn has_multiprocess(self) -> bool {
    matches!(self, Self::Multiprocess | Self::WebviewMultiprocess)
  }

  fn has_webview(self) -> bool {
    matches!(self, Self::Webview | Self::WebviewMultiprocess | Self::WindowsWebview)
  }
}

/// The starter and trusted framework archive embedded by the CLI.
#[cfg(feature = "portable")]
pub struct EmbeddedStarter<'a> {
  pub rust_source: &'a str,
  pub html: Option<&'a str>,
  pub framework_archive: &'a [u8],
}

/// Generate a project from a local Lintel checkout and return its absolute directory.
pub fn create_project(directory: &Path, template: ProjectTemplate, lintel_path: Option<&Path>) -> Result<PathBuf> {
  #[cfg(target_os = "macos")]
  return macos::create_project(directory, template, lintel_path);

  #[cfg(not(target_os = "macos"))]
  {
    let _ = (directory, template, lintel_path);
    anyhow::bail!("Lintel application starters require macOS");
  }
}

/// Generate an application with its own framework sources and return its absolute directory.
#[cfg(feature = "portable")]
pub fn create_portable_project(
  directory: &Path,
  template: ProjectTemplate,
  starter: &EmbeddedStarter<'_>,
) -> Result<PathBuf> {
  #[cfg(target_os = "macos")]
  return macos::create_portable_project(directory, template, starter);

  #[cfg(not(target_os = "macos"))]
  {
    let _ = (directory, template, starter);
    anyhow::bail!("Lintel application starters require macOS");
  }
}

#[cfg(target_os = "macos")]
mod macos {
  use std::ffi::CString;
  use std::fs::{self, File, OpenOptions};
  use std::io;
  use std::os::unix::ffi::OsStrExt;
  use std::os::unix::fs::OpenOptionsExt;
  use std::path::{Path, PathBuf};

  use anyhow::{Context, Result, ensure};

  #[cfg(feature = "portable")]
  use super::EmbeddedStarter;
  use super::ProjectTemplate;

  pub(super) fn create_project(
    directory: &Path,
    template: ProjectTemplate,
    lintel_path: Option<&Path>,
  ) -> Result<PathBuf> {
    stage_project(directory, |project, name| {
      validate_template_name(name, template)?;
      let source_checkout = match lintel_path {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("locate the current Lintel checkout")?,
      };
      let checkout = source_checkout.canonicalize().context("locate the Lintel checkout")?;
      validate_checkout(&checkout)?;
      crate::prepare_public_iced(&checkout)?;

      let starter = checkout.join("templates").join(match template {
        ProjectTemplate::Native => "native",
        ProjectTemplate::Multiprocess => "multiprocess",
        ProjectTemplate::Webview => "webview",
        ProjectTemplate::WebviewMultiprocess => "webview-multiprocess",
        ProjectTemplate::WindowsWebview => "windows-webview",
      });
      let rust_file = if matches!(template, ProjectTemplate::WindowsWebview) {
        "src/lib.rs"
      } else {
        "src/main.rs"
      };
      let rust_source = fs::read(starter.join(rust_file)).context("read the starter Rust source")?;
      let html = if template.has_webview() {
        Some(fs::read(starter.join("ui/index.html")).context("read the starter HTML")?)
      } else {
        None
      };
      write_project(
        project,
        &checkout,
        &checkout,
        name,
        template,
        &rust_source,
        html.as_deref(),
      )
    })
  }

  fn validate_checkout(checkout: &Path) -> Result<()> {
    for required in [
      "Cargo.toml",
      "rust-toolchain.toml",
      ".cargo/macos-public-dependencies.toml",
      "profiles/macos-public/Cargo.lock",
      "profiles/macos-public/iced.patch",
      "vendor/iced/Cargo.toml",
    ] {
      ensure!(
        checkout.join(required).is_file(),
        "the selected directory is not a complete Lintel checkout (missing {required}); pass an explicit framework checkout"
      );
    }
    Ok(())
  }

  #[cfg(feature = "portable")]
  pub(super) fn create_portable_project(
    directory: &Path,
    template: ProjectTemplate,
    starter: &EmbeddedStarter<'_>,
  ) -> Result<PathBuf> {
    stage_project(directory, |project, name| {
      validate_template_name(name, template)?;
      let framework_path = Path::new(".lintel/framework");
      let framework = project.join(framework_path);
      unpack_framework(&framework, starter.framework_archive)?;
      write_project(
        project,
        &framework,
        framework_path,
        name,
        template,
        starter.rust_source.as_bytes(),
        starter.html.map(str::as_bytes),
      )
    })
  }

  fn stage_project(directory: &Path, write: impl FnOnce(&Path, &str) -> Result<()>) -> Result<PathBuf> {
    let directory: PathBuf = directory.components().collect();
    let existing = empty_directory_exists(&directory)?;
    let destination = if existing {
      directory
        .canonicalize()
        .context("resolve the empty project directory")?
    } else {
      let parent = directory
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
      let name = directory
        .file_name()
        .context("provide a directory for the application")?;
      parent
        .canonicalize()
        .context("resolve the project parent directory")?
        .join(name)
    };
    let name = destination
      .file_name()
      .and_then(|name| name.to_str())
      .filter(|name| valid_package_name(name))
      .context("choose a directory name such as my-app (up to 63 lowercase ASCII letters, digits and hyphens)")?;

    let parent = destination.parent().context("the project directory has no parent")?;
    let stage = tempfile::Builder::new()
      .prefix(".lintel-project-")
      .tempdir_in(parent)
      .with_context(|| format!("stage the project beside {}", destination.display()))?;
    let project = stage.path().join("project");
    write(&project, name).context("write the staged Lintel project")?;
    if existing {
      install_into_empty_directory(&project, &destination)?;
    } else {
      rename_exclusive(&project, &destination)?;
    }
    Ok(destination)
  }

  #[cfg(feature = "portable")]
  fn unpack_framework(framework: &Path, archive_bytes: &[u8]) -> Result<()> {
    use std::path::Component;

    fs::create_dir_all(framework)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(archive_bytes));
    archive.set_overwrite(false);
    // The archive keeps deterministic timestamps, but extracted sources must be fresh so a
    // shared Cargo target cannot reuse metadata from an older generated framework revision.
    archive.set_preserve_mtime(false);
    for entry in archive
      .entries()
      .context("read the embedded Lintel framework archive")?
    {
      let mut entry = entry?;
      let path = entry.path()?.into_owned();
      let kind = entry.header().entry_type();
      ensure!(
        kind.is_file() || kind.is_dir(),
        "embedded framework entry must be a regular file or directory: {}",
        path.display()
      );
      ensure!(
        path
          .components()
          .all(|part| matches!(part, Component::Normal(_) | Component::CurDir)),
        "embedded framework entry has an invalid path: {}",
        path.display()
      );
      ensure!(
        entry.unpack_in(framework)?,
        "embedded framework entry cannot be extracted: {}",
        path.display()
      );
    }
    // Read the gzip trailer even if the tar reader has already reached its end marker.
    io::copy(&mut archive.into_inner(), &mut io::sink()).context("verify the embedded archive gzip trailer")?;
    ensure!(
      framework.join("Cargo.toml").is_file() && framework.join(".cargo/macos-public-dependencies.toml").is_file(),
      "embedded Lintel framework archive is missing its Cargo manifest or configuration"
    );
    Ok(())
  }

  fn empty_directory_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
      Ok(metadata) => {
        ensure!(
          !metadata.is_symlink(),
          "destination must not be a symbolic link: {}",
          path.display()
        );
        ensure!(
          metadata.is_dir() && fs::read_dir(path)?.next().transpose()?.is_none(),
          "destination is not an empty directory: {}",
          path.display()
        );
        Ok(true)
      }
      Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
      Err(error) => Err(error).with_context(|| format!("inspect destination {}", path.display())),
    }
  }

  fn valid_package_name(name: &str) -> bool {
    name.len() <= 63
      && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
      && name.split('-').all(|part| {
        !part.is_empty()
          && part
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
      })
  }

  fn validate_template_name(name: &str, template: ProjectTemplate) -> Result<()> {
    ensure!(
      name != "lintel-framework",
      "application name {name:?} conflicts with the framework package; choose a different directory name such as my-app"
    );
    if template.has_multiprocess() {
      ensure!(
        !["lintel-worker", "lintel-app-protocol"].contains(&name),
        "application name {name:?} conflicts with the multiprocess starter's fixed worker target or protocol package; choose a different directory name"
      );
    }
    Ok(())
  }

  fn write_project(
    project: &Path,
    framework: &Path,
    dependency_path: &Path,
    name: &str,
    template: ProjectTemplate,
    rust_source: &[u8],
    html: Option<&[u8]>,
  ) -> Result<()> {
    validate_template_name(name, template)?;
    let dependency = quoted_path(dependency_path)?;
    let config_path = dependency_path.join(".cargo/macos-public-dependencies.toml");
    // An include path is relative to the generated application's .cargo directory.
    let config_path = if config_path.is_absolute() {
      config_path
    } else {
      Path::new("..").join(config_path)
    };
    let config = quoted_path(&config_path)?;
    let toolchain =
      fs::read_to_string(framework.join("rust-toolchain.toml")).context("read the Lintel toolchain pin")?;
    let channel = toolchain
      .lines()
      .find(|line| line.starts_with("channel = "))
      .context("the Lintel toolchain pin has no channel")?;
    let (title, features, build_dependencies) = match template {
      ProjectTemplate::Native => ("Counter", r#"["public-iced"]"#, String::new()),
      ProjectTemplate::Multiprocess => ("Lintel Services", r#"["public-iced", "multiprocess"]"#, String::new()),
      ProjectTemplate::Webview | ProjectTemplate::WebviewMultiprocess => {
        let build_dependency = quoted_path(&dependency_path.join("crates/lintel-build"))?;
        (
          if template.has_multiprocess() {
            "Lintel WebView Services"
          } else {
            "Lintel WebView"
          },
          if template.has_multiprocess() {
            r#"["public-iced", "public-cef", "multiprocess"]"#
          } else {
            r#"["public-iced", "public-cef"]"#
          },
          format!("[build-dependencies]\nlintel-build = {{ path = {build_dependency} }}\n\n"),
        )
      }
      ProjectTemplate::WindowsWebview => (
        "Lintel Windows CEF (Experimental)",
        r#"["public-iced", "public-cef"]"#,
        String::new(),
      ),
    };
    let workspace_exclude = if dependency_path.is_absolute() {
      String::new()
    } else {
      format!("exclude = [{dependency}]\n")
    };
    fs::create_dir_all(project.join(".cargo"))?;
    fs::create_dir(project.join("src"))?;
    let (default_run, service_metadata, protocol_dependency, service_targets) = if template.has_multiprocess() {
      (
        format!("default-run = \"{name}\"\n"),
        "[[package.metadata.lintel.service_processes]]\nname = \"echo-worker\"\nbin = \"lintel-worker\"\n\n",
        "lintel-app-protocol = { path = \"protocol\" }\n",
        "[[bin]]\nname = \"lintel-worker\"\npath = \"src/bin/lintel-worker.rs\"\n\n",
      )
    } else {
      (String::new(), "", "", "")
    };
    let service_features = if template.has_multiprocess() {
      "[features]\ndefault = []\nservice-crash-reports = [\"lintel/crash-reporting\"]\n\n"
    } else {
      ""
    };
    let manifest = if matches!(template, ProjectTemplate::WindowsWebview) {
      format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
         [package.metadata.lintel]\nid = \"dev.example.{name}\"\nname = \"{title}\"\nexperimental-windows-cef = true\n\n\
         [lib]\nname = \"App\"\ncrate-type = [\"cdylib\"]\n\n\
         [features]\ndefault = []\n# Type checks only: do not build or distribute with api-only.\napi-only = [\"dep:cef\", \"cef/dox\"]\n\n\
         [target.'cfg(target_os = \"windows\")'.dependencies]\n\
         lintel = {{ package = \"lintel-framework\", path = {dependency}, default-features = false, features = {features} }}\n\
         cef = {{ version = \"143\", optional = true }}\n\n\
         [workspace]\n{workspace_exclude}"
      )
    } else {
      format!(
        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n{default_run}\n\
         [package.metadata.lintel]\nid = \"dev.example.{name}\"\nname = \"{title}\"\n\n\
         {service_metadata}{service_features}[dependencies]\nlintel = {{ package = \"lintel-framework\", path = {dependency}, features = {features} }}\n{protocol_dependency}\n\
         {service_targets}{build_dependencies}[workspace]\n{workspace_exclude}"
      )
    };
    fs::write(project.join("Cargo.toml"), manifest)?;
    fs::write(
      project.join(".cargo/config.toml"),
      format!("include = [{config}]\n\n[net]\ngit-fetch-with-cli = true\n"),
    )?;
    fs::write(project.join("rust-toolchain.toml"), format!("[toolchain]\n{channel}\n"))?;
    let windows = matches!(template, ProjectTemplate::WindowsWebview);
    fs::write(
      project.join(".gitignore"),
      if windows {
        "/target/\n/build/windows/\n"
      } else {
        "/target/\n/build/macos/\n"
      },
    )?;
    fs::write(
      project.join(if windows { "src/lib.rs" } else { "src/main.rs" }),
      rust_source,
    )
    .context("write the starter Rust source")?;
    if template.has_multiprocess() {
      write_process_sources(project, framework)?;
    }
    if template.has_webview() {
      let html = html.context("the WebView starter is missing its HTML")?;
      fs::create_dir(project.join("ui"))?;
      fs::write(project.join("ui/index.html"), html).context("write the starter HTML")?;
      if !windows {
        fs::write(
          project.join("build.rs"),
          "fn main() {\n  lintel_build::configure_macos_cef();\n}\n",
        )?;
      }
    }
    if windows {
      fs::write(
        project.join("WINDOWS-CEF.md"),
        include_str!("../resources/templates/windows-webview/WINDOWS-CEF.md"),
      )?;
    }
    // Cargo updates the root package entry on first run and retains the public pins.
    fs::copy(
      framework.join("profiles/macos-public/Cargo.lock"),
      project.join("Cargo.lock"),
    )
    .context("copy the pinned public lockfile")?;
    write_agent_guides(project, framework, dependency_path, name, template)?;
    Ok(())
  }

  fn write_process_sources(project: &Path, framework: &Path) -> Result<()> {
    for path in [
      "src/crash_options.rs",
      "src/bin/lintel-worker.rs",
      "protocol/Cargo.toml",
      "protocol/build.rs",
      "protocol/src/lib.rs",
      "protocol/schema/echo.mojom",
    ] {
      let output = project.join(path);
      fs::create_dir_all(output.parent().context("multiprocess source has no parent")?)?;
      fs::copy(framework.join("templates/multiprocess").join(path), output)
        .with_context(|| format!("copy multiprocess starter {path}"))?;
    }
    Ok(())
  }

  fn write_agent_guides(
    project: &Path,
    framework: &Path,
    dependency_path: &Path,
    name: &str,
    template: ProjectTemplate,
  ) -> Result<()> {
    let catalog: serde_json::Value = serde_json::from_str(include_str!("../resources/lintel.project.json"))
      .context("parse the embedded project-generation catalog")?;
    ensure!(
      catalog["schema_version"].as_u64() == Some(1),
      "unsupported project catalog schema_version"
    );
    ensure!(
      catalog["kind"].as_str() == Some("lintel.framework"),
      "invalid project catalog kind"
    );
    let template_name = match template {
      ProjectTemplate::Native => "native",
      ProjectTemplate::Multiprocess => "multiprocess",
      ProjectTemplate::Webview => "webview",
      ProjectTemplate::WebviewMultiprocess => "webview-multiprocess",
      ProjectTemplate::WindowsWebview => "windows-webview",
    };
    let records = catalog["templates"]
      .as_array()
      .context("project catalog has no templates array")?;
    let matching: Vec<_> = records
      .iter()
      .filter(|record| record["name"] == template_name)
      .collect();
    ensure!(
      matching.len() == 1,
      "project catalog must define the selected template exactly once"
    );
    let record = matching[0];
    let field = |key: &str| -> Result<&str> {
      record[key]
        .as_str()
        .filter(|value| !value.is_empty())
        .with_context(|| format!("template catalog has no {key}"))
    };
    let entry = field("app_entry")?;
    let sources = catalog_strings(&record["source_paths"], "template source_paths")?;
    ensure!(
      sources.contains(&entry),
      "template source_paths must include the application entry"
    );
    for source in &sources {
      ensure!(
        !Path::new(source).is_absolute()
          && Path::new(source)
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
          && !source.contains(['\\', ':', '\n', '\r', '`', '[', ']']),
        "template source path must be a plain relative application path"
      );
      ensure!(
        project.join(source).is_file(),
        "template catalog refers to a missing application source: {source}"
      );
    }
    let source_links = sources
      .iter()
      .map(|source| format!("- [{source}]({source})"))
      .collect::<Vec<_>>()
      .join("\n");
    let commands = catalog_strings(&record["validation_commands"], "template validation_commands")?.join("\n");
    let run_guidance = match record.get("run_command") {
      Some(run) => {
        let command = run["command"].as_str().context("template run_command has no command")?;
        let platforms = catalog_strings(&run["platforms"], "template run_command platforms")?.join(", ");
        format!("Development run command on {platforms}:\n\n```sh\n{command}\n```\n\nRun the application when the task requires its behavior to be checked.")
      }
      None => "This template declares no default run command. Follow the validation boundary and its application guide before attempting native execution.".into(),
    };
    let references: String = if dependency_path.is_absolute() {
      // An arbitrary checkout path may contain Markdown/newlines. Resolve it
      // through the already-quoted Cargo dependency instead of injecting it here.
      "This project uses an explicit local checkout. Resolve the `lintel` dependency's path in `Cargo.toml`; that checkout's `lintel.project.json`, `llms.txt` and `AGENTS.md` describe its framework. Read framework contribution rules when editing that checkout.".into()
    } else if ["AGENTS.md", "llms.txt", "lintel.project.json"]
      .iter()
      .all(|file| framework.join(file).is_file())
    {
      "- [Pinned framework catalog](.lintel/framework/lintel.project.json)\n- [Framework navigation](.lintel/framework/llms.txt)\n- [Framework contribution rules](.lintel/framework/AGENTS.md), for edits within the framework".into()
    } else {
      // Keep EmbeddedStarter's existing archive/literal API compatible with
      // older archives that predate the discovery documents.
      "- [Pinned framework manifest](.lintel/framework/Cargo.toml)\n\nThis embedded framework predates the discovery documents. Inspect its pinned sources and version; the installed CLI's catalog may describe a different framework revision.".into()
    };
    let values = [
      ("{{APP_NAME}}", name),
      ("{{TEMPLATE_NAME}}", template_name),
      ("{{TEMPLATE_SUMMARY}}", field("summary")?),
      ("{{APP_ENTRY}}", entry),
      ("{{SOURCE_LINKS}}", source_links.as_str()),
      ("{{FRAMEWORK_REFERENCES}}", references.as_str()),
      ("{{VALIDATION_COMMANDS}}", commands.as_str()),
      ("{{VALIDATION_BOUNDARY}}", field("validation_boundary")?),
      ("{{RUN_GUIDANCE}}", run_guidance.as_str()),
    ];
    for (path, source) in [
      ("AGENTS.md", include_str!("../resources/templates/agent/AGENTS.md")),
      ("llms.txt", include_str!("../resources/templates/agent/llms.txt")),
    ] {
      let mut text = source.to_owned();
      for (placeholder, value) in values {
        text = text.replace(placeholder, value);
      }
      ensure!(!text.contains("{{"), "unresolved agent guide placeholder");
      fs::write(project.join(path), text).with_context(|| format!("write the application {path}"))?;
    }
    Ok(())
  }

  fn catalog_strings<'a>(value: &'a serde_json::Value, label: &str) -> Result<Vec<&'a str>> {
    let list = value
      .as_array()
      .with_context(|| format!("catalog {label} must be an array"))?;
    ensure!(!list.is_empty(), "catalog {label} must not be empty");
    list
      .iter()
      .map(|item| {
        item
          .as_str()
          .filter(|text| !text.is_empty())
          .with_context(|| format!("catalog {label} must contain nonempty strings"))
      })
      .collect()
  }

  fn quoted_path(path: &Path) -> Result<String> {
    let path = path.to_str().context("the Lintel checkout path must be UTF-8")?;
    Ok(serde_json::to_string(path)?)
  }

  fn install_into_empty_directory(project: &Path, destination: &Path) -> Result<()> {
    // Lock the existing directory inode without creating a lockfile inside it.
    let directory = OpenOptions::new()
      .read(true)
      .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
      .open(destination)
      .with_context(|| format!("open existing project directory {}", destination.display()))?;
    directory.lock().context("lock the existing project directory")?;
    ensure!(
      empty_directory_exists(destination)?,
      "the project directory disappeared"
    );

    let mut installed = Vec::new();
    if let Err(error) = copy_new_entries(project, destination, &mut installed) {
      for (path, directory) in installed.into_iter().rev() {
        if directory {
          let _ = fs::remove_dir(path);
        } else {
          let _ = fs::remove_file(path);
        }
      }
      return Err(error).with_context(|| format!("install the project into {}", destination.display()));
    }
    Ok(())
  }

  fn copy_new_entries(source: &Path, target: &Path, installed: &mut Vec<(PathBuf, bool)>) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
      let entry = entry?;
      let target = target.join(entry.file_name());
      let kind = entry.file_type()?;
      if kind.is_dir() {
        fs::create_dir(&target)?;
        installed.push((target.clone(), true));
        copy_new_entries(&entry.path(), &target, installed)?;
      } else if kind.is_file() {
        let mut input = File::open(entry.path())?;
        let mut output = File::create_new(&target)?;
        installed.push((target, false));
        io::copy(&mut input, &mut output)?;
        output.set_permissions(input.metadata()?.permissions())?;
      } else {
        return Err(io::Error::new(
          io::ErrorKind::InvalidData,
          format!("unsupported project entry {}", entry.path().display()),
        ));
      }
    }
    Ok(())
  }

  fn rename_exclusive(source: &Path, destination: &Path) -> Result<()> {
    let from = CString::new(source.as_os_str().as_bytes())?;
    let to = CString::new(destination.as_os_str().as_bytes())?;
    // SAFETY: Both NUL-terminated strings stay valid for the call. RENAME_EXCL never replaces an existing path.
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) } != 0 {
      return Err(io::Error::last_os_error())
        .with_context(|| format!("publish the project at {}", destination.display()));
    }
    Ok(())
  }

  #[cfg(test)]
  mod tests {
    use super::*;

    const WINDOWS_SOURCE: &[u8] = include_bytes!("../resources/templates/windows-webview/src/lib.rs");
    const WINDOWS_HTML: &[u8] = include_bytes!("../resources/templates/windows-webview/ui/index.html");

    #[test]
    fn checkout_selection_requires_framework_files_instead_of_using_a_build_machine_directory() {
      let root = tempfile::tempdir().unwrap();
      fs::write(root.path().join("Cargo.toml"), "[package]\nname=\"consumer\"\n").unwrap();
      assert!(validate_checkout(root.path()).is_err());
      for path in [
        "rust-toolchain.toml",
        ".cargo/macos-public-dependencies.toml",
        "profiles/macos-public/Cargo.lock",
        "profiles/macos-public/iced.patch",
        "vendor/iced/Cargo.toml",
      ] {
        let path = root.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "fixture").unwrap();
      }
      assert!(validate_checkout(root.path()).is_ok());
    }

    fn assert_worker_crash_budgets(source: &str) {
      let (default, enabled) = source
        .split_once("if let Some(directory) = crash.service_directory {")
        .expect("the template must opt into worker crash reporting explicitly");
      assert!(default.contains(".startup_timeout(Duration::from_secs(5))"));
      assert!(default.contains(".shutdown_grace(Duration::from_millis(500))"));
      let enabled = enabled
        .split_once("let config = HostConfig")
        .expect("the optional service configuration must precede the host")
        .0;
      assert!(enabled.contains(".arg(\"--service-crash-directory\")"));
      assert!(enabled.contains(".startup_timeout(Duration::from_secs(10))"));
      assert!(enabled.contains(".shutdown_grace(Duration::from_secs(3))"));
    }

    fn framework(base: &Path) -> PathBuf {
      let framework = base.join("framework");
      fs::create_dir_all(framework.join("profiles/macos-public")).unwrap();
      fs::write(
        framework.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"nightly-2026-04-01\"\n",
      )
      .unwrap();
      fs::write(framework.join("profiles/macos-public/Cargo.lock"), "version = 4\n").unwrap();
      let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources");
      for path in [
        "src/crash_options.rs",
        "src/bin/lintel-worker.rs",
        "protocol/Cargo.toml",
        "protocol/build.rs",
        "protocol/src/lib.rs",
        "protocol/schema/echo.mojom",
      ] {
        let destination = framework.join("templates/multiprocess").join(path);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::copy(checkout.join("templates/multiprocess").join(path), destination).unwrap();
      }
      framework
    }

    #[test]
    fn multiprocess_starter_includes_shared_protocol_worker_and_unambiguous_default_binary() {
      assert_worker_crash_budgets(include_str!("../resources/templates/multiprocess/src/main.rs"));
      let root = tempfile::tempdir().unwrap();
      let framework = framework(root.path());
      let project = root.path().join("service-app");
      write_project(
        &project,
        &framework,
        &framework,
        "service-app",
        ProjectTemplate::Multiprocess,
        b"fn main() {}",
        None,
      )
      .unwrap();
      let manifest = fs::read_to_string(project.join("Cargo.toml")).unwrap();
      assert!(manifest.contains("default-run = \"service-app\""));
      assert!(manifest.contains("\"public-iced\", \"multiprocess\""));
      assert!(manifest.contains("lintel-app-protocol = { path = \"protocol\" }"));
      assert!(manifest.contains("[features]\ndefault = []\nservice-crash-reports = [\"lintel/crash-reporting\"]"));
      assert!(
        manifest
          .contains("[[package.metadata.lintel.service_processes]]\nname = \"echo-worker\"\nbin = \"lintel-worker\"")
      );
      assert!(manifest.contains("[[bin]]\nname = \"lintel-worker\"\npath = \"src/bin/lintel-worker.rs\""));
      for path in [
        "src/crash_options.rs",
        "src/bin/lintel-worker.rs",
        "protocol/Cargo.toml",
        "protocol/build.rs",
        "protocol/src/lib.rs",
        "protocol/schema/echo.mojom",
      ] {
        assert_eq!(
          fs::read(project.join(path)).unwrap(),
          fs::read(framework.join("templates/multiprocess").join(path)).unwrap()
        );
      }
      assert!(!project.join("native").exists());
      assert!(!manifest.contains("mojo_runtime ="));
      assert!(
        fs::read_to_string(project.join("AGENTS.md"))
          .unwrap()
          .contains("multiprocess")
      );
    }

    #[test]
    fn framework_package_name_is_rejected_by_all_starters_without_publishing_or_changing_an_empty_directory() {
      for template in [
        ProjectTemplate::Native,
        ProjectTemplate::Multiprocess,
        ProjectTemplate::Webview,
        ProjectTemplate::WebviewMultiprocess,
        ProjectTemplate::WindowsWebview,
      ] {
        for existing in [false, true] {
          let root = tempfile::tempdir().unwrap();
          let destination = root.path().join("lintel-framework");
          if existing {
            fs::create_dir(&destination).unwrap();
          }
          let assert_not_published = || {
            assert_eq!(destination.exists(), existing);
            if existing {
              assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
            }
            assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
              !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".lintel-project-")
            }));
          };
          let error = create_project(&destination, template, Some(&root.path().join("missing-checkout"))).unwrap_err();
          assert!(format!("{error:#}").contains("conflicts with the framework package"));
          assert_not_published();
          #[cfg(feature = "portable")]
          {
            let starter = EmbeddedStarter {
              rust_source: "fn main() {}",
              html: template.has_webview().then_some("<html></html>"),
              framework_archive: b"invalid archive must not be opened for a reserved application name",
            };
            let error = create_portable_project(&destination, template, &starter).unwrap_err();
            assert!(format!("{error:#}").contains("conflicts with the framework package"));
            assert_not_published();
          }
          for name in ["my-app", "lintel", "lintel-framework-app", "app-lintel-framework"] {
            assert!(
              validate_template_name(name, template).is_ok(),
              "unexpectedly rejected {name}"
            );
          }
        }
      }
    }

    #[test]
    fn multiprocess_reserved_names_fail_without_publishing_or_changing_an_empty_destination() {
      for template in [ProjectTemplate::Multiprocess, ProjectTemplate::WebviewMultiprocess] {
        for name in ["lintel-worker", "lintel-app-protocol"] {
          for existing in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let framework = framework(root.path());
            let destination = root.path().join(name);
            if existing {
              fs::create_dir(&destination).unwrap();
            }
            let error = stage_project(&destination, |project, name| {
              write_project(
                project,
                &framework,
                &framework,
                name,
                template,
                b"fn main() {}",
                template.has_webview().then_some(WINDOWS_HTML),
              )
            })
            .unwrap_err();
            assert!(format!("{error:#}").contains("conflicts with the multiprocess starter"));
            assert_eq!(destination.exists(), existing);
            if existing {
              assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
            }
            assert!(fs::read_dir(root.path()).unwrap().all(|entry| {
              !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".lintel-project-")
            }));
          }
        }
      }
    }

    #[test]
    fn non_multiprocess_starters_keep_their_existing_application_name_range() {
      let root = tempfile::tempdir().unwrap();
      let framework = framework(root.path());
      for template in [
        ProjectTemplate::Native,
        ProjectTemplate::Webview,
        ProjectTemplate::WindowsWebview,
      ] {
        for name in ["lintel-worker", "lintel-app-protocol"] {
          assert!(validate_template_name(name, template).is_ok());
          let project = root.path().join(format!("{template:?}-{name}"));
          write_project(
            &project,
            &framework,
            &framework,
            name,
            template,
            if matches!(template, ProjectTemplate::WindowsWebview) {
              WINDOWS_SOURCE
            } else {
              b"fn main() {}"
            },
            template.has_webview().then_some(WINDOWS_HTML),
          )
          .unwrap();
          assert!(
            fs::read_to_string(project.join("Cargo.toml"))
              .unwrap()
              .contains(&format!("name = \"{name}\""))
          );
        }
      }
    }

    #[test]
    fn webview_multiprocess_starter_combines_cef_build_setup_with_shared_worker_sources() {
      assert_worker_crash_budgets(include_str!("../resources/templates/webview-multiprocess/src/main.rs"));
      let root = tempfile::tempdir().unwrap();
      let framework = framework(root.path());
      for (label, dependency) in [
        ("checkout", framework.as_path()),
        ("portable", Path::new(".lintel/framework")),
      ] {
        let project = root.path().join(label);
        write_project(
          &project,
          &framework,
          dependency,
          "web-services",
          ProjectTemplate::WebviewMultiprocess,
          b"fn main() {}",
          Some(WINDOWS_HTML),
        )
        .unwrap();
        let manifest = fs::read_to_string(project.join("Cargo.toml")).unwrap();
        assert!(manifest.contains("default-run = \"web-services\""));
        assert!(manifest.contains("\"public-iced\", \"public-cef\", \"multiprocess\""));
        assert!(manifest.contains("[build-dependencies]\nlintel-build ="));
        assert!(manifest.contains("lintel-app-protocol = { path = \"protocol\" }"));
        assert!(manifest.contains("[features]\ndefault = []\nservice-crash-reports = [\"lintel/crash-reporting\"]"));
        assert!(
          manifest
            .contains("[[package.metadata.lintel.service_processes]]\nname = \"echo-worker\"\nbin = \"lintel-worker\"")
        );
        assert!(manifest.contains("[[bin]]\nname = \"lintel-worker\"\npath = \"src/bin/lintel-worker.rs\""));
        assert_eq!(fs::read(project.join("ui/index.html")).unwrap(), WINDOWS_HTML);
        assert!(
          fs::read_to_string(project.join("build.rs"))
            .unwrap()
            .contains("configure_macos_cef")
        );
        for path in [
          "src/crash_options.rs",
          "src/bin/lintel-worker.rs",
          "protocol/Cargo.toml",
          "protocol/build.rs",
          "protocol/src/lib.rs",
          "protocol/schema/echo.mojom",
        ] {
          assert_eq!(
            fs::read(project.join(path)).unwrap(),
            fs::read(framework.join("templates/multiprocess").join(path)).unwrap()
          );
        }
        for guide in ["AGENTS.md", "llms.txt"] {
          let guide = fs::read_to_string(project.join(guide)).unwrap();
          assert!(guide.contains("ui/index.html"));
          assert!(guide.contains("src/bin/lintel-worker.rs"));
        }
        assert!(!manifest.contains("mojo_runtime ="));
        assert!(!project.join("native").exists());
      }
    }

    #[test]
    fn windows_starter_is_cdylib_only_and_has_explicit_api_only_feature_and_bootstrap_guide() {
      let root = tempfile::tempdir().unwrap();
      let framework = framework(root.path());
      let destination = root.path().join("my-windows-app");
      fs::create_dir(&destination).unwrap();
      stage_project(&destination, |project, name| {
        write_project(
          project,
          &framework,
          &framework,
          name,
          ProjectTemplate::WindowsWebview,
          WINDOWS_SOURCE,
          Some(WINDOWS_HTML),
        )
      })
      .unwrap();
      let manifest = fs::read_to_string(destination.join("Cargo.toml")).unwrap();
      assert!(manifest.contains("[lib]\nname = \"App\"\ncrate-type = [\"cdylib\"]"));
      assert!(manifest.contains("default = []\n") && manifest.contains("api-only = [\"dep:cef\", \"cef/dox\"]"));
      assert!(manifest.contains("[target.'cfg(target_os = \"windows\")'.dependencies]"));
      assert!(manifest.contains("default-features = false") && manifest.contains("public-cef"));
      assert!(!destination.join("src/main.rs").exists() && !destination.join("build.rs").exists());
      assert_eq!(fs::read(destination.join("src/lib.rs")).unwrap(), WINDOWS_SOURCE);
      assert_eq!(fs::read(destination.join("ui/index.html")).unwrap(), WINDOWS_HTML);
      let guide = fs::read_to_string(destination.join("WINDOWS-CEF.md")).unwrap();
      assert!(
        guide.contains("App.exe") && guide.contains("cef_version_info_t*") && guide.contains("packaging_enabled")
      );
      assert!(!destination.join("App.exe").exists() && !destination.join("App.dll").exists());
    }

    #[test]
    fn native_and_macos_webview_starters_keep_main_and_existing_build_behavior() {
      let root = tempfile::tempdir().unwrap();
      let framework = framework(root.path());
      for (name, template, html) in [
        ("native-app", ProjectTemplate::Native, None),
        ("webview-app", ProjectTemplate::Webview, Some(WINDOWS_HTML)),
      ] {
        let project = root.path().join(name);
        write_project(&project, &framework, &framework, name, template, b"fn main() {}", html).unwrap();
        let manifest = fs::read_to_string(project.join("Cargo.toml")).unwrap();
        assert!(!manifest.contains("service-crash-reports"));
        assert!(project.join("src/main.rs").is_file() && !project.join("src/lib.rs").exists());
        assert!(manifest.contains("[dependencies]\n") && !manifest.contains("[lib]") && !manifest.contains("api-only"));
        assert_eq!(
          project.join("build.rs").exists(),
          matches!(template, ProjectTemplate::Webview)
        );
        if matches!(template, ProjectTemplate::Webview) {
          assert!(
            fs::read_to_string(project.join("build.rs"))
              .unwrap()
              .contains("configure_macos_cef")
          );
        }
      }
    }

    #[test]
    fn consumer_guides_follow_the_selected_catalog_template_and_preserve_windows_api_only_boundary() {
      let root = tempfile::tempdir().unwrap();
      let framework = framework(root.path());
      let catalog: serde_json::Value = serde_json::from_str(include_str!("../resources/lintel.project.json")).unwrap();
      for (label, template, html) in [
        ("native", ProjectTemplate::Native, None),
        ("multiprocess", ProjectTemplate::Multiprocess, None),
        ("webview", ProjectTemplate::Webview, Some(WINDOWS_HTML)),
        (
          "webview-multiprocess",
          ProjectTemplate::WebviewMultiprocess,
          Some(WINDOWS_HTML),
        ),
        ("windows-webview", ProjectTemplate::WindowsWebview, Some(WINDOWS_HTML)),
      ] {
        let project = root.path().join(format!("guide-{label}"));
        write_project(
          &project,
          &framework,
          &framework,
          "guide-app",
          template,
          b"fn main() {}",
          html,
        )
        .unwrap();
        let selected = catalog["templates"]
          .as_array()
          .unwrap()
          .iter()
          .find(|item| item["name"] == label)
          .unwrap();
        let agents = fs::read_to_string(project.join("AGENTS.md")).unwrap();
        let index = fs::read_to_string(project.join("llms.txt")).unwrap();
        for output in [&agents, &index] {
          assert!(!output.contains("{{"));
          assert!(output.contains(selected["summary"].as_str().unwrap()));
          assert!(output.contains(selected["validation_boundary"].as_str().unwrap()));
          for source in selected["source_paths"].as_array().unwrap() {
            assert!(output.contains(source.as_str().unwrap()));
          }
        }
        for command in selected["validation_commands"].as_array().unwrap() {
          assert!(agents.contains(command.as_str().unwrap()));
        }
        assert!(agents.contains("AI Dev Loop is off by default"));
        if label == "windows-webview" {
          assert!(agents.contains("--features api-only"));
          assert!(!agents.contains("lintel run") && !index.contains("lintel run"));
        }
      }
    }

    #[test]
    fn checkout_guides_resolve_the_manifest_instead_of_injecting_arbitrary_path_text() {
      let root = tempfile::tempdir().unwrap();
      let unusual = root.path().join("checkout `notes`\nnot-an-instruction");
      let framework = framework(&unusual);
      let project = root.path().join("guide-app");
      write_project(
        &project,
        &framework,
        &framework,
        "guide-app",
        ProjectTemplate::Native,
        b"fn main() {}",
        None,
      )
      .unwrap();
      let guides = fs::read_to_string(project.join("AGENTS.md")).unwrap();
      assert!(!guides.contains("not-an-instruction"));
      assert!(!guides.contains(root.path().to_str().unwrap()));
      assert!(guides.contains("dependency's path in `Cargo.toml`"));
      let manifest = fs::read_to_string(project.join("Cargo.toml")).unwrap();
      assert!(manifest.contains(&quoted_path(&framework).unwrap()));
    }

    #[test]
    fn failed_windows_generation_preserves_existing_empty_directory_and_refuses_user_data() {
      let root = tempfile::tempdir().unwrap();
      let framework = framework(root.path());
      let destination = root.path().join("windows-app");
      fs::create_dir(&destination).unwrap();
      assert!(
        stage_project(&destination, |project, name| write_project(
          project,
          &framework,
          &framework,
          name,
          ProjectTemplate::WindowsWebview,
          WINDOWS_SOURCE,
          None
        ))
        .is_err()
      );
      assert!(fs::read_dir(&destination).unwrap().next().is_none());
      fs::write(destination.join("customer.txt"), b"keep").unwrap();
      assert!(
        stage_project(&destination, |_, _| panic!(
          "must reject nonempty directory before writing"
        ))
        .is_err()
      );
      assert_eq!(fs::read(destination.join("customer.txt")).unwrap(), b"keep");
    }

    #[cfg(feature = "portable")]
    #[test]
    fn portable_windows_starter_preserves_existing_embedded_starter_literal_contract() {
      use flate2::{Compression, write::GzEncoder};
      let root = tempfile::tempdir().unwrap();
      let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
      for (path, text) in [
        ("Cargo.toml", "[package]\nname=\"lintel\"\nversion=\"0.1.0\"\n"),
        (".cargo/macos-public-dependencies.toml", "# fixture\n"),
        ("rust-toolchain.toml", "[toolchain]\nchannel = \"nightly-2026-04-01\"\n"),
        ("profiles/macos-public/Cargo.lock", "version = 4\n"),
        ("AGENTS.md", include_str!("../resources/AGENTS.md")),
        ("llms.txt", include_str!("../resources/llms.txt")),
        ("lintel.project.json", include_str!("../resources/lintel.project.json")),
      ] {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(text.len() as u64);
        header.set_cksum();
        archive.append_data(&mut header, path, text.as_bytes()).unwrap();
      }
      let archive = archive.into_inner().unwrap().finish().unwrap();
      let destination = root.path().join("portable-windows-app");
      create_portable_project(
        &destination,
        ProjectTemplate::WindowsWebview,
        &EmbeddedStarter {
          rust_source: std::str::from_utf8(WINDOWS_SOURCE).unwrap(),
          html: Some(std::str::from_utf8(WINDOWS_HTML).unwrap()),
          framework_archive: &archive,
        },
      )
      .unwrap();
      let manifest = fs::read_to_string(destination.join("Cargo.toml")).unwrap();
      assert!(
        manifest.contains("path = \".lintel/framework\"") && manifest.contains("exclude = [\".lintel/framework\"]")
      );
      assert!(destination.join(".lintel/framework/Cargo.toml").is_file());
      assert!(destination.join("src/lib.rs").is_file() && destination.join("WINDOWS-CEF.md").is_file());
      for file in ["AGENTS.md", "llms.txt", "lintel.project.json"] {
        assert!(destination.join(".lintel/framework").join(file).is_file());
      }
      let agents = fs::read_to_string(destination.join("AGENTS.md")).unwrap();
      assert!(agents.contains(".lintel/framework/lintel.project.json"));
      assert!(agents.contains(".lintel/framework/llms.txt") && agents.contains(".lintel/framework/AGENTS.md"));
    }

    #[cfg(feature = "portable")]
    fn deterministic_framework_archive(mtime: u64, extra: Option<(&str, tar::EntryType)>) -> Vec<u8> {
      use flate2::{Compression, write::GzEncoder};
      let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
      for (path, text) in [
        ("Cargo.toml", "[package]\nname=\"lintel\"\nversion=\"0.1.0\"\n"),
        (".cargo/macos-public-dependencies.toml", "# fixture\n"),
        ("rust-toolchain.toml", "[toolchain]\nchannel = \"nightly-2026-04-01\"\n"),
        ("profiles/macos-public/Cargo.lock", "version = 4\n"),
        ("crates/lintel-desktop/src/lib.rs", "pub mod client_menu;\n"),
      ] {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        header.set_size(text.len() as u64);
        header.set_mtime(mtime);
        header.set_cksum();
        archive.append_data(&mut header, path, text.as_bytes()).unwrap();
      }
      if let Some((path, kind)) = extra {
        // Raw header construction intentionally bypasses Builder's safe-path validation.
        let mut header = tar::Header::new_gnu();
        assert!(path.len() < 100);
        header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
        header.set_entry_type(kind);
        header.set_mode(0o644);
        header.set_mtime(mtime);
        if kind.is_file() {
          header.set_size(4);
        } else {
          header.set_size(0);
          header.set_link_name("../escaped.rs").unwrap();
        }
        header.set_cksum();
        archive
          .append(&header, if kind.is_file() { &b"evil"[..] } else { &b""[..] })
          .unwrap();
      }
      archive.into_inner().unwrap().finish().unwrap()
    }

    #[cfg(feature = "portable")]
    #[test]
    fn portable_sources_get_creation_time_even_when_archive_mtime_is_deterministic_and_old() {
      use std::time::{Duration, SystemTime, UNIX_EPOCH};
      // 1153704088 is tar's deterministic 2006 timestamp observed in the stale Cargo-cache bug.
      for mtime in [1_153_704_088, 0] {
        let bytes = deterministic_framework_archive(mtime, None);
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
        for entry in archive.entries().unwrap() {
          assert_eq!(entry.unwrap().header().mtime().unwrap(), mtime);
        }
        for existing in [false, true] {
          let root = tempfile::tempdir().unwrap();
          let destination = root.path().join("mtime-app");
          if existing {
            fs::create_dir(&destination).unwrap();
          }
          let before = SystemTime::now();
          create_portable_project(
            &destination,
            ProjectTemplate::Native,
            &EmbeddedStarter {
              rust_source: "fn main() {}",
              html: None,
              framework_archive: &bytes,
            },
          )
          .unwrap();
          let after = SystemTime::now();
          let source = destination.join(".lintel/framework/crates/lintel-desktop/src/lib.rs");
          assert_eq!(fs::read(&source).unwrap(), b"pub mod client_menu;\n");
          let metadata = fs::metadata(&source).unwrap();
          let modified = metadata.modified().unwrap();
          assert_ne!(modified, UNIX_EPOCH + Duration::from_secs(mtime));
          // Allow filesystem timestamp precision without sleeping or touching extracted files.
          assert!(
            modified >= before - Duration::from_secs(1) && modified <= after + Duration::from_secs(1),
            "extracted mtime is not its creation time: {modified:?}"
          );
          let created = metadata.created().unwrap();
          assert!(created >= before - Duration::from_secs(1) && created <= after + Duration::from_secs(1));
        }
      }
    }

    #[cfg(feature = "portable")]
    #[test]
    fn portable_extraction_still_rejects_unsafe_paths_links_and_duplicate_files() {
      for (path, kind, expected) in [
        ("../escaped.rs", tar::EntryType::Regular, "invalid path"),
        ("/escaped.rs", tar::EntryType::Regular, "invalid path"),
        ("linked.rs", tar::EntryType::Symlink, "regular file or directory"),
        ("hard.rs", tar::EntryType::Link, "regular file or directory"),
        ("Cargo.toml", tar::EntryType::Regular, ""),
      ] {
        let root = tempfile::tempdir().unwrap();
        let framework = root.path().join("framework");
        let bytes = deterministic_framework_archive(1_153_704_088, Some((path, kind)));
        let error = unpack_framework(&framework, &bytes).unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
        assert!(!root.path().join("escaped.rs").exists());
        assert!(!framework.join("linked.rs").exists() && !framework.join("hard.rs").exists());
        if path == "Cargo.toml" {
          assert_eq!(
            fs::read(framework.join(path)).unwrap(),
            b"[package]\nname=\"lintel\"\nversion=\"0.1.0\"\n"
          );
        }
      }
    }
  }
}
