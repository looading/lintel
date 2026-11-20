use std::{
  any::Any,
  fmt::Debug,
  hash::Hash,
  sync::{Arc, Mutex},
  time::Duration,
};

use crate::{
  IcedAppPluginApi, Result,
  core::{
    context::{AppContext, AppContextApi},
    tasks::TaskScope,
  },
  iced::{AnyMessage, Element, IcedApplication, ProgramId, ProgramIdOfMessage, Subscription, Task, window as native},
  runtime::{AppBuilder, PluginCreator},
  window::{Command, WindowError, WindowEvent, WindowId, WindowManager, WindowStatus},
};

type EventCallback<Message> = Arc<dyn Fn(WindowEvent) -> Message + Send + Sync>;
type ClosePolicy<State> = Arc<dyn Fn(&State, WindowId) -> bool + Send + Sync>;
type StateSubscription<State, Message> = Arc<dyn Fn(&State) -> Subscription<Message> + Send + Sync>;

/// Builds an application with shared state and an independently routed view for each window.
///
/// Store the supplied `WindowManager` in state to open or close windows from normal
/// update callbacks. Both boot and update return ordinary iced tasks.
pub fn multi_window<State, Message, Boot, Update, View>(
  boot: Boot,
  update: Update,
  view: View,
) -> MultiWindow<State, Message, Boot, Update, View>
where
  Boot: Fn(WindowManager) -> (State, Task<Message>),
  Update: Fn(&mut State, Message) -> Task<Message>,
  View: for<'a> Fn(&'a State, WindowId) -> Element<'a, Message>,
{
  MultiWindow::new(boot, update, view)
}

/// Boots shared state after plugins/services are started, without global context lookup.
pub fn multi_window_with_context<State, Message, Boot, Update, View>(
  boot: Boot,
  update: Update,
  view: View,
) -> MultiWindow<State, Message, ContextBoot<Boot>, Update, View>
where
  Boot: Fn(&AppContext, WindowManager) -> Result<(State, Task<Message>)>,
  Update: Fn(&mut State, Message) -> Task<Message>,
  View: for<'a> Fn(&'a State, WindowId) -> Element<'a, Message>,
{
  MultiWindow::new(ContextBoot(boot), update, view)
}
#[doc(hidden)]
pub struct ContextBoot<F>(F);
impl<F> ContextBoot<F> {
  fn call<C, State, Message>(&self, context: &C, windows: WindowManager) -> Result<(State, Task<Message>)>
  where
    F: Fn(&C, WindowManager) -> Result<(State, Task<Message>)>,
  {
    (self.0)(context, windows)
  }
}
/// Boot contract used by generic MultiWindow configurations.
#[doc(hidden)]
pub trait MultiWindowBoot<State, Message> {
  fn boot(&self, context: &AppContext, windows: WindowManager) -> Result<(State, Task<Message>)>;
}
impl<State, Message, F: Fn(WindowManager) -> (State, Task<Message>)> MultiWindowBoot<State, Message> for F {
  fn boot(&self, _: &AppContext, windows: WindowManager) -> Result<(State, Task<Message>)> {
    Ok(self(windows))
  }
}
impl<State, Message, F: Fn(&AppContext, WindowManager) -> Result<(State, Task<Message>)>>
  MultiWindowBoot<State, Message> for ContextBoot<F>
{
  fn boot(&self, context: &AppContext, windows: WindowManager) -> Result<(State, Task<Message>)> {
    self.call(context, windows)
  }
}

/// Configuration for a native application with any number of managed windows.
pub struct MultiWindow<State, Message, Boot, Update, View> {
  boot: Boot,
  update: Update,
  view: View,
  id: Option<String>,
  title: &'static str,
  plugins: Vec<PluginCreator>,
  #[cfg(feature = "multiprocess")]
  processes: Option<crate::process::HostConfig>,
  on_event: Option<EventCallback<Message>>,
  close_policy: ClosePolicy<State>,
  subscription: StateSubscription<State, Message>,
  exit_on_last_window: bool,
  open_timeout: Duration,
  activation: Option<Arc<dyn Fn(crate::runtime::ActivationEvent) -> Message + Send + Sync>>,
  #[cfg(feature = "crash-reporting")]
  crash: Option<lintel_crash::CrashConfig>,
  #[cfg(feature = "crash-reports")]
  reports: Option<crate::crash_reports::CrashReportsConfig>,
  #[cfg(feature = "desktop")]
  desktop: bool,
  #[cfg(all(feature = "desktop", feature = "public-iced"))]
  accessibility: bool,
  #[cfg(feature = "desktop")]
  activation_options: crate::activation::ActivationOptions,
  #[cfg(feature = "desktop")]
  desktop_event: Option<Arc<dyn Fn(lintel_desktop::DesktopEvent) -> Message + Send + Sync>>,
}

