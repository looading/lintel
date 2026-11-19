use std::{
  collections::{HashMap, VecDeque},
  sync::{
    LazyLock, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
  },
  time::{Duration, Instant},
};

pub const UI_PERF_ENV_VAR: &str = "LINTEL_UI_PERF";
pub const UI_PERF_OPEN_DEVTOOL_ENV_VAR: &str = "LINTEL_UI_PERF_OPEN_DEVTOOL";
pub const UI_PERF_CMD_SWITCH: &str = "lintel-ui-perf";
pub const UI_PERF_OPEN_DEVTOOL_CMD_SWITCH: &str = "lintel-ui-perf-open-devtool";
pub const SLOW_UPDATE_THRESHOLD_MS: u64 = 48;
pub const SLOW_VIEW_THRESHOLD_MS: u64 = 32;
pub const SLOW_EVENT_LOOP_GAP_THRESHOLD_MS: u64 = 100;
pub const SLOW_EVENT_LOOP_TASK_THRESHOLD_MS: u64 = 48;
pub const SLOW_CEF_PUMP_THRESHOLD_MS: u64 = 50;
pub const SLOW_WEBVIEW_FRAME_THRESHOLD_MS: u64 = 32;
pub const SLOW_WEBVIEW_PAINT_THRESHOLD_MS: u64 = 16;
pub const SLOW_BUSINESS_STAGE_THRESHOLD_MS: u64 = 48;
const MAX_SLOW_EVENTS: usize = 30;
const MAX_PENDING_SAMPLES: usize = 4096;
const CONSUMER_REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const WEBVIEW_INTERACTION_SAMPLE_WINDOW: Duration = Duration::from_millis(500);

static STORE: LazyLock<Mutex<PerfStore>> = LazyLock::new(|| Mutex::new(PerfStore::default()));
static SAMPLE_BUFFER: LazyLock<Mutex<VecDeque<PerfSample>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));
static CACHED_SNAPSHOT: LazyLock<Mutex<UiPerfSnapshot>> = LazyLock::new(|| Mutex::new(empty_snapshot()));
static WEBVIEW_INTERACTION_ACTIVE_UNTIL: LazyLock<Mutex<HashMap<i32, Instant>>> =
  LazyLock::new(|| Mutex::new(HashMap::new()));
