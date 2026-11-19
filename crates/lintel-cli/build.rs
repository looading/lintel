use std::env;
use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

use flate2::{Compression, GzBuilder, write::GzEncoder};
use tar::{Builder, HeaderMode};
mod source_policy;

use source_policy::SourcePolicy;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
struct Archive {
  builder: Builder<GzEncoder<File>>,
  policy: SourcePolicy,
}

const ARCHIVE_NAME: &str = "lintel-framework.tar.gz";
const ICED_PATH: &str = "vendor/iced";
const ICED_PATCH: &str = "profiles/macos-public/iced.patch";
const PUBLIC_CONFIG: &str = ".cargo/macos-public-dependencies.toml";
const PUBLIC_LOCK: &str = "profiles/macos-public/Cargo.lock";

const CRATES: &[&str] = &[
  "lintel-crash",
  "lintel-desktop",
  "lintel-desktop/native-platform",
  "lintel-updater",
  "lintel-build",
  "lintel-process",
  "lintel-core",
  "lintel-iced-widgets",
  "lintel-lib-config",
  "lintel-lib-crypto",
  "lintel-lib-env",
  "lintel-lib-event",
  "lintel-lib-iced",
  "lintel-lib-jsb-macro",
  "lintel-lib-store",
  "lintel-lib-utils",
  "lintel-plugin-iced",
  "lintel-plugin-iced-api",
  "lintel-shared-data",
];

const FILES: &[&str] = &[
  "README.md",
  ".gitignore",
  "AGENTS.md",
  "llms.txt",
  "lintel.project.json",
  ".cargo/config.toml",
  ".cargo/macos-public.toml",
  "docs/assets/lintel-architecture.png",
  "build.rs",
  "assets/macos/signing/cef-adhoc/loader.plist",
  "assets/macos/signing/cef-adhoc/jit.plist",
  "assets/macos/signing/cef-adhoc/plugin.plist",
  "assets/macos/signing/cef-adhoc/README.md",
  "crates/lintel-updater/tests/fixtures/pax-size-override.tar.gz",
  "crates/lintel-build/src/windows_cef/cef143_minimal_manifest.json",
  "crates/lintel-desktop/native-platform/Cargo.lock",
  "crates/lintel-desktop/native-platform/SAFETY.md",
  "crates/lintel-desktop/README.md",
  "crates/lintel-crash/README.md",
  "crates/lintel-process/README.md",
  "crates/lintel-process/Cargo.lock",
  "crates/lintel-process/tests/protocol/Cargo.toml",
  "crates/lintel-process/tests/protocol/build.rs",
  "crates/lintel-process/tests/protocol/schema/echo.mojom",
  "crates/lintel-process/tests/protocol/src/lib.rs",
  "crates/lintel-desktop/docs/linux-backends.md",
  "crates/lintel-desktop/docs/client-menubar.md",
  "crates/lintel-desktop/examples/linux_desktop.rs",
  "crates/lintel-desktop/examples/desktop.rs",
  "crates/lintel-desktop/tests/linux_dbus.rs",
  "crates/lintel-updater/examples/self_update.rs",
  "templates/updater/native.rs",
  "docs/updater.md",
  "crates/lintel-iced-widgets/build.rs",
  "crates/lintel-lib-store/examples/desk-store-example.rs",
  "examples/multi_window.rs",
  "examples/window_manager.rs",
  "examples/native_window.rs",
  "examples/accessibility.rs",
  "examples/client_menubar.rs",
  "examples/windows_cef_entry.rs",
  "examples/desktop_window.rs",
  "examples/webview_windows.rs",
  "examples/services.rs",
  "examples/agent_tools.rs",
  "examples/ai_native_app.rs",
  "examples/diagnostic_export.rs",
  "examples/activation.rs",
  "docs/runtime.md",
  "docs/capabilities.md",
  "docs/windows-bundles.md",
  "docs/windows-cef-provenance.md",
  "docs/windows-signing.md",
  "docs/windows-distribution.md",
  "docs/menu-accessibility.md",
  "docs/ai-native.md",
  "docs/cli-describe.md",
  "docs/context-initialization.md",
  "docs/agent-tools.md",
  "docs/multiprocess.md",
  "docs/webview-services.md",
  "examples/webview.html",
  "examples/webview.rs",
  ICED_PATCH,
  "rust-toolchain.toml",
  "templates/native/src/main.rs",
  "templates/multiprocess/src/main.rs",
  "templates/multiprocess/src/crash_options.rs",
  "templates/multiprocess/src/bin/lintel-worker.rs",
  "templates/multiprocess/protocol/Cargo.toml",
  "templates/multiprocess/protocol/build.rs",
  "templates/multiprocess/protocol/src/lib.rs",
  "templates/multiprocess/protocol/schema/echo.mojom",
  "templates/agent/AGENTS.md",
  "templates/agent/llms.txt",
  "templates/webview/src/main.rs",
  "templates/webview/ui/index.html",
  "templates/webview-multiprocess/src/main.rs",
  "templates/webview-multiprocess/ui/index.html",
  "templates/windows-webview/src/lib.rs",
  "templates/windows-webview/ui/index.html",
  "templates/windows-webview/WINDOWS-CEF.md",
];

