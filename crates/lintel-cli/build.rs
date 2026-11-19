use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File};
use std::io::{self, Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};

use flate2::{Compression, GzBuilder, write::GzEncoder};
use sha2::{Digest, Sha256};
use tar::{Builder, HeaderMode};
mod source_policy;

use source_policy::SourcePolicy;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
struct Archive {
  builder: Builder<GzEncoder<File>>,
  policy: SourcePolicy,
  entries: BTreeSet<PathBuf>,
}

const ARCHIVE_NAME: &str = "lintel-framework.tar.gz";
const ICED_PATH: &str = "vendor/iced";
const PUBLIC_CONFIG: &str = ".cargo/macos-public-dependencies.toml";
const PUBLIC_LOCK: &str = "profiles/macos-public/Cargo.lock";

const SOURCES: &str = include_str!("resources/framework-sources.json");
const MAX_ARCHIVE_BYTES: u64 = 9_750_000;
const MAX_MEMBER_BYTES: u64 = 32 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 128 * 1024 * 1024;
const OUT_TEXT_FILES: &[&str] = &[
  "lintel.project.json",
  "templates/native/src/main.rs",
  "templates/multiprocess/src/main.rs",
  "templates/webview/src/main.rs",
  "templates/webview/ui/index.html",
  "templates/webview-multiprocess/src/main.rs",
  "templates/webview-multiprocess/ui/index.html",
  "templates/windows-webview/src/lib.rs",
  "templates/windows-webview/ui/index.html",
];

struct Sources {
  crates: Vec<String>,
  files: Vec<String>,
  resource_trees: Vec<String>,
}

fn sources() -> Result<Sources> {
  let value: serde_json::Value = serde_json::from_str(SOURCES)?;
  if value["schema_version"] != 1 {
    return Err("unsupported framework source recipe".into());
  }
  let strings = |key: &str| -> Result<Vec<String>> {
    let values = value[key].as_array().ok_or("framework source recipe requires arrays")?;
    if values.is_empty() || values.len() > 512 {
      return Err("invalid framework source recipe size".into());
    }
    values
      .iter()
      .map(|value| {
        let path = value.as_str().ok_or("framework source path must be a string")?;
        if path.is_empty() {
          return Err("empty framework source path".into());
        }
        SourcePolicy::check_path(Path::new(path)).map_err(io::Error::other)?;
        Ok(path.to_owned())
      })
      .collect()
  };
  Ok(Sources {
    crates: strings("crates")?,
    files: strings("files")?,
    resource_trees: strings("resource_trees")?,
  })
}

fn main() -> Result<()> {
  let package = Path::new(env!("CARGO_MANIFEST_DIR"));
  let out = PathBuf::from(env::var_os("OUT_DIR").ok_or("OUT_DIR is not set")?);
  watch(&package.join("resources/framework-sources.json"));
  let recipe = sources()?;
  let policy = load_source_policy()?;
  // Registry/package verification layouts are recognized before considering any ancestor.
  // A normal crate copy also uses only its package-local prepared resources.
  let checkout = if !package.join(".cargo_vcs_info.json").exists() && package.ends_with(Path::new("crates/lintel-cli"))
  {
    package
      .parent()
      .and_then(Path::parent)
      .filter(|root| root.join("lintel.project.json").is_file())
  } else {
    None
  };
  if let Some(root) = checkout {
    build_checkout_archive(root, &out, &recipe, policy)?;
  } else {
    let archive = package.join("resources").join(ARCHIVE_NAME);
    let checksum = package.join("resources/lintel-framework.sha256");
    watch(&archive);
    watch(&checksum);
    let checksum = read_real_file(&checksum, 128)?;
    let checksum = std::str::from_utf8(&checksum)?.trim();
    if checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
      return Err("invalid prepared framework SHA-256".into());
    }
    let bytes = read_real_file(&archive, MAX_ARCHIVE_BYTES)?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(checksum) {
      return Err("prepared framework archive SHA-256 mismatch".into());
    }
    let views = verify_archive(&bytes, &recipe, &policy)?;
    write_views(&out, views)?;
    write_output(&out.join(ARCHIVE_NAME), &bytes)?;
  }
  Ok(())
}

