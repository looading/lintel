// 抽取自 lintel_desktop_next/app/src/runner/mod.rs。
#[cfg(all(feature = "desktop", feature = "public-iced"))]
use std::sync::Arc;
use std::{ops::Deref, process, thread, time::Duration};

use anyhow::{Context, Result, anyhow};
use global_hotkey::GlobalHotKeyEvent;
use tokio::runtime::Builder;
use tracing::{info, warn};
use winit::event_loop::{ActiveEventLoop, EventLoop};

use lintel_core::{
  context::{AppContext, AppContextApi, AppContextBuilder, info::AppInfoBuilder, process_info::ProcessInfo},
  event_loop::event::{AppEvent, LintelEvent, QuitState},
};
use lintel_lib_iced::Action;
use lintel_lib_store::store::{Store, StoreConfig};

mod event_loop;
#[cfg(target_os = "macos")]
mod macos_reopen;

use event_loop::AppRunner;

#[cfg(feature = "public-cef")]
use lintel_iced_widgets::webview::BrowserProcessMessage;

pub type PluginCreator = Box<dyn FnOnce(&mut AppContext) + Send + Sync>;

/// 操作系统或宿主转发给运行中应用的激活事件。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActivationEvent {
  Opened {
    urls: Vec<String>,
  },
  Reopen {
    has_visible_window: bool,
  },
  /// 初次启动或其他实例转发的启动数据；URL 由显式 scheme 策略验证。
  Launch {
    arguments: Vec<String>,
    working_directory: std::path::PathBuf,
    urls: Vec<String>,
    source: ActivationSource,
  },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivationSource {
  Initial,
  SecondaryInstance,
}

type ActivationHandler = Box<dyn Fn(&AppContext, ActivationEvent) + Send + Sync>;
#[cfg(feature = "desktop")]
type DesktopEventHandler = Box<dyn Fn(&AppContext, lintel_desktop::DesktopEvent) + Send + Sync>;

/// CEF 宿主在外层完成 cef::shutdown 和单实例锁释放后，再执行替换进程启动。
#[cfg(feature = "public-cef")]
#[derive(Clone, Default)]
pub(crate) struct DeferredRelaunch {
  request: std::sync::Arc<std::sync::Mutex<Option<(lintel_core::utils::process::Env, Option<Vec<String>>)>>>,
}

#[cfg(feature = "public-cef")]
impl DeferredRelaunch {
  fn record(&self, env: lintel_core::utils::process::Env, args: Option<Vec<String>>) -> Result<()> {
    let mut request = self
      .request
      .lock()
      .map_err(|_| anyhow!("CEF relaunch handoff lock poisoned"))?;
    anyhow::ensure!(request.is_none(), "CEF relaunch is already queued");
    *request = Some((env, args));
    Ok(())
  }
  pub(crate) fn run(self) -> Result<()> {
    let request = self
      .request
      .lock()
      .map_err(|_| anyhow!("CEF relaunch handoff lock poisoned"))?
      .take();
    if let Some((env, args)) = request {
      lintel_core::utils::process::spawn_replacement(&env, args.as_deref())
        .context("cannot relaunch application after CEF cleanup")?;
    }
    Ok(())
  }
}

/// 浏览器关闭协调器可推迟退出，让 CEF 继续处理关闭回调。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuitDecision {
  Proceed,
  Defer,
}

/// 注册为 AppContext 服务后，所有 Quit/Exit/Relaunch 的 Init 请求都会先经过此协调器。
/// Defer 后由协调器在窗口关闭完毕时重新发送原始 Init 事件。
pub struct QuitCoordinator {
  handler: Box<dyn Fn(&AppContext, &AppEvent) -> Result<QuitDecision> + Send + Sync>,
}

impl QuitCoordinator {
  pub fn new(handler: impl Fn(&AppContext, &AppEvent) -> Result<QuitDecision> + Send + Sync + 'static) -> Self {
    Self {
      handler: Box::new(handler),
    }
  }

