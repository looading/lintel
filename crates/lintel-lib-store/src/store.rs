use lintel_lib_crypto::aes::{HexString, decryp_aes_128_ecb, encrypt_aes_128_ecb};
use rkv::{
  Manager, Rkv, SingleStore, StoreOptions, Value as RkvValue,
  backend::{SafeMode, SafeModeDatabase, SafeModeEnvironment},
};
use semver::Version;
use std::{
  collections::hash_map::DefaultHasher,
  fs,
  hash::{Hash, Hasher},
  io::{Error, ErrorKind},
  path::PathBuf,
  sync::{Arc, RwLock},
};
use tracing::{debug, info, trace, warn};

const STORE_META_KEY: &str = "__x_lib_store_meta__";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, Default)]
pub enum DefaultSyncPolicy {
  #[default]
  MissingOnly,
  PruneRemovedManagedKeys,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct StoreMigrationContext {
  pub from_version: Option<Version>,
  pub to_version: Version,
  pub previous_managed_keys: Vec<String>,
  pub current_managed_keys: Vec<String>,
}

pub type StoreMigrationFn = Arc<dyn Fn(&Store, &StoreMigrationContext) -> Result<(), Error> + Send + Sync>;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
struct StoreMeta {
  version: Option<Version>,
  managed_keys: Vec<String>,
  defaults_hash: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct StoreMetaSnapshot {
  pub version: Option<Version>,
  pub managed_keys: Vec<String>,
  pub defaults_hash: Option<String>,
}

#[derive(Clone)]
pub struct Store {
  env: Arc<RwLock<Rkv<SafeModeEnvironment>>>,
  single_store: SingleStore<SafeModeDatabase>,
  secret_key: Option<[u8; 16]>,
  defaults: serde_json::Value,
  version: Version,
  clear_invalid_config: bool,
  default_sync_policy: DefaultSyncPolicy,
  migration: Option<StoreMigrationFn>,
  managed_default_keys: Vec<String>,
}

impl std::fmt::Debug for Store {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Store")
      .field("encrypted", &self.secret_key.is_some())
      .field("version", &self.version)
      .field("default_sync_policy", &self.default_sync_policy)
      .finish()
  }
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
pub struct StoreConfig {
  pub cwd: PathBuf,
  #[serde(rename = "clearInvalidConfig")]
  pub clear_invalid_config: bool,
  pub name: String,
  pub defaults: serde_json::Value,
  #[serde(skip)]
  pub secret_key: Option<[u8; 16]>,
  pub version: Version,
  #[serde(default)]
  pub default_sync_policy: DefaultSyncPolicy,
  #[serde(skip, default)]
  pub migration: Option<StoreMigrationFn>,
}

impl std::fmt::Debug for StoreConfig {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("StoreConfig")
      .field("cwd", &self.cwd)
      .field("clear_invalid_config", &self.clear_invalid_config)
      .field("name", &self.name)
      .field("defaults", &self.defaults)
      .field("secret_key", &"***")
      .field("version", &self.version)
      .field("default_sync_policy", &self.default_sync_policy)
      .finish()
  }
}

impl Default for StoreConfig {
  fn default() -> Self {
    StoreConfig {
      cwd: "".into(),
      clear_invalid_config: true,
      name: "lintel-config".into(),
      defaults: serde_json::json!({}),
      secret_key: None,
      version: Version::new(0, 0, 0),
      default_sync_policy: DefaultSyncPolicy::MissingOnly,
      migration: None,
    }
  }
}

fn encode_value(value: &serde_json::Value, secret_key: &Option<[u8; 16]>) -> Result<Vec<u8>, Error> {
  let bytes = serde_json::to_vec(value).map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))?;
  if let Some(key) = secret_key {
    encrypt_aes_128_ecb(HexString::from_vec(bytes).0.as_str(), key)
      .and_then(|buf| buf.to_vec())
      .map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))
  } else {
    Ok(bytes)
  }
}

fn decode_value(bytes: &[u8], secret_key: &Option<[u8; 16]>) -> Result<serde_json::Value, Error> {
  let bytes = if let Some(key) = secret_key {
    let decrypted = decryp_aes_128_ecb(HexString::from_vec(bytes.to_vec()), key)
      .map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))?;
    HexString(decrypted)
      .to_vec()
      .map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))?
  } else {
    bytes.to_vec()
  };
  serde_json::from_slice(&bytes).map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))
}

fn is_reserved_key(key: &str) -> bool {
  key == STORE_META_KEY
}