fn build_checkout_archive(checkout: &Path, out: &Path, recipe: &Sources, policy: SourcePolicy) -> Result<()> {
  watch(&checkout.join("Cargo.toml"));
  watch(&checkout.join(PUBLIC_CONFIG));
  watch(&checkout.join(PUBLIC_LOCK));
  verify_package_resources(checkout)?;
  verify_checkout_sources(checkout, checkout, &policy)?;
  let iced = checkout.join(ICED_PATH);
  if !iced.join("Cargo.toml").is_file() || !iced.join("LICENSE").is_file() {
    return Err("vendored iced source or its LICENSE is missing; run lintel prepare explicitly".into());
  }
  watch(&iced);
  let temporary = tempfile::NamedTempFile::new_in(out)?;
  let gzip = GzBuilder::new()
    .mtime(0)
    .write(temporary.reopen()?, Compression::best());
  let mut builder = Builder::new(gzip);
  builder.mode(HeaderMode::Deterministic);
  let mut archive = Archive {
    builder,
    policy,
    entries: BTreeSet::new(),
  };
  for path in ["Cargo.toml", "Cargo.lock", PUBLIC_CONFIG, PUBLIC_LOCK] {
    append_checkout_file(&mut archive, checkout, Path::new(path))?;
  }
  for path in &recipe.files {
    append_checkout_file(&mut archive, checkout, Path::new(path))?;
  }
  for crate_name in &recipe.crates {
    let directory = Path::new("crates").join(crate_name);
    append_checkout_file(&mut archive, checkout, &directory.join("Cargo.toml"))?;
    append_checkout_tree(&mut archive, checkout, &directory.join("src"))?;
    for notice in ["LICENSE", "README.md"] {
      let relative = directory.join(notice);
      if checkout.join(&relative).is_file() {
        append_checkout_file(&mut archive, checkout, &relative)?;
      }
    }
  }
  for path in ["src", "assets", "crates/lintel-iced-widgets/assets"] {
    append_checkout_tree(&mut archive, checkout, Path::new(path))?;
  }
  for relative in &recipe.resource_trees {
    let path = checkout.join(relative);
    watch(&path);
    append_tree(&mut archive, &path, Path::new(relative), false)?;
  }
  append_tree(&mut archive, &iced, Path::new(ICED_PATH), false)?;
  let Archive { builder, policy, .. } = archive;
  builder.into_inner()?.finish()?.flush()?;
  let bytes = read_real_file(temporary.path(), MAX_ARCHIVE_BYTES)?;
  let views = verify_archive(&bytes, recipe, &policy)?;
  write_views(out, views)?;
  temporary.persist(out.join(ARCHIVE_NAME))?;
  Ok(())
}

fn verify_package_resources(checkout: &Path) -> Result<()> {
  let resources = checkout.join("crates/lintel-build/resources");
  let map = resources.join("source-map.json");
  watch(&map);
  let map: serde_json::Value = serde_json::from_slice(&fs::read(map)?)?;
  if map["schema_version"] != 1 {
    return Err("invalid package resource source map".into());
  }
  for path in map["files"].as_array().ok_or("resource source map requires files")? {
    let path = path.as_str().ok_or("resource source path must be a string")?;
    SourcePolicy::check_path(Path::new(path)).map_err(io::Error::other)?;
    let source = checkout.join(path);
    let resource = resources.join(path);
    watch(&source);
    watch(&resource);
    if fs::read(source)? != fs::read(resource)? {
      return Err(format!("stale package resource {path}; run the explicit release preparation --sync step").into());
    }
  }
  for relative in ["crates/lintel-build/LICENSE", "crates/lintel-cli/resources/LICENSE"] {
    watch(&checkout.join(relative));
    if fs::read(checkout.join(relative))? != fs::read(checkout.join("LICENSE"))? {
      return Err("package MIT license does not match the repository LICENSE".into());
    }
  }
  Ok(())
}

