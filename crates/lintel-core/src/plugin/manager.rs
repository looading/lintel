use std::{
  collections::HashSet,
  panic::{AssertUnwindSafe, catch_unwind},
  sync::Arc,
};

use anyhow::{Context, Result, anyhow, bail};
use global_hotkey::GlobalHotKeyEvent;
use parking_lot::Mutex;

use crate::{context::AppContext, event_loop::event::LintelEvent, plugin::Plugin};

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum Phase {
  #[default]
  Registered,
  Starting,
  Running,
  Stopped,
}

#[derive(Default)]
struct Lifecycle {
  phase: Phase,
  stop_requested: bool,
  active: Vec<Arc<dyn Plugin>>,
}

#[derive(Default)]
pub struct PluginManager {
  plugins: Vec<Arc<dyn Plugin>>,
  lifecycle: Arc<Mutex<Lifecycle>>,
  startup_finished: Arc<tokio::sync::Notify>,
}

/// 启动事务从管理器分离，回调执行期间不持有管理器锁。
pub(crate) struct PluginStartup {
  plugins: Vec<Arc<dyn Plugin>>,
  lifecycle: Arc<Mutex<Lifecycle>>,
  startup_finished: Arc<tokio::sync::Notify>,
}

/// 先取走清理名单，再执行回调，防止重复退出和回调重入。
#[derive(Default)]
pub(crate) struct PluginCleanup {
  stop: Vec<Arc<dyn Plugin>>,
  drop: Vec<Arc<dyn Plugin>>,
}

impl PluginCleanup {
  pub(crate) fn run(self, context: &AppContext) {
    self.run_with(|plugin| plugin.on_stop(context), |plugin| plugin.on_drop(context));
  }

  fn run_with(self, mut stop: impl FnMut(&Arc<dyn Plugin>), mut drop: impl FnMut(&Arc<dyn Plugin>)) {
    for plugin in self.stop.into_iter().rev() {
      let _span = tracing::debug_span!("plugin_stop", plugin = plugin.name()).entered();
      if catch_unwind(AssertUnwindSafe(|| stop(&plugin))).is_err() {
        tracing::error!(plugin = plugin.name(), "plugin stop panicked; continuing cleanup");
      }
    }
    for plugin in self.drop.into_iter().rev() {
      if catch_unwind(AssertUnwindSafe(|| drop(&plugin))).is_err() {
        tracing::error!(plugin = plugin.name(), "plugin drop panicked; continuing cleanup");
      }
    }
  }
}

impl PluginStartup {
  pub(crate) fn run(self, context: &AppContext) -> Result<()> {
    self.run_with(
      |plugin| plugin.on_start(context),
      |plugin| plugin.on_stop(context),
      |plugin| plugin.on_drop(context),
    )
  }

  fn run_with(
    self,
    mut start: impl FnMut(&Arc<dyn Plugin>) -> Result<()>,
    stop: impl FnMut(&Arc<dyn Plugin>),
    drop: impl FnMut(&Arc<dyn Plugin>),
  ) -> Result<()> {
    let mut attempted = Vec::new();
    let mut result = Ok(());
    for plugin in &self.plugins {
      if self.lifecycle.lock().stop_requested {
        result = Err(anyhow!("plugin startup cancelled by application shutdown"));
        break;
      }
      // 失败的钩子也可能已经获取资源，因此也必须参与回滚。
      attempted.push(plugin.clone());
      let _span = tracing::debug_span!("plugin_start", plugin = plugin.name()).entered();
      result = catch_unwind(AssertUnwindSafe(|| start(plugin)))
        .unwrap_or_else(|_| Err(anyhow!("plugin start panicked")))
        .with_context(|| format!("cannot start plugin {:?}", plugin.name()));
      if result.is_err() {
        break;
      }
    }
    {
      let mut lifecycle = self.lifecycle.lock();
      if lifecycle.stop_requested && result.is_ok() {
        result = Err(anyhow!("plugin startup cancelled by application shutdown"));
      }
      if result.is_ok() {
        lifecycle.active = attempted;
        lifecycle.phase = Phase::Running;
        self.startup_finished.notify_waiters();
        return Ok(());
      }
    }
    PluginCleanup {
      stop: attempted,
      drop: unique_plugins(self.plugins),
    }
    .run_with(stop, drop);
    self.lifecycle.lock().phase = Phase::Stopped;
    self.startup_finished.notify_waiters();
    result
  }
}

fn unique_plugins(plugins: Vec<Arc<dyn Plugin>>) -> Vec<Arc<dyn Plugin>> {
  let mut unique = Vec::new();
  for plugin in plugins {
    if !unique.iter().any(|existing| Arc::ptr_eq(existing, &plugin)) {
      unique.push(plugin);
    }
  }
  unique
}

