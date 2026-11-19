use std::{
  collections::{BTreeMap, BTreeSet},
  future::Future,
  sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
  },
};

use anyhow::{Context, ensure};
use serde::{Serialize, de::DeserializeOwned};
use url::Url;

use crate::{
  IcedAppPluginApi, Result, cef,
  core::{
    context::{AppContext, AppContextApi},
    event_loop::event::{AppEvent, LintelEvent},
    tasks::TaskScope,
  },
  iced::{
    self, AnyMessage, Element, IcedAppEvent, IcedApplication, ProgramId, ProgramIdOfMessage, Subscription, Task, window,
  },
  runtime::{AppBuilder, PluginCreator},
  webview::{CefAction, CefBrowser, CefMessage, LocalPagePolicy, RequestEvent, WebviewConfig, WebviewRenderMode},
  webview_bridge::{BridgePagePolicy, BridgeSession, Commands, WebviewCommandContext},
};

/// Builds an application serving trusted, embedded HTML in one or more native windows.
pub fn webview_app() -> WebviewApplication {
  WebviewApplication {
    id: None,
    title: "Lintel WebView",
    page: None,
    commands: Commands::default(),
    windows: Vec::new(),
    plugins: Vec::new(),
    #[cfg(feature = "multiprocess")]
    processes: None,
    setup: None,
    on_event: None,
    exit_on_last_window: true,
    permission_labels: BTreeSet::new(),
    activation: None,
    cef_reports: None,
    #[cfg(feature = "crash-reporting")]
    crash: None,
    #[cfg(feature = "desktop")]
    desktop: false,
    #[cfg(feature = "desktop")]
    activation_options: Default::default(),
    #[cfg(feature = "desktop")]
    desktop_event: None,
  }
}

type SetupCallback = Box<dyn Fn(&AppContext, WebviewWindows) -> Result<()> + Send + Sync>;
enum Setup {
  BeforeAdmission(SetupCallback),
  AfterAdmission(SetupCallback),
}
type EventCallback = Arc<dyn Fn(&str, WebviewWindowEvent) + Send + Sync>;
type ScreenshotResult = std::result::Result<window::Screenshot, String>;
type ScreenshotReply = Arc<Mutex<Option<tokio::sync::oneshot::Sender<ScreenshotResult>>>>;
#[cfg(debug_assertions)]
type RendererFaultReply = Arc<Mutex<Option<tokio::sync::oneshot::Sender<std::result::Result<i32, String>>>>>;

struct Page {
  label: String,
  title: &'static str,
  html: &'static str,
}

/// Configuration for local WebView windows sharing one CEF runtime.
pub struct WebviewApplication {
  id: Option<String>,
  title: &'static str,
  page: Option<&'static str>,
  commands: Commands,
  windows: Vec<Page>,
  plugins: Vec<PluginCreator>,
  #[cfg(feature = "multiprocess")]
  processes: Option<crate::process::HostConfig>,
  setup: Option<Setup>,
  on_event: Option<EventCallback>,
  exit_on_last_window: bool,
  permission_labels: BTreeSet<String>,
  activation: Option<Arc<dyn Fn(&AppContext, crate::runtime::ActivationEvent) + Send + Sync>>,
  cef_reports: Option<cef::CefCrashReports>,
  #[cfg(feature = "crash-reporting")]
  crash: Option<lintel_crash::CrashConfig>,
  #[cfg(feature = "desktop")]
  desktop: bool,
  #[cfg(feature = "desktop")]
  activation_options: crate::activation::ActivationOptions,
  #[cfg(feature = "desktop")]
  desktop_event: Option<Arc<dyn Fn(&AppContext, lintel_desktop::DesktopEvent) + Send + Sync>>,
}

impl WebviewApplication {
  pub fn id(mut self, id: impl Into<String>) -> Self {
    self.id = Some(id.into());
    self
  }
  pub fn title(mut self, title: &'static str) -> Self {
    self.title = title;
    self
  }

  /// Opens the default window labeled main with this embedded HTML document.
  pub fn local_page(mut self, html: &'static str) -> Self {
    self.page = Some(html);
    self
  }

  /// Adds an independently routed local page. Labels must be unique safe path segments.
  /// Every page receives its own directory, policy, bridge session, and native window.
  pub fn window(mut self, label: impl Into<String>, title: &'static str, html: &'static str) -> Self {
    self.windows.push(Page {
      label: label.into(),
      title,
      html,
    });
    self
  }

  /// 装配独立业务服务；CEF 子进程继续由 CEF 管理。
  #[cfg(feature = "multiprocess")]
  pub fn processes(mut self, config: crate::process::HostConfig) -> Self {
    self.processes = Some(config);
    self
  }

  pub fn plugin(mut self, creator: impl FnOnce(&mut AppContext) + Send + Sync + 'static) -> Self {
    self.plugins.push(Box::new(creator));
    self
  }

  /// Enables CEF's local Crashpad backend using the packaged static configuration.
  /// This is exclusive with `.crash_reporting` and requires an app-owned private root.
  pub fn cef_crash_reports(mut self, directory: impl Into<std::path::PathBuf>) -> Self {
    self.cef_reports = Some(cef::CefCrashReports::new(directory));
    self
  }

  /// Covers browser-process native startup, UI, and CEF shutdown with local minidumps.
  #[cfg(feature = "crash-reporting")]
  pub fn crash_reporting(mut self, config: lintel_crash::CrashConfig) -> Self {
    self.crash = Some(config);
    self
  }

  /// Supplies a handle for closing individual windows or gracefully quitting all of them.
  pub fn setup(mut self, setup: impl Fn(&AppContext, WebviewWindows) + Send + Sync + 'static) -> Self {
    self.setup = Some(Setup::AfterAdmission(Box::new(move |context, windows| {
      setup(context, windows);
      Ok(())
    })));
    self
  }

  /// 在 browser 进程、插件和服务启动后，默认窗口提交前初始化。
  /// 失败撤销本次 Init 中尚未启动的窗口预约，经原有退出路径返回原错误。
  pub fn try_setup(
    mut self,
    setup: impl Fn(&AppContext, WebviewWindows) -> Result<()> + Send + Sync + 'static,
  ) -> Self {
    self.setup = Some(Setup::BeforeAdmission(Box::new(setup)));
    self
  }