  pub fn decide(&self, context: &AppContext, event: &AppEvent) -> Result<QuitDecision> {
    (self.handler)(context, event)
  }
}

/// 使用迁出的 iced runner 和插件管理器运行独立应用。
pub struct AppBuilder {
  name: &'static str,
  app_id: Option<String>,
  plugins: Vec<PluginCreator>,
  #[cfg(feature = "multiprocess")]
  processes: Option<crate::process::HostConfig>,
  activation_handler: Option<ActivationHandler>,
  #[cfg(feature = "crash-reporting")]
  crash: Option<lintel_crash::CrashConfig>,
  #[cfg(feature = "desktop")]
  desktop_enabled: bool,
  #[cfg(all(feature = "desktop", feature = "public-iced"))]
  accessibility: bool,
  #[cfg(feature = "desktop")]
  activation_options: crate::activation::ActivationOptions,
  #[cfg(feature = "desktop")]
  prepared_activation: Option<crate::activation::ActivationRuntime>,
  #[cfg(feature = "desktop")]
  desktop_event_handler: Option<DesktopEventHandler>,
  #[cfg(feature = "public-cef")]
  cef_pump: Option<tokio::sync::mpsc::UnboundedReceiver<BrowserProcessMessage>>,
  #[cfg(feature = "public-cef")]
  deferred_relaunch: Option<DeferredRelaunch>,
}

impl AppBuilder {
  pub fn new(name: &'static str) -> Self {
    Self {
      name,
      app_id: None,
      plugins: Vec::new(),
      #[cfg(feature = "multiprocess")]
      processes: None,
      activation_handler: None,
      #[cfg(feature = "crash-reporting")]
      crash: None,
      #[cfg(feature = "desktop")]
      desktop_enabled: false,
      #[cfg(all(feature = "desktop", feature = "public-iced"))]
      accessibility: false,
      #[cfg(feature = "desktop")]
      activation_options: Default::default(),
      #[cfg(feature = "desktop")]
      prepared_activation: None,
      #[cfg(feature = "desktop")]
      desktop_event_handler: None,
      #[cfg(feature = "public-cef")]
      cef_pump: None,
      #[cfg(feature = "public-cef")]
      deferred_relaunch: None,
    }
  }

  pub(crate) fn with_app_id(mut self, app_id: String) -> Self {
    self.app_id = Some(app_id);
    self
  }

  /// 以现有 AppContext 服务公开进程宿主，并在退出/启动失败时等待回收。
  #[cfg(feature = "multiprocess")]
  pub fn processes(mut self, config: crate::process::HostConfig) -> Self {
    self.processes = Some(config);
    self
  }

  pub fn with_plugins(mut self, plugins: Vec<PluginCreator>) -> Self {
    self.plugins = plugins;
    self
  }

  /// Installs local native crash capture after secondary-instance forwarding and before GUI startup.
  #[cfg(feature = "crash-reporting")]
  pub fn crash_reporting(mut self, config: lintel_crash::CrashConfig) -> Self {
    self.crash = Some(config);
    self
  }

  /// 接收 URL、重新激活及初次/后续进程启动数据。
  /// 配置 protocol_schemes 后统一校验 URL；未配置时保持宿主自行校验的约定。
  pub fn with_activation_handler(
    mut self,
    handler: impl Fn(&AppContext, ActivationEvent) + Send + Sync + 'static,
  ) -> Self {
    self.activation_handler = Some(Box::new(handler));
    self
  }

  /// 在创建 GUI/插件前将后续实例的参数转发给主实例。
  #[cfg(feature = "desktop")]
  pub fn single_instance(mut self) -> Self {
    self.activation_options.single_instance = true;
    self
  }