impl<State, Message, Boot, Update, View> MultiWindow<State, Message, Boot, Update, View> {
  fn new(boot: Boot, update: Update, view: View) -> Self {
    Self {
      boot,
      update,
      view,
      id: None,
      title: "Lintel",
      plugins: Vec::new(),
      #[cfg(feature = "multiprocess")]
      processes: None,
      on_event: None,
      close_policy: Arc::new(|_, _| true),
      subscription: Arc::new(|_| Subscription::none()),
      exit_on_last_window: true,
      open_timeout: Duration::from_secs(15),
      activation: None,
      #[cfg(feature = "crash-reporting")]
      crash: None,
      #[cfg(feature = "crash-reports")]
      reports: None,
      #[cfg(feature = "desktop")]
      desktop: false,
      #[cfg(all(feature = "desktop", feature = "public-iced"))]
      accessibility: false,
      #[cfg(feature = "desktop")]
      activation_options: Default::default(),
      #[cfg(feature = "desktop")]
      desktop_event: None,
    }
  }
}

impl<State, Message, Boot, Update, View> MultiWindow<State, Message, Boot, Update, View> {
  /// Publishes native semantics from the optional accessible-widget helpers.
  #[cfg(all(feature = "desktop", feature = "public-iced"))]
  pub fn accessibility(mut self) -> Self {
    self.accessibility = true;
    self
  }

  pub fn id(mut self, id: impl Into<String>) -> Self {
    self.id = Some(id.into());
    self
  }

  /// Sets the application name. Each window gets its own title when opened.
  pub fn title(mut self, title: &'static str) -> Self {
    self.title = title;
    self
  }

  /// 装配共享的 Mojo 服务进程，窗口通过上下文取得宿主句柄。
  #[cfg(feature = "multiprocess")]
  pub fn processes(mut self, config: crate::process::HostConfig) -> Self {
    self.processes = Some(config);
    self
  }

  pub fn plugin(mut self, creator: impl FnOnce(&mut AppContext) + Send + Sync + 'static) -> Self {
    self.plugins.push(Box::new(creator));
    self
  }

  /// Enables local native minidumps using the bundled out-of-process helper.
  #[cfg(feature = "crash-reporting")]
  pub fn crash_reporting(mut self, config: lintel_crash::CrashConfig) -> Self {
    self.crash = Some(config);
    self
  }

  /// Configures capture and the optional application crash report service.
  #[cfg(feature = "crash-reports")]
  pub fn crash_reports(mut self, config: crate::crash_reports::CrashReportsConfig) -> Self {
    self.reports = Some(config);
    self
  }

  /// Converts native lifecycle events to ordinary application messages.
  pub fn on_window_event(mut self, callback: impl Fn(WindowEvent) -> Message + Send + Sync + 'static) -> Self {
    self.on_event = Some(Arc::new(callback));
    self
  }

  /// Returns `false` to veto a native close button or `WindowManager::close` request.
  /// Use `WindowManager::destroy` after an asynchronous confirmation is accepted.
  pub fn close_request(mut self, policy: impl Fn(&State, WindowId) -> bool + Send + Sync + 'static) -> Self {
    self.close_policy = Arc::new(policy);
    self
  }

  pub fn subscription(
    mut self,
    subscription: impl Fn(&State) -> Subscription<Message> + Send + Sync + 'static,
  ) -> Self {
    self.subscription = Arc::new(subscription);
    self
  }

  /// Defaults to true. Disable for tray applications that may reopen their last window.
  pub fn exit_on_last_window(mut self, exit: bool) -> Self {
    self.exit_on_last_window = exit;
    self
  }

