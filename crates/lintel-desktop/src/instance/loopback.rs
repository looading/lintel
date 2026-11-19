//! Authenticated loopback transport for Windows, exercised on Unix by protocol tests.
use std::{
  fs::{self, File, OpenOptions},
  io::{Read, Write},
  net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
  path::{Path, PathBuf},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender},
  },
  thread::{self, JoinHandle},
  time::{Duration, Instant},
};

use fs2::FileExt;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::{Zeroize, Zeroizing};

use super::{Activation, MAX_FRAME_BYTES};
use crate::{
  DesktopError, Result,
  error::{invalid, native},
};

const AUTH_BYTES: usize = 32;
const FRAME_DEADLINE: Duration = Duration::from_millis(750);
const HELLO_DOMAIN: &[u8] = b"lintel/activation/v1/server";
const REQUEST_DOMAIN: &[u8] = b"lintel/activation/v1/request";
const ACK_DOMAIN: &[u8] = b"lintel/activation/v1/accepted";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Endpoint {
  version: u8,
  port: u16,
  token: [u8; AUTH_BYTES],
}
impl Drop for Endpoint {
  fn drop(&mut self) {
    self.token.zeroize();
  }
}

pub(super) enum Role {
  Primary(Primary),
  Secondary(Secondary),
}
pub(super) struct Primary {
  pub(super) receiver: Receiver<Activation>,
  endpoint: PathBuf,
  stopping: Arc<AtomicBool>,
  worker: Option<JoinHandle<()>>,
  _lock: File,
}
pub(super) struct Secondary {
  endpoint: PathBuf,
}

pub(super) fn acquire(directory: &Path, wake: impl Fn() + Send + Sync + 'static) -> Result<Role> {
  prepare_directory(directory)?;
  let directory = directory.canonicalize()?;
  let lock = open_private_file(&directory.join("instance.lock"), false)?;
  let endpoint = directory.join("activation.json");
  match FileExt::try_lock_exclusive(&lock) {
    Ok(()) => {}
    Err(error)
      if error.kind() == std::io::ErrorKind::WouldBlock || (cfg!(windows) && error.raw_os_error() == Some(33)) =>
    {
      return Ok(Role::Secondary(Secondary { endpoint }));
    }
    Err(error) => return Err(error.into()),
  }
  let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
  listener.set_nonblocking(true)?;
  let token = Zeroizing::new(random_bytes()?);
  let description = Endpoint {
    version: 1,
    port: listener.local_addr()?.port(),
    token: *token,
  };
  publish_endpoint(&endpoint, &description)?;
  let (sender, receiver) = mpsc::sync_channel(64);
  let stopping = Arc::new(AtomicBool::new(false));
  let stop = stopping.clone();
  let worker = thread::Builder::new()
    .name("lintel-activation".into())
    .spawn(move || {
      while !stop.load(Ordering::Acquire) {
        match listener.accept() {
          Ok((mut stream, address)) => {
            if stop.load(Ordering::Acquire) {
              break;
            }
            if !address.ip().is_loopback() {
              continue;
            }
            let _ = receive(&mut stream, &token, &sender, &wake);
          }
          Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            thread::park_timeout(Duration::from_millis(100))
          }
          Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
          Err(_) => break,
        }
      }
    })
    .map_err(|error| native("start authenticated activation listener", error))?;
  Ok(Role::Primary(Primary {
    receiver,
    endpoint,
    stopping,
    worker: Some(worker),
    _lock: lock,
  }))
}

impl Secondary {
  pub(super) fn activate(&self, activation: &Activation) -> Result<()> {
    activation.validate()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
      let connection = read_endpoint(&self.endpoint).and_then(|endpoint| {
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), endpoint.port);
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))?;
        Ok((endpoint, stream))
      });
      match connection {
        Ok((endpoint, stream)) => return send_connected(&endpoint, activation, stream),
        // Discovery can race the primary's initial publication. Authentication failure is final;
        // retrying a transmitted request could duplicate an activation whose acknowledgment was lost.
        Err(DesktopError::Io(error))
          if matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
          ) && Instant::now() < deadline =>
        {
          thread::sleep(Duration::from_millis(10))
        }
        Err(error) => return Err(error),
      }
    }
  }
}