  pub fn on_window_event(mut self, callback: impl Fn(&str, WebviewWindowEvent) + Send + Sync + 'static) -> Self {
    self.on_event = Some(Arc::new(callback));
    self
  }

  pub fn exit_on_last_window(mut self, exit: bool) -> Self {
    self.exit_on_last_window = exit;
    self
  }

  /// Forwards secondary launches before GUI/browser startup and keeps one primary process.
  #[cfg(feature = "desktop")]
  pub fn single_instance(mut self) -> Self {
    self.activation_options.single_instance = true;
    self
  }

  /// Allows these protocol schemes and extracts matching URLs from startup arguments.
  #[cfg(feature = "desktop")]
  pub fn protocol_schemes(mut self, schemes: impl IntoIterator<Item = impl Into<String>>) -> Self {
    self.activation_options.protocol_schemes = Some(schemes.into_iter().map(Into::into).collect());
    self
  }

  #[cfg(feature = "desktop")]
  pub fn desktop(mut self) -> Self {
    self.desktop = true;
    self
  }

  #[cfg(feature = "desktop")]
  pub fn on_desktop_event(
    mut self,
    callback: impl Fn(&AppContext, lintel_desktop::DesktopEvent) + Send + Sync + 'static,
  ) -> Self {
    self.desktop = true;
    self.desktop_event = Some(Arc::new(callback));
    self
  }

  pub fn on_activation(
    mut self,
    callback: impl Fn(&AppContext, crate::runtime::ActivationEvent) + Send + Sync + 'static,
  ) -> Self {
    self.activation = Some(Arc::new(callback));
    self
  }

  pub fn command<Req, Res, F>(mut self, name: &str, handler: F) -> Self
  where
    Req: DeserializeOwned + 'static,
    Res: Serialize + 'static,
    F: Fn(Req) -> Result<Res> + Send + Sync + 'static,
  {
    self.commands.add(name, handler);
    self
  }

  /// 通过来源窗口的弱上下文取得服务，不需要全局变量或捕获 AppContext。
  pub fn command_with_context<Req, Res, F>(mut self, name: &str, handler: F) -> Self
  where
    Req: DeserializeOwned + 'static,
    Res: Serialize + 'static,
    F: Fn(WebviewCommandContext, Req) -> Result<Res> + Send + Sync + 'static,
  {
    self.commands.add_with_context(name, handler);
    self
  }

  /// Applies an explicit host-created command identity and its capability registry.
  pub fn permissions(
    mut self,
    principal: crate::permissions::Principal,
    capabilities: crate::permissions::Capabilities,
  ) -> Self {
    self.commands.set_permissions(principal, capabilities);
    self
  }

  /// Assigns command grants to one configured page using a host-created identity.
  pub fn window_permissions(
    mut self,
    label: impl Into<String>,
    principal: crate::permissions::Principal,
    capabilities: crate::permissions::Capabilities,
  ) -> Self {
    let label = label.into();
    self.permission_labels.insert(label.clone());
    self.commands.set_window_permissions(label, principal, capabilities);
    self
  }

  /// Executes a typed command asynchronously within its originating window scope.
  pub fn async_command<Req, Res, F, Fut>(mut self, name: &str, handler: F) -> Self
  where
    Req: DeserializeOwned + Send + 'static,
    Res: Serialize + Send + 'static,
    F: Fn(Req) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Res>> + Send + 'static,
  {
    self.commands.add_async(name, handler);
    self
  }

  /// 在既有窗口任务域内执行带上下文的命令，保留权限、取消与期限控制。
  pub fn async_command_with_context<Req, Res, F, Fut>(mut self, name: &str, handler: F) -> Self
  where
    Req: DeserializeOwned + Send + 'static,
    Res: Serialize + Send + 'static,
    F: Fn(WebviewCommandContext, Req) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Res>> + Send + 'static,
  {
    self.commands.add_async_with_context(name, handler);
    self
  }

  /// 配置受管命令执行期限，在 CEF 启动前校验。
  /// JS timeoutMs 只控制本地等待，不能延长该宿主期限。
  pub fn command_timeout(mut self, timeout: std::time::Duration) -> Self {
    self.commands.set_timeout(timeout);
    self
  }

