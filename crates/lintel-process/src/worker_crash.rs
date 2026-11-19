use std::{ffi::OsStr, io, ops::Deref, path::Path};

use lintel_crash::{CrashConfig, CrashReporter, ServiceCrashContext};
use mojo_service::{SUPERVISOR_ATTEMPT_ENV, watch_supervisor};

use crate::{ProcessWorker, TrustedRuntimeLibrary};

/// 当前服务进程及其独立报告 helper 的联合 owner，不改变旧 ProcessWorker 的自动 trait。
/// worker 的所有字段先释放，之后才撤销崩溃 hook 并回收报告 helper。
/// 父进程 EOF 的 SDK exit 路径不执行 Drop；helper 通过自己的控制管道 EOF 结束。
pub struct CrashReportingWorker {
  worker: ProcessWorker,
  reporter: CrashReporter,
}

impl CrashReportingWorker {
  pub fn crash_report_directory(&self) -> &Path {
    self.reporter.directory()
  }

  pub fn crash_helper_pid(&self) -> u32 {
    self.reporter.helper_pid()
  }
}

impl Deref for CrashReportingWorker {
  type Target = ProcessWorker;

  fn deref(&self) -> &Self::Target {
    &self.worker
  }
}

pub(crate) fn connect(
  library: TrustedRuntimeLibrary,
  service_identity: String,
  config: CrashConfig,
) -> io::Result<CrashReportingWorker> {
  let attempt = parse_attempt(std::env::var_os(SUPERVISOR_ATTEMPT_ENV).as_deref())?;
  let context = ServiceCrashContext::new(service_identity, attempt)
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
  // 在任何新 helper 或 Runtime::load 前保留 SDK 父进程监控及 stdin 归属。
  watch_supervisor()?;
  let reporter = CrashReporter::start_service(config, context).map_err(io::Error::other)?;
  // 既有入口的 watcher 是幂等的；若后续连接失败，局部 reporter 自动清理。
  let worker = ProcessWorker::connect(library)?;
  Ok(CrashReportingWorker { worker, reporter })
}

fn parse_attempt(value: Option<&OsStr>) -> io::Result<u64> {
  value
    .and_then(OsStr::to_str)
    .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
    .and_then(|value| value.parse::<u64>().ok())
    .filter(|attempt| *attempt > 0)
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing or invalid supervisor attempt"))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn attempt_requires_a_positive_decimal_u64_without_reflecting_input() {
    assert_eq!(parse_attempt(Some(OsStr::new("1"))).unwrap(), 1);
    assert_eq!(parse_attempt(Some(OsStr::new("01"))).unwrap(), 1);
    assert_eq!(
      parse_attempt(Some(OsStr::new("18446744073709551615"))).unwrap(),
      u64::MAX
    );
    for value in [
      "",
      "0",
      "-1",
      "+1",
      " 1",
      "1 ",
      "1\n",
      "1.0",
      "1e2",
      "18446744073709551616",
      "service-endpoint=private",
    ] {
      let error = parse_attempt(Some(OsStr::new(value))).unwrap_err();
      assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
      assert_eq!(error.to_string(), "missing or invalid supervisor attempt");
    }
    let missing = parse_attempt(None).unwrap_err();
    assert_eq!(missing.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(missing.to_string(), "missing or invalid supervisor attempt");
  }

  #[cfg(unix)]
  #[test]
  fn non_utf8_attempt_is_rejected_without_changing_process_environment() {
    use std::os::unix::ffi::OsStrExt;

    let error = parse_attempt(Some(OsStr::from_bytes(&[b'1', 0xff]))).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(error.to_string(), "missing or invalid supervisor attempt");
  }

  #[test]
  fn crash_guard_exposes_only_borrowed_worker_and_report_observation() {
    fn require_worker_deref<T: Deref<Target = ProcessWorker>>() {}
    require_worker_deref::<CrashReportingWorker>();
    let _: fn(&CrashReportingWorker) -> &Path = CrashReportingWorker::crash_report_directory;
    let _: fn(&CrashReportingWorker) -> u32 = CrashReportingWorker::crash_helper_pid;
    let _: fn(TrustedRuntimeLibrary, String, CrashConfig) -> io::Result<CrashReportingWorker> =
      ProcessWorker::connect_with_crash_reporting;
  }
}