  /// 设置显式 URL scheme 白名单，并从启动参数中提取允许的 URL。
  #[cfg(feature = "desktop")]
  pub fn protocol_schemes(mut self, schemes: impl IntoIterator<Item = impl Into<String>>) -> Self {
    self.activation_options.protocol_schemes = Some(schemes.into_iter().map(Into::into).collect());
    self
  }

  #[cfg(feature = "desktop")]
  pub(crate) fn with_activation_options(mut self, options: crate::activation::ActivationOptions) -> Self {
    self.activation_options = options;
    self
  }

  #[cfg(feature = "desktop")]
  pub(crate) fn with_prepared_activation(mut self, activation: crate::activation::ActivationRuntime) -> Self {
    self.prepared_activation = Some(activation);
    self
  }

  /// 启用主线程原生桌面服务，可从 AppContext 获取 DesktopService。
  #[cfg(all(feature = "desktop", feature = "public-iced"))]
  pub fn with_accessibility(mut self) -> Self {
    self.accessibility = true;
    self
  }

  #[cfg(feature = "desktop")]
  pub fn with_desktop(mut self) -> Self {
    self.desktop_enabled = true;
    self
  }

  /// 启用桌面服务并接收逻辑菜单、托盘和全局快捷键事件。
  #[cfg(feature = "desktop")]
  pub fn with_desktop_event_handler(
    mut self,
    handler: impl Fn(&AppContext, lintel_desktop::DesktopEvent) + Send + Sync + 'static,
  ) -> Self {
    self.desktop_enabled = true;
    self.desktop_event_handler = Some(Box::new(handler));
    self
  }

  #[cfg(feature = "public-cef")]
  pub(crate) fn with_deferred_relaunch(mut self, relaunch: DeferredRelaunch) -> Self {
    self.deferred_relaunch = Some(relaunch);
    self
  }

  /// 连接已初始化的 CEF 消息泵。调用方负责先退出浏览器，再关闭 CEF。
  #[cfg(feature = "public-cef")]
  pub fn with_cef_pump(mut self, pump: tokio::sync::mpsc::UnboundedReceiver<BrowserProcessMessage>) -> Self {
    self.cef_pump = Some(pump);
    self
  }

  /// 在主线程启动事件循环，并在收到首个启动事件时调用 setup。
  pub fn run(self, setup: impl Fn(&AppContext) + Send + Sync + 'static) -> Result<()> {
    self.try_run(move |context| {
      setup(context);
      Ok(())
    })
  }

