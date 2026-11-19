use lintel_core::event_loop::event::LintelEvent;
pub use lintel_lib_iced::{Action, iced_winit, iced_winit::Runner};
pub use lintel_plugin_iced_api::*;
use std::{
  cell::RefCell,
  collections::BTreeMap,
  sync::{Arc, Mutex},
  time::Instant,
};
use tokio::sync::mpsc::{Receiver, Sender};
use winit::event_loop::EventLoopProxy;
mod plugin;
pub use plugin::*;

const SLOW_UPDATE_MS: u128 = 48;
const SLOW_VIEW_MS: u128 = 32;

pub fn create_iced_runner(
  event_loop_proxy: EventLoopProxy<Action<LintelEvent>>,
  display_handle: winit::event_loop::OwnedDisplayHandle,
  name: &'static str,
) -> std::result::Result<(Sender<IcedPluginEvent>, Runner<LintelEvent>), iced_winit::Error> {
  create_iced_runner_inner(event_loop_proxy, display_handle, name, None)
}

pub fn create_iced_runner_with_native_windows(
  event_loop_proxy: EventLoopProxy<Action<LintelEvent>>,
  display_handle: winit::event_loop::OwnedDisplayHandle,
  name: &'static str,
  native_factory: Option<Arc<dyn iced_winit::NativeWindowFactory>>,
) -> std::result::Result<(Sender<IcedPluginEvent>, Runner<LintelEvent>), iced_winit::Error> {
  create_iced_runner_inner(event_loop_proxy, display_handle, name, native_factory)
}

fn create_iced_runner_inner(
  event_loop_proxy: EventLoopProxy<Action<LintelEvent>>,
  display_handle: winit::event_loop::OwnedDisplayHandle,
  name: &'static str,
  native_factory: Option<Arc<dyn iced_winit::NativeWindowFactory>>,
) -> std::result::Result<(Sender<IcedPluginEvent>, Runner<LintelEvent>), iced_winit::Error> {
  let (tx, app) = IcedAppProxy::new();
  let app = RefCell::new(Some(app));
  let daemon = iced::daemon(
    move || app.borrow_mut().take().unwrap(),
    IcedAppProxy::update,
    IcedAppProxy::view,
  );
  let daemon = daemon.title(move |proxy: &IcedAppProxy, id| {
    proxy
      .apps
      .values()
      .find(|app| app.owns_window(id))
      .and_then(|app| app.window_title_for(id))
      .unwrap_or_else(|| name.to_owned())
  });
  let daemon = daemon
    .default_font(lintel_lib_iced::normal_font())
    .theme(iced::Theme::Light)
    .subscription(IcedAppProxy::subscription);
  #[cfg(feature = "debug")]
  lintel_lib_iced::iced_debug::init(lintel_lib_iced::iced_debug::Metadata {
    name,
    theme: None,
    can_time_travel: false,
  });

  let runner = iced_winit::create_runner_with_native_windows(daemon, event_loop_proxy, display_handle, native_factory);
  runner.map(|runner| (tx, runner))
}

struct IcedAppProxy {
  apps: BTreeMap<ProgramId, Box<dyn IcedApplication>>,
  rx: Arc<Mutex<Option<Receiver<IcedPluginEvent>>>>,
}

impl IcedAppProxy {
  fn new() -> (Sender<IcedPluginEvent>, Self) {
    let (tx, rx) = tokio::sync::mpsc::channel(10);
    (
      tx,
      Self {
        rx: Arc::new(Mutex::new(Some(rx))),
        apps: BTreeMap::new(),
      },
    )
  }

  fn add_program(&mut self, program: Box<dyn IcedApplication>) {
    self.apps.insert(program.__id(), program);
  }

