use std::{
  fmt,
  future::Future,
  sync::atomic::{AtomicBool, AtomicUsize, Ordering},
  sync::{Arc, OnceLock},
};

#[cfg(not(target_os = "linux"))]
use global_hotkey::GlobalHotKeyManager;
use lintel_lib_iced::Action;
use parking_lot::Mutex;

use anyhow::{Context, Result, anyhow};
use info::AppInfo;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use muda::MenuEvent;
use process_info::ProcessInfo;
use store::{AppResources, Resource, ResourceError, ResourceId, ServiceStore};
use tracing::info;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use tray_icon::TrayIconEvent;
use winit::event_loop::EventLoopProxy;

pub mod shell_context;

pub mod info;
pub mod process_info;

use crate::{
  event_loop::event::{AppEvent, LintelEvent, QuitState},
  plugin::{Plugin, manager::PluginManager},
  tasks::{TaskHandle, TaskScope, TaskSpawnError, TaskTracker},
};

pub mod service;
pub mod store;

pub static GLOBAL_CONTEXT: OnceLock<AppContext> = OnceLock::new();

pub fn set_global_context(context: AppContext) {
  GLOBAL_CONTEXT.set(context).unwrap();
}

pub fn get_global_context() -> Option<&'static AppContext> {
  GLOBAL_CONTEXT.get()
}

#[derive(Clone)]
pub struct AppContext {
  pub event_proxy: EventLoopProxy<Action<LintelEvent>>,
  pub plugin_manager: Arc<Mutex<PluginManager>>,
  pub app_info: AppInfo,
  pub runtime: tokio::runtime::Handle,
  pub process_info: ProcessInfo,
  pub service_store: Arc<store::ServiceStore>,
  pub resource_store: Arc<store::AppResources>,
  #[cfg(not(target_os = "linux"))]
  pub hotkeys_manager: Arc<GlobalHotKeyManager>,
  // 应用异步运行时，禁止在业务线程中使用，禁止阻塞性操作
  pub app_runtime: tokio::runtime::Handle,
  task_tracker: TaskTracker,
  destroyed: Arc<AtomicBool>,
  cleanup_completed: Arc<AtomicBool>,
  cleanup_notification: Arc<tokio::sync::Notify>,
  pending_main_tasks: Arc<AtomicUsize>,
}

unsafe impl Send for AppContext {}
unsafe impl Sync for AppContext {}

impl fmt::Debug for AppContext {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("AppContext")
      .field("event_proxy", &self.event_proxy)
      .field("app_info", &self.app_info)
      .field("runtime", &self.runtime)
      .field("process_info", &self.process_info)
      .finish()
  }
}

impl AppContext {
  /// 同步触发清理；异步退出路径应使用 shutdown 等待任务 Future 释放。
  pub fn destroy(&self) {
    if self.destroyed.swap(true, Ordering::AcqRel) {
      return;
    }
    let _completion = CleanupCompletion {
      completed: self.cleanup_completed.clone(),
      notification: self.cleanup_notification.clone(),
    };
    info!("app context drop");
    self.task_tracker.shutdown();
    self.clear_plugin();
    self.clear_resource();
    self.service_store.clear();
  }

  /// 先取消任务并等待其清理，再停止插件和释放服务。
  pub async fn shutdown(&self) {
    self.task_tracker.shutdown();
    self.task_tracker.wait_idle().await;
    // 启动中的插件由启动事务回滚，等回滚结束后才能释放其服务。
    loop {
      let notification = self.plugin_manager.lock().request_startup_stop();
      let Some(notification) = notification else {
        break;
      };
      let notified = notification.notified();
      tokio::pin!(notified);
      notified.as_mut().enable();
      let is_starting = self.plugin_manager.lock().is_starting();
      if is_starting {
        notified.await;
      }
    }
    self.destroy();
    // 其他线程可能已经开始同步清理，重复 shutdown 同样等待其结束。
    loop {
      let notification = self.cleanup_notification.notified();
      tokio::pin!(notification);
      notification.as_mut().enable();
      if self.cleanup_completed.load(Ordering::Acquire) {
        break;
      }
      notification.await;
    }
  }

