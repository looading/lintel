//! `LimitCapacityLogFileManager`提供了以下功能
//! 1、向指定的文件中追加内容
//! 2、一次性读取文件中所有内容，然后清空
//! 3、上述两种操作都不会重新打开或关闭fd
//! 4、可指定文件容量，超过容量的不会写入
//! 使用追加内容需要`use std::io::Write`
use std::path::PathBuf;

use anyhow::anyhow;

use crate::unit;

#[derive(Debug)]
pub struct LimitCapacityLogFileManager {
  pub file: std::fs::File,
  pub cap: u64,
}

impl LimitCapacityLogFileManager {
  pub fn new(log_path: PathBuf, cap: Option<u64>) -> anyhow::Result<LimitCapacityLogFileManager> {
    Ok(Self {
      file: std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(log_path)?,
      cap: cap.unwrap_or(50 * unit::MB),
    })
  }

  #[allow(clippy::disallowed_methods)]
  fn read_to_end_and_clear_impl(&mut self, buf: &mut Vec<u8>) -> anyhow::Result<()> {
    use std::io::{Read, Seek};
    // 从文件起始开始读取
    self.file.seek(std::io::SeekFrom::Start(0))?;
    self.file.by_ref().take(self.cap).read_to_end(buf)?;
    self.file.set_len(0)?;
    self.file.seek(std::io::SeekFrom::Start(0))?;
    Ok(())
  }

  /// 为了防止oom设置了读取文件的上限值
  #[allow(clippy::ptr_arg)]
  pub fn read_to_end_and_clear(&mut self, buf: &mut Vec<u8>) -> anyhow::Result<()> {
    self
      .read_to_end_and_clear_impl(buf)
      .map_err(|err| anyhow!("read_to_end_and_clear_impl_err={:?}", err))
  }

  // 获取tea落盘文件的大小，单位是字节
  pub fn get_file_size(&self) -> u64 {
    self.file.metadata().map(|meta| meta.len()).unwrap_or(0)
  }
}

impl std::io::Write for LimitCapacityLogFileManager {
  fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
    use std::io::Seek;
    // 此时文件大小最大可能为 2 * self.cap
    if self.get_file_size() > self.cap || buf.len() as u64 > self.cap {
      return Err(std::io::Error::other("log file full"));
    }
    // 从末尾开始追加写入
    self.file.seek(std::io::SeekFrom::End(0))?;
    self.file.write(buf)
  }
  fn flush(&mut self) -> std::io::Result<()> {
    self.file.flush()
  }
}

#[cfg(test)]
mod tests {

  use super::LimitCapacityLogFileManager;
  use std::{fs, io::Write, os, path};
  #[test]
  fn test_read_and_write() {
    let tmp_dir = std::env::temp_dir();
    let tmp_folder = tmp_dir.to_str().unwrap();
    let tmp_folder = path::Path::new(tmp_folder);
    let log_path = tmp_folder.join("tea_records_v2.log");

    // 先清空log文件
    {
      let mut log_file_manager = LimitCapacityLogFileManager::new(log_path.clone(), None).unwrap();
      let mut buf = Vec::new();
      log_file_manager.read_to_end_and_clear(&mut buf).unwrap();
    }
    let part_3 = "11\n12\n13\n14\n15\n";
    {
      let mut log_file_manager = LimitCapacityLogFileManager::new(log_path.clone(), None).unwrap();
      let part_1 = "1\n2\n3\n4\n5\n";
      log_file_manager.write_all(part_1.as_bytes()).unwrap();
      let part_2 = "6\n7\n8\n9\n10\n";
      log_file_manager.write_all(part_2.as_bytes()).unwrap();
      let should_be = format!("{}{}", part_1, part_2);
      let mut buf = Vec::new();
      log_file_manager.read_to_end_and_clear(&mut buf).unwrap();
      let buf: String = String::from_utf8(buf).unwrap();
      assert_eq!(buf, should_be);

      log_file_manager.write_all(part_3.as_bytes()).unwrap();
    }
    // 此时log中预期有剩余
    {
      let mut log_file_manager: LimitCapacityLogFileManager =
        LimitCapacityLogFileManager::new(log_path.clone(), None).unwrap();
      let part_4 = "16\n17\n18\n19\n20\n";
      log_file_manager.write_all(part_4.as_bytes()).unwrap();
      let mut buf = Vec::new();
      log_file_manager.read_to_end_and_clear(&mut buf).unwrap();
      let buf = String::from_utf8(buf).unwrap();
      let should_be = format!("{}{}", part_3, part_4);
      assert_eq!(buf, should_be);
    }
  }
}
