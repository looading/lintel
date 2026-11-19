use std::{
  fmt,
  path::{Path, PathBuf},
  str::FromStr,
  sync::Arc,
};

use serde::{Deserialize, Serialize};
use tracing::{error, info};
use urlpattern::UrlPattern;

use crate::env::EnvType;

pub const DEFAULT_RUNTIME_CONFIG_FILE: &str = "lintel-runtime.yml";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeConfig {
  #[serde(alias = "env", default)]
  pub env: EnvType,
  #[serde(alias = "enableDebugPanel", default)]
  pub enable_debug_panel: bool,
  #[serde(alias = "uiPerfConfig", default)]
  pub ui_perf_config: UiPerfConfig,
  #[serde(default)]
  pub mcp: McpRuntimeConfig,
  #[serde(default)]
  pub proxy: Proxy,
}

/// MCP runtime 配置。
#[derive(Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct McpRuntimeConfig {
  #[serde(default)]
  pub enabled: bool,
  #[serde(default)]
  pub host: Option<String>,
  #[serde(default)]
  pub port: Option<u16>,
  #[serde(default)]
  pub token: Option<String>,
  #[serde(alias = "tokenFile", default)]
  pub token_file: Option<PathBuf>,
  #[serde(alias = "endpointFile", default)]
  pub endpoint_file: Option<PathBuf>,
}