impl PluginManager {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn register_plugin(&mut self, plugin: Arc<dyn Plugin>) {
    self.plugins.push(plugin);
  }

  pub fn get_plugin(&self, name: &str) -> Option<Arc<dyn Plugin>> {
    self.plugins.iter().find(|plugin| plugin.name() == name).cloned()
  }

  pub fn get_plugins(&self) -> &Vec<Arc<dyn Plugin>> {
    &self.plugins
  }

  pub fn get_plugins_mut(&mut self) -> &mut Vec<Arc<dyn Plugin>> {
    &mut self.plugins
  }

  pub fn get_plugin_mut(&mut self, name: &str) -> Option<&mut Arc<dyn Plugin>> {
    self.plugins.iter_mut().find(|plugin| plugin.name() == name)
  }

  pub fn unregister_plugin(&mut self, name: &str) {
    self.plugins.retain(|plugin| plugin.name() != name);
  }

  pub(crate) fn prepare_start(&self) -> Result<PluginStartup> {
    let mut names = HashSet::new();
    for plugin in &self.plugins {
      if plugin.name().is_empty() {
        bail!("plugin name must not be empty");
      }
      if !names.insert(plugin.name()) {
        bail!("duplicate plugin name {:?}", plugin.name());
      }
    }
    let mut lifecycle = self.lifecycle.lock();
    match lifecycle.phase {
      Phase::Registered => lifecycle.phase = Phase::Starting,
      Phase::Starting => bail!("plugins are already starting"),
      Phase::Running => bail!("plugins have already started"),
      Phase::Stopped => bail!("plugins have already stopped"),
    }
    Ok(PluginStartup {
      plugins: self.plugins.clone(),
      lifecycle: self.lifecycle.clone(),
      startup_finished: self.startup_finished.clone(),
    })
  }

  pub(crate) fn request_startup_stop(&self) -> Option<Arc<tokio::sync::Notify>> {
    let mut lifecycle = self.lifecycle.lock();
    if lifecycle.phase != Phase::Starting {
      return None;
    }
    lifecycle.stop_requested = true;
    Some(self.startup_finished.clone())
  }

  pub(crate) fn is_starting(&self) -> bool {
    self.lifecycle.lock().phase == Phase::Starting
  }

  pub(crate) fn prepare_shutdown(&self) -> PluginCleanup {
    let mut lifecycle = self.lifecycle.lock();
    match lifecycle.phase {
      Phase::Stopped => PluginCleanup::default(),
      Phase::Starting => {
        // 当前钩子返回后由启动事务回滚，避免并发清理正在运行的钩子。
        lifecycle.stop_requested = true;
        PluginCleanup::default()
      }
      Phase::Registered | Phase::Running => {
        lifecycle.phase = Phase::Stopped;
        PluginCleanup {
          stop: std::mem::take(&mut lifecycle.active),
          drop: unique_plugins(self.plugins.clone()),
        }
      }
    }
  }

  pub(crate) fn active_plugins(&self) -> Vec<Arc<dyn Plugin>> {
    self.lifecycle.lock().active.clone()
  }

  pub(crate) fn clear_registry(&mut self) {
    self.plugins.clear();
  }

  /// 推荐通过 AppContext::start_plugins 调用，以免调用方持有管理器锁。
  pub fn start(&self, context: &AppContext) -> Result<()> {
    self.prepare_start()?.run(context)
  }

  pub fn clear(&mut self, context: &AppContext) {
    self.prepare_shutdown().run(context);
    self.clear_registry();
  }

  /// 兼容旧入口；需要处理启动错误的调用方请使用 start。
  pub fn on_init(&self, context: &AppContext) {
    if let Err(error) = self.start(context) {
      tracing::error!(?error, "plugin initialization failed");
    }
  }

  pub fn on_exit(&self, context: &AppContext) {
    self.prepare_shutdown().run(context);
  }

  pub fn on_hotkeys_event(&self, context: &AppContext, event: GlobalHotKeyEvent) {
    for plugin in self.active_plugins() {
      plugin.on_hotkeys_event(context, event);
    }
  }

  pub fn on_event(&self, context: &AppContext, event: &LintelEvent) {
    for plugin in self.active_plugins() {
      plugin.on_event(context, event);
    }
  }
}

impl From<Vec<Arc<dyn Plugin>>> for PluginManager {
  fn from(plugins: Vec<Arc<dyn Plugin>>) -> Self {
    Self {
      plugins,
      ..Self::default()
    }
  }
}

#[cfg(test)]
mod tests {
  use std::cell::RefCell;

  use super::*;
  use crate::plugin::PluginLifecycle;

