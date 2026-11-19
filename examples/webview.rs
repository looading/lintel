use std::{
  borrow::Cow,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use lintel::{
  IcedAppPluginApi, cef,
  core::context::{AppContext, AppContextApi},
  iced::{self, AnyMessage, Element, IcedApplication, ProgramId, ProgramIdOfMessage, Subscription, Task, window},
  runtime::AppBuilder,
  webview::{CefAction, CefBrowser, CefMessage, JsHandler, JsInvocation, WebviewConfig, WebviewRenderMode},
};
use lintel_shared_data::TransferData;
use tokio::sync::mpsc::UnboundedSender;
use url::Url;

const APP_NAME: &str = "Lintel WebView";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const SMOKE_READY_TIMEOUT: Duration = Duration::from_secs(15);
const SMOKE_EXIT_TIMEOUT: Duration = Duration::from_secs(20);

fn main() -> Result<()> {
  tracing_subscriber::fmt().with_target(false).init();
  let smoke = std::env::args().any(|arg| arg == "--smoke");
  let smoke_completed = Arc::new(AtomicBool::new(false));
  let completed = smoke_completed.clone();

  let process = cef::run(APP_VERSION, move |profile, pump| {
    ensure!(
      lintel_lib_env::webview_test_url_override().is_none(),
      "clear the imported WebView test URL override to run the local fixture"
    );
    let page = profile.join("webview.html");
    let html = include_str!("webview.html");
    let html = if smoke {
      Cow::Owned(html.replacen(
        "<body>",
        r#"<body><div aria-hidden="true" style="position:fixed;left:8px;top:8px;width:64px;height:44px;background:#e317ad"></div>"#,
        1,
      ))
    } else {
      Cow::Borrowed(html)
    };
    std::fs::write(&page, html.as_bytes())?;
    let url = Url::from_file_path(&page).map_err(|()| anyhow::anyhow!("cannot convert demo page to file URL"))?;
    if smoke {
      iced::ui_perf::set_enabled(true);
    }
    AppBuilder::new(APP_NAME).with_cef_pump(pump).run(move |context| {
      let plugin = context
        .get_service::<dyn IcedAppPluginApi>()
        .expect("runtime registers iced before calling setup");
      plugin.create(Box::new(Demo::new(
        context.clone(),
        url.clone(),
        smoke,
        completed.clone(),
      )));
    })
  })?;

  if smoke && process == cef::Process::Browser {
    ensure!(
      smoke_completed.load(Ordering::Acquire),
      "WebView smoke scenario did not complete"
    );
    println!("LINTEL_WEBVIEW_SMOKE_OK");
  }
  Ok(())
}

#[derive(Debug, Clone)]
enum Message {
  Opened(window::Id),
  Cef(CefMessage),
  BridgeVerified,
  CloseRequested(window::Id),
  NativeClosed(window::Id),
  SmokeTick,
  SmokeScreenshot(window::Screenshot),
}

impl ProgramIdOfMessage for Message {
  fn program_id(&self) -> ProgramId {
    Demo::id()
  }
}

fn program_message(message: Message) -> Arc<dyn AnyMessage> {
  Arc::new(message)
}

struct Demo {
  context: AppContext,
  window: Option<window::Id>,
  browser: CefBrowser,
  url: Url,
  loaded: bool,
  bridge_verified: bool,
  close_requested: bool,
  close_sent: bool,
  browser_closed: bool,
  smoke: bool,
  smoke_ready: bool,
  smoke_screenshot_pending: bool,
  smoke_completed: Arc<AtomicBool>,
  started: Instant,
}

impl Demo {
  fn new(context: AppContext, url: Url, smoke: bool, smoke_completed: Arc<AtomicBool>) -> Self {
    Self {
      context,
      window: None,
      browser: CefBrowser::new(),
      url,
      loaded: false,
      bridge_verified: false,
      close_requested: false,
      close_sent: false,
      browser_closed: false,
      smoke,
      smoke_ready: false,
      smoke_screenshot_pending: false,
      smoke_completed,
      started: Instant::now(),
    }
  }

  fn request_close(&mut self) {
    self.close_requested = true;
    if !self.close_sent
      && let Some(browser) = self.browser.browser_id()
    {
      match cef::close_browser(browser) {
        Ok(()) => self.close_sent = true,
        Err(error) => tracing::error!(%error, "cannot request CEF browser close"),
      }
    }
  }

  fn update_browser(&mut self, message: CefMessage) -> Task<Message> {
    let (action, task) = self.browser.update(message).split(Message::Cef);
    match action {
      CefAction::Created(browser, js) => {
        tracing::info!(?browser, "standalone CEF browser created");
        if self.close_requested {
          self.request_close();
        }
        return Task::batch([task, js.spawn_handler(|output| EchoHandler { output })]);
      }
      CefAction::Loaded(browser) => {
        self.loaded = true;
        tracing::info!(?browser, "standalone CEF page loaded");
      }
      CefAction::LoadError {
        error_text,
        is_main_frame: true,
        ..
      } => {
        tracing::error!(?error_text, "standalone CEF page failed to load");
      }
      CefAction::Closed(browser) => {
        tracing::info!(?browser, "standalone CEF browser close callback received");
        self.browser_closed = true;
        if let Some(window) = self.window {
          return task.chain(window::close(window));
        }
        self.finish_exit();
      }
      _ => {}
    }
    task
  }

  fn finish_exit(&self) {
    self.smoke_completed.store(self.smoke_ready, Ordering::Release);
    if let Err(error) = self.context.quit() {
      tracing::error!(%error, "cannot request event-loop exit");
    }
  }

  fn check_smoke(&mut self) -> Task<Message> {
    let elapsed = self.started.elapsed();
    if elapsed >= SMOKE_EXIT_TIMEOUT {
      // Do not enter CefShutdown if the browser never acknowledged its close.
      tracing::error!(
        loaded = self.loaded,
        bridge_verified = self.bridge_verified,
        "WebView smoke timed out before CEF close callback"
      );
      std::process::exit(1);
    }
    if self.close_requested {
      self.request_close();
      return Task::none();
    }

    if elapsed >= SMOKE_READY_TIMEOUT {
      tracing::error!(
        loaded = self.loaded,
        bridge_verified = self.bridge_verified,
        "WebView smoke did not see load, bridge, paint and composition"
      );
      self.request_close();
      return Task::none();
    }

    if !self.smoke_screenshot_pending
      && self.loaded
      && self.bridge_verified
      && let Some(id) = self.browser.browser_id()
      && let Some(window) = self.window
    {
      let paint = iced::ui_perf::snapshot().entries.into_iter().find(|entry| {
        entry.browser_id == Some(id.inner())
          && entry.kind == "webview_paint"
          && matches!(entry.name.as_str(), "accelerated_paint" | "software_paint")
      });
      if let Some(paint) = paint {
        tracing::info!(browser = id.inner(), mode = %paint.name, frames = paint.count, "standalone CEF paint observed");
        self.smoke_screenshot_pending = true;
        return window::screenshot(window).map(Message::SmokeScreenshot);
      }
    }
    Task::none()
  }

  fn check_composition(&mut self, screenshot: window::Screenshot) {
    self.smoke_screenshot_pending = false;
    if self.close_requested {
      return;
    }

    // Count the fixture's #e317ad marker in iced's RGBA/sRGB compositor output.
    let marker_pixels = screenshot
      .rgba
      .chunks_exact(4)
      .filter(|rgba| rgba[0].abs_diff(0xe3) <= 40 && rgba[1] <= 85 && rgba[2].abs_diff(0xad) <= 40 && rgba[3] >= 230)
      .count();
    if marker_pixels >= 1024 {
      tracing::info!(
        marker_pixels,
        width = screenshot.size.width,
        height = screenshot.size.height,
        "LINTEL_WEBVIEW_COMPOSITION_OK"
      );
      self.smoke_ready = true;
      self.request_close();
    }
  }
}

impl IcedApplication for Demo {
  fn boot(&mut self) -> Task<Arc<dyn AnyMessage>> {
    let (id, task) = window::open(window::Settings {
      size: iced::Size::new(840.0, 580.0),
      exit_on_close_request: false,
      ..Default::default()
    });
    self.window = Some(id);
    task.map(|id| program_message(Message::Opened(id)))
  }

  fn window_id(&self) -> Option<window::Id> {
    self.window
  }

  fn view(&self, id: window::Id) -> Element<'_, Arc<dyn AnyMessage>> {
    let status = if self.close_requested {
      "Closing browser…"
    } else if self.bridge_verified {
      "JS ↔ Rust roundtrip verified"
    } else if self.loaded {
      "Page loaded. Waiting for the bridge."
    } else {
      "Opening CEF browser…"
    };
    let toolbar = iced::widget::row![
      iced::widget::text(status),
      iced::widget::space::horizontal(),
      iced::widget::button("Close example").on_press(Message::CloseRequested(id)),
    ]
    .align_y(iced::Alignment::Center)
    .padding(12);
    let webview = self
      .browser
      .view_with_render_mode(WebviewRenderMode::Crop)
      .map(Message::Cef);
    let content = iced::widget::column![
      toolbar,
      iced::widget::container(webview).width(iced::Fill).height(iced::Fill),
    ];
    Element::from(content).map(program_message)
  }

  fn update(&mut self, message: Arc<dyn AnyMessage>) -> Task<Arc<dyn AnyMessage>> {
    let Ok(message) = iced::downcast_message::<Message>(message) else {
      return Task::none();
    };
    match message {
      Message::Opened(window_id) => self
        .update_browser(CefMessage::Create {
          window_id,
          config: WebviewConfig {
            url: self.url.clone(),
            persist_session_cookies: false,
            ..Default::default()
          },
        })
        .map(program_message),
      Message::Cef(message) => self.update_browser(message).map(program_message),
      Message::BridgeVerified => {
        if !self.bridge_verified {
          tracing::info!("standalone CEF JS-to-Rust roundtrip verified");
        }
        self.bridge_verified = true;
        Task::none()
      }
      Message::CloseRequested(id) => {
        if self.window == Some(id) {
          self.request_close();
        }
        Task::none()
      }
      Message::NativeClosed(id) => {
        if self.window == Some(id) {
          self.window = None;
          tracing::info!(?id, "standalone WebView window closed");
          if self.browser_closed {
            self.finish_exit();
          } else {
            self.request_close();
          }
        }
        Task::none()
      }
      Message::SmokeTick => self.check_smoke().map(program_message),
      Message::SmokeScreenshot(screenshot) => {
        self.check_composition(screenshot);
        Task::none()
      }
    }
  }

  fn subscription(&self) -> Subscription<Arc<dyn AnyMessage>> {
    let smoke = if self.smoke {
      iced::time::every(Duration::from_millis(200)).map(|_| Message::SmokeTick)
    } else {
      Subscription::none()
    };
    Subscription::batch([
      self.browser.subscription().map(Message::Cef),
      window::close_requests().map(Message::CloseRequested),
      window::close_events().map(Message::NativeClosed),
      smoke,
    ])
    .map(program_message)
  }
}

struct EchoHandler {
  output: UnboundedSender<Message>,
}

impl JsHandler for EchoHandler {
  type Message = JsInvocation;

  fn handle(&mut self, invocation: JsInvocation) {
    let Some(response) = invocation.responder() else { return };
    let reply = TransferData::from_bytes(invocation.params).and_then(|value| {
      let text = value.json_value().context("demo echo expects a text payload")?;
      match invocation.fn_name.as_str() {
        "lintel.demo.echo" => TransferData::String(text).to_bytes(),
        "lintel.demo.verified" => {
          let _ = self.output.send(Message::BridgeVerified);
          TransferData::String(text).to_bytes()
        }
        _ => anyhow::bail!("unknown demo method"),
      }
    });
    response.send(reply);
  }
}
