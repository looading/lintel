use std::sync::Arc;

use lintel_lib_store::store::{DefaultSyncPolicy, Store, StoreConfig};
use semver::Version;

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let cwd = std::env::temp_dir().join("lintel-lib-store-example");
  if cwd.exists() {
    std::fs::remove_dir_all(&cwd)?;
  }
  std::fs::create_dir_all(&cwd)?;

  let store = Store::new(StoreConfig {
    cwd: cwd.clone(),
    name: "settings".into(),
    defaults: serde_json::json!({
      "theme": "light",
      "fontSize": 14
    }),
    version: Version::new(1, 0, 0),
    ..Default::default()
  });

  store.set("theme".into(), serde_json::json!("dark"))?;
  store.set("dynamic_key".into(), serde_json::json!(true))?;
  println!("v1 store: {}", store.get_all());

  let store = Store::new(StoreConfig {
    cwd: cwd.clone(),
    name: "settings".into(),
    defaults: serde_json::json!({
      "themeMode": "light",
      "fontSize": 16,
      "locale": "zh-CN"
    }),
    version: Version::new(2, 0, 0),
    default_sync_policy: DefaultSyncPolicy::MissingOnly,
    migration: Some(Arc::new(|store, ctx| {
      println!("migrate from {:?} to {}", ctx.from_version, ctx.to_version);
      if let Some(theme) = store.get("theme") {
        store.set("themeMode".into(), theme)?;
        store.delete("theme")?;
      }
      Ok(())
    })),
    ..Default::default()
  });

  println!("v2 store: {}", store.get_all());
  println!("meta: {:?}", store.meta()?);

  std::fs::remove_dir_all(&cwd)?;
  Ok(())
}
