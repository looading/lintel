use std::{
  future::Future,
  io,
  time::{Duration, Instant},
};

use mojo_service::{ProcessSnapshot, ProcessState};
use tokio::sync::broadcast;

use crate::ProcessHost;

pub(crate) fn wait(
  host: ProcessHost,
  identity: String,
  previous_attempt: u64,
  timeout: Duration,
) -> impl Future<Output = io::Result<ProcessSnapshot>> + Send + 'static {
  let deadline = deadline(&identity, timeout);
  // 先订阅，防止检查快照与开始等待之间的状态通知丢失。
  let events = host.subscribe_events();
  async move {
    wait_state(
      &identity,
      previous_attempt,
      deadline?,
      events,
      || host.snapshots(),
      || host.is_stopping(),
    )
    .await
  }
}

fn deadline(identity: &str, timeout: Duration) -> io::Result<Instant> {
  if identity.is_empty()
    || identity.len() > 256
    || identity.chars().any(char::is_control)
    || timeout.is_zero()
    || timeout > Duration::from_secs(300)
  {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "valid process identity and readiness timeout in (0, 300s] required",
    ));
  }
  Instant::now()
    .checked_add(timeout)
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "readiness deadline overflow"))
}

/// 将真实快照的就绪决策与等待过程独立，测试不伪造上游 ProcessId 或加载 FFI。
trait ReadinessSnapshot {
  fn identity(&self) -> &str;
  fn state(&self) -> ProcessState;
  fn attempts(&self) -> u64;
}

impl ReadinessSnapshot for ProcessSnapshot {
  fn identity(&self) -> &str {
    &self.identity
  }
  fn state(&self) -> ProcessState {
    self.state
  }
  fn attempts(&self) -> u64 {
    self.attempts
  }
}

