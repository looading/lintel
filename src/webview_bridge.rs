use std::{
  collections::{BTreeMap, HashMap},
  future::Future,
  pin::Pin,
  sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
  },
  time::Duration,
};

use anyhow::{Context, Result, ensure};
use lintel_core::{
  context::{AppContext, store::ServiceStore},
  tasks::{TaskCancellation, TaskScope, TaskTracker},
};
use lintel_iced_widgets::webview::{
  CefAction, ClientLifecyeleImpl, JsHandler, JsInvocation, LocalPagePolicy, RequestEvent,
};
use lintel_shared_data::TransferData;
use serde::{Serialize, de::DeserializeOwned};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::permissions::{Capabilities, Principal};

type CommandFuture = Pin<Box<dyn Future<Output = Result<TransferData>> + Send>>;
type Command =
  Box<dyn Fn(WebviewCommandContext, TransferData, Option<OwnedSemaphorePermit>) -> CommandFuture + Send + Sync>;

/// 来自宿主的窗口身份和弱服务入口，不持有应用或延长页面生命周期。
/// 命令仍需通过既有页面、权限和会话检查；取得服务不构成操作系统沙箱。
#[derive(Clone)]
pub struct WebviewCommandContext {
  services: Weak<ServiceStore>,
  label: Arc<str>,
  window_scope: u64,
  session: Weak<BridgeSession>,
  generation: u64,
}

impl WebviewCommandContext {
  /// 返回由宿主配置的窗口标签，不从请求参数推断窗口身份。
  pub fn label(&self) -> &str {
    &self.label
  }

  /// 返回当前命令所属的受管窗口任务域。
  pub fn window_scope(&self) -> u64 {
    self.window_scope
  }

  /// 只在来源页面会话仍有效时查询服务；返回值由调用方独立持有。
  /// 导航或关闭不能撤销已经交付给可信 Rust 代码的服务句柄。
  pub fn service<S>(&self) -> Option<Arc<S>>
  where
    S: Send + Sync + ?Sized + 'static,
  {
    let session = self.session.upgrade()?;
    if session.closed.load(Ordering::Acquire) || session.generation.load(Ordering::Acquire) != self.generation {
      return None;
    }
    self.services.upgrade()?.get_service::<S>()
  }

  /// 必须存在的服务未启动、已释放或页面已失效时，返回固定错误。
  pub fn require_service<S>(&self) -> Result<Arc<S>>
  where
    S: Send + Sync + ?Sized + 'static,
  {
    self.service::<S>().context("LINTEL_SERVICE_UNAVAILABLE")
  }

  #[cfg(test)]
  fn unavailable() -> Self {
    Self {
      services: Weak::new(),
      label: "test".into(),
      window_scope: 0,
      session: Weak::new(),
      generation: 0,
    }
  }
}

/// 每个窗口实例独立绑定文档会话，复用的页面蓝图只提供 URL/资源访问策略。
pub(crate) struct BridgePagePolicy {
  page: Arc<LocalPagePolicy>,
  session: Weak<BridgeSession>,
}

impl BridgePagePolicy {
  pub(crate) fn new(page: Arc<LocalPagePolicy>, session: &Arc<BridgeSession>) -> Self {
    Self {
      page,
      session: Arc::downgrade(session),
    }
  }
}

impl std::fmt::Debug for BridgePagePolicy {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("BridgePagePolicy")
      .field("page", &self.page)
      .finish_non_exhaustive()
  }
}

impl ClientLifecyeleImpl for BridgePagePolicy {
  fn on_main_frame_load_start(&self) {
    if let Some(session) = self.session.upgrade() {
      session.invalidate();
    }
  }

  fn bridge_generation(&self) -> Option<u64> {
    self
      .session
      .upgrade()
      .map(|session| session.generation.load(Ordering::Acquire))
  }

  fn before_browser(&self, event: RequestEvent) -> bool {
    self.page.before_browser(event)
  }
  fn allows_navigation(&self, url: &str, is_main_frame: bool) -> bool {
    self.page.allows_navigation(url, is_main_frame)
  }
  fn allows_js_invocation(&self, url: &str, is_main_frame: bool, from_renderer: bool, payload_size: usize) -> bool {
    self
      .page
      .allows_js_invocation(url, is_main_frame, from_renderer, payload_size)
  }
  fn restricts_resources(&self) -> bool {
    self.page.restricts_resources()
  }
  fn allows_resource(&self, url: &str) -> bool {
    self.page.allows_resource(url)
  }
}

pub(crate) struct Commands {
  entries: BTreeMap<String, Command>,
  duplicate: Option<String>,
  permissions: Option<(Principal, Capabilities)>,
  window_permissions: BTreeMap<String, (Principal, Capabilities)>,
  timeout: Duration,
}

impl Default for Commands {
  fn default() -> Self {
    Self {
      entries: BTreeMap::new(),
      duplicate: None,
      permissions: None,
      window_permissions: BTreeMap::new(),
      timeout: Duration::from_secs(15),
    }
  }
}

