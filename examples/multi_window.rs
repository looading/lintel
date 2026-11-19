use std::{
  any::Any,
  hash::{DefaultHasher, Hash, Hasher},
  sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
  },
  time::Duration,
};

use tokio::sync::Barrier;

use lintel::{
  IcedAppPluginApi,
  core::{
    context::{AppContext, AppContextApi},
    plugin::{Plugin, PluginLifecycle},
  },
  iced::{
    AnyMessage, Element, IcedApplication, Point, ProgramId, ProgramIdOfMessage, Size, Subscription, Task,
    widget::{button, center, column, text},
    window,
  },
  runtime::AppBuilder,
};

#[derive(Default)]
struct Evidence {
  opened: AtomicUsize,
  updated: AtomicUsize,
  rendered: Mutex<Vec<(window::Id, u64)>>,
  closed: AtomicUsize,
  plugin_initialized: AtomicUsize,
  plugin_exited: AtomicUsize,
}

struct ExamplePlugin(Arc<Evidence>);

impl Plugin for ExamplePlugin {
  fn name(&self) -> &str {
    "lintel-native-example"
  }
}

impl PluginLifecycle for ExamplePlugin {
  fn on_init(&self, _context: &AppContext) {
    self.0.plugin_initialized.fetch_add(1, Ordering::SeqCst);
    tracing::info!("example plugin initialized");
  }

  fn on_exit(&self, _context: &AppContext) {
    self.0.plugin_exited.fetch_add(1, Ordering::SeqCst);
    tracing::info!("example plugin exited");
  }
}

struct WindowProgram {
  program_id: ProgramId,
  window_id: Option<window::Id>,
  title: &'static str,
  position: Point,
  count: u32,
  smoke: Option<Arc<Barrier>>,
  context: AppContext,
  evidence: Arc<Evidence>,
}

#[derive(Debug)]
enum WindowEvent {
  Opened(window::Id),
  Increment,
  Capture(window::Id, u8),
  Frame(window::Id, u8, window::Screenshot),
  Close,
  Closed(window::Id),
}

#[derive(Debug)]
struct Message {
  program_id: ProgramId,
  event: WindowEvent,
}

impl Message {
  fn new(program_id: ProgramId, event: WindowEvent) -> Arc<dyn AnyMessage> {
    Arc::new(Self { program_id, event })
  }
}

impl ProgramIdOfMessage for Message {
  fn program_id(&self) -> ProgramId {
    self.program_id
  }
}

impl WindowProgram {
  fn capture_after_draw(&self, id: window::Id, attempt: u8) -> Task<Arc<dyn AnyMessage>> {
    let program_id = self.program_id;
    Task::perform(
      async { tokio::time::sleep(Duration::from_millis(150)).await },
      move |()| Message::new(program_id, WindowEvent::Capture(id, attempt)),
    )
  }

  fn verify_frame(&self, id: window::Id, attempt: u8, screenshot: &window::Screenshot) -> Task<Arc<dyn AnyMessage>> {
    let expected_len = (screenshot.size.width as usize)
      .checked_mul(screenshot.size.height as usize)
      .and_then(|count| count.checked_mul(4));
    let mut pixels = screenshot.rgba.chunks_exact(4);
    let has_content = pixels
      .next()
      .is_some_and(|first| pixels.any(|pixel| pixel[..3] != first[..3] && pixel[3] != 0));
    if expected_len != Some(screenshot.rgba.len()) || !has_content {
      if attempt < 5 {
        return self.capture_after_draw(id, attempt + 1);
      }
      tracing::error!(
        ?id,
        title = self.title,
        ?screenshot,
        "native window frame has no valid visible RGBA content"
      );
      let _ = self.context.exit(1);
      return Task::none();
    }

    let mut hasher = DefaultHasher::new();
    screenshot.rgba.hash(&mut hasher);
    let fingerprint = hasher.finish();
    self.evidence.rendered.lock().unwrap().push((id, fingerprint));
    tracing::info!(
      ?id,
      title = self.title,
      width = screenshot.size.width,
      height = screenshot.size.height,
      rgba_bytes = screenshot.rgba.len(),
      fingerprint = format_args!("{fingerprint:016x}"),
      "native window frame verified"
    );

    let Some(frame_gate) = self.smoke.clone() else {
      return Task::none();
    };
    let program_id = self.program_id;
    let delay = if self.position.x < 300.0 { 250 } else { 750 };
    Task::perform(
      async move {
        frame_gate.wait().await;
        tokio::time::sleep(Duration::from_millis(delay)).await;
      },
      move |()| Message::new(program_id, WindowEvent::Close),
    )
  }
}

impl IcedApplication for WindowProgram {
  fn __id(&self) -> ProgramId {
    self.program_id
  }

  fn window_id(&self) -> Option<window::Id> {
    self.window_id
  }

  #[cfg(feature = "public-iced")]
  fn window_title(&self) -> Option<&str> {
    Some(self.title)
  }

