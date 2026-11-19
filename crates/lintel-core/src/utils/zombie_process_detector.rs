use anyhow::{Context, Result};
use std::collections::HashMap;
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{debug, error, info, warn};

static ZOMBIE_PROCESS_COUNT: AtomicUsize = AtomicUsize::new(0);

/// 获取检测到的僵尸进程数量
pub fn get_zombie_process_count() -> usize {
  ZOMBIE_PROCESS_COUNT.load(Ordering::Relaxed)
}

/// 设置检测到的僵尸进程数量
fn set_zombie_process_count(count: usize) {
  ZOMBIE_PROCESS_COUNT.store(count, Ordering::Relaxed)
}

#[cfg(target_os = "windows")]
mod platform {
  use super::*;
  use std::os::windows::ffi::OsStringExt;
  use windows::Win32::System::Diagnostics::ToolHelp::*;
  use windows::Win32::UI::WindowsAndMessaging::{
    IDCANCEL, IDOK, MB_DEFBUTTON2, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_OKCANCEL, MessageBoxW,
  };

  pub fn find_zombie_processes() -> Result<Vec<u32>> {
    let current_pid = process::id();
    info!(current_pid, "Starting zombie process detection");

    // Check if test mode is enabled
    let test_mode = std::env::var("LINTEL_ZOMBIE_TEST").is_ok();
    if test_mode {
      info!("=== TEST MODE ENABLED ===");
      info!("In test mode, all processes with the same name will be treated as zombie processes");
    }

    let current_exe = std::env::current_exe()
      .context("cannot get current exe path")?
      .canonicalize()?;
    let current_exe_name = current_exe
      .file_name()
      .context("cannot get exe name")?
      .to_string_lossy()
      .to_lowercase();

    debug!(current_exe_name, "Current executable name");

    let process_list = get_all_processes()?;
    info!(process_count = process_list.len(), "Got all processes");

    let mut zombie_pids = Vec::new();

    for (&pid, (parent_pid, exe_name)) in process_list.iter() {
      if pid == current_pid {
        debug!(pid, "Skipping current process");
        continue;
      }

      let exe_name_lower = exe_name.to_lowercase();
      if exe_name_lower == current_exe_name {
        debug!(
          pid,
          exe_name, parent_pid, "Found Lintel process, checking parent process"
        );

        if test_mode || !is_valid_parent(*parent_pid, &process_list) {
          if test_mode {
            warn!(pid, "Test mode: process marked as zombie");
          } else {
            warn!(pid, "Process is a zombie, adding to cleanup list");
          }
          zombie_pids.push(pid);
        } else {
          debug!(pid, "Process has valid parent, not a zombie");
        }
      }
    }

    info!(zombie_count = zombie_pids.len(), "Zombie process detection complete");
    Ok(zombie_pids)
  }

  fn get_all_processes() -> Result<HashMap<u32, (u32, String)>> {
    let mut processes = HashMap::new();

    debug!("Starting to enumerate system processes");
    unsafe {
      let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
        .map_err(|e| anyhow::anyhow!("failed to create snapshot: {:?}", e))?;
      let snapshot = windows::core::Owned::new(snapshot);

      let mut entry: PROCESSENTRY32W = std::mem::zeroed();
      entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

      if Process32FirstW(*snapshot, &mut entry).is_err() {
        return Err(anyhow::anyhow!("failed to get first process"));
      }

      loop {
        let pid = entry.th32ProcessID;
        let parent_pid = entry.th32ParentProcessID;
        // Find null terminator position, only convert valid part
        let null_pos = entry
          .szExeFile
          .iter()
          .position(|&c| c == 0)
          .unwrap_or(entry.szExeFile.len());
        let exe_name = std::ffi::OsString::from_wide(&entry.szExeFile[..null_pos])
          .into_string()
          .unwrap_or_else(|_| "Unknown".into());

        processes.insert(pid, (parent_pid, exe_name));

        if Process32NextW(*snapshot, &mut entry).is_err() {
          break;
        }
      }
    }
    debug!("Enumerated system processes: {:?}", processes);

    Ok(processes)
  }

