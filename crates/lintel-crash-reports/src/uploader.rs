use std::{
  error::Error as _,
  fs,
  io::{BufReader, Read, Write},
  path::Path,
  process::{Child, Command, Stdio},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
  },
  time::{Duration, Instant, SystemTime},
};

use reqwest::{
  Client, StatusCode,
  header::RETRY_AFTER,
  multipart::{Form, Part},
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::{
  FailureReason, ReportError, ReportStore, ReportsConfig, UploadMode, UploadReceipt,
  importer::valid_text,
  store::{UploadAttempt, UploadOutcome, hash},
};

const SWITCH: &str = "--lintel-crash-uploader-v1";
const CONFIG_LIMIT: usize = 64 * 1024;
const RECEIPT_LIMIT: usize = 8 * 1024;
const MAX_GRACE: Duration = Duration::from_secs(120);
const POLICY_POLL: Duration = Duration::from_millis(100);
const SCAN_INTERVAL: Duration = Duration::from_millis(500);
const WAKE: u8 = 1;
const FINISH: u8 = 2;

#[derive(Serialize, Deserialize)]
struct Launch {
  config: ReportsConfig,
  grace: Duration,
}

/// Inherited control pipe and process handle for a single uploader.
/// Dropping the pipe starts the uploader's configured orphan grace period.
pub struct UploaderProcess {
  child: Option<Child>,
  control: Option<mpsc::SyncSender<()>>,
  finish: Arc<AtomicBool>,
}

impl UploaderProcess {
  /// Ask the uploader to scan immediately. Repeated notifications coalesce.
  pub fn wake(&mut self) {
    if let Some(control) = &self.control {
      let _ = control.try_send(());
    }
  }

  /// Ask the uploader to finish its current attempt and exit without waiting for it.
  pub fn finish(&mut self) {
    self.finish.store(true, Ordering::Release);
    self.control.take();
  }

  pub fn is_running(&mut self) -> bool {
    self
      .child
      .as_mut()
      .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
  }
}

impl Drop for UploaderProcess {
  fn drop(&mut self) {
    self.control.take();
    if let Some(mut child) = self.child.take() {
      let _ = std::thread::Builder::new()
        .name("lintel-uploader-reap".to_owned())
        .spawn(move || {
          let _ = child.wait();
        });
    }
  }
}

impl ReportStore {
  /// Start a separately packaged uploader and pass the configuration through its stdin.
  pub fn spawn_uploader(&self, path: &Path, grace: Duration) -> Result<UploaderProcess, ReportError> {
    if self.config.upload.is_none() {
      return Err(ReportError::MissingTarget);
    }
    if grace > MAX_GRACE || !path.is_absolute() {
      return Err(ReportError::InvalidConfiguration);
    }
    self.settings()?;
    let path = fs::canonicalize(path).map_err(|_| ReportError::UploaderUnavailable)?;
    if !path.is_file() {
      return Err(ReportError::UploaderUnavailable);
    }
    let bytes = serde_json::to_vec(&Launch {
      config: self.config.clone(),
      grace,
    })?;
    if bytes.len() > CONFIG_LIMIT {
      return Err(ReportError::InvalidConfiguration);
    }
    let mut child = Command::new(path)
      .arg(SWITCH)
      .stdin(Stdio::piped())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn()
      .map_err(|_| ReportError::UploaderUnavailable)?;
    let Some(mut stdin) = child.stdin.take() else {
      let _ = child.kill();
      let _ = child.wait();
      return Err(ReportError::UploaderUnavailable);
    };
    let (control, receive) = mpsc::sync_channel(1);
    let finish = Arc::new(AtomicBool::new(false));
    let writer_finish = Arc::clone(&finish);
    let started = std::thread::Builder::new()
      .name("lintel-uploader-control".to_owned())
      .spawn(move || {
        let frame = (bytes.len() as u32).to_le_bytes();
        if stdin.write_all(&frame).and_then(|()| stdin.write_all(&bytes)).is_err() {
          return;
        }
        drop(bytes);
        while receive.recv().is_ok() {
          if writer_finish.load(Ordering::Acquire) || stdin.write_all(&[WAKE]).is_err() {
            break;
          }
        }
        if writer_finish.load(Ordering::Acquire) {
          let _ = stdin.write_all(&[FINISH]);
        }
      });
    if started.is_err() {
      let _ = child.kill();
      let _ = child.wait();
      return Err(ReportError::UploaderUnavailable);
    }
    Ok(UploaderProcess {
      child: Some(child),
      control: Some(control),
      finish,
    })
  }
}

#[derive(Clone, Copy)]
enum Control {
  Attached,
  Orphaned(Instant),
  Finish,
}

impl Control {
  fn stop(self) -> bool {
    matches!(self, Self::Finish) || self.expired()
  }

  fn expired(self) -> bool {
    matches!(self, Self::Orphaned(deadline) if deadline <= Instant::now())
  }
}

/// Entry point for the separately launched `lintel-crash-uploader` executable.
#[doc(hidden)]
pub fn run_uploader() -> Result<(), ReportError> {
  let mut args = std::env::args_os().skip(1);
  if args.next().as_deref() != Some(SWITCH.as_ref()) || args.next().is_some() {
    return Err(ReportError::InvalidConfiguration);
  }
  let mut stdin = BufReader::new(std::io::stdin());
  let mut length = [0; 4];
  stdin.read_exact(&mut length)?;
  let length = u32::from_le_bytes(length) as usize;
  if length > CONFIG_LIMIT {
    return Err(ReportError::InvalidConfiguration);
  }
  let mut frame = vec![0; length];
  stdin.read_exact(&mut frame)?;
  let Launch { config, grace } = serde_json::from_slice(&frame)?;
  drop(frame);
  if grace > MAX_GRACE {
    return Err(ReportError::InvalidConfiguration);
  }
  let (transmit, mut receive) = watch::channel(Control::Attached);
  std::thread::Builder::new()
    .name("lintel-uploader-input".to_owned())
    .spawn(move || {
      let mut command = [0];
      while stdin.read_exact(&mut command).is_ok() {
        match command[0] {
          WAKE => transmit.send_modify(|control| *control = Control::Attached),
          FINISH => {
            let _ = transmit.send(Control::Finish);
            return;
          }
          _ => break,
        }
      }
      let _ = transmit.send(Control::Orphaned(Instant::now() + grace));
    })?;
  let store = match ReportStore::open_child(config) {
    Ok(store) => store,
    Err(ReportError::StaleConfiguration) => return Ok(()),
    Err(error) => return Err(error),
  };
  let client = client_builder().build().map_err(|_| ReportError::UploaderUnavailable)?;
  let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
  let result = runtime.block_on(run(&store, &client, &mut receive));
  // The import may be in a short blocking filesystem call. The next sender recovers uncommitted staging.
  runtime.shutdown_background();
  match result {
    Err(ReportError::StaleConfiguration) => Ok(()),
    other => other,
  }
}

fn client_builder() -> reqwest::ClientBuilder {
  Client::builder()
    .https_only(true)
    .redirect(reqwest::redirect::Policy::none())
    .retry(reqwest::retry::never())
    .no_proxy()
    .tls_built_in_root_certs(true)
}

async fn run(store: &ReportStore, client: &Client, control: &mut watch::Receiver<Control>) -> Result<(), ReportError> {
  let _send_lock = loop {
    if control.borrow().stop() {
      return Ok(());
    }
    store.settings()?;
    if let Some(lock) = store.try_sender_lock()? {
      break lock;
    }
    wait_for_control(control, POLICY_POLL).await;
  };
  store.sender_started()?;
  let mut last_policy = None;
  let mut scan = None::<tokio::task::JoinHandle<()>>;
  let mut next_scan = Instant::now();
  loop {
    let signal = *control.borrow();
    if signal.stop() {
      break;
    }
    let settings = store.settings()?;
    if last_policy != Some(settings.version) {
      store.acknowledge(settings.version)?;
      last_policy = Some(settings.version);
    }
    if settings.policy.mode == UploadMode::Disabled {
      // The host may replace us on the next authorized request.
      break;
    }
    if scan.as_ref().is_some_and(|task| task.is_finished()) {
      scan = None;
    }
    if scan.is_none() && Instant::now() >= next_scan {
      let scan_control = control.clone();
      let scanner = store.clone();
      scan = Some(tokio::task::spawn_blocking(move || {
        let _ = scanner.collect_until(|| !scan_control.borrow().stop());
      }));
      next_scan = Instant::now() + SCAN_INTERVAL;
    }
    if let Some(attempt) = store.claim(settings.version)? {
      let outcome = perform(store, client, control, settings.version, &attempt).await?;
      store.complete(&attempt, outcome)?;
      continue;
    }
    if wait_for_control(control, POLICY_POLL).await {
      next_scan = Instant::now();
    }
  }
  Ok(())
}

async fn wait_for_control(control: &mut watch::Receiver<Control>, wait: Duration) -> bool {
  tokio::select! {
      changed = control.changed(), if control.has_changed().is_ok() => changed.is_ok(),
      () = tokio::time::sleep(wait) => false,
  }
}

async fn perform(
  store: &ReportStore,
  client: &Client,
  control: &mut watch::Receiver<Control>,
  version: u64,
  attempt: &UploadAttempt,
) -> Result<UploadOutcome, ReportError> {
  let mut signal = *control.borrow_and_update();
  if signal.stop() || !policy_matches(store, version)? {
    return Ok(UploadOutcome::Interrupted);
  }
  let request = send(store, client, attempt);
  tokio::pin!(request);
  let request_deadline = Instant::now() + store.config.limits.request_timeout;
  let mut next_policy_check = Instant::now() + POLICY_POLL;
  let mut control_open = true;
  loop {
    let now = Instant::now();
    if signal.expired() {
      return Ok(UploadOutcome::Interrupted);
    }
    if now >= request_deadline {
      return Ok(UploadOutcome::Retry(retry_delay(store, attempt.attempt, None)));
    }
    if now >= next_policy_check {
      if !policy_matches(store, version)? {
        return Ok(UploadOutcome::Interrupted);
      }
      next_policy_check = now + POLICY_POLL;
    }
    let mut until = request_deadline.min(next_policy_check);
    if let Control::Orphaned(deadline) = signal {
      until = until.min(deadline);
    }
    tokio::select! {
        biased;
        changed = control.changed(), if control_open => {
            signal = *control.borrow_and_update();
            control_open = changed.is_ok();
        }
        () = tokio::time::sleep_until(until.into()) => {}
        outcome = &mut request => return Ok(outcome),
    }
  }
}

fn policy_matches(store: &ReportStore, version: u64) -> Result<bool, ReportError> {
  match store.settings() {
    Ok(settings) => Ok(settings.version == version && settings.policy.mode != UploadMode::Disabled),
    Err(ReportError::StaleConfiguration) => Ok(false),
    Err(error) => Err(error),
  }
}

async fn send(store: &ReportStore, client: &Client, attempt: &UploadAttempt) -> UploadOutcome {
  let Some(target) = &store.config.upload else {
    return UploadOutcome::Interrupted;
  };
  let file = match store.open_upload_dump(attempt.id) {
    Ok(file) if file.metadata().is_ok_and(|metadata| metadata.len() == attempt.size) => file,
    _ => return UploadOutcome::Failed(FailureReason::InvalidDump),
  };
  let body = reqwest::Body::wrap_stream(ReaderStream::new(tokio::fs::File::from_std(file)));
  let dump = Part::stream_with_length(body, attempt.size)
    .file_name("report.dmp")
    .mime_str("application/octet-stream");
  let metadata = Part::text(attempt.metadata.clone()).mime_str("application/json");
  let (Ok(dump), Ok(metadata)) = (dump, metadata) else {
    return UploadOutcome::Failed(FailureReason::ReceiptProtocol);
  };
  let key = hash(format!("{}\0{}", store.config.application_id, attempt.id));
  let form = Form::new().part("metadata", metadata).part("dump", dump);
  let mut request = client.post(&target.url).header("Idempotency-Key", key).multipart(form);
  if let Some(token) = &target.bearer_token {
    request = request.bearer_auth(token);
  }
  let mut response = match request.send().await {
    Ok(response) => response,
    Err(error) => return from_request_error(store, attempt, error),
  };
  let status = response.status();
  if !status.is_success() {
    let retry_after = response
      .headers()
      .get(RETRY_AFTER)
      .and_then(|value| value.to_str().ok())
      .and_then(parse_retry_after);
    return match status {
      StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => UploadOutcome::Failed(FailureReason::Authentication),
      StatusCode::CONFLICT => UploadOutcome::Failed(FailureReason::ContentConflict),
      status if status.is_redirection() => UploadOutcome::Failed(FailureReason::Redirect),
      status
        if status == StatusCode::TOO_MANY_REQUESTS
          || (status.is_server_error()
            && status != StatusCode::NOT_IMPLEMENTED
            && status != StatusCode::HTTP_VERSION_NOT_SUPPORTED) =>
      {
        UploadOutcome::Retry(retry_delay(store, attempt.attempt, retry_after))
      }
      _ => UploadOutcome::Failed(FailureReason::HttpRejected),
    };
  }
  if response
    .content_length()
    .is_some_and(|length| length > RECEIPT_LIMIT as u64)
  {
    return UploadOutcome::Failed(FailureReason::ReceiptProtocol);
  }
  let mut bytes = Vec::new();
  loop {
    match response.chunk().await {
      Ok(Some(chunk)) if bytes.len().saturating_add(chunk.len()) <= RECEIPT_LIMIT => bytes.extend_from_slice(&chunk),
      Ok(Some(_)) => return UploadOutcome::Failed(FailureReason::ReceiptProtocol),
      Ok(None) => break,
      Err(error) => return from_request_error(store, attempt, error),
    }
  }
  match serde_json::from_slice::<UploadReceipt>(&bytes) {
    Ok(receipt)
      if receipt.schema_version == 1 && receipt.report_id == attempt.id && valid_text(&receipt.receipt_id, 512) =>
    {
      UploadOutcome::Delivered(receipt)
    }
    _ => UploadOutcome::Failed(FailureReason::ReceiptProtocol),
  }
}

fn from_request_error(store: &ReportStore, attempt: &UploadAttempt, error: reqwest::Error) -> UploadOutcome {
  let mut cause = error.source();
  while let Some(error) = cause {
    if error.is::<rustls::Error>()
      || error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.get_ref().is_some_and(|inner| inner.is::<rustls::Error>()))
    {
      return UploadOutcome::Failed(FailureReason::SecureConnection);
    }
    cause = error.source();
  }
  UploadOutcome::Retry(retry_delay(store, attempt.attempt, None))
}

