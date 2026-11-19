//! Single-instance launch forwarding. Unix transport endpoints live inside an owner-only directory.
//! Windows uses a per-user ACL-protected descriptor and mutually authenticated loopback transport.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Result, error::invalid};

#[cfg(any(windows, test))]
mod loopback;

const MAX_FRAME_BYTES: usize = 64 * 1024;

/// Untrusted launch data. Validate protocol URLs with ProtocolRouter and authorize file paths
/// before acting on them. Receiving this structure does not execute any command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activation {
  pub arguments: Vec<String>,
  pub working_directory: PathBuf,
  pub urls: Vec<String>,
}

impl Activation {
  pub fn validate(&self) -> Result<()> {
    if self.arguments.len() > 4096 || self.urls.len() > 256 || !self.working_directory.is_absolute() {
      return Err(invalid(
        "activation",
        "too many arguments/URLs or working directory is not absolute",
      ));
    }
    for value in self.arguments.iter().chain(self.urls.iter()) {
      crate::model::validate_text("activation argument/URL", value, MAX_FRAME_BYTES)?;
    }
    let encoded = serde_json::to_vec(self).map_err(|e| invalid("activation", e.to_string()))?;
    if encoded.len() > MAX_FRAME_BYTES {
      return Err(invalid("activation", "encoded launch data exceeds 64 KiB"));
    }
    Ok(())
  }
}

/// Derive a stable compact directory name for transport discovery. Keeping an application ID out
/// of the socket path avoids Unix SUN_LEN limits. `base` should be the user's runtime/temp root;
/// acquire still validates current-user ownership and private permissions before using it.
pub fn namespace_directory(base: &Path, namespace: &str) -> Result<PathBuf> {
  use sha2::{Digest, Sha256};
  crate::model::validate_text("instance namespace", namespace, 4096)?;
  if namespace.is_empty() || !base.is_absolute() {
    return Err(invalid(
      "instance namespace",
      "requires a nonempty namespace and absolute base directory",
    ));
  }
  let digest = Sha256::digest(namespace.as_bytes());
  let short: String = digest[..12].iter().map(|byte| format!("{byte:02x}")).collect();
  #[cfg(unix)]
  let name = format!("li{:x}-{short}", rustix::process::geteuid().as_raw());
  #[cfg(not(unix))]
  let name = format!("li-{short}");
  Ok(base.join(name))
}

pub enum InstanceRole {
  Primary(PrimaryInstance),
  Secondary(SecondaryInstance),
}

/// Claim a dedicated per-user application instance directory. Unix uses an owner-only directory
/// and local socket. Windows applies a current-user-only protected DACL and uses a mutually
/// authenticated loopback endpoint. Keep PrimaryInstance alive until shutdown; the lock file is
/// intentionally retained to prevent lock-inode races.
pub fn acquire(directory: &Path, wake: impl Fn() + Send + Sync + 'static) -> Result<InstanceRole> {
  #[cfg(unix)]
  {
    unix::acquire(directory, wake)
  }
  #[cfg(windows)]
  {
    match loopback::acquire(directory, wake)? {
      loopback::Role::Primary(inner) => Ok(InstanceRole::Primary(PrimaryInstance { inner })),
      loopback::Role::Secondary(inner) => Ok(InstanceRole::Secondary(SecondaryInstance { inner })),
    }
  }
  #[cfg(not(any(unix, windows)))]
  {
    let _ = (directory, wake);
    Err(crate::error::unsupported(
      "single-instance activation",
      "activation requires a supported Unix or Windows local transport",
    ))
  }
}

/// Receives launches queued by secondary processes. This type owns the lock and listener worker.
pub struct PrimaryInstance {
  #[cfg(unix)]
  inner: unix::Primary,
  #[cfg(windows)]
  inner: loopback::Primary,
}

impl PrimaryInstance {
  pub fn try_recv(&self) -> Option<Activation> {
    #[cfg(any(unix, windows))]
    {
      self.inner.receiver.try_recv().ok()
    }
    #[cfg(not(any(unix, windows)))]
    {
      None
    }
  }
}

pub struct SecondaryInstance {
  #[cfg(unix)]
  endpoint: PathBuf,
  #[cfg(windows)]
  inner: loopback::Secondary,
}

impl SecondaryInstance {
  /// Return only after the primary queues the message and acknowledges it. The message can still
  /// be declined by the application's own authorization/routing rules.
  pub fn activate(&self, activation: &Activation) -> Result<()> {
    activation.validate()?;
    #[cfg(unix)]
    {
      unix::activate(&self.endpoint, activation)
    }
    #[cfg(windows)]
    {
      self.inner.activate(activation)
    }
    #[cfg(not(any(unix, windows)))]
    {
      Err(crate::error::unsupported(
        "single-instance activation",
        "activation requires a supported Unix or Windows local transport",
      ))
    }
  }
}

