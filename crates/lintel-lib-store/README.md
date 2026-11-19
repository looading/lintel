# lintel-lib-store

`lintel-lib-store` 是一个通用的持久化 key-value store，面向桌面端配置和业务数据存储场景。

它支持：
- 默认值补齐
- schema 版本迁移
- 可选加密
- 动态 key 与 schema key 共存

## 设计约束

`defaults` 在这个 crate 里不是“把所有未声明 key 都删掉”的强 schema，而是：
- 声明一组由框架托管的默认字段
- 当字段缺失时自动补齐默认值
- 配合 `version` 和 `migration` 完成演进

这意味着：
- 新增默认字段：会自动补齐
- 删除默认字段：默认不会删除历史数据
- 动态 key：不会因为 defaults 变化被误删

## 基础用法

```rust
use std::path::PathBuf;
use lintel_lib_store::store::{Store, StoreConfig};

let store = Store::new(StoreConfig {
  cwd: PathBuf::from("./config"),
  name: "settings".into(),
  defaults: serde_json::json!({
    "theme": "light",
    "fontSize": 14
  }),
  version: "1.0.0".into(),
  ..Default::default()
});

store.set("theme".into(), serde_json::json!("dark"))?;
store.set("dynamicKey".into(), serde_json::json!(true))?;

assert_eq!(store.get("theme"), Some(serde_json::json!("dark")));
assert_eq!(store.get("fontSize"), Some(serde_json::json!(14)));
assert_eq!(store.get("dynamicKey"), Some(serde_json::json!(true)));
# Ok::<(), std::io::Error>(())
```

## defaults 同步策略

`StoreConfig.default_sync_policy` 控制 defaults 演进时的行为：

- `DefaultSyncPolicy::MissingOnly`
  - 只补齐新增的默认字段
  - 不删除已经从 defaults 移除的历史值
  - 这是默认值，也是最安全的模式
- `DefaultSyncPolicy::PruneRemovedManagedKeys`
  - 删除“曾经由 defaults 管理、现在已从 defaults 移除”的 key
  - 不会删除动态 key

```rust
use lintel_lib_store::store::{DefaultSyncPolicy, Store, StoreConfig};

let store = Store::new(StoreConfig {
  cwd: "./config".into(),
  name: "settings".into(),
  defaults: serde_json::json!({
    "theme": "light"
  }),
  version: "1.0.0".into(),
  default_sync_policy: DefaultSyncPolicy::MissingOnly,
  ..Default::default()
});
# let _ = store;
```

## migration 用法

当出现字段重命名、类型变更、需要显式删除旧字段时，使用 `migration`。

```rust
use std::sync::Arc;
use lintel_lib_store::store::{DefaultSyncPolicy, Store, StoreConfig};

let store = Store::new(StoreConfig {
  cwd: "./config".into(),
  name: "settings".into(),
  defaults: serde_json::json!({
    "themeMode": "light",
    "fontSize": 14
  }),
  version: "2.0.0".into(),
  default_sync_policy: DefaultSyncPolicy::MissingOnly,
  migration: Some(Arc::new(|store, ctx| {
    if ctx.from_version.as_deref() == Some("1.0.0") {
      if let Some(theme) = store.get("theme") {
        store.set("themeMode".into(), theme)?;
        store.delete("theme")?;
      }
      store.delete("legacySidebarState")?;
    }
    Ok(())
  })),
  ..Default::default()
});
# let _ = store;
```

## schema 元数据

store 内部会保留一个元数据 key，用于记录：
- 当前 version
- 当前 managed keys
- 当前 defaults hash

可以通过 `meta()` 查看：

```rust
use lintel_lib_store::store::{Store, StoreConfig};

let store = Store::new(StoreConfig {
  cwd: "./config".into(),
  name: "settings".into(),
  defaults: serde_json::json!({
    "theme": "light"
  }),
  version: "1.0.0".into(),
  ..Default::default()
});

let meta = store.meta()?;
println!("{meta:?}");
# Ok::<(), std::io::Error>(())
```

## 加密

传入 16 字节 `secret_key` 后，value 会在写入时进行 AES-128-ECB 加密。

```rust
use lintel_lib_store::store::{Store, StoreConfig};

let store = Store::new(StoreConfig {
  cwd: "./config".into(),
  name: "secure-settings".into(),
  secret_key: Some(*b"0123456789abcdef"),
  defaults: serde_json::json!({
    "token": ""
  }),
  version: "1.0.0".into(),
  ..Default::default()
});
# let _ = store;
```

## 对外 API

### StoreConfig

- `cwd`: store 所在目录
- `name`: store 名称
- `defaults`: 默认值集合
- `secret_key`: 可选加密 key
- `version`: 当前 schema 版本
- `default_sync_policy`: defaults 同步策略
- `migration`: 版本迁移钩子

### Store

- `new(config)`: 打开 store，并自动执行 migration + defaults reconcile
- `get(key)`: 读取单个值，不存在时回退到 defaults
- `set(key, value)`: 写入单个值
- `set_all(values)`: 批量写入
- `delete(key)`: 删除单个值
- `clear()`: 清空业务数据后重新同步 defaults 和 meta
- `reset(Some(key))`: 重置指定 key
- `reset(None)`: 重置整个 store
- `get_all()`: 获取 defaults 与已存值合并后的视图
- `meta()`: 查看当前 schema 元数据
- `needs_migration()`: 判断当前 version 是否与 store meta 不一致
- `reconcile_schema()`: 手动执行 schema 同步
- `migrate()`: 手动执行 migration + schema 同步

## 示例

可直接运行：

```bash
cargo run -p lintel-lib-store --example desk-store-example
```
