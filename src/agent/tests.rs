use super::*;
use crate::core::tasks::{TaskScope, TaskTracker};
use std::{
  future::{Future, pending},
  sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicUsize, Ordering},
  },
  task::Poll,
  time::Duration,
};
use tokio::{runtime::Handle, sync::Notify};
fn spec(id: &str, effect: ToolEffect) -> ToolSpec {
  ToolSpec::new(id, "Application-owned test tool", json!({"type":"integer"}), effect)
}
fn registry(counter: Arc<AtomicUsize>) -> Arc<ToolRegistry> {
  let mut registry = ToolRegistry::new();
  registry
    .register::<i64, i64, _, _>(spec("example.read", ToolEffect::ReadOnly), move |_, input| {
      counter.fetch_add(1, Ordering::SeqCst);
      async move { Ok(input + 1) }
    })
    .unwrap();
  registry
    .register::<i64, i64, _, _>(
      spec("example.write", ToolEffect::LocalMutation),
      |_, input| async move { Ok(input) },
    )
    .unwrap();
  Arc::new(registry)
}
fn session(tracker: &TaskTracker, registry: Arc<ToolRegistry>, policy: ToolPolicy, limits: ToolLimits) -> ToolSession {
  ToolSession::tracked(tracker.clone(), TaskScope::Window(7), registry, policy, limits).unwrap()
}
#[tokio::test]
async fn default_deny_and_exact_effect_grants_filter_discovery_and_execution() {
  let tracker = TaskTracker::new();
  let calls = Arc::new(AtomicUsize::new(0));
  let registry = registry(calls.clone());
  assert_eq!(registry.catalog().schema_version, 1);
  assert_eq!(registry.catalog().kind, "lintel.agent.tools");
  assert_eq!(registry.catalog().tools.len(), 2);
  let denied = session(&tracker, registry.clone(), ToolPolicy::default(), ToolLimits::default());
  assert!(denied.catalog().unwrap().tools.is_empty());
  assert_eq!(
    denied.invoke_json("example.read", b"1").unwrap_err().code,
    ToolErrorCode::Denied
  );
  assert_eq!(calls.load(Ordering::SeqCst), 0);
  let reader = session(
    &tracker,
    registry.clone(),
    ToolPolicy::read_only(),
    ToolLimits::default(),
  );
  assert_eq!(reader.catalog().unwrap().tools.len(), 1);
  assert_eq!(
    reader.invoke_json("example.write", b"1").unwrap_err().code,
    ToolErrorCode::Denied
  );
  assert_eq!(
    reader.invoke_json("example.read", b"1").unwrap().join().await.unwrap(),
    json!(2)
  );
  let wrong = ToolPolicy::allow([ToolGrant::new("example.write", ToolEffect::ReadOnly).unwrap()]).unwrap();
  let mismatch = session(&tracker, registry.clone(), wrong, ToolLimits::default());
  assert!(mismatch.catalog().unwrap().tools.is_empty());
  assert_eq!(
    mismatch.invoke_json("example.write", b"1").unwrap_err().code,
    ToolErrorCode::Denied
  );
  let exact = ToolPolicy::allow([ToolGrant::new("example.write", ToolEffect::LocalMutation).unwrap()]).unwrap();
  let writer = session(&tracker, registry, exact, ToolLimits::default());
  assert_eq!(
    writer.invoke_json("example.write", b"3").unwrap().join().await.unwrap(),
    json!(3)
  );
  denied.shutdown().await;
  reader.shutdown().await;
  mismatch.shutdown().await;
  writer.shutdown().await;
  tracker.wait_idle().await;
}
#[tokio::test]
async fn typed_serde_domain_validation_and_size_errors_never_echo_input_or_internal_errors() {
  let tracker = TaskTracker::new();
  let calls = Arc::new(AtomicUsize::new(0));
  let count = calls.clone();
  let mut registry = ToolRegistry::new();
  registry
    .register_validated::<i64, i64, _, _, _>(
      spec("check", ToolEffect::ReadOnly),
      |value| *value >= 0,
      move |_, value| {
        count.fetch_add(1, Ordering::SeqCst);
        async move { Ok(value) }
      },
    )
    .unwrap();
  registry
    .register::<i64, String, _, _>(spec("large", ToolEffect::ReadOnly), |_, _| async move {
      Ok("x".repeat(100))
    })
    .unwrap();
  registry
    .register::<i64, i64, _, _>(spec("failure", ToolEffect::ReadOnly), |_, _| async move {
      Err(std::io::Error::other("internal fixture detail").into())
    })
    .unwrap();
  let limits = ToolLimits {
    max_input_bytes: 16,
    max_output_bytes: 16,
    ..Default::default()
  };
  let session = session(&tracker, Arc::new(registry), ToolPolicy::read_only(), limits);
  for input in [&b"\"private-input\""[..], &b"-1"[..]] {
    let error = session.invoke_json("check", input).unwrap().join().await.unwrap_err();
    assert_eq!(error.code, ToolErrorCode::InvalidInput);
    assert!(!format!("{error:?} {error}").contains("private-input"));
  }
  assert_eq!(calls.load(Ordering::SeqCst), 0);
  assert_eq!(
    session.invoke_json("check", &[b' '; 17]).unwrap_err().code,
    ToolErrorCode::InputTooLarge
  );
  assert_eq!(
    session
      .invoke_json("large", b"0")
      .unwrap()
      .join()
      .await
      .unwrap_err()
      .code,
    ToolErrorCode::OutputTooLarge
  );
  let error = session.invoke_json("failure", b"0").unwrap().join().await.unwrap_err();
  assert_eq!(error.code, ToolErrorCode::HandlerFailed);
  assert!(!format!("{error:?} {error}").contains("internal fixture"));
  assert_eq!(
    session.invoke_json("missing", b"0").unwrap_err().code,
    ToolErrorCode::ToolNotFound
  );
  session.shutdown().await;
}
#[test]
fn registry_bounds_duplicate_ids_and_advertised_schema_do_not_install_an_evaluator() {
  let mut registry = ToolRegistry::new();
  registry
    .register::<i64, i64, _, _>(spec("duplicate", ToolEffect::ReadOnly), |_, n| async move { Ok(n) })
    .unwrap();
  assert_eq!(
    registry
      .register::<i64, i64, _, _>(spec("duplicate", ToolEffect::External), |_, n| async move { Ok(n) })
      .unwrap_err()
      .code,
    ToolErrorCode::DuplicateTool
  );
  let mut invalid = spec("bad", ToolEffect::ReadOnly);
  invalid.input_schema = json!({"description":"s".repeat(20*1024)});
  assert_eq!(
    registry
      .register::<i64, i64, _, _>(invalid, |_, n| async move { Ok(n) })
      .unwrap_err()
      .code,
    ToolErrorCode::InvalidSpec
  );
  for index in 1..64 {
    registry
      .register::<i64, i64, _, _>(spec(&format!("tool{index}"), ToolEffect::ReadOnly), |_, n| async move {
        Ok(n)
      })
      .unwrap();
  }
  assert_eq!(
    registry
      .register::<i64, i64, _, _>(spec("overflow", ToolEffect::ReadOnly), |_, n| async move { Ok(n) })
      .unwrap_err()
      .code,
    ToolErrorCode::RegistryFull
  );
  assert!(registry.catalog().to_json().unwrap().len() <= 256 * 1024);
}
struct DropProbe {
  drops: Arc<AtomicUsize>,
  owner: Option<Weak<ToolSession>>,
}
impl Drop for DropProbe {
  fn drop(&mut self) {
    if let Some(owner) = self.owner.as_ref().and_then(Weak::upgrade) {
      assert_eq!(
        owner.active_count(),
        1,
        "permit must remain until the handler future is destroyed"
      );
    }
    self.drops.fetch_add(1, Ordering::SeqCst);
  }
}
fn pending_registry(
  started: Arc<Notify>,
  drops: Arc<AtomicUsize>,
  owner: Arc<Mutex<Option<Weak<ToolSession>>>>,
) -> Arc<ToolRegistry> {
  let mut registry = ToolRegistry::new();
  registry
    .register::<i64, i64, _, _>(spec("pending", ToolEffect::ReadOnly), move |_, _| {
      let probe = DropProbe {
        drops: drops.clone(),
        owner: owner.lock().unwrap().clone(),
      };
      let started = started.clone();
      async move {
        let _probe = probe;
        started.notify_one();
        pending::<()>().await;
        Ok(1)
      }
    })
    .unwrap();
  Arc::new(registry)
}
#[tokio::test]
async fn busy_and_cancel_keep_the_permit_until_real_handler_cleanup() {
  let tracker = TaskTracker::new();
  let started = Arc::new(Notify::new());
  let drops = Arc::new(AtomicUsize::new(0));
  let owner = Arc::new(Mutex::new(None));
  let registry = pending_registry(started.clone(), drops.clone(), owner.clone());
  let session = Arc::new(session(
    &tracker,
    registry,
    ToolPolicy::read_only(),
    ToolLimits {
      max_concurrency: 1,
      ..Default::default()
    },
  ));
  *owner.lock().unwrap() = Some(Arc::downgrade(&session));
  let invocation = session.invoke_json("pending", b"0").unwrap();
  started.notified().await;
  assert_eq!(
    session.invoke_json("pending", b"0").unwrap_err().code,
    ToolErrorCode::Busy
  );
  invocation.cancel();
  assert_eq!(invocation.join().await.unwrap_err().code, ToolErrorCode::Cancelled);
  assert_eq!(drops.load(Ordering::SeqCst), 1);
  assert_eq!(session.active_count(), 0);
  session.shutdown().await;
  tracker.wait_idle().await;
}
#[tokio::test]
async fn cooperative_timeout_cleans_up_the_handler_and_releases_its_slot() {
  let tracker = TaskTracker::new();
  let started = Arc::new(Notify::new());
  let drops = Arc::new(AtomicUsize::new(0));
  let owner = Arc::new(Mutex::new(None));
  let session = Arc::new(session(
    &tracker,
    pending_registry(started, drops.clone(), owner.clone()),
    ToolPolicy::read_only(),
    ToolLimits {
      max_concurrency: 1,
      timeout: Duration::from_millis(15),
      ..Default::default()
    },
  ));
  *owner.lock().unwrap() = Some(Arc::downgrade(&session));
  assert_eq!(
    session
      .invoke_json("pending", b"0")
      .unwrap()
      .join()
      .await
      .unwrap_err()
      .code,
    ToolErrorCode::Timeout
  );
  assert_eq!(drops.load(Ordering::SeqCst), 1);
  assert_eq!(session.active_count(), 0);
  session.shutdown().await;
}
#[tokio::test]
async fn dropping_invocation_or_unpolled_and_polled_join_requests_cancellation() {
  for mode in 0..3 {
    let tracker = TaskTracker::new();
    let started = Arc::new(Notify::new());
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = Arc::new(Mutex::new(None));
    let session = Arc::new(session(
      &tracker,
      pending_registry(started.clone(), drops.clone(), owner.clone()),
      ToolPolicy::read_only(),
      ToolLimits::default(),
    ));
    *owner.lock().unwrap() = Some(Arc::downgrade(&session));
    let invocation = session.invoke_json("pending", b"0").unwrap();
    let cancellation = invocation.cancellation_handle();
    if mode == 0 {
      drop(invocation);
    } else if mode == 1 {
      drop(invocation.join());
    } else {
      started.notified().await;
      let mut join = Box::pin(invocation.join());
      std::future::poll_fn(|cx| {
        assert!(join.as_mut().poll(cx).is_pending());
        Poll::Ready(())
      })
      .await;
      drop(join);
    }
    assert!(cancellation.is_cancelled());
    session.shutdown().await;
    tracker.wait_idle().await;
    assert!(cancellation.is_finished());
    assert_eq!(session.active_count(), 0);
    if mode == 2 {
      assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
  }
}
#[tokio::test]
async fn closing_a_session_never_cancels_other_sessions_or_business_in_the_same_scope() {
  let tracker = TaskTracker::new();
  let scope = TaskScope::Window(7);
  let business = tracker
    .spawn(&Handle::current(), scope.clone(), pending::<()>())
    .unwrap();
  let registry = pending_registry(
    Arc::new(Notify::new()),
    Arc::new(AtomicUsize::new(0)),
    Arc::new(Mutex::new(None)),
  );
  let a = session(
    &tracker,
    registry.clone(),
    ToolPolicy::read_only(),
    ToolLimits::default(),
  );
  let b = session(&tracker, registry, ToolPolicy::read_only(), ToolLimits::default());
  let own = a.invoke_json("pending", b"0").unwrap();
  let other = b.invoke_json("pending", b"0").unwrap();
  a.shutdown().await;
  assert_eq!(own.join().await.unwrap_err().code, ToolErrorCode::Cancelled);
  assert!(!business.cancellation_handle().is_cancelled());
  assert!(!other.cancellation_handle().is_cancelled());
  assert_eq!(
    a.invoke_json("pending", b"0").unwrap_err().code,
    ToolErrorCode::SessionClosed
  );
  b.close();
  assert_eq!(other.join().await.unwrap_err().code, ToolErrorCode::Cancelled);
  business.cancel();
  let _ = business.join().await;
  tracker.wait_idle().await;
}
#[tokio::test]
async fn unpolled_scope_sentinel_closes_the_old_session_but_not_a_new_scope_binding() {
  let tracker = TaskTracker::new();
  let registry = registry(Arc::new(AtomicUsize::new(0)));
  let old = ToolSession::tracked(
    tracker.clone(),
    TaskScope::page(7, "settings"),
    registry.clone(),
    ToolPolicy::read_only(),
    ToolLimits::default(),
  )
  .unwrap();
  // No yield: the sentinel has not been polled, but core cancellation is already authoritative.
  tracker.cancel_scope(&TaskScope::Window(7));
  assert_eq!(
    old.invoke_json("example.read", b"1").unwrap_err().code,
    ToolErrorCode::ScopeClosed
  );
  tracker.wait_idle().await;
  assert_eq!(old.catalog().unwrap_err().code, ToolErrorCode::ScopeClosed);
  let fresh = ToolSession::tracked(
    tracker.clone(),
    TaskScope::page(7, "settings"),
    registry,
    ToolPolicy::read_only(),
    ToolLimits::default(),
  )
  .unwrap();
  assert_eq!(
    fresh.invoke_json("example.read", b"1").unwrap().join().await.unwrap(),
    json!(2)
  );
  fresh.shutdown().await;
}
#[tokio::test]
async fn progress_backpressure_budget_and_stale_contexts_have_explicit_numeric_results() {
  let tracker = TaskTracker::new();
  let started = Arc::new(Notify::new());
  let proceed = Arc::new(Notify::new());
  let saved = Arc::new(Mutex::new(None::<ToolContext>));
  let notify = started.clone();
  let gate = proceed.clone();
  let out = saved.clone();
  let mut registry = ToolRegistry::new();
  registry
    .register::<i64, i64, _, _>(spec("progress", ToolEffect::ReadOnly), move |context, _| {
      let notify = notify.clone();
      let gate = gate.clone();
      *out.lock().unwrap() = Some(context.clone());
      async move {
        assert_eq!(context.progress(0, Some(0)), Err(ProgressError::InvalidProgress));
        context.progress(1, Some(3))?;
        assert_eq!(context.progress(2, Some(3)), Err(ProgressError::Backpressure));
        notify.notify_one();
        gate.notified().await;
        context.progress(2, Some(3))?;
        assert_eq!(context.progress(3, Some(3)), Err(ProgressError::LimitReached));
        Ok(5)
      }
    })
    .unwrap();
  let session = session(
    &tracker,
    Arc::new(registry),
    ToolPolicy::read_only(),
    ToolLimits {
      progress_capacity: 1,
      max_progress_events: 2,
      ..Default::default()
    },
  );
  let mut invocation = session.invoke_json("progress", b"0").unwrap();
  let mut progress = invocation.take_progress().unwrap();
  started.notified().await;
  let first = progress.recv().await.unwrap();
  assert_eq!((first.sequence, first.completed), (1, 1));
  proceed.notify_one();
  assert_eq!(invocation.join().await.unwrap(), json!(5));
  let second = progress.recv().await.unwrap();
  assert_eq!((second.sequence, second.completed), (2, 2));
  assert!(progress.recv().await.is_none());
  assert_eq!(
    saved.lock().unwrap().as_ref().unwrap().progress(3, Some(3)),
    Err(ProgressError::Closed)
  );
  session.shutdown().await;
}
#[tokio::test]
async fn concurrent_progress_sequence_and_delivery_are_serialized_together() {
  let tracker = TaskTracker::new();
  let mut registry = ToolRegistry::new();
  registry
    .register::<i64, i64, _, _>(spec("concurrent", ToolEffect::ReadOnly), |context, _| async move {
      std::thread::scope(|scope| {
        for completed in 1..=12 {
          let context = context.clone();
          scope.spawn(move || context.progress(completed, Some(12)).unwrap());
        }
      });
      Ok(12)
    })
    .unwrap();
  let session = session(
    &tracker,
    Arc::new(registry),
    ToolPolicy::read_only(),
    ToolLimits {
      progress_capacity: 16,
      max_progress_events: 16,
      ..Default::default()
    },
  );
  let mut invocation = session.invoke_json("concurrent", b"0").unwrap();
  let mut progress = invocation.take_progress().unwrap();
  invocation.join().await.unwrap();
  let mut sequences = Vec::new();
  while let Some(event) = progress.recv().await {
    sequences.push(event.sequence);
  }
  assert_eq!(sequences, (1..=12).collect::<Vec<_>>());
  session.shutdown().await;
}
#[tokio::test]
async fn close_between_reservation_and_commit_cannot_escape_cancellation() {
  let tracker = TaskTracker::new();
  let calls = Arc::new(AtomicUsize::new(0));
  let session = session(
    &tracker,
    registry(calls.clone()),
    ToolPolicy::read_only(),
    ToolLimits::default(),
  );
  let invocation = session
    .invoke_before_commit("example.read", b"1", || {
      session.close();
    })
    .unwrap();
  assert_eq!(invocation.join().await.unwrap_err().code, ToolErrorCode::Cancelled);
  session.shutdown().await;
  assert_eq!(session.active_count(), 0);
  assert_eq!(calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn aborted_before_commit_entry_is_not_resurrected_by_late_registration() {
  let tracker = TaskTracker::new();
  let calls = Arc::new(AtomicUsize::new(0));
  let session = Arc::new(session(
    &tracker,
    registry(calls.clone()),
    ToolPolicy::read_only(),
    ToolLimits::default(),
  ));
  let worker_session = session.clone();
  let (entered, observed) = tokio::sync::oneshot::channel();
  let (resume, resumed) = std::sync::mpsc::sync_channel(0);
  let worker = std::thread::spawn(move || {
    worker_session.invoke_before_commit("example.read", b"1", || {
      let _ = entered.send(());
      resumed.recv().unwrap();
    })
  });
  observed.await.unwrap();
  tracker.cancel_scope(&TaskScope::Window(7));
  tracker.wait_idle().await;
  assert_eq!(session.active_count(), 0);
  resume.send(()).unwrap();
  let invocation = worker.join().unwrap().unwrap();
  assert_eq!(invocation.join().await.unwrap_err().code, ToolErrorCode::Cancelled);
  assert_eq!(session.active_count(), 0);
  assert_eq!(calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn spawn_failure_destroys_the_reserved_future_without_a_lock_reentry_deadlock() {
  let tracker = TaskTracker::new();
  let session = session(
    &tracker,
    registry(Arc::new(AtomicUsize::new(0))),
    ToolPolicy::read_only(),
    ToolLimits::default(),
  );
  let result = session.invoke_before_spawn("example.read", b"1", || {
    tracker.shutdown();
  });
  assert_eq!(result.unwrap_err().code, ToolErrorCode::ScopeClosed);
  assert_eq!(session.active_count(), 0);
  tracker.wait_idle().await;
}
#[tokio::test]
async fn dropping_the_session_cancels_pending_work_without_a_strong_lifetime_cycle() {
  let tracker = TaskTracker::new();
  let session = Arc::new(session(
    &tracker,
    pending_registry(
      Arc::new(Notify::new()),
      Arc::new(AtomicUsize::new(0)),
      Arc::new(Mutex::new(None)),
    ),
    ToolPolicy::read_only(),
    ToolLimits::default(),
  ));
  let weak = Arc::downgrade(&session);
  let invocation = session.invoke_json("pending", b"0").unwrap();
  drop(session);
  assert!(weak.upgrade().is_none());
  assert_eq!(invocation.join().await.unwrap_err().code, ToolErrorCode::Cancelled);
  tracker.wait_idle().await;
}
#[tokio::test]
async fn panics_in_typed_decode_validator_factory_and_serialization_return_only_a_fixed_code() {
  struct PanicDecode;
  impl<'de> serde::Deserialize<'de> for PanicDecode {
    fn deserialize<D: serde::Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
      panic!("test decode panic");
    }
  }
  struct PanicSerialize;
  impl serde::Serialize for PanicSerialize {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
      panic!("test serialization panic");
    }
  }
  let mut registry = ToolRegistry::new();
  registry
    .register::<PanicDecode, (), _, _>(spec("decode", ToolEffect::ReadOnly), |_, _| async { Ok(()) })
    .unwrap();
  registry
    .register_validated::<i64, (), _, _, _>(
      spec("validator", ToolEffect::ReadOnly),
      |_| panic!("test domain panic"),
      |_, _| async { Ok(()) },
    )
    .unwrap();
  registry
    .register::<i64, (), _, _>(
      spec("factory", ToolEffect::ReadOnly),
      |_, _| -> std::future::Ready<crate::Result<()>> { panic!("test factory panic") },
    )
    .unwrap();
  registry
    .register::<i64, PanicSerialize, _, _>(spec("serialize", ToolEffect::ReadOnly), |_, _| async {
      Ok(PanicSerialize)
    })
    .unwrap();
  let tracker = TaskTracker::new();
  let session = session(
    &tracker,
    Arc::new(registry),
    ToolPolicy::read_only(),
    ToolLimits::default(),
  );
  for id in ["decode", "validator", "factory", "serialize"] {
    let error = session.invoke_json(id, b"1").unwrap().join().await.unwrap_err();
    assert_eq!(error.code, ToolErrorCode::HandlerPanicked);
    assert!(!format!("{error:?} {error}").contains("test "));
    assert_eq!(session.active_count(), 0);
  }
  session.shutdown().await;
}

#[tokio::test]
async fn a_late_non_yielding_poll_is_not_preempted_but_cannot_report_success() {
  let tracker = TaskTracker::new();
  let mut registry = ToolRegistry::new();
  registry
    .register::<i64, i64, _, _>(spec("late", ToolEffect::ReadOnly), |_, _| async {
      // Deliberately short blocking fixture: production handlers must not do this.
      std::thread::sleep(Duration::from_millis(10));
      Ok(1)
    })
    .unwrap();
  let session = session(
    &tracker,
    Arc::new(registry),
    ToolPolicy::read_only(),
    ToolLimits {
      timeout: Duration::from_millis(1),
      ..Default::default()
    },
  );
  assert_eq!(
    session
      .invoke_json("late", b"0")
      .unwrap()
      .join()
      .await
      .unwrap_err()
      .code,
    ToolErrorCode::Timeout
  );
  session.shutdown().await;
}

#[tokio::test]
async fn failed_serialization_and_invalid_limits_are_redacted_and_do_not_leak_slots() {
  struct RejectedOutput;
  impl serde::Serialize for RejectedOutput {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
      Err(serde::ser::Error::custom("internal serializer fixture detail"))
    }
  }
  let tracker = TaskTracker::new();
  let mut registry = ToolRegistry::new();
  registry
    .register::<i64, RejectedOutput, _, _>(spec("rejected", ToolEffect::ReadOnly), |_, _| async {
      Ok(RejectedOutput)
    })
    .unwrap();
  let registry = Arc::new(registry);
  let invalid = ToolSession::tracked(
    tracker.clone(),
    TaskScope::Window(7),
    registry.clone(),
    ToolPolicy::read_only(),
    ToolLimits {
      max_concurrency: 0,
      ..Default::default()
    },
  );
  assert_eq!(invalid.err().unwrap().code, ToolErrorCode::InvalidLimits);
  assert_eq!(tracker.active_count(), 0);
  let invalid = ToolSession::tracked(
    tracker.clone(),
    TaskScope::Window(7),
    registry.clone(),
    ToolPolicy::read_only(),
    ToolLimits {
      timeout: Duration::from_secs(301),
      ..Default::default()
    },
  );
  assert_eq!(invalid.err().unwrap().code, ToolErrorCode::InvalidLimits);
  let session = session(&tracker, registry, ToolPolicy::read_only(), ToolLimits::default());
  let error = session.invoke_json("rejected", b"0").unwrap().join().await.unwrap_err();
  assert_eq!(error.code, ToolErrorCode::InvalidOutput);
  assert!(!format!("{error:?} {error}").contains("internal serializer"));
  assert_eq!(session.active_count(), 0);
  session.shutdown().await;
}