impl Commands {
  pub(crate) fn set_timeout(&mut self, timeout: Duration) {
    self.timeout = timeout;
  }
  pub(crate) fn add<Request, Response, F>(&mut self, name: &str, handler: F)
  where
    Request: DeserializeOwned + 'static,
    Response: Serialize + 'static,
    F: Fn(Request) -> Result<Response> + Send + Sync + 'static,
  {
    let handler = Arc::new(handler);
    self.insert(
      name,
      Box::new(move |_, payload, permit| {
        let handler = Arc::clone(&handler);
        Box::pin(async move {
          tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let request = payload.json::<Request>().context("LINTEL_INVALID_INPUT")?;
            TransferData::from_json(handler(request).context("LINTEL_COMMAND_FAILED")?)
          })
          .await
          .context("LINTEL_COMMAND_FAILED")?
        })
      }),
    );
  }

  pub(crate) fn add_async<Request, Response, F, Fut>(&mut self, name: &str, handler: F)
  where
    Request: DeserializeOwned + Send + 'static,
    Response: Serialize + Send + 'static,
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response>> + Send + 'static,
  {
    let handler = Arc::new(handler);
    self.insert(
      name,
      Box::new(move |_, payload, permit| {
        let handler = Arc::clone(&handler);
        Box::pin(async move {
          let _permit = permit;
          let request = payload.json::<Request>().context("LINTEL_INVALID_INPUT")?;
          TransferData::from_json(handler(request).await.context("LINTEL_COMMAND_FAILED")?)
        })
      }),
    );
  }

  pub(crate) fn add_with_context<Request, Response, F>(&mut self, name: &str, handler: F)
  where
    Request: DeserializeOwned + 'static,
    Response: Serialize + 'static,
    F: Fn(WebviewCommandContext, Request) -> Result<Response> + Send + Sync + 'static,
  {
    let handler = Arc::new(handler);
    self.insert(
      name,
      Box::new(move |context, payload, permit| {
        let handler = Arc::clone(&handler);
        Box::pin(async move {
          tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let request = payload.json::<Request>().context("LINTEL_INVALID_INPUT")?;
            TransferData::from_json(handler(context, request).context("LINTEL_COMMAND_FAILED")?)
          })
          .await
          .context("LINTEL_COMMAND_FAILED")?
        })
      }),
    );
  }

  pub(crate) fn add_async_with_context<Request, Response, F, Fut>(&mut self, name: &str, handler: F)
  where
    Request: DeserializeOwned + Send + 'static,
    Response: Serialize + Send + 'static,
    F: Fn(WebviewCommandContext, Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Response>> + Send + 'static,
  {
    let handler = Arc::new(handler);
    self.insert(
      name,
      Box::new(move |context, payload, permit| {
        let handler = Arc::clone(&handler);
        Box::pin(async move {
          let _permit = permit;
          let request = payload.json::<Request>().context("LINTEL_INVALID_INPUT")?;
          TransferData::from_json(handler(context, request).await.context("LINTEL_COMMAND_FAILED")?)
        })
      }),
    );
  }

  fn insert(&mut self, name: &str, command: Command) {
    if self.entries.insert(name.to_owned(), command).is_some() && self.duplicate.is_none() {
      self.duplicate = Some(name.to_owned());
    }
  }

  pub(crate) fn set_permissions(&mut self, principal: Principal, permissions: Capabilities) {
    self.permissions = Some((principal, permissions));
  }

  pub(crate) fn set_window_permissions(&mut self, label: String, principal: Principal, permissions: Capabilities) {
    self.window_permissions.insert(label, (principal, permissions));
  }

  pub(crate) fn validate(&self) -> Result<()> {
    ensure!(
      !self.timeout.is_zero() && self.timeout <= Duration::from_secs(3600),
      "invalid command timeout"
    );
    if let Some(name) = &self.duplicate {
      anyhow::bail!("duplicate WebView command {name:?}");
    }
    for name in self.entries.keys() {
      ensure!(
        !name.is_empty()
          && name.len() <= 64
          && !name.starts_with("__lintel.")
          && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)),
        "invalid or reserved WebView command name {name:?}"
      );
    }
    Ok(())
  }

  pub(crate) fn handler_for_window(
    self: &Arc<Self>,
    context: AppContext,
    window_scope: u64,
    session: Arc<BridgeSession>,
    label: &str,
  ) -> impl JsHandler<Message = JsInvocation> + Send + 'static {
    CommandHandler {
      commands: Arc::clone(self),
      tracker: context.task_tracker().clone(),
      runtime: context.runtime.clone(),
      services: Arc::downgrade(&context.service_store),
      label: Arc::from(label),
      window_scope,
      session,
      permissions: self
        .window_permissions
        .get(label)
        .cloned()
        .or_else(|| self.permissions.clone()),
    }
  }

  #[cfg(test)]
  fn dispatch(&self, name: &str, payload: TransferData) -> Result<CommandFuture> {
    if let Some((principal, permissions)) = &self.permissions {
      ensure!(permissions.allows(principal, name), "LINTEL_DENIED");
    }
    let command = self.entries.get(name).context("LINTEL_UNKNOWN_COMMAND")?;
    Ok(command(WebviewCommandContext::unavailable(), payload, None))
  }
}

type Response = Box<dyn FnOnce(Result<TransferData>) + Send>;

#[derive(Clone)]
struct Reply(Arc<Mutex<Option<Response>>>);

impl Reply {
  fn send(&self, result: Result<TransferData>) {
    if let Ok(mut reply) = self.0.lock()
      && let Some(reply) = reply.take()
    {
      reply(result);
    }
  }
}

struct Pending {
  generation: u64,
  cancellation: Option<TaskCancellation>,
  reply: Reply,
}

/// 每个 WebView 独立持有请求表，导航和关闭时撤销旧文档。
pub(crate) struct BridgeSession {
  generation: AtomicU64,
  closed: AtomicBool,
  pending: Mutex<HashMap<String, Pending>>,
  slots: Arc<Semaphore>,
}

impl BridgeSession {
  /// 加载通知只是延迟到达的 UI 元数据，旧文档撤销已在原生来源回调完成。
  /// 保留 renderer 故障的既有 UI 撤销路径。
  pub(crate) fn observe_browser_action(&self, action: &CefAction) {
    if matches!(action, CefAction::Request(RequestEvent::RenderProcessTerminated { .. })) {
      self.invalidate();
    }
  }

  pub(crate) fn new() -> Arc<Self> {
    Arc::new(Self {
      generation: AtomicU64::new(0),
      closed: AtomicBool::new(false),
      pending: Mutex::new(HashMap::new()),
      slots: Arc::new(Semaphore::new(64)),
    })
  }

  pub(crate) fn invalidate(&self) {
    self.cancel_all();
  }

  pub(crate) fn close(&self) {
    self.closed.store(true, Ordering::Release);
    self.invalidate();
  }

  fn cancel_all(&self) {
    if let Ok(mut pending) = self.pending.lock() {
      self.generation.fetch_add(1, Ordering::AcqRel);
      for (_, request) in pending.drain() {
        request.reply.send(Err(anyhow::anyhow!("LINTEL_CANCELLED")));
        if let Some(cancellation) = request.cancellation {
          cancellation.cancel();
        }
      }
    }
  }

  #[cfg(test)]
  fn cancel(&self, id: &str) {
    let _ = self.cancel_in_generation(id, self.generation.load(Ordering::Acquire));
  }

  fn cancel_in_generation(&self, id: &str, generation: u64) -> Result<()> {
    let request = {
      let mut pending = self.pending.lock().map_err(|_| anyhow::anyhow!("LINTEL_BUSY"))?;
      ensure!(
        !self.closed.load(Ordering::Acquire) && self.generation.load(Ordering::Acquire) == generation,
        "LINTEL_CANCELLED"
      );
      if pending.get(id).is_some_and(|request| request.generation == generation) {
        pending.remove(id)
      } else {
        None
      }
    };
    if let Some(request) = request {
      request.reply.send(Err(anyhow::anyhow!("LINTEL_CANCELLED")));
      if let Some(cancellation) = request.cancellation {
        cancellation.cancel();
      }
    }
    Ok(())
  }
}

struct PendingGuard {
  session: Arc<BridgeSession>,
  id: String,
  generation: u64,
  reply: Reply,
}

