use std::{env, path::PathBuf};

fn main() {
  if cfg!(feature = "public-cef") {
    return;
  }

  for key in dawn_env_keys() {
    println!("cargo:rerun-if-env-changed={key}");
  }

  let Some(dawn_root) = dawn_root() else {
    println!("cargo:warning=DAWN_ROOT for current target is not set; Dawn examples may fail to link");
    return;
  };

  let dawn_lib_dir = dawn_root.join("lib");
  println!("cargo:rustc-link-search=native={}", dawn_lib_dir.display());
  println!("cargo:rustc-link-lib=dylib={}", dawn_link_lib_name());

  if target_os().as_deref() == Some("macos") {
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", dawn_lib_dir.display());
  }
}

fn dawn_root() -> Option<PathBuf> {
  env::var_os("DAWN_ROOT")
    .map(PathBuf::from)
    .or_else(|| current_target_dawn_root().and_then(env::var_os).map(PathBuf::from))
}

fn dawn_link_lib_name() -> &'static str {
  if target_os().as_deref() == Some("windows") {
    "webgpu_dawn.dll"
  } else {
    "webgpu_dawn"
  }
}

fn current_target_dawn_root() -> Option<String> {
  let os = target_os()?;
  let arch = target_arch()?;
  Some(dawn_root_key(&os, &arch))
}

fn dawn_env_keys() -> Vec<String> {
  let mut keys = vec!["DAWN_ROOT".to_string()];
  if let Some(key) = current_target_dawn_root() {
    keys.push(key);
  }
  keys
}

fn dawn_root_key(os: &str, arch: &str) -> String {
  format!("DAWN_ROOT_{}_{}", os, dawn_arch(arch))
}

fn dawn_arch(arch: &str) -> &str {
  match arch {
    "x86" | "i686" => "x86",
    other => other,
  }
}

fn target_os() -> Option<String> {
  env::var("CARGO_CFG_TARGET_OS").ok()
}

fn target_arch() -> Option<String> {
  env::var("CARGO_CFG_TARGET_ARCH").ok()
}