  /// Reports a failed open if the native runner never acknowledges creation.
  pub fn open_timeout(mut self, timeout: Duration) -> Self {
    self.open_timeout = timeout;
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
    callback: impl Fn(lintel_desktop::DesktopEvent) -> Message + Send + Sync + 'static,
  ) -> Self {
    self.desktop = true;
    self.desktop_event = Some(Arc::new(callback));
    self
  }

  pub fn on_activation(
    mut self,
    callback: impl Fn(crate::runtime::ActivationEvent) -> Message + Send + Sync + 'static,
  ) -> Self {
    self.activation = Some(Arc::new(callback));
    self
  }

  pub fn run(self) -> Result<()>
  where
    State: Send + Sync + 'static,
    Message: Clone + Debug + Send + Sync + 'static,
    Boot: MultiWindowBoot<State, Message> + Send + Sync + 'static,
    Update: Fn(&mut State, Message) -> Task<Message> + Send + Sync + 'static,
    View: for<'a> Fn(&'a State, WindowId) -> Element<'a, Message> + Send + Sync + 'static,
  {
    let Self {
      boot,
      update,
      view,
      id,
      title,
      plugins,
      #[cfg(feature = "multiprocess")]
      processes,
      on_event,
      close_policy,
      subscription,
      exit_on_last_window,
      open_timeout,
      activation,
      #[cfg(feature = "crash-reporting")]
      crash,
      #[cfg(feature = "crash-reports")]
      reports,
      #[cfg(feature = "desktop")]
      desktop,
      #[cfg(all(feature = "desktop", feature = "public-iced"))]
      accessibility,
      #[cfg(feature = "desktop")]
      activation_options,
      #[cfg(feature = "desktop")]
      desktop_event,
    } = self;
    let id = id.ok_or_else(|| anyhow::anyhow!("set the application ID with .id(\"dev.example.app\")"))?;
    let callbacks = Arc::new((update, view));
    let mut app = AppBuilder::new(title).with_app_id(id).with_plugins(plugins);
    #[cfg(feature = "multiprocess")]
    if let Some(config) = processes {
      app = app.processes(config);
    }
    #[cfg(all(feature = "desktop", feature = "public-iced"))]
    if accessibility {
      app = app.with_accessibility();
    }
    #[cfg(feature = "crash-reporting")]
    if let Some(config) = crash {
      app = app.crash_reporting(config);
    }
    #[cfg(feature = "crash-reports")]
    if let Some(config) = reports {
      app = app.crash_reports(config);
    }
    let program_id = NativeWindows::<State, Message, Update, View>::id();
    if let Some(callback) = activation {
      app = app.with_activation_handler(move |context, event| {
        if let Err(error) = crate::dispatch_program_message(context, route(program_id, Event::Update(callback(event))))
        {
          tracing::error!(?error, "cannot deliver application activation");
        }
      });
    }
    #[cfg(feature = "desktop")]
    {
      app = app.with_activation_options(activation_options);
      if desktop {
        app = app.with_desktop();
      }
      if let Some(callback) = desktop_event {
        app = app.with_desktop_event_handler(move |context, event| {
          if let Err(error) =
            crate::dispatch_program_message(context, route(program_id, Event::Update(callback(event))))
          {
            tracing::error!(?error, "cannot deliver native desktop event");
          }
        });
      }
    }
    app.try_run(move |context| {
      let (windows, rx) = WindowManager::channel();
      #[cfg(feature = "desktop")]
      windows.install_desktop(context.get_service::<lintel_desktop::DesktopService>());
      let boot_windows = windows.clone();
      crate::runtime::initialize_then(
        || boot.boot(context, boot_windows),
        |(state, task)| {
          let iced = context
            .get_service::<dyn IcedAppPluginApi>()
            .ok_or_else(|| anyhow::anyhow!("iced plugin service is unavailable"))?;
          iced
            .try_create(Box::new(NativeWindows {
              state,
              callbacks: callbacks.clone(),
              windows,
              rx: Arc::new(Mutex::new(Some(rx))),
              initial_task: Mutex::new(Some(task)),
              lifecycle: Lifecycle::new(context),
              on_event: on_event.clone(),
              close_policy: close_policy.clone(),
              subscription: subscription.clone(),
              exit_on_last_window,
              open_timeout,
            }))
            .map_err(Into::into)
        },
      )
    })
  }
}