impl Drop for PendingGuard {
  fn drop(&mut self) {
    if let Ok(mut pending) = self.session.pending.lock()
      && pending
        .get(&self.id)
        .is_some_and(|request| request.generation == self.generation && Arc::ptr_eq(&request.reply.0, &self.reply.0))
    {
      pending.remove(&self.id);
    }
    self.reply.send(Err(anyhow::anyhow!("LINTEL_CANCELLED")));
  }
}

struct CommandHandler {
  commands: Arc<Commands>,
  tracker: TaskTracker,
  runtime: tokio::runtime::Handle,
  services: Weak<ServiceStore>,
  label: Arc<str>,
  window_scope: u64,
  session: Arc<BridgeSession>,
  permissions: Option<(Principal, Capabilities)>,
}

impl JsHandler for CommandHandler {
  type Message = JsInvocation;

  fn handle(&mut self, invocation: Self::Message) {
    let Some(response) = invocation.responder() else {
      return;
    };
    let reply = Reply(Arc::new(Mutex::new(Some(Box::new(move |result| {
      response.send(result.and_then(TransferData::to_bytes));
    })))));
    let host_generation = invocation.host_generation;
    let payload = TransferData::from_bytes(invocation.params);
    let result = payload.and_then(|payload| {
      self.start_from_generation(
        &invocation.fn_name,
        invocation.context_id,
        payload,
        reply.clone(),
        host_generation,
      )
    });
    if let Err(error) = result {
      reply.send(Err(error));
    }
  }
}

impl CommandHandler {
  #[cfg(test)]
  fn start(&self, name: &str, context_id: String, payload: TransferData, reply: Reply) -> Result<()> {
    self.start_from_generation(
      name,
      context_id,
      payload,
      reply,
      Some(self.session.generation.load(Ordering::Acquire)),
    )
  }

  fn start_from_generation(
    &self,
    name: &str,
    context_id: String,
    payload: TransferData,
    reply: Reply,
    host_generation: Option<u64>,
  ) -> Result<()> {
    self.start_with_stage_hook(name, context_id, payload, reply, host_generation, |_| {})
  }

