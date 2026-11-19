use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use lintel_lib_config::runtime_config::RuntimeConfig;
use lintel_lib_store::store::Store;
use tracing::level_filters::LevelFilter;

use crate::utils::process::Env;

#[derive(Debug, Clone)]
pub struct BuildInfo {
  pub git_branch: String,
  pub git_commit_id: String,
}

impl BuildInfo {
  pub fn new() -> Self {
    let git_branch = std::env::var("GIT_BRANCH").unwrap_or_else(|_| "unknown".to_string());
    let git_commit_id = std::env::var("GIT_COMMIT_ID").unwrap_or_else(|_| "unknown".to_string());
    Self {
      git_branch,
      git_commit_id,
    }
  }
}

#[derive(Debug, Clone)]
pub struct AppInfo {
  pub app_path: String,
  pub name: &'static str,
  pub exe_name: String,
  pub version: &'static str,
  pub app_id: u16,
  pub project_dir: ProjectDirs,
  pub frame_rate: u64,
  pub log_level: LogLevel,
  pub use_et_test: bool,
  pub device_id: u64,
  pub user_data: UserData,
  pub env: Env,
  pub runtime_config: RuntimeConfig,
  pub build_info: BuildInfo,
}

#[derive(Debug, Clone)]
pub struct LogLevel(LevelFilter);

impl LogLevel {
  pub fn info() -> Self {
    Self(LevelFilter::INFO)
  }
  pub fn debug() -> Self {
    Self(LevelFilter::DEBUG)
  }
  pub fn trace() -> Self {
    Self(LevelFilter::TRACE)
  }
  pub fn warn() -> Self {
    Self(LevelFilter::WARN)
  }
  pub fn error() -> Self {
    Self(LevelFilter::ERROR)
  }
  pub fn off() -> Self {
    Self(LevelFilter::OFF)
  }
  pub fn get_level(&self) -> LevelFilter {
    self.0
  }

  pub fn get_str(&self) -> &'static str {
    match self.0 {
      LevelFilter::TRACE => "trace",
      LevelFilter::DEBUG => "debug",
      LevelFilter::INFO => "info",
      LevelFilter::WARN => "warn",
      LevelFilter::ERROR => "error",
      LevelFilter::OFF => "off",
    }
  }
}

impl Default for LogLevel {
  fn default() -> Self {
    Self(LevelFilter::INFO)
  }
}

impl From<LevelFilter> for LogLevel {
  fn from(value: LevelFilter) -> Self {
    Self(value)
  }
}

impl From<LogLevel> for LevelFilter {
  fn from(value: LogLevel) -> Self {
    value.get_level()
  }
}

#[derive(Debug, Default)]
pub struct AppInfoBuilder {
  pub app_path: String,
  pub name: &'static str,
  pub exe_name: String,
  pub version: &'static str,
  pub app_id: u16,
  pub frame_rate: u64,
  pub log_level: LogLevel,
  pub use_et_test: bool,
  pub device_id: u64,
  pub app_info_store: Option<Store>,
}

impl AppInfoBuilder {
  pub fn app_path(mut self, app_path: String) -> Self {
    self.app_path = app_path;
    self
  }
  pub fn name(mut self, name: &'static str) -> Self {
    self.name = name;
    self
  }
  pub fn exe_name(mut self, exe_name: String) -> Self {
    self.exe_name = exe_name;
    self
  }
  pub fn version(mut self, version: &'static str) -> Self {
    self.version = version;
    self
  }
  pub fn app_id(mut self, app_id: u16) -> Self {
    self.app_id = app_id;
    self
  }

  pub fn frame_rate(mut self, rate: u64) -> Self {
    self.frame_rate = rate;
    self
  }

  pub fn log_level(mut self, level: LogLevel) -> Self {
    self.log_level = level;
    self
  }

  pub fn use_et_test(mut self, use_et_test: bool) -> Self {
    self.use_et_test = use_et_test;
    self
  }

  pub fn device_id(mut self, device_id: u64) -> Self {
    self.device_id = device_id;
    self
  }

  pub fn app_info_store(mut self, app_info_store: Store) -> Self {
    self.app_info_store = Some(app_info_store);
    self
  }

  pub fn build(self) -> AppInfo {
    self.build_configured(None, None)
  }

  /// 使用宿主提供的运行配置，跳过本地配置文件加载。
  pub fn build_with_runtime_config(self, runtime_config: RuntimeConfig) -> AppInfo {
    self.build_configured(None, Some(runtime_config))
  }