  /// Starts a shared browser runtime or handles a CEF helper process.
  pub fn run(mut self, version: &'static str) -> Result<()> {
    #[cfg(feature = "crash-reporting")]
    if lintel_crash::helper_entry()? {
      return Ok(());
    }
    if crate::activation::is_cef_helper_process() {
      cef::run(version, |_, _| {
        anyhow::bail!("CEF helper unexpectedly entered the browser application callback")
      })?;
      return Ok(());
    }
    #[cfg(feature = "desktop")]
    let activation = {
      let project = crate::activation::project_dirs(self.title, self.id.as_deref())?;
      match crate::activation::prepare(&project, self.activation_options)? {
        crate::activation::PreparedActivation::Forwarded => return Ok(()),
        crate::activation::PreparedActivation::Primary(activation) => activation,
      }
    };
    #[cfg(feature = "desktop")]
    let _primary_until_cef_shutdown = activation.retain_primary();
    if let Some(html) = self.page {
      self.windows.insert(
        0,
        Page {
          label: "main".into(),
          title: self.title,
          html,
        },
      );
    }
    validate_pages(&self.windows)?;
    for label in &self.permission_labels {
      ensure!(
        self.windows.iter().any(|page| &page.label == label),
        "permissions refer to unknown WebView window {label:?}"
      );
    }
    self.commands.validate()?;
    ensure!(
      lintel_lib_env::webview_test_url_override().is_none(),
      "clear the imported WebView test URL override to run a local WebView application"
    );
    #[cfg(feature = "crash-reporting")]
    let native_reporter = self.crash.is_some();
    #[cfg(not(feature = "crash-reporting"))]
    let native_reporter = false;
    cef::validate_crash_reports(self.cef_reports.as_ref(), native_reporter)?;
    let cef_reports = self
      .cef_reports
      .take()
      .map(|reports| reports.with_application_id(self.id.clone().unwrap_or_else(|| "lintel".into())));
    #[cfg(feature = "crash-reporting")]
    let _crash_reporter = self.crash.take().map(lintel_crash::CrashReporter::start).transpose()?;
    let commands = Arc::new(self.commands);
    let relaunch = crate::runtime::DeferredRelaunch::default();
    let deferred_relaunch = relaunch.clone();
    cef::run_configured(version, cef_reports, move |profile, pump| {
      crate::webview_bridge::init_logging();
      let resources = Arc::new(WebviewResources {
        commands: commands.clone(),
        on_event: self.on_event.clone(),
      });
      let mut blueprints = BTreeMap::new();
      for page in self.windows {
        let directory = profile.join("windows").join(&page.label);
        std::fs::create_dir_all(&directory).context("cannot create the local WebView directory")?;
        let path = directory.join("index.html");
        std::fs::write(&path, page.html).context("cannot write the embedded WebView page")?;
        std::fs::write(directory.join("lintel.js"), include_str!("webview_bridge.js"))
          .context("cannot write the WebView bridge script")?;
        let url =
          Url::from_file_path(&path).map_err(|()| anyhow::anyhow!("cannot convert the WebView page to a file URL"))?;
        let policy = Arc::new(LocalPagePolicy::new(url.clone()).with_asset(url.join("lintel.js")?));
        blueprints.insert(
          page.label.clone(),
          Blueprint {
            title: page.title,
            url,
            policy,
            resources: Arc::downgrade(&resources),
          },
        );
      }
      let labels: Arc<BTreeSet<String>> = Arc::new(blueprints.keys().cloned().collect());
      let group = Arc::new(WindowGroup::new(blueprints, self.exit_on_last_window));
      let mut app = AppBuilder::new(self.title)
        .with_deferred_relaunch(deferred_relaunch)
        .with_cef_pump(pump)
        .with_plugins(self.plugins);
      #[cfg(feature = "multiprocess")]
      if let Some(config) = self.processes {
        app = app.processes(config);
      }
      if let Some(id) = self.id {
        app = app.with_app_id(id);
      }
      if let Some(callback) = self.activation {
        app = app.with_activation_handler(move |context, event| callback(context, event));
      }
      #[cfg(feature = "desktop")]
      {
        app = app.with_prepared_activation(activation);
        if self.desktop {
          app = app.with_desktop();
        }
        if let Some(callback) = self.desktop_event {
          app = app.with_desktop_event_handler(move |context, event| callback(context, event));
        }
      }
      let result = app.try_run(move |context| {
        let _plugin = context
          .get_service::<dyn IcedAppPluginApi>()
          .context("iced plugin service is unavailable")?;
        let controls = WebviewWindows {
          context: context.clone(),
          labels: labels.clone(),
          group: group.clone(),
        };
        let quit_controls = controls.clone();
        let coordinator = Arc::new(crate::runtime::QuitCoordinator::new(move |_, event| {
          if quit_controls.group.remaining() == 0 {
            return Ok(crate::runtime::QuitDecision::Proceed);
          }
          quit_controls
            .group
            .pending_quit
            .lock()
            .unwrap()
            .get_or_insert_with(|| event.clone());
          quit_controls.quit()?;
          Ok(crate::runtime::QuitDecision::Defer)
        }));
        context.register_service(move || coordinator.clone());
        if let Some(Setup::BeforeAdmission(setup)) = &self.setup {
          if let Err(error) = setup(context, controls.clone()) {
            return Err(controls.group.rollback_startup_error(error));
          }
        }
        for label in controls.labels() {
          if controls.group.quit_requested.load(Ordering::Acquire) {
            break;
          }
          // try_setup 可显式预约某个已批准窗口；默认提交不能重复该预约。
          if controls.instance_id(label).is_some() {
            continue;
          }
          if let Err(error) = controls.reopen(label) {
            tracing::error!(%error, label, "cannot admit configured WebView window");
            if let Some(callback) = &self.on_event {
              callback(
                label,
                WebviewWindowEvent::OpenFailed {
                  description: error.to_string(),
                },
              );
            }
            return Err(controls.group.rollback_startup_error(error));
          }
        }
        if let Some(Setup::AfterAdmission(setup)) = &self.setup {
          setup(context, controls)?;
        }
        Ok(())
      });
      // The blueprints retain only weak resources, preventing callbacks that capture
      // a WebviewWindows handle from forming a permanent registry/command cycle.
      drop(resources);
      result
    })?;
    #[cfg(feature = "desktop")]
    drop(_primary_until_cef_shutdown);
    relaunch.run()?;
    Ok(())
  }
}

fn validate_pages(pages: &[Page]) -> Result<()> {
  ensure!(
    !pages.is_empty(),
    "set a nonempty HTML document with .local_page(...) or .window(...)"
  );
  let mut labels = BTreeSet::new();
  for page in pages {
    ensure!(!page.html.trim().is_empty(), "WebView page {:?} is empty", page.label);
    ensure!(
      !page.label.is_empty()
        && page.label.len() <= 64
        && page
          .label
          .bytes()
          .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)),
      "invalid WebView window label {:?}",
      page.label
    );
    ensure!(
      labels.insert(page.label.as_str()),
      "duplicate WebView window label {:?}",
      page.label
    );
  }
  Ok(())
}

/// Events from one local WebView, identified by the label passed to the callback.
#[derive(Clone, Debug)]
pub enum WebviewWindowEvent {
  Opened(window::Id),
  OpenFailed {
    description: String,
  },
  PageLoaded,
  LoadFailed {
    error_code: i32,
    description: Option<String>,
  },
  Closed,
  RendererUnresponsive,
  RendererResponsive,
  RendererTerminated {
    status: String,
    error_code: i32,
    description: Option<String>,
  },
}

struct WebviewResources {
  commands: Arc<Commands>,
  on_event: Option<EventCallback>,
}

#[derive(Clone)]
struct Blueprint {
  title: &'static str,
  url: Url,
  policy: Arc<LocalPagePolicy>,
  resources: Weak<WebviewResources>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Instance {
  program_id: ProgramId,
  scope_id: u64,
  close_requested: bool,
  closed: bool,
  browser_ready: bool,
  native_window: Option<window::Id>,
}

struct ConfiguredWindow {
  blueprint: Blueprint,
  instance: Option<Instance>,
}

struct WindowGroup {
  registry: Mutex<BTreeMap<String, ConfiguredWindow>>,
  quit_requested: AtomicBool,
  exit_on_last_window: bool,
  pending_quit: Mutex<Option<AppEvent>>,
}

impl WindowGroup {
  fn new(blueprints: BTreeMap<String, Blueprint>, exit_on_last_window: bool) -> Self {
    Self {
      registry: Mutex::new(
        blueprints
          .into_iter()
          .map(|(label, blueprint)| {
            (
              label,
              ConfiguredWindow {
                blueprint,
                instance: None,
              },
            )
          })
          .collect(),
      ),
      quit_requested: AtomicBool::new(false),
      exit_on_last_window,
      pending_quit: Mutex::new(None),
    }
  }