  struct TestPlugin(&'static str);
  impl PluginLifecycle for TestPlugin {}
  impl Plugin for TestPlugin {
    fn name(&self) -> &str {
      self.0
    }
  }

  fn manager(names: &[&'static str]) -> PluginManager {
    PluginManager::from(
      names
        .iter()
        .map(|name| Arc::new(TestPlugin(name)) as Arc<dyn Plugin>)
        .collect::<Vec<_>>(),
    )
  }

  #[test]
  fn duplicate_names_are_rejected_before_any_start_hook() {
    let manager = manager(&["a", "a"]);
    assert!(
      manager
        .prepare_start()
        .err()
        .unwrap()
        .to_string()
        .contains("duplicate plugin name")
    );
    assert!(manager.lifecycle.lock().phase == Phase::Registered);
    assert!(manager.active_plugins().is_empty());
  }

  #[test]
  fn failed_start_rolls_back_attempted_plugins_in_reverse_and_cleans_up_once() {
    let manager = manager(&["a", "b", "c"]);
    let events = RefCell::new(Vec::new());
    let result = manager.prepare_start().unwrap().run_with(
      |plugin| {
        events.borrow_mut().push(format!("start:{}", plugin.name()));
        if plugin.name() == "b" {
          bail!("unavailable");
        }
        Ok(())
      },
      |plugin| events.borrow_mut().push(format!("stop:{}", plugin.name())),
      |plugin| events.borrow_mut().push(format!("drop:{}", plugin.name())),
    );
    assert!(result.unwrap_err().to_string().contains('b'));
    manager.prepare_shutdown().run_with(
      |plugin| events.borrow_mut().push(format!("stop-again:{}", plugin.name())),
      |plugin| events.borrow_mut().push(format!("drop-again:{}", plugin.name())),
    );
    assert_eq!(
      *events.borrow(),
      ["start:a", "start:b", "stop:b", "stop:a", "drop:c", "drop:b", "drop:a"]
    );
    assert!(manager.prepare_start().is_err());
  }

  #[test]
  fn normal_shutdown_is_reverse_order_and_idempotent() {
    let manager = manager(&["a", "b"]);
    manager
      .prepare_start()
      .unwrap()
      .run_with(|_| Ok(()), |_| {}, |_| {})
      .unwrap();
    let events = RefCell::new(Vec::new());
    manager.prepare_shutdown().run_with(
      |plugin| events.borrow_mut().push(format!("stop:{}", plugin.name())),
      |plugin| events.borrow_mut().push(format!("drop:{}", plugin.name())),
    );
    manager.prepare_shutdown().run_with(
      |plugin| events.borrow_mut().push(format!("stop-again:{}", plugin.name())),
      |plugin| events.borrow_mut().push(format!("drop-again:{}", plugin.name())),
    );
    assert_eq!(*events.borrow(), ["stop:b", "stop:a", "drop:b", "drop:a"]);
    assert!(manager.active_plugins().is_empty());
  }

  #[test]
  fn shutdown_during_startup_defers_cleanup_until_the_hook_returns() {
    let manager = manager(&["a", "b"]);
    let events = RefCell::new(Vec::new());
    let result = manager.prepare_start().unwrap().run_with(
      |plugin| {
        events.borrow_mut().push(format!("start:{}", plugin.name()));
        manager.prepare_shutdown().run_with(
          |plugin| events.borrow_mut().push(format!("concurrent-stop:{}", plugin.name())),
          |plugin| events.borrow_mut().push(format!("concurrent-drop:{}", plugin.name())),
        );
        Ok(())
      },
      |plugin| events.borrow_mut().push(format!("stop:{}", plugin.name())),
      |_| {},
    );
    assert!(result.is_err());
    assert_eq!(*events.borrow(), ["start:a", "stop:a"]);
  }

  #[tokio::test]
  async fn startup_shutdown_notification_waits_for_rollback_cleanup() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let manager = manager(&["a"]);
    let startup = manager.prepare_start().unwrap();
    let cleaned = Arc::new(AtomicBool::new(false));
    let cleanup_signal = cleaned.clone();
    let (started, started_rx) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::sync_channel(1);
    let startup_task = tokio::task::spawn_blocking(move || {
      let mut started = Some(started);
      startup.run_with(
        |_| {
          started.take().unwrap().send(()).unwrap();
          released.recv().unwrap();
          Ok(())
        },
        |_| {
          cleanup_signal.store(true, Ordering::Release);
        },
        |_| {},
      )
    });
    started_rx.await.unwrap();
    let notification = manager.request_startup_stop().unwrap();
    let notified = notification.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    release.send(()).unwrap();
    notified.await;
    assert!(cleaned.load(Ordering::Acquire));
    assert!(!manager.is_starting());
    assert!(startup_task.await.unwrap().is_err());
  }
}
