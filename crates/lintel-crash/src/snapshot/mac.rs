use std::{
  ffi::CString,
  fs::{self, File, OpenOptions},
  io::{Read, Seek, SeekFrom, Write},
  os::{fd::AsFd, unix::fs::OpenOptionsExt},
  path::Path,
  process::{Child, Command, Stdio},
  thread,
  time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use mach2::message::MACH_MSG_TYPE_MAKE_SEND;
use minidump_writer::minidump_writer::MinidumpWriter;

use super::SnapshotError;
use crate::{TOKEN_BYTES, ensure_private_directory};

mod ports;
use ports::{ReceiveRight, SendRight};

const SWITCH: &str = "--lintel-snapshot-helper";
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_TIMEOUT: Duration = Duration::from_secs(120);
const RENDEZVOUS_BYTES: usize = 32;
const HELPER_HELLO: i32 = 1;
const SELF_TASK: i32 = 2;

pub(super) fn capture(helper: &Path, path: &Path, timeout: Duration, is_cancelled: impl Fn() -> bool) -> Result<()> {
  if timeout.is_zero() {
    return Err(SnapshotError::TimedOut.into());
  }
  ensure!(timeout <= MAX_TIMEOUT, "invalid native snapshot timeout");
  let deadline = Instant::now() + timeout;
  check_deadline(deadline, &is_cancelled)?;
  ensure!(
    helper.is_absolute() && path.is_absolute(),
    "snapshot paths must be absolute"
  );
  let helper = helper
    .canonicalize()
    .context("cannot find the bundled snapshot helper")?;
  ensure!(helper.is_file(), "snapshot helper is not a regular executable file");
  ensure_private_directory(path.parent().context("snapshot staging directory is missing")?)?;

  let mut random = [0_u8; RENDEZVOUS_BYTES + TOKEN_BYTES];
  getrandom::fill(&mut random).map_err(|error| anyhow::anyhow!("cannot create snapshot authentication: {error}"))?;
  let (nonce, token) = random.split_at(RENDEZVOUS_BYTES);
  let token: &[u8; TOKEN_BYTES] = token.try_into()?;
  let server = ReceiveRight::check_in(&service_name(nonce)?)?;
  let mut output = StagedDump::create(path)?;
  check_deadline(deadline, &is_cancelled)?;
  let mut child = SnapshotChild(
    Command::new(helper)
      .arg(SWITCH)
      .arg(timeout.as_millis().max(1).to_string())
      .stdin(Stdio::piped())
      .stdout(Stdio::from(output.file.try_clone()?))
      .stderr(Stdio::null())
      .spawn()
      .context("cannot launch the bundled snapshot helper")?,
  );
  {
    let mut input = child.0.stdin.take().context("snapshot helper has no control pipe")?;
    input.write_all(&std::process::id().to_ne_bytes())?;
    input.write_all(&random)?;
  }

  let child_pid = i32::try_from(child.0.id())?;
  let peer = loop {
    let remaining = check_deadline(deadline, &is_cancelled)?;
    if let Some(peer) = server.receive(HELPER_HELLO, child_pid, token, remaining.min(POLL_INTERVAL))? {
      break peer;
    }
    ensure!(
      child.0.try_wait()?.is_none(),
      "snapshot helper exited before authentication"
    );
  };
  let remaining = check_deadline(deadline, &is_cancelled)?;
  peer.send_self_task(SELF_TASK, token, remaining)?;
  drop((server, peer));

  loop {
    let remaining = check_deadline(deadline, &is_cancelled)?;
    if let Some(status) = child.0.try_wait()? {
      ensure!(status.success(), "snapshot helper failed to write the minidump");
      break;
    }
    thread::sleep(remaining.min(POLL_INTERVAL));
  }
  validate_dump(&mut output.file)?;
  check_deadline(deadline, &is_cancelled)?;
  output.keep = true;
  Ok(())
}

pub(super) fn helper_entry() -> Result<()> {
  let mut arguments = std::env::args_os().skip(1);
  ensure!(
    arguments.next().is_some_and(|arg| arg == SWITCH),
    "private snapshot helper invocation required"
  );
  let timeout_ms: u64 = arguments
    .next()
    .as_deref()
    .and_then(|arg| arg.to_str())
    .context("invalid snapshot helper timeout")?
    .parse()?;
  let timeout = Duration::from_millis(timeout_ms);
  ensure!(
    arguments.next().is_none() && !timeout.is_zero() && timeout <= MAX_TIMEOUT,
    "invalid snapshot helper invocation"
  );
  let deadline = Instant::now() + timeout;
  // The process owns a single capture. Bound its lifetime even when the host dies during a dump.
  thread::Builder::new()
    .name("lintel-snapshot-deadline".into())
    .spawn(move || {
      thread::sleep(timeout);
      std::process::exit(3);
    })?;

  let mut pid = [0_u8; 4];
  let mut random = [0_u8; RENDEZVOUS_BYTES + TOKEN_BYTES];
  {
    let mut input = std::io::stdin().lock();
    input.read_exact(&mut pid).context("missing snapshot parent")?;
    input
      .read_exact(&mut random)
      .context("missing snapshot authentication")?;
  }
  let parent = i32::from_ne_bytes(pid);
  // SAFETY: `getppid` has no arguments and returns the kernel's current parent PID.
  let actual_parent = unsafe { libc::getppid() };
  ensure!(
    parent > 0 && parent == actual_parent,
    "snapshot helper has the wrong parent"
  );
  let (nonce, token) = random.split_at(RENDEZVOUS_BYTES);
  let token: &[u8; TOKEN_BYTES] = token.try_into()?;
  let peer = SendRight::look_up(&service_name(nonce)?)?;
  let server = ReceiveRight::allocate()?;
  peer.send(
    HELPER_HELLO,
    server.name(),
    MACH_MSG_TYPE_MAKE_SEND,
    token,
    check_deadline(deadline, &|| false)?,
  )?;
  drop(peer);
  let task = server
    .receive(SELF_TASK, parent, token, check_deadline(deadline, &|| false)?)?
    .ok_or(SnapshotError::TimedOut)?;
  drop(server);
  ensure!(task.pid()? == parent, "snapshot task does not belong to the host");

  // Stdout is the only output capability the host supplies. Duplicate its inherited file
  // descriptor so the writer can seek without opening any path inside the helper.
  let mut file = File::from(std::io::stdout().as_fd().try_clone_to_owned()?);
  ensure!(
    file.metadata()?.is_file(),
    "snapshot helper output must be a regular file"
  );
  // A null handler thread keeps all available host threads in the non-exception dump.
  MinidumpWriter::new(Some(task.name()), Some(mach2::port::MACH_PORT_NULL)).dump(&mut file)?;
  file.flush()?;
  file.sync_all()?;
  Ok(())
}

fn service_name(nonce: &[u8]) -> Result<CString> {
  let mut name = String::from("org.lintel.snapshot.");
  use std::fmt::Write;
  for byte in nonce {
    write!(&mut name, "{byte:02x}")?;
  }
  Ok(CString::new(name)?)
}

fn check_deadline(deadline: Instant, is_cancelled: &impl Fn() -> bool) -> Result<Duration> {
  if is_cancelled() {
    return Err(SnapshotError::Cancelled.into());
  }
  let remaining = deadline.saturating_duration_since(Instant::now());
  if remaining.is_zero() {
    return Err(SnapshotError::TimedOut.into());
  }
  Ok(remaining)
}

struct SnapshotChild(Child);

impl Drop for SnapshotChild {
  fn drop(&mut self) {
    if !matches!(self.0.try_wait(), Ok(Some(_))) {
      let _ = self.0.kill();
      let _ = self.0.wait();
    }
  }
}

struct StagedDump<'a> {
  path: &'a Path,
  file: File,
  keep: bool,
}