fn read_real_file(path: &Path, limit: u64) -> Result<Vec<u8>> {
  if !fs::symlink_metadata(path)?.is_file() {
    return Err("prepared resource must be a regular file".into());
  }
  let mut bytes = Vec::new();
  File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
  if bytes.is_empty() || bytes.len() as u64 > limit {
    return Err("prepared resource is empty or exceeds its size budget".into());
  }
  Ok(bytes)
}

fn verify_archive(bytes: &[u8], recipe: &Sources, policy: &SourcePolicy) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
  let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(Cursor::new(bytes)));
  let mut entries = BTreeSet::new();
  let mut views = BTreeMap::new();
  let mut expanded = 0_u64;
  for entry in archive.entries()? {
    let entry = entry?;
    let path = entry.path()?.into_owned();
    SourcePolicy::check_path(&path).map_err(io::Error::other)?;
    if !entry.header().entry_type().is_file() || !entries.insert(path.clone()) {
      return Err("prepared framework has a special or duplicate member".into());
    }
    let size = entry.header().size()?;
    expanded = expanded.checked_add(size).ok_or("framework size overflow")?;
    if size > MAX_MEMBER_BYTES || expanded > MAX_UNPACKED_BYTES || entries.len() > 10_000 {
      return Err("prepared framework exceeds its source size budget".into());
    }
    let mut content = Vec::new();
    entry.take(MAX_MEMBER_BYTES + 1).read_to_end(&mut content)?;
    if content.len() as u64 != size {
      return Err("prepared framework member size mismatch".into());
    }
    policy.check(&path, &content).map_err(io::Error::other)?;
    if OUT_TEXT_FILES.iter().any(|file| path == Path::new(file)) {
      if content.is_empty() || std::str::from_utf8(&content)?.contains('\0') {
        return Err("embedded CLI template must be nonempty UTF-8 text".into());
      }
      views.insert(path, content);
    }
  }
  io::copy(&mut archive.into_inner(), &mut io::sink())?;
  for required in [
    "Cargo.toml",
    "Cargo.lock",
    "src/lib.rs",
    "LICENSE",
    "vendor/iced/Cargo.toml",
    "vendor/iced/LICENSE",
  ] {
    if !entries.contains(Path::new(required)) {
      return Err(format!("prepared framework is missing {required}").into());
    }
  }
  for required in &recipe.files {
    if !entries.contains(Path::new(required)) {
      return Err(format!("prepared framework is missing {required}").into());
    }
  }
  for name in &recipe.crates {
    if !entries.contains(&Path::new("crates").join(name).join("Cargo.toml")) {
      return Err(format!("prepared framework is missing crate {name}").into());
    }
  }
  if views.len() != OUT_TEXT_FILES.len() {
    return Err("prepared framework is missing CLI templates/catalog".into());
  }
  Ok(views)
}

fn write_views(out: &Path, views: BTreeMap<PathBuf, Vec<u8>>) -> Result<()> {
  for (path, bytes) in views {
    write_output(&out.join(path), &bytes)?;
  }
  Ok(())
}

fn write_output(path: &Path, bytes: &[u8]) -> Result<()> {
  let parent = path.parent().ok_or("generated view has no parent")?;
  fs::create_dir_all(parent)?;
  let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
  temporary.write_all(bytes)?;
  temporary.flush()?;
  temporary.persist(path)?;
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
  if archive.entries.insert(relative.to_path_buf()) {
    file.rewind()?;
    archive.builder.append_file(relative, &mut file)?;
  }
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
    // Prepared registry payloads are generated artifacts, never checkout build inputs.
    if matches!(
      relative.to_str(),
      Some(
        "crates/lintel-cli/resources/lintel-framework.tar.gz" | "crates/lintel-cli/resources/lintel-framework.sha256"
      )
    ) {
      continue;
    }
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