  fn remaining(&self) -> usize {
    self
      .registry
      .lock()
      .unwrap()
      .values()
      .filter(|entry| entry.instance.is_some_and(|instance| !instance.closed))
      .count()
  }

  /// 只用于事件循环首次 Init 回调内，此时排队的程序尚未执行 boot。
  /// 撤销的是窗口预约而非已创建浏览器；队列 owner 在原有核心清理中释放。
  fn rollback_startup(&self) -> Result<()> {
    let mut registry = self.registry.lock().unwrap();
    ensure!(
      registry.values().all(|entry| entry
        .instance
        .is_none_or(|instance| instance.native_window.is_none() && !instance.browser_ready)),
      "cannot roll back started WebView windows"
    );
    self.quit_requested.store(true, Ordering::Release);
    for entry in registry.values_mut() {
      entry.instance = None;
    }
    Ok(())
  }

  fn rollback_startup_error(&self, error: anyhow::Error) -> anyhow::Error {
    match self.rollback_startup() {
      Ok(()) => error,
      // Init 的单线程边界使此分支不应发生；保留原错误类型且明确暴露破坏的前提。
      Err(rollback) => error.context(format!("WebView startup reservation rollback failed: {rollback}")),
    }
  }

  fn snapshot(&self, label: &str) -> Result<Instance> {
    self
      .registry
      .lock()
      .unwrap()
      .get(label)
      .context("unknown WebView window label")?
      .instance
      .filter(|instance| !instance.closed)
      .context("WebView window is closed")
  }

  fn current(&self, label: &str, id: ProgramId) -> bool {
    self
      .registry
      .lock()
      .unwrap()
      .get(label)
      .and_then(|entry| entry.instance)
      .is_some_and(|instance| instance.program_id == id)
  }

  fn change(&self, label: &str, id: ProgramId, change: impl FnOnce(&mut Instance)) -> bool {
    let mut registry = self.registry.lock().unwrap();
    if let Some(instance) = registry.get_mut(label).and_then(|entry| entry.instance.as_mut())
      && instance.program_id == id
      && !instance.closed
    {
      change(instance);
      true
    } else {
      false
    }
  }

  fn admit_reopen(&self, label: &str, dispatch: impl FnOnce(&Blueprint, Instance) -> Result<()>) -> Result<ProgramId> {
    let mut registry = self.registry.lock().unwrap();
    ensure!(
      !self.quit_requested.load(Ordering::Acquire),
      "application shutdown is already requested"
    );
    let entry = registry.get_mut(label).context("unknown WebView window label")?;
    ensure!(
      entry.instance.is_none_or(|instance| instance.closed),
      "WebView window is already open or closing"
    );
    let previous = entry.instance;
    let scope_id = crate::window::new_scope_id();
    let instance = Instance {
      program_id: ProgramId::new("lintel::local-webview-window", scope_id),
      scope_id,
      close_requested: false,
      closed: false,
      browser_ready: false,
      native_window: None,
    };
    entry.instance = Some(instance);
    if let Err(error) = dispatch(&entry.blueprint, instance) {
      entry.instance = previous;
      return Err(error);
    }
    Ok(instance.program_id)
  }

  fn admit_close(&self, label: &str, dispatch: impl FnOnce(ProgramId) -> Result<()>) -> Result<()> {
    let mut registry = self.registry.lock().unwrap();
    let instance = registry
      .get_mut(label)
      .context("unknown WebView window label")?
      .instance
      .as_mut()
      .filter(|instance| !instance.closed)
      .context("WebView window is closed")?;
    if instance.close_requested {
      return Ok(());
    }
    dispatch(instance.program_id)?;
    instance.close_requested = true;
    Ok(())
  }

  /// The identity check prevents an old close callback from completing a replacement.
  fn complete(&self, label: &str, id: ProgramId) -> Option<bool> {
    let mut registry = self.registry.lock().unwrap();
    let instance = registry.get_mut(label)?.instance.as_mut()?;
    if instance.program_id != id || instance.closed {
      return None;
    }
    instance.closed = true;
    instance.native_window = None;
    Some(
      !registry
        .values()
        .any(|entry| entry.instance.is_some_and(|instance| !instance.closed))
        && (self.exit_on_last_window || self.quit_requested.load(Ordering::Acquire)),
    )
  }
}

/// Thread-safe controls for approved configured pages, sharing the application's CEF runtime.
#[derive(Clone)]
pub struct WebviewWindows {
  context: AppContext,
  labels: Arc<BTreeSet<String>>,
  group: Arc<WindowGroup>,
}

impl WebviewWindows {
  pub fn labels(&self) -> impl Iterator<Item = &str> {
    self.labels.iter().map(String::as_str)
  }

  /// Returns the live instance identity, which changes whenever a label is reopened.
  pub fn instance_id(&self, label: &str) -> Option<ProgramId> {
    self.group.snapshot(label).ok().map(|instance| instance.program_id)
  }

  /// Opens a fresh instance of one approved configured page. The old instance must have
  /// completely closed. A rejected bounded admission restores the previous registry state.
  pub fn reopen(&self, label: &str) -> Result<ProgramId> {
    let plugin = self
      .context
      .get_service::<dyn IcedAppPluginApi>()
      .context("iced plugin service is unavailable")?;
    self.group.admit_reopen(label, |blueprint, instance| {
      let resources = blueprint.resources.upgrade().context("WebView host has stopped")?;
      let mut browser = CefBrowser::new();
      // Commands/BridgeSession 已拥有期限、取消和一次性回复，不能再用 widget 的
      // legacy 15 秒计时器提前结束合法命令。该策略在 browser 创建前固定。
      browser.set_managed_command_responses()?;
      let session = BridgeSession::new();
      let policy = Arc::new(BridgePagePolicy::new(blueprint.policy.clone(), &session));
      plugin
        .try_create(Box::new(WebviewWindow {
          context: self.context.clone(),
          program_id: instance.program_id,
          scope_id: instance.scope_id,
          label: label.to_owned(),
          title: blueprint.title,
          window: None,
          browser,
          url: blueprint.url.clone(),
          policy,
          commands: resources.commands.clone(),
          session,
          state: BrowserState::WaitingForWindow,
          group: self.group.clone(),
          on_event: resources.on_event.clone(),
          finished: false,
        }))
        .map_err(Into::into)
    })
  }