  fn is_valid_parent(parent_pid: u32, process_list: &HashMap<u32, (u32, String)>) -> bool {
    debug!(parent_pid, "Checking if parent process is valid");

    if parent_pid == 0 {
      debug!(parent_pid, "Parent process ID is 0, invalid");
      return false;
    }

    if let Some((_, parent_name)) = process_list.get(&parent_pid) {
      let parent_name_lower = parent_name.to_lowercase();
      debug!(parent_pid, parent_name = parent_name_lower, "Got parent process name");

      let is_valid = parent_name_lower == "explorer.exe"
        || parent_name_lower == "lintel.exe"
        || parent_name_lower == "lintel_helper.exe";

      debug!(parent_pid, is_valid, "Parent process validity check result");
      return is_valid;
    }

    debug!(parent_pid, "Parent process not found, considered invalid");
    false
  }

  pub fn terminate_process(pid: u32) -> Result<()> {
    info!(pid, "Starting to terminate zombie process");

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    unsafe {
      let handle = OpenProcess(PROCESS_TERMINATE, false, pid).map_err(|e| {
        error!(pid, error = ?e, "Failed to open process handle");
        anyhow::anyhow!("failed to open process: {:?}", e)
      })?;
      let handle = windows::core::Owned::new(handle);

      TerminateProcess(*handle, 0).map_err(|e| {
        error!(pid, error = ?e, "Failed to terminate process");
        anyhow::anyhow!("failed to terminate process: {:?}", e)
      })?;
    }

    info!(pid, "Successfully terminated zombie process");
    Ok(())
  }

  pub fn show_zombie_warning(zombie_count: usize) -> bool {
    info!(zombie_count, "Showing zombie process warning dialog");

    use windows::core::PCWSTR;

    let message = format!(
      "检测到 {} 个未终止进程，可能导致应用启动异常。是否立即清理？",
      zombie_count
    );

    let caption = "检测到后台残留进程";

    let result = unsafe {
      MessageBoxW(
        None,
        PCWSTR::from_raw(widestring::WideCString::from_str(message).unwrap().as_ptr()),
        PCWSTR::from_raw(widestring::WideCString::from_str(caption).unwrap().as_ptr()),
        MB_ICONWARNING | MB_OKCANCEL | MB_DEFBUTTON2,
      )
    };

    let user_choice = result == IDOK;
    info!(user_choice, "User choice");

    user_choice
  }

  pub fn show_cleanup_result(success_count: usize, failed_pids: &[u32]) {
    info!(
      success_count,
      failed_count = failed_pids.len(),
      "Showing cleanup result dialog"
    );

    use windows::core::PCWSTR;

    let message = if failed_pids.is_empty() {
      format!("成功清理了 {} 个未终止进程！", success_count)
    } else {
      format!(
        "清理完成：\n- 成功清理: {} 个\n- 清理失败: {} 个 (PID: {:?})\n\n请手动从任务管理器中结束失败的进程，然后继续使用应用。",
        success_count,
        failed_pids.len(),
        failed_pids
      )
    };

    let caption = "清理结果";

    unsafe {
      MessageBoxW(
        None,
        PCWSTR::from_raw(widestring::WideCString::from_str(message).unwrap().as_ptr()),
        PCWSTR::from_raw(widestring::WideCString::from_str(caption).unwrap().as_ptr()),
        MB_ICONINFORMATION | MB_OK,
      );
    }
  }
}

#[cfg(target_os = "macos")]
mod platform {
  use super::*;
  use libc::{pid_t, proc_listchildpids, proc_name};