  fn start_with_stage_hook(
    &self,
    name: &str,
    context_id: String,
    payload: TransferData,
    reply: Reply,
    host_generation: Option<u64>,
    stage: impl Fn(AdmissionStage),
  ) -> Result<()> {
    ensure!(!self.session.closed.load(Ordering::Acquire), "LINTEL_CANCELLED");
    let generation = host_generation.context("LINTEL_CANCELLED")?;
    ensure!(
      generation == self.session.generation.load(Ordering::Acquire),
      "LINTEL_CANCELLED"
    );
    let envelope: serde_json::Value = payload.json()?;
    if name == "__lintel.cancel" {
      let id = envelope
        .get("id")
        .and_then(|id| id.as_str())
        .context("LINTEL_INVALID_INPUT")?;
      self.session.cancel_in_generation(id, generation)?;
      reply.send(TransferData::from_json(()));
      return Ok(());
    }
    let (id, input) = if envelope.get("__lintel").and_then(|version| version.as_str()) == Some("request-v1") {
      let id = envelope
        .get("id")
        .and_then(|id| id.as_str())
        .context("LINTEL_INVALID_INPUT")?;
      ensure!(!id.is_empty() && id.len() <= 128, "LINTEL_INVALID_INPUT");
      (
        id.to_owned(),
        TransferData::from_json(envelope.get("input").cloned().unwrap_or_default())?,
      )
    } else {
      (context_id, payload)
    };
    let permit = Arc::clone(&self.session.slots)
      .try_acquire_owned()
      .context("LINTEL_BUSY")?;
    if let Some((principal, permissions)) = &self.permissions {
      ensure!(permissions.allows(principal, name), "LINTEL_DENIED");
    }
    let context = WebviewCommandContext {
      services: self.services.clone(),
      label: self.label.clone(),
      window_scope: self.window_scope,
      session: Arc::downgrade(&self.session),
      generation,
    };
    let future = self.commands.entries.get(name).context("LINTEL_UNKNOWN_COMMAND")?(context, input, Some(permit));
    let session = Arc::clone(&self.session);
    let key = id.clone();
    let response = reply.clone();
    let mut pending = self
      .session
      .pending
      .lock()
      .map_err(|_| anyhow::anyhow!("LINTEL_BUSY"))?;
    ensure!(
      !self.session.closed.load(Ordering::Acquire) && self.session.generation.load(Ordering::Acquire) == generation,
      "LINTEL_CANCELLED"
    );
    ensure!(!pending.contains_key(&id), "LINTEL_DUPLICATE_REQUEST");
    pending.insert(
      id.clone(),
      Pending {
        generation,
        cancellation: None,
        reply: reply.clone(),
      },
    );
    drop(pending);
    stage(AdmissionStage::Reserved);
    let guard = PendingGuard {
      session: Arc::clone(&session),
      id: key,
      generation,
      reply: response.clone(),
    };
    let (open_start, await_start) = tokio::sync::oneshot::channel();
    let timeout = self.commands.timeout;
    let task = self.tracker.spawn(
      &self.runtime,
      TaskScope::page(self.window_scope, format!("bridge-{generation}")),
      async move {
        let _guard = guard;
        if !await_registered_start(await_start, &session, generation).await {
          return;
        }
        let result = match tokio::time::timeout(timeout, future).await {
          Ok(result) => result,
          Err(_) => Err(anyhow::anyhow!("LINTEL_TIMEOUT")),
        };
        if !session.closed.load(Ordering::Acquire) && session.generation.load(Ordering::Acquire) == generation {
          response.send(result);
        } else {
          response.send(Err(anyhow::anyhow!("LINTEL_CANCELLED")));
        }
      },
    )?;
    stage(AdmissionStage::Spawned);
    let mut pending = self
      .session
      .pending
      .lock()
      .map_err(|_| anyhow::anyhow!("LINTEL_BUSY"))?;
    if let Some(request) = pending.get_mut(&id).filter(|request| {
      request.generation == generation
        && Arc::ptr_eq(&request.reply.0, &reply.0)
        && !self.session.closed.load(Ordering::Acquire)
        && self.session.generation.load(Ordering::Acquire) == generation
    }) {
      request.cancellation = Some(task.cancellation_handle());
      drop(pending);
      if open_start.send(()).is_err() {
        task.cancel();
      }
    } else {
      drop(pending);
      task.cancel();
    }
    Ok(())
  }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AdmissionStage {
  Reserved,
  Spawned,
}

async fn await_registered_start(
  start: tokio::sync::oneshot::Receiver<()>,
  session: &BridgeSession,
  generation: u64,
) -> bool {
  start.await.is_ok()
    && !session.closed.load(Ordering::Acquire)
    && session.generation.load(Ordering::Acquire) == generation
}

pub(crate) fn init_logging() {
  if std::env::var_os("LINTEL_WEBVIEW_LOG").is_some() {
    let _ = tracing_subscriber::fmt()
      .with_target(false)
      .with_max_level(tracing::Level::INFO)
      .try_init();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn invocation(
    command: &str,
    request_id: &str,
    payload: serde_json::Value,
    host_generation: Option<u64>,
  ) -> (JsInvocation, impl Future<Output = Result<TransferData>>) {
    let (send, receive) = tokio::sync::oneshot::channel();
    let mut invocation = JsInvocation::new(
      crate::webview::BrowserId::new(17),
      request_id.into(),
      "main-frame".into(),
      command.into(),
      TransferData::from_json(payload).unwrap().to_bytes().unwrap(),
      send,
    );
    invocation.host_generation = host_generation;
    (invocation, async move { TransferData::from_bytes(receive.await??) })
  }

  #[tokio::test]
  async fn native_queued_ingress_requires_the_captured_epoch_and_cannot_use_a_json_epoch() {
    use std::sync::atomic::AtomicUsize;
    let session = BridgeSession::new();
    let policy = BridgePagePolicy::new(
      Arc::new(LocalPagePolicy::new(
        url::Url::parse("file:///app/ui/index.html").unwrap(),
      )),
      &session,
    );
    let old_epoch = policy.bridge_generation();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let mut commands = Commands::default();
    commands.add_async("increment", move |number: u32| {
      observed.fetch_add(1, Ordering::AcqRel);
      async move { Ok(number + 1) }
    });
    let tracker = TaskTracker::new();
    let mut handler = CommandHandler {
      commands: Arc::new(commands),
      tracker: tracker.clone(),
      runtime: tokio::runtime::Handle::current(),
      services: Weak::new(),
      label: "ingress-window".into(),
      window_scope: 61,
      session: session.clone(),
      permissions: None,
    };
    policy.on_main_frame_load_start();
    let fresh_epoch = policy.bridge_generation();
    for stamp in [old_epoch, None] {
      let (queued, result) = invocation(
        "increment",
        "queued",
        serde_json::json!({
          "__lintel":"request-v1", "id":"reused", "input":41,
          "host_generation": fresh_epoch, "generation": fresh_epoch,
        }),
        stamp,
      );
      handler.handle(queued);
      assert_eq!(result.await.unwrap_err().to_string(), "LINTEL_CANCELLED");
    }
    assert_eq!(calls.load(Ordering::Acquire), 0);
    let (fresh, result) = invocation(
      "increment",
      "fresh",
      serde_json::json!({
        "__lintel":"request-v1", "id":"reused", "input":41,
      }),
      fresh_epoch,
    );
    handler.handle(fresh);
    assert_eq!(result.await.unwrap().json::<u32>().unwrap(), 42);
    tracker.wait_idle().await;
    assert_eq!(calls.load(Ordering::Acquire), 1);
  }

  #[tokio::test]
  async fn native_old_epoch_cancel_cannot_cancel_a_reused_id_from_the_new_document() {
    let session = BridgeSession::new();
    let mut commands = Commands::default();
    commands.add_async("pending", |_: ()| async {
      std::future::pending::<()>().await;
      Ok(())
    });
    let tracker = TaskTracker::new();
    let mut handler = CommandHandler {
      commands: Arc::new(commands),
      tracker: tracker.clone(),
      runtime: tokio::runtime::Handle::current(),
      services: Weak::new(),
      label: "cancel-window".into(),
      window_scope: 62,
      session: session.clone(),
      permissions: None,
    };
    session.invalidate();
    let (fresh, response) = invocation("pending", "reused", serde_json::Value::Null, Some(1));
    handler.handle(fresh);
    let mut response = Box::pin(response);
    let (old_cancel, result) = invocation(
      "__lintel.cancel",
      "old-cancel",
      serde_json::json!({"id":"reused"}),
      Some(0),
    );
    handler.handle(old_cancel);
    assert_eq!(result.await.unwrap_err().to_string(), "LINTEL_CANCELLED");
    assert!(session.pending.lock().unwrap().contains_key("reused"));
    assert!(matches!(
      crate::iced::futures::poll!(response.as_mut()),
      std::task::Poll::Pending
    ));
    let (current_cancel, result) = invocation(
      "__lintel.cancel",
      "current-cancel",
      serde_json::json!({"id":"reused"}),
      Some(1),
    );
    handler.handle(current_cancel);
    result.await.unwrap();
    assert_eq!(response.await.unwrap_err().to_string(), "LINTEL_CANCELLED");
    tracker.wait_idle().await;
  }

  #[tokio::test]
  async fn native_revoke_during_reservation_or_spawn_never_polls_old_user_work_and_keeps_reused_id() {
    use std::sync::atomic::AtomicUsize;
    for revoke_stage in [AdmissionStage::Reserved, AdmissionStage::Spawned] {
      let session = BridgeSession::new();
      let calls = Arc::new(AtomicUsize::new(0));
      let observed = calls.clone();
      let mut commands = Commands::default();
      commands.add_async("work", move |number: u32| {
        observed.fetch_add(1, Ordering::AcqRel);
        async move { Ok(number) }
      });
      let tracker = TaskTracker::new();
      let handler = CommandHandler {
        commands: Arc::new(commands),
        tracker: tracker.clone(),
        runtime: tokio::runtime::Handle::current(),
        services: Weak::new(),
        label: "reserved-window".into(),
        window_scope: 63,
        session: session.clone(),
        permissions: None,
      };
      let (old_reply, old_result) = reply_channel();
      let (fresh_reply, fresh_result) = reply_channel();
      let fresh_reply = Mutex::new(Some(fresh_reply));
      handler
        .start_with_stage_hook(
          "work",
          "reused".into(),
          TransferData::from_json(1).unwrap(),
          old_reply,
          Some(0),
          |stage| {
            if stage == revoke_stage {
              session.invalidate();
              handler
                .start_from_generation(
                  "work",
                  "reused".into(),
                  TransferData::from_json(2).unwrap(),
                  fresh_reply.lock().unwrap().take().unwrap(),
                  Some(1),
                )
                .unwrap();
            }
          },
        )
        .unwrap();
      assert_eq!(old_result.await.unwrap().unwrap_err().to_string(), "LINTEL_CANCELLED");
      assert_eq!(fresh_result.await.unwrap().unwrap().json::<u32>().unwrap(), 2);
      tracker.wait_idle().await;
      assert_eq!(calls.load(Ordering::Acquire), 1);
      assert!(session.pending.lock().unwrap().is_empty());
      assert_eq!(session.slots.available_permits(), 64);
    }
  }

  #[tokio::test]
  async fn native_start_gate_is_pending_until_commit_and_rechecks_revoke_before_user_poll() {
    use std::sync::atomic::AtomicUsize;
    let session = BridgeSession::new();
    let calls = AtomicUsize::new(0);
    let (commit, start) = tokio::sync::oneshot::channel();
    let mut task = Box::pin(async {
      if await_registered_start(start, &session, 0).await {
        calls.fetch_add(1, Ordering::AcqRel);
      }
    });
    assert!(crate::iced::futures::poll!(task.as_mut()).is_pending());
    assert_eq!(calls.load(Ordering::Acquire), 0);
    session.invalidate();
    commit.send(()).unwrap();
    task.await;
    assert_eq!(calls.load(Ordering::Acquire), 0);
  }

  #[tokio::test]
  async fn native_main_frame_start_revokes_old_work_before_fresh_commands_and_delayed_ui_metadata() {
    let session = BridgeSession::new();
    let page = Arc::new(LocalPagePolicy::new(
      url::Url::parse("file:///app/ui/index.html").unwrap(),
    ));
    let policy = BridgePagePolicy::new(page, &session);
    let (started, mut ready) = tokio::sync::mpsc::unbounded_channel();
    let mut commands = Commands::default();
    commands.add_async("pending", move |name: String| {
      let started = started.clone();
      async move {
        started.send(name).unwrap();
        std::future::pending::<()>().await;
        Ok(())
      }
    });
    let tracker = TaskTracker::new();
    let handler = CommandHandler {
      commands: Arc::new(commands),
      tracker: tracker.clone(),
      runtime: tokio::runtime::Handle::current(),
      services: Weak::new(),
      label: "document-window".into(),
      window_scope: 51,
      session: session.clone(),
      permissions: None,
    };
    let (reply, old) = reply_channel();
    handler
      .start(
        "pending",
        "old-id".into(),
        TransferData::from_json("old").unwrap(),
        reply,
      )
      .unwrap();
    assert_eq!(ready.recv().await.unwrap(), "old");
    policy.on_main_frame_load_start();
    assert_eq!(old.await.unwrap().unwrap_err().to_string(), "LINTEL_CANCELLED");
    tracker.wait_idle().await;
    assert_eq!(session.generation.load(Ordering::Acquire), 1);
    let (reply, mut fresh) = reply_channel();
    handler
      .start(
        "pending",
        "fresh-id".into(),
        TransferData::from_json("fresh").unwrap(),
        reply,
      )
      .unwrap();
    assert_eq!(ready.recv().await.unwrap(), "fresh");
    session.observe_browser_action(&CefAction::LoadStart {
      browser_id: crate::webview::BrowserId::new(17),
      frame_id: "main-frame".into(),
      transition_type: 0,
    });
    assert_eq!(session.generation.load(Ordering::Acquire), 1);
    assert!(session.pending.lock().unwrap().contains_key("fresh-id"));
    assert!(matches!(
      fresh.try_recv(),
      Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    session.cancel("fresh-id");
    assert_eq!(fresh.await.unwrap().unwrap_err().to_string(), "LINTEL_CANCELLED");
    tracker.wait_idle().await;
    assert_eq!(session.slots.available_permits(), 64);
  }

  #[test]
  fn native_main_frame_policy_keeps_reopened_sessions_isolated_and_delegates_access_checks() {
    let url = "file:///app/ui/index.html";
    let script = "file:///app/ui/lintel.js";
    let page =
      Arc::new(LocalPagePolicy::new(url::Url::parse(url).unwrap()).with_asset(url::Url::parse(script).unwrap()));
    let old = BridgeSession::new();
    let old_policy = BridgePagePolicy::new(page.clone(), &old);
    let fresh = BridgeSession::new();
    let fresh_policy = BridgePagePolicy::new(page.clone(), &fresh);
    old.close();
    old_policy.on_main_frame_load_start();
    assert_eq!(fresh.generation.load(Ordering::Acquire), 0);
    fresh_policy.on_main_frame_load_start();
    assert_eq!(fresh.generation.load(Ordering::Acquire), 1);
    old_policy.on_main_frame_load_start();
    assert_eq!(fresh.generation.load(Ordering::Acquire), 1);
    for address in [
      url,
      script,
      "about:blank",
      "https://example.invalid/",
      "file:///other.html",
    ] {
      for main in [false, true] {
        assert_eq!(
          fresh_policy.allows_navigation(address, main),
          page.allows_navigation(address, main)
        );
        for renderer in [false, true] {
          assert_eq!(
            fresh_policy.allows_js_invocation(address, main, renderer, 128),
            page.allows_js_invocation(address, main, renderer, 128)
          );
        }
      }
      assert_eq!(fresh_policy.allows_resource(address), page.allows_resource(address));
    }
    assert_eq!(fresh_policy.restricts_resources(), page.restricts_resources());
    assert_eq!(Arc::strong_count(&fresh), 1);
    drop(fresh);
    assert!(fresh_policy.session.upgrade().is_none());
    fresh_policy.on_main_frame_load_start();
  }

  #[test]
  fn webview_bridge_javascript_native_failure_contract() {
    use std::{
      io::Write,
      process::{Command, Stdio},
    };
    let harness = r#"
const vm = require('node:vm');
const assert = require('node:assert/strict');
const source = require('node:fs').readFileSync(0, 'utf8');
const plans = [];
const cancellations = [];
const listeners = new Map();
const window = {
  __rsAPI__(command, payload, success, failure) {
    if (command === '__lintel.cancel') {
      cancellations.push(JSON.parse(payload).id);
      return;
    }
    assert.equal(command, 'probe');
    const envelope = JSON.parse(payload);
    assert.equal(envelope.__lintel, 'request-v1');
    assert.equal('timeoutMs' in envelope, false);
    const plan = plans.shift();
    assert.equal(typeof plan, 'function');
    plan(success, failure);
  },
  addEventListener(name, callback) { listeners.set(name, callback); },
};
vm.runInNewContext(source, { window, setTimeout, clearTimeout });
(async () => {
  for (const value of [undefined, null, false, 0, '', NaN]) {
    plans.push((success, failure) => { failure(value); success('"must not rescue a failure"'); });
    await assert.rejects(window.lintel.invoke('probe'), error => error.code === 'COMMAND_FAILED');
  }
  plans.push((_, failure) => failure('LINTEL_DENIED'));
  await assert.rejects(window.lintel.invoke('probe'), error =>
    error.code === 'DENIED' && error.message === 'LINTEL_DENIED');
  for (const value of [null, false, 0, '', [1, 2], { ok: true }, '中文🙂']) {
    plans.push(success => success(JSON.stringify(value)));
    assert.equal(JSON.stringify(await window.lintel.invoke('probe')), JSON.stringify(value));
  }
  plans.push(success => success(undefined));
  await assert.rejects(window.lintel.invoke('probe'), error => error.code === 'INVALID_RESPONSE');
  plans.push(() => {});
  const controller = new AbortController();
  const cancelled = window.lintel.invoke('probe', null, { signal: controller.signal });
  controller.abort();
  await assert.rejects(cancelled, error => error.code === 'CANCELLED');
  plans.push(() => {});
  await assert.rejects(window.lintel.invoke('probe', null, { timeoutMs: 5 }), error => error.code === 'TIMEOUT');
  plans.push(() => {});
  const hidden = window.lintel.invoke('probe');
  listeners.get('pagehide')();
  await assert.rejects(hidden, error => error.code === 'CANCELLED');
  assert.equal(cancellations.length, 3);
  assert.equal(plans.length, 0);
  console.log('JS_NATIVE_FAILURE_CONTRACT_OK');
})().catch(error => { console.error(error); process.exitCode = 1; });
"#;
    let mut child = Command::new("node")
      .args(["-e", harness])
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .expect("Node.js is required for the WebView JavaScript contract test");
    child
      .stdin
      .take()
      .unwrap()
      .write_all(include_str!("webview_bridge.js").as_bytes())
      .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
      output.status.success(),
      "JavaScript contract failed: {}",
      String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("JS_NATIVE_FAILURE_CONTRACT_OK"));
  }

  fn command_context(
    services: &Arc<ServiceStore>,
    session: &Arc<BridgeSession>,
    label: &str,
    window_scope: u64,
  ) -> WebviewCommandContext {
    WebviewCommandContext {
      services: Arc::downgrade(services),
      label: Arc::from(label),
      window_scope,
      session: Arc::downgrade(session),
      generation: session.generation.load(Ordering::Acquire),
    }
  }

  #[test]
  fn context_does_not_keep_services_or_page_alive_and_refuses_stale_lookups() {
    fn require_send_sync<T: Send + Sync>() {}
    require_send_sync::<WebviewCommandContext>();
    let services = Arc::new(ServiceStore::new());
    let value = Arc::new(41_u32);
    let service = value.clone();
    services.register_service(move || service.clone());
    let session = BridgeSession::new();
    let context = command_context(&services, &session, "trusted-main", 19);
    assert_eq!(context.label(), "trusted-main");
    assert_eq!(context.window_scope(), 19);
    assert!(Arc::ptr_eq(&context.service::<u32>().unwrap(), &value));
    assert_eq!(Arc::strong_count(&services), 1);
    assert_eq!(Arc::strong_count(&session), 1);
    assert!(
      context
        .require_service::<String>()
        .unwrap_err()
        .to_string()
        .contains("SERVICE_UNAVAILABLE")
    );
    let granted = context.require_service::<u32>().unwrap();
    session.invalidate();
    assert!(context.service::<u32>().is_none());
    // 失效不能撤销此前交付给可信 Rust 代码的句柄。
    assert_eq!(*granted, 41);
    let current = command_context(&services, &session, "trusted-main", 19);
    assert_eq!(*current.require_service::<u32>().unwrap(), 41);
    drop(services);
    assert!(current.service::<u32>().is_none());
    assert_eq!(current.label(), "trusted-main");
    drop(session);
    assert!(current.session.upgrade().is_none());
  }

  #[tokio::test]
  async fn context_commands_receive_host_identity_and_services_without_payload_identity_handoff() {
    let services = Arc::new(ServiceStore::new());
    services.register_service(|| Arc::new(40_u32));
    let mut commands = Commands::default();
    commands.add_with_context("sync", |context, input: serde_json::Value| {
      Ok((
        context.label().to_owned(),
        context.window_scope(),
        *context.require_service::<u32>()? + input["increment"].as_u64().unwrap() as u32,
      ))
    });
    commands.add_async_with_context("async", |context, input: u32| async move {
      tokio::task::yield_now().await;
      Ok((context.label().to_owned(), *context.require_service::<u32>()? + input))
    });
    commands.validate().unwrap();
    let session = BridgeSession::new();
    let tracker = TaskTracker::new();
    let handler = CommandHandler {
      commands: Arc::new(commands),
      tracker: tracker.clone(),
      runtime: tokio::runtime::Handle::current(),
      services: Arc::downgrade(&services),
      label: "host-source".into(),
      window_scope: 31,
      session,
      permissions: None,
    };
    let (reply, response) = reply_channel();
    handler
      .start(
        "sync",
        "sync-id".into(),
        TransferData::from_json(serde_json::json!({"label":"forged-source","scope":999,"increment":2})).unwrap(),
        reply,
      )
      .unwrap();
    assert_eq!(
      response.await.unwrap().unwrap().json::<(String, u64, u32)>().unwrap(),
      ("host-source".to_owned(), 31, 42)
    );
    let (reply, response) = reply_channel();
    handler
      .start("async", "async-id".into(), TransferData::from_json(2).unwrap(), reply)
      .unwrap();
    assert_eq!(
      response.await.unwrap().unwrap().json::<(String, u32)>().unwrap(),
      ("host-source".to_owned(), 42)
    );
    tracker.wait_idle().await;
    assert_eq!(Arc::strong_count(&services), 1);
  }

  #[tokio::test]
  async fn context_callbacks_keep_denial_input_cancellation_and_window_scope_guards() {
    use std::sync::atomic::AtomicUsize;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let mut commands = Commands::default();
    commands.add_async_with_context("checked", move |_, input: u32| {
      observed.fetch_add(1, Ordering::AcqRel);
      async move { Ok(input) }
    });
    let principal = Principal::new(9, "trusted-page").unwrap();
    let permissions = Capabilities::default();
    let session = BridgeSession::new();
    let tracker = TaskTracker::new();
    let handler = CommandHandler {
      commands: Arc::new(commands),
      tracker: tracker.clone(),
      runtime: tokio::runtime::Handle::current(),
      services: Weak::new(),
      label: "checked-window".into(),
      window_scope: 11,
      session: session.clone(),
      permissions: Some((principal.clone(), permissions.clone())),
    };
    let (reply, _) = reply_channel();
    assert!(
      handler
        .start("checked", "denied".into(), TransferData::from_json(1).unwrap(), reply)
        .unwrap_err()
        .to_string()
        .contains("DENIED")
    );
    permissions.grant(&principal, "checked").unwrap();
    let (reply, response) = reply_channel();
    handler
      .start(
        "checked",
        "invalid".into(),
        TransferData::from_json("wrong type").unwrap(),
        reply,
      )
      .unwrap();
    assert!(
      response
        .await
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("INVALID_INPUT")
    );
    tracker.wait_idle().await;
    assert_eq!(calls.load(Ordering::Acquire), 0);

    let dropped = Arc::new(AtomicBool::new(false));
    struct DropGuard(Arc<AtomicBool>);
    impl Drop for DropGuard {
      fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
      }
    }
    let (started, ready) = tokio::sync::oneshot::channel();
    let started = Arc::new(Mutex::new(Some(started)));
    let marker = dropped.clone();
    let mut commands = Commands::default();
    commands.add_async_with_context("pending", move |context, _: ()| {
      let marker = marker.clone();
      let started = started.clone();
      async move {
        let _guard = DropGuard(marker);
        assert_eq!(context.label(), "pending-window");
        let _ = started.lock().unwrap().take().unwrap().send(());
        std::future::pending::<()>().await;
        Ok(())
      }
    });
    let handler = CommandHandler {
      commands: Arc::new(commands),
      tracker: tracker.clone(),
      runtime: tokio::runtime::Handle::current(),
      services: Weak::new(),
      label: "pending-window".into(),
      window_scope: 12,
      session,
      permissions: None,
    };
    let (other_done, other_result) = tokio::sync::oneshot::channel();
    let (release_other, other_wait) = tokio::sync::oneshot::channel();
    tracker
      .spawn(&handler.runtime, TaskScope::Window(13), async move {
        other_wait.await.unwrap();
        let _ = other_done.send(42);
      })
      .unwrap();
    let (reply, response) = reply_channel();
    handler
      .start(
        "pending",
        "pending-id".into(),
        TransferData::from_json(()).unwrap(),
        reply,
      )
      .unwrap();
    ready.await.unwrap();
    tracker.cancel_scope(&TaskScope::Window(12));
    assert!(response.await.unwrap().unwrap_err().to_string().contains("CANCELLED"));
    release_other.send(()).unwrap();
    tracker.wait_idle().await;
    assert!(dropped.load(Ordering::Acquire));
    assert_eq!(other_result.await.unwrap(), 42);
    assert_eq!(handler.session.slots.available_permits(), 64);
  }

  #[tokio::test]
  async fn context_handler_deadline_releases_future_and_returns_stable_error() {
    let dropped = Arc::new(AtomicBool::new(false));
    struct DropGuard(Arc<AtomicBool>);
    impl Drop for DropGuard {
      fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
      }
    }
    let marker = dropped.clone();
    let mut commands = Commands::default();
    commands.set_timeout(Duration::from_millis(5));
    commands.add_async_with_context("deadline", move |context, _: ()| {
      let marker = marker.clone();
      async move {
        let _guard = DropGuard(marker);
        assert_eq!(context.label(), "timed-window");
        std::future::pending::<()>().await;
        Ok(())
      }
    });
    let tracker = TaskTracker::new();
    let handler = CommandHandler {
      commands: Arc::new(commands),
      tracker: tracker.clone(),
      runtime: tokio::runtime::Handle::current(),
      services: Weak::new(),
      label: "timed-window".into(),
      window_scope: 21,
      session: BridgeSession::new(),
      permissions: None,
    };
    let (reply, response) = reply_channel();
    handler
      .start(
        "deadline",
        "timed-id".into(),
        TransferData::from_json(()).unwrap(),
        reply,
      )
      .unwrap();
    assert_eq!(response.await.unwrap().unwrap_err().to_string(), "LINTEL_TIMEOUT");
    tracker.wait_idle().await;
    assert!(dropped.load(Ordering::Acquire));
    assert!(handler.session.pending.lock().unwrap().is_empty());
    assert_eq!(handler.session.slots.available_permits(), 64);
  }

  #[test]
  fn managed_command_deadlines_keep_the_host_authority_and_reject_invalid_durations() {
    for timeout in [Duration::ZERO, Duration::from_secs(3601), Duration::MAX] {
      let mut commands = Commands::default();
      commands.set_timeout(timeout);
      assert!(commands.validate().is_err());
    }
    for timeout in [
      Duration::from_nanos(1),
      Duration::from_secs(20),
      Duration::from_secs(3600),
    ] {
      let mut commands = Commands::default();
      commands.set_timeout(timeout);
      commands.validate().unwrap();
      assert_eq!(commands.timeout, timeout);
    }
  }

  #[tokio::test]
  async fn managed_command_timeout_keeps_a_blocking_handler_slot_until_actual_work_ends() {
    let (started, ready) = tokio::sync::oneshot::channel();
    let started = Arc::new(Mutex::new(Some(started)));
    let (release, blocked) = std::sync::mpsc::channel();
    let blocked = Arc::new(Mutex::new(blocked));
    let mut commands = Commands::default();
    commands.set_timeout(Duration::from_millis(10));
    commands.add("blocking", move |_: ()| {
      started.lock().unwrap().take().unwrap().send(()).unwrap();
      blocked.lock().unwrap().recv_timeout(Duration::from_secs(3))?;
      Ok(())
    });
    let tracker = TaskTracker::new();
    let session = BridgeSession::new();
    let handler = CommandHandler {
      commands: Arc::new(commands),
      tracker: tracker.clone(),
      runtime: tokio::runtime::Handle::current(),
      services: Weak::new(),
      label: "blocking-window".into(),
      window_scope: 41,
      session: session.clone(),
      permissions: None,
    };
    let (reply, response) = reply_channel();
    handler
      .start(
        "blocking",
        "blocking-id".into(),
        TransferData::from_json(()).unwrap(),
        reply,
      )
      .unwrap();
    ready.await.unwrap();
    assert_eq!(response.await.unwrap().unwrap_err().to_string(), "LINTEL_TIMEOUT");
    tracker.wait_idle().await;
    // 已结束本地等待不代表不可抢占的同步工作已完成。
    assert_eq!(session.slots.available_permits(), 63);
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
      while session.slots.available_permits() != 64 {
        tokio::task::yield_now().await;
      }
    })
    .await
    .unwrap();
    assert!(session.pending.lock().unwrap().is_empty());
  }

  #[tokio::test]
  async fn renderer_reset_rejects_old_blocking_reply_and_allows_reused_id_in_new_document() {
    let (started, ready) = tokio::sync::oneshot::channel();
    let started = Arc::new(Mutex::new(Some(started)));
    let (release, blocked) = std::sync::mpsc::channel();
    let blocked = Arc::new(Mutex::new(blocked));
    let completed = Arc::new(AtomicBool::new(false));
    let finished = completed.clone();
    let mut commands = Commands::default();
    commands.add("echo", move |value: String| {
      if value == "old-document" {
        if let Some(started) = started.lock().unwrap().take() {
          let _ = started.send(());
        }
        blocked.lock().unwrap().recv_timeout(Duration::from_secs(3))?;
        finished.store(true, Ordering::Release);
      }
      Ok(value)
    });
    let session = BridgeSession::new();
    let tracker = TaskTracker::new();
    let handler = CommandHandler {
      commands: Arc::new(commands),
      tracker: tracker.clone(),
      runtime: tokio::runtime::Handle::current(),
      services: Weak::new(),
      label: "test".into(),
      window_scope: 7,
      session: session.clone(),
      permissions: None,
    };
    let (reply, old_reply) = reply_channel();
    handler
      .start(
        "echo",
        "reused-after-crash".into(),
        TransferData::from_json("old-document").unwrap(),
        reply,
      )
      .unwrap();
    tokio::time::timeout(Duration::from_secs(2), ready)
      .await
      .unwrap()
      .unwrap();
    session.invalidate(); // Same operation invoked by the real renderer-terminated callback.
    assert!(old_reply.await.unwrap().unwrap_err().to_string().contains("CANCELLED"));
    let (reply, new_reply) = reply_channel();
    handler
      .start(
        "echo",
        "reused-after-crash".into(),
        TransferData::from_json("new-document").unwrap(),
        reply,
      )
      .unwrap();
    assert_eq!(
      new_reply.await.unwrap().unwrap().json::<String>().unwrap(),
      "new-document"
    );
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
      while !completed.load(Ordering::Acquire) || session.slots.available_permits() != 64 {
        tokio::task::yield_now().await;
      }
    })
    .await
    .unwrap();
    tracker.wait_idle().await;
    assert!(session.pending.lock().unwrap().is_empty());
  }

  #[tokio::test]
  async fn commands_round_trip_typed_json_and_reject_unknown_or_invalid_input() {
    let mut commands = Commands::default();
    commands.add("increment", |value: u32| Ok(value + 1));
    commands.add_async("double", |value: u32| async move { Ok(value * 2) });
    commands.validate().unwrap();
    let reply = commands
      .dispatch("increment", TransferData::from_json(41).unwrap())
      .unwrap()
      .await
      .unwrap();
    assert_eq!(reply.json::<u32>().unwrap(), 42);
    let reply = commands
      .dispatch("double", TransferData::from_json(21).unwrap())
      .unwrap()
      .await
      .unwrap();
    assert_eq!(reply.json::<u32>().unwrap(), 42);
    assert!(commands.dispatch("unknown", TransferData::default()).is_err());
    assert!(
      commands
        .dispatch("increment", TransferData::from_json("wrong type").unwrap())
        .unwrap()
        .await
        .is_err()
    );
  }

  #[test]
  fn duplicate_reserved_commands_and_unauthorized_subjects_are_rejected() {
    let mut commands = Commands::default();
    commands.add("echo", |value: String| Ok(value));
    commands.add("echo", |value: String| Ok(value));
    assert!(commands.validate().is_err());
    let mut commands = Commands::default();
    commands.add("__lintel.cancel", |_: ()| Ok(()));
    assert!(commands.validate().is_err());
    let mut commands = Commands::default();
    commands.add("read", |value: String| Ok(value));
    let principal = Principal::new(1, "page").unwrap();
    let permissions = Capabilities::default();
    commands.set_permissions(principal.clone(), permissions.clone());
    assert!(commands.dispatch("read", TransferData::default()).is_err());
    permissions.grant(&principal, "read").unwrap();
    assert!(commands.dispatch("read", TransferData::default()).is_ok());
    let session = BridgeSession::new();
    session.invalidate();
    assert_eq!(session.generation.load(Ordering::Acquire), 1);
    session.close();
    assert!(session.closed.load(Ordering::Acquire));
  }

  fn reply_channel() -> (Reply, tokio::sync::oneshot::Receiver<Result<TransferData>>) {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let reply = Reply(Arc::new(Mutex::new(Some(Box::new(move |result| {
      let _ = sender.send(result);
    })))));
    (reply, receiver)
  }

  #[tokio::test]
  async fn abort_and_navigation_cancel_native_work_and_release_capacity() {
    let mut commands = Commands::default();
    commands.add_async("wait", |_: ()| async {
      std::future::pending::<()>().await;
      Ok(())
    });
    let session = BridgeSession::new();
    let tracker = TaskTracker::new();
    let handler = CommandHandler {
      commands: Arc::new(commands),
      tracker: tracker.clone(),
      runtime: tokio::runtime::Handle::current(),
      services: Weak::new(),
      label: "test".into(),
      window_scope: 1,
      session: Arc::clone(&session),
      permissions: None,
    };
    let (reply, response) = reply_channel();
    handler
      .start("wait", "first".into(), TransferData::from_json(()).unwrap(), reply)
      .unwrap();
    let (reply, _) = reply_channel();
    assert!(
      handler
        .start("wait", "first".into(), TransferData::from_json(()).unwrap(), reply)
        .is_err()
    );
    let (reply, _) = reply_channel();
    handler
      .start(
        "__lintel.cancel",
        "cancel".into(),
        TransferData::from_json(serde_json::json!({"id": "first"})).unwrap(),
        reply,
      )
      .unwrap();
    assert!(response.await.unwrap().unwrap_err().to_string().contains("CANCELLED"));
    tracker.wait_idle().await;
    assert_eq!(session.slots.available_permits(), 64);
    assert!(session.pending.lock().unwrap().is_empty());
    let (reply, response) = reply_channel();
    handler
      .start("wait", "second".into(), TransferData::from_json(()).unwrap(), reply)
      .unwrap();
    session.invalidate();
    assert!(response.await.unwrap().is_err());
    tracker.wait_idle().await;
    assert_eq!(session.slots.available_permits(), 64);
    session.close();
    let (reply, _) = reply_channel();
    assert!(
      handler
        .start("wait", "third".into(), TransferData::from_json(()).unwrap(), reply)
        .is_err()
    );
  }

  #[tokio::test]
  async fn old_request_cleanup_cannot_remove_reused_request_identity() {
    let mut commands = Commands::default();
    commands.add_async("wait", |_: ()| async {
      std::future::pending::<()>().await;
      Ok(())
    });
    let session = BridgeSession::new();
    let tracker = TaskTracker::new();
    let handler = CommandHandler {
      commands: Arc::new(commands),
      tracker: tracker.clone(),
      runtime: tokio::runtime::Handle::current(),
      services: Weak::new(),
      label: "test".into(),
      window_scope: 4,
      session: Arc::clone(&session),
      permissions: None,
    };
    let (reply, _) = reply_channel();
    handler
      .start("wait", "reused".into(), TransferData::from_json(()).unwrap(), reply)
      .unwrap();
    session.cancel("reused");
    let (reply, receiver) = reply_channel();
    handler
      .start("wait", "reused".into(), TransferData::from_json(()).unwrap(), reply)
      .unwrap();
    tokio::task::yield_now().await;
    assert!(session.pending.lock().unwrap().contains_key("reused"));
    session.cancel("reused");
    assert!(receiver.await.unwrap().is_err());
    tracker.wait_idle().await;
    assert!(session.pending.lock().unwrap().is_empty());
    assert_eq!(session.slots.available_permits(), 64);
  }
}
