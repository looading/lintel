use std::{
  process,
  thread::{self, ThreadId},
};

//use sysinfo::{DiskUsage, Pid, Process, RefreshKind, System};

/// ProcessInfo is the information of the current process.
/// todo: should 优化
#[derive(Debug, Clone)]
pub struct ProcessInfo {
  // sys: Arc<Mutex<System>>,
  pub main_process_id: u32,
  pub main_thread_id: ThreadId,
  pub process_id: u32,
  pub thread_id: ThreadId,
  // pub process_exe_path: String,
  pub process_type: String,
  // create_time: u64, // timestamp s
}

impl ProcessInfo {
  pub fn new(main_process_id: u32, main_thread_id: ThreadId) -> Self {
    // let mut sys = System::new_with_specifics(RefreshKind::everything());
    let current_process_id = process::id();
    let current_thread_id = thread::current().id();
    // sys.refresh_all();
    // let process_info: &Process = sys.process(Pid::from_u32(current_process_id)).unwrap();
    // let create_time = process_info.start_time();
    Self {
      main_process_id,
      main_thread_id,
      process_id: current_process_id,
      thread_id: current_thread_id,
      // process_exe_path: process_info.exe().unwrap().display().to_string(),
      process_type: "".to_string(),
      // sys: Arc::new(Mutex::new(sys)),
      // create_time,
    }
  }
}

impl ProcessInfo {
  // fn with_sys<F, R>(&self, cb: F) -> R
  // where
  //   F: FnOnce(&mut System) -> R,
  // {
  //   let mut sys = self.sys.lock();
  //   sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
  //   cb(&mut sys)
  // }

  // fn with_current_process<F, R>(&self, cb: F) -> R
  // where
  //   F: FnOnce(&Process) -> R,
  // {
  //   let mut sys = self.sys.lock();
  //   sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
  //   let info = sys.process(Pid::from_u32(self.process_id)).unwrap();
  //   cb(info)
  // }

  // /// Refresn all info
  // pub fn refresh(&self) {
  //   self.with_sys(|s| s.refresh_all());
  // }

  // /// Returns the total CPU usage (in %) of current process.
  // pub fn get_cpu_usage(&self) -> f32 {
  //   self.with_current_process(|p| p.cpu_usage())
  // }

  // /// Returns the total CPU usage between 0% and 100% of current process.
  // pub fn get_cpu_usage_100(&self) -> f32 {
  //   self.with_current_process(|p| p.cpu_usage() / num_cpus::get() as f32)
  // }

  // /// Returns the disk usage between 0% and 100% of current process.
  // pub fn get_disk_usage(&self) -> DiskUsage {
  //   self.with_current_process(|p| p.disk_usage())
  // }

  // /// Returns the memory usage (in bytes) of current process.
  // pub fn get_memory_usage(&self) -> u64 {
  //   self.with_current_process(|p| p.memory())
  // }

  pub fn is_in_app_main_thread(&self) -> bool {
    thread::current().id() == self.main_thread_id && process::id() == self.main_process_id
  }

  // /// Returns crete time (in timestamp s) of current process.
  // pub fn get_create_time_s(&self) -> i64 {
  //   self.create_time as i64
  // }

  // /// Returns crete time (in timestamp ms) of current process.
  // pub fn get_create_time_ms(&self) -> i64 {
  //   (self.create_time * 1000) as i64
  // }

  // /// Returns elapsed time (in timestamp s) of current process.
  // pub fn elapsed_s(&self) -> i64 {
  //   self.with_current_process(|p| p.run_time()) as i64
  // }

  // /// Returns elapsed time (in timestamp ms) of current process.
  // pub fn elapsed_ms(&self) -> i64 {
  //   (self.with_current_process(|p| p.run_time()) * 1000) as i64
  // }

  // /// Returns the total CPU usage (in %) of system.
  // pub fn get_sys_cpu_usage(&self) -> f32 {
  //   self.with_sys(|s: &mut System| s.global_cpu_usage())
  // }

  // /// Returns the total CPU usage between 0% and 100% of system.
  // pub fn get_sys_cpu_usage_100(&self) -> f32 {
  //   self.with_sys(|s| s.global_cpu_usage() / num_cpus::get() as f32)
  // }

  // /// Returns the memory usage (in bytes) of system.
  // pub fn get_sys_memory_usage(&self) -> u64 {
  //   self.with_sys(|s| s.used_memory())
  // }

  // /// Returns the free memory (in bytes) of system.
  // pub fn get_sys_free_mem(&self) -> u64 {
  //   self.with_sys(|s| s.free_memory())
  // }

  // /// Returns the total memory (in bytes) of system.
  // pub fn get_sys_total_mem(&self) -> u64 {
  //   self.with_sys(|s| s.total_memory())
  // }

  // /// Returns the memory level of system.
  // pub fn get_sys_mem_level(&self) -> MemLevel {
  //   let mem = self.get_sys_total_mem();
  //   match mem {
  //     0..4 => MemLevel::Low,
  //     4..8 => MemLevel::Low,
  //     8..16 => MemLevel::Middle,
  //     _ => MemLevel::High,
  //   }
  // }
}

#[derive(Clone, Debug, Default)]
pub enum MemLevel {
  #[default]
  Low = 0,
  Middle = 1,
  High = 2,
}
