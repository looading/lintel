// 抽取自 lintel_desktop_next/app/src/runner/lintel_winit.rs。
use std::time::Instant;

#[cfg(all(feature = "public-cef", target_os = "macos"))]
use std::time::Duration;

use global_hotkey::GlobalHotKeyEventReceiver;
use lintel_core::{
  context::AppContext,
  event_loop::event::{AppEvent, HotkeyManagerEvent, LintelEvent, QuitState},
};
use lintel_lib_iced::ui_perf;
use lintel_plugin_iced::iced::Action;
use tracing::{info, warn};
use winit::{
  event::{DeviceEvent, WindowEvent},
  window::WindowId,
};

use super::{ActivationEvent, ActivationHandler, AppRuntime, QuitCoordinator, QuitDecision};

// 提供点窗口信息，后续完善
fn monitor_window(event_loop: &winit::event_loop::ActiveEventLoop, id: &WindowId, event: &WindowEvent) {
  let sc = event_loop.primary_monitor().map(|m| m.size());
  match event {
    WindowEvent::Resized(size) => {
      if let Some(sc) = sc
        && (size.width > sc.width || size.height > sc.height)
      {
        warn!(?id, ?size, "window resized overflow");
      }
    }
    WindowEvent::Moved(pos) => {
      // 如果窗口起点超出屏幕范围，打印日志
      if pos.x < 0 || pos.y < 0 {
        warn!(?id, ?pos, "window moved overflow");
      }
    }
    WindowEvent::CloseRequested => info!(?id, "window close requested"),
    WindowEvent::Focused(focused) => info!(?id, ?focused, "window focused",),
    WindowEvent::Destroyed => info!(?id, "window destroyed"),
    _ => {}
  }
}

pub(super) struct AppRunner<'a> {
  pub(crate) app_context: AppContext,
  pub(crate) exit_code: Option<i32>,
  pub(crate) relaunch_args: Option<Option<Vec<String>>>,
  pub(crate) activation_handler: Option<ActivationHandler>,
  pub(crate) activation_inbox: crate::activation::ActivationInbox,
  #[cfg(feature = "desktop")]
  pub(crate) activation: crate::activation::ActivationRuntime,
  pub(crate) setup: super::StartupSetup<AppContext>,
  pub(crate) global_hotkey_rx: &'a GlobalHotKeyEventReceiver,
  pub(crate) iced: Option<lintel_plugin_iced::Runner<LintelEvent>>,
  pub(crate) cef_enabled: bool,
  pub(crate) cef_schedule: Option<Instant>,
  pub(crate) quit_started: bool,
  pub(crate) last_event_loop_activity: Option<Instant>,
  #[cfg(feature = "desktop")]
  pub(crate) desktop: Option<lintel_desktop::DesktopRuntime>,
  #[cfg(feature = "desktop")]
  pub(crate) desktop_hotkeys: Option<std::sync::mpsc::Receiver<global_hotkey::GlobalHotKeyEvent>>,
  #[cfg(feature = "desktop")]
  pub(crate) desktop_event_handler: Option<super::DesktopEventHandler>,
}

