//! 宿主按窗口与视图授权命令。主体由 Rust 创建，网页只提交命令和参数。

use std::{
  collections::{BTreeMap, BTreeSet},
  sync::{Arc, RwLock},
};

use anyhow::{Result, ensure};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Principal {
  window: u64,
  view: String,
}

impl Principal {
  pub fn new(window: u64, view: impl Into<String>) -> Result<Self> {
    let view = view.into();
    ensure!(!view.is_empty() && view.len() <= 128, "invalid view identity");
    Ok(Self { window, view })
  }

  pub fn window(&self) -> u64 {
    self.window
  }
  pub fn view(&self) -> &str {
    &self.view
  }
}

#[derive(Clone, Default)]
pub struct Capabilities(Arc<RwLock<BTreeMap<Principal, BTreeSet<String>>>>);

impl Capabilities {
  pub fn grant(&self, principal: &Principal, capability: impl Into<String>) -> Result<()> {
    let capability = capability.into();
    ensure!(
      !capability.is_empty()
        && capability.len() <= 128
        && capability
          .bytes()
          .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte)),
      "invalid capability name"
    );
    self
      .0
      .write()
      .map_err(|_| anyhow::anyhow!("capability registry lock poisoned"))?
      .entry(principal.clone())
      .or_default()
      .insert(capability);
    Ok(())
  }

  pub fn allows(&self, principal: &Principal, capability: &str) -> bool {
    self
      .0
      .read()
      .is_ok_and(|grants| grants.get(principal).is_some_and(|grants| grants.contains(capability)))
  }

  pub fn revoke(&self, principal: &Principal) {
    if let Ok(mut grants) = self.0.write() {
      grants.remove(principal);
    }
  }

  pub fn revoke_window(&self, window: u64) {
    if let Ok(mut grants) = self.0.write() {
      grants.retain(|principal, _| principal.window != window);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn grants_are_explicit_and_do_not_cross_window_or_view_boundaries() {
    let a = Principal::new(1, "main").unwrap();
    let b = Principal::new(2, "main").unwrap();
    let embedded = Principal::new(1, "embedded").unwrap();
    let capabilities = Capabilities::default();
    assert!(!capabilities.allows(&a, "notes.read"));
    capabilities.grant(&a, "notes.read").unwrap();
    assert!(capabilities.allows(&a, "notes.read"));
    assert!(!capabilities.allows(&b, "notes.read"));
    assert!(!capabilities.allows(&embedded, "notes.read"));
    assert!(!capabilities.allows(&a, "notes.write"));
    capabilities.revoke_window(1);
    assert!(!capabilities.allows(&a, "notes.read"));
    assert!(capabilities.grant(&a, "*").is_err());
  }
}