fn read_endpoint(path: &Path) -> Result<Endpoint> {
  check_private_file(path)?;
  let mut bytes = Zeroizing::new(Vec::new());
  File::open(path)?.take(1025).read_to_end(&mut bytes)?;
  if bytes.len() > 1024 {
    return Err(invalid("activation discovery", "descriptor exceeds 1024 bytes"));
  }
  let endpoint: Endpoint =
    serde_json::from_slice(&bytes).map_err(|_| invalid("activation discovery", "invalid descriptor"))?;
  if endpoint.version != 1 || endpoint.port == 0 || endpoint.token.iter().all(|byte| *byte == 0) {
    return Err(invalid(
      "activation discovery",
      "unsupported protocol or invalid endpoint",
    ));
  }
  Ok(endpoint)
}

fn publish_endpoint(path: &Path, endpoint: &Endpoint) -> Result<()> {
  let suffix = random_bytes()?
    .iter()
    .take(16)
    .map(|byte| format!("{byte:02x}"))
    .collect::<String>();
  let temporary = path.with_extension(format!("{suffix}.tmp"));
  let result = (|| {
    let mut file = open_private_file(&temporary, true)?;
    let bytes =
      Zeroizing::new(serde_json::to_vec(endpoint).map_err(|error| native("serialize activation discovery", error))?);
    file.write_all(&bytes)?;
    file.sync_all()?;
    // The primary holds the permanent lock inode before removing stale discovery state.
    match fs::remove_file(path) {
      Ok(()) => {}
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
      Err(error) => return Err(error.into()),
    }
    fs::rename(&temporary, path)?;
    Ok(())
  })();
  if result.is_err() {
    let _ = fs::remove_file(temporary);
  }
  result
}

fn random_bytes() -> Result<[u8; AUTH_BYTES]> {
  let mut bytes = [0; AUTH_BYTES];
  getrandom::fill(&mut bytes).map_err(|error| native("generate activation authentication material", error))?;
  Ok(bytes)
}

fn mac(token: &[u8; AUTH_BYTES], domain: &[u8], pieces: &[&[u8]]) -> Hmac<Sha256> {
  let mut mac = Hmac::<Sha256>::new_from_slice(token).expect("HMAC accepts a 32-byte key");
  mac.update(domain);
  for piece in pieces {
    mac.update(piece);
  }
  mac
}
fn authentication_failed() -> DesktopError {
  native("authenticate activation peer", "invalid authentication proof")
}

fn receive(
  stream: &mut TcpStream,
  token: &[u8; AUTH_BYTES],
  sender: &SyncSender<Activation>,
  wake: &dyn Fn(),
) -> Result<()> {
  // Accepted sockets can inherit O_NONBLOCK from the listener on macOS.
  stream.set_nonblocking(false)?;
  let deadline = Instant::now() + FRAME_DEADLINE;
  let mut client_nonce = [0; AUTH_BYTES];
  read_before_deadline(stream, &mut client_nonce, deadline)?;
  let server_nonce = random_bytes()?;
  let hello = mac(token, HELLO_DOMAIN, &[&client_nonce, &server_nonce])
    .finalize()
    .into_bytes();
  write_before_deadline(stream, &server_nonce, deadline)?;
  write_before_deadline(stream, &hello, deadline)?;
  let mut header = [0; 4];
  read_before_deadline(stream, &mut header, deadline)?;
  let length = u32::from_be_bytes(header) as usize;
  if length == 0 || length > MAX_FRAME_BYTES {
    return Err(invalid("activation frame", "invalid length"));
  }
  let mut body = Zeroizing::new(vec![0; length]);
  let mut proof = [0; AUTH_BYTES];
  read_before_deadline(stream, &mut body, deadline)?;
  read_before_deadline(stream, &mut proof, deadline)?;
  mac(token, REQUEST_DOMAIN, &[&client_nonce, &server_nonce, &header, &body])
    .verify_slice(&proof)
    .map_err(|_| authentication_failed())?;
  let activation: Activation =
    serde_json::from_slice(&body).map_err(|_| invalid("activation JSON", "invalid launch data"))?;
  activation.validate()?;
  sender
    .try_send(activation)
    .map_err(|error| native("queue activation", error))?;
  wake();
  let ack = mac(token, ACK_DOMAIN, &[&client_nonce, &server_nonce, &proof])
    .finalize()
    .into_bytes();
  write_before_deadline(stream, &ack, deadline)
}