  /// 在插件启动完成后的 Init 阶段执行可失败 setup；清理完成后返回原始错误。
  pub fn try_run(self, setup: impl Fn(&AppContext) -> Result<()> + Send + Sync + 'static) -> Result<()> {
    #[cfg(feature = "crash-reporting")]
    if lintel_crash::helper_entry()? {
      return Ok(());
    }
    let project_dir = crate::activation::project_dirs(self.name, self.app_id.as_deref())?;
    #[cfg(feature = "desktop")]
    let activation = match self.prepared_activation {
      Some(activation) => activation,
      None => match crate::activation::prepare(&project_dir, self.activation_options)? {
        crate::activation::PreparedActivation::Forwarded => return Ok(()),
        crate::activation::PreparedActivation::Primary(activation) => activation,
      },
    };

    #[cfg(feature = "crash-reporting")]
    let _crash_reporter = self.crash.map(lintel_crash::CrashReporter::start).transpose()?;

    let runtime = RuntimeOwner::new(Builder::new_multi_thread().enable_all().build()?);
    let _runtime = runtime.enter();
    let app_runtime = RuntimeOwner::new(
      Builder::new_multi_thread()
        .enable_all()
        .thread_name("app_runtime")
        .build()?,
    );

    #[cfg(feature = "multiprocess")]
    let mut process_owner = crate::process::ProcessOwner::start(self.processes, runtime.handle().clone())?;

    let mut event_loop_builder = EventLoop::<Action<LintelEvent>>::with_user_event();
    #[cfg(all(feature = "desktop", target_os = "windows"))]
    let menu_message_hook = self
      .desktop_enabled
      .then(lintel_desktop::windows_menu::WindowsMenuMessageHook::default);
    #[cfg(all(feature = "desktop", target_os = "windows"))]
    if let Some(hook) = &menu_message_hook {
      hook.install(&mut event_loop_builder);
    }
    let event_loop = event_loop_builder.build()?;
    let proxy = event_loop.create_proxy();
    #[cfg(target_os = "macos")]
    let _reopen_bridge = if self.activation_handler.is_some() {
      let proxy = proxy.clone();
      Some(macos_reopen::ReopenBridge::install(move || {
        let _ = proxy.send_event(Action::Output(LintelEvent::Task(Box::new(|| {}))));
      })?)
    } else {
      None
    };
    let native_factory = {
      #[cfg(all(feature = "desktop", feature = "public-iced"))]
      {
        self.accessibility.then(|| {
          Arc::new(crate::accessibility::Factory { proxy: proxy.clone() })
            as Arc<dyn lintel_lib_iced::iced_winit::NativeWindowFactory>
        })
      }
      #[cfg(not(all(feature = "desktop", feature = "public-iced")))]
      {
        None
      }
    };
    let runner = lintel_plugin_iced::create_iced_runner_with_native_windows(
      proxy.clone(),
      event_loop.owned_display_handle(),
      self.name,
      native_factory,
    );
    let (tx, iced_runner) = runner.map_err(|error| anyhow!("cannot initialize iced runner: {error:?}"))?;

    #[cfg(feature = "public-cef")]
    let cef_enabled = self.cef_pump.is_some();
    #[cfg(not(feature = "public-cef"))]
    let cef_enabled = false;
    #[cfg(feature = "desktop")]
    let desktop = if self.desktop_enabled {
      let proxy = proxy.clone();
      Some(
        lintel_desktop::DesktopRuntime::new(move || {
          let _ = proxy.send_event(Action::Output(LintelEvent::Task(Box::new(|| {}))));
        })
        .context("cannot initialize native desktop services")?,
      )
    } else {
      None
    };
    #[cfg(all(feature = "desktop", target_os = "windows"))]
    if let (Some(hook), Some((runtime, _))) = (&menu_message_hook, &desktop) {
      hook.bind(runtime.windows_menu_dispatcher());
    }
    #[cfg(feature = "desktop")]
    let desktop_hotkeys = desktop.as_ref().map(|_| install_desktop_event_bridge(proxy.clone()));

    let store = Store::new(StoreConfig {
      cwd: project_dir.config_dir().to_path_buf(),
      name: "lintel-app-info".to_owned(),
      ..Default::default()
    });
    let exe = std::env::current_exe()?;
    let app_info = AppInfoBuilder::default()
      .app_path(exe.to_string_lossy().into_owned())
      .name(self.name)
      .exe_name(self.name.to_owned())
      .version(env!("CARGO_PKG_VERSION"))
      .app_info_store(store)
      .build_with_project_dirs(project_dir, Default::default());

    let mut plugins: Vec<PluginCreator> = vec![Box::new(move |context| lintel_plugin_iced::creator(context, tx))];
    plugins.extend(self.plugins);
    let app_context = AppContextBuilder::new()
      .with_event_proxy(proxy)
      .with_app_info(app_info)
      .with_process_info(ProcessInfo::new(process::id(), thread::current().id()))
      .with_runtime(runtime.handle().clone())
      .with_app_runtime(app_runtime.handle().clone())
      .with_plugins(plugins)
      .try_build()?;
    #[cfg(feature = "desktop")]
    let desktop = desktop.map(|(runtime, service)| {
      let service = std::sync::Arc::new(service);
      app_context.register_service(move || service.clone());
      runtime
    });
    #[cfg(feature = "desktop")]
    activation.attach(&app_context);
    #[cfg(feature = "multiprocess")]
    process_owner.register(&app_context);
    if let Err(error) = app_context.start_plugins() {
      runtime.block_on(app_context.shutdown());
      return Err(error);
    }
    #[cfg(feature = "public-cef")]
    if let Some(mut pump) = self.cef_pump {
      let proxy = app_context.event_proxy.clone();
      if let Err(error) = app_context.spawn_task(lintel_core::tasks::TaskScope::Application, async move {
        while let Some(message) = pump.recv().await {
          if let BrowserProcessMessage::Tick(time) = message {
            if proxy.send_event(Action::Output(LintelEvent::TickCef(time))).is_err() {
              break;
            }
            tokio::task::yield_now().await;
          }
        }
      }) {
        runtime.block_on(app_context.shutdown());
        return Err(error.into());
      }
    }

    let mut app = AppRunner {
      app_context,
      setup: StartupSetup::new(setup),
      exit_code: None,
      relaunch_args: None,
      activation_handler: self.activation_handler,
      activation_inbox: Default::default(),
      #[cfg(feature = "desktop")]
      activation,
      global_hotkey_rx: GlobalHotKeyEvent::receiver(),
      iced: Some(iced_runner),
      cef_enabled,
      cef_schedule: None,
      quit_started: false,
      last_event_loop_activity: None,
      #[cfg(feature = "desktop")]
      desktop,
      #[cfg(feature = "desktop")]
      desktop_hotkeys,
      #[cfg(feature = "desktop")]
      desktop_event_handler: self.desktop_event_handler,
    };
    let event_loop_result = event_loop.run_app(&mut app);
    let result = match app.setup.take_error() {
      Some(error) => Err(error),
      None => event_loop_result.map_err(Into::into),
    };
    let result = runtime.block_on(shutdown_preserving_result(result, app.app_context.shutdown()));
    #[cfg(feature = "multiprocess")]
    let process_result = runtime.block_on(process_owner.shutdown());
    result?;
    #[cfg(feature = "multiprocess")]
    process_result?;
    if let Some(error) = app.iced.as_mut().and_then(|runner| runner.take_error()) {
      return Err(anyhow!("iced runner failed: {error}"));
    }
    let exit_code = app.exit_code;
    let relaunch = app.relaunch_args.take();
    let env = app.app_context.app_info.env.clone();
    // 释放窗口和主线程原生服务后才创建替换进程。
    drop(app);
    if let Some(args) = relaunch {
      #[cfg(feature = "public-cef")]
      match self.deferred_relaunch {
        Some(deferred) => deferred.record(env, args)?,
        None => {
          lintel_core::utils::process::spawn_replacement(&env, args.as_deref())
            .context("cannot relaunch application after cleanup")?;
        }
      }
      #[cfg(not(feature = "public-cef"))]
      {
        lintel_core::utils::process::spawn_replacement(&env, args.as_deref())
          .context("cannot relaunch application after cleanup")?;
      }
    }
    if let Some(code) = exit_code.filter(|code| *code != 0) {
      anyhow::bail!("application exited with status {code}");
    }
    Ok(())
  }
}

