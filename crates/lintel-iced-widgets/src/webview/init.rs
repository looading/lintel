use anyhow::{Context, Result};
use cef::ImplCommandLine as _;

use std::{
  path::{Path, PathBuf},
  sync::atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::webview::{
  CefError,
  browser::{AppBuilder, IcyBrowserProcessHandler, IcyCefApp},
};

// CEF supports one browser-process initialization per process. Publish the selected
// policy only after successful initialization, before exposing the runtime to callers.
// The regular initializer explicitly publishes false rather than inheriting a prior value.
static MEMORY_ONLY_CONTEXTS: AtomicBool = AtomicBool::new(false);

// Sandbox/bootstrap support follows the native platform, never a dependency-profile feature.
const CEF_NO_SANDBOX: bool = cfg!(not(any(target_os = "macos", target_os = "windows")));

pub(super) fn memory_only_contexts() -> bool {
  MEMORY_ONLY_CONTEXTS.load(Ordering::Acquire)
}

pub(super) fn request_context_cache_path(session: Option<&Path>, fallback: &Path) -> Option<PathBuf> {
  resolve_request_context_cache_path(memory_only_contexts(), session, fallback)
}

fn resolve_request_context_cache_path(memory_only: bool, session: Option<&Path>, fallback: &Path) -> Option<PathBuf> {
  if memory_only {
    None
  } else {
    Some(
      session
        .map(Path::to_path_buf)
        .unwrap_or_else(|| fallback.join("Default")),
    )
  }
}

#[cfg(target_os = "macos")]
pub fn pre_init_cef() -> Result<cef::library_loader::LibraryLoader> {
  let loader = cef::library_loader::LibraryLoader::new(
    &std::env::current_exe().expect("cannot get current exe"),
    has_helper_switch(std::env::args_os().skip(1)),
  );
  if !loader.load() {
    std::panic!("cannot load cef library");
  }
  let _ = cef::api_hash(cef::sys::CEF_API_VERSION_EXPERIMENTAL, 0);
  Ok(loader)
}

#[cfg(any(target_os = "macos", test))]
fn has_helper_switch(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
  arguments
    .into_iter()
    .any(|argument| argument == "--type" || argument.as_encoded_bytes().starts_with(b"--type="))
}

#[cfg(all(test, unix))]
mod argument_tests {
  use super::*;
  use std::os::unix::ffi::OsStringExt;

  #[test]
  fn helper_switch_scan_ignores_unrelated_non_unicode_arguments() {
    let unrelated = std::ffi::OsString::from_vec(vec![0xff, b'X']);
    assert!(has_helper_switch([unrelated.clone(), "--type=renderer".into()]));
    assert!(has_helper_switch([
      unrelated.clone(),
      "--type".into(),
      "crashpad-handler".into()
    ]));
    assert!(!has_helper_switch([unrelated, "--application-argument".into()]));
  }
}

#[cfg(target_os = "windows")]
pub fn pre_init_cef() -> Result<()> {
  let _ = super::windows_bootstrap::sandbox_info()?;
  let _ = cef::api_hash(cef::sys::CEF_API_VERSION_EXPERIMENTAL, 0);
  // Validate inherited native switches before CefExecuteProcess can start a
  // browser/helper. Legitimate CEF service-sandbox-type flags remain untouched.
  let _ = super::windows_bootstrap::command_line()?;
  Ok(())
}

pub enum BrowserProcessMessage {
  Ready,
  Tick(std::time::Instant),
}

pub fn init_cef(
  app_version: &'static str,
  device_id: Option<String>,
  cache_path: Option<&Path>,
) -> Result<
  Option<(
    IcyCefApp,
    UnboundedReceiver<BrowserProcessMessage>,
    tokio::sync::oneshot::Receiver<super::ChromiumGpuInfo>,
  )>,
> {
  init_cef_storage(app_version, device_id, cache_path, None)
}

/// 使用独立持久根保存 CEF 安装数据/Crashpad，浏览器缓存保持 incognito。
/// cache_path 仅保存本次运行日志；CEF 143 会忽略 root 下任意子 profile，故不把它当持久浏览器缓存。
pub fn init_cef_with_root_cache(
  app_version: &'static str,
  device_id: Option<String>,
  cache_path: Option<&Path>,
  root_cache_path: &Path,
) -> Result<
  Option<(
    IcyCefApp,
    UnboundedReceiver<BrowserProcessMessage>,
    tokio::sync::oneshot::Receiver<super::ChromiumGpuInfo>,
  )>,
> {
  init_cef_storage(app_version, device_id, cache_path, Some(root_cache_path))
}

fn init_cef_storage(
  app_version: &'static str,
  device_id: Option<String>,
  cache_path: Option<&Path>,
  persistent_root: Option<&Path>,
) -> Result<
  Option<(
    IcyCefApp,
    UnboundedReceiver<BrowserProcessMessage>,
    tokio::sync::oneshot::Receiver<super::ChromiumGpuInfo>,
  )>,
> {
  #[cfg(target_os = "windows")]
  let args = cef::args::Args::from(super::windows_bootstrap::main_args()?);
  #[cfg(not(target_os = "windows"))]
  let args = cef::args::Args::new();
  #[cfg(target_os = "windows")]
  let cmd = super::windows_bootstrap::command_line()?;
  #[cfg(not(target_os = "windows"))]
  let cmd = args.as_cmd_line().context("cannot get command line")?;
  #[cfg(target_os = "windows")]
  let windows_sandbox_info = super::windows_bootstrap::sandbox_info()?;
  #[cfg(not(target_os = "windows"))]
  let windows_sandbox_info = std::ptr::null_mut();
  let is_browser_process = cmd.has_switch(Some(&cef::CefString::from("type"))) != 1;

  if !is_browser_process {
    let ret = cef::execute_process(
      Some(args.as_main_args()),
      Some(&mut super::renderer_process::ChildProcessApp::new(app_version)),
      windows_sandbox_info,
    );
    if ret < 0 {
      anyhow::bail!("cannot launch cef");
    }

    // non-browser process does not initialize cef
    return Ok(None);
  }

  let Some(cache_path) = cache_path else {
    anyhow::bail!("no cache path")
  };
  if !cache_path.exists() {
    _ = std::fs::create_dir_all(cache_path);
  }
  let root_cache_path = persistent_root.unwrap_or(cache_path);
  if persistent_root.is_some() {
    let root = root_cache_path
      .canonicalize()
      .context("cannot resolve CEF root cache")?;
    let cache = cache_path.canonicalize().context("cannot resolve CEF runtime cache")?;
    anyhow::ensure!(
      root.is_absolute() && cache.starts_with(&root),
      "CEF runtime cache must remain under its persistent root"
    );
  }

  let (browser_handler, browser_message_rx, gpu_info_rx) = IcyBrowserProcessHandler::new(device_id);
  let app = IcyCefApp::new();
  let mut cef_app = AppBuilder::build(app.clone(), browser_handler);
  let ret = cef::execute_process(Some(args.as_main_args()), Some(&mut cef_app), windows_sandbox_info);
  if ret != -1 {
    return Err(CefError::ProcessLaunchFailed).context("initialization");
  }

  let log_file = cache_path
    .join("logs")
    .join("applogs")
    .join("cef.log")
    .to_str()
    .map(cef::CefString::from)
    .unwrap();

  let chrome_version = cef::sys::CHROME_VERSION_MAJOR;
  let settings = cef::Settings {
    root_cache_path: root_cache_path.to_str().map(cef::CefString::from).unwrap(),
    cache_path: if persistent_root.is_some() {
      Default::default()
    } else {
      cache_path.to_str().map(cef::CefString::from).unwrap()
    },
    #[cfg(debug_assertions)]
    log_severity: cef::sys::cef_log_severity_t::LOGSEVERITY_DEBUG.into(),
    #[cfg(not(debug_assertions))]
    log_severity: cef::sys::cef_log_severity_t::LOGSEVERITY_INFO.into(),
    persist_session_cookies: true as _,
    #[cfg(not(target_os = "windows"))]
    external_message_pump: true as _,
    #[cfg(target_os = "windows")]
    multi_threaded_message_loop: true as _,
    user_agent_product: format!("Chrome/{chrome_version}.0.0.0 lintel/{app_version}")
      .as_str()
      .into(),
    accept_language_list: "zh-CN".into(),
    windowless_rendering_enabled: true as _,
    no_sandbox: CEF_NO_SANDBOX as _,
    log_file,
    locale: "zh_CN".into(),
    ..Default::default()
  };
  let ret = cef::initialize(
    Some(args.as_main_args()),
    Some(&settings),
    Some(&mut cef_app),
    windows_sandbox_info,
  );
  if ret != 1 {
    anyhow::bail!("cannot initialize cef");
  }
  MEMORY_ONLY_CONTEXTS.store(persistent_root.is_some(), Ordering::Release);
  Ok(Some((app, browser_message_rx, gpu_info_rx)))
}

#[cfg(test)]
mod storage_tests {
  use super::*;

  #[cfg(any(target_os = "macos", target_os = "windows"))]
  #[test]
  fn official_desktop_cef_keeps_sandbox_enabled_without_profile_feature_requirements() {
    assert!(!CEF_NO_SANDBOX);
  }

  #[test]
  fn crashpad_incognito_context_never_falls_back_to_runtime_or_partition_profile() {
    let fallback = Path::new("runtime");
    let partition = Path::new("runtime/partition");
    assert_eq!(resolve_request_context_cache_path(true, None, fallback), None);
    assert_eq!(
      resolve_request_context_cache_path(true, Some(partition), fallback),
      None
    );
    assert_eq!(
      resolve_request_context_cache_path(false, None, fallback),
      Some(fallback.join("Default"))
    );
    assert_eq!(
      resolve_request_context_cache_path(false, Some(partition), fallback),
      Some(partition.to_path_buf())
    );
  }
}
