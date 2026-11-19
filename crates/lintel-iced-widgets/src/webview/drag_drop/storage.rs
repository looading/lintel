use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::System::Memory::{GLOBAL_ALLOC_FLAGS, GMEM_FIXED, GMEM_ZEROINIT, GlobalSize};
use windows::Win32::UI::Shell::{FD_LINKUI, FILEDESCRIPTORW, FILEGROUPDESCRIPTORW};
use windows::core::{Error, Result};

use super::win32_memory::{GlobalAlloc, GlobalLock, GlobalUnlock};

pub fn get_storage_for_bytes(data: &[u8]) -> Result<HGLOBAL> {
  unsafe {
    let flags = GLOBAL_ALLOC_FLAGS(GMEM_FIXED.0 | GMEM_ZEROINIT.0);
    let handle = GlobalAlloc(flags, data.len())?;
    let ptr = GlobalLock(handle) as *mut u8;
    if ptr.is_null() {
      let _ = GlobalUnlock(handle);
      return Err(Error::from_win32());
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
    GlobalUnlock(handle)?;
    Ok(handle)
  }
}

pub fn get_storage_for_utf16(wide: &[u16]) -> Result<HGLOBAL> {
  unsafe {
    let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
    get_storage_for_bytes(bytes)
  }
}

pub fn get_storage_for_ansi_string(s: &str) -> Result<HGLOBAL> {
  let mut bytes = s.as_bytes().to_vec();
  bytes.push(0);
  get_storage_for_bytes(&bytes)
}

pub fn get_storage_for_wide_string(s: &str) -> Result<HGLOBAL> {
  let mut wide: Vec<u16> = s.encode_utf16().collect();
  wide.push(0);
  get_storage_for_utf16(&wide)
}

pub fn get_storage_for_file_descriptor(file_name: &str) -> Result<HGLOBAL> {
  unsafe {
    let size = std::mem::size_of::<FILEGROUPDESCRIPTORW>();
    let flags = GLOBAL_ALLOC_FLAGS(GMEM_FIXED.0 | GMEM_ZEROINIT.0);
    let handle = GlobalAlloc(flags, size)?;
    let ptr = GlobalLock(handle) as *mut FILEGROUPDESCRIPTORW;
    if ptr.is_null() {
      let _ = GlobalUnlock(handle);
      return Err(Error::from_win32());
    }

    (*ptr).cItems = 1;
    let desc: &mut FILEDESCRIPTORW = &mut (*ptr).fgd[0];

    let mut name_buf: [u16; 260] = [0; 260];
    let mut name_str: Vec<_> = file_name.encode_utf16().collect();
    name_str.truncate(name_buf.len() - 1);
    name_buf[0..name_str.len()].copy_from_slice(&name_str);
    *desc = FILEDESCRIPTORW {
      dwFlags: (FD_LINKUI.0) as u32,
      cFileName: name_buf,
      ..FILEDESCRIPTORW::default()
    };

    GlobalUnlock(handle)?;
    Ok(handle)
  }
}

pub fn dup_global_mem(src: HGLOBAL) -> Result<HGLOBAL> {
  unsafe {
    let len = GlobalSize(src);
    let src_ptr = GlobalLock(src) as *const u8;
    if src_ptr.is_null() {
      let _ = GlobalUnlock(src);
      return Err(Error::from_win32());
    }
    let flags = GLOBAL_ALLOC_FLAGS(GMEM_FIXED.0 | GMEM_ZEROINIT.0);
    let dst = GlobalAlloc(flags, len)?;
    let dst_ptr = GlobalLock(dst) as *mut u8;
    if dst_ptr.is_null() {
      let _ = GlobalUnlock(dst);
      let _ = GlobalUnlock(src);
      return Err(Error::from_win32());
    }
    std::ptr::copy_nonoverlapping(src_ptr, dst_ptr, len);
    GlobalUnlock(src)?;
    GlobalUnlock(dst)?;
    Ok(dst)
  }
}