/// 初始化成功后才向渲染器提交程序；失败时不执行可能创建窗口的 admission。
pub(crate) fn initialize_then<State>(
  initialize: impl FnOnce() -> Result<State>,
  admit: impl FnOnce(State) -> Result<()>,
) -> Result<()> {
  admit(initialize()?)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupStatus {
  Pending,
  Ready,
  Failed,
}
/// 仅运行一次，同时保留旧 setup 捕获值直至 runner 释放。
pub(super) struct StartupSetup<C> {
  handler: Box<dyn Fn(&C) -> Result<()> + Send + Sync>,
  status: SetupStatus,
  error: Option<anyhow::Error>,
}
impl<C> StartupSetup<C> {
  fn new(handler: impl Fn(&C) -> Result<()> + Send + Sync + 'static) -> Self {
    Self {
      handler: Box::new(handler),
      status: SetupStatus::Pending,
      error: None,
    }
  }
  fn run(&mut self, context: &C) -> bool {
    if self.status == SetupStatus::Pending {
      match (self.handler)(context) {
        Ok(()) => self.status = SetupStatus::Ready,
        Err(error) => {
          self.status = SetupStatus::Failed;
          self.error = Some(error);
        }
      }
    }
    self.status == SetupStatus::Ready
  }
  fn failed(&self) -> bool {
    self.status == SetupStatus::Failed
  }
  fn ready(&self) -> bool {
    self.status == SetupStatus::Ready
  }
  fn take_error(&mut self) -> Option<anyhow::Error> {
    self.error.take()
  }
}
async fn shutdown_preserving_result(result: Result<()>, shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
  shutdown.await;
  result
}

// UI/CEF 关闭由上面的协调流程完成。超时只限制 Tokio 等待不可取消阻塞任务的时间。
struct RuntimeOwner(Option<tokio::runtime::Runtime>);

impl RuntimeOwner {
  fn new(runtime: tokio::runtime::Runtime) -> Self {
    Self(Some(runtime))
  }
}

impl Deref for RuntimeOwner {
  type Target = tokio::runtime::Runtime;

  fn deref(&self) -> &Self::Target {
    self.0.as_ref().expect("runtime owner was already destroyed")
  }
}

impl Drop for RuntimeOwner {
  fn drop(&mut self) {
    if let Some(runtime) = self.0.take() {
      runtime.shutdown_timeout(Duration::from_secs(2));
    }
  }
}

#[cfg(feature = "desktop")]
fn install_desktop_event_bridge(
  proxy: winit::event_loop::EventLoopProxy<Action<LintelEvent>>,
) -> std::sync::mpsc::Receiver<GlobalHotKeyEvent> {
  #[cfg(any(target_os = "macos", target_os = "windows"))]
  {
    use lintel_desktop::native_ui::{muda::MenuEvent, tray_icon::TrayIconEvent};

    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
      let _ = menu_proxy.send_event(Action::Output(LintelEvent::MenuEvent(event)));
    }));
    let tray_proxy = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
      let _ = tray_proxy.send_event(Action::Output(LintelEvent::TrayIconEvent(event)));
    }));
  }
  let (sender, receiver) = std::sync::mpsc::sync_channel(128);
  GlobalHotKeyEvent::set_event_handler(Some(move |event| {
    if let Err(error) = sender.try_send(event) {
      warn!(?error, "native shortcut event queue is full or closed");
      return;
    }
    let _ = proxy.send_event(Action::Output(LintelEvent::Task(Box::new(|| {}))));
  }));
  receiver
}

