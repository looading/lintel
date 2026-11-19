use anyhow::Context;
use std::{
  path::{Path, PathBuf},
  sync::OnceLock,
};

mod env_vars;

pub use env_vars::{
  DEFAULT_GLOBAL_WEBVIEW_MAX_CONCURRENT, GLOBAL_WEBVIEW_MAX_CONCURRENT_ENV, WEBVIEW_TEST_URL_ENV,
  global_webview_max_concurrent, webview_test_url_override,
};

#[derive(Debug)]
pub struct LintelEnv {
  app_id: u16,
  name: &'static str,
  device_id: u64,
  version: semver::Version,
  #[cfg(target_os = "macos")]
  frameworks_path: PathBuf,
  resources_path: PathBuf,
  user_data_path: PathBuf,
  cef_user_data_path: PathBuf,
  download_path: PathBuf,
}

static LINTEL_ENV: OnceLock<LintelEnv> = OnceLock::new();

impl LintelEnv {
  pub fn init(
    app_id: u16,
    version: semver::Version,
    exe_path: &Path,
    user_data_path: &Path,
    cef_user_data_path: &Path,
    device_id: u64,
    download_path: &Path,
  ) -> anyhow::Result<()> {
    LINTEL_ENV
      .set(LintelEnv {
        name: "Lintel",
        app_id,
        version,
        device_id,
        user_data_path: user_data_path.to_path_buf(),
        cef_user_data_path: cef_user_data_path.to_path_buf(),
        #[cfg(target_os = "macos")]
        resources_path: exe_path.join("../../Resources").canonicalize()?,
        #[cfg(target_os = "windows")]
        resources_path: exe_path.join("../resources").canonicalize()?,
        #[cfg(target_os = "linux")]
        resources_path: exe_path
          .parent()
          .context("executable has no resource parent")?
          .canonicalize()?
          .join("resources"),
        #[cfg(target_os = "macos")]
        frameworks_path: exe_path.join("../../Frameworks").canonicalize()?,
        download_path: download_path.canonicalize()?,
      })
      .map_err(|env| anyhow::anyhow!("{env:?}"))
      .context("cannot initialize lintel env")?;
    Ok(())
  }

  pub fn init_for_test() -> anyhow::Result<()> {
    let tmp = std::env::temp_dir();
    let tmp = tmp.as_path().join("lintel-test");

    std::fs::create_dir_all(tmp.as_path())?;

    let _ = std::fs::create_dir_all(tmp.join("user_data_path"));

    #[cfg(target_os = "macos")]
    let _ = std::fs::create_dir_all(tmp.join("Frameworks"));

    let _ = std::fs::create_dir_all(tmp.join("assets"));
    let _ = std::fs::create_dir_all(tmp.join("download_path"));

    let val = LintelEnv {
      name: "Lintel",
      app_id: 1,
      version: semver::Version::new(0, 1, 0),
      user_data_path: tmp.join("user_data_path").canonicalize().context("user_data_path")?,
      cef_user_data_path: tmp.join("user_data_path").canonicalize().context("user_data_path")?,
      resources_path: tmp.join("assets").canonicalize().context("assets")?,
      #[cfg(target_os = "macos")]
      frameworks_path: tmp.join("Frameworks").canonicalize().context("Frameworks")?,
      device_id: 42,
      download_path: tmp.join("download_path").canonicalize().context("download_path")?,
    };

    #[cfg(target_os = "macos")]
    {
      let _ = std::fs::create_dir_all(val.frameworks_path.as_path());
    }
    let _ = std::fs::create_dir_all(val.download_path.as_path());
    let _ = std::fs::create_dir_all(val.resources_path.as_path());
    let _ = std::fs::create_dir_all(val.user_data_path.as_path());

    println!("{val:?}");

    LINTEL_ENV
      .set(val)
      .map_err(|env| anyhow::anyhow!("{env:?}"))
      .context("cannot initialize lintel env")?;
    Ok(())
  }

  pub fn version() -> &'static semver::Version {
    LINTEL_ENV
      .get()
      .map(|env| &env.version)
      .expect("lintel env not initialized")
  }

  pub fn app_id() -> u16 {
    LINTEL_ENV
      .get()
      .map(|env| env.app_id)
      .expect("lintel env not initialized")
  }

  pub fn app_name() -> &'static str {
    LINTEL_ENV
      .get()
      .map(|env| env.name)
      .expect("lintel env not initialized")
  }

  pub fn resources_path() -> &'static Path {
    LINTEL_ENV
      .get()
      .map(|env| &env.resources_path)
      .expect("lintel env not initialized")
  }

  pub fn user_data_path() -> &'static Path {
    LINTEL_ENV
      .get()
      .map(|env| &env.user_data_path)
      .expect("lintel env not initialized")
  }

  pub fn cef_user_data_path() -> &'static Path {
    LINTEL_ENV
      .get()
      .map(|env| &env.cef_user_data_path)
      .expect("lintel env not initialized")
  }

  #[cfg(target_os = "macos")]
  pub fn frameworks_path() -> &'static Path {
    LINTEL_ENV
      .get()
      .map(|env| &env.frameworks_path)
      .expect("lintel env not initialized")
  }

  pub fn device_id() -> u64 {
    LINTEL_ENV
      .get()
      .map(|env| env.device_id)
      .expect("lintel env not initialized")
  }

  pub fn download_path() -> &'static Path {
    LINTEL_ENV
      .get()
      .map(|env| &env.download_path)
      .expect("lintel env not initialized")
  }
}
