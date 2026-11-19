use std::collections::HashSet;

use crate::{Result, error::invalid, model::validate_text};

/// A URL that passed the host's explicit scheme allowlist. Routing never opens external programs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolUrl(url::Url);
impl ProtocolUrl {
  pub fn url(&self) -> &url::Url {
    &self.0
  }
  pub fn into_url(self) -> url::Url {
    self.0
  }
}

/// Validates URLs received from OS delegates, command-line launches, or single-instance IPC.
/// OS protocol association must separately be declared by the application's installer/bundle.
#[derive(Debug, Clone)]
pub struct ProtocolRouter {
  schemes: HashSet<String>,
}

impl ProtocolRouter {
  pub fn new(schemes: impl IntoIterator<Item = impl Into<String>>) -> Result<Self> {
    let mut allowed = HashSet::new();
    for scheme in schemes {
      let scheme = scheme.into().to_ascii_lowercase();
      let mut bytes = scheme.bytes();
      if !bytes.next().is_some_and(|c| c.is_ascii_alphabetic())
        || !bytes.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.'))
        || scheme.len() > 128
      {
        return Err(invalid("protocol scheme", "expected a scheme name without ':' or '/'"));
      }
      allowed.insert(scheme);
    }
    if allowed.is_empty() {
      return Err(invalid("protocol schemes", "allow at least one explicit scheme"));
    }
    Ok(Self { schemes: allowed })
  }

  pub fn route(&self, value: &str) -> Result<ProtocolUrl> {
    validate_text("protocol URL", value, 64 * 1024)?;
    let url = url::Url::parse(value).map_err(|e| invalid("protocol URL", e.to_string()))?;
    if !self.schemes.contains(url.scheme()) {
      return Err(invalid(
        "protocol URL",
        format!("scheme {:?} is not allowed", url.scheme()),
      ));
    }
    Ok(ProtocolUrl(url))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn protocol_delivery_uses_explicit_case_insensitive_scheme_allowlist() {
    let router = ProtocolRouter::new(["Lintel-Demo"]).unwrap();
    assert_eq!(
      router.route("LINTEL-DEMO://open/item?id=42").unwrap().url().scheme(),
      "lintel-demo"
    );
    assert!(router.route("javascript:alert(1)").is_err());
    assert!(router.route("file:///tmp/example").is_err());
    assert!(router.route("relative/path").is_err());
    assert!(ProtocolRouter::new(["bad://"]).is_err());
  }
}
