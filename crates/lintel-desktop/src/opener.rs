use std::path::Path;

use crate::{Result, error::invalid};

/// Open an external web/mail URL. Custom protocols are deliberately handled by ProtocolRouter.
/// The host should authorize webview-originated requests before calling this function.
pub fn open_url(value: &str) -> Result<()> {
  let url = checked_url(value)?;
  #[cfg(feature = "opener")]
  {
    open::that(url.as_str()).map_err(|e| crate::error::native("open external URL", e))
  }
  #[cfg(not(feature = "opener"))]
  {
    let _ = url;
    Err(crate::error::unsupported("opener", "enable the opener feature"))
  }
}

/// Resolve an existing filesystem path and open it with the OS-associated application.
pub fn open_path(path: &Path) -> Result<()> {
  let path = path.canonicalize()?;
  #[cfg(feature = "opener")]
  {
    open::that(path).map_err(|e| crate::error::native("open path", e))
  }
  #[cfg(not(feature = "opener"))]
  {
    let _ = path;
    Err(crate::error::unsupported("opener", "enable the opener feature"))
  }
}

fn checked_url(value: &str) -> Result<url::Url> {
  crate::model::validate_text("URL", value, 64 * 1024)?;
  let url = url::Url::parse(value).map_err(|e| invalid("URL", e.to_string()))?;
  if !matches!(url.scheme(), "http" | "https" | "mailto") {
    return Err(invalid(
      "URL scheme",
      "only http, https, and mailto external URLs are accepted",
    ));
  }
  if matches!(url.scheme(), "http" | "https") && url.host_str().is_none() {
    return Err(invalid("URL", "web URL has no host"));
  }
  Ok(url)
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn executable_and_file_protocols_are_not_external_urls() {
    assert!(checked_url("https://example.com/path?q=one").is_ok());
    for value in [
      "javascript:alert(1)",
      "file:///etc/passwd",
      "unknown:run",
      "not a url",
      "https://example.com/\0",
    ] {
      assert!(checked_url(value).is_err(), "{value:?}");
    }
  }
}