  pub fn close(&self, label: &str) -> Result<()> {
    self.group.admit_close(label, |id| {
      crate::dispatch_program_message(&self.context, program_message(id, Message::Close)).map_err(Into::into)
    })
  }

  /// Reloads the same approved page and invalidates obsolete bridge requests.
  pub fn reload(&self, label: &str) -> Result<()> {
    let instance = self.group.snapshot(label)?;
    ensure!(
      instance.browser_ready && !instance.close_requested,
      "WebView browser is not ready to reload"
    );
    crate::dispatch_program_message(&self.context, program_message(instance.program_id, Message::Reload))?;
    Ok(())
  }

  /// Debug-only fault injection using Chromium's Page.crash command. It terminates
  /// the selected renderer process, never the host. Observe RendererTerminated to
  /// confirm the real fault; a successful return only confirms command admission.
  #[cfg(debug_assertions)]
  pub async fn crash_renderer_for_testing(&self, label: &str) -> Result<i32> {
    let instance = self.group.snapshot(label)?;
    ensure!(
      instance.browser_ready && !instance.close_requested,
      "WebView renderer is not ready for fault injection"
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    crate::dispatch_program_message(
      &self.context,
      program_message(
        instance.program_id,
        Message::CrashRendererForTesting(Arc::new(Mutex::new(Some(tx)))),
      ),
    )?;
    tokio::time::timeout(std::time::Duration::from_secs(5), rx)
      .await
      .context("renderer fault injection was not acknowledged")?
      .context("renderer fault injection was canceled")?
      .map_err(anyhow::Error::msg)
  }

  /// Captures pixels only from the instance that was current when this call began.
  pub async fn screenshot(&self, label: &str) -> Result<window::Screenshot> {
    let instance = self.group.snapshot(label)?;
    ensure!(
      instance.native_window.is_some() && !instance.close_requested,
      "WebView window is not ready to capture"
    );
    let (tx, rx) = tokio::sync::oneshot::channel();
    crate::dispatch_program_message(
      &self.context,
      program_message(instance.program_id, Message::Capture(Arc::new(Mutex::new(Some(tx))))),
    )?;
    tokio::time::timeout(std::time::Duration::from_secs(5), rx)
      .await
      .context("WebView screenshot timed out")?
      .context("WebView screenshot was canceled")?
      .map_err(anyhow::Error::msg)
  }

  /// Closes all live instances before requesting application and plugin shutdown.
  pub fn quit(&self) -> Result<()> {
    self.group.quit_requested.store(true, Ordering::Release);
    if self.group.remaining() == 0 {
      return self.context.quit();
    }
    for label in self.labels() {
      if self.group.snapshot(label).is_ok() {
        self.close(label)?;
      }
    }
    Ok(())
  }
}

#[derive(Debug)]
enum Message {
  Opened(window::Id),
  Cef(CefMessage),
  Close,
  Reload,
  CloseRequested(window::Id),
  NativeClosed(window::Id),
  Capture(ScreenshotReply),
  Captured,
  #[cfg(debug_assertions)]
  CrashRendererForTesting(RendererFaultReply),
}

#[derive(Debug)]
struct Routed {
  id: ProgramId,
  message: Message,
}
impl ProgramIdOfMessage for Routed {
  fn program_id(&self) -> ProgramId {
    self.id
  }
}
fn program_message(id: ProgramId, message: Message) -> Arc<dyn AnyMessage> {
  Arc::new(Routed { id, message })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BrowserState {
  WaitingForWindow,
  Creating,
  CloseAfterCreate,
  Open,
  Closing,
  Closed,
}

struct WebviewWindow {
  context: AppContext,
  program_id: ProgramId,
  scope_id: u64,
  label: String,
  title: &'static str,
  window: Option<window::Id>,
  browser: CefBrowser,
  url: Url,
  policy: Arc<BridgePagePolicy>,
  commands: Arc<Commands>,
  session: Arc<BridgeSession>,
  state: BrowserState,
  group: Arc<WindowGroup>,
  on_event: Option<EventCallback>,
  finished: bool,
}

impl WebviewWindow {
  fn notify(&self, event: WebviewWindowEvent) {
    if !self.group.current(&self.label, self.program_id)
      || (!matches!(&event, WebviewWindowEvent::Closed) && self.group.snapshot(&self.label).is_err())
    {
      return;
    }
    if let Some(callback) = &self.on_event {
      callback(&self.label, event);
    }
  }

  fn finish(&mut self) {
    if self.finished || self.window.is_some() || self.state != BrowserState::Closed {
      return;
    }
    self.finished = true;
    self.session.close();
    self.context.cancel_tasks(&TaskScope::Window(self.scope_id));
    let completion = self.group.complete(&self.label, self.program_id);
    if completion.is_some() {
      self.notify(WebviewWindowEvent::Closed);
    }
    // The native window and CEF browser are gone. Retire this exact program, so
    // repeated reopen does not retain its subscriptions, callbacks, or browser state.
    if let Err(error) = self
      .context
      .send_user_event(LintelEvent::IcedApp(IcedAppEvent::CloseProgram(self.program_id)))
    {
      tracing::debug!(?error, "renderer already stopped while retiring WebView program");
    }
    if completion == Some(true) && self.group.remaining() == 0 {
      let result = match self.group.pending_quit.lock().unwrap().take() {
        Some(event) => self.context.send_app_event(event),
        None => self.context.quit(),
      };
      if let Err(error) = result {
        tracing::error!(%error, "cannot request event-loop exit");
      }
    }
  }

  fn request_close(&mut self) -> Task<Message> {
    self
      .group
      .change(&self.label, self.program_id, |instance| instance.close_requested = true);
    self.session.close();
    self.context.cancel_tasks(&TaskScope::Window(self.scope_id));
    match self.state {
      BrowserState::WaitingForWindow => {
        self.state = BrowserState::Closed;
        return self.close_window();
      }
      BrowserState::Creating => self.state = BrowserState::CloseAfterCreate,
      BrowserState::Open => {
        if let Some(browser) = self.browser.browser_id() {
          match cef::close_browser(browser) {
            Ok(()) => self.state = BrowserState::Closing,
            Err(error) => tracing::error!(%error, "cannot request CEF browser close"),
          }
        }
      }
      BrowserState::CloseAfterCreate | BrowserState::Closing | BrowserState::Closed => {}
    }
    Task::none()
  }

  fn close_window(&mut self) -> Task<Message> {
    if let Some(window) = self.window {
      window::close(window)
    } else {
      self.finish();
      Task::none()
    }
  }

  fn update_browser(&mut self, message: CefMessage) -> Task<Message> {
    let (action, task) = self.browser.update(message).split(Message::Cef);
    if self.state == BrowserState::Open {
      self.session.observe_browser_action(&action);
    }
    match action {
      CefAction::Created(browser, js) => {
        tracing::info!(?browser, label = self.label, "standalone CEF browser created");
        let close_requested = self.state == BrowserState::CloseAfterCreate;
        self.state = BrowserState::Open;
        self
          .group
          .change(&self.label, self.program_id, |instance| instance.browser_ready = true);
        if close_requested {
          return task.chain(self.request_close());
        }
        let handler =
          self
            .commands
            .handler_for_window(self.context.clone(), self.scope_id, self.session.clone(), &self.label);
        return Task::batch([task, js.spawn_handler(move |_| handler)]);
      }
      CefAction::Loaded(browser) => {
        tracing::info!(?browser, label = self.label, "standalone CEF page loaded");
        self.notify(WebviewWindowEvent::PageLoaded);
      }
      CefAction::LoadError {
        error_code,
        error_text,
        is_main_frame: true,
        ..
      } => {
        tracing::error!(
          error_code,
          ?error_text,
          label = self.label,
          "standalone CEF page failed to load"
        );
        self.notify(WebviewWindowEvent::LoadFailed {
          error_code,
          description: error_text,
        });
      }
      CefAction::Closed(browser) => {
        tracing::info!(
          ?browser,
          label = self.label,
          "standalone CEF browser close callback received"
        );
        self.state = BrowserState::Closed;
        return task.chain(self.close_window());
      }
      CefAction::Request(request) if self.state == BrowserState::Open => match request {
        RequestEvent::RenderProcessUnresponsive { browser_id } => {
          tracing::warn!(?browser_id, label = self.label, "WebView renderer is unresponsive");
          self.notify(WebviewWindowEvent::RendererUnresponsive);
        }
        RequestEvent::RenderProcessResponsive { browser_id } => {
          tracing::info!(?browser_id, label = self.label, "WebView renderer is responsive again");
          self.notify(WebviewWindowEvent::RendererResponsive);
        }
        RequestEvent::RenderProcessTerminated {
          browser_id,
          status,
          error_code,
          error_string,
        } => {
          tracing::error!(
            ?browser_id,
            ?status,
            error_code,
            ?error_string,
            label = self.label,
            "WebView renderer terminated"
          );
          self.notify(WebviewWindowEvent::RendererTerminated {
            status: format!("{status:?}"),
            error_code,
            description: error_string,
          });
        }
        _ => {}
      },
      _ => {}
    }
    task
  }
}

impl IcedApplication for WebviewWindow {
  fn __id(&self) -> ProgramId {
    self.program_id
  }

  fn boot(&mut self) -> Task<Arc<dyn AnyMessage>> {
    if self.group.snapshot(&self.label).map_or(true, |instance| {
      instance.program_id != self.program_id || instance.close_requested
    }) {
      self.state = BrowserState::Closed;
      self.finish();
      return Task::none();
    }
    let (id, task) = window::open(window::Settings {
      size: iced::Size::new(840.0, 580.0),
      exit_on_close_request: false,
      ..Default::default()
    });
    self.window = Some(id);
    self.group.change(&self.label, self.program_id, |instance| {
      instance.native_window = Some(id)
    });
    let program_id = self.program_id;
    task.map(move |id| program_message(program_id, Message::Opened(id)))
  }

  fn window_id(&self) -> Option<window::Id> {
    self.window
  }
  fn window_title(&self) -> Option<&str> {
    Some(self.title)
  }

  fn view(&self, _id: window::Id) -> Element<'_, Arc<dyn AnyMessage>> {
    let program_id = self.program_id;
    let webview = self
      .browser
      .view_with_render_mode(WebviewRenderMode::Crop)
      .map(Message::Cef);
    Element::from(iced::widget::container(webview).width(iced::Fill).height(iced::Fill))
      .map(move |message| program_message(program_id, message))
  }

  fn update(&mut self, message: Arc<dyn AnyMessage>) -> Task<Arc<dyn AnyMessage>> {
    let any: &dyn std::any::Any = message.as_ref();
    let Some(routed) = any.downcast_ref::<Routed>() else {
      return Task::none();
    };
    if routed.id != self.program_id {
      return Task::none();
    }
    let task = match &routed.message {
      Message::Opened(id) if self.window == Some(*id) && self.state == BrowserState::WaitingForWindow => {
        self.notify(WebviewWindowEvent::Opened(*id));
        self.state = BrowserState::Creating;
        self.update_browser(CefMessage::Create {
          window_id: *id,
          config: WebviewConfig {
            url: self.url.clone(),
            persist_session_cookies: false,
            lifecycle: Some(self.policy.clone()),
            ..Default::default()
          },
        })
      }
      Message::Opened(id) if self.window == Some(*id) && self.state == BrowserState::Closed => window::close(*id),
      Message::Cef(message) => self.update_browser(message.clone()),
      Message::Close => self.request_close(),
      Message::Reload if self.state == BrowserState::Open => {
        self.session.invalidate();
        self.browser.reload();
        Task::none()
      }
      #[cfg(debug_assertions)]
      Message::CrashRendererForTesting(response) => {
        if let Some(response) = response.lock().unwrap().take() {
          let browser_id = self.browser.browser_id();
          let group = self.group.clone();
          let label = self.label.clone();
          let program_id = self.program_id;
          let queued = crate::webview::task::cef_post_task(::cef::sys::cef_thread_id_t::TID_UI.into(), move || {
            use ::cef::{ImplBrowser, ImplBrowserHost};
            let result = (|| -> Result<i32> {
              let current = group.snapshot(&label)?;
              ensure!(
                current.program_id == program_id && !current.close_requested,
                "WebView instance expired before fault injection"
              );
              let browser_id = browser_id.context("CEF browser is not available")?;
              let browser =
                ::cef::browser_host_get_browser_by_identifier(browser_id.inner()).context("CEF browser is closed")?;
              let host = browser.host().context("CEF browser host is unavailable")?;
              let message_id = host.execute_dev_tools_method(0, Some(&"Page.crash".into()), None);
              ensure!(message_id > 0, "CEF rejected Page.crash");
              tracing::info!(
                ?browser_id,
                message_id,
                label,
                cef_crash_reporting_enabled = ::cef::crash_reporting_enabled() == 1,
                "debug renderer fault injection admitted"
              );
              Ok(message_id)
            })();
            let _ = response.send(result.map_err(|error| format!("{error:#}")));
          });
          if let Err(error) = queued {
            tracing::error!(?error, "cannot queue renderer fault injection");
          }
        }
        Task::none()
      }
      Message::Capture(response) => {
        if let Some(response) = response.lock().unwrap().take() {
          if let Some(window) = self.window {
            let response = Arc::new(Mutex::new(Some(response)));
            let program_id = self.program_id;
            let group = self.group.clone();
            let label = self.label.clone();
            return window::screenshot(window).map(move |frame| {
              if let Some(response) = response.lock().unwrap().take() {
                let current = group
                  .snapshot(&label)
                  .is_ok_and(|instance| instance.program_id == program_id && !instance.close_requested);
                let _ = response.send(if current {
                  Ok(frame)
                } else {
                  Err("WebView instance expired before capture completed".into())
                });
              }
              program_message(program_id, Message::Captured)
            });
          }
          let _ = response.send(Err("WebView window is closed".into()));
        }
        Task::none()
      }
      Message::CloseRequested(id) if self.window == Some(*id) => self.request_close(),
      Message::NativeClosed(id) if self.window == Some(*id) => {
        self.window = None;
        tracing::info!(?id, label = self.label, "standalone WebView window closed");
        if self.state == BrowserState::Closed {
          self.finish();
          Task::none()
        } else {
          self.request_close()
        }
      }
      _ => Task::none(),
    };
    let program_id = self.program_id;
    task.map(move |message| program_message(program_id, message))
  }

  fn subscription(&self) -> Subscription<Arc<dyn AnyMessage>> {
    let program_id = self.program_id;
    Subscription::batch([
      self.browser.subscription().map(Message::Cef),
      window::close_requests().map(Message::CloseRequested),
      window::close_events().map(Message::NativeClosed),
    ])
    .with(program_id)
    .map(|(id, message)| program_message(id, message))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn pages_require_nonempty_html_unique_safe_labels() {
    assert!(validate_pages(&[]).is_err());
    for label in ["", "..", "../page", "a/b", "a\\b"] {
      assert!(
        validate_pages(&[Page {
          label: label.into(),
          title: "test",
          html: "<html/>"
        }])
        .is_err()
      );
    }
    assert!(
      validate_pages(&[Page {
        label: "a".into(),
        title: "A",
        html: " "
      }])
      .is_err()
    );
    let pages = vec![
      Page {
        label: "a".into(),
        title: "A",
        html: "<html>A</html>",
      },
      Page {
        label: "a".into(),
        title: "B",
        html: "<html>B</html>",
      },
    ];
    assert!(validate_pages(&pages).is_err());
    assert!(
      validate_pages(&[Page {
        label: "main-2_A".into(),
        title: "A",
        html: "<html/>"
      }])
      .is_ok()
    );
  }

  #[test]
  fn per_window_messages_preserve_program_identity() {
    let a = ProgramId::new("webview", crate::window::new_scope_id());
    let b = ProgramId::new("webview", crate::window::new_scope_id());
    assert_ne!(a, b);
    assert_eq!(program_message(a, Message::Close).program_id(), a);
    assert_eq!(program_message(b, Message::Close).program_id(), b);
  }

  fn empty_group(exit_on_last_window: bool) -> WindowGroup {
    let url = Url::parse("file:///tmp/lintel-registry-test.html").unwrap();
    let blueprint = Blueprint {
      title: "test",
      url: url.clone(),
      policy: Arc::new(LocalPagePolicy::new(url)),
      resources: Weak::new(),
    };
    WindowGroup::new(
      BTreeMap::from([("a".into(), blueprint.clone()), ("b".into(), blueprint)]),
      exit_on_last_window,
    )
  }

  fn group(exit_on_last_window: bool) -> WindowGroup {
    let group = empty_group(exit_on_last_window);
    group.admit_reopen("a", |_, _| Ok(())).unwrap();
    group.admit_reopen("b", |_, _| Ok(())).unwrap();
    group
  }

  #[derive(Debug)]
  struct SetupFailure(Arc<()>);
  impl std::fmt::Display for SetupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
      formatter.write_str("requested WebView setup failure")
    }
  }
  impl std::error::Error for SetupFailure {}