static ENABLED: AtomicBool = AtomicBool::new(false);
static CONTROL_ALLOWED: AtomicBool = AtomicBool::new(false);
static CONSUMER_STARTED: AtomicBool = AtomicBool::new(false);
static SAMPLES_DROPPED: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct PerfStore {
  entries: HashMap<MetricKey, PerfEntry>,
  slow_events: Vec<SlowEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum MetricKind {
  Update,
  View,
  EventLoopGap,
  EventLoopTask,
  CefPump,
  CefFrameArrival,
  CefForegroundFrameArrival,
  CefFrameConsume,
  WebviewInteractionFrameToPresent,
  WebviewPaint,
  WebviewGpuPoll,
  BusinessStage,
}

impl MetricKind {
  fn label(self) -> &'static str {
    match self {
      Self::Update => "update",
      Self::View => "view",
      Self::EventLoopGap => "event_loop_gap",
      Self::EventLoopTask => "event_loop_task",
      Self::CefPump => "cef_pump",
      Self::CefFrameArrival => "cef_frame_arrival",
      Self::CefForegroundFrameArrival => "cef_foreground_frame_arrival",
      Self::CefFrameConsume => "cef_frame_consume",
      Self::WebviewInteractionFrameToPresent => "webview_interaction_frame_to_present",
      Self::WebviewPaint => "webview_paint",
      Self::WebviewGpuPoll => "webview_gpu_poll",
      Self::BusinessStage => "business_stage",
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct MetricKey {
  kind: MetricKind,
  name: &'static str,
  browser_id: Option<i32>,
}

#[derive(Default)]
struct PerfEntry {
  count: u64,
  total_us: u64,
  last_us: u64,
  max_us: u64,
  slow_count: u64,
}

struct PerfSample {
  kind: MetricKind,
  name: &'static str,
  browser_id: Option<i32>,
  elapsed_us: u64,
  threshold_ms: u64,
  at: Instant,
}

struct SlowEvent {
  kind: MetricKind,
  name: &'static str,
  browser_id: Option<i32>,
  elapsed_us: u64,
  threshold_ms: u64,
  at: Instant,
}

pub fn set_enabled(enabled: bool) {
  ENABLED.store(enabled, Ordering::Relaxed);
  if !enabled {
    clear_all_webview_interactions();
  }
  if enabled {
    #[cfg(not(test))]
    ensure_consumer_worker();
  }
  update_cached_enabled(enabled);
}

pub fn is_enabled() -> bool {
  ENABLED.load(Ordering::Relaxed)
}

pub fn set_control_allowed(allowed: bool) {
  CONTROL_ALLOWED.store(allowed, Ordering::Relaxed);
}

pub fn is_control_allowed() -> bool {
  CONTROL_ALLOWED.load(Ordering::Relaxed)
}

pub fn try_set_enabled_from_external(enabled: bool) -> bool {
  if !is_control_allowed() {
    return false;
  }
  set_enabled(enabled);
  true
}

pub fn parse_enabled_flag(value: &str) -> bool {
  matches!(
    value.trim().to_ascii_lowercase().as_str(),
    "1" | "true" | "yes" | "y" | "on" | "enable" | "enabled"
  )
}

pub fn is_enabled_by_env() -> bool {
  std::env::var(UI_PERF_ENV_VAR)
    .map(|value| parse_enabled_flag(&value))
    .unwrap_or(false)
}

pub fn is_open_devtool_by_env() -> bool {
  std::env::var(UI_PERF_OPEN_DEVTOOL_ENV_VAR)
    .map(|value| parse_enabled_flag(&value))
    .unwrap_or(false)
}

pub fn measure<T>(operation: impl FnOnce() -> T, recorder: impl FnOnce(Duration)) -> T {
  if !is_enabled() {
    return operation();
  }

  let start = Instant::now();
  let output = operation();
  recorder(start.elapsed());
  output
}

fn record_metric(kind: MetricKind, name: &'static str, elapsed: Duration, threshold_ms: u64) {
  record_metric_with_browser(kind, name, None, elapsed, threshold_ms);
}

fn record_metric_with_browser(
  kind: MetricKind,
  name: &'static str,
  browser_id: Option<i32>,
  elapsed: Duration,
  threshold_ms: u64,
) {
  if !is_enabled() {
    return;
  }
  // UI 热路径不能因为消费侧锁竞争阻塞，拿不到锁就丢样并计数。
  let mut samples = match SAMPLE_BUFFER.try_lock() {
    Ok(samples) => samples,
    Err(_) => {
      record_dropped_sample();
      return;
    }
  };
  let elapsed_us = elapsed.as_micros().min(u64::MAX as u128) as u64;
  if samples.len() >= MAX_PENDING_SAMPLES {
    samples.pop_front();
    record_dropped_sample();
  }
  samples.push_back(PerfSample {
    kind,
    name,
    browser_id,
    elapsed_us,
    threshold_ms,
    at: Instant::now(),
  });
}

fn record_dropped_sample() {
  let _ = SAMPLES_DROPPED.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
    Some(value.saturating_add(1))
  });
}

fn apply_sample(store: &mut PerfStore, sample: PerfSample) {
  let entry = store
    .entries
    .entry(MetricKey {
      kind: sample.kind,
      name: sample.name,
      browser_id: sample.browser_id,
    })
    .or_default();
  entry.count += 1;
  entry.total_us = entry.total_us.saturating_add(sample.elapsed_us);
  entry.last_us = sample.elapsed_us;
  if sample.elapsed_us > entry.max_us {
    entry.max_us = sample.elapsed_us;
  }
  if sample.elapsed_us >= sample.threshold_ms * 1_000 {
    entry.slow_count += 1;
    store.slow_events.push(SlowEvent {
      kind: sample.kind,
      name: sample.name,
      browser_id: sample.browser_id,
      elapsed_us: sample.elapsed_us,
      threshold_ms: sample.threshold_ms,
      at: sample.at,
    });
    if store.slow_events.len() > MAX_SLOW_EVENTS {
      store.slow_events.remove(0);
    }
  }
}

fn drain_pending_samples() -> Vec<PerfSample> {
  let Ok(mut samples) = SAMPLE_BUFFER.lock() else {
    return vec![];
  };
  samples.drain(..).collect()
}

#[cfg(not(test))]
fn ensure_consumer_worker() {
  if CONSUMER_STARTED
    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
    .is_err()
  {
    return;
  }

  let spawn_result = std::thread::Builder::new()
    .name("ui_perf_consumer".to_string())
    .spawn(|| {
      loop {
        std::thread::sleep(CONSUMER_REFRESH_INTERVAL);
        if is_enabled() {
          refresh_cached_snapshot();
        } else {
          update_cached_enabled(false);
        }
      }
    });

  if spawn_result.is_err() {
    CONSUMER_STARTED.store(false, Ordering::Relaxed);
  }
}

pub fn record_update(program: &'static str, elapsed: Duration) {
  record_metric(MetricKind::Update, program, elapsed, SLOW_UPDATE_THRESHOLD_MS);
}

pub fn record_view(program: &'static str, elapsed: Duration) {
  record_metric(MetricKind::View, program, elapsed, SLOW_VIEW_THRESHOLD_MS);
}

pub fn record_event_loop_gap(elapsed: Duration) {
  record_metric(
    MetricKind::EventLoopGap,
    "winit",
    elapsed,
    SLOW_EVENT_LOOP_GAP_THRESHOLD_MS,
  );
}

pub fn record_event_loop_task(name: &'static str, elapsed: Duration) {
  record_metric(
    MetricKind::EventLoopTask,
    name,
    elapsed,
    SLOW_EVENT_LOOP_TASK_THRESHOLD_MS,
  );
}

pub fn record_cef_pump(elapsed: Duration) {
  record_metric(MetricKind::CefPump, "cef", elapsed, SLOW_CEF_PUMP_THRESHOLD_MS);
}

pub fn record_cef_frame_arrival(elapsed: Duration) {
  record_metric(
    MetricKind::CefFrameArrival,
    "webview",
    elapsed,
    SLOW_WEBVIEW_FRAME_THRESHOLD_MS,
  );
}

pub fn record_cef_frame_arrival_for_browser(browser_id: i32, elapsed: Duration) {
  record_metric_with_browser(
    MetricKind::CefFrameArrival,
    "webview",
    Some(browser_id),
    elapsed,
    SLOW_WEBVIEW_FRAME_THRESHOLD_MS,
  );
}

/// 记录当前前台 WebView 的 CEF 帧到达间隔，供应用性能事件聚合使用。
pub fn record_foreground_cef_frame_arrival_for_browser(browser_id: i32, elapsed: Duration) {
  record_metric_with_browser(
    MetricKind::CefForegroundFrameArrival,
    "webview",
    Some(browser_id),
    elapsed,
    SLOW_WEBVIEW_FRAME_THRESHOLD_MS,
  );
}

pub fn record_cef_frame_consume(elapsed: Duration) {
  record_metric(
    MetricKind::CefFrameConsume,
    "webview",
    elapsed,
    SLOW_WEBVIEW_FRAME_THRESHOLD_MS,
  );
}

pub fn record_cef_frame_consume_for_browser(browser_id: i32, elapsed: Duration) {
  record_metric_with_browser(
    MetricKind::CefFrameConsume,
    "webview",
    Some(browser_id),
    elapsed,
    SLOW_WEBVIEW_FRAME_THRESHOLD_MS,
  );
}

/// 记录前台 WebView 从 CEF OnPaint 到最终 present 的端到端耗时。
pub fn record_webview_interaction_frame_to_present_for_browser(browser_id: i32, elapsed: Duration) {
  record_metric_with_browser(
    MetricKind::WebviewInteractionFrameToPresent,
    "interaction",
    Some(browser_id),
    elapsed,
    SLOW_WEBVIEW_FRAME_THRESHOLD_MS,
  );
}

/// 标记前台 WebView 发生了用户交互，后续短时间内的帧到上屏延迟会被采样。
pub fn mark_webview_interaction(browser_id: i32) {
  if !is_enabled() {
    return;
  }
  if let Ok(mut active_until) = WEBVIEW_INTERACTION_ACTIVE_UNTIL.try_lock() {
    active_until.insert(browser_id, Instant::now() + WEBVIEW_INTERACTION_SAMPLE_WINDOW);
  }
}

/// 仅用于 CEF OnPaint 热路径的非阻塞采样门控。
pub fn is_webview_interaction_active(browser_id: i32) -> bool {
  if !is_enabled() {
    return false;
  }
  let now = Instant::now();
  let Ok(mut active_until) = WEBVIEW_INTERACTION_ACTIVE_UNTIL.try_lock() else {
    return false;
  };
  match active_until.get(&browser_id).copied() {
    Some(until) if until >= now => true,
    Some(_) => {
      active_until.remove(&browser_id);
      false
    }
    None => false,
  }
}

/// WebView 离开前台时清理交互采样状态，避免后续同 browser id 复用时误采样。
pub fn clear_webview_interaction(browser_id: i32) {
  if let Ok(mut active_until) = WEBVIEW_INTERACTION_ACTIVE_UNTIL.try_lock() {
    active_until.remove(&browser_id);
  }
}

fn clear_all_webview_interactions() {
  if let Ok(mut active_until) = WEBVIEW_INTERACTION_ACTIVE_UNTIL.try_lock() {
    active_until.clear();
  }
}

pub fn record_webview_paint(name: &'static str, elapsed: Duration) {
  record_metric(MetricKind::WebviewPaint, name, elapsed, SLOW_WEBVIEW_PAINT_THRESHOLD_MS);
}

pub fn record_webview_paint_for_browser(browser_id: i32, name: &'static str, elapsed: Duration) {
  record_metric_with_browser(
    MetricKind::WebviewPaint,
    name,
    Some(browser_id),
    elapsed,
    SLOW_WEBVIEW_PAINT_THRESHOLD_MS,
  );
}

pub fn record_webview_gpu_poll(elapsed: Duration) {
  record_metric(
    MetricKind::WebviewGpuPoll,
    "webview",
    elapsed,
    SLOW_WEBVIEW_PAINT_THRESHOLD_MS,
  );
}

pub fn record_webview_gpu_poll_for_browser(browser_id: i32, elapsed: Duration) {
  record_metric_with_browser(
    MetricKind::WebviewGpuPoll,
    "webview",
    Some(browser_id),
    elapsed,
    SLOW_WEBVIEW_PAINT_THRESHOLD_MS,
  );
}

pub fn record_business_stage(name: &'static str, elapsed: Duration) {
  record_metric(
    MetricKind::BusinessStage,
    name,
    elapsed,
    SLOW_BUSINESS_STAGE_THRESHOLD_MS,
  );
}

#[derive(Debug, Clone)]
pub struct UiPerfEntry {
  pub kind: String,
  pub name: String,
  pub browser_id: Option<i32>,
  pub count: u64,
  pub last_ms: f64,
  pub avg_ms: f64,
  pub max_ms: f64,
  pub slow_count: u64,
}

#[derive(Debug, Clone)]
pub struct UiJankEvent {
  pub kind: String,
  pub name: String,
  pub program: String,
  pub browser_id: Option<i32>,
  pub elapsed_ms: f64,
  pub threshold_ms: u64,
  pub secs_ago: u64,
}

pub type SlowEventEntry = UiJankEvent;

#[derive(Debug, Clone)]
pub struct UiPerfSnapshot {
  pub entries: Vec<UiPerfEntry>,
  pub recent_slow: Vec<UiJankEvent>,
  pub dropped_samples: u64,
  pub enabled: bool,
}

fn empty_snapshot() -> UiPerfSnapshot {
  UiPerfSnapshot {
    entries: vec![],
    recent_slow: vec![],
    dropped_samples: SAMPLES_DROPPED.load(Ordering::Relaxed),
    enabled: is_enabled(),
  }
}

fn update_cached_enabled(enabled: bool) {
  if let Ok(mut snapshot) = CACHED_SNAPSHOT.try_lock() {
    snapshot.enabled = enabled;
  }
}

fn us_to_ms(us: u64) -> f64 {
  us as f64 / 1_000.0
}

fn short_name(full: &str) -> String {
  full.rsplit("::").next().unwrap_or(full).to_string()
}

pub fn snapshot() -> UiPerfSnapshot {
  // 同步 drain/sort 只用于测试或非 UI 调试路径，UI 面板应使用 consumer::snapshot 读取缓存快照。
  refresh_cached_snapshot();
  cached_snapshot()
}

fn refresh_cached_snapshot() {
  let pending_samples = drain_pending_samples();
  let Ok(mut store) = STORE.lock() else {
    return;
  };
  for sample in pending_samples {
    apply_sample(&mut store, sample);
  }

  let mut entries: Vec<UiPerfEntry> = store
    .entries
    .iter()
    .map(|(key, e)| UiPerfEntry {
      kind: key.kind.label().to_string(),
      name: short_name(key.name),
      browser_id: key.browser_id,
      count: e.count,
      last_ms: us_to_ms(e.last_us),
      avg_ms: e.total_us.checked_div(e.count).map(us_to_ms).unwrap_or_default(),
      max_ms: us_to_ms(e.max_us),
      slow_count: e.slow_count,
    })
    .collect();
  entries.sort_by(|a, b| b.max_ms.total_cmp(&a.max_ms));

  let recent_slow = store
    .slow_events
    .iter()
    .rev()
    .take(10)
    .map(|ev| SlowEventEntry {
      kind: ev.kind.label().to_string(),
      name: short_name(ev.name),
      program: short_name(ev.name),
      browser_id: ev.browser_id,
      elapsed_ms: us_to_ms(ev.elapsed_us),
      threshold_ms: ev.threshold_ms,
      secs_ago: ev.at.elapsed().as_secs(),
    })
    .collect();

  let snapshot = UiPerfSnapshot {
    entries,
    recent_slow,
    dropped_samples: SAMPLES_DROPPED.load(Ordering::Relaxed),
    enabled: is_enabled(),
  };

  drop(store);
  if let Ok(mut cached) = CACHED_SNAPSHOT.lock() {
    *cached = snapshot;
  }
}

fn cached_snapshot() -> UiPerfSnapshot {
  CACHED_SNAPSHOT
    .lock()
    .map(|snapshot| snapshot.clone())
    .unwrap_or_else(|_| UiPerfSnapshot {
      entries: vec![],
      recent_slow: vec![],
      dropped_samples: SAMPLES_DROPPED.load(Ordering::Relaxed),
      enabled: is_enabled(),
    })
}

pub fn reset() {
  if let Ok(mut samples) = SAMPLE_BUFFER.lock() {
    samples.clear();
  }
  SAMPLES_DROPPED.store(0, Ordering::Relaxed);
  clear_all_webview_interactions();
  if let Ok(mut store) = STORE.lock() {
    store.entries.clear();
    store.slow_events.clear();
  }
  if let Ok(mut cached) = CACHED_SNAPSHOT.lock() {
    *cached = empty_snapshot();
  }
}

pub mod consumer {
  use super::UiPerfSnapshot;

  /// 返回后台消费线程维护的缓存快照，避免在 iced update 路径同步聚合。
  pub fn snapshot() -> UiPerfSnapshot {
    #[cfg(not(test))]
    if super::is_enabled() {
      super::ensure_consumer_worker();
    }
    super::cached_snapshot()
  }

  pub fn reset() {
    super::reset();
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

  fn reset_enabled() {
    set_enabled(true);
    set_control_allowed(false);
    reset();
  }

  #[test]
  fn test_open_devtool_env_uses_enabled_flag_parser() {
    let _guard = TEST_LOCK.lock().unwrap();
    let old_value = std::env::var_os(UI_PERF_OPEN_DEVTOOL_ENV_VAR);

    unsafe {
      std::env::set_var(UI_PERF_OPEN_DEVTOOL_ENV_VAR, "on");
    }
    assert!(is_open_devtool_by_env());

    unsafe {
      std::env::set_var(UI_PERF_OPEN_DEVTOOL_ENV_VAR, "false");
    }
    assert!(!is_open_devtool_by_env());

    unsafe {
      match old_value {
        Some(value) => std::env::set_var(UI_PERF_OPEN_DEVTOOL_ENV_VAR, value),
        None => std::env::remove_var(UI_PERF_OPEN_DEVTOOL_ENV_VAR),
      }
    }
  }

  #[test]
  fn test_snapshot_keeps_sub_millisecond_precision() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_enabled();
    record_update("demo::app", Duration::from_micros(500));
    record_update("demo::app", Duration::from_micros(750));

    let snapshot = snapshot();
    let entry = snapshot.entries.iter().find(|entry| entry.name == "app").unwrap();

    assert_eq!(entry.count, 2);
    assert!((entry.last_ms - 0.75).abs() < 0.001);
    assert!((entry.avg_ms - 0.625).abs() < 0.001);
    assert!((entry.max_ms - 0.75).abs() < 0.001);
  }

  #[test]
  fn test_snapshot_tracks_slow_event_threshold_in_milliseconds() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_enabled();
    record_update("demo::slow_app", Duration::from_millis(SLOW_UPDATE_THRESHOLD_MS + 4));

    let snapshot = snapshot();
    let entry = snapshot.entries.iter().find(|entry| entry.name == "slow_app").unwrap();

    assert_eq!(entry.slow_count, 1);
    assert_eq!(snapshot.recent_slow.len(), 1);
    assert!(snapshot.recent_slow[0].elapsed_ms >= SLOW_UPDATE_THRESHOLD_MS as f64);
  }

  #[test]
  fn test_snapshot_splits_update_and_view_metrics_for_same_program() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_enabled();
    record_update("demo::app", Duration::from_millis(5));
    record_view("demo::app", Duration::from_millis(7));

    let snapshot = snapshot();
    let update = snapshot
      .entries
      .iter()
      .find(|entry| entry.kind == "update" && entry.name == "app")
      .unwrap();
    let view = snapshot
      .entries
      .iter()
      .find(|entry| entry.kind == "view" && entry.name == "app")
      .unwrap();

    assert_eq!(update.count, 1);
    assert_eq!(view.count, 1);
    assert_eq!(update.max_ms, 5.0);
    assert_eq!(view.max_ms, 7.0);
  }

  #[test]
  fn test_slow_event_records_metric_kind_and_threshold() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_enabled();
    record_event_loop_gap(Duration::from_millis(SLOW_EVENT_LOOP_GAP_THRESHOLD_MS + 1));

    let snapshot = snapshot();
    assert_eq!(snapshot.recent_slow.len(), 1);
    assert_eq!(snapshot.recent_slow[0].kind, "event_loop_gap");
    assert_eq!(snapshot.recent_slow[0].name, "winit");
    assert_eq!(snapshot.recent_slow[0].threshold_ms, SLOW_EVENT_LOOP_GAP_THRESHOLD_MS);
  }

  #[test]
  fn test_reset_clears_all_metric_kinds() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_enabled();
    record_update("demo::app", Duration::from_millis(1));
    record_view("demo::app", Duration::from_millis(1));
    record_cef_frame_consume(Duration::from_millis(SLOW_WEBVIEW_FRAME_THRESHOLD_MS + 1));

    reset();
    let snapshot = snapshot();

    assert!(snapshot.entries.is_empty());
    assert!(snapshot.recent_slow.is_empty());
    assert!(snapshot.enabled);
  }

  #[test]
  fn test_snapshot_tracks_all_perf_doctor_metric_kinds() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_enabled();
    record_event_loop_task(
      "lintel_task",
      Duration::from_millis(SLOW_EVENT_LOOP_TASK_THRESHOLD_MS + 1),
    );
    record_cef_pump(Duration::from_millis(SLOW_CEF_PUMP_THRESHOLD_MS + 1));
    record_cef_frame_arrival(Duration::from_millis(SLOW_WEBVIEW_FRAME_THRESHOLD_MS + 1));
    record_cef_frame_consume(Duration::from_millis(SLOW_WEBVIEW_FRAME_THRESHOLD_MS + 1));
    record_webview_interaction_frame_to_present_for_browser(
      42,
      Duration::from_millis(SLOW_WEBVIEW_FRAME_THRESHOLD_MS + 1),
    );
    record_webview_paint(
      "software_paint",
      Duration::from_millis(SLOW_WEBVIEW_PAINT_THRESHOLD_MS + 1),
    );
    record_webview_gpu_poll(Duration::from_millis(SLOW_WEBVIEW_PAINT_THRESHOLD_MS + 1));

    let snapshot = snapshot();
    let kinds = snapshot
      .entries
      .iter()
      .map(|entry| entry.kind.as_str())
      .collect::<Vec<_>>();

    assert!(kinds.contains(&"event_loop_task"));
    assert!(kinds.contains(&"cef_pump"));
    assert!(kinds.contains(&"cef_frame_arrival"));
    assert!(kinds.contains(&"cef_frame_consume"));
    assert!(kinds.contains(&"webview_interaction_frame_to_present"));
    assert!(kinds.contains(&"webview_paint"));
    assert!(kinds.contains(&"webview_gpu_poll"));
    assert_eq!(snapshot.recent_slow.len(), 7);
  }

  #[test]
  fn test_webview_metrics_keep_browser_id_dimension() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_enabled();
    record_webview_paint_for_browser(
      42,
      "software_paint",
      Duration::from_millis(SLOW_WEBVIEW_PAINT_THRESHOLD_MS + 1),
    );

    let snapshot = snapshot();
    let entry = snapshot
      .entries
      .iter()
      .find(|entry| entry.kind == "webview_paint" && entry.name == "software_paint")
      .unwrap();

    assert_eq!(entry.browser_id, Some(42));
    assert_eq!(snapshot.recent_slow.len(), 1);
    assert_eq!(snapshot.recent_slow[0].browser_id, Some(42));
  }

  #[test]
  fn test_webview_interaction_frame_to_present_keeps_browser_dimension() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_enabled();
    record_webview_interaction_frame_to_present_for_browser(42, Duration::from_millis(20));

    let snapshot = snapshot();
    let entry = snapshot
      .entries
      .iter()
      .find(|entry| entry.kind == "webview_interaction_frame_to_present")
      .unwrap();

    assert_eq!(entry.name, "interaction");
    assert_eq!(entry.browser_id, Some(42));
    assert_eq!(entry.count, 1);
    assert_eq!(entry.slow_count, 0);
  }

  #[test]
  fn test_webview_interaction_sampling_window_is_cleared_when_browser_leaves_foreground() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_enabled();

    mark_webview_interaction(42);
    assert!(is_webview_interaction_active(42));

    clear_webview_interaction(42);
    assert!(!is_webview_interaction_active(42));
  }

  #[test]
  fn test_business_stage_records_kind_and_slow_threshold() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_enabled();
    record_business_stage(
      "im_set_msgs_by_account",
      Duration::from_millis(SLOW_BUSINESS_STAGE_THRESHOLD_MS + 1),
    );

    let snapshot = snapshot();
    let entry = snapshot
      .entries
      .iter()
      .find(|entry| entry.kind == "business_stage" && entry.name == "im_set_msgs_by_account")
      .unwrap();

    assert_eq!(entry.count, 1);
    assert_eq!(entry.slow_count, 1);
    assert_eq!(snapshot.recent_slow.len(), 1);
    assert_eq!(snapshot.recent_slow[0].kind, "business_stage");
    assert_eq!(snapshot.recent_slow[0].threshold_ms, SLOW_BUSINESS_STAGE_THRESHOLD_MS);
  }

  #[test]
  fn test_metrics_are_noop_until_enabled() {
    let _guard = TEST_LOCK.lock().unwrap();
    set_enabled(false);
    reset();
    record_update("demo::app", Duration::from_millis(SLOW_UPDATE_THRESHOLD_MS + 1));

    assert!(snapshot().entries.is_empty());

    set_enabled(true);
    record_update("demo::app", Duration::from_millis(SLOW_UPDATE_THRESHOLD_MS + 1));

    let snapshot = snapshot();
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(snapshot.recent_slow.len(), 1);
  }

  #[test]
  fn test_measure_skips_recorder_until_enabled() {
    let _guard = TEST_LOCK.lock().unwrap();
    set_enabled(false);
    reset();

    let value = measure(|| 42, |elapsed| record_update("demo::app", elapsed));

    assert_eq!(value, 42);
    assert!(snapshot().entries.is_empty());

    set_enabled(true);
    measure(
      || (),
      |elapsed| {
        record_update(
          "demo::app",
          elapsed.max(Duration::from_millis(SLOW_UPDATE_THRESHOLD_MS + 1)),
        )
      },
    );

    assert_eq!(snapshot().entries.len(), 1);
  }

  #[test]
  fn test_recording_buffers_until_snapshot_consumes_samples() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_enabled();

    record_update("demo::app", Duration::from_millis(5));

    {
      let store = STORE.lock().unwrap();
      assert!(store.entries.is_empty());
      assert!(store.slow_events.is_empty());
    }

    let first_snapshot = snapshot();
    assert_eq!(first_snapshot.entries.len(), 1);
    assert_eq!(first_snapshot.entries[0].count, 1);

    let second_snapshot = snapshot();
    assert_eq!(second_snapshot.entries.len(), 1);
    assert_eq!(second_snapshot.entries[0].count, 1);
  }

  #[test]
  fn test_pending_sample_buffer_reports_overflow_drops() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_enabled();

    for _ in 0..MAX_PENDING_SAMPLES + 2 {
      record_update("demo::app", Duration::from_millis(5));
    }

    let snapshot = snapshot();
    let entry = snapshot.entries.iter().find(|entry| entry.name == "app").unwrap();

    assert_eq!(entry.count, MAX_PENDING_SAMPLES as u64);
    assert_eq!(snapshot.dropped_samples, 2);
  }

  #[test]
  fn test_recording_drops_sample_instead_of_waiting_when_buffer_is_locked() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_enabled();
    let samples = SAMPLE_BUFFER.lock().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();

    let handle = std::thread::spawn(move || {
      record_update("demo::app", Duration::from_millis(5));
      tx.send(()).unwrap();
    });
    let completed = rx.recv_timeout(Duration::from_millis(50)).is_ok();
    drop(samples);
    let _ = handle.join();

    assert!(completed, "record_update blocked on the pending sample buffer");
    let snapshot = snapshot();
    assert!(snapshot.entries.is_empty());
    assert_eq!(snapshot.dropped_samples, 1);
  }

  #[test]
  fn test_external_control_requires_native_allowance() {
    let _guard = TEST_LOCK.lock().unwrap();
    set_enabled(false);
    set_control_allowed(false);
    reset();

    assert!(!try_set_enabled_from_external(true));
    assert!(!is_enabled());

    set_control_allowed(true);
    assert!(try_set_enabled_from_external(true));
    assert!(is_enabled());

    assert!(try_set_enabled_from_external(false));
    assert!(!is_enabled());
    set_control_allowed(false);
  }

  #[test]
  fn test_slow_event_store_keeps_bounded_recent_events() {
    let _guard = TEST_LOCK.lock().unwrap();
    reset_enabled();
    for _ in 0..MAX_SLOW_EVENTS + 5 {
      record_update("demo::slow_app", Duration::from_millis(SLOW_UPDATE_THRESHOLD_MS + 1));
    }

    let snapshot = snapshot();
    let entry = snapshot.entries.iter().find(|entry| entry.name == "slow_app").unwrap();

    assert_eq!(entry.slow_count, (MAX_SLOW_EVENTS + 5) as u64);
    assert_eq!(snapshot.recent_slow.len(), 10);
  }
}