#[cfg(test)]
fn send(endpoint: &Endpoint, activation: &Activation) -> Result<()> {
  let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), endpoint.port);
  let stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))?;
  send_connected(endpoint, activation, stream)
}

fn send_connected(endpoint: &Endpoint, activation: &Activation, mut stream: TcpStream) -> Result<()> {
  let deadline = Instant::now() + FRAME_DEADLINE;
  let client_nonce = random_bytes()?;
  write_before_deadline(&mut stream, &client_nonce, deadline)?;
  let mut server_nonce = [0; AUTH_BYTES];
  let mut proof = [0; AUTH_BYTES];
  read_before_deadline(&mut stream, &mut server_nonce, deadline)?;
  read_before_deadline(&mut stream, &mut proof, deadline)?;
  mac(&endpoint.token, HELLO_DOMAIN, &[&client_nonce, &server_nonce])
    .verify_slice(&proof)
    .map_err(|_| authentication_failed())?;
  // Do not disclose activation data to a port-reuse attacker before authenticating the server.
  let body = Zeroizing::new(serde_json::to_vec(activation).map_err(|error| native("serialize activation", error))?);
  let header = (body.len() as u32).to_be_bytes();
  let proof = mac(
    &endpoint.token,
    REQUEST_DOMAIN,
    &[&client_nonce, &server_nonce, &header, &body],
  )
  .finalize()
  .into_bytes();
  write_before_deadline(&mut stream, &header, deadline)?;
  write_before_deadline(&mut stream, &body, deadline)?;
  write_before_deadline(&mut stream, &proof, deadline)?;
  let mut ack = [0; AUTH_BYTES];
  read_before_deadline(&mut stream, &mut ack, deadline)?;
  mac(&endpoint.token, ACK_DOMAIN, &[&client_nonce, &server_nonce, &proof])
    .verify_slice(&ack)
    .map_err(|_| authentication_failed())
}

fn remaining(deadline: Instant) -> Result<Duration> {
  deadline
    .checked_duration_since(Instant::now())
    .filter(|remaining| !remaining.is_zero())
    .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "activation protocol deadline elapsed").into())
}
fn read_before_deadline(stream: &mut TcpStream, mut bytes: &mut [u8], deadline: Instant) -> Result<()> {
  while !bytes.is_empty() {
    stream.set_read_timeout(Some(remaining(deadline)?))?;
    match stream.read(bytes) {
      Ok(0) => {
        return Err(
          std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "incomplete authenticated activation frame",
          )
          .into(),
        );
      }
      Ok(count) => bytes = &mut bytes[count..],
      Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
      Err(error) => return Err(error.into()),
    }
  }
  Ok(())
}
fn write_before_deadline(stream: &mut TcpStream, mut bytes: &[u8], deadline: Instant) -> Result<()> {
  while !bytes.is_empty() {
    stream.set_write_timeout(Some(remaining(deadline)?))?;
    match stream.write(bytes) {
      Ok(0) => {
        return Err(
          std::io::Error::new(
            std::io::ErrorKind::WriteZero,
            "activation connection stopped accepting bytes",
          )
          .into(),
        );
      }
      Ok(count) => bytes = &bytes[count..],
      Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
      Err(error) => return Err(error.into()),
    }
  }
  Ok(())
}

impl Drop for Primary {
  fn drop(&mut self) {
    self.stopping.store(true, Ordering::Release);
    if let Some(worker) = self.worker.take() {
      worker.thread().unpark();
      let _ = worker.join();
    }
    let _ = fs::remove_file(&self.endpoint);
  }
}

