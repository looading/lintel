//! Verify or prepare the pinned public iced adapter in the vendored source tree.

use std::{
  collections::BTreeMap,
  env,
  fs::{self, File, OpenOptions},
  io,
  path::{Component, Path, PathBuf},
  process::{Command, Output, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const ICED_REV: &str = "90037336e80f02b817f2f68a5adf877bdb85639a";
const ICED_URL: &str = "https://github.com/iced-rs/iced.git";
const MARKER: &str = ".lintel-iced-profile.json";
const REQUIRED_CRATES: [&str; 7] = ["", "core", "debug", "program", "runtime", "wgpu", "winit"];

/// Verify the vendored iced tree, or prepare it from the pinned public source when absent.
pub fn prepare_public_iced(checkout: &Path) -> Result<PathBuf> {
  let checkout = checkout
    .canonicalize()
    .with_context(|| format!("locate Lintel checkout {}", checkout.display()))?;
  let patch = checkout.join("profiles/macos-public/iced.patch");
  let patch_text = fs::read_to_string(&patch).with_context(|| format!("read {}", patch.display()))?;
  let patch_hash = sha256(patch_text.as_bytes());
  let patched_files: Vec<_> = patch_text
    .lines()
    .filter_map(|line| line.strip_prefix("+++ b/"))
    .collect();
  ensure!(!patched_files.is_empty(), "iced patch contains no target files");
  for file in &patched_files {
    ensure!(
      !file.is_empty()
        && Path::new(file)
          .components()
          .all(|part| matches!(part, Component::Normal(_))),
      "invalid iced patch target {file:?}"
    );
  }

  let parent = checkout.join("vendor");
  let destination = parent.join("iced");
  fs::create_dir_all(&parent)?;
  let lock = OpenOptions::new()
    .read(true)
    .write(true)
    .create(true)
    .truncate(false)
    .open(parent.join(".prepare.lock"))?;
  lock.lock().context("lock macOS public iced source preparation")?;

  match fs::symlink_metadata(&destination) {
    Ok(metadata) => {
      if metadata.is_dir() && is_ready(&destination, &patch_hash, &patched_files) {
        return Ok(destination);
      }
      bail!(
        "move the unexpected generated source aside before retrying: {}",
        destination.display()
      );
    }
    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
    Err(error) => return Err(error).with_context(|| format!("inspect {}", destination.display())),
  }

  let stage = tempfile::Builder::new().prefix(".prepare-").tempdir_in(&parent)?;
  let repository = source_repository(stage.path())?;
  let archive = stage.path().join("iced.tar");
  checked(
    git()
      .arg("-C")
      .arg(repository)
      .args(["archive", "--format=tar", ICED_REV])
      .stdout(Stdio::from(File::create(&archive)?)),
  )?;

  let tree = stage.path().join("source");
  fs::create_dir(&tree)?;
  checked(Command::new("tar").arg("-xf").arg(&archive).arg("-C").arg(&tree))?;
  let staged_patch = stage.path().join("iced.patch");
  fs::write(&staged_patch, patch_text.as_bytes())?;
  checked(
    Command::new("patch")
      .args(["-f", "-s", "-F", "0", "-p1", "-d"])
      .arg(&tree)
      .arg("-i")
      .arg(&staged_patch),
  )?;
  ensure!(
    has_required_crates(&tree),
    "prepared iced archive is missing required crate manifests"
  );
  let mut marker = serde_json::to_vec_pretty(&source_marker(&tree, &patch_hash, &patched_files)?)?;
  marker.push(b'\n');
  fs::write(tree.join(MARKER), marker)?;
  fs::rename(&tree, &destination).with_context(|| format!("publish prepared iced source {}", destination.display()))?;
  Ok(destination)
}

fn has_required_crates(tree: &Path) -> bool {
  REQUIRED_CRATES
    .iter()
    .all(|directory| tree.join(directory).join("Cargo.toml").is_file())
}

fn is_ready(tree: &Path, patch_hash: &str, patched_files: &[&str]) -> bool {
  if !has_required_crates(tree) {
    return false;
  }
  let Ok(marker) = fs::read(tree.join(MARKER)) else {
    return false;
  };
  let Ok(marker) = serde_json::from_slice::<Value>(&marker) else {
    return false;
  };
  source_marker(tree, patch_hash, patched_files).is_ok_and(|actual| marker == actual)
}

fn source_marker(tree: &Path, patch_hash: &str, patched_files: &[&str]) -> Result<Value> {
  let mut files = BTreeMap::new();
  for &file in patched_files {
    let path = tree.join(file);
    let bytes = fs::read(&path).with_context(|| format!("read patched iced file {}", path.display()))?;
    files.insert(file, sha256(&bytes));
  }
  Ok(json!({ "source": ICED_URL, "revision": ICED_REV, "patch_sha256": patch_hash, "files": files }))
}

fn sha256(bytes: &[u8]) -> String {
  format!("{:x}", Sha256::digest(bytes))
}

fn source_repository(staging: &Path) -> Result<PathBuf> {
  let commit = format!("{ICED_REV}^{{commit}}");
  if let Some(home) = cargo_home() {
    let cache = home.join("git");
    let mut candidates = matching_directories(&cache.join("db"), "iced-");
    for checkout in matching_directories(&cache.join("checkouts"), "iced-") {
      candidates.extend(matching_directories(&checkout, &ICED_REV[..7]));
    }
    for candidate in candidates {
      let status = git()
        .arg("-C")
        .arg(&candidate)
        .args(["cat-file", "-e", &commit])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("check pinned iced commit in Cargo Git cache")?;
      if status.success() {
        return Ok(candidate);
      }
    }
  }

  if env::var("CARGO_NET_OFFLINE").is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true")) {
    bail!(
      "pinned iced commit {ICED_REV} is missing from the offline Cargo Git cache. Populate the cache or rerun with CARGO_NET_OFFLINE=false to allow the fetch."
    );
  }

  // Fetch in staging without modifying an existing Cargo checkout or Git database.
  let repository = staging.join("iced.git");
  checked(git().args(["init", "--quiet", "--bare"]).arg(&repository))?;
  checked(
    git()
      .arg("-C")
      .arg(&repository)
      .args(["fetch", "--no-tags", "--depth=1", ICED_URL, ICED_REV]),
  )?;
  let fetched = checked(
    git()
      .arg("-C")
      .arg(&repository)
      .args(["rev-parse", "FETCH_HEAD^{commit}"]),
  )?;
  let fetched = std::str::from_utf8(&fetched.stdout)?.trim();
  ensure!(
    fetched == ICED_REV,
    "iced fetch returned {fetched}, expected {ICED_REV}"
  );
  Ok(repository)
}

fn cargo_home() -> Option<PathBuf> {
  let home = env::var_os("HOME").map(PathBuf::from);
  match env::var_os("CARGO_HOME").map(PathBuf::from) {
    Some(path) => match path.strip_prefix("~") {
      Ok(suffix) => home.map(|home| home.join(suffix)),
      Err(_) => Some(path),
    },
    None => home.map(|home| home.join(".cargo")),
  }
}

fn matching_directories(directory: &Path, prefix: &str) -> Vec<PathBuf> {
  let Ok(entries) = fs::read_dir(directory) else {
    return Vec::new();
  };
  let mut matches: Vec<_> = entries
    .filter_map(|entry| entry.ok())
    .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
    .map(|entry| entry.path())
    .filter(|path| path.is_dir())
    .collect();
  matches.sort();
  matches
}

fn git() -> Command {
  let mut command = Command::new("git");
  command.arg("--no-replace-objects");
  for key in [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
  ] {
    command.env_remove(key);
  }
  command
}

fn checked(command: &mut Command) -> Result<Output> {
  let output = command.output().with_context(|| format!("run {command:?}"))?;
  ensure!(
    output.status.success(),
    "{command:?} failed with {}: {}{}",
    output.status,
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  Ok(output)
}
