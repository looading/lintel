use std::collections::HashMap;

use crate::svg::Handle;
use crate::svg_resources;
use once_cell::sync::Lazy;

#[derive(Debug)]
pub struct Assets {
  raws: HashMap<String, &'static [u8]>,
  pub handlers: HashMap<String, Handle>,
}

impl Assets {
  pub fn new() -> Self {
    Self {
      raws: HashMap::new(),
      handlers: HashMap::new(),
    }
  }

  pub fn from_vec(vec: Vec<(String, &'static [u8])>) -> Self {
    let mut assets = Self::new();
    for (raw, raw_data) in vec {
      assets.raws.insert(raw.to_string(), raw_data);
      let handler = Handle::from_memory(raw_data);
      assets.handlers.insert(raw.to_string(), handler);
    }

    assets
  }
}

impl From<Vec<(String, &'static [u8])>> for Assets {
  fn from(vec: Vec<(String, &'static [u8])>) -> Self {
    Self::from_vec(vec)
  }
}

impl Assets {
  pub fn get(&self, raw: &str) -> Option<&Handle> {
    self.handlers.get(raw)
  }
}

pub enum SharedAssetKey {
  MacMaxActive,
  MacCloseActive,
  MacMinActive,
}

pub struct SharedAssets(Assets);

impl SharedAssets {
  pub fn new() -> Self {
    Self(Assets::from_vec(vec![
      ("mac_max_active".to_string(), svg_resources::MAC_MAX_ACTIVE_SVG_BYTES),
      (
        "mac_close_active".to_string(),
        svg_resources::MAC_CLOSE_ACTIVE_SVG_BYTES,
      ),
      ("mac_min_active".to_string(), svg_resources::MAC_MIN_ACTIVE_SVG_BYTES),
    ]))
  }

  pub fn get(&self, key: SharedAssetKey) -> Option<&Handle> {
    match key {
      SharedAssetKey::MacMaxActive => self.0.get("mac_max_active"),
      SharedAssetKey::MacCloseActive => self.0.get("mac_close_active"),
      SharedAssetKey::MacMinActive => self.0.get("mac_min_active"),
    }
  }
}

pub static SHARED_ASSETS: Lazy<SharedAssets> = Lazy::new(SharedAssets::new);