impl AppRunner<'_> {
  #[cfg(feature = "desktop")]
  fn dispatch_desktop_event(&self, event: Option<lintel_desktop::DesktopEvent>) {
    if let Some(event) = event
      && let Some(handler) = &self.desktop_event_handler
    {
      handler(&self.app_context, event);
    }
  }

  fn activate(&mut self, event: ActivationEvent) {
    if self.setup.failed() {
      return;
    }
    #[cfg(feature = "desktop")]
    let event = match self.activation.validate(event) {
      Ok(event) => event,
      Err(error) => {
        warn!(%error, "application activation rejected by protocol policy");
        return;
      }
    };
    let event = match self.activation_inbox.receive(event) {
      Ok(Some(event)) => event,
      Ok(None) => return,
      Err(error) => {
        warn!(%error, "application activation rejected before setup");
        return;
      }
    };
    self.deliver_activation(event);
  }

  fn deliver_activation(&self, event: ActivationEvent) {
    if matches!(&event, ActivationEvent::Opened { urls } if urls.is_empty()) {
      return;
    }
    if let Some(handler) = &self.activation_handler {
      handler(&self.app_context, event.clone());
    }
    let urls = match event {
      ActivationEvent::Opened { urls } | ActivationEvent::Launch { urls, .. } => urls,
      ActivationEvent::Reopen { .. } => return,
    };
    if !urls.is_empty() {
      self
        .app_context
        .dispatch_plugin_event(&LintelEvent::AppEvent(AppEvent::Opened { urls }));
    }
  }

  fn drain_activation(&mut self) {
    #[cfg(target_os = "macos")]
    for has_visible_window in super::macos_reopen::drain() {
      self.activate(ActivationEvent::Reopen { has_visible_window });
    }
    #[cfg(feature = "desktop")]
    for event in self.activation.drain(64) {
      self.activate(event);
    }
  }

  fn drain_desktop(&mut self) {
    #[cfg(feature = "desktop")]
    if let Some(desktop) = &mut self.desktop {
      let mut events = Vec::new();
      desktop.drain(64);
      events.extend(desktop.take_events(64));
      let dropped = desktop.take_event_overflow_count();
      if dropped > 0 {
        warn!(dropped, "native desktop event queue overflowed");
      }
      if let Some(receiver) = &self.desktop_hotkeys {
        for event in receiver.try_iter().take(64) {
          #[cfg(any(target_os = "macos", target_os = "windows"))]
          if let Some(event) = desktop.shortcut_event(&event) {
            events.push(event);
          }
          self.app_context.dispatch_hotkey_event(event);
        }
      }
      for event in events {
        self.dispatch_desktop_event(Some(event));
      }
    }
  }

  fn record_event_loop_activity(&mut self) {
    if !ui_perf::is_enabled() {
      return;
    }

    let now = Instant::now();
    if let Some(last_activity_at) = self.last_event_loop_activity.replace(now) {
      ui_perf::record_event_loop_gap(now.saturating_duration_since(last_activity_at));
    }
  }

  fn handle_quit_event(&mut self, event: AppEvent, event_loop: &winit::event_loop::ActiveEventLoop) {
    if matches!(
      event,
      AppEvent::Quit(QuitState::Init) | AppEvent::Exit(_, QuitState::Init) | AppEvent::Relaunch(_, QuitState::Init)
    ) {
      if self.quit_started {
        return;
      }
      if let Some(coordinator) = self.app_context.service_store.get_service::<QuitCoordinator>() {
        match coordinator.decide(&self.app_context, &event) {
          Ok(QuitDecision::Proceed) => {}
          Ok(QuitDecision::Defer) => return,
          Err(error) => {
            warn!(
              ?error,
              "application quit deferred because browser shutdown could not start"
            );
            return;
          }
        }
      }
      self.quit_started = true;
    }
    if let AppEvent::Exit(code, _) = &event {
      self.exit_code = Some(*code);
    }
    if let AppEvent::Relaunch(args, QuitState::Init) = &event {
      self.relaunch_args = Some(args.clone());
    }
    AppRuntime::new(self.app_context.clone()).handle_app_quit(event, event_loop);
  }
  fn tick_cef(&mut self, time: Instant) -> bool {
    if time <= Instant::now() {
      self.pump_cef();
      true
    } else {
      self.cef_schedule = Some(self.cef_schedule.map_or(time, |scheduled| scheduled.min(time)));
      false
    }
  }
  fn pump_cef(&mut self) {
    #[cfg(all(feature = "public-cef", target_os = "macos"))]
    if self.cef_enabled {
      let t = Instant::now();
      cef::do_message_loop_work();
      let elapsed = t.elapsed();
      let elapsed_ms = elapsed.as_millis();
      ui_perf::record_cef_pump(elapsed);
      if elapsed_ms >= ui_perf::SLOW_CEF_PUMP_THRESHOLD_MS as u128 {
        warn!(elapsed_ms, "[cef-pump] slow CEF message loop work, may block UI thread");
      }
    }
  }
}

