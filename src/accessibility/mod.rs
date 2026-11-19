//! Optional native semantics for ordinary iced application messages.
//! Use stable per-window keys; helpers do not infer semantics for unknown custom widgets.
mod collect;
pub(crate) mod menu;
mod model;
mod widgets;

pub use model::SemanticAction;
pub use widgets::{Semantic, SemanticRole, button, checkbox, disabled_button, password_input, semantic, text_input};

use crate::iced::iced_winit::{NativeWindowFactory, NativeWindowIntegration};
use crate::{
  core::event_loop::event::LintelEvent,
  desktop::accessibility::{AccessibilityWindow, accesskit as ak},
  iced::Action,
};
use model::WindowState;
use std::sync::{Arc, atomic::Ordering};

pub(crate) struct Factory {
  pub proxy: winit::event_loop::EventLoopProxy<Action<LintelEvent>>,
}
struct Integration {
  // Native adapter must drop before its last native window owner.
  adapter: AccessibilityWindow,
  window: Arc<winit::window::Window>,
  state: Arc<WindowState>,
  proxy: winit::event_loop::EventLoopProxy<Action<LintelEvent>>,
  id: crate::iced::window::Id,
  title: String,
  published: u64,
  trace_modifiers: winit::keyboard::ModifiersState,
}
impl NativeWindowFactory for Factory {
  fn create(
    &self,
    event_loop: &winit::event_loop::ActiveEventLoop,
    window: Arc<winit::window::Window>,
    id: crate::iced::window::Id,
    title: &str,
  ) -> Result<Box<dyn NativeWindowIntegration>, String> {
    let weak = Arc::downgrade(&window);
    let state = WindowState::new(move || {
      if let Some(window) = weak.upgrade() {
        window.request_redraw();
      }
    });
    let adapter = AccessibilityWindow::with_direct_handlers(
      event_loop,
      &window,
      Activation(state.clone()),
      Actions(state.clone()),
      Deactivation,
    )
    .map_err(|error| error.to_string())?;
    Ok(Box::new(Integration {
      adapter,
      window,
      state,
      proxy: self.proxy.clone(),
      id,
      title: title.into(),
      published: 0,
      trace_modifiers: Default::default(),
    }))
  }
}
impl NativeWindowIntegration for Integration {
  fn before_event(&mut self, event: &winit::event::WindowEvent) -> bool {
    if crate::menu::input_trace_enabled() {
      use winit::event::WindowEvent;
      if let WindowEvent::ModifiersChanged(modifiers) = event {
        self.trace_modifiers = modifiers.state();
      }
      match event {
        WindowEvent::KeyboardInput {
          event, is_synthetic, ..
        } => {
          let class = match &event.logical_key {
            winit::keyboard::Key::Named(named) => format!("Named::{named:?}"),
            winit::keyboard::Key::Character(_) => "Character".into(),
            winit::keyboard::Key::Dead(_) => "Dead".into(),
            winit::keyboard::Key::Unidentified(_) => "Unidentified".into(),
          };
          crate::menu::input_trace(
            "native",
            format_args!(
              "window={:?} state={:?} key={class} repeat={} synthetic={is_synthetic} mods={:?}",
              self.id, event.state, event.repeat, self.trace_modifiers
            ),
          );
        }
        WindowEvent::ModifiersChanged(_) => crate::menu::input_trace(
          "native",
          format_args!("window={:?} modifiers={:?}", self.id, self.trace_modifiers),
        ),
        WindowEvent::Focused(focused) => {
          crate::menu::input_trace("native", format_args!("window={:?} focused={focused}", self.id))
        }
        WindowEvent::Ime(event) => crate::menu::input_trace(
          "native",
          format_args!(
            "window={:?} ime={}",
            self.id,
            match event {
              winit::event::Ime::Enabled => "Enabled",
              winit::event::Ime::Disabled => "Disabled",
              winit::event::Ime::Preedit(..) => "Preedit",
              winit::event::Ime::Commit(_) => "Commit",
            }
          ),
        ),
        _ => {}
      }
    }
    if let Err(error) = self.adapter.process_event(&self.window, event) {
      tracing::error!(%error,"accessibility event rejected");
    }
    let generation = self.state.generation.load(Ordering::Acquire);
    if generation != self.published {
      if let Some(tree) = self.state.tree() {
        self.adapter.update_if_active(|| tree);
        self.published = generation;
      }
    }
    false
  }
  fn after_redraw(&mut self) {
    let operation = collect::Collect::new(self.state.clone(), Arc::downgrade(&self.window), self.title.clone());
    let _ = self
      .proxy
      .send_event(Action::Window(crate::iced::window::Action::Operate(
        self.id,
        Box::new(operation),
      )));
  }
}
impl Drop for Integration {
  fn drop(&mut self) {
    self.state.close();
  }
}
struct Activation(Arc<WindowState>);
impl ak::ActivationHandler for Activation {
  fn request_initial_tree(&mut self) -> Option<ak::TreeUpdate> {
    let tree = self.0.tree();
    (self.0.wake)();
    tree
  }
}
struct Actions(Arc<WindowState>);
impl ak::ActionHandler for Actions {
  fn do_action(&mut self, request: ak::ActionRequest) {
    self.0.action(request);
  }
}
struct Deactivation;
impl ak::DeactivationHandler for Deactivation {
  fn deactivate_accessibility(&mut self) {}
}