impl<'a> StagedDump<'a> {
  fn create(path: &'a Path) -> std::io::Result<Self> {
    let file = OpenOptions::new()
      .read(true)
      .write(true)
      .create_new(true)
      .mode(0o600)
      .open(path)?;
    Ok(Self {
      path,
      file,
      keep: false,
    })
  }
}

impl Drop for StagedDump<'_> {
  fn drop(&mut self) {
    if !self.keep {
      let _ = fs::remove_file(self.path);
    }
  }
}

fn validate_dump(file: &mut File) -> Result<()> {
  let size = file.metadata()?.len();
  let mut header = [0_u8; 32];
  file.rewind()?;
  file
    .read_exact(&mut header)
    .context("incomplete native snapshot header")?;
  ensure!(
    &header[..4] == b"MDMP" && word(&header, 4) & 0xffff == 0xa793,
    "invalid native snapshot header"
  );
  let streams = word(&header, 8);
  let directory = u64::from(word(&header, 12));
  ensure!(
    (1..=1024).contains(&streams) && directory >= 32 && directory + u64::from(streams) * 12 <= size,
    "invalid native snapshot stream directory"
  );
  let mut has_threads = false;
  let mut has_modules = false;
  for index in 0..u64::from(streams) {
    let mut entry = [0_u8; 12];
    file.seek(SeekFrom::Start(directory + index * 12))?;
    file.read_exact(&mut entry)?;
    let kind = word(&entry, 0);
    let length = u64::from(word(&entry, 4));
    let offset = u64::from(word(&entry, 8));
    ensure!(offset + length <= size, "invalid native snapshot stream bounds");
    if matches!(kind, 3 | 4) {
      ensure!(length >= 4, "incomplete native snapshot list");
      file.seek(SeekFrom::Start(offset))?;
      let mut count = [0_u8; 4];
      file.read_exact(&mut count)?;
      let count = u64::from(u32::from_le_bytes(count));
      let record_size = if kind == 3 { 48 } else { 108 };
      ensure!(
        count > 0 && 4 + count * record_size <= length,
        "invalid native snapshot list bounds"
      );
      if kind == 3 {
        has_threads = true;
      } else {
        has_modules = true;
      }
    }
  }
  ensure!(
    has_threads && has_modules,
    "native snapshot is missing its thread or module list"
  );
  Ok(())
}

