use std::env;

pub const GLOBAL_WEBVIEW_MAX_CONCURRENT_ENV: &str = "LINTEL_WEBVIEW_MAX_CONCURRENT";
pub const WEBVIEW_TEST_URL_ENV: &str = "LINTEL_WEBVIEW_TEST_URL";
pub const DEFAULT_GLOBAL_WEBVIEW_MAX_CONCURRENT: usize = 1;

pub fn global_webview_max_concurrent() -> usize {
  parse_positive_usize_env(
    env::var(GLOBAL_WEBVIEW_MAX_CONCURRENT_ENV).ok().as_deref(),
    DEFAULT_GLOBAL_WEBVIEW_MAX_CONCURRENT,
  )
}

pub fn webview_test_url_override() -> Option<String> {
  env::var(WEBVIEW_TEST_URL_ENV)
    .ok()
    .filter(|value| !value.trim().is_empty())
}

fn parse_positive_usize_env(raw: Option<&str>, default: usize) -> usize {
  raw
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .and_then(|value| value.parse::<usize>().ok())
    .filter(|value| *value > 0)
    .unwrap_or(default)
}

#[cfg(test)]
mod tests {
  use super::{DEFAULT_GLOBAL_WEBVIEW_MAX_CONCURRENT, parse_positive_usize_env};

  #[test]
  fn test_parse_positive_usize_env_fallbacks_to_default() {
    assert_eq!(
      parse_positive_usize_env(None, DEFAULT_GLOBAL_WEBVIEW_MAX_CONCURRENT),
      DEFAULT_GLOBAL_WEBVIEW_MAX_CONCURRENT
    );
    assert_eq!(
      parse_positive_usize_env(Some(""), DEFAULT_GLOBAL_WEBVIEW_MAX_CONCURRENT),
      DEFAULT_GLOBAL_WEBVIEW_MAX_CONCURRENT
    );
    assert_eq!(
      parse_positive_usize_env(Some("0"), DEFAULT_GLOBAL_WEBVIEW_MAX_CONCURRENT),
      DEFAULT_GLOBAL_WEBVIEW_MAX_CONCURRENT
    );
    assert_eq!(
      parse_positive_usize_env(Some("abc"), DEFAULT_GLOBAL_WEBVIEW_MAX_CONCURRENT),
      DEFAULT_GLOBAL_WEBVIEW_MAX_CONCURRENT
    );
  }

  #[test]
  fn test_parse_positive_usize_env_accepts_positive_integer() {
    assert_eq!(
      parse_positive_usize_env(Some("2"), DEFAULT_GLOBAL_WEBVIEW_MAX_CONCURRENT),
      2
    );
    assert_eq!(
      parse_positive_usize_env(Some(" 3 "), DEFAULT_GLOBAL_WEBVIEW_MAX_CONCURRENT),
      3
    );
  }
}