fn parse_retry_after(value: &str) -> Option<Duration> {
  value.parse::<u64>().ok().map(Duration::from_secs).or_else(|| {
    let time = httpdate::parse_http_date(value).ok()?;
    Some(time.duration_since(SystemTime::now()).unwrap_or(Duration::ZERO))
  })
}

fn retry_delay(store: &ReportStore, attempts: u32, retry_after: Option<Duration>) -> Duration {
  let seconds = 1_u64 << attempts.saturating_sub(1).min(20);
  let jitter_ms = u64::from(Uuid::new_v4().as_bytes()[0]) * seconds * 2;
  Duration::from_secs(seconds)
    .saturating_add(Duration::from_millis(jitter_ms))
    .max(retry_after.unwrap_or(Duration::ZERO))
    .min(store.config.limits.max_retry_delay)
}

#[cfg(test)]
mod tests {
  use std::io::{BufRead as _, BufReader};
  use std::net::{TcpListener, TcpStream};

  use super::*;
  use crate::{
    ReportQuery, ReportStatus, UploadTarget,
    test_data::{config, dump, fatal},
  };

  #[tokio::test]
  async fn receipt_loss_reuses_idempotency_key_and_immutable_report_parts() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let rcgen::CertifiedKey { cert, signing_key } =
      rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let client = client_builder()
      .add_root_certificate(reqwest::Certificate::from_der(cert.der()).unwrap())
      .build()
      .unwrap();
    let server = rustls::ServerConfig::builder()
      .with_no_client_auth()
      .with_single_cert(
        vec![cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
      )
      .unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let mut config = config(temporary.path());
    config.upload = Some(
      UploadTarget::new(format!("https://localhost:{}/crashes", address.port()))
        .with_bearer_token("test-credential", 1),
    );
    let store = ReportStore::open(config.clone()).unwrap();
    fatal(&config, "report-upload", None, true);
    store.collect().unwrap();
    let id = store.list_reports(ReportQuery::default()).unwrap()[0].id;
    store.request_upload(id).unwrap();
    let version = store.settings().unwrap().version;
    let observed = std::thread::spawn(move || {
      let mut requests = Vec::new();
      let server = Arc::new(server);
      for attempt in 0..2 {
        let (tcp, _) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        tcp.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
        let connection = rustls::ServerConnection::new(Arc::clone(&server)).unwrap();
        let mut stream = rustls::StreamOwned::new(connection, tcp);
        requests.push(receive_request(&mut stream));
        if attempt == 1 {
          let receipt = format!(r#"{{"schema_version":1,"report_id":"{id}","receipt_id":"durable"}}"#);
          let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{receipt}",
            receipt.len()
          );
          stream.write_all(response.as_bytes()).unwrap();
          stream.flush().unwrap();
        }
      }
      requests
    });
    let first = store.claim(version).unwrap().unwrap();
    assert!(matches!(
      tokio::time::timeout(Duration::from_secs(5), send(&store, &client, &first))
        .await
        .unwrap(),
      UploadOutcome::Retry(_)
    ));
    // Normally the retry waits for its backoff. Use a zero test delay.
    store.complete(&first, UploadOutcome::Retry(Duration::ZERO)).unwrap();
    let second = store.claim(version).unwrap().unwrap();
    assert_eq!(second.attempt, 2);
    let outcome = tokio::time::timeout(Duration::from_secs(5), send(&store, &client, &second))
      .await
      .unwrap();
    let UploadOutcome::Delivered(receipt) = outcome else {
      panic!("the retry did not return its durable receipt")
    };
    store
      .complete(&second, UploadOutcome::Delivered(receipt.clone()))
      .unwrap();
    assert_eq!(store.status(id).unwrap().status, ReportStatus::Uploaded(receipt));
    assert_eq!(first.id, second.id);
    assert_eq!(first.metadata, second.metadata);
    let requests = observed.join().unwrap();
    assert_eq!(requests[0].0, requests[1].0);
    assert!(!requests[0].0.is_empty());
    let dump = dump(&[(6, 168)]);
    for (_, body) in requests {
      assert!(
        body
          .windows(first.metadata.len())
          .any(|bytes| bytes == first.metadata.as_bytes())
      );
      assert!(body.windows(dump.len()).any(|bytes| bytes == dump));
      assert!(!body.windows(b"process_id".len()).any(|bytes| bytes == b"process_id"));
    }
  }

