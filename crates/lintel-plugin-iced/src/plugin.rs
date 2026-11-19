use std::{
  collections::VecDeque,
  sync::{Arc, Mutex},
};

use lintel_core::{
  context::AppContext,
  plugin::{Plugin, PluginLifecycle},
  tasks::TaskScope,
};
use lintel_plugin_iced_api::{IcedAppPluginApi, IcedApplication, IcedPluginEvent, ProgramCommandError, ProgramId};
use tokio::sync::mpsc::Sender;
use tracing::info;

pub fn creator(context: &mut AppContext, tx: Sender<IcedPluginEvent>) {
  let iced_plugin = Arc::new(IcedAppPlugin::new(tx, context.clone()));

  context.register_plugin(iced_plugin.clone());

  context.register_service(move || -> std::sync::Arc<dyn IcedAppPluginApi> { iced_plugin.clone() });
}

pub struct IcedAppPlugin {
  tx: Sender<IcedPluginEvent>,
  app_context: AppContext,
  pending: Arc<Mutex<PendingCommands>>,
}

#[derive(Default)]
struct PendingCommands {
  queue: VecDeque<IcedPluginEvent>,
  draining: bool,
}

impl PendingCommands {
  fn admit(&mut self, tx: &Sender<IcedPluginEvent>, event: IcedPluginEvent) -> Result<bool, ProgramCommandError> {
    if tx.is_closed() {
      return Err(ProgramCommandError::RendererStopped);
    }
    let event = if self.draining {
      event
    } else {
      match tx.try_send(event) {
        Ok(()) => return Ok(false),
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return Err(ProgramCommandError::RendererStopped),
        Err(tokio::sync::mpsc::error::TrySendError::Full(event)) => event,
      }
    };
    if self.queue.len() >= 256 {
      return Err(ProgramCommandError::QueueFull);
    }
    self.queue.push_back(event);
    let start = !self.draining;
    self.draining = true;
    Ok(start)
  }
}

impl Plugin for IcedAppPlugin {
  fn name(&self) -> &str {
    "iced"
  }
}

impl PluginLifecycle for IcedAppPlugin {
  fn on_drop(&self, app_context: &AppContext) {
    info!("drop iced app plugin");
  }
  fn on_exit(&self, app_context: &AppContext) {
    info!("exit iced app plugin");
  }
  fn on_hotkeys_event(&self, app_context: &AppContext, event: global_hotkey::GlobalHotKeyEvent) {}
  fn on_init(&self, app_context: &AppContext) {}
}

impl IcedAppPlugin {
  pub fn new(tx: Sender<IcedPluginEvent>, app_context: AppContext) -> Self {
    Self {
      tx,
      app_context,
      pending: Arc::new(Mutex::new(PendingCommands::default())),
    }
  }
  fn send(&self, event: IcedPluginEvent) {
    if let Err(error) = self.try_send(event) {
      tracing::error!(%error, "iced program command rejected");
    }
  }

  fn try_send(&self, event: IcedPluginEvent) -> Result<(), ProgramCommandError> {
    if self.app_context.task_tracker().is_shutting_down() {
      return Err(ProgramCommandError::ApplicationStopping);
    }
    let start = self
      .pending
      .lock()
      .map_err(|_| ProgramCommandError::QueuePoisoned)?
      .admit(&self.tx, event)?;
    if !start {
      return Ok(());
    }
    let tx = self.tx.clone();
    let pending = self.pending.clone();
    if let Err(error) = self
      .app_context
      .spawn_task(TaskScope::Application, drain_pending(tx, pending))
    {
      let mut pending = self.pending.lock().map_err(|_| ProgramCommandError::QueuePoisoned)?;
      tracing::error!(
        ?error,
        discarded = pending.queue.len(),
        "iced program commands rejected during application shutdown"
      );
      pending.queue.clear();
      pending.draining = false;
      return Err(ProgramCommandError::ApplicationStopping);
    }
    Ok(())
  }
}

async fn drain_pending(tx: Sender<IcedPluginEvent>, pending: Arc<Mutex<PendingCommands>>) {
  loop {
    let next = {
      let mut pending = pending.lock().unwrap();
      match pending.queue.pop_front() {
        Some(next) => next,
        None => {
          pending.draining = false;
          return;
        }
      }
    };
    if tx.send(next).await.is_err() {
      let mut pending = pending.lock().unwrap();
      tracing::error!(
        discarded = pending.queue.len() + 1,
        "queued iced program commands rejected: renderer has stopped"
      );
      pending.queue.clear();
      pending.draining = false;
      return;
    }
  }
}

impl IcedAppPluginApi for IcedAppPlugin {
  fn try_create(&self, program: Box<dyn IcedApplication>) -> Result<(), ProgramCommandError> {
    self.try_send(IcedPluginEvent::Create { program })
  }
  fn try_dispatch(&self, message: Arc<dyn lintel_plugin_iced_api::AnyMessage>) -> Result<(), ProgramCommandError> {
    self.try_send(IcedPluginEvent::Task(message))
  }