fn word(bytes: &[u8], offset: usize) -> u32 {
  u32::from_le_bytes(
    bytes[offset..offset + 4]
      .try_into()
      .expect("fixed minidump field width"),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn handoff_checks_both_kernel_sender_and_inherited_secret() {
    let mut nonce = [0_u8; RENDEZVOUS_BYTES];
    getrandom::fill(&mut nonce).unwrap();
    let name = service_name(&nonce).unwrap();
    let server = ReceiveRight::check_in(&name).unwrap();
    let client = SendRight::look_up(&name).unwrap();
    let reply = ReceiveRight::allocate().unwrap();
    let pid = i32::try_from(std::process::id()).unwrap();
    let token = [1_u8; TOKEN_BYTES];
    let timeout = Duration::from_secs(1);
    for (expected_pid, expected_token) in [(pid + 1, [1; TOKEN_BYTES]), (pid, [2; TOKEN_BYTES])] {
      client
        .send(HELPER_HELLO, reply.name(), MACH_MSG_TYPE_MAKE_SEND, &token, timeout)
        .unwrap();
      assert!(
        server
          .receive(HELPER_HELLO, expected_pid, &expected_token, timeout)
          .is_err()
      );
    }
    client
      .send(HELPER_HELLO, reply.name(), MACH_MSG_TYPE_MAKE_SEND, &token, timeout)
      .unwrap();
    assert!(server.receive(HELPER_HELLO, pid, &token, timeout).unwrap().is_some());
  }
}