async fn wait_state<S: ReadinessSnapshot, E: Clone>(
  identity: &str,
  previous_attempt: u64,
  deadline: Instant,
  mut events: broadcast::Receiver<E>,
  mut snapshots: impl FnMut() -> Vec<S>,
  mut stopping: impl FnMut() -> bool,
) -> io::Result<S> {
  loop {
    if stopping() {
      return Err(io::Error::new(io::ErrorKind::BrokenPipe, "process host is stopping"));
    }
    if Instant::now() >= deadline {
      return Err(io::Error::new(io::ErrorKind::TimedOut, "process readiness timed out"));
    }
    let mut matching = snapshots()
      .into_iter()
      .filter(|snapshot| snapshot.identity() == identity);
    let snapshot = matching
      .next()
      .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown process identity"))?;
    if matching.next().is_some() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "process identity is ambiguous",
      ));
    }
    if Instant::now() >= deadline {
      return Err(io::Error::new(io::ErrorKind::TimedOut, "process readiness timed out"));
    }
    match snapshot.state() {
      ProcessState::Ready if snapshot.attempts() > previous_attempt => return Ok(snapshot),
      ProcessState::Failed | ProcessState::Stopped => {
        return Err(io::Error::new(
          io::ErrorKind::BrokenPipe,
          "process stopped before requested readiness",
        ));
      }
      _ => {}
    }
    match tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), events.recv()).await {
      Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
      Ok(Err(broadcast::error::RecvError::Closed)) => {
        return Err(io::Error::new(
          io::ErrorKind::BrokenPipe,
          "process host event stream closed",
        ));
      }
      Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "process readiness timed out")),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{
    cell::Cell,
    sync::{
      Arc,
      atomic::{AtomicBool, Ordering},
    },
  };

  #[derive(Clone, Debug)]
  struct Snapshot {
    identity: &'static str,
    state: ProcessState,
    attempt: u64,
  }
  impl ReadinessSnapshot for Snapshot {
    fn identity(&self) -> &str {
      self.identity
    }
    fn state(&self) -> ProcessState {
      self.state
    }
    fn attempts(&self) -> u64 {
      self.attempt
    }
  }
  fn snapshot(state: ProcessState, attempt: u64) -> Snapshot {
    Snapshot {
      identity: "echo",
      state,
      attempt,
    }
  }
  fn due() -> Instant {
    Instant::now() + Duration::from_secs(1)
  }

  #[tokio::test(flavor = "current_thread")]
  async fn already_ready_returns_snapshot_and_new_attempt_rejects_old_ready() {
    let (send, events) = broadcast::channel::<()>(2);
    let ready = wait_state(
      "echo",
      0,
      due(),
      events,
      || vec![snapshot(ProcessState::Ready, 1)],
      || false,
    )
    .await
    .unwrap();
    assert_eq!(ready.attempt, 1);
    let events = send.subscribe();
    let read = Cell::new(0);
    send.send(()).unwrap();
    let newer = wait_state(
      "echo",
      1,
      due(),
      events,
      || {
        let attempt = read.get() + 1;
        read.set(attempt);
        vec![snapshot(ProcessState::Ready, attempt)]
      },
      || false,
    )
    .await
    .unwrap();
    assert_eq!(newer.attempt, 2);
  }

  #[tokio::test(flavor = "current_thread")]
  async fn event_during_first_snapshot_is_not_lost() {
    let (send, events) = broadcast::channel::<()>(1);
    let initial = Cell::new(true);
    let ready = wait_state(
      "echo",
      0,
      due(),
      events,
      || {
        if initial.replace(false) {
          send.send(()).unwrap();
          vec![snapshot(ProcessState::Starting, 1)]
        } else {
          vec![snapshot(ProcessState::Ready, 1)]
        }
      },
      || false,
    )
    .await
    .unwrap();
    assert_eq!(ready.state, ProcessState::Ready);
  }

  #[tokio::test(flavor = "current_thread")]
  async fn lagged_events_recheck_snapshot_instead_of_failing() {
    let (send, events) = broadcast::channel::<()>(1);
    send.send(()).unwrap();
    send.send(()).unwrap();
    send.send(()).unwrap();
    let initial = Cell::new(true);
    let ready = wait_state(
      "echo",
      0,
      due(),
      events,
      || {
        vec![snapshot(
          if initial.replace(false) {
            ProcessState::Starting
          } else {
            ProcessState::Ready
          },
          1,
        )]
      },
      || false,
    )
    .await
    .unwrap();
    assert_eq!(ready.state, ProcessState::Ready);
  }

  #[tokio::test(flavor = "current_thread")]
  async fn terminal_unknown_and_ambiguous_processes_report_stable_errors() {
    let (send, _) = broadcast::channel::<()>(1);
    for state in [ProcessState::Failed, ProcessState::Stopped] {
      assert_eq!(
        wait_state(
          "echo",
          0,
          due(),
          send.subscribe(),
          || vec![snapshot(state, 1)],
          || false
        )
        .await
        .unwrap_err()
        .kind(),
        io::ErrorKind::BrokenPipe
      );
    }
    assert_eq!(
      wait_state(
        "missing",
        0,
        due(),
        send.subscribe(),
        || vec![snapshot(ProcessState::Ready, 1)],
        || false
      )
      .await
      .unwrap_err()
      .kind(),
      io::ErrorKind::NotFound
    );
    assert_eq!(
      wait_state(
        "echo",
        0,
        due(),
        send.subscribe(),
        || vec![snapshot(ProcessState::Ready, 1), snapshot(ProcessState::Ready, 2)],
        || false
      )
      .await
      .unwrap_err()
      .kind(),
      io::ErrorKind::InvalidInput
    );
  }

  #[tokio::test(flavor = "current_thread")]
  async fn shutdown_and_closed_events_cannot_report_ready() {
    let (send, events) = broadcast::channel::<()>(1);
    assert_eq!(
      wait_state(
        "echo",
        0,
        due(),
        events,
        || vec![snapshot(ProcessState::Ready, 1)],
        || true
      )
      .await
      .unwrap_err()
      .kind(),
      io::ErrorKind::BrokenPipe
    );
    let events = send.subscribe();
    drop(send);
    assert_eq!(
      wait_state(
        "echo",
        0,
        due(),
        events,
        || vec![snapshot(ProcessState::Starting, 1)],
        || false
      )
      .await
      .unwrap_err()
      .kind(),
      io::ErrorKind::BrokenPipe
    );
  }

  #[tokio::test(flavor = "current_thread")]
  async fn readiness_wait_times_out_and_does_not_reset_deadline_on_events() {
    let (send, events) = broadcast::channel::<()>(1);
    send.send(()).unwrap();
    assert_eq!(
      wait_state(
        "echo",
        0,
        Instant::now() + Duration::from_millis(5),
        events,
        || vec![snapshot(ProcessState::Starting, 1)],
        || false
      )
      .await
      .unwrap_err()
      .kind(),
      io::ErrorKind::TimedOut
    );
    let slow_read_deadline = Instant::now() + Duration::from_millis(1);
    assert_eq!(
      wait_state(
        "echo",
        0,
        slow_read_deadline,
        send.subscribe(),
        || {
          std::thread::sleep(Duration::from_millis(3));
          vec![snapshot(ProcessState::Ready, 1)]
        },
        || false
      )
      .await
      .unwrap_err()
      .kind(),
      io::ErrorKind::TimedOut
    );
    let already_expired = Instant::now() - Duration::from_secs(1);
    assert_eq!(
      wait_state(
        "echo",
        0,
        already_expired,
        send.subscribe(),
        || vec![snapshot(ProcessState::Ready, 1)],
        || false
      )
      .await
      .unwrap_err()
      .kind(),
      io::ErrorKind::TimedOut
    );
  }

  #[tokio::test(flavor = "current_thread")]
  async fn shutdown_after_first_check_wakes_to_error() {
    let (send, events) = broadcast::channel::<()>(1);
    let stop = Arc::new(AtomicBool::new(false));
    let ready = wait_state(
      "echo",
      0,
      due(),
      events,
      || {
        stop.store(true, Ordering::Release);
        send.send(()).unwrap();
        vec![snapshot(ProcessState::Starting, 1)]
      },
      || stop.load(Ordering::Acquire),
    )
    .await;
    assert_eq!(ready.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
  }

  #[test]
  fn invalid_timeout_and_identity_are_rejected_and_public_futures_are_send_static() {
    for timeout in [Duration::ZERO, Duration::from_secs(301)] {
      assert_eq!(
        deadline("echo", timeout).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
      );
    }
    for identity in ["", "echo\n"] {
      assert_eq!(
        deadline(identity, Duration::from_secs(1)).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
      );
    }
    fn contract(host: &ProcessHost) {
      fn send_static<T: Send + 'static>(_: T) {}
      send_static(host.wait_ready("echo", Duration::from_secs(1)));
      send_static(host.wait_ready_after("echo", 1, Duration::from_secs(1)));
    }
    let _ = contract;
  }
}