  fn create(&self, program: Box<dyn IcedApplication>) {
    info!("create program {:?}, closed{:?}", program.__id(), self.tx.is_closed());
    self.send(IcedPluginEvent::Create { program });
  }

  fn close(&self, program_id: ProgramId) {
    info!("close program {:?}", program_id);
    self.send(IcedPluginEvent::Close { program_id });
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  struct ProbeProgram;
  impl lintel_plugin_iced_api::IcedApplication for ProbeProgram {
    fn window_id(&self) -> Option<lintel_plugin_iced_api::window::Id> {
      None
    }
    fn view(
      &self,
      _: lintel_plugin_iced_api::window::Id,
    ) -> lintel_plugin_iced_api::Element<'_, Arc<dyn lintel_plugin_iced_api::AnyMessage>> {
      lintel_plugin_iced_api::widget::text("probe").into()
    }
    fn update(
      &mut self,
      _: Arc<dyn lintel_plugin_iced_api::AnyMessage>,
    ) -> lintel_plugin_iced_api::Task<Arc<dyn lintel_plugin_iced_api::AnyMessage>> {
      lintel_plugin_iced_api::Task::none()
    }
  }
  #[derive(Debug)]
  struct ProbeMessage;
  impl lintel_plugin_iced_api::ProgramIdOfMessage for ProbeMessage {
    fn program_id(&self) -> ProgramId {
      ProbeProgram::id()
    }
  }

  #[tokio::test]
  async fn early_typed_message_is_delivered_after_its_program_past_capacity() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    assert!(
      tx.try_send(IcedPluginEvent::Close {
        program_id: ProgramId::new("existing", 0)
      })
      .is_ok()
    );
    let pending = Arc::new(Mutex::new(PendingCommands::default()));
    assert!(
      pending
        .lock()
        .unwrap()
        .admit(
          &tx,
          IcedPluginEvent::Create {
            program: Box::new(ProbeProgram)
          }
        )
        .unwrap()
    );
    assert!(
      !pending
        .lock()
        .unwrap()
        .admit(&tx, IcedPluginEvent::Task(Arc::new(ProbeMessage)))
        .unwrap()
    );
    let task = tokio::spawn(drain_pending(tx, pending));
    assert!(matches!(rx.recv().await, Some(IcedPluginEvent::Close { .. })));
    let program = match rx.recv().await.unwrap() {
      IcedPluginEvent::Create { program } => program,
      _ => panic!("program must arrive first"),
    };
    assert!(matches!(rx.recv().await, Some(IcedPluginEvent::Task(message)) if message.program_id() == program.__id()));
    task.await.unwrap();
  }

  #[test]
  fn dispatch_admission_reports_full_and_stopped_without_reordering() {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let mut pending = PendingCommands {
      queue: (0..256)
        .map(|id| IcedPluginEvent::Close {
          program_id: ProgramId::new("queued", id),
        })
        .collect(),
      draining: true,
    };
    assert_eq!(
      pending.admit(&tx, IcedPluginEvent::Task(Arc::new(ProbeMessage))),
      Err(ProgramCommandError::QueueFull)
    );
    assert_eq!(pending.queue.len(), 256);
    assert_eq!(
      pending.admit(
        &tx,
        IcedPluginEvent::Create {
          program: Box::new(ProbeProgram)
        }
      ),
      Err(ProgramCommandError::QueueFull)
    );
    assert_eq!(pending.queue.len(), 256);
    drop(rx);
    assert_eq!(
      pending.admit(&tx, IcedPluginEvent::Task(Arc::new(ProbeMessage))),
      Err(ProgramCommandError::RendererStopped)
    );
    assert_eq!(pending.queue.len(), 256);
  }

  #[tokio::test]
  async fn queued_commands_preserve_order_past_channel_capacity() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let pending = Arc::new(Mutex::new(PendingCommands {
      queue: (0..20)
        .map(|id| IcedPluginEvent::Close {
          program_id: ProgramId::new("queued", id),
        })
        .collect(),
      draining: true,
    }));
    let task = tokio::spawn(drain_pending(tx, pending.clone()));
    for id in 0..20 {
      assert!(
        matches!(rx.recv().await, Some(IcedPluginEvent::Close { program_id }) if program_id == ProgramId::new("queued", id))
      );
    }
    task.await.unwrap();
    assert!(!pending.lock().unwrap().draining);
    assert!(pending.lock().unwrap().queue.is_empty());
  }

  #[tokio::test]
  async fn renderer_stop_releases_queued_program_commands() {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let pending = Arc::new(Mutex::new(PendingCommands {
      queue: (0..20)
        .map(|id| IcedPluginEvent::Close {
          program_id: ProgramId::new("queued", id),
        })
        .collect(),
      draining: true,
    }));
    drop(rx);
    drain_pending(tx, pending.clone()).await;
    assert!(!pending.lock().unwrap().draining);
    assert!(pending.lock().unwrap().queue.is_empty());
  }
}