  pub fn find_zombie_processes() -> Result<Vec<u32>> {
    // macOS 平台暂时不启用僵尸进程检测
    // let current_pid = process::id();
    // info!(current_pid, "开始检测僵尸进程");

    // // 检查是否启用测试模式
    // let test_mode = std::env::var("LINTEL_ZOMBIE_TEST").is_ok();
    // if test_mode {
    //   info!("=== 测试模式已启用 ===");
    //   info!("在测试模式下，所有同名进程都会被当作僵尸进程");
    // }

    // let current_exe = std::env::current_exe()
    //   .context("cannot get current exe path")?
    //   .canonicalize()?;
    // let current_exe_name = current_exe
    //   .file_name()
    //   .context("cannot get exe name")?
    //   .to_string_lossy()
    //   .to_lowercase();

    // debug!(current_exe_name, "当前可执行文件名称");

    // let process_list = get_all_processes()?;
    // info!(process_count = process_list.len(), "获取到所有进程");

    // let mut zombie_pids = Vec::new();

    // for (&pid, (parent_pid, exe_name)) in process_list.iter() {
    //   if pid == current_pid {
    //     debug!(pid, "跳过当前进程");
    //     continue;
    //   }

    //   let exe_name_lower = exe_name.to_lowercase();
    //   if exe_name_lower.contains("lintel") {
    //     debug!(pid, exe_name, parent_pid, "发现Lintel进程，开始判断父进程");

    //     if test_mode || !is_valid_parent(*parent_pid, &process_list) {
    //       if test_mode {
    //         warn!(pid, "测试模式：该进程被标记为僵尸进程");
    //       } else {
    //         warn!(pid, "该进程为僵尸进程，加入清理列表");
    //       }
    //       zombie_pids.push(pid);
    //     } else {
    //       debug!(pid, "该进程父进程有效，非僵尸进程");
    //     }
    //   }
    // }

    // info!(zombie_count = zombie_pids.len(), "僵尸进程检测完成");
    // Ok(zombie_pids)
    info!("macOS 平台暂不检测僵尸进程");
    Ok(Vec::new())
  }

  fn get_all_processes() -> Result<HashMap<u32, (u32, String)>> {
    let mut processes = HashMap::new();

    debug!("Starting to enumerate system processes");
    let mut pids: Vec<pid_t> = Vec::with_capacity(1024);
    pids.resize(1024, 0);

    let count = unsafe {
      proc_listchildpids(
        1,
        pids.as_mut_ptr().cast(),
        (pids.len() * std::mem::size_of::<pid_t>()) as libc::c_int,
      )
    };
    debug!(count, "Got process count");

    for &pid in pids.iter().take(count as usize) {
      if pid != 0 {
        let (parent_pid, name) = get_process_info(pid);
        processes.insert(pid as u32, (parent_pid as u32, name));
      }
    }

    Ok(processes)
  }

  fn get_process_info(pid: pid_t) -> (pid_t, String) {
    const MAX_LEN: usize = 64;
    let mut name = [0u8; MAX_LEN];
    let res = unsafe { proc_name(pid, name.as_mut_ptr().cast(), MAX_LEN as _) };
    let name_str = if res > 0 {
      String::from_utf8_lossy(&name[..res as usize]).into_owned()
    } else {
      String::from("Unknown")
    };

    let mut parent_pid: pid_t = 0;
    let path = format!("/proc/{}/status", pid);
    if let Ok(content) = std::fs::read_to_string(&path) {
      for line in content.lines() {
        if line.starts_with("PPid:") {
          if let Some(p) = line.split_whitespace().nth(1) {
            parent_pid = p.parse().unwrap_or(0);
          }
          break;
        }
      }
    }

    (parent_pid, name_str)
  }

  fn is_valid_parent(parent_pid: u32, process_list: &HashMap<u32, (u32, String)>) -> bool {
    debug!(parent_pid, "Checking if parent process is valid");

    if parent_pid == 0 {
      debug!(parent_pid, "Parent process ID is 0, invalid");
      return false;
    }

    if let Some((_, parent_name)) = process_list.get(&parent_pid) {
      let parent_name_lower = parent_name.to_lowercase();
      debug!(parent_pid, parent_name = parent_name_lower, "Got parent process name");

      let is_valid = parent_name_lower == "launchd" || parent_name_lower.starts_with("lintel");

      debug!(parent_pid, is_valid, "Parent process validity check result");
      return is_valid;
    }

    debug!(parent_pid, "Parent process not found, considered invalid");
    false
  }