#[cfg(unix)]
fn prepare_directory(path: &Path) -> Result<()> {
  use std::os::unix::fs::{DirBuilderExt, MetadataExt};
  fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_dir() || metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
    return Err(invalid(
      "activation directory",
      "expected an owner-only directory owned by the current user",
    ));
  }
  Ok(())
}
#[cfg(unix)]
fn open_private_file(path: &Path, exclusive: bool) -> Result<File> {
  use std::os::unix::fs::OpenOptionsExt;
  let file = OpenOptions::new()
    .read(true)
    .write(true)
    .create(!exclusive)
    .create_new(exclusive)
    .truncate(false)
    .mode(0o600)
    .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
    .open(path)?;
  check_private_file(path)?;
  Ok(file)
}
#[cfg(unix)]
fn check_private_file(path: &Path) -> Result<()> {
  use std::os::unix::fs::MetadataExt;
  let metadata = fs::symlink_metadata(path)?;
  if !metadata.is_file() || metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
    return Err(invalid(
      "activation file",
      "expected an owner-only regular file owned by the current user",
    ));
  }
  Ok(())
}

#[cfg(windows)]
fn prepare_directory(path: &Path) -> Result<()> {
  fs::create_dir_all(path)?;
  reject_reparse(path, true)?;
  protect_windows_path(path)
}
#[cfg(windows)]
fn open_private_file(path: &Path, exclusive: bool) -> Result<File> {
  use std::os::windows::fs::OpenOptionsExt;
  // Open the reparse point itself rather than following it; its attributes are checked below.
  let file = OpenOptions::new()
    .read(true)
    .write(true)
    .create(!exclusive)
    .create_new(exclusive)
    .truncate(false)
    .custom_flags(0x0020_0000)
    .open(path)?;
  reject_reparse(path, false)?;
  protect_windows_path(path)?;
  Ok(file)
}
#[cfg(windows)]
fn reject_reparse(path: &Path, directory: bool) -> Result<()> {
  use std::os::windows::fs::MetadataExt;
  let metadata = fs::symlink_metadata(path)?;
  if metadata.file_attributes() & 0x400 != 0 || (directory && !metadata.is_dir()) || (!directory && !metadata.is_file())
  {
    return Err(invalid(
      "activation path",
      "reparse points and unexpected file types are rejected",
    ));
  }
  Ok(())
}
#[cfg(windows)]
fn protect_windows_path(path: &Path) -> Result<()> {
  use windows_permissions::{
    LocalBox, SecurityDescriptor,
    constants::{SeObjectType, SecurityInformation},
    utilities::current_process_sid,
    wrappers,
  };
  let sid = current_process_sid()?;
  let current = wrappers::GetNamedSecurityInfo(
    path.as_os_str(),
    SeObjectType::SE_FILE_OBJECT,
    SecurityInformation::Owner,
  )?;
  if current.owner() != Some(&*sid) {
    return Err(invalid(
      "activation path owner",
      "path must be owned by the current process user",
    ));
  }
  let descriptor: LocalBox<SecurityDescriptor> = format!("D:P(A;OICI;FA;;;{sid})").parse()?;
  wrappers::SetNamedSecurityInfo(
    path.as_os_str(),
    SeObjectType::SE_FILE_OBJECT,
    SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
    None,
    None,
    descriptor.dacl(),
    None,
  )?;
  check_windows_acl(path)
}
#[cfg(windows)]
fn check_windows_acl(path: &Path) -> Result<()> {
  use windows_permissions::{
    constants::{AceType, SeObjectType, SecurityInformation},
    utilities::current_process_sid,
    wrappers,
  };
  let sid = current_process_sid()?;
  let descriptor = wrappers::GetNamedSecurityInfo(
    path.as_os_str(),
    SeObjectType::SE_FILE_OBJECT,
    SecurityInformation::Owner | SecurityInformation::Dacl,
  )?;
  let dacl = descriptor
    .dacl()
    .ok_or_else(|| invalid("activation ACL", "a null DACL is not private"))?;
  if descriptor.owner() != Some(&*sid) || dacl.len() == 0 {
    return Err(invalid(
      "activation ACL",
      "expected an ACL owned by and granting access only to the current user",
    ));
  }
  for index in 0..dacl.len() {
    let ace = dacl
      .get_ace(index)
      .ok_or_else(|| invalid("activation ACL", "invalid ACE"))?;
    if ace.ace_type() != AceType::ACCESS_ALLOWED_ACE_TYPE || ace.sid() != Some(&*sid) {
      return Err(invalid(
        "activation ACL",
        "access is granted to a principal other than the current user",
      ));
    }
  }
  Ok(())
}
#[cfg(windows)]
fn check_private_file(path: &Path) -> Result<()> {
  reject_reparse(path, false)?;
  check_windows_acl(path)
}

