//! Opt-in readiness handshake bound to one retained child; successful exit is never readiness.
use super::*;
use std::{io::Read, time::Instant};

const MARKER: &str = "LINTEL_UPDATE_HEALTH";
const PREFIX: &str = "LINTEL_HEALTH_V1 ";
const FRAME_LIMIT: usize = 4096;
const OUTPUT_LIMIT: usize = 65536;

/// Optional business-readiness deadline. Existing updates do not enable this contract.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowsStartupHealth {
  timeout_ms: u64,
  minimum_uptime_ms: u64,
}

impl WindowsStartupHealth {
  pub fn new(timeout: Duration, minimum_uptime: Duration) -> Result<Self> {
    let value = Self {
      timeout_ms: timeout.as_millis().try_into()?,
      minimum_uptime_ms: minimum_uptime.as_millis().try_into()?,
    };
    value.validate()?;
    Ok(value)
  }
  pub(super) fn validate(&self) -> Result<()> {
    ensure!(
      (100..=300_000).contains(&self.timeout_ms) && self.minimum_uptime_ms < self.timeout_ms,
      "health deadline must be 100 ms..5 minutes and exceed minimum uptime"
    );
    Ok(())
  }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProcessIdentity {
  pub(super) pid: u32,
  pub(super) created: u64,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Attempt {
  nonce: String,
  options: WindowsStartupHealth,
  child: Option<ProcessIdentity>,
  accepted: bool,
}

impl Attempt {
  pub(super) fn accepted(&self) -> bool {
    self.accepted
  }
  #[cfg(test)]
  pub(super) fn accept_for_test(&mut self) {
    self.accepted = true;
  }
  pub(super) fn new(options: WindowsStartupHealth) -> Result<Self> {
    options.validate()?;
    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce).map_err(|error| anyhow::anyhow!("create health challenge: {error}"))?;
    Ok(Self {
      nonce: nonce.iter().map(|byte| format!("{byte:02x}")).collect(),
      options,
      child: None,
      accepted: false,
    })
  }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
  nonce: String,
  process: ProcessIdentity,
  manifest: String,
}

fn frame_bytes(frame: &Frame) -> Result<Vec<u8>> {
  let bytes = format!("{PREFIX}{}\n", serde_json::to_string(frame)?).into_bytes();
  ensure!(bytes.len() <= FRAME_LIMIT, "health frame is oversized");
  Ok(bytes)
}

fn parse_frame(line: &[u8]) -> Result<Frame> {
  ensure!(line.len() <= FRAME_LIMIT, "health frame is oversized");
  let line = std::str::from_utf8(line)?.trim_end_matches(['\r', '\n']);
  let frame: Frame = serde_json::from_str(line.strip_prefix(PREFIX).context("missing readiness frame prefix")?)?;
  ensure!(
    frame.nonce.len() == 64
      && frame.nonce.bytes().all(|value| value.is_ascii_hexdigit())
      && frame.manifest.len() == 64
      && frame.manifest.bytes().all(|value| value.is_ascii_hexdigit())
      && frame.process.pid != 0
      && frame.process.created != 0,
    "invalid health frame identity"
  );
  Ok(frame)
}

#[derive(Default)]
struct Scanner {
  pending: Vec<u8>,
  scanned: usize,
}
impl Scanner {
  fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>> {
    self.scanned = self
      .scanned
      .checked_add(bytes.len())
      .context("health log counter overflow")?;
    ensure!(
      self.scanned <= OUTPUT_LIMIT,
      "startup output exceeds 64 KiB readiness scan bound"
    );
    let mut frames = Vec::new();
    for byte in bytes {
      self.pending.push(*byte);
      ensure!(
        self.pending.len() <= FRAME_LIMIT,
        "startup output line exceeds 4 KiB readiness bound"
      );
      if *byte == b'\n' {
        if self.pending.starts_with(PREFIX.as_bytes()) {
          frames.push(parse_frame(&self.pending)?);
        }
        self.pending.clear();
      }
    }
    Ok(frames)
  }
}

fn should_confirm(marker: Option<&str>, arguments: &[OsString]) -> bool {
  marker == Some("1")
    && !arguments
      .iter()
      .any(|argument| argument == "--type" || argument.to_string_lossy().starts_with("--type="))
}

fn guarded_confirmation(
  marker: Option<&str>,
  arguments: &[OsString],
  confirm: impl FnOnce() -> Result<bool>,
) -> Result<bool> {
  if !should_confirm(marker, arguments) {
    return Ok(false);
  }
  confirm()
}

trait ChallengeInput {
  fn available(&mut self) -> Result<usize>;
  fn read_raw(&mut self, buffer: &mut [u8]) -> Result<usize>;
}

fn read_challenge(input: &mut impl ChallengeInput, deadline: Instant) -> Result<Frame> {
  let mut line = Vec::new();
  loop {
    ensure!(
      Instant::now() < deadline,
      "timed out reading explicit startup challenge"
    );
    if input.available()? == 0 {
      std::thread::sleep(Duration::from_millis(10));
      continue;
    }
    // The available count and this read refer to the same unbuffered transport. A buffered
    // StdinLock would prefetch the whole frame and invalidate the next OS-level availability check.
    let mut byte = [0];
    ensure!(input.read_raw(&mut byte)? == 1, "startup challenge pipe reached EOF");
    line.push(byte[0]);
    ensure!(line.len() <= FRAME_LIMIT, "startup challenge exceeds bound");
    if byte[0] == b'\n' {
      return parse_frame(&line);
    }
  }
}

fn accept_response(response: &Frame, expected: &Frame, ready: &mut bool) -> Result<()> {
  ensure!(
    response == expected && !*ready,
    "startup response is stale/replayed or belongs to another process/version"
  );
  *ready = true;
  Ok(())
}

fn readiness(ready: bool, alive: bool, elapsed: Duration, options: &WindowsStartupHealth) -> Result<bool> {
  ensure!(
    alive,
    "candidate exited before confirmed readiness/minimum uptime (exit success is not readiness)"
  );
  ensure!(
    elapsed < Duration::from_millis(options.timeout_ms),
    "candidate did not confirm business readiness before deadline"
  );
  Ok(ready && elapsed >= Duration::from_millis(options.minimum_uptime_ms))
}

/// Call at the application's business-ready point (e.g. a setup/ready callback), after CEF's
/// subprocess dispatch. With no explicit updater marker, or in a CEF --type process, this
/// immediately returns false without touching stdin. It is not a framework-defined health test.
pub fn confirm_windows_startup() -> Result<bool> {
  let marker = std::env::var(MARKER).ok();
  guarded_confirmation(
    marker.as_deref(),
    &std::env::args_os().skip(1).collect::<Vec<_>>(),
    || {
      #[cfg(windows)]
      {
        use std::os::windows::io::AsRawHandle;
        let (_, _, active, _) = current_installation()?;
        let stdin = std::io::stdin();
        struct RawInput(windows_sys::Win32::Foundation::HANDLE);
        impl ChallengeInput for RawInput {
          fn available(&mut self) -> Result<usize> {
            Ok(native::pipe_available(self.0)? as usize)
          }
          fn read_raw(&mut self, buffer: &mut [u8]) -> Result<usize> {
            native::read_pipe(self.0, buffer)
          }
        }
        let challenge = read_challenge(
          &mut RawInput(stdin.as_raw_handle()),
          Instant::now() + Duration::from_secs(5),
        )?;
        ensure!(
          challenge.process == native::current_process_identity()? && challenge.manifest == active.manifest_sha256,
          "startup challenge is for another process/version"
        );
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(b"\n")?;
        stdout.write_all(&frame_bytes(&challenge)?)?;
        stdout.flush()?;
        Ok(true)
      }
      #[cfg(not(windows))]
      {
        bail!("explicit Windows startup health handshake requires Windows");
      }
    },
  )
}

#[cfg(windows)]
pub(super) fn launch(
  root: &Path,
  metadata: &Installation,
  pending: &mut PendingSwitch,
  directory: &Path,
  info: &WindowsBundleInfo,
) -> Result<std::process::Child> {
  use std::os::windows::io::AsRawHandle;
  let options = pending
    .health
    .as_ref()
    .context("missing health attempt")?
    .options
    .clone();
  options.validate()?;
  let mut child = Command::new(directory.join(&info.executable))
    .current_dir(root)
    .env(MARKER, "1")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .spawn()?;
  // A slow CreateProcess/loader is not application uptime. The readiness deadline and
  // minimum survival interval begin only after a retained child has actually been created.
  let started = Instant::now();
  let result = (|| {
    let identity = native::child_identity(&child)?;
    let attempt = pending.health.as_mut().context("missing health attempt")?;
    attempt.child = Some(identity.clone());
    let frame = Frame {
      nonce: attempt.nonce.clone(),
      process: identity,
      manifest: pending.new.manifest_sha256.clone(),
    };
    write_json(&root.join(JOURNAL), pending)?;
    let mut input = child.stdin.take().context("health child has no input pipe")?;
    input.write_all(&frame_bytes(&frame)?)?;
    drop(input);
    let mut output = child.stdout.take().context("health child has no output pipe")?;
    let mut scanner = Scanner::default();
    let mut ready = false;
    loop {
      readiness(ready, child.try_wait()?.is_none(), started.elapsed(), &options)?;
      let available = native::pipe_available(output.as_raw_handle())? as usize;
      if available > 0 {
        let mut bytes = vec![0; available.min(FRAME_LIMIT)];
        let count = output.read(&mut bytes)?;
        for response in scanner.push(&bytes[..count])? {
          accept_response(&response, &frame, &mut ready)?;
        }
      }
      if readiness(ready, child.try_wait()?.is_none(), started.elapsed(), &options)? {
        break;
      }
      std::thread::sleep(Duration::from_millis(10));
    }
    ensure!(
      native::child_identity(&child)? == frame.process,
      "child identity changed during readiness"
    );
    pending.health.as_mut().context("missing attempt")?.accepted = true;
    write_json(&root.join(JOURNAL), pending)?;
    Ok(output)
  })();
  let mut output = match result {
    Ok(output) => output,
    Err(error) => {
      // This is the exact retained Child created for an explicit kill-on-timeout contract. No PID reopen.
      if child.try_wait()?.is_none() {
        child.kill().context("terminate retained unhealthy candidate")?;
        child.wait()?;
      }
      return Err(error);
    }
  };
  // Keep stdout drained while the healthy child runs; inherited logging must not block on a full pipe.
  // The dedicated helper is stable; this lightweight thread ends when the actual child pipe closes.
  std::thread::spawn(move || {
    let _ = std::io::copy(&mut output, &mut std::io::sink());
  });
  let _ = metadata;
  Ok(child)
}

pub(super) fn recover(root: &Path, metadata: &Installation, pending: &PendingSwitch, quiet: bool) -> Result<()> {
  let attempt = pending.health.as_ref().context("missing health state")?;
  attempt.options.validate()?;
  if attempt.accepted {
    ensure!(
      read_json::<ActiveVersion>(&root.join(ACTIVE))? == pending.new,
      "confirmed version no longer active"
    );
    versions::set_rollback(root, metadata, &pending.old)?;
  } else {
    ensure!(
      quiet,
      "unconfirmed candidate/helpers still running; health recovery deferred (no stored PID termination)"
    );
    ensure!(
      read_json::<ActiveVersion>(&root.join(ACTIVE))? == pending.new
        || read_json::<ActiveVersion>(&root.join(ACTIVE))? == pending.old,
      "another version became active; health recovery will not overwrite it"
    );
    if read_json::<ActiveVersion>(&root.join(ACTIVE))? == pending.new {
      write_json(&root.join(ACTIVE), &pending.old)?;
    }
  }
  fs::remove_file(root.join(JOURNAL))?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  fn frame() -> Frame {
    Frame {
      nonce: "a".repeat(64),
      process: ProcessIdentity { pid: 42, created: 123 },
      manifest: "b".repeat(64),
    }
  }
  #[test]
  fn normal_start_and_cef_helpers_do_not_read_input() {
    for (marker, arguments) in [
      (None, vec![]),
      (Some("0"), vec![]),
      (Some("1"), vec!["--type=renderer".into()]),
      (Some("1"), vec!["--type".into(), "gpu-process".into()]),
    ] {
      assert!(
        !guarded_confirmation(marker, &arguments, || panic!(
          "must not touch ordinary stdin/CEF challenge"
        ))
        .unwrap()
      );
    }
    assert!(guarded_confirmation(Some("1"), &[], || Ok(true)).unwrap());
  }
  #[test]
  fn scanner_bounds_logs_and_rejects_stale_process_version_and_replays() {
    let expected = frame();
    let bytes = frame_bytes(&expected).unwrap();
    let mut scanner = Scanner::default();
    assert!(scanner.push(b"ordinary boot log\n").unwrap().is_empty());
    assert!(scanner.push(&bytes[..7]).unwrap().is_empty());
    assert_eq!(scanner.push(&bytes[7..]).unwrap(), vec![expected.clone()]);
    let mut wrong_pid = expected.clone();
    wrong_pid.process.pid += 1;
    let mut reused_pid = expected.clone();
    reused_pid.process.created += 1;
    let mut old_attempt = expected.clone();
    old_attempt.nonce = "c".repeat(64);
    let mut wrong_version = expected.clone();
    wrong_version.manifest = "d".repeat(64);
    for altered in [wrong_pid, reused_pid, old_attempt, wrong_version] {
      let mut ready = false;
      assert!(accept_response(&altered, &expected, &mut ready).is_err());
      assert!(!ready);
    }
    let mut ready = false;
    accept_response(&expected, &expected, &mut ready).unwrap();
    assert!(accept_response(&expected, &expected, &mut ready).is_err());
    assert!(Scanner::default().push(&vec![b'x'; FRAME_LIMIT + 1]).is_err());
    let mut scanner = Scanner::default();
    for _ in 0..OUTPUT_LIMIT / 2 {
      scanner.push(b"x\n").unwrap();
    }
    assert!(scanner.push(b"x\n").is_err());
  }
  #[test]
  fn health_policy_has_explicit_bounded_deadline() {
    assert!(WindowsStartupHealth::new(Duration::from_millis(10), Duration::ZERO).is_err());
    assert!(WindowsStartupHealth::new(Duration::from_secs(2), Duration::from_secs(2)).is_err());
    assert!(WindowsStartupHealth::new(Duration::from_secs(30), Duration::from_secs(1)).is_ok());
    assert!(parse_frame(b"ordinary stdout\n").is_err());
  }
  #[test]
  fn raw_challenge_read_survives_writer_close_without_losing_prefetched_bytes() {
    use std::io::{BufReader, Cursor};
    struct Raw(Cursor<Vec<u8>>);
    impl ChallengeInput for Raw {
      fn available(&mut self) -> Result<usize> {
        let remaining = self.0.get_ref().len() - self.0.position() as usize;
        ensure!(remaining > 0, "OS pipe writer closed after frame");
        Ok(remaining)
      }
      fn read_raw(&mut self, buffer: &mut [u8]) -> Result<usize> {
        Ok(self.0.read(buffer)?)
      }
    }
    struct Buffered(BufReader<Cursor<Vec<u8>>>);
    impl ChallengeInput for Buffered {
      fn available(&mut self) -> Result<usize> {
        let remaining = self.0.get_ref().get_ref().len() - self.0.get_ref().position() as usize;
        ensure!(remaining > 0, "OS pipe is empty despite unread BufReader bytes");
        Ok(remaining)
      }
      fn read_raw(&mut self, buffer: &mut [u8]) -> Result<usize> {
        Ok(self.0.read(buffer)?)
      }
    }
    let expected = frame();
    let bytes = frame_bytes(&expected).unwrap();
    // Reproduce the original StdinLock failure: its first one-byte read prefetched the frame.
    assert!(
      read_challenge(
        &mut Buffered(BufReader::new(Cursor::new(bytes.clone()))),
        Instant::now() + Duration::from_secs(1)
      )
      .is_err()
    );
    assert_eq!(
      read_challenge(&mut Raw(Cursor::new(bytes)), Instant::now() + Duration::from_secs(1)).unwrap(),
      expected
    );
    assert!(
      read_challenge(
        &mut Raw(Cursor::new(vec![b'x'; FRAME_LIMIT + 1])),
        Instant::now() + Duration::from_secs(1)
      )
      .is_err()
    );
    assert!(read_challenge(&mut Raw(Cursor::new(frame_bytes(&frame()).unwrap())), Instant::now()).is_err());
  }
  #[test]
  fn exit_success_no_response_and_timeout_never_count_as_readiness() {
    let options = WindowsStartupHealth::new(Duration::from_secs(30), Duration::from_secs(1)).unwrap();
    assert!(!readiness(false, true, Duration::from_secs(1), &options).unwrap());
    assert!(!readiness(true, true, Duration::from_millis(100), &options).unwrap());
    assert!(readiness(true, true, Duration::from_secs(1), &options).unwrap());
    assert!(readiness(true, false, Duration::from_secs(1), &options).is_err());
    assert!(readiness(false, false, Duration::from_secs(1), &options).is_err());
    assert!(readiness(true, true, Duration::from_secs(30), &options).is_err());
  }
}