  #[test]
  fn startup_failure_retires_unstarted_reservations_and_preserves_original_error() {
    let group = group(false);
    let a = group.snapshot("a").unwrap().program_id;
    let b = group.snapshot("b").unwrap().program_id;
    let marker = Arc::new(());
    let error = group.rollback_startup_error(SetupFailure(marker.clone()).into());
    assert!(Arc::ptr_eq(&error.downcast_ref::<SetupFailure>().unwrap().0, &marker));
    assert_eq!(group.remaining(), 0);
    assert!(group.quit_requested.load(Ordering::Acquire));
    assert!(group.snapshot("a").is_err() && group.snapshot("b").is_err());
    assert!(!group.current("a", a) && !group.current("b", b));
    // 排队的旧程序即使后续被观察，boot 的 registry 检查也会拒绝创建窗口。
    assert_eq!(group.complete("a", a), None);
    assert!(group.admit_reopen("a", |_, _| Ok(())).is_err());
  }

  #[test]
  fn partial_startup_admission_failure_rolls_back_prior_reservations_without_faking_native_close() {
    let group = empty_group(false);
    let a = group.admit_reopen("a", |_, _| Ok(())).unwrap();
    let error = group
      .admit_reopen("b", |_, _| Err(crate::ProgramCommandError::QueueFull.into()))
      .unwrap_err();
    assert_eq!(group.remaining(), 1);
    let error = group.rollback_startup_error(error);
    assert_eq!(
      error.downcast_ref::<crate::ProgramCommandError>(),
      Some(&crate::ProgramCommandError::QueueFull)
    );
    assert_eq!(group.remaining(), 0);
    assert!(!group.current("a", a));
    assert!(
      group
        .registry
        .lock()
        .unwrap()
        .values()
        .all(|entry| entry.instance.is_none())
    );
  }