pub(crate) fn is_application_id(id: &str) -> bool {
  fn is_label_edge(byte: &u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
  }

  id.len() <= 253
    && id.bytes().filter(|byte| *byte == b'.').count() >= 2
    && id.split('.').all(|label| {
      let bytes = label.as_bytes();
      bytes.len() <= 63
        && bytes.first().is_some_and(is_label_edge)
        && bytes.last().is_some_and(is_label_edge)
        && bytes.iter().all(|byte| is_label_edge(byte) || *byte == b'-')
    })
}

#[cfg(test)]
mod tests {
  use super::is_application_id;

  #[test]
  fn runtime_owner_bounds_an_already_running_blocking_task() {
    let runtime = super::RuntimeOwner::new(
      tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap(),
    );
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    runtime.spawn_blocking(move || {
      started_tx.send(()).unwrap();
      let _ = release_rx.recv_timeout(std::time::Duration::from_secs(10));
      let _ = done_tx.send(());
    });
    started_rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
    let before = std::time::Instant::now();
    drop(runtime);
    assert!(before.elapsed() < std::time::Duration::from_secs(5));
    assert!(
      done_rx.try_recv().is_err(),
      "timeout must not pretend to terminate synchronous work"
    );
    release_tx.send(()).unwrap();
    done_rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
  }