#[cfg(unix)]
mod unix {
  use super::*;
  use crate::error::native;
  use fs2::FileExt;
  use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
      fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
      net::{UnixListener, UnixStream},
    },
    sync::{
      Arc,
      atomic::{AtomicBool, Ordering},
      mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
  };

  pub(super) struct Primary {
    pub(super) receiver: Receiver<Activation>,
    endpoint: PathBuf,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    _lock: File,
  }

  pub(super) fn acquire(directory: &Path, wake: impl Fn() + Send + Sync + 'static) -> Result<InstanceRole> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(directory)?;
    let metadata = fs::symlink_metadata(directory)?;
    validate_private_metadata(&metadata, true)?;
    let directory = directory.canonicalize()?;
    let lock_path = directory.join("instance.lock");
    let lock = OpenOptions::new()
      .read(true)
      .write(true)
      .create(true)
      .truncate(false)
      .mode(0o600)
      .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
      .open(lock_path)?;
    validate_private_metadata(&lock.metadata()?, false)?;
    let endpoint = directory.join("activation.sock");
    match FileExt::try_lock_exclusive(&lock) {
      Ok(()) => {}
      Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
        return Ok(InstanceRole::Secondary(SecondaryInstance { endpoint }));
      }
      Err(error) => return Err(error.into()),
    }
    match fs::symlink_metadata(&endpoint) {
      Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&endpoint)?,
      Ok(_) => return Err(invalid("activation endpoint", "stale endpoint is not a Unix socket")),
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
      Err(error) => return Err(error.into()),
    }
    let listener = UnixListener::bind(&endpoint)?;
    listener.set_nonblocking(true)?;
    fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600))?;
    let (sender, receiver) = mpsc::sync_channel(64);
    let stopping = Arc::new(AtomicBool::new(false));
    let stop = stopping.clone();
    let worker = thread::Builder::new()
      .name("lintel-activation".into())
      .spawn(move || {
        while !stop.load(Ordering::Acquire) {
          match listener.accept() {
            Ok((mut stream, _)) => {
              if stop.load(Ordering::Acquire) {
                break;
              }
              if stream.set_write_timeout(Some(Duration::from_millis(500))).is_err() {
                continue;
              }
              let _ = receive_activation(&mut stream, &sender, &wake);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
              thread::park_timeout(Duration::from_millis(100))
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
          }
        }
      })
      .map_err(|e| native("start activation listener", e))?;
    Ok(InstanceRole::Primary(PrimaryInstance {
      inner: Primary {
        receiver,
        endpoint,
        stopping,
        worker: Some(worker),
        _lock: lock,
      },
    }))
  }

  fn validate_private_metadata(metadata: &fs::Metadata, directory: bool) -> Result<()> {
    if (directory && !metadata.is_dir())
      || (!directory && !metadata.is_file())
      || metadata.uid() != rustix::process::geteuid().as_raw()
      || metadata.permissions().mode() & 0o077 != 0
    {
      return Err(invalid(
        "instance directory/lock",
        "expected an owner-only directory/file owned by the current user",
      ));
    }
    Ok(())
  }

  fn receive_activation(stream: &mut UnixStream, sender: &SyncSender<Activation>, wake: &dyn Fn()) -> Result<()> {
    stream.set_nonblocking(false)?;
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut header = [0; 4];
    read_before_deadline(stream, &mut header, deadline)?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
      return Err(invalid("activation frame", "invalid length"));
    }
    let mut body = vec![0; length];
    read_before_deadline(stream, &mut body, deadline)?;
    let activation: Activation =
      serde_json::from_slice(&body).map_err(|e| invalid("activation JSON", e.to_string()))?;
    activation.validate()?;
    sender.try_send(activation).map_err(|e| native("queue activation", e))?;
    wake();
    stream.write_all(&[1])?;
    Ok(())
  }

  fn read_before_deadline(stream: &mut UnixStream, mut bytes: &mut [u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "activation frame deadline elapsed"))?;
      stream.set_read_timeout(Some(remaining))?;
      match stream.read(bytes) {
        Ok(0) => {
          return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "incomplete activation frame").into());
        }
        Ok(count) => bytes = &mut bytes[count..],
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
        Err(error) => return Err(error.into()),
      }
    }
    Ok(())
  }

  pub(super) fn activate(endpoint: &Path, activation: &Activation) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut stream = loop {
      match UnixStream::connect(endpoint) {
        Ok(stream) => break stream,
        Err(error)
          if matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
          ) && Instant::now() < deadline =>
        {
          thread::sleep(Duration::from_millis(10))
        }
        Err(error) => return Err(error.into()),
      }
    };
    let timeout = Some(Duration::from_secs(2));
    stream.set_read_timeout(timeout)?;
    stream.set_write_timeout(timeout)?;
    let body = serde_json::to_vec(activation).map_err(|e| invalid("activation", e.to_string()))?;
    stream.write_all(&(body.len() as u32).to_be_bytes())?;
    stream.write_all(&body)?;
    let mut ack = [0];
    stream.read_exact(&mut ack)?;
    if ack != [1] {
      return Err(invalid(
        "activation acknowledgment",
        "primary did not accept launch data",
      ));
    }
    Ok(())
  }

  impl Drop for Primary {
    fn drop(&mut self) {
      self.stopping.store(true, Ordering::Release);
      // Unpark even if another process removed the socket path. In-flight reads have an absolute deadline.
      if let Some(worker) = self.worker.take() {
        worker.thread().unpark();
        let _ = worker.join();
      }
      let _ = fs::remove_file(&self.endpoint);
      // The lock is released only after the worker and socket are gone.
    }
  }

  #[cfg(test)]
  mod tests {
    use super::*;

    #[test]
    fn real_local_ipc_forwards_launch_and_releases_lock_on_drop() {
      let temporary = tempfile::tempdir().unwrap();
      let directory = temporary.path().join("instance");
      let (wake_tx, wake_rx) = mpsc::channel();
      let InstanceRole::Primary(primary) = acquire(&directory, move || {
        let _ = wake_tx.send(());
      })
      .unwrap() else {
        panic!("first launch must be primary")
      };
      let InstanceRole::Secondary(secondary) = acquire(&directory, || {}).unwrap() else {
        panic!("second launch must be secondary")
      };
      let activation = Activation {
        arguments: vec!["--show".into()],
        working_directory: temporary.path().to_owned(),
        urls: vec!["lintel-demo://open/42".into()],
      };
      secondary.activate(&activation).unwrap();
      wake_rx.recv_timeout(Duration::from_secs(2)).unwrap();
      assert_eq!(primary.try_recv(), Some(activation));
      drop(primary);
      assert!(!directory.join("activation.sock").exists());
      assert!(matches!(acquire(&directory, || {}).unwrap(), InstanceRole::Primary(_)));
    }

    #[test]
    fn permissive_or_symlinked_instance_directory_is_rejected() {
      let temporary = tempfile::tempdir().unwrap();
      let directory = temporary.path().join("public");
      fs::create_dir(&directory).unwrap();
      fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
      assert!(acquire(&directory, || {}).is_err());
      let link = temporary.path().join("link");
      std::os::unix::fs::symlink(temporary.path(), &link).unwrap();
      assert!(acquire(&link, || {}).is_err());
    }

    #[test]
    fn primary_shutdown_does_not_depend_on_socket_path_existing() {
      let temporary = tempfile::tempdir().unwrap();
      let directory = temporary.path().join("instance");
      let InstanceRole::Primary(primary) = acquire(&directory, || {}).unwrap() else {
        panic!("expected primary")
      };
      fs::remove_file(directory.join("activation.sock")).unwrap();
      let before = Instant::now();
      drop(primary);
      assert!(before.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn incomplete_activation_frame_has_an_absolute_deadline() {
      let (mut client, mut server) = UnixStream::pair().unwrap();
      client.write_all(&[0]).unwrap();
      let mut buffer = [0; 4];
      let before = Instant::now();
      assert!(read_before_deadline(&mut server, &mut buffer, before + Duration::from_millis(50)).is_err());
      assert!(before.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn oversize_frame_is_rejected_without_allocating_it() {
      let (mut client, mut server) = UnixStream::pair().unwrap();
      client.write_all(&((MAX_FRAME_BYTES as u32) + 1).to_be_bytes()).unwrap();
      let (sender, _) = mpsc::sync_channel(1);
      assert!(receive_activation(&mut server, &sender, &|| {}).is_err());
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn long_application_names_use_compact_stable_distinct_transport_paths() {
    let base = std::env::temp_dir();
    let first = namespace_directory(&base, &"long-application".repeat(100)).unwrap();
    assert_eq!(
      first,
      namespace_directory(&base, &"long-application".repeat(100)).unwrap()
    );
    assert_ne!(first, namespace_directory(&base, "another-app").unwrap());
    assert!(first.file_name().unwrap().len() <= 36);
    assert!(namespace_directory(Path::new("relative"), "app").is_err());
  }

  #[test]
  fn launch_validation_bounds_payload_and_requires_absolute_cwd() {
    let mut activation = Activation {
      arguments: vec![],
      working_directory: PathBuf::from("relative"),
      urls: vec![],
    };
    assert!(activation.validate().is_err());
    activation.working_directory = std::env::temp_dir();
    activation.arguments.push("x".repeat(MAX_FRAME_BYTES));
    assert!(activation.validate().is_err());
  }
}