  /// 使用宿主选定的应用目录和运行配置。
  pub fn build_with_project_dirs(self, project_dir: ProjectDirs, runtime_config: RuntimeConfig) -> AppInfo {
    self.build_configured(Some(project_dir), Some(runtime_config))
  }

  fn build_configured(self, project_dir: Option<ProjectDirs>, runtime_config: Option<RuntimeConfig>) -> AppInfo {
    let project_dir = project_dir.unwrap_or_else(|| default_project_dirs(self.name));

    let runtime_config = runtime_config.unwrap_or_else(|| RuntimeConfig::from_config_dir(project_dir.config_dir()));

    #[cfg(debug_assertions)]
    tracing::debug!("Runtime_config: {:?} {:?}", project_dir.config_dir(), runtime_config);
    // 更新环境
    lintel_lib_config::env::ENV.update(runtime_config.env);
    let user_data = UserData::new(project_dir.clone(), self.app_info_store.unwrap());
    AppInfo {
      app_path: self.app_path,
      name: self.name,
      exe_name: self.exe_name,
      version: self.version,
      app_id: self.app_id,
      project_dir,
      frame_rate: if self.frame_rate > 0 { self.frame_rate } else { 60 },
      log_level: self.log_level,
      use_et_test: self.use_et_test,
      device_id: self.device_id,
      user_data,
      env: Env::default(),
      runtime_config,
      build_info: BuildInfo::new(),
    }
  }
}

fn default_project_dirs(name: &str) -> ProjectDirs {
  ProjectDirs::from("com", "lintel", name).expect("cannot find application directory")
}

#[cfg(test)]
mod project_directory_tests {
  use super::*;

  #[test]
  fn app_info_default_directories_use_the_lintel_namespace() {
    let actual = default_project_dirs("Example App");
    let expected = ProjectDirs::from("com", "lintel", "Example App").unwrap();
    assert_eq!(actual.config_dir(), expected.config_dir());
    assert_eq!(actual.data_local_dir(), expected.data_local_dir());
    assert_eq!(actual.cache_dir(), expected.cache_dir());
  }
}

pub struct DeviceInfo {
  device_id: i64,
}

impl DeviceInfo {}

#[derive(Debug, Clone)]
pub struct UserData {
  pub app_info_store: Store,
  project_dir: ProjectDirs,
}

impl UserData {
  fn new(project_dir: ProjectDirs, app_info_store: Store) -> Self {
    Self {
      project_dir,
      app_info_store,
    }
  }
  pub fn get_user_data_path(&self) -> String {
    #[cfg(target_os = "windows")]
    let roaming_config_path = self.project_dir.config_dir().parent().unwrap().display().to_string();
    #[cfg(target_os = "macos")]
    let roaming_config_path = self.project_dir.config_dir().display().to_string();
    #[cfg(target_os = "linux")]
    let roaming_config_path = self.project_dir.data_local_dir().display().to_string();
    let mut new_path = roaming_config_path.clone();
    if let Some(cwd_path) = self.app_info_store.get("user_data_path") {
      new_path = cwd_path.as_str().unwrap().to_string();
    }
    new_path
  }
  pub fn get_default_user_data_path(&self) -> String {
    #[cfg(target_os = "windows")]
    let ret = self
      .project_dir
      .config_dir()
      .parent()
      .unwrap()
      .display()
      .to_string()
      .clone();
    #[cfg(target_os = "macos")]
    let ret = self.project_dir.config_dir().display().to_string().clone();
    #[cfg(target_os = "linux")]
    let ret = self.project_dir.data_local_dir().display().to_string();
    ret
  }
  pub fn get_store_path(&self) -> PathBuf {
    let user_data_path = self.get_user_data_path().clone();
    let user_data_path = Path::new(&user_data_path);
    user_data_path.join("store")
  }
  pub fn get_cache_path(&self) -> PathBuf {
    let user_data_path = self.get_user_data_path().clone();
    let user_data_path = Path::new(&user_data_path);
    user_data_path.join("cache")
  }
  pub fn set_user_data_path(&self, new_path: String) {
    _ = self
      .app_info_store
      .set("user_data_path".to_string(), serde_json::Value::String(new_path));
  }
  pub fn set_clearup_flag_on(&self) {
    _ = self
      .app_info_store
      .set("should_cleanup_storage".to_string(), serde_json::Value::Bool(true));
  }
}