  #[derive(Debug)]
  struct ContextInitializationError(std::sync::Arc<()>);
  impl std::fmt::Display for ContextInitializationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      f.write_str("context initialization failed")
    }
  }
  impl std::error::Error for ContextInitializationError {}

  #[test]
  fn context_setup_is_once_and_failure_preserves_the_original_error_without_admission() {
    use std::sync::{
      Arc,
      atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let called = calls.clone();
    let marker = Arc::new(());
    let cause = marker.clone();
    let mut setup = super::StartupSetup::new(move |_: &()| {
      called.fetch_add(1, Ordering::SeqCst);
      Err(ContextInitializationError(cause.clone()).into())
    });
    assert!(!setup.ready());
    assert!(!setup.run(&()));
    assert!(!setup.run(&()));
    assert!(setup.failed());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let error = setup.take_error().unwrap();
    assert!(Arc::ptr_eq(
      &error.downcast_ref::<ContextInitializationError>().unwrap().0,
      &marker
    ));
    assert!(setup.failed());
    let admissions = std::cell::Cell::new(0);
    let error = super::initialize_then(
      || Err::<(), _>(error),
      |_| {
        admissions.set(admissions.get() + 1);
        Ok(())
      },
    )
    .unwrap_err();
    assert_eq!(admissions.get(), 0);
    assert!(Arc::ptr_eq(
      &error.downcast_ref::<ContextInitializationError>().unwrap().0,
      &marker
    ));
  }
  #[test]
  fn context_setup_success_reuses_started_services_and_keeps_captures_alive() {
    use std::sync::{
      Arc,
      atomic::{AtomicUsize, Ordering},
    };
    struct Guard(Arc<AtomicUsize>);
    impl Drop for Guard {
      fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
      }
    }
    let services = crate::core::context::store::ServiceStore::new();
    services.register_service(|| Arc::new(41usize));
    let drops = Arc::new(AtomicUsize::new(0));
    let guard = Guard(drops.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let mut setup = super::StartupSetup::new(move |context: &crate::core::context::store::ServiceStore| {
      let _guard = &guard;
      count.fetch_add(1, Ordering::SeqCst);
      super::initialize_then(
        || Ok(*context.get_service::<usize>().unwrap() + 1),
        |state| {
          assert_eq!(state, 42);
          Ok(())
        },
      )
    });
    assert!(setup.run(&services));
    assert!(setup.run(&services));
    assert!(setup.ready());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(setup);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
  }
  #[test]
  fn context_initialization_preserves_typed_program_admission_errors() {
    let error =
      super::initialize_then(|| Ok(42usize), |_| Err(crate::ProgramCommandError::QueueFull.into())).unwrap_err();
    assert!(matches!(
      error.downcast_ref::<crate::ProgramCommandError>(),
      Some(crate::ProgramCommandError::QueueFull)
    ));
  }
  #[tokio::test]
  async fn context_initialization_error_waits_for_tracked_task_and_service_cleanup() {
    use std::sync::{
      Arc,
      atomic::{AtomicUsize, Ordering},
    };
    struct Guard {
      stage: Arc<AtomicUsize>,
      value: usize,
    }
    impl Drop for Guard {
      fn drop(&mut self) {
        let previous = self.stage.swap(self.value, Ordering::SeqCst);
        assert_eq!(previous, self.value - 1);
      }
    }
    let stage = Arc::new(AtomicUsize::new(0));
    let tracker = crate::core::tasks::TaskTracker::new();
    let task_guard = Guard {
      stage: stage.clone(),
      value: 1,
    };
    let (started, ready) = tokio::sync::oneshot::channel();
    tracker
      .spawn(
        &tokio::runtime::Handle::current(),
        crate::core::tasks::TaskScope::Application,
        async move {
          let _guard = task_guard;
          let _ = started.send(());
          std::future::pending::<()>().await;
        },
      )
      .unwrap();
    ready.await.unwrap();
    let services = crate::core::context::store::ServiceStore::new();
    let service = Arc::new(Guard {
      stage: stage.clone(),
      value: 2,
    });
    services.register_service(move || service.clone());
    let marker = Arc::new(());
    let result = Err(ContextInitializationError(marker.clone()).into());
    let error = super::shutdown_preserving_result(result, async {
      tracker.shutdown();
      tracker.wait_idle().await;
      assert_eq!(stage.load(Ordering::SeqCst), 1);
      services.clear();
    })
    .await
    .unwrap_err();
    assert_eq!(stage.load(Ordering::SeqCst), 2);
    assert!(Arc::ptr_eq(
      &error.downcast_ref::<ContextInitializationError>().unwrap().0,
      &marker
    ));
    assert!(tracker.is_shutting_down());
  }
  #[test]
  fn application_ids_are_safe_directory_names() {
    assert!(is_application_id("dev.example.my-counter"));
    for invalid in [
      "",
      "example",
      "dev..counter",
      "dev.example.-counter",
      "../dev.example.counter",
      "dev.example/a",
    ] {
      assert!(
        !is_application_id(invalid),
        "accepted invalid application ID {invalid:?}"
      );
    }
  }
}