impl fmt::Debug for McpRuntimeConfig {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("McpRuntimeConfig")
      .field("enabled", &self.enabled)
      .field("host", &self.host)
      .field("port", &self.port)
      .field("token", &self.token.as_ref().map(|_| "<redacted>"))
      .field("token_file", &self.token_file)
      .field("endpoint_file", &self.endpoint_file)
      .finish()
  }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiPerfConfig {
  #[serde(alias = "enabled", alias = "enableUiPerf", default)]
  pub enable: Option<bool>,
  #[serde(alias = "openDevtool", alias = "openDevTool", default)]
  pub open_devtool: bool,
  #[serde(default)]
  pub reporting: UiPerfReportingConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiPerfReportingConfig {
  #[serde(alias = "enabled", alias = "enableReporting", default)]
  pub enable: Option<bool>,
  #[serde(alias = "sampleRate", default)]
  pub sample_rate: Option<f64>,
  #[serde(alias = "intervalSecs", alias = "intervalSeconds", default)]
  pub interval_secs: Option<u64>,
  #[serde(alias = "maxEventsPerReport", default)]
  pub max_events_per_report: Option<usize>,
}

pub fn default_url_patters() -> Arc<once_cell::sync::OnceCell<Vec<UrlPattern>>> {
  Arc::new(once_cell::sync::OnceCell::new())
}

// 定义 proxy 结构体
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Proxy {
  #[serde(alias = "urlsFilter")]
  pub urls_filter: Vec<String>,
  pub headers: std::collections::HashMap<String, String>,
  #[serde(skip, default = "default_url_patters")]
  pub patterns: Arc<once_cell::sync::OnceCell<Vec<UrlPattern>>>,
}

impl Proxy {
  pub fn test(&self, url: &str) -> bool {
    let url = url::Url::from_str(url).unwrap();

    let url_filters = self.urls_filter.clone();
    let patterns = self.patterns.get_or_init(|| {
      url_filters
        .iter()
        .filter_map(|rule| {
          let rule = url::Url::from_str(rule);

          rule.ok().map(|rule| {
            let init = urlpattern::UrlPatternInit {
              protocol: Some(rule.scheme().to_string()),
              hostname: Some(rule.host_str().unwrap_or_default().to_string()),
              pathname: Some(rule.path().to_string()),
              ..Default::default()
            };
            <UrlPattern>::parse(init, Default::default()).unwrap()
          })
        })
        .collect::<Vec<UrlPattern>>()
    });
    for pattern in patterns {
      if let Ok(Some(_)) = pattern.exec(urlpattern::UrlPatternMatchInput::Url(url.clone())) {
        return true;
      }
    }
    false
  }
}

impl RuntimeConfig {
  pub fn default_file_path(config_dir: impl AsRef<Path>) -> PathBuf {
    config_dir.as_ref().join(DEFAULT_RUNTIME_CONFIG_FILE)
  }

  pub fn from_config_dir(config_dir: impl AsRef<Path>) -> RuntimeConfig {
    RuntimeConfig::from_file(RuntimeConfig::default_file_path(config_dir))
  }

  pub fn from_file(file: PathBuf) -> RuntimeConfig {
    if file.is_file() && file.exists() {
      let data = std::fs::read_to_string(&file).unwrap_or_default();
      let data = data.as_str();
      info!("RuntimeConfig file loaded: {:?}", file);
      return RuntimeConfig::from_str(data);
    }
    error!("RuntimeConfig file not found: {:?}", file);
    Default::default()
  }

  #[allow(clippy::should_implement_trait)]
  pub fn from_str(data: &str) -> RuntimeConfig {
    match serde_yml::from_str::<RuntimeConfig>(data) {
      Ok(config) => config,
      Err(e) => {
        error!("RuntimeConfig from file error: {:?}", e);
        Default::default()
      }
    }
  }
}

impl RuntimeConfig {
  pub fn match_proxy_headers(&self, url: &str) -> Option<&std::collections::HashMap<String, String>> {
    if self.proxy.test(url) && !self.env.is_production() {
      Some(&self.proxy.headers)
    } else {
      None
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_runtime_config_parses_staging_proxy_rules() {
    let config = RuntimeConfig::from_str(
      r#"
env: staging
proxy:
  headers:
    x-environment: staging
  urlsFilter:
    - https://app.example.test/*
    - https://chat.example.test/*
"#,
    );
    assert!(config.env.is_staging());
    assert!(config.proxy.test("https://app.example.test"));
    assert!(config.proxy.test("https://chat.example.test"));
    assert!(config.proxy.test("https://chat.example.test/1"));
    assert!(config.proxy.test("https://chat.example.test/a?c=1"));
    assert!(!config.proxy.test("https://app-other.example.test"));
  }

  #[test]
  fn test_runtime_config_mcp_supports_camel_case_fields() {
    let config = RuntimeConfig::from_str(
      r#"
mcp:
  enabled: true
  host: 127.0.0.1
  port: 3210
  tokenFile: /tmp/lintel-mcp-token
  endpointFile: /tmp/lintel-mcp-endpoint.json
"#,
    );

    assert!(config.mcp.enabled);
    assert_eq!(config.mcp.host.as_deref(), Some("127.0.0.1"));
    assert_eq!(config.mcp.port, Some(3210));
    assert_eq!(config.mcp.token_file, Some(PathBuf::from("/tmp/lintel-mcp-token")));
    assert_eq!(
      config.mcp.endpoint_file,
      Some(PathBuf::from("/tmp/lintel-mcp-endpoint.json"))
    );
  }

  #[test]
  fn test_runtime_config_serde_supports_generic_fields_and_aliases() {
    let config = RuntimeConfig::from_str(
      r#"
env: development
enable_debug_panel: true
ui_perf_config:
  enable: true
  open_devtool: true
  reporting:
    sample_rate: 0.5
mcp:
  enabled: true
  host: "::1"
  port: 4321
  endpoint_file: /tmp/runtime-endpoint.json
proxy:
  urls_filter:
    - https://example.test/*
  headers:
    x-runtime: enabled
"#,
    );
    assert_eq!(config.env, EnvType::Development);
    assert!(config.enable_debug_panel);
    assert_eq!(config.ui_perf_config.enable, Some(true));
    assert!(config.ui_perf_config.open_devtool);
    assert_eq!(config.ui_perf_config.reporting.sample_rate, Some(0.5));
    assert!(config.mcp.enabled);
    assert_eq!(config.mcp.host.as_deref(), Some("::1"));
    assert_eq!(config.mcp.port, Some(4321));
    assert_eq!(
      config.mcp.endpoint_file,
      Some(PathBuf::from("/tmp/runtime-endpoint.json"))
    );
    assert_eq!(config.proxy.urls_filter, vec!["https://example.test/*"]);
    assert_eq!(
      config.proxy.headers.get("x-runtime").map(String::as_str),
      Some("enabled")
    );

    let serialized = serde_yml::to_string(&config).unwrap();
    let value: serde_yml::Value = serde_yml::from_str(&serialized).unwrap();
    let mut fields = value
      .as_mapping()
      .unwrap()
      .keys()
      .map(|field| field.as_str().unwrap())
      .collect::<Vec<_>>();
    fields.sort_unstable();
    assert_eq!(
      fields,
      vec!["enable_debug_panel", "env", "mcp", "proxy", "ui_perf_config"]
    );
    let roundtrip = RuntimeConfig::from_str(&serialized);
    assert_eq!(roundtrip.env, EnvType::Development);
    assert_eq!(roundtrip.ui_perf_config.reporting.sample_rate, Some(0.5));
  }

  #[test]
  fn test_proxy_schema_roundtrips_generic_headers_and_url_alias() {
    let proxy = Proxy {
      urls_filter: vec!["https://schema.example.test/*".to_string()],
      headers: [("x-schema".to_string(), "enabled".to_string())].into(),
      ..Default::default()
    };
    assert!(format!("{:?}", proxy.clone()).contains("schema.example.test"));
    let serialized = serde_yml::to_string(&proxy).unwrap();
    let roundtrip: Proxy = serde_yml::from_str(&serialized).unwrap();
    assert_eq!(roundtrip.urls_filter, proxy.urls_filter);
    assert_eq!(roundtrip.headers, proxy.headers);
    let aliases: Proxy = serde_yml::from_str(
      r#"
urlsFilter:
  - https://alias.example.test/*
headers:
  x-alias: "enabled"
"#,
    )
    .unwrap();
    assert_eq!(aliases.headers.get("x-alias").map(String::as_str), Some("enabled"));
    assert!(aliases.test("https://alias.example.test/path"));
  }

  #[test]
  fn test_mcp_runtime_config_debug_redacts_inline_token() {
    let config = RuntimeConfig::from_str(
      r#"
mcp:
  enabled: true
  token: secret-token
"#,
    );

    let debug = format!("{:?}", config.mcp);

    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("secret-token"));
  }

  #[test]
  fn test_runtime_config_does_not_load_an_unrelated_runtime_file() {
    let dir = std::env::temp_dir().join(format!("lintel-lib-config-runtime-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("runtime.yml"), "enableDebugPanel: true\n").unwrap();

    let config = RuntimeConfig::from_config_dir(&dir);

    assert!(!config.enable_debug_panel);
    std::fs::remove_dir_all(dir).unwrap();
  }

  #[test]
  fn test_runtime_config_missing_or_invalid_files_return_default() {
    let dir = std::env::temp_dir().join(format!("lintel-lib-config-runtime-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();

    assert_eq!(
      RuntimeConfig::default_file_path(&dir),
      dir.join(DEFAULT_RUNTIME_CONFIG_FILE)
    );

    let missing = RuntimeConfig::from_config_dir(&dir);
    assert_eq!(missing.env, EnvType::Production);
    assert!(!missing.enable_debug_panel);

    let missing_file = RuntimeConfig::from_file(dir.join("missing.yml"));
    assert_eq!(missing_file.env, EnvType::Production);

    let invalid = RuntimeConfig::from_str("proxy: [");
    assert_eq!(invalid.env, EnvType::Production);

    std::fs::remove_dir_all(dir).unwrap();
  }

  #[test]
  fn test_runtime_config_proxy_headers_require_non_production_and_matching_origin() {
    for env in [EnvType::Development, EnvType::Staging, EnvType::Production] {
      let config = RuntimeConfig {
        env,
        proxy: Proxy {
          urls_filter: vec!["https://app.example.test/*".to_string()],
          headers: [("x-environment".to_string(), "testing".to_string())].into(),
          ..Default::default()
        },
        ..Default::default()
      };
      assert!(config.match_proxy_headers("https://other.example.test/path").is_none());
      let headers = config.match_proxy_headers("https://app.example.test/path");
      if env.is_production() {
        assert!(headers.is_none());
      } else {
        assert_eq!(
          headers.and_then(|value| value.get("x-environment")).map(String::as_str),
          Some("testing")
        );
      }
    }
  }

  #[test]
  fn test_runtime_config_loads_lintel_runtime_yml_only() {
    let dir = std::env::temp_dir().join(format!("lintel-lib-config-runtime-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(DEFAULT_RUNTIME_CONFIG_FILE, "lintel-runtime.yml");
    std::fs::write(dir.join(DEFAULT_RUNTIME_CONFIG_FILE), "enableDebugPanel: true\n").unwrap();
    std::fs::write(dir.join("runtime.yml"), "enableDebugPanel: false\n").unwrap();

    let config = RuntimeConfig::from_config_dir(&dir);

    assert!(config.enable_debug_panel);
    std::fs::remove_dir_all(dir).unwrap();
  }

  #[test]
  fn test_runtime_config_parses_ui_perf_config() {
    let data = r#"
uiPerfConfig:
  enable: true
  openDevtool: true
    "#;

    let config = RuntimeConfig::from_str(data);

    assert_eq!(config.ui_perf_config.enable, Some(true));
    assert!(config.ui_perf_config.open_devtool);
  }

  #[test]
  fn test_runtime_config_defaults_ui_perf_disabled() {
    let config = RuntimeConfig::from_str("");

    assert_eq!(config.ui_perf_config.enable, None);
    assert!(!config.ui_perf_config.open_devtool);
    assert_eq!(config.ui_perf_config.reporting.enable, None);
    assert_eq!(config.ui_perf_config.reporting.sample_rate, None);
    assert_eq!(config.ui_perf_config.reporting.interval_secs, None);
    assert_eq!(config.ui_perf_config.reporting.max_events_per_report, None);
  }

  #[test]
  fn test_runtime_config_parses_ui_perf_reporting_config() {
    let data = r#"
uiPerfConfig:
  reporting:
    enable: true
    sampleRate: 0.25
    intervalSecs: 45
    maxEventsPerReport: 4
    "#;

    let config = RuntimeConfig::from_str(data);

    assert_eq!(config.ui_perf_config.reporting.enable, Some(true));
    assert_eq!(config.ui_perf_config.reporting.sample_rate, Some(0.25));
    assert_eq!(config.ui_perf_config.reporting.interval_secs, Some(45));
    assert_eq!(config.ui_perf_config.reporting.max_events_per_report, Some(4));
  }

  #[test]
  fn test_runtime_config_parses_ui_perf_reporting_aliases() {
    let data = r#"
uiPerfConfig:
  reporting:
    enabled: true
    intervalSeconds: 30
    "#;

    let config = RuntimeConfig::from_str(data);

    assert_eq!(config.ui_perf_config.reporting.enable, Some(true));
    assert_eq!(config.ui_perf_config.reporting.interval_secs, Some(30));
  }

  #[test]
  fn test_runtime_config_from_invalid_yaml_returns_default() {
    let config = RuntimeConfig::from_str("uiPerfConfig: [");

    assert_eq!(config.ui_perf_config.enable, None);
    assert!(!config.ui_perf_config.open_devtool);
  }

  #[test]
  fn test_runtime_config_from_file_reads_existing_file() {
    let path = std::env::temp_dir().join(format!(
      "lintel-runtime-config-{}.yml",
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ));
    std::fs::write(
      &path,
      r#"
uiPerfConfig:
  reporting:
    enable: true
    sampleRate: 0.1
      "#,
    )
    .unwrap();

    let config = RuntimeConfig::from_file(path.clone());

    assert_eq!(config.ui_perf_config.reporting.enable, Some(true));
    assert_eq!(config.ui_perf_config.reporting.sample_rate, Some(0.1));

    let _ = std::fs::remove_file(path);
  }

  #[test]
  fn test_runtime_config_from_file_returns_default_for_missing_file() {
    let config = RuntimeConfig::from_file(std::env::temp_dir().join("missing-lintel-runtime-config.yml"));

    assert_eq!(config.ui_perf_config.enable, None);
    assert_eq!(config.env, EnvType::Production);
  }

  #[test]
  fn test_runtime_config_proxy_environment_names_are_deserialized() {
    let staging = RuntimeConfig::from_str(
      r#"
env: staging
proxy:
  headers:
    x-environment: staging
  urlsFilter:
    - https://example.test/*
"#,
    );
    assert_eq!(
      staging
        .match_proxy_headers("https://example.test/path")
        .and_then(|headers| headers.get("x-environment"))
        .map(String::as_str),
      Some("staging")
    );
    assert!(staging.match_proxy_headers("https://other.example.test/path").is_none());
    let production = RuntimeConfig::from_str(
      r#"
env: production
proxy:
  headers:
    x-environment: production
  urlsFilter:
    - https://example.test/*
"#,
    );
    assert!(production.match_proxy_headers("https://example.test/path").is_none());
  }
}