  fn boot(&mut self) -> Task<Arc<dyn AnyMessage>> {
    let (id, open) = window::open(window::Settings {
      size: Size::new(400.0, 240.0),
      position: window::Position::Specific(self.position),
      ..Default::default()
    });
    self.window_id = Some(id);
    let program_id = self.program_id;
    open.map(move |id| Message::new(program_id, WindowEvent::Opened(id)))
  }

  fn view(&self, _id: window::Id) -> Element<'_, Arc<dyn AnyMessage>> {
    center(
      column![
        text(self.title).size(26),
        text(format!("Window-local count: {}", self.count)),
        button("Increment").on_press(Message::new(self.program_id, WindowEvent::Increment)),
        button("Close window").on_press(Message::new(self.program_id, WindowEvent::Close)),
      ]
      .spacing(12),
    )
    .into()
  }

  fn update(&mut self, message: Arc<dyn AnyMessage>) -> Task<Arc<dyn AnyMessage>> {
    let message: &dyn Any = message.as_ref();
    let Some(message) = message.downcast_ref::<Message>() else {
      return Task::none();
    };

    match message.event {
      WindowEvent::Opened(id) if self.window_id == Some(id) => {
        self.evidence.opened.fetch_add(1, Ordering::SeqCst);
        tracing::info!(?id, title = self.title, "native window opened");
        #[cfg(not(feature = "public-iced"))]
        let title = window::set_title(id, self.title.to_owned());
        #[cfg(feature = "public-iced")]
        let title = Task::none();
        if self.smoke.is_none() {
          return title;
        }
        let program_id = self.program_id;
        Task::batch([
          title,
          Task::done(Message::new(program_id, WindowEvent::Increment)),
          self.capture_after_draw(id, 1),
        ])
      }
      WindowEvent::Increment => {
        self.count += 1;
        self.evidence.updated.fetch_add(1, Ordering::SeqCst);
        tracing::info!(title = self.title, count = self.count, "native window state updated");
        Task::none()
      }
      WindowEvent::Capture(id, attempt) if self.window_id == Some(id) => {
        let program_id = self.program_id;
        window::screenshot(id).map(move |frame| Message::new(program_id, WindowEvent::Frame(id, attempt, frame)))
      }
      WindowEvent::Frame(id, attempt, ref screenshot) if self.window_id == Some(id) => {
        self.verify_frame(id, attempt, screenshot)
      }
      WindowEvent::Close => self.window_id.map_or_else(Task::none, window::close),
      WindowEvent::Closed(id) if self.window_id == Some(id) => {
        self.window_id = None;
        let closed = self.evidence.closed.fetch_add(1, Ordering::SeqCst) + 1;
        tracing::info!(?id, title = self.title, closed, "native window closed");
        if closed == 2
          && let Err(error) = self.context.quit()
        {
          tracing::error!(?error, "cannot request application quit");
        }
        Task::none()
      }
      _ => Task::none(),
    }
  }

  fn subscription(&self) -> Subscription<Arc<dyn AnyMessage>> {
    window::close_events()
      .with(self.program_id)
      .map(|(program_id, id)| Message::new(program_id, WindowEvent::Closed(id)))
  }
}

fn main() -> anyhow::Result<()> {
  tracing_subscriber::fmt().with_target(false).init();

  let smoke = std::env::args().any(|arg| arg == "--smoke");
  let frame_gate = smoke.then(|| Arc::new(Barrier::new(2)));
  let evidence = Arc::new(Evidence::default());
  let plugin_evidence = evidence.clone();
  let app_evidence = evidence.clone();
  AppBuilder::new("Lintel Native")
    .with_plugins(vec![Box::new(move |context| {
      context.register_plugin(Arc::new(ExamplePlugin(plugin_evidence)));
    })])
    .run(move |context| {
      let iced = context
        .get_service::<dyn IcedAppPluginApi>()
        .expect("iced plugin must be registered before setup");
      for (index, (title, x)) in [("Lintel Native A", 100.0), ("Lintel Native B", 540.0)]
        .into_iter()
        .enumerate()
      {
        iced.create(Box::new(WindowProgram {
          program_id: ProgramId::new("lintel-native-window", index as u64),
          window_id: None,
          title,
          position: Point::new(x, 120.0),
          count: 0,
          smoke: frame_gate.clone(),
          context: context.clone(),
          evidence: app_evidence.clone(),
        }));
      }
    })?;

  if smoke {
    let rendered = evidence.rendered.lock().unwrap();
    anyhow::ensure!(
      evidence.opened.load(Ordering::SeqCst) == 2
        && evidence.updated.load(Ordering::SeqCst) == 2
        && matches!(&rendered[..], [(first, first_frame), (second, second_frame)] if first != second && first_frame != second_frame)
        && evidence.closed.load(Ordering::SeqCst) == 2
        && evidence.plugin_initialized.load(Ordering::SeqCst) == 1
        && evidence.plugin_exited.load(Ordering::SeqCst) == 1,
      "expected two windows to open, update, render distinct frames, and close, plus one plugin start and stop"
    );
    tracing::info!("native multi-window smoke passed");
  }
  Ok(())
}
