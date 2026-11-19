use std::sync::Arc;

use anyhow::{Result, ensure};
use lintel_core::{
  context::AppContext,
  plugin::{Plugin, PluginLifecycle},
};

pub use lintel_lib_store::store::{Store, StoreConfig};

/// 在应用启动时打开配置，迁移失败会阻止插件启动。
pub fn store(config: StoreConfig) -> impl FnOnce(&mut AppContext) + Send + Sync {
  move |context| context.register_plugin(Arc::new(StorePlugin(config)))
}

struct StorePlugin(StoreConfig);

impl Plugin for StorePlugin {
  fn name(&self) -> &str {
    "lintel.store"
  }
}

impl PluginLifecycle for StorePlugin {
  fn on_start(&self, context: &AppContext) -> Result<()> {
    let mut config = self.0.clone();
    ensure!(
      !config.name.is_empty()
        && config.name.len() <= 128
        && config
          .name
          .bytes()
          .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        && config.name != "."
        && config.name != "..",
      "invalid store name"
    );
    if config.cwd.as_os_str().is_empty() {
      config.cwd = context.app_info.project_dir.config_dir().to_path_buf();
    }
    let store = Arc::new(Store::try_new(config)?);
    context.register_service(move || Arc::clone(&store));
    Ok(())
  }

  fn on_stop(&self, context: &AppContext) {
    context.remove_service::<Store>();
  }
}