struct NativeWindows<State, Message, Update, View> {
  state: State,
  callbacks: Arc<(Update, View)>,
  windows: WindowManager,
  rx: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<Command>>>>,
  initial_task: Mutex<Option<Task<Message>>>,
  lifecycle: Lifecycle,
  on_event: Option<EventCallback<Message>>,
  close_policy: ClosePolicy<State>,
  subscription: StateSubscription<State, Message>,
  exit_on_last_window: bool,
  open_timeout: Duration,
}

struct Lifecycle {
  quit: Arc<dyn Fn() + Send + Sync>,
  cancel_window_tasks: Arc<dyn Fn(u64) + Send + Sync>,
}

impl Lifecycle {
  fn new(context: &AppContext) -> Self {
    let quit_context = context.clone();
    let cancel_context = context.clone();
    Self {
      quit: Arc::new(move || {
        if let Err(error) = quit_context.quit() {
          tracing::error!(?error, "cannot request application quit");
        }
      }),
      cancel_window_tasks: Arc::new(move |id| {
        cancel_context.cancel_tasks(&TaskScope::Window(id));
      }),
    }
  }
}

#[derive(Debug)]
enum Event<Message> {
  Update(Message),
  Command(Mutex<Option<Command>>),
  Opened(WindowId),
  OpenTimedOut(WindowId),
  Native(WindowId, native::Event),
}

#[derive(Debug)]
struct Routed<Message> {
  program_id: ProgramId,
  event: Event<Message>,
}

impl<Message> ProgramIdOfMessage for Routed<Message> {
  fn program_id(&self) -> ProgramId {
    self.program_id
  }
}

fn route<Message: Debug + Send + Sync + 'static>(program_id: ProgramId, event: Event<Message>) -> Arc<dyn AnyMessage> {
  Arc::new(Routed { program_id, event })
}

impl<State, Message, Update, View> NativeWindows<State, Message, Update, View>
where
  State: Send + Sync + 'static,
  Message: Clone + Debug + Send + Sync + 'static,
  Update: Fn(&mut State, Message) -> Task<Message> + Send + Sync + 'static,
  View: for<'a> Fn(&'a State, WindowId) -> Element<'a, Message> + Send + Sync + 'static,
{
  fn update_message(&mut self, message: Message) -> Task<Arc<dyn AnyMessage>> {
    let program_id = self.__id();
    (self.callbacks.0)(&mut self.state, message).map(move |message| route(program_id, Event::Update(message)))
  }

  fn emit(&mut self, event: WindowEvent) -> Task<Arc<dyn AnyMessage>> {
    match self.on_event.as_ref().map(|callback| callback(event)) {
      Some(message) => self.update_message(message),
      None => Task::none(),
    }
  }

  fn request_quit(&self) {
    (self.lifecycle.quit)();
  }

  fn close(&mut self, id: WindowId, force: bool) -> Task<Arc<dyn AnyMessage>> {
    if !self.windows.contains(id) || self.windows.status(id) == Some(WindowStatus::Closing) {
      return Task::none();
    }
    let notification = self.emit(WindowEvent::CloseRequested(id));
    if !force && !(self.close_policy)(&self.state, id) {
      return Task::batch([notification, self.emit(WindowEvent::CloseVetoed(id))]);
    }
    self.windows.change(id, |info| info.status = WindowStatus::Closing);
    Task::batch([notification, native::close(id)])
  }

  fn opened(&mut self, id: WindowId) -> Task<Arc<dyn AnyMessage>> {
    // Native Opened and the open task may both acknowledge the same ID.
    if !self.windows.contains(id) {
      // A late native acknowledgement after timeout must not leave an orphan window.
      return native::close(id);
    }
    let Some(status) = self.windows.mark_opened(id) else {
      return Task::none();
    };
    Task::batch([
      self.emit(WindowEvent::Opened(id)),
      if status == WindowStatus::Closing {
        native::close(id)
      } else {
        Task::none()
      },
    ])
  }

  fn closed(&mut self, id: WindowId, failed: bool) -> Task<Arc<dyn AnyMessage>> {
    let Some(info) = self.windows.remove(id) else {
      return Task::none();
    };
    (self.lifecycle.cancel_window_tasks)(info.scope_id);
    let task = if failed {
      self.emit(WindowEvent::OpenFailed {
        id,
        error: WindowError::OpenTimedOut,
      })
    } else {
      self.emit(WindowEvent::Closed(id))
    };
    // Event handlers can synchronously open replacement windows before this decision.
    if self.exit_on_last_window && self.windows.ids().is_empty() {
      self.request_quit();
    }
    task
  }

  fn command(&mut self, command: Command) -> Task<Arc<dyn AnyMessage>> {
    match command {
      Command::Open { id, task } if self.windows.contains(id) => {
        let program_id = self.__id();
        let timeout = self.open_timeout;
        Task::batch([
          task.map(move |id| route::<Message>(program_id, Event::Opened(id))),
          Task::perform(
            async move {
              tokio::time::sleep(timeout).await;
              id
            },
            move |id| route::<Message>(program_id, Event::OpenTimedOut(id)),
          ),
        ])
      }
      Command::Close { id, force } => self.close(id, force),
      Command::Title { id, title } => {
        if !self.windows.change(id, |info| info.title = title.clone()) {
          return Task::none();
        }
        Task::none()
      }
      Command::Operate { id, operation } => {
        if self.windows.contains(id) {
          operation.task(id)
        } else {
          Task::none()
        }
      }
      Command::Quit => {
        self.request_quit();
        Task::none()
      }
      Command::Open { .. } => Task::none(),
    }
  }
}