  #[test]
  fn startup_rollback_refuses_started_windows_without_clearing_any_reservation() {
    for browser_ready in [false, true] {
      let group = group(false);
      let id = group.snapshot("a").unwrap().program_id;
      group.change("a", id, |instance| {
        if browser_ready {
          instance.browser_ready = true;
        } else {
          instance.native_window = Some(window::Id::unique());
        }
      });
      let before = group.snapshot("a").unwrap();
      assert!(group.rollback_startup().is_err());
      assert_eq!(group.snapshot("a").unwrap(), before);
      assert_eq!(group.remaining(), 2);
      assert!(!group.quit_requested.load(Ordering::Acquire));
    }
  }

  #[test]
  fn rejected_close_keeps_quit_cause_for_retry_and_never_finishes_live_browsers() {
    let group = group(false);
    let quit = AppEvent::Relaunch(
      Some(vec!["--after-update".into()]),
      crate::core::event_loop::event::QuitState::Init,
    );
    *group.pending_quit.lock().unwrap() = Some(quit);
    group.quit_requested.store(true, Ordering::Release);
    assert!(
      group
        .admit_close("a", |_| Err(crate::ProgramCommandError::QueueFull.into()))
        .is_err()
    );
    assert!(!group.snapshot("a").unwrap().close_requested);
    assert!(group.pending_quit.lock().unwrap().is_some());
    assert_eq!(group.remaining(), 2);
    group.admit_close("a", |_| Ok(())).unwrap();
    assert!(group.snapshot("a").unwrap().close_requested);
    let a = group.snapshot("a").unwrap().program_id;
    let b = group.snapshot("b").unwrap().program_id;
    assert_eq!(group.complete("a", a), Some(false));
    assert!(group.pending_quit.lock().unwrap().is_some());
    assert_eq!(group.complete("b", b), Some(true));
  }