  pub fn task_tracker(&self) -> &TaskTracker {
    &self.task_tracker
  }

  pub fn spawn_task<F>(&self, scope: TaskScope, future: F) -> Result<TaskHandle<F::Output>, TaskSpawnError>
  where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
  {
    self.task_tracker.spawn(&self.runtime, scope, future)
  }

  pub fn cancel_tasks(&self, scope: &TaskScope) -> usize {
    self.task_tracker.cancel_scope(scope)
  }

  /// 在全部名称检查通过后启动插件；回调可安全查询上下文服务和插件。
  pub fn start_plugins(&self) -> Result<()> {
    if self.task_tracker.is_shutting_down() {
      return Err(anyhow!("application is shutting down"));
    }
    let startup = self.plugin_manager.lock().prepare_start()?;
    startup.run(self)
  }

  pub fn dispatch_plugin_event(&self, event: &LintelEvent) {
    let plugins = self.plugin_manager.lock().active_plugins();
    for plugin in plugins {
      plugin.on_event(self, event);
    }
  }

  pub fn dispatch_hotkey_event(&self, event: global_hotkey::GlobalHotKeyEvent) {
    let plugins = self.plugin_manager.lock().active_plugins();
    for plugin in plugins {
      plugin.on_hotkeys_event(self, event);
    }
  }
  /// 注册服务
  pub fn register_service<F, S>(&self, service_getter: F)
  where
    F: Fn() -> Arc<S> + Send + Sync + 'static,
    S: Send + Sync + ?Sized + 'static,
  {
    self.service_store.register_service(service_getter);
  }

  /// 获取注册的服务
  pub fn get_service<S>(&self) -> Option<Arc<S>>
  where
    S: Send + Sync + ?Sized + 'static,
  {
    let s = self.service_store.get_service::<S>();
    if s.is_none() {
      tracing::warn!("service not found: {:?}", std::any::type_name::<S>());
    }
    s
  }

  /// 移除所有服务
  pub fn remove_service<S>(&self)
  where
    S: Send + Sync + ?Sized + 'static,
  {
    self.service_store.remove_service::<S>()
  }

  /// 移除插件
  pub fn clear_plugin(&self) {
    let cleanup = self.plugin_manager.lock().prepare_shutdown();
    cleanup.run(self);
    self.plugin_manager.lock().clear_registry();
  }

  /// 注册插件
  pub fn register_plugin(&self, plugin: Arc<dyn Plugin>) {
    self.plugin_manager.lock().register_plugin(plugin);
  }

  /// 移除插件
  pub fn remove_plugin(&self, name: &str) {
    self.plugin_manager.lock().unregister_plugin(name);
  }

  /// 获取插件
  pub fn get_plugin(&self, name: &str) -> Option<Arc<dyn Plugin>> {
    self.plugin_manager.lock().get_plugin(name)
  }

  /// 注册资源
  pub fn create_resource<T: Resource>(&self, resource: T) -> ResourceId {
    self.resource_store.insert(resource)
  }

  /// 获取资源
  pub fn get_resource<T: Resource>(&self, id: ResourceId) -> Result<Arc<T>, ResourceError> {
    self.resource_store.get(id)
  }

  /// 获取并移除资源
  pub fn take_resource<T: Resource>(&self, id: ResourceId) -> Result<Arc<T>, ResourceError> {
    self.resource_store.take(id)
  }

  /// 移除资源
  pub fn remove_resource(&self, id: ResourceId) -> Result<Arc<dyn Resource>, ResourceError> {
    self.resource_store.remove(id)
  }

  /// 移除所有资源
  pub fn clear_resource(&self) {
    self.resource_store.clear();
  }

  pub fn send_user_event(&self, event: LintelEvent) -> Result<()> {
    let ret = self
      .event_proxy
      .send_event(Action::Output(event))
      .map_err(|err| anyhow!("EventLoopError: {err:?}"));
    ret
  }
}

pub trait AppContextApi {
  fn is_in_app_main_thread(&self) -> bool;

  fn run_on_app_main_thread<F>(&self, f: F) -> Result<()>
  where
    F: FnOnce() + Send + 'static;