  pub fn terminate_process(pid: u32) -> Result<()> {
    info!(pid, "Starting to terminate zombie process");

    unsafe {
      if libc::kill(pid as pid_t, libc::SIGTERM) != 0 {
        let e = std::io::Error::last_os_error();
        error!(pid, error = ?e, "Failed to terminate process");
        return Err(anyhow::anyhow!("failed to terminate process: {}", e));
      }
    }

    info!(pid, "Successfully terminated zombie process");
    Ok(())
  }

  pub fn show_zombie_warning(zombie_count: usize) -> bool {
    info!(zombie_count, "Showing zombie process warning dialog");

    let message = format!(
      "检测到 {} 个Lintel僵尸进程，可能导致应用启动异常。是否立即清理？",
      zombie_count
    );

    let output = match std::process::Command::new("osascript")
      .arg("-e")
      .arg(format!(
        "display dialog \"{}\" with title \"检测到僵尸进程\" buttons {{\"取消\", \"清理\"}} default button \"取消\" with icon caution",
        message
      ))
      .output()
    {
      Ok(output) => output,
      Err(err) => {
        error!(error = ?err, "failed to show dialog");
        return false;
      }
    };

    let user_choice = String::from_utf8_lossy(&output.stdout).contains("清理");
    info!(user_choice, "User choice");

    user_choice
  }

  pub fn show_cleanup_result(success_count: usize, failed_pids: &[u32]) {
    info!(
      success_count,
      failed_count = failed_pids.len(),
      "Showing cleanup result dialog"
    );

    let message = if failed_pids.is_empty() {
      format!("成功清理了 {} 个僵尸进程！", success_count)
    } else {
      format!(
        "清理完成：\n- 成功清理: {} 个\n- 清理失败: {} 个 (PID: {:?})\n\n请手动从任务管理器中结束失败的进程，然后继续使用应用。",
        success_count,
        failed_pids.len(),
        failed_pids
      )
    };

    let _ = std::process::Command::new("osascript")
      .arg("-e")
      .arg(format!(
        "display dialog \"{}\" with title \"清理结果\" buttons {{\"确定\"}} default button \"确定\"",
        message
      ))
      .output();
  }
}

pub fn detect_and_handle_zombie_processes() -> Result<()> {
  println!("detect_and_handle_zombie_processes");
  info!("=== Zombie Process Detection Process Started ===");

  let zombie_pids = platform::find_zombie_processes()?;

  // 记录僵尸进程数量，用于后续上报
  set_zombie_process_count(zombie_pids.len());

  if zombie_pids.is_empty() {
    info!("No zombie processes detected, continuing app startup");
    return Ok(());
  }

  info!(zombie_count = zombie_pids.len(), zombie_pids = ?zombie_pids, "Zombie processes detected");

  if platform::show_zombie_warning(zombie_pids.len()) {
    info!("User chose to clean up zombie processes");

    let mut success_count = 0;
    let mut failed_pids = Vec::new();

    for pid in zombie_pids {
      match platform::terminate_process(pid) {
        Ok(_) => success_count += 1,
        Err(err) => {
          error!(pid, error = ?err, "Failed to clean up zombie process");
          failed_pids.push(pid);
        }
      }
    }

    info!(
      success_count,
      failed_count = failed_pids.len(),
      "Zombie process cleanup complete"
    );
    platform::show_cleanup_result(success_count, &failed_pids);
  } else {
    info!("User canceled zombie process cleanup, continuing app startup");
  }

  info!("=== Zombie Process Detection Process Complete ===");
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_find_zombie_processes() -> Result<()> {
    let pids = platform::find_zombie_processes()?;
    info!(?pids, "found zombie processes");
    Ok(())
  }
}