fn managed_default_keys(defaults: &serde_json::Value) -> Vec<String> {
  let mut keys = defaults
    .as_object()
    .map(|obj| obj.keys().cloned().collect::<Vec<_>>())
    .unwrap_or_default();
  keys.sort();
  keys
}

fn defaults_hash(defaults: &serde_json::Value) -> Result<String, Error> {
  let bytes = serde_json::to_vec(defaults).map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))?;
  let mut hasher = DefaultHasher::new();
  bytes.hash(&mut hasher);
  Ok(format!("{:016x}", hasher.finish()))
}

impl Store {
  // Aligns with Electron's electron-store API shape:
  //   new Store({ cwd, clearInvalidConfig, name, defaults })
  pub fn new(config: StoreConfig) -> Self {
    let store = Self::open(config).expect("failed to open rkv store");
    if let Err(error) = store.apply_schema_changes() {
      tracing::error!(?error, "failed to apply store schema changes");
    }
    store
  }

  /// 打开配置并传播目录、数据库和 schema 迁移错误。
  pub fn try_new(config: StoreConfig) -> Result<Self, Error> {
    let store = Self::open(config)?;
    store.apply_schema_changes()?;
    Ok(store)
  }

  fn open(config: StoreConfig) -> Result<Self, Error> {
    let StoreConfig {
      cwd,
      clear_invalid_config,
      name,
      defaults,
      secret_key,
      version,
      default_sync_policy,
      migration,
    } = config;
    let db_path = cwd.join(format!("{}.rkv", name));
    fs::create_dir_all(&db_path)?;
    info!(path = %db_path.display(), "opening rkv store");

    let shared_env = Manager::<SafeModeEnvironment>::singleton()
      .write()
      .unwrap_or_else(|e| e.into_inner())
      .get_or_create(&*db_path, Rkv::new::<SafeMode>)
      .map_err(|error| Error::other(error.to_string()))?;

    let single_store = {
      let env = shared_env.read().unwrap_or_else(|e| e.into_inner());
      env
        .open_single("data", StoreOptions::create())
        .map_err(|error| Error::other(error.to_string()))?
    };

    let managed_default_keys = managed_default_keys(&defaults);
    let store = Store {
      env: shared_env,
      single_store,
      secret_key,
      defaults,
      version,
      clear_invalid_config,
      default_sync_policy,
      migration,
      managed_default_keys,
    };

    Ok(store)
  }

  fn load_meta(&self) -> Result<StoreMeta, Error> {
    let env = self.env.read().map_err(|e| Error::other(e.to_string()))?;
    let reader = env.read().map_err(|e| Error::other(e.to_string()))?;
    match self.single_store.get(&reader, STORE_META_KEY) {
      Ok(Some(RkvValue::Blob(bytes))) => match decode_value(bytes, &self.secret_key) {
        Ok(value) => match serde_json::from_value::<StoreMeta>(value) {
          Ok(meta) => Ok(meta),
          Err(e) if self.clear_invalid_config => {
            warn!(?e, "invalid store meta, fallback to default meta");
            Ok(StoreMeta::default())
          }
          Err(e) => Err(Error::new(ErrorKind::InvalidData, e.to_string())),
        },
        Err(e) if self.clear_invalid_config => {
          warn!(?e, "invalid store meta, fallback to default meta");
          Ok(StoreMeta::default())
        }
        Err(e) => Err(e),
      },
      Ok(Some(_)) if self.clear_invalid_config => {
        warn!("unexpected store meta type, fallback to default meta");
        Ok(StoreMeta::default())
      }
      Ok(Some(_)) => Err(Error::new(ErrorKind::InvalidData, "invalid store meta type")),
      Ok(None) => Ok(StoreMeta::default()),
      Err(e) => Err(Error::other(e.to_string())),
    }
  }