  #[tokio::test]
  async fn eof_during_an_attempt_caps_it_at_the_remaining_grace() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let temporary = tempfile::tempdir().unwrap();
    let mut config = config(temporary.path());
    config.upload = Some(UploadTarget::new(format!(
      "https://localhost:{}/crashes",
      address.port()
    )));
    let store = ReportStore::open(config.clone()).unwrap();
    fatal(&config, "report-active", None, true);
    store.collect().unwrap();
    let id = store.list_reports(ReportQuery::default()).unwrap()[0].id;
    store.request_upload(id).unwrap();
    let version = store.settings().unwrap().version;
    let attempt = store.claim(version).unwrap().unwrap();
    let client = client_builder().build().unwrap();
    let (signal, mut control) = watch::channel(Control::Attached);
    let (release, finished) = mpsc::channel();
    let remote = std::thread::spawn(move || {
      let (_connection, _) = listener.accept().unwrap();
      signal
        .send(Control::Orphaned(Instant::now() + Duration::from_millis(60)))
        .unwrap();
      drop(signal);
      let _ = finished.recv_timeout(Duration::from_secs(5));
    });
    let result = tokio::time::timeout(
      Duration::from_secs(2),
      perform(&store, &client, &mut control, version, &attempt),
    )
    .await;
    let _ = release.send(());
    remote.join().unwrap();
    let outcome = result.unwrap().unwrap();
    assert!(matches!(outcome, UploadOutcome::Interrupted));
    store.complete(&attempt, outcome).unwrap();
    assert_eq!(store.status(id).unwrap().status, ReportStatus::Queued);
  }

  fn receive_request(stream: &mut rustls::StreamOwned<rustls::ServerConnection, TcpStream>) -> (String, Vec<u8>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "POST /crashes HTTP/1.1");
    let mut length = 0;
    let mut key = String::new();
    let mut authorized = false;
    loop {
      line.clear();
      assert!(reader.read_line(&mut line).unwrap() > 0);
      let header = line.trim();
      if header.is_empty() {
        break;
      }
      let (name, value) = header.split_once(':').unwrap();
      match name.to_ascii_lowercase().as_str() {
        "content-length" => length = value.trim().parse().unwrap(),
        "idempotency-key" => key = value.trim().to_owned(),
        "authorization" => authorized = value.trim() == "Bearer test-credential",
        _ => {}
      }
    }
    assert!(authorized);
    assert!((200..=4096).contains(&length));
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    (key, body)
  }
}