fn main() -> Result<()> {
  let checkout = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
  let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
  watch(&checkout.join("Cargo.toml"));
  watch(&checkout.join(PUBLIC_CONFIG));
  watch(&checkout.join(PUBLIC_LOCK));

  let policy = load_source_policy()?;
  verify_checkout_sources(&checkout, &checkout, &policy)?;
  let iced = checkout.join(ICED_PATH);
  if !iced.join("Cargo.toml").is_file() || !iced.join("LICENSE").is_file() {
    return Err("vendored iced source or its LICENSE is missing; run lintel prepare explicitly".into());
  }
  watch(&iced);

  let temporary = tempfile::NamedTempFile::new_in(&out)?;
  let gzip = GzBuilder::new()
    .mtime(0)
    .write(temporary.reopen()?, Compression::default());
  let mut builder = Builder::new(gzip);
  builder.mode(HeaderMode::Deterministic);
  let mut archive = Archive { builder, policy };
  for path in ["Cargo.toml", "Cargo.lock", PUBLIC_CONFIG, PUBLIC_LOCK] {
    append_checkout_file(&mut archive, &checkout, Path::new(path))?;
  }
  for path in FILES {
    append_checkout_file(&mut archive, &checkout, Path::new(path))?;
  }
  for crate_name in CRATES {
    let directory = Path::new("crates").join(crate_name);
    let manifest = directory.join("Cargo.toml");
    append_checkout_file(&mut archive, &checkout, &manifest)?;
    append_checkout_tree(&mut archive, &checkout, &directory.join("src"))?;
  }
  for path in ["src", "assets", "crates/lintel-iced-widgets/assets"] {
    append_checkout_tree(&mut archive, &checkout, Path::new(path))?;
  }
  append_tree(&mut archive, &iced, Path::new(ICED_PATH), false)?;
  archive.builder.into_inner()?.finish()?.flush()?;
  temporary.persist(out.join(ARCHIVE_NAME))?;
  Ok(())
}

fn watch(path: &Path) {
  println!("cargo:rerun-if-changed={}", path.display());
}

fn append_checkout_file(archive: &mut Archive, checkout: &Path, relative: &Path) -> Result<()> {
  let source = checkout.join(relative);
  watch(&source);
  append_file(archive, &source, relative)
}

fn append_checkout_tree(archive: &mut Archive, checkout: &Path, relative: &Path) -> Result<()> {
  let source = checkout.join(relative);
  watch(&source);
  append_tree(archive, &source, relative, true)
}

fn append_tree(archive: &mut Archive, source: &Path, relative: &Path, framework: bool) -> Result<()> {
  if !fs::symlink_metadata(source)?.is_dir() {
    return Err(format!("source directory is not a real directory: {}", source.display()).into());
  }
  let mut entries = fs::read_dir(source)?.collect::<std::io::Result<Vec<_>>>()?;
  entries.sort_by_key(fs::DirEntry::file_name);
  for entry in entries {
    let name = entry.file_name();
    let destination = relative.join(&name);
    SourcePolicy::check_path(&destination).map_err(std::io::Error::other)?;
    let kind = entry.file_type()?;
    if name.to_str().is_some_and(|name| matches!(name, "target" | "build")) {
      continue;
    }
    if framework
      && name
        .to_str()
        .is_none_or(|name| name.starts_with('.') || matches!(name, "target" | "build" | "third_party"))
    {
      continue;
    }
    if kind.is_dir() {
      append_tree(archive, &entry.path(), &destination, framework)?;
    } else if !kind.is_file() {
      return Err(format!("unsupported file type in source archive: {}", entry.path().display()).into());
    } else if framework {
      append_framework_source(archive, &entry.path(), &destination)?;
    } else {
      append_file(archive, &entry.path(), &destination)?;
    }
  }
  Ok(())
}

fn append_framework_source(archive: &mut Archive, source: &Path, destination: &Path) -> Result<()> {
  if is_source_file(destination) {
    append_file(archive, source, destination)?;
  }
  Ok(())
}

fn is_source_file(path: &Path) -> bool {
  matches!(
    path.extension().and_then(|extension| extension.to_str()),
    Some("rs" | "wgsl" | "svg" | "js" | "html")
  )
}

fn append_file(archive: &mut Archive, source: &Path, relative: &Path) -> Result<()> {
  if !fs::symlink_metadata(source)?.is_file() {
    return Err(format!("source file is not a regular file: {}", source.display()).into());
  }
  let mut file = File::open(source)?;
  let mut bytes = Vec::new();
  file.read_to_end(&mut bytes)?;
  archive.policy.check(relative, &bytes).map_err(std::io::Error::other)?;
  file.rewind()?;
  archive.builder.append_file(relative, &mut file)?;
  Ok(())
}

fn load_source_policy() -> Result<SourcePolicy> {
  const POLICY_ENV: &str = "LINTEL_RELEASE_DENYLIST";
  println!("cargo:rerun-if-env-changed={POLICY_ENV}");
  let Some(path) = env::var_os(POLICY_ENV).map(PathBuf::from) else {
    return Ok(SourcePolicy::default());
  };
  watch(&path);
  let source = fs::read_to_string(path).map_err(|_| std::io::Error::other("cannot read external source policy"))?;
  SourcePolicy::parse(&source).map_err(|error| std::io::Error::other(error).into())
}

fn verify_checkout_sources(checkout: &Path, directory: &Path, policy: &SourcePolicy) -> Result<()> {
  for entry in fs::read_dir(directory)? {
    let entry = entry?;
    let name = entry.file_name();
    if name.to_str().is_some_and(|name| {
      matches!(
        name,
        ".git" | ".ai" | "build" | "target" | "third_party" | "vendor" | "node_modules"
      )
    }) {
      continue;
    }
    let path = entry.path();
    let relative = path.strip_prefix(checkout)?;
    let kind = entry.file_type()?;
    if kind.is_dir() {
      verify_checkout_sources(checkout, &path, policy)?;
    } else if kind.is_file() {
      policy
        .check(relative, &fs::read(&path)?)
        .map_err(std::io::Error::other)?;
    } else {
      return Err(
        format!(
          "source checkout entry must be a real file or directory: {}",
          relative.display()
        )
        .into(),
      );
    }
  }
  Ok(())
}