/// 沿用原运行时的 Init → Waiting → Done 退出流程。
pub(super) struct AppRuntime {
  ctx: AppContext,
}

impl AppRuntime {
  pub(super) fn new(ctx: AppContext) -> Self {
    Self { ctx }
  }

  pub(super) fn handle_app_quit(&self, event: AppEvent, event_loop: &ActiveEventLoop) {
    assert!(self.ctx.is_in_app_main_thread(), "必须在主线程调用");
    match event {
      AppEvent::Quit(QuitState::Init) | AppEvent::Exit(_, QuitState::Init) | AppEvent::Relaunch(_, QuitState::Init) => {
        #[cfg(target_os = "macos")]
        {
          use winit::platform::macos::ActiveEventLoopExtMacOS;
          event_loop.hide_application();
        }
        info!(?event, "start application quit");
        let waiting = match event {
          AppEvent::Exit(code, _) => AppEvent::Exit(code, QuitState::Waiting),
          AppEvent::Relaunch(ref args, _) => AppEvent::Relaunch(args.clone(), QuitState::Waiting),
          _ => AppEvent::Quit(QuitState::Waiting),
        };
        let _ = self.ctx.send_app_event(waiting);
        self.cleanup_before_exit(event);
      }
      AppEvent::Quit(QuitState::Waiting)
      | AppEvent::Exit(_, QuitState::Waiting)
      | AppEvent::Relaunch(_, QuitState::Waiting) => {
        info!("waiting for plugin and resource cleanup");
      }
      AppEvent::Quit(QuitState::Done) | AppEvent::Exit(_, QuitState::Done) | AppEvent::Relaunch(_, QuitState::Done) => {
        info!("application cleanup finished");
        event_loop.exit();
      }
      _ => {}
    }
  }

  fn cleanup_before_exit(&self, event: AppEvent) {
    let ctx = self.ctx.clone();
    self.ctx.app_runtime.spawn(async move {
      ctx.shutdown().await;
      let done = match event {
        AppEvent::Exit(code, _) => AppEvent::Exit(code, QuitState::Done),
        AppEvent::Relaunch(args, _) => AppEvent::Relaunch(args, QuitState::Done),
        _ => AppEvent::Quit(QuitState::Done),
      };
      if let Err(error) = ctx.send_app_event(done) {
        warn!(?error, "event loop closed before application cleanup finished");
      }
    });
  }
}
