use std::{fmt::Debug, marker::PhantomData, sync::Arc};

use crate::{
  IcedAppPluginApi, Result,
  core::context::{AppContext, AppContextApi},
  iced::{AnyMessage, Element, IcedApplication, ProgramId, ProgramIdOfMessage, Subscription, Task, window},
  runtime::{AppBuilder, PluginCreator},
};

/// Builds a native single-window application with typed state and messages.
pub fn application<State, Message, Initialize, Update, View>(
  initialize: Initialize,
  update: Update,
  view: View,
) -> Application<Initialize, Update, View>
where
  Initialize: Fn() -> State,
  Update: Fn(&mut State, Message) -> Task<Message>,
  View: for<'a> Fn(&'a State) -> Element<'a, Message>,
{
  Application::new(initialize, update, view)
}

/// Builds state after plugin startup with the application's services and scoped task context.
pub fn application_with_context<State, Message, Initialize, Update, View>(
  initialize: Initialize,
  update: Update,
  view: View,
) -> Application<ContextInitialize<Initialize>, Update, View>
where
  Initialize: Fn(&AppContext) -> Result<State>,
  Update: Fn(&mut State, Message) -> Task<Message>,
  View: for<'a> Fn(&'a State) -> Element<'a, Message>,
{
  Application::new(ContextInitialize(initialize), update, view)
}

/// Internal adapter type; applications normally infer it from application_with_context.
#[doc(hidden)]
pub struct ContextInitialize<F>(F);
impl<F> ContextInitialize<F> {
  fn call<C, State>(&self, context: &C) -> Result<State>
  where
    F: Fn(&C) -> Result<State>,
  {
    (self.0)(context)
  }
}
/// Initialization contract used by generic Application configurations.
#[doc(hidden)]
pub trait ApplicationInitializer<State> {
  fn initialize(&self, context: &AppContext) -> Result<State>;
}
impl<State, F: Fn() -> State> ApplicationInitializer<State> for F {
  fn initialize(&self, _: &AppContext) -> Result<State> {
    Ok(self())
  }
}
impl<State, F: Fn(&AppContext) -> Result<State>> ApplicationInitializer<State> for ContextInitialize<F> {
  fn initialize(&self, context: &AppContext) -> Result<State> {
    self.call(context)
  }
}

/// Configuration for a native application.
pub struct Application<Initialize, Update, View, Subscribe = NoSubscription> {
  initialize: Initialize,
  update: Update,
  view: View,
  id: Option<String>,
  title: &'static str,
  plugins: Vec<PluginCreator>,
  #[cfg(feature = "multiprocess")]
  processes: Option<crate::process::HostConfig>,
  subscription: Subscribe,
  settings: window::Settings,
  exit_on_last_window: bool,
  activation: Option<Arc<dyn Fn(&AppContext, ProgramId, crate::runtime::ActivationEvent) + Send + Sync>>,
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
  desktop_event: Option<Arc<dyn Fn(&AppContext, ProgramId, lintel_desktop::DesktopEvent) + Send + Sync>>,
}

/// The default native application has no user-defined subscriptions.
pub struct NoSubscription;

pub trait ApplicationSubscription<State, Message>: Send + Sync + 'static {
  fn subscribe(&self, state: &State) -> Subscription<Message>;
}

impl<State, Message> ApplicationSubscription<State, Message> for NoSubscription {
  fn subscribe(&self, _state: &State) -> Subscription<Message> {
    Subscription::none()
  }
}

impl<State, Message, F> ApplicationSubscription<State, Message> for F
where
  F: Fn(&State) -> Subscription<Message> + Send + Sync + 'static,
{
  fn subscribe(&self, state: &State) -> Subscription<Message> {
    self(state)
  }
}

