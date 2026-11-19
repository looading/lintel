use serde::Serialize;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Numeric-only, no implicit text/input/error logging. Sequence and sending are serialized together.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ProgressEvent {
  pub invocation_id: u64,
  pub sequence: u64,
  pub completed: u64,
  pub total: Option<u64>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProgressError {
  InvalidProgress,
  Backpressure,
  LimitReached,
  Closed,
}
impl std::fmt::Display for ProgressError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(match self {
      Self::InvalidProgress => "progress values are invalid",
      Self::Backpressure => "progress queue is full",
      Self::LimitReached => "progress event budget reached",
      Self::Closed => "progress stream is closed",
    })
  }
}
impl std::error::Error for ProgressError {}
pub struct ProgressStream {
  receiver: mpsc::Receiver<ProgressEvent>,
}
impl ProgressStream {
  pub async fn recv(&mut self) -> Option<ProgressEvent> {
    self.receiver.recv().await
  }
  pub fn try_recv(&mut self) -> Result<ProgressEvent, mpsc::error::TryRecvError> {
    self.receiver.try_recv()
  }
}
struct State {
  sender: Option<mpsc::Sender<ProgressEvent>>,
  sent: u64,
  budget: u64,
}
pub(super) struct Progress {
  id: u64,
  state: Mutex<State>,
}
impl Progress {
  pub fn new(id: u64, capacity: usize, budget: usize) -> (Arc<Self>, ProgressStream) {
    let (sender, receiver) = mpsc::channel(capacity);
    (
      Arc::new(Self {
        id,
        state: Mutex::new(State {
          sender: Some(sender),
          sent: 0,
          budget: budget as u64,
        }),
      }),
      ProgressStream { receiver },
    )
  }
  pub fn close(&self) {
    self.state.lock().unwrap().sender.take();
  }
  pub fn report(&self, completed: u64, total: Option<u64>) -> Result<(), ProgressError> {
    let mut state = self.state.lock().unwrap();
    let sender = state.sender.as_ref().ok_or(ProgressError::Closed)?;
    if sender.is_closed() {
      state.sender = None;
      return Err(ProgressError::Closed);
    }
    if total.is_some_and(|total| total == 0 || completed > total) {
      return Err(ProgressError::InvalidProgress);
    }
    if state.sent == state.budget {
      return Err(ProgressError::LimitReached);
    }
    let event = ProgressEvent {
      invocation_id: self.id,
      sequence: state.sent + 1,
      completed,
      total,
    };
    match sender.try_send(event) {
      Ok(()) => {
        state.sent += 1;
        Ok(())
      }
      Err(mpsc::error::TrySendError::Full(_)) => Err(ProgressError::Backpressure),
      Err(mpsc::error::TrySendError::Closed(_)) => {
        state.sender = None;
        Err(ProgressError::Closed)
      }
    }
  }
}
