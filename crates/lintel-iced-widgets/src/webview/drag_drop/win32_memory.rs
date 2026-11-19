use windows::Win32::Foundation::{BOOL, HGLOBAL};
use windows::Win32::System::Memory::GLOBAL_ALLOC_FLAGS;
use windows::core::{Error, Result};

#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
  #[link_name = "GlobalAlloc"]
  fn global_alloc_raw(uflags: GLOBAL_ALLOC_FLAGS, dwbytes: usize) -> HGLOBAL;

  #[link_name = "GlobalLock"]
  fn global_lock_raw(hmem: HGLOBAL) -> *mut core::ffi::c_void;

  #[link_name = "GlobalUnlock"]
  fn global_unlock_raw(hmem: HGLOBAL) -> BOOL;
}

#[allow(non_snake_case)]
#[inline]
pub(super) unsafe fn GlobalAlloc(uflags: GLOBAL_ALLOC_FLAGS, dwbytes: usize) -> Result<HGLOBAL> {
  let result = unsafe { global_alloc_raw(uflags, dwbytes) };
  (!result.is_invalid()).then_some(result).ok_or_else(Error::from_win32)
}

#[allow(non_snake_case)]
#[inline]
pub(super) unsafe fn GlobalLock(hmem: HGLOBAL) -> *mut core::ffi::c_void {
  unsafe { global_lock_raw(hmem) }
}

#[allow(non_snake_case)]
#[inline]
pub(super) unsafe fn GlobalUnlock(hmem: HGLOBAL) -> Result<()> {
  unsafe { global_unlock_raw(hmem) }.ok()
}