  #[test]
  fn sibling_close_and_duplicate_callbacks_do_not_quit_or_underflow() {
    let group = group(true);
    let a = group.snapshot("a").unwrap().program_id;
    let b = group.snapshot("b").unwrap().program_id;
    assert_eq!(group.complete("a", a), Some(false));
    assert_eq!(group.complete("a", a), None);
    assert_eq!(group.remaining(), 1);
    assert_eq!(group.complete("b", b), Some(true));
    assert_eq!(group.complete("b", b), None);
    assert_eq!(group.remaining(), 0);
  }

  #[test]
  fn explicit_quit_finishes_after_all_browsers_even_with_keep_alive() {
    let keep_alive = group(false);
    let a = keep_alive.snapshot("a").unwrap().program_id;
    let b = keep_alive.snapshot("b").unwrap().program_id;
    assert_eq!(keep_alive.complete("a", a), Some(false));
    assert_eq!(keep_alive.complete("b", b), Some(false));
    let explicit = group(false);
    let a = explicit.snapshot("a").unwrap().program_id;
    let b = explicit.snapshot("b").unwrap().program_id;
    explicit.quit_requested.store(true, Ordering::Release);
    assert_eq!(explicit.complete("a", a), Some(false));
    assert_eq!(explicit.complete("b", b), Some(true));
    assert!(explicit.admit_reopen("a", |_, _| Ok(())).is_err());
  }

  #[test]
  fn keep_alive_reopens_after_all_pages_close_with_fresh_identity_and_rejects_stale_callbacks() {
    let group = group(false);
    let old_a = group.snapshot("a").unwrap();
    let old_b = group.snapshot("b").unwrap();
    assert_eq!(group.complete("a", old_a.program_id), Some(false));
    assert_eq!(group.complete("b", old_b.program_id), Some(false));
    assert_eq!(group.remaining(), 0);
    let fresh = group.admit_reopen("b", |_, _| Ok(())).unwrap();
    let current = group.snapshot("b").unwrap();
    assert_eq!(current.program_id, fresh);
    assert_ne!(current.program_id, old_b.program_id);
    assert_ne!(current.scope_id, old_b.scope_id);
    assert!(!current.close_requested && !current.browser_ready);
    assert_eq!(group.complete("b", old_b.program_id), None);
    assert!(!group.change("b", old_b.program_id, |instance| instance.close_requested = true));
    assert_eq!(group.remaining(), 1);
    assert!(!group.current("b", old_b.program_id));
    assert!(group.current("b", fresh));
    assert_eq!(
      program_message(old_b.program_id, Message::Close).program_id(),
      old_b.program_id
    );
  }

  #[test]
  fn rejected_reopen_restores_closed_instance_and_never_admits_unknown_pages() {
    let group = group(false);
    let old = group.snapshot("b").unwrap();
    group.complete("b", old.program_id);
    let before = group.registry.lock().unwrap().get("b").unwrap().instance;
    assert!(
      group
        .admit_reopen("b", |_, _| Err(crate::ProgramCommandError::QueueFull.into()))
        .is_err()
    );
    assert_eq!(group.registry.lock().unwrap().get("b").unwrap().instance, before);
    assert_eq!(group.remaining(), 1);
    assert!(group.admit_reopen("remote-injected", |_, _| Ok(())).is_err());
    assert!(group.admit_reopen("a", |_, _| Ok(())).is_err());
    assert!(group.admit_reopen("b", |_, _| Ok(())).is_ok());
    assert_eq!(group.remaining(), 2);
  }
}