  fn send_app_event(&self, event: AppEvent) -> Result<()>;

  #[cfg(any(target_os = "macos", target_os = "windows"))]
  fn send_menu_event(&self, event: MenuEvent) -> Result<()>;

  #[cfg(any(target_os = "macos", target_os = "windows"))]
  fn send_tray_event(&self, event: TrayIconEvent) -> Result<()>;

  fn send_task_event(&self, task: Box<dyn FnOnce() + Send>) -> Result<()>;

  /// 退出应用
  fn quit(&self) -> Result<()> {
    self.send_app_event(AppEvent::Quit(QuitState::Init))
  }

  /// 退出应用, 携带状态mas
  fn exit(&self, code: i32) -> Result<()> {
    self.send_app_event(AppEvent::Exit(code, QuitState::Init))
  }

  /// 重启应用
  fn relaunch(&self, args: Option<Vec<String>>) -> Result<()> {
    self.send_app_event(AppEvent::Relaunch(args, QuitState::Init))
  }

  /// 使用系统默认文件管理器打开
  fn open_path(&self, path: String) -> Result<()> {
    open::that(path).map_err(|err| anyhow!("open path failed: {err:?}"))
  }

  /// 使用系统默认浏览器打开
  fn open_url(&self, url: String) -> Result<()> {
    open::that(url).map_err(|err| anyhow!("open url failed: {err:?}"))
  }
}

impl AppContextApi for AppContext {
  fn is_in_app_main_thread(&self) -> bool {
    self.process_info.is_in_app_main_thread()
  }

  fn run_on_app_main_thread<F>(&self, f: F) -> Result<()>
  where
    F: FnOnce() + Send + 'static,
  {
    self.send_task_event(Box::new(f))
  }

  fn send_app_event(&self, event: AppEvent) -> Result<()> {
    self
      .send_user_event(LintelEvent::AppEvent(event))
      .map_err(|_| anyhow!("EventLoopClosed"))
  }

  #[cfg(any(target_os = "macos", target_os = "windows"))]
  fn send_menu_event(&self, event: MenuEvent) -> Result<()> {
    self
      .send_user_event(LintelEvent::MenuEvent(event))
      .map_err(|_| anyhow!("EventLoopClosed"))
  }

  #[cfg(any(target_os = "macos", target_os = "windows"))]
  fn send_tray_event(&self, event: TrayIconEvent) -> Result<()> {
    self
      .send_user_event(LintelEvent::TrayIconEvent(event))
      .map_err(|_| anyhow!("EventLoopClosed"))
  }

  fn send_task_event(&self, task: Box<dyn FnOnce() + Send>) -> Result<()> {
    const MAX_PENDING_MAIN_TASKS: usize = 1024;
    if self.task_tracker.is_shutting_down() {
      return Err(anyhow!("application is shutting down"));
    }
    self
      .pending_main_tasks
      .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
        (pending < MAX_PENDING_MAIN_TASKS).then_some(pending + 1)
      })
      .map_err(|_| anyhow!("application main-thread task queue is full"))?;
    let permit = MainTaskPermit(self.pending_main_tasks.clone());
    self.send_user_event(LintelEvent::Task(Box::new(move || {
      let _permit = permit;
      task();
    })))
  }

  /// 退出应用
  fn quit(&self) -> Result<()> {
    self.send_app_event(AppEvent::Quit(QuitState::Init))
  }

  /// 退出应用, 携带状态mas
  fn exit(&self, code: i32) -> Result<()> {
    self.send_app_event(AppEvent::Exit(code, QuitState::Init))
  }

  /// 重启应用
  fn relaunch(&self, args: Option<Vec<String>>) -> Result<()> {
    self.send_app_event(AppEvent::Relaunch(args, QuitState::Init))
  }
}

pub struct AppContextBuilder {
  pub event_proxy: Option<EventLoopProxy<Action<LintelEvent>>>,
  pub plugin_manager: Arc<Mutex<PluginManager>>,
  pub app_info: Option<AppInfo>,
  pub plugins: Vec<Box<dyn FnOnce(&mut AppContext) + Send + Sync>>,
  pub runtime: Option<tokio::runtime::Handle>,
  pub process_info: Option<ProcessInfo>,
  pub app_runtime: Option<tokio::runtime::Handle>,
}