impl<State, Message, Update, View> IcedApplication for NativeWindows<State, Message, Update, View>
where
  State: Send + Sync + 'static,
  Message: Clone + Debug + Send + Sync + 'static,
  Update: Fn(&mut State, Message) -> Task<Message> + Send + Sync + 'static,
  View: for<'a> Fn(&'a State, WindowId) -> Element<'a, Message> + Send + Sync + 'static,
{
  fn boot(&mut self) -> Task<Arc<dyn AnyMessage>> {
    let id = self.__id();
    self
      .initial_task
      .lock()
      .unwrap()
      .take()
      .unwrap_or_else(Task::none)
      .map(move |message| route(id, Event::Update(message)))
  }

  fn window_id(&self) -> Option<WindowId> {
    self.windows.ids().first().copied()
  }
  fn window_ids(&self) -> Vec<WindowId> {
    self.windows.ids()
  }
  fn owns_window(&self, id: WindowId) -> bool {
    self.windows.contains(id)
  }
  fn window_title_for(&self, id: WindowId) -> Option<String> {
    self.windows.title(id)
  }

  fn view(&self, id: WindowId) -> Element<'_, Arc<dyn AnyMessage>> {
    let program_id = self.__id();
    (self.callbacks.1)(&self.state, id).map(move |message| route(program_id, Event::Update(message)))
  }

  fn update(&mut self, message: Arc<dyn AnyMessage>) -> Task<Arc<dyn AnyMessage>> {
    let any: &dyn Any = message.as_ref();
    let Some(message) = any.downcast_ref::<Routed<Message>>() else {
      return Task::none();
    };
    match &message.event {
      Event::Update(message) => self.update_message(message.clone()),
      Event::Command(command) => command
        .lock()
        .unwrap()
        .take()
        .map_or_else(Task::none, |command| self.command(command)),
      Event::Opened(id) => self.opened(*id),
      Event::OpenTimedOut(id) if self.windows.awaiting_open(*id) => {
        Task::batch([self.closed(*id, true), native::close(*id)])
      }
      Event::Native(id, event) if self.windows.contains(*id) => match event {
        native::Event::Opened { .. } => self.opened(*id),
        native::Event::CloseRequested => self.close(*id, false),
        native::Event::Closed => self.closed(*id, false),
        event => self.emit(WindowEvent::Native {
          id: *id,
          event: event.clone(),
        }),
      },
      _ => Task::none(),
    }
  }

  fn subscription(&self) -> Subscription<Arc<dyn AnyMessage>> {
    let id = self.__id();
    Subscription::batch([
      crate::iced::advanced::subscription::from_recipe(CommandStream::<Message> {
        id,
        rx: self.rx.clone(),
        marker: std::marker::PhantomData,
      }),
      native::events()
        .with(id)
        .map(|(program_id, (id, event))| route::<Message>(program_id, Event::Native(id, event))),
      (self.subscription)(&self.state)
        .with(id)
        .map(|(id, message)| route(id, Event::Update(message))),
    ])
  }
}