impl winit::application::ApplicationHandler<Action<LintelEvent>> for AppRunner<'_> {
  // The selected public iced-rs/winit revision owns native GURL delivery.
  #[cfg(target_os = "macos")]
  fn received_url(&mut self, _event_loop: &winit::event_loop::ActiveEventLoop, url: String) {
    self.activate(ActivationEvent::Opened { urls: vec![url] });
  }

  fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
    self.record_event_loop_activity();
    if !self.setup.failed() {
      self.drain_desktop();
      self.drain_activation();
      if let Some(iced) = &mut self.iced {
        iced.about_to_wait(event_loop);
      }
    }
    #[cfg(all(feature = "public-cef", target_os = "macos"))]
    if self.cef_enabled {
      if self.setup.failed() {
        self.pump_cef();
      }
      let now = Instant::now();
      if self.cef_schedule.is_some_and(|deadline| deadline <= now) {
        self.cef_schedule = None;
        self.pump_cef();
      }
      // 空闲时也唤醒外部消息泵，保留 iced 主动请求的 Poll。
      if !matches!(event_loop.control_flow(), winit::event_loop::ControlFlow::Poll) {
        let timeout = now + Duration::from_millis(10);
        let deadline = timeout.min(self.cef_schedule.unwrap_or(timeout));
        event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(deadline));
      }
    }
  }
  fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
    if self.setup.failed() {
      return;
    }
    self.drain_desktop();
    self.drain_activation();
    if let Some(iced) = &mut self.iced {
      iced.resumed(event_loop);
    }
    self.pump_cef();
  }
  fn device_event(
    &mut self,
    event_loop: &winit::event_loop::ActiveEventLoop,
    device_id: winit::event::DeviceId,
    event: DeviceEvent,
  ) {
    if self.setup.failed() {
      return;
    }
    if let Some(iced) = &mut self.iced {
      iced.device_event(event_loop, device_id, event.clone());
    }
    self.pump_cef();
  }
  fn exiting(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
    if self.setup.failed() {
      return;
    }
    if let Some(iced) = &mut self.iced {
      iced.exiting(event_loop);
    }
    self.pump_cef();
    info!("exit event loop");
  }
  fn user_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, event: Action<LintelEvent>) {
    if self.setup.failed() {
      let mut cef_ticked = false;
      match event {
        Action::Output(LintelEvent::Task(task)) => task(),
        Action::Output(LintelEvent::AppEvent(event @ (AppEvent::Quit(_) | AppEvent::Exit(_, _)))) => {
          self.handle_quit_event(event, event_loop)
        }
        Action::Output(LintelEvent::TickCef(time)) if self.cef_enabled => cef_ticked = self.tick_cef(time),
        _ => {}
      }
      if !cef_ticked {
        self.pump_cef();
      }
      return;
    }
    let user_event_start = ui_perf::is_enabled().then(Instant::now);
    let mut cef_ticked = false;
    match event {
      Action::Output(event) if !matches!(event, LintelEvent::IcedApp(_)) => match event {
        LintelEvent::AppEvent(event @ (AppEvent::Quit(_) | AppEvent::Exit(_, _) | AppEvent::Relaunch(_, _))) => {
          self.handle_quit_event(event, event_loop)
        }
        LintelEvent::AppEvent(AppEvent::Opened { urls }) => {
          self.activate(ActivationEvent::Opened { urls });
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        LintelEvent::MenuEvent(ref _native_event) => {
          #[cfg(all(feature = "desktop", any(target_os = "macos", target_os = "windows")))]
          self.dispatch_desktop_event(
            self
              .desktop
              .as_ref()
              .and_then(|desktop| desktop.menu_event(_native_event)),
          );
          self.app_context.dispatch_plugin_event(&event);
        }
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        LintelEvent::TrayIconEvent(ref _native_event) => {
          #[cfg(all(feature = "desktop", any(target_os = "macos", target_os = "windows")))]
          self.dispatch_desktop_event(
            self
              .desktop
              .as_ref()
              .and_then(|desktop| desktop.tray_event(_native_event)),
          );
          self.app_context.dispatch_plugin_event(&event);
        }
        LintelEvent::HotkeyManager(event) => match event {
          HotkeyManagerEvent::Register(key) => {
            #[cfg(target_os = "linux")]
            warn!(
              ?key,
              "legacy hotkeys require an explicit Linux X11 or portal controller"
            );
            #[cfg(not(target_os = "linux"))]
            self.app_context.hotkeys_manager.register(key).inspect_err(|err| {
              tracing::error!(%err, ?key, "cannot register hotkey");
            });
          }
          HotkeyManagerEvent::Unregister(key) => {
            #[cfg(target_os = "linux")]
            warn!(?key, "close or unregister through the Linux shortcut controller");
            #[cfg(not(target_os = "linux"))]
            self.app_context.hotkeys_manager.unregister(key).inspect_err(|err| {
              tracing::error!(%err, ?key, "cannot unregister hotkey");
            });
          }
        },
        LintelEvent::Task(task) => {
          ui_perf::measure(task, |elapsed| ui_perf::record_event_loop_task("lintel_task", elapsed));
        }
        LintelEvent::TickCef(time) if self.cef_enabled => cef_ticked = self.tick_cef(time),
        _ => {}
      },
      event => {
        if let Some(iced) = &mut self.iced {
          ui_perf::measure(
            || iced.user_event(event_loop, event),
            |elapsed| ui_perf::record_event_loop_task("iced_user_event", elapsed),
          );
        }
      }
    }
    if !cef_ticked {
      self.pump_cef();
    }
    self.drain_desktop();
    self.drain_activation();
    if let Some(user_event_start) = user_event_start {
      ui_perf::record_event_loop_task("user_event", user_event_start.elapsed());
    }
  }
  fn window_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
    if self.setup.failed() {
      return;
    }
    monitor_window(event_loop, &window_id, &event);

    if let Some(iced) = &mut self.iced {
      iced.window_event(event_loop, window_id, event);
    }
    self.pump_cef();
  }
  fn new_events(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, cause: winit::event::StartCause) {
    if self.setup.failed() {
      return;
    }
    if matches!(cause, winit::event::StartCause::Init) {
      if !self.setup.run(&self.app_context) {
        self.handle_quit_event(AppEvent::Exit(1, QuitState::Init), event_loop);
        return;
      }
      for event in self.activation_inbox.finish_setup() {
        self.deliver_activation(event);
      }
      self.drain_activation();
      if let Some(primary_monitor) = event_loop.primary_monitor()
        && let Some(refresh_rate) = primary_monitor.refresh_rate_millihertz()
      {
        tracing::info!(refresh_rate = refresh_rate / 1_000, "monitor refresh rate");
      }
    }
    if self.setup.ready() {
      if let Some(iced) = &mut self.iced {
        iced.new_events(event_loop, cause);
      }
    }
    if let Ok(event) = self.global_hotkey_rx.try_recv() {
      #[cfg(all(feature = "desktop", any(target_os = "macos", target_os = "windows")))]
      self.dispatch_desktop_event(self.desktop.as_ref().and_then(|desktop| desktop.shortcut_event(&event)));
      self.app_context.dispatch_hotkey_event(event);
    }
    self.pump_cef();
  }
}