impl AppContextBuilder {
  pub fn new() -> Self {
    Self {
      plugins: Vec::new(),
      event_proxy: None,
      plugin_manager: Arc::new(Mutex::new(PluginManager::new())),
      app_info: None,
      runtime: None,
      process_info: None,
      app_runtime: None,
    }
  }

  pub fn with_event_proxy(mut self, event_proxy: EventLoopProxy<Action<LintelEvent>>) -> Self {
    self.event_proxy = Some(event_proxy);
    self
  }

  pub fn with_plugins(mut self, plugins: Vec<Box<dyn FnOnce(&mut AppContext) + Send + Sync>>) -> Self {
    self.plugins = plugins;
    self
  }
  pub fn with_app_info(mut self, app_info: AppInfo) -> Self {
    self.app_info = Some(app_info);
    self
  }
  pub fn with_runtime(mut self, runtime: tokio::runtime::Handle) -> Self {
    self.runtime = Some(runtime);
    self
  }

  pub fn with_process_info(mut self, process_info: ProcessInfo) -> Self {
    self.process_info = Some(process_info);
    self
  }

  pub fn with_app_runtime(mut self, app_runtime: tokio::runtime::Handle) -> Self {
    self.app_runtime = Some(app_runtime);
    self
  }

  pub fn build(self) -> AppContext {
    self.try_build().expect("cannot build application context")
  }

  /// 可失败的构建入口，供实际运行时传播配置和原生初始化错误。
  pub fn try_build(self) -> Result<AppContext> {
    let event_proxy = self
      .event_proxy
      .context("application context requires an event proxy")?;
    let app_info = self.app_info.context("application context requires app information")?;
    let runtime = self.runtime.context("application context requires a task runtime")?;
    let process_info = self
      .process_info
      .context("application context requires process information")?;
    let app_runtime = self
      .app_runtime
      .context("application context requires an application runtime")?;
    #[cfg(not(target_os = "linux"))]
    let hotkeys_manager =
      GlobalHotKeyManager::new().map_err(|error| anyhow!("cannot initialize global hotkey manager: {error}"))?;
    let mut context = AppContext {
      event_proxy,
      plugin_manager: self.plugin_manager,
      app_info,
      runtime,
      process_info,
      service_store: Arc::new(ServiceStore::new()),
      resource_store: Arc::new(AppResources::new()),
      #[cfg(not(target_os = "linux"))]
      hotkeys_manager: Arc::new(hotkeys_manager),
      app_runtime,
      task_tracker: TaskTracker::new(),
      destroyed: Arc::new(AtomicBool::new(false)),
      cleanup_completed: Arc::new(AtomicBool::new(false)),
      cleanup_notification: Arc::new(tokio::sync::Notify::new()),
      pending_main_tasks: Arc::new(AtomicUsize::new(0)),
    };

    // exec plugin creator
    for plugin in self.plugins {
      if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| plugin(&mut context))).is_err() {
        context.destroy();
        return Err(anyhow!("plugin creator panicked"));
      }
    }

    if GLOBAL_CONTEXT.set(context.clone()).is_err() {
      context.destroy();
      return Err(anyhow!("application context has already been initialized"));
    }
    Ok(context)
  }
}

struct MainTaskPermit(Arc<AtomicUsize>);

struct CleanupCompletion {
  completed: Arc<AtomicBool>,
  notification: Arc<tokio::sync::Notify>,
}

impl Drop for CleanupCompletion {
  fn drop(&mut self) {
    self.completed.store(true, Ordering::Release);
    self.notification.notify_waiters();
  }
}

impl Drop for MainTaskPermit {
  fn drop(&mut self) {
    self.0.fetch_sub(1, Ordering::AcqRel);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn fallible_context_builder_reports_missing_configuration_before_native_initialization() {
    assert!(
      AppContextBuilder::new()
        .try_build()
        .unwrap_err()
        .to_string()
        .contains("event proxy")
    );
  }
}