  fn apply_schema_changes(&self) -> Result<(), Error> {
    let previous_meta = self.load_meta()?;
    let context = StoreMigrationContext {
      from_version: previous_meta.version.clone(),
      to_version: self.version.clone(),
      previous_managed_keys: previous_meta.managed_keys.clone(),
      current_managed_keys: self.managed_default_keys.clone(),
    };

    if previous_meta.version.as_ref() != Some(&self.version)
      && let Some(migration) = &self.migration
    {
      migration(self, &context)?;
    }

    let env = self.env.read().map_err(|e| Error::other(e.to_string()))?;
    let mut writer = env.write().map_err(|e| Error::other(e.to_string()))?;

    if let Some(defaults_obj) = self.defaults.as_object() {
      for (key, value) in defaults_obj {
        if self.single_store.get(&writer, key.as_str()).ok().flatten().is_none() {
          let bytes = encode_value(value, &self.secret_key)?;
          self
            .single_store
            .put(&mut writer, key.as_str(), &RkvValue::Blob(&bytes))
            .map_err(|e| Error::other(e.to_string()))?;
        }
      }
    }

    if self.default_sync_policy == DefaultSyncPolicy::PruneRemovedManagedKeys {
      for key in &previous_meta.managed_keys {
        if !self.managed_default_keys.contains(key) {
          self
            .single_store
            .delete(&mut writer, key.as_str())
            .map_err(|e| Error::other(e.to_string()))?;
        }
      }
    }

    let meta_bytes = encode_value(
      &serde_json::to_value(StoreMeta {
        version: Some(self.version.clone()),
        managed_keys: self.managed_default_keys.clone(),
        defaults_hash: Some(defaults_hash(&self.defaults)?),
      })
      .map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))?,
      &self.secret_key,
    )?;
    self
      .single_store
      .put(&mut writer, STORE_META_KEY, &RkvValue::Blob(&meta_bytes))
      .map_err(|e| Error::other(e.to_string()))?;
    writer.commit().map_err(|e| Error::other(e.to_string()))
  }

  pub fn reconcile_schema(&self) -> Result<(), Error> {
    self.apply_schema_changes()
  }

  pub fn migrate(&self) -> Result<(), Error> {
    self.apply_schema_changes()
  }

  pub fn meta(&self) -> Result<StoreMetaSnapshot, Error> {
    let meta = self.load_meta()?;
    Ok(StoreMetaSnapshot {
      version: meta.version,
      managed_keys: meta.managed_keys,
      defaults_hash: meta.defaults_hash,
    })
  }

  pub fn needs_migration(&self) -> Result<bool, Error> {
    Ok(self.load_meta()?.version.as_ref() != Some(&self.version))
  }

  pub fn get(&self, key: &str) -> Option<serde_json::Value> {
    if is_reserved_key(key) {
      return None;
    }
    trace!(?key, "get");
    let env = self.env.read().ok()?;
    let reader = env.read().ok()?;
    match self.single_store.get(&reader, key) {
      Ok(Some(RkvValue::Blob(bytes))) => decode_value(bytes, &self.secret_key)
        .ok()
        .or_else(|| self.defaults.get(key).cloned()),
      _ => self.defaults.get(key).cloned(),
    }
  }

  pub fn set(&self, key: String, value: serde_json::Value) -> Result<(), Error> {
    if is_reserved_key(&key) {
      return Err(Error::new(ErrorKind::InvalidInput, "reserved store key"));
    }
    trace!(?key, "set");
    let bytes = encode_value(&value, &self.secret_key)?;
    let env = self.env.read().map_err(|e| Error::other(e.to_string()))?;
    let mut writer = env.write().map_err(|e| Error::other(e.to_string()))?;
    self
      .single_store
      .put(&mut writer, key.as_str(), &RkvValue::Blob(&bytes))
      .map_err(|e| Error::other(e.to_string()))?;
    writer.commit().map_err(|e| Error::other(e.to_string()))
  }

  pub fn delete(&self, key: &str) -> Result<(), Error> {
    if is_reserved_key(key) {
      return Err(Error::new(ErrorKind::InvalidInput, "reserved store key"));
    }
    trace!(?key, "delete");
    let env = self.env.read().map_err(|e| Error::other(e.to_string()))?;
    let mut writer = env.write().map_err(|e| Error::other(e.to_string()))?;
    self
      .single_store
      .delete(&mut writer, key)
      .map_err(|e| Error::other(e.to_string()))?;
    writer.commit().map_err(|e| Error::other(e.to_string()))
  }

  pub fn clear(&self) -> Result<(), Error> {
    debug!("clear");
    let env = self.env.read().map_err(|e| Error::other(e.to_string()))?;
    let mut writer = env.write().map_err(|e| Error::other(e.to_string()))?;
    self
      .single_store
      .clear(&mut writer)
      .map_err(|e| Error::other(e.to_string()))?;
    writer.commit().map_err(|e| Error::other(e.to_string()))?;
    self.apply_schema_changes()
  }

  pub fn set_all(&self, values: Vec<(String, serde_json::Value)>) -> Result<(), Error> {
    let env = self.env.read().map_err(|e| Error::other(e.to_string()))?;
    let mut writer = env.write().map_err(|e| Error::other(e.to_string()))?;
    for (key, value) in &values {
      if is_reserved_key(key) {
        return Err(Error::new(ErrorKind::InvalidInput, "reserved store key"));
      }
      let bytes = encode_value(value, &self.secret_key)?;
      self
        .single_store
        .put(&mut writer, key.as_str(), &RkvValue::Blob(&bytes))
        .map_err(|e| Error::other(e.to_string()))?;
    }
    writer.commit().map_err(|e| Error::other(e.to_string()))
  }

  pub fn get_all(&self) -> serde_json::Value {
    let mut map = serde_json::map::Map::new();

    // Seed with defaults first, then overwrite with stored values
    if let Some(defaults_obj) = self.defaults.as_object() {
      map.extend(defaults_obj.clone());
    }

    let env = match self.env.read() {
      Ok(env) => env,
      Err(e) => {
        tracing::error!(?e, "store env lock poisoned");
        return serde_json::Value::Object(map);
      }
    };
    let reader = match env.read() {
      Ok(r) => r,
      Err(e) => {
        tracing::error!(?e, "failed to open rkv reader");
        return serde_json::Value::Object(map);
      }
    };

    if let Ok(iter) = self.single_store.iter_start(&reader) {
      for item in iter {
        if let Ok((key_bytes, RkvValue::Blob(bytes))) = item
          && let (Ok(key), Ok(value)) = (std::str::from_utf8(key_bytes), decode_value(bytes, &self.secret_key))
        {
          if is_reserved_key(key) {
            continue;
          }
          map.insert(key.to_string(), value);
        }
      }
    }

    serde_json::Value::Object(map)
  }

  // reset store. 如果 key 为 None, 则重置所有 key 为默认值，否则重置指定 key 为默认值
  pub fn reset(&self, key: Option<&str>) -> Result<(), Error> {
    if let Some(key) = key {
      self.delete(key)?;
    } else {
      self.clear()?;
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use std::{
    path::PathBuf,
    sync::{
      Arc,
      atomic::{AtomicBool, AtomicUsize, Ordering},
    },
  };

  use super::{STORE_META_KEY, StoreMeta, encode_value};
  use crate::store::{DefaultSyncPolicy, RkvValue, Store, StoreConfig};
  use semver::Version;
  use std::io::Error;

  fn version(raw: &str) -> Version {
    Version::parse(raw).unwrap()
  }

  fn test_dir(name: &str) -> PathBuf {
    let cwd = std::env::temp_dir().join(name);
    if cwd.exists() {
      std::fs::remove_dir_all(&cwd).unwrap();
    }
    std::fs::create_dir_all(&cwd).unwrap();
    cwd
  }

  fn cleanup_dir(cwd: &PathBuf) {
    if cwd.exists() {
      std::fs::remove_dir_all(cwd).unwrap();
    }
  }

  fn write_raw_blob(store: &Store, key: &str, bytes: &[u8]) {
    let env = store.env.read().unwrap();
    let mut writer = env.write().unwrap();
    store
      .single_store
      .put(&mut writer, key, &RkvValue::Blob(bytes))
      .unwrap();
    writer.commit().unwrap();
  }

  fn write_raw_value(store: &Store, key: &str, value: RkvValue<'_>) {
    let env = store.env.read().unwrap();
    let mut writer = env.write().unwrap();
    store.single_store.put(&mut writer, key, &value).unwrap();
    writer.commit().unwrap();
  }

  fn poison_store_env(store: &Store) {
    let env = store.env.clone();
    let _ = std::thread::spawn(move || {
      let _guard = env.write().unwrap();
      panic!("poison store env lock");
    })
    .join();
  }

  #[test]
  fn test_store() {
    let tmp = test_dir("lintel-lib-store-test");
    let store = Store::new(StoreConfig {
      cwd: tmp.clone(),
      name: "settings-config".into(),
      defaults: Default::default(),
      ..Default::default()
    });

    store.set("foo".to_string(), serde_json::json!("bar")).unwrap();
    assert_eq!(store.get("foo"), Some(serde_json::json!("bar")));
    assert_eq!(store.get_all()["foo"], serde_json::json!("bar"));
    cleanup_dir(&tmp);
  }

  #[test]
  fn test_store_debug_output() {
    let cwd = test_dir("store_test_debug_output");
    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "debug-store".into(),
      defaults: serde_json::json!({ "theme": "light" }),
      version: version("1.2.3"),
      ..Default::default()
    });

    let store_debug = format!("{store:?}");
    let config_debug = format!(
      "{:?}",
      StoreConfig {
        cwd: cwd.clone(),
        name: "debug-store".into(),
        defaults: serde_json::json!({ "theme": "light" }),
        version: version("1.2.3"),
        ..Default::default()
      }
    );

    assert!(store_debug.contains("encrypted"));
    assert!(store_debug.contains("version"));
    assert!(config_debug.contains("clear_invalid_config"));
    assert!(config_debug.contains("default_sync_policy"));

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_sync_missing_defaults() {
    let cwd = test_dir("store_test_missing_defaults");

    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "sync-defaults-store".into(),
      defaults: serde_json::json!({
        "theme": "light"
      }),
      version: version("1.0.0"),
      ..Default::default()
    });
    assert_eq!(store.get("theme"), Some(serde_json::json!("light")));

    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "sync-defaults-store".into(),
      defaults: serde_json::json!({
        "theme": "light",
        "fontSize": 14
      }),
      version: version("1.1.0"),
      ..Default::default()
    });

    assert_eq!(store.get("theme"), Some(serde_json::json!("light")));
    assert_eq!(store.get("fontSize"), Some(serde_json::json!(14)));
    assert_eq!(store.get_all()["fontSize"], serde_json::json!(14));

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_keeps_removed_defaults_by_default() {
    let cwd = test_dir("store_test_keeps_removed_defaults");

    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "keeps-removed-defaults".into(),
      defaults: serde_json::json!({
        "legacy": true
      }),
      version: version("1.0.0"),
      ..Default::default()
    });
    assert_eq!(store.get("legacy"), Some(serde_json::json!(true)));

    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "keeps-removed-defaults".into(),
      defaults: serde_json::json!({}),
      version: version("1.1.0"),
      ..Default::default()
    });
    assert_eq!(store.get_all()["legacy"], serde_json::json!(true));

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_features() {
    let cwd = test_dir("store_test_features");

    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "feature-store".into(),
      defaults: serde_json::json!({
        "default_key": "default_value",
        "another_default": 123
      }),
      ..Default::default()
    });

    // 1. Get with fallback
    // Should get default value since not set in data

    assert_eq!(store.get("default_key").unwrap().as_str().unwrap(), "default_value");

    // 2. Has
    assert!(store.get("default_key").is_some()); // In defaults
    assert!(store.get("non_existent").is_none());

    // 3. Set and overwrite default
    store.set("default_key".into(), serde_json::json!("new_value")).unwrap();
    assert_eq!(store.get("default_key").unwrap().as_str().unwrap(), "new_value");

    // 4. Delete (should revert to default)
    store.delete("default_key".into()).unwrap();
    assert_eq!(store.get("default_key").unwrap().as_str().unwrap(), "default_value");

    // 5. Set new key (no default)
    store.set("new_key".into(), serde_json::json!("dynamic")).unwrap();
    assert!(store.get("new_key").is_some());
    assert_eq!(store.get("new_key").unwrap().as_str().unwrap(), "dynamic");

    // 6. Delete new key (should be gone)
    store.delete("new_key").unwrap();
    assert!(store.get("new_key").is_none());

    // 7. Reset
    store.set("default_key".into(), serde_json::json!("changed")).unwrap();
    store.set("another_default".into(), serde_json::json!(456)).unwrap();
    store.reset(Some("default_key")).unwrap();
    assert_eq!(store.get("default_key").unwrap().as_str().unwrap(), "default_value");
    assert_eq!(store.get("another_default").unwrap().as_u64().unwrap(), 456); // Should remain changed

    // 8. Clear
    store.clear().unwrap();
    assert_eq!(store.get("default_key").unwrap().as_str().unwrap(), "default_value");
    assert_eq!(store.get("another_default").unwrap().as_u64().unwrap(), 123); // Reverted to default

    // 9. reset(None)
    store
      .set("default_key".into(), serde_json::json!("changed-again"))
      .unwrap();
    store.reset(None).unwrap();
    assert_eq!(store.get("default_key").unwrap().as_str().unwrap(), "default_value");

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_prunes_removed_managed_keys() {
    let cwd = test_dir("store_test_prune_removed_defaults");

    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "prune-removed-defaults".into(),
      defaults: serde_json::json!({
        "legacy": true,
        "keep": 1
      }),
      version: version("1.0.0"),
      default_sync_policy: DefaultSyncPolicy::PruneRemovedManagedKeys,
      ..Default::default()
    });
    store.set("dynamic".into(), serde_json::json!("keep-me")).unwrap();

    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "prune-removed-defaults".into(),
      defaults: serde_json::json!({
        "keep": 1
      }),
      version: version("1.1.0"),
      default_sync_policy: DefaultSyncPolicy::PruneRemovedManagedKeys,
      ..Default::default()
    });

    let all = store.get_all();
    assert!(all.get("legacy").is_none());
    assert_eq!(all["dynamic"], serde_json::json!("keep-me"));

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_migration_runs_on_version_change() {
    let cwd = test_dir("store_test_migration");

    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "migration-store".into(),
      defaults: serde_json::json!({
        "theme": "light"
      }),
      version: version("1.0.0"),
      ..Default::default()
    });
    store.set("theme".into(), serde_json::json!("dark")).unwrap();

    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "migration-store".into(),
      defaults: serde_json::json!({
        "themeMode": "light"
      }),
      version: version("2.0.0"),
      migration: Some(Arc::new(|store, ctx| {
        assert_eq!(ctx.from_version, Some(version("1.0.0")));
        assert_eq!(ctx.to_version, version("2.0.0"));
        if let Some(theme) = store.get("theme") {
          store.set("themeMode".into(), theme)?;
          store.delete("theme")?;
        }
        Ok(())
      })),
      ..Default::default()
    });

    assert_eq!(store.get("themeMode"), Some(serde_json::json!("dark")));
    assert!(store.get_all().get("theme").is_none());
    assert!(!store.needs_migration().unwrap());

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_meta_snapshot_tracks_schema() {
    let cwd = test_dir("store_test_meta_snapshot");

    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "meta-store".into(),
      defaults: serde_json::json!({
        "theme": "light",
        "fontSize": 14
      }),
      version: version("3.1.0"),
      ..Default::default()
    });

    let meta = store.meta().unwrap();
    assert_eq!(meta.version, Some(version("3.1.0")));
    assert_eq!(meta.managed_keys, vec!["fontSize".to_string(), "theme".to_string()]);
    assert!(meta.defaults_hash.is_some());

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_rejects_reserved_key() {
    let cwd = test_dir("store_test_reserved_key");

    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "reserved-key-store".into(),
      defaults: serde_json::json!({}),
      version: version("1.0.0"),
      ..Default::default()
    });

    assert!(store.set("__x_lib_store_meta__".into(), serde_json::json!(1)).is_err());
    assert!(store.delete("__x_lib_store_meta__").is_err());
    assert!(
      store
        .set_all(vec![("__x_lib_store_meta__".into(), serde_json::json!(1))])
        .is_err()
    );
    assert!(store.get("__x_lib_store_meta__").is_none());
    assert!(store.get_all().get("__x_lib_store_meta__").is_none());

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_supports_encrypted_values() {
    let cwd = test_dir("store_test_encrypted");
    let secret_key = *b"0123456789abcdef";

    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "encrypted-store".into(),
      defaults: serde_json::json!({
        "token": ""
      }),
      secret_key: Some(secret_key),
      version: version("1.0.0"),
      ..Default::default()
    });

    store.set("token".into(), serde_json::json!("secret-token")).unwrap();
    assert_eq!(store.get("token"), Some(serde_json::json!("secret-token")));
    assert_eq!(store.get_all()["token"], serde_json::json!("secret-token"));

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_set_all_success() {
    let cwd = test_dir("store_test_set_all_success");
    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "set-all-store".into(),
      defaults: serde_json::json!({}),
      ..Default::default()
    });

    store
      .set_all(vec![
        ("foo".into(), serde_json::json!(1)),
        ("bar".into(), serde_json::json!(true)),
      ])
      .unwrap();

    assert_eq!(store.get("foo"), Some(serde_json::json!(1)));
    assert_eq!(store.get("bar"), Some(serde_json::json!(true)));

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_handles_non_object_defaults() {
    let cwd = test_dir("store_test_non_object_defaults");
    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "non-object-defaults".into(),
      defaults: serde_json::json!(["a", "b"]),
      version: version("1.0.0"),
      ..Default::default()
    });

    let meta = store.meta().unwrap();
    assert!(meta.managed_keys.is_empty());
    assert_eq!(store.get_all(), serde_json::json!({}));

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_skips_corrupted_value_and_falls_back_to_default() {
    let cwd = test_dir("store_test_corrupted_value");
    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "corrupted-value-store".into(),
      defaults: serde_json::json!({
        "theme": "light"
      }),
      ..Default::default()
    });

    write_raw_blob(&store, "theme", b"not-json");

    assert_eq!(store.get("theme"), Some(serde_json::json!("light")));
    assert_eq!(store.get_all()["theme"], serde_json::json!("light"));

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_invalid_meta_fallback_when_clear_invalid_config_enabled() {
    let cwd = test_dir("store_test_invalid_meta_fallback");
    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "invalid-meta-store".into(),
      defaults: serde_json::json!({
        "theme": "light"
      }),
      clear_invalid_config: true,
      version: version("1.0.0"),
      ..Default::default()
    });

    write_raw_blob(&store, STORE_META_KEY, b"bad-meta");

    let meta = store.meta().unwrap();
    assert_eq!(meta.version, None);
    assert!(store.reconcile_schema().is_ok());
    assert_eq!(store.meta().unwrap().version, Some(version("1.0.0")));

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_invalid_meta_type_fallback_when_clear_invalid_config_enabled() {
    let cwd = test_dir("store_test_invalid_meta_type_fallback");
    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "invalid-meta-type-store".into(),
      defaults: serde_json::json!({}),
      clear_invalid_config: true,
      version: version("1.0.0"),
      ..Default::default()
    });

    write_raw_value(&store, STORE_META_KEY, RkvValue::I64(42));

    let meta = store.meta().unwrap();
    assert_eq!(meta.version, None);

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_invalid_meta_returns_error_when_clear_invalid_config_disabled() {
    let cwd = test_dir("store_test_invalid_meta_error");
    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "invalid-meta-error-store".into(),
      defaults: serde_json::json!({}),
      clear_invalid_config: false,
      version: version("1.0.0"),
      ..Default::default()
    });

    write_raw_blob(&store, STORE_META_KEY, b"bad-meta");

    assert!(store.meta().is_err());
    assert!(store.needs_migration().is_err());

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_invalid_meta_type_returns_error_when_clear_invalid_config_disabled() {
    let cwd = test_dir("store_test_invalid_meta_type_error");
    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "invalid-meta-type-error-store".into(),
      defaults: serde_json::json!({}),
      clear_invalid_config: false,
      version: version("1.0.0"),
      ..Default::default()
    });

    write_raw_value(&store, STORE_META_KEY, RkvValue::I64(7));

    assert!(store.meta().is_err());
    assert!(store.needs_migration().is_err());

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_migration_not_run_when_version_unchanged() {
    let cwd = test_dir("store_test_migration_not_run");
    let called = Arc::new(AtomicBool::new(false));

    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "no-migration-store".into(),
      defaults: serde_json::json!({
        "theme": "light"
      }),
      version: version("1.0.0"),
      ..Default::default()
    });
    assert!(!store.needs_migration().unwrap());

    let called_clone = called.clone();
    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "no-migration-store".into(),
      defaults: serde_json::json!({
        "theme": "light"
      }),
      version: version("1.0.0"),
      migration: Some(Arc::new(move |_, _| {
        called_clone.store(true, Ordering::SeqCst);
        Ok(())
      })),
      ..Default::default()
    });

    assert!(!called.load(Ordering::SeqCst));
    assert!(!store.needs_migration().unwrap());

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_new_swallows_migration_error_and_keeps_pending_state() {
    let cwd = test_dir("store_test_migration_error");
    let called = Arc::new(AtomicUsize::new(0));

    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "migration-error-store".into(),
      defaults: serde_json::json!({
        "theme": "light"
      }),
      version: version("1.0.0"),
      ..Default::default()
    });
    assert_eq!(store.meta().unwrap().version, Some(version("1.0.0")));

    let called_clone = called.clone();
    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "migration-error-store".into(),
      defaults: serde_json::json!({
        "themeMode": "light"
      }),
      version: version("2.0.0"),
      migration: Some(Arc::new(move |_, _| {
        called_clone.fetch_add(1, Ordering::SeqCst);
        Err(Error::other("migration failed"))
      })),
      ..Default::default()
    });

    assert_eq!(called.load(Ordering::SeqCst), 1);
    assert!(store.needs_migration().unwrap());
    assert_eq!(store.meta().unwrap().version, Some(version("1.0.0")));

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_manual_migrate_updates_meta_after_raw_version_regression() {
    let cwd = test_dir("store_test_manual_migrate");
    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "manual-migrate-store".into(),
      defaults: serde_json::json!({
        "theme": "light"
      }),
      version: version("2.0.0"),
      ..Default::default()
    });

    let raw_meta = serde_json::to_value(StoreMeta {
      version: Some(version("1.0.0")),
      managed_keys: vec!["theme".into()],
      defaults_hash: Some("old".into()),
    })
    .unwrap();
    let raw_meta_bytes = encode_value(&raw_meta, &store.secret_key).unwrap();
    write_raw_blob(&store, STORE_META_KEY, &raw_meta_bytes);

    assert!(store.needs_migration().unwrap());
    store.migrate().unwrap();
    assert!(!store.needs_migration().unwrap());
    assert_eq!(store.meta().unwrap().version, Some(version("2.0.0")));

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_invalid_meta_version_fallback_when_clear_invalid_config_enabled() {
    let cwd = test_dir("store_test_invalid_meta_version_fallback");
    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "invalid-meta-version-store".into(),
      defaults: serde_json::json!({
        "theme": "light"
      }),
      clear_invalid_config: true,
      version: version("1.0.0"),
      ..Default::default()
    });

    let raw_meta = serde_json::json!({
      "version": "invalid",
      "managed_keys": ["theme"],
      "defaults_hash": "old",
    });
    let raw_meta_bytes = encode_value(&raw_meta, &store.secret_key).unwrap();
    write_raw_blob(&store, STORE_META_KEY, &raw_meta_bytes);

    let meta = store.meta().unwrap();
    assert_eq!(meta.version, None);
    assert!(store.reconcile_schema().is_ok());
    assert_eq!(store.meta().unwrap().version, Some(version("1.0.0")));

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_invalid_meta_version_returns_error_when_clear_invalid_config_disabled() {
    let cwd = test_dir("store_test_invalid_meta_version_error");
    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "invalid-meta-version-error-store".into(),
      defaults: serde_json::json!({}),
      clear_invalid_config: false,
      version: version("1.0.0"),
      ..Default::default()
    });

    let raw_meta = serde_json::json!({
      "version": "invalid",
      "managed_keys": [],
      "defaults_hash": "old",
    });
    let raw_meta_bytes = encode_value(&raw_meta, &store.secret_key).unwrap();
    write_raw_blob(&store, STORE_META_KEY, &raw_meta_bytes);

    assert!(store.meta().is_err());
    assert!(store.needs_migration().is_err());

    cleanup_dir(&cwd);
  }

  #[test]
  fn test_store_handles_poisoned_env_lock() {
    let cwd = test_dir("store_test_poisoned_env_lock");
    let store = Store::new(StoreConfig {
      cwd: cwd.clone(),
      name: "poisoned-env-store".into(),
      defaults: serde_json::json!({
        "theme": "light"
      }),
      version: version("1.0.0"),
      ..Default::default()
    });

    poison_store_env(&store);

    assert_eq!(store.get("theme"), None);
    assert_eq!(store.get_all(), serde_json::json!({ "theme": "light" }));
    assert!(store.meta().is_err());
    assert!(store.needs_migration().is_err());
    assert!(store.reconcile_schema().is_err());
    assert!(store.migrate().is_err());
    assert!(store.set("foo".into(), serde_json::json!(1)).is_err());
    assert!(store.delete("foo").is_err());
    assert!(store.clear().is_err());
    assert!(store.set_all(vec![("foo".into(), serde_json::json!(1))]).is_err());

    cleanup_dir(&cwd);
  }
}

#[cfg(test)]
mod fallible_open_tests {
  use super::*;

  #[test]
  fn fallible_open_returns_directory_and_migration_errors() {
    let directory = std::env::temp_dir().join(format!("lintel-fallible-store-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("blocked.rkv"), b"file").unwrap();
    assert!(
      Store::try_new(StoreConfig {
        cwd: directory.clone(),
        name: "blocked".into(),
        ..Default::default()
      })
      .is_err()
    );
    let config = StoreConfig {
      cwd: directory.clone(),
      name: "migration".into(),
      version: Version::new(1, 0, 0),
      migration: Some(Arc::new(|_, _| Err(Error::other("migration failed")))),
      ..Default::default()
    };
    assert!(Store::try_new(config).is_err());
    let store = Store::try_new(StoreConfig {
      cwd: directory.clone(),
      name: "good".into(),
      ..Default::default()
    })
    .unwrap();
    store.set("value".into(), serde_json::json!(42)).unwrap();
    assert_eq!(store.get("value"), Some(serde_json::json!(42)));
    fs::remove_dir_all(directory).unwrap();
  }
}