struct CommandStream<Message> {
  id: ProgramId,
  rx: Arc<Mutex<Option<tokio::sync::mpsc::Receiver<Command>>>>,
  marker: std::marker::PhantomData<fn() -> Message>,
}

impl<Message: Debug + Send + Sync + 'static> crate::iced::advanced::subscription::Recipe for CommandStream<Message> {
  type Output = Arc<dyn AnyMessage>;

  fn hash(&self, state: &mut crate::iced::advanced::subscription::Hasher) {
    std::any::TypeId::of::<Self>().hash(state);
    self.id.hash(state);
  }

  fn stream(
    self: Box<Self>,
    _input: crate::iced::futures::stream::BoxStream<'static, crate::iced::advanced::subscription::Event>,
  ) -> crate::iced::futures::stream::BoxStream<'static, Self::Output> {
    let rx = self
      .rx
      .lock()
      .unwrap()
      .take()
      .expect("window command receiver must be consumed once");
    let id = self.id;
    Box::pin(crate::iced::futures::stream::unfold(rx, move |mut rx| async move {
      rx.recv()
        .await
        .map(|command| (route::<Message>(id, Event::Command(Mutex::new(Some(command)))), rx))
    }))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
  };

  #[derive(Clone, Debug)]
  enum Message {
    Increment(WindowId),
    Lifecycle(WindowEvent),
  }

  struct State {
    counts: BTreeMap<WindowId, usize>,
    events: Vec<WindowEvent>,
    rendered: Mutex<Vec<WindowId>>,
  }

  fn update(state: &mut State, message: Message) -> Task<Message> {
    match message {
      Message::Increment(id) => *state.counts.entry(id).or_default() += 1,
      Message::Lifecycle(event) => state.events.push(event),
    }
    Task::none()
  }

  fn view(state: &State, id: WindowId) -> Element<'_, Message> {
    state.rendered.lock().unwrap().push(id);
    // The element borrows state: this guards the normal iced view lifetime contract.
    crate::iced::widget::text(if state.counts.contains_key(&id) {
      "owned"
    } else {
      "unknown"
    })
    .into()
  }

  type TestApp = NativeWindows<
    State,
    Message,
    fn(&mut State, Message) -> Task<Message>,
    for<'a> fn(&'a State, WindowId) -> Element<'a, Message>,
  >;

  fn app() -> (TestApp, Arc<AtomicUsize>, Arc<Mutex<Vec<u64>>>) {
    let (windows, rx) = WindowManager::channel();
    let quits = Arc::new(AtomicUsize::new(0));
    let cancellations = Arc::new(Mutex::new(Vec::new()));
    let quit_count = quits.clone();
    let cancelled = cancellations.clone();
    (
      NativeWindows {
        state: State {
          counts: BTreeMap::new(),
          events: Vec::new(),
          rendered: Mutex::new(Vec::new()),
        },
        callbacks: Arc::new((
          update as fn(&mut State, Message) -> Task<Message>,
          view as for<'a> fn(&'a State, WindowId) -> Element<'a, Message>,
        )),
        windows,
        rx: Arc::new(Mutex::new(Some(rx))),
        initial_task: Mutex::new(Some(Task::none())),
        lifecycle: Lifecycle {
          quit: Arc::new(move || {
            quit_count.fetch_add(1, Ordering::SeqCst);
          }),
          cancel_window_tasks: Arc::new(move |id| {
            cancelled.lock().unwrap().push(id);
          }),
        },
        on_event: Some(Arc::new(Message::Lifecycle)),
        close_policy: Arc::new(|_, _| true),
        subscription: Arc::new(|_| Subscription::none()),
        exit_on_last_window: true,
        open_timeout: Duration::from_secs(1),
      },
      quits,
      cancellations,
    )
  }

  #[test]
  fn both_windows_route_views_and_updates_to_their_own_state() {
    let (mut app, _, _) = app();
    let a = app.windows.open("A", native::Settings::default()).unwrap();
    let b = app.windows.open("B", native::Settings::default()).unwrap();
    let program = app.__id();
    let _ = app.update(route::<Message>(program, Event::Update(Message::Increment(a))));
    let _ = app.update(route::<Message>(program, Event::Update(Message::Increment(b))));
    let _ = app.update(route::<Message>(program, Event::Update(Message::Increment(b))));
    assert!(app.owns_window(a) && app.owns_window(b));
    assert_eq!(app.state.counts[&a], 1);
    assert_eq!(app.state.counts[&b], 2);
    let _ = app.view(a);
    let _ = app.view(b);
    assert_eq!(*app.state.rendered.lock().unwrap(), vec![a, b]);
    assert_eq!(app.window_title_for(b).as_deref(), Some("B"));
  }

  #[test]
  fn close_veto_and_last_window_exit_are_independent_of_siblings() {
    let (mut app, quits, cancellations) = app();
    let a = app.windows.open("A", native::Settings::default()).unwrap();
    let b = app.windows.open("B", native::Settings::default()).unwrap();
    let scope_a = app.windows.scope_id(a).unwrap();
    let scope_b = app.windows.scope_id(b).unwrap();
    let _ = app.opened(a);
    let _ = app.opened(b);
    app.close_policy = Arc::new(move |_, id| id != a);
    let _ = app.close(a, false);
    assert_eq!(app.windows.status(a), Some(WindowStatus::Open));
    assert!(
      app
        .state
        .events
        .iter()
        .any(|event| matches!(event, WindowEvent::CloseVetoed(id) if *id == a))
    );
    let _ = app.close(b, false);
    assert_eq!(app.windows.status(b), Some(WindowStatus::Closing));
    let _ = app.closed(b, false);
    assert!(app.owns_window(a));
    assert_eq!(quits.load(Ordering::SeqCst), 0);
    let _ = app.close(a, true);
    let _ = app.closed(a, false);
    let _ = app.closed(a, false);
    assert_eq!(quits.load(Ordering::SeqCst), 1);
    assert_eq!(*cancellations.lock().unwrap(), vec![scope_b, scope_a]);
  }

  #[test]
  fn timeout_and_late_commands_cannot_resurrect_a_failed_window() {
    let (mut app, quits, cancellations) = app();
    let a = app.windows.open("A", native::Settings::default()).unwrap();
    let b = app.windows.open("B", native::Settings::default()).unwrap();
    let scope = app.windows.scope_id(a).unwrap();
    let program = app.__id();
    let _ = app.update(route::<Message>(program, Event::OpenTimedOut(a)));
    assert!(!app.owns_window(a));
    assert!(app.owns_window(b));
    assert_eq!(quits.load(Ordering::SeqCst), 0);
    assert_eq!(*cancellations.lock().unwrap(), vec![scope]);
    assert!(
      app
        .state
        .events
        .iter()
        .any(|event| matches!(event, WindowEvent::OpenFailed { id, .. } if *id == a))
    );
    let _ = app.update(route::<Message>(program, Event::Opened(a)));
    let _ = app.command(Command::Title {
      id: a,
      title: "late".into(),
    });
    let _ = app.command(Command::Close { id: a, force: false });
    assert!(!app.owns_window(a));
    assert_eq!(app.windows.title(a), None);
  }

  #[test]
  fn close_during_open_is_replayed_after_acknowledgement_and_timeout_is_ignored() {
    let (mut app, _, _) = app();
    let id = app.windows.open("A", native::Settings::default()).unwrap();
    let _ = app.close(id, false);
    let _ = app.opened(id);
    assert_eq!(app.windows.status(id), Some(WindowStatus::Closing));
    assert!(!app.windows.awaiting_open(id));
    let _ = app.update(route::<Message>(app.__id(), Event::OpenTimedOut(id)));
    assert!(app.owns_window(id));
    let _ = app.opened(id);
    assert_eq!(
      app
        .state
        .events
        .iter()
        .filter(|event| matches!(event, WindowEvent::Opened(_)))
        .count(),
      1
    );
  }

  #[test]
  fn keep_alive_allows_reopen_and_explicit_quit() {
    let (mut app, quits, _) = app();
    app.exit_on_last_window = false;
    let a = app.windows.open("A", native::Settings::default()).unwrap();
    let _ = app.closed(a, false);
    assert_eq!(quits.load(Ordering::SeqCst), 0);
    let b = app.windows.open("B", native::Settings::default()).unwrap();
    assert_ne!(a, b);
    assert!(app.owns_window(b));
    let _ = app.command(Command::Quit);
    assert_eq!(quits.load(Ordering::SeqCst), 1);
  }

  #[test]
  fn native_operations_emit_effects_only_for_owned_window_ids() {
    use crate::window::Operation;
    let (mut app, _, _) = app();
    let id = app.windows.open("A", native::Settings::default()).unwrap();
    for operation in [
      Operation::Focus,
      Operation::Show,
      Operation::Hide,
      Operation::Resize(crate::iced::Size::new(500.0, 400.0)),
      Operation::Move(crate::iced::Point::new(-50.0, 20.0)),
      Operation::Minimize(true),
      Operation::Maximize(false),
    ] {
      assert_eq!(app.command(Command::Operate { id, operation }).units(), 1);
    }
    let _ = app.closed(id, false);
    assert_eq!(
      app
        .command(Command::Operate {
          id,
          operation: Operation::Show
        })
        .units(),
      0
    );
  }
  #[test]
  fn context_multi_boot_receives_services_manager_and_initial_task() {
    let services = crate::core::context::store::ServiceStore::new();
    services.register_service(|| Arc::new(7usize));
    let boot = ContextBoot(
      |context: &crate::core::context::store::ServiceStore, windows: WindowManager| -> Result<(WindowId, Task<u8>)> {
        let title = format!("service {}", context.get_service::<usize>().unwrap());
        Ok((windows.open(title, native::Settings::default())?, Task::none()))
      },
    );
    let (windows, mut receiver) = WindowManager::channel();
    let (id, _task) = boot.call(&services, windows.clone()).unwrap();
    assert_eq!(windows.status(id), Some(WindowStatus::Opening));
    assert!(matches!(receiver.try_recv().unwrap(),Command::Open{id:opened,..}if opened==id));
  }
  #[test]
  fn failed_context_multi_boot_drops_unadmitted_window_commands() {
    let (windows, receiver) = WindowManager::channel();
    let escaped = windows.clone();
    let admissions = Arc::new(AtomicUsize::new(0));
    let admitted = admissions.clone();
    let boot = ContextBoot(|_: &(), windows: WindowManager| -> Result<((), Task<u8>)> {
      windows.open("Never admitted", native::Settings::default())?;
      Err(anyhow::anyhow!("boot failed"))
    });
    let error = crate::runtime::initialize_then(
      || boot.call(&(), windows),
      move |_| {
        admitted.fetch_add(1, Ordering::SeqCst);
        drop(receiver);
        Ok(())
      },
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "boot failed");
    assert_eq!(admissions.load(Ordering::SeqCst), 0);
    assert_eq!(
      escaped.open("Too late", native::Settings::default()),
      Err(WindowError::ManagerClosed)
    );
  }
  #[test]
  fn context_multi_builder_infers_messages_and_missing_id_prevents_gui() {
    struct ContextState {
      label: String,
    }
    fn context_view(state: &ContextState, _: WindowId) -> Element<'_, u8> {
      crate::iced::widget::text(&state.label).into()
    }
    let app = multi_window_with_context(
      |_: &AppContext, _windows: WindowManager| -> Result<(ContextState, Task<u8>)> { panic!("boot must not run") },
      |_: &mut ContextState, _: u8| Task::none(),
      context_view,
    )
    .on_window_event(|_| 0u8)
    .subscription(|state| {
      let _ = &state.label;
      Subscription::none()
    })
    .on_activation(|_| 0u8);
    assert!(app.run().unwrap_err().to_string().contains("application ID"));
  }
}