  fn update(&mut self, message: LintelEvent) -> Task<LintelEvent> {
    if let LintelEvent::IcedApp(event) = message {
      return match event {
        IcedAppEvent::NewProgram(program) => {
          if let Some(mut program) = Arc::into_inner(program) {
            let task = program.boot();
            self.add_program(program);
            return task.map(IcedAppEvent::Action).map(LintelEvent::IcedApp);
          }
          Task::none()
        }
        IcedAppEvent::CloseProgram(program_id) => {
          let app = self.apps.remove(&program_id);

          if let Some(app) = app {
            Task::batch(app.window_ids().into_iter().map(window::close::<LintelEvent>))
          } else {
            Task::none()
          }
        }
        IcedAppEvent::Action(message) => {
          if let Some(app) = self.apps.get_mut(&message.program_id()) {
            let program_id = message.program_id();
            let t = Instant::now();
            let task = app.update(message).map(IcedAppEvent::Action).map(LintelEvent::IcedApp);
            let elapsed = t.elapsed();
            let elapsed_ms = elapsed.as_secs_f64() * 1000.0;
            lintel_lib_iced::ui_perf::record_update(program_id.name(), elapsed);
            if elapsed_ms >= SLOW_UPDATE_MS as f64 {
              tracing::warn!(
                program = program_id.name(),
                elapsed_ms,
                "[ui-perf] slow update detected, may cause frame drop"
              );
            }
            return task;
          }

          Task::none()
        }
      };
    }
    Task::none()
  }

  fn view(&self, id: window::Id) -> Element<'_, LintelEvent> {
    if let Some(p) = self.apps.values().find(|app| app.owns_window(id)) {
      let program_id = p.__id();
      let t = Instant::now();
      let element = p.view(id).map(IcedAppEvent::Action).map(LintelEvent::IcedApp);
      let elapsed = t.elapsed();
      let elapsed_ms = elapsed.as_millis();
      lintel_lib_iced::ui_perf::record_view(program_id.name(), elapsed);
      if elapsed_ms >= SLOW_VIEW_MS {
        tracing::warn!(
          program = program_id.name(),
          elapsed_ms,
          "[ui-perf] slow view, may cause frame drop"
        );
      }
      element
    } else {
      iced::widget::space::horizontal().into()
    }
  }

  fn subscription(&self) -> Subscription<LintelEvent> {
    // 这里使用 Arc<Mutex<Option<Receiver>>> 共享状态模式。
    // 每次调用 subscription()（例如在 UI 更新时），都会重新构建 ProgramRx。
    // 但由于 ProgramRx 使用 TypeId 进行哈希，Iced 会将其识别为【同一个】订阅。
    // 这确保了运行时会【保持】最初消耗了 Receiver 的那个流（Stream）持续运行。
    // 如果我们在这里返回无效的订阅（例如因为 rx 已经被取走而返回 Subscription::none()），
    // Iced 就会丢弃原始流，导致通道被关闭，从而无法再创建新应用。
    let program_subscription = iced::advanced::subscription::from_recipe(ProgramRx(self.rx.clone()));
    Subscription::batch(
      self
        .apps
        .values()
        .map(|w| w.subscription().map(IcedAppEvent::Action).map(LintelEvent::IcedApp))
        .chain(std::iter::once(program_subscription)),
    )
  }
}

#[derive(Clone)]
struct ProgramRx(Arc<Mutex<Option<Receiver<IcedPluginEvent>>>>);

impl iced::advanced::subscription::Recipe for ProgramRx {
  type Output = LintelEvent;
  fn hash(&self, state: &mut iced::advanced::subscription::Hasher) {
    use std::hash::Hash;
    std::any::TypeId::of::<Self>().hash(state);
  }

  fn stream(
    self: Box<Self>,
    _input: std::pin::Pin<Box<dyn tokio_stream::Stream<Item = iced::advanced::subscription::Event> + Send>>,
  ) -> std::pin::Pin<Box<dyn tokio_stream::Stream<Item = LintelEvent> + Send>> {
    use tokio_stream::{StreamExt, wrappers::ReceiverStream};

    let rx = self.0.lock().unwrap().take().expect("rx should be some");
    Box::pin(ReceiverStream::new(rx).map(|event| match event {
      IcedPluginEvent::Create { program } => LintelEvent::IcedApp(IcedAppEvent::NewProgram(program.into())),
      IcedPluginEvent::Close { program_id } => LintelEvent::IcedApp(IcedAppEvent::CloseProgram(program_id)),
      IcedPluginEvent::Task(task) => LintelEvent::IcedApp(IcedAppEvent::Action(task)),
    }))
  }
}