#[cfg(test)]
mod tests {
  use super::*;
  fn activation() -> Activation {
    Activation {
      arguments: vec!["--show".into()],
      working_directory: std::env::temp_dir(),
      urls: vec!["lintel-test://open/42".into()],
    }
  }

  #[test]
  fn authenticated_loopback_delivers_and_releases_discovery_and_lock() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("activation");
    let (wake_tx, wake_rx) = mpsc::channel();
    let Role::Primary(primary) = acquire(&directory, move || {
      let _ = wake_tx.send(());
    })
    .unwrap() else {
      panic!("primary expected")
    };
    let Role::Secondary(secondary) = acquire(&directory, || {}).unwrap() else {
      panic!("secondary expected")
    };
    secondary.activate(&activation()).unwrap();
    wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(primary.receiver.try_recv().unwrap(), activation());
    drop(primary);
    assert!(!directory.join("activation.json").exists());
    assert!(matches!(acquire(&directory, || {}).unwrap(), Role::Primary(_)));
  }

  #[test]
  fn wrong_token_is_rejected_before_activation_data_is_transmitted() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      let (sender, receiver) = mpsc::sync_channel(1);
      assert!(receive(&mut stream, &[42; AUTH_BYTES], &sender, &|| {}).is_err());
      assert!(receiver.try_recv().is_err());
    });
    let wrong = Endpoint {
      version: 1,
      port,
      token: [7; AUTH_BYTES],
    };
    assert!(matches!(
      send(&wrong, &activation()),
      Err(DesktopError::Native {
        operation: "authenticate activation peer",
        ..
      })
    ));
    server.join().unwrap();
  }

  #[test]
  fn tampered_authenticated_request_is_not_delivered() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("activation");
    let Role::Primary(primary) = acquire(&directory, || {}).unwrap() else {
      panic!("primary expected")
    };
    let endpoint = read_endpoint(&directory.join("activation.json")).unwrap();
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, endpoint.port)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let client_nonce = [7; AUTH_BYTES];
    write_before_deadline(&mut stream, &client_nonce, deadline).unwrap();
    let mut server_nonce = [0; AUTH_BYTES];
    let mut server_proof = [0; AUTH_BYTES];
    read_before_deadline(&mut stream, &mut server_nonce, deadline).unwrap();
    read_before_deadline(&mut stream, &mut server_proof, deadline).unwrap();
    let body = serde_json::to_vec(&activation()).unwrap();
    let header = (body.len() as u32).to_be_bytes();
    let proof = mac(
      &endpoint.token,
      REQUEST_DOMAIN,
      &[&client_nonce, &server_nonce, &header, &body],
    )
    .finalize()
    .into_bytes();
    let mut tampered = body;
    tampered[0] ^= 1;
    write_before_deadline(&mut stream, &header, deadline).unwrap();
    write_before_deadline(&mut stream, &tampered, deadline).unwrap();
    write_before_deadline(&mut stream, &proof, deadline).unwrap();
    let mut ack = [0; AUTH_BYTES];
    assert!(read_before_deadline(&mut stream, &mut ack, deadline).is_err());
    assert!(primary.receiver.try_recv().is_err());
  }

  #[test]
  fn nonprivate_discovery_is_rejected() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("activation.json");
    fs::write(&path, b"not valid discovery").unwrap();
    assert!(read_endpoint(&path).is_err());
  }
}