impl<Initialize, Update, View> Application<Initialize, Update, View> {
  fn new(initialize: Initialize, update: Update, view: View) -> Self {
    Self {
      initialize,
      update,
      view,
      id: None,
      title: "Lintel",
      plugins: Vec::new(),
      #[cfg(feature = "multiprocess")]
      processes: None,
      subscription: NoSubscription,
      settings: window::Settings::default(),
      exit_on_last_window: true,
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

impl<Initialize, Update, View, Subscribe> Application<Initialize, Update, View, Subscribe> {
  /// Sets the required reverse-DNS identifier used for the application's data directories.
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

  /// Sets the window title.
  pub fn title(mut self, title: &'static str) -> Self {
    self.title = title;
    self
  }

  /// Registers a plugin creator before the application starts.
  /// 装配由框架负责退出清理的独立 Mojo 服务进程。
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

  /// Configures the initial window using iced's native settings.
  pub fn window(mut self, settings: window::Settings) -> Self {
    self.settings = settings;
    self
  }

  /// Keeps the process alive after its window closes, for example for a tray app.
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

  /// Installs the native menu, tray, and shortcut services.
  #[cfg(feature = "desktop")]
  pub fn desktop(mut self) -> Self {
    self.desktop = true;
    self
  }

  /// Routes native menu, tray, and global-shortcut events into the normal update function.
  #[cfg(feature = "desktop")]
  pub fn on_desktop_event<State, Message>(
    mut self,
    callback: impl Fn(lintel_desktop::DesktopEvent) -> Message + Send + Sync + 'static,
  ) -> Self
  where
    Initialize: ApplicationInitializer<State>,
    Update: Fn(&mut State, Message) -> Task<Message>,
    Message: Debug + Send + Sync + 'static,
  {
    self.desktop = true;
    self.desktop_event = Some(Arc::new(move |context, program_id, event| {
      if let Err(error) = crate::dispatch_program_message(context, Event::Update(callback(event)).route(program_id)) {
        tracing::error!(?error, "cannot deliver native desktop event");
      }
    }));
    self
  }

  /// Routes initial/secondary launches, open-URL, and application-reopen events into state.
  pub fn on_activation<State, Message>(
    mut self,
    callback: impl Fn(crate::runtime::ActivationEvent) -> Message + Send + Sync + 'static,
  ) -> Self
  where
    Initialize: ApplicationInitializer<State>,
    Update: Fn(&mut State, Message) -> Task<Message>,
    Message: Debug + Send + Sync + 'static,
  {
    self.activation = Some(Arc::new(move |context, program_id, event| {
      if let Err(error) = crate::dispatch_program_message(context, Event::Update(callback(event)).route(program_id)) {
        tracing::error!(?error, "cannot deliver application activation");
      }
    }));
    self
  }

  /// Adds state-dependent timers, keyboard events, or other iced subscriptions.
  pub fn subscription<State, Message, NewSubscribe>(
    self,
    subscription: NewSubscribe,
  ) -> Application<Initialize, Update, View, NewSubscribe>
  where
    Initialize: ApplicationInitializer<State>,
    Update: Fn(&mut State, Message) -> Task<Message>,
    NewSubscribe: Fn(&State) -> Subscription<Message>,
  {
    Application {
      initialize: self.initialize,
      update: self.update,
      view: self.view,
      id: self.id,
      title: self.title,
      plugins: self.plugins,
      #[cfg(feature = "multiprocess")]
      processes: self.processes,
      subscription,
      settings: self.settings,
      exit_on_last_window: self.exit_on_last_window,
      activation: self.activation,
      #[cfg(feature = "crash-reporting")]
      crash: self.crash,
      #[cfg(feature = "crash-reports")]
      reports: self.reports,
      #[cfg(feature = "desktop")]
      desktop: self.desktop,
      #[cfg(all(feature = "desktop", feature = "public-iced"))]
      accessibility: self.accessibility,
      #[cfg(feature = "desktop")]
      activation_options: self.activation_options,
      #[cfg(feature = "desktop")]
      desktop_event: self.desktop_event,
    }
  }

  /// Opens the window and runs the application on the main thread.
  pub fn run<State, Message>(self) -> Result<()>
  where
    State: Send + Sync + 'static,
    Message: Clone + Debug + Send + Sync + 'static,
    Initialize: ApplicationInitializer<State> + Send + Sync + 'static,
    Update: Fn(&mut State, Message) -> Task<Message> + Send + Sync + 'static,
    View: for<'a> Fn(&'a State) -> Element<'a, Message> + Send + Sync + 'static,
    Subscribe: ApplicationSubscription<State, Message>,
  {
    let Self {
      initialize,
      update,
      view,
      id,
      title,
      plugins,
      #[cfg(feature = "multiprocess")]
      processes,
      subscription,
      settings,
      exit_on_last_window,
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
    let program_id = NativeWindow::<State, Message, Update, View, Subscribe>::id();
    if let Some(callback) = activation {
      app = app.with_activation_handler(move |context, event| callback(context, program_id, event));
    }
    #[cfg(feature = "desktop")]
    {
      app = app.with_activation_options(activation_options);
      if desktop {
        app = app.with_desktop();
      }
      if let Some(callback) = desktop_event {
        app = app.with_desktop_event_handler(move |context, event| callback(context, program_id, event));
      }
    }

    let callbacks = Arc::new((update, view));
    let subscription = Arc::new(subscription);
    app.try_run(move |context| {
      crate::runtime::initialize_then(
        || initialize.initialize(context),
        |state| {
          let iced = context
            .get_service::<dyn IcedAppPluginApi>()
            .ok_or_else(|| anyhow::anyhow!("iced plugin service is unavailable"))?;
          iced
            .try_create(Box::new(NativeWindow {
              state,
              callbacks: Arc::clone(&callbacks),
              title,
              window_id: None,
              context: context.clone(),
              message: PhantomData::<fn() -> Message>,
              subscription: subscription.clone(),
              settings: settings.clone(),
              exit_on_last_window,
            }))
            .map_err(Into::into)
        },
      )
    })
  }
}

struct NativeWindow<State, Message, Update, View, Subscribe> {
  state: State,
  callbacks: Arc<(Update, View)>,
  title: &'static str,
  window_id: Option<window::Id>,
  context: AppContext,
  message: PhantomData<fn() -> Message>,
  subscription: Arc<Subscribe>,
  settings: window::Settings,
  exit_on_last_window: bool,
}

#[derive(Debug)]
enum Event<Message> {
  Update(Message),
  Closed(window::Id),
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

impl<Message: Debug + Send + Sync + 'static> Event<Message> {
  fn route(self, program_id: ProgramId) -> Arc<dyn AnyMessage> {
    Arc::new(Routed {
      program_id,
      event: self,
    })
  }
}

impl<State, Message, Update, View, Subscribe> IcedApplication for NativeWindow<State, Message, Update, View, Subscribe>
where
  State: Send + Sync + 'static,
  Message: Clone + Debug + Send + Sync + 'static,
  Update: Fn(&mut State, Message) -> Task<Message> + Send + Sync + 'static,
  View: for<'a> Fn(&'a State) -> Element<'a, Message> + Send + Sync + 'static,
  Subscribe: ApplicationSubscription<State, Message>,
{
  fn boot(&mut self) -> Task<Arc<dyn AnyMessage>> {
    let (id, open) = window::open(self.settings.clone());
    self.window_id = Some(id);
    open.discard()
  }

  fn window_id(&self) -> Option<window::Id> {
    self.window_id
  }

  fn window_title(&self) -> Option<&str> {
    Some(self.title)
  }

  fn view(&self, _id: window::Id) -> Element<'_, Arc<dyn AnyMessage>> {
    let program_id = self.__id();
    let (_, view) = self.callbacks.as_ref();
    view(&self.state).map(move |message| Event::Update(message).route(program_id))
  }

  fn update(&mut self, message: Arc<dyn AnyMessage>) -> Task<Arc<dyn AnyMessage>> {
    let message: &dyn std::any::Any = message.as_ref();
    let Some(message) = message.downcast_ref::<Routed<Message>>() else {
      return Task::none();
    };

    match &message.event {
      Event::Update(message) => {
        let program_id = self.__id();
        let (update, _) = self.callbacks.as_ref();
        update(&mut self.state, message.clone()).map(move |message| Event::Update(message).route(program_id))
      }
      Event::Closed(id) if self.window_id == Some(*id) => {
        self.window_id = None;
        if self.exit_on_last_window
          && let Err(error) = self.context.quit()
        {
          tracing::error!(?error, "cannot request application quit");
        }
        Task::none()
      }
      Event::Closed(_) => Task::none(),
    }
  }

  fn subscription(&self) -> Subscription<Arc<dyn AnyMessage>> {
    let closed = window::close_events()
      .with(self.__id())
      .map(|(program_id, id)| Event::<Message>::Closed(id).route(program_id));
    let program_id = self.__id();
    Subscription::batch([
      closed,
      self
        .subscription
        .subscribe(&self.state)
        .with(program_id)
        .map(|(program_id, message)| Event::Update(message).route(program_id)),
    ])
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  struct State {
    title: String,
  }

  fn view(state: &State) -> Element<'_, u32> {
    crate::iced::widget::text(state.title.as_str()).into()
  }

  #[test]
  fn single_window_builder_preserves_borrowed_views_and_infers_subscription_state() {
    let app = application(
      || State {
        title: "Borrowed state".into(),
      },
      |state: &mut State, count: u32| {
        state.title = count.to_string();
        Task::none()
      },
      view,
    );
    // Existing explicitly three-parameter Application annotations remain valid.
    fn legacy_builder<Initialize, Update, View>(
      app: Application<Initialize, Update, View>,
    ) -> Application<Initialize, Update, View> {
      app
    }
    let app = legacy_builder(app).subscription(|state| {
      assert_eq!(state.title, "Borrowed state");
      Subscription::none()
    });
    let state = (app.initialize)();
    let _ = app.subscription.subscribe(&state);
    let _ = (app.view)(&state);
  }
  #[test]
  fn context_application_infers_state_subscription_and_keeps_three_parameter_configuration() {
    let app = application_with_context(
      |_: &AppContext| {
        Ok(State {
          title: "Context state".into(),
        })
      },
      |state: &mut State, count: u32| {
        state.title = count.to_string();
        Task::none()
      },
      view,
    )
    .id("dev.lintel.context-init")
    .on_activation(|_| 0u32);
    let _: &NoSubscription = &app.subscription;
    fn three<I, U, V>(app: Application<I, U, V>) -> Application<I, U, V> {
      app
    }
    let app = three(app).subscription(|state| {
      let _ = &state.title;
      Subscription::none()
    });
    fn runnable<I, U, V, S>(app: Application<I, U, V, S>)
    where
      I: ApplicationInitializer<State> + Send + Sync + 'static,
      U: Fn(&mut State, u32) -> Task<u32> + Send + Sync + 'static,
      V: for<'a> Fn(&'a State) -> Element<'a, u32> + Send + Sync + 'static,
      S: ApplicationSubscription<State, u32>,
    {
      let _never_run = move || app.run::<State, u32>();
    }
    runnable(app);
  }
  #[test]
  fn context_initializer_reads_services_without_a_global_context_and_propagates_error() {
    let services = crate::core::context::store::ServiceStore::new();
    services.register_service(|| Arc::new("registered".to_owned()));
    let initialize = ContextInitialize(|context: &crate::core::context::store::ServiceStore| -> Result<State> {
      Ok(State {
        title: context.get_service::<String>().unwrap().to_string(),
      })
    });
    assert_eq!(initialize.call(&services).unwrap().title, "registered");
    let failed = ContextInitialize(|_: &()| -> Result<State> { Err(anyhow::anyhow!("initializer failure")) });
    assert_eq!(failed.call(&()).err().unwrap().to_string(), "initializer failure");
  }
  #[test]
  fn context_application_missing_id_fails_before_context_or_gui_initialization() {
    let error = application_with_context(
      |_: &AppContext| -> Result<State> { panic!("initializer must not run") },
      |_: &mut State, _: u32| Task::none(),
      view,
    )
    .run()
    .unwrap_err();
    assert!(error.to_string().contains("application ID"));
  }
  #[test]
  fn legacy_generic_fn_configuration_remains_runnable() {
    fn configure<I, U, V>(app: Application<I, U, V>) -> Application<I, U, V>
    where
      I: Fn() -> State,
      U: Fn(&mut State, u32) -> Task<u32>,
    {
      app.on_activation(|_| 0u32)
    }
    let app = configure(application(
      || State { title: "Legacy".into() },
      |_: &mut State, _: u32| Task::none(),
      view,
    ));
    assert_eq!((app.initialize)().title, "Legacy");
    let _: &NoSubscription = &app.subscription;
  }
}
