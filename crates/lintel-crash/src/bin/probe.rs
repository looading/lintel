//! Isolated validation fixture. It aborts only when an explicit abort mode is supplied.
use std::{io::Write, path::PathBuf};

fn main() -> anyhow::Result<()> {
  let args: Vec<_> = std::env::args_os().skip(1).collect();
  #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
  if args.first().is_some_and(|arg| arg == "--lintel-snapshot-helper") {
    // This fixture impersonates an unresponsive helper for deadline and reaping tests.
    std::thread::sleep(std::time::Duration::from_secs(30));
    return Ok(());
  }
  anyhow::ensure!(
    (3..=4).contains(&args.len()),
    "usage: lintel-crash-probe <absolute-report-root> <absolute-helper> <mode> [snapshot-helper]"
  );
  let mode = args[2].to_str().unwrap_or_default();
  #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
  if mode == "--snapshots-only" {
    return snapshots(PathBuf::from(&args[0]), PathBuf::from(&args[1]));
  }
  #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
  anyhow::ensure!(
    mode != "--snapshots-abort",
    "native snapshot probe requires macOS arm64"
  );
  anyhow::ensure!(
    matches!(
      mode,
      "--abort" | "--abort-sidecar-failure" | "--clean" | "--snapshots-abort"
    ),
    "probe mode must be explicit"
  );
  let mut config = lintel_crash::CrashConfig::new(PathBuf::from(&args[0]), PathBuf::from(&args[1]));
  config.dump_timeout = std::time::Duration::from_secs(5);
  let guard = lintel_crash::CrashReporter::start(config.clone())?;
  anyhow::ensure!(
    lintel_crash::CrashReporter::start(config).is_err(),
    "a second reporter must be rejected"
  );
  println!("report={} helper={}", guard.directory().display(), guard.helper_pid());
  std::io::stdout().flush()?;
  #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
  if mode == "--snapshots-abort" {
    let helper = args.get(3).ok_or_else(|| anyhow::anyhow!("snapshot helper required"))?;
    snapshots(PathBuf::from(&args[0]), PathBuf::from(helper))?;
    println!("captured both snapshots before abort");
    std::io::stdout().flush()?;
  }
  if mode == "--abort-sidecar-failure" {
    std::fs::create_dir(guard.directory().join("capture.json"))?;
  }
  if matches!(mode, "--abort" | "--abort-sidecar-failure" | "--snapshots-abort") {
    std::process::abort();
  }
  guard.shutdown()?;
  Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn snapshots(root: PathBuf, helper: PathBuf) -> anyhow::Result<()> {
  use std::{fs, time::Duration};

  use lintel_crash::{SnapshotError, capture_snapshot, capture_snapshot_until};

  let timeout = Duration::from_secs(5);
  let before = snapshot_resources()?;
  let partial = root.join("partial.dmp");
  let hang_helper = std::env::current_exe()?;
  let error = capture_snapshot(&hang_helper, &partial, Duration::from_millis(150)).unwrap_err();
  anyhow::ensure!(
    error.downcast_ref() == Some(&SnapshotError::TimedOut),
    "unexpected timeout result: {error:#}"
  );
  anyhow::ensure!(!partial.exists(), "timed out snapshot left a partial file");
  // The first launch and bootstrap lookup can initialize process-lifetime cached Mach rights.
  let baseline = snapshot_resources()?;
  anyhow::ensure!(
    baseline.0 == before.0 && baseline.2 == before.2,
    "timeout leaked descriptors or children"
  );
  let start = std::time::Instant::now();
  let error = capture_snapshot_until(&hang_helper, &partial, timeout, || {
    start.elapsed() >= Duration::from_millis(150)
  })
  .unwrap_err();
  anyhow::ensure!(
    error.downcast_ref() == Some(&SnapshotError::Cancelled),
    "unexpected cancellation result: {error:#}"
  );
  anyhow::ensure!(!partial.exists(), "cancelled snapshot left a partial file");
  anyhow::ensure!(
    snapshot_resources()? == baseline,
    "cancellation leaked snapshot resources"
  );
  for name in ["first.dmp", "second.dmp"] {
    capture_snapshot(&helper, &root.join(name), timeout)?;
    anyhow::ensure!(
      snapshot_resources()? == baseline,
      "successful snapshot leaked resources"
    );
  }
  let before = fs::metadata(root.join("first.dmp"))?.len();
  anyhow::ensure!(
    capture_snapshot(&helper, &root.join("first.dmp"), timeout).is_err(),
    "existing dump was accepted"
  );
  anyhow::ensure!(
    fs::metadata(root.join("first.dmp"))?.len() == before,
    "existing dump was modified"
  );
  let after = snapshot_resources()?;
  anyhow::ensure!(
    after == baseline,
    "snapshot resources changed from {baseline:?} to {after:?}"
  );
  Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[allow(unsafe_code)]
fn snapshot_resources() -> anyhow::Result<(usize, u32, Vec<libc::pid_t>)> {
  use mach2::{kern_return::KERN_SUCCESS, traps::mach_task_self, vm::mach_vm_deallocate};

  unsafe extern "C" {
    fn mach_port_names(
      task: u32,
      names: *mut *mut u32,
      names_count: *mut u32,
      types: *mut *mut u32,
      types_count: *mut u32,
    ) -> i32;
  }

  let fds = std::fs::read_dir("/dev/fd")?.count();
  let (mut names, mut types) = (std::ptr::null_mut(), std::ptr::null_mut());
  let (mut names_count, mut types_count) = (0, 0);
  // SAFETY: All outputs are writable storage. On success the kernel allocates both arrays
  // in this process; deallocate them with the returned element counts before proceeding.
  unsafe {
    let task = mach_task_self();
    anyhow::ensure!(
      mach_port_names(task, &mut names, &mut names_count, &mut types, &mut types_count) == KERN_SUCCESS,
      "cannot count probe Mach ports"
    );
    for (address, count) in [(names, names_count), (types, types_count)] {
      if !address.is_null() {
        let result = mach_vm_deallocate(task, address as u64, u64::from(count) * 4);
        anyhow::ensure!(result == KERN_SUCCESS, "cannot release probe Mach port names");
      }
    }
  }

  let mut children = [0; 16];
  let capacity = size_of_val(&children) as i32;
  // SAFETY: `children` has space for `capacity` bytes and libproc takes the current PID.
  let count =
    unsafe { libc::proc_listchildpids(std::process::id().try_into()?, children.as_mut_ptr().cast(), capacity) };
  anyhow::ensure!(
    (0..children.len() as i32).contains(&count),
    "cannot count probe child processes"
  );
  let mut children = children[..count as usize].to_vec();
  children.sort_unstable();
  Ok((fds, names_count, children))
}
