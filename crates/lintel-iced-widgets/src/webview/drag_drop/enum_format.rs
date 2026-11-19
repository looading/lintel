use std::cell::Cell;
use windows::Win32::Foundation::{S_FALSE, S_OK};
use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree, FORMATETC, IEnumFORMATETC};
use windows::core::{HRESULT, Result, implement};

#[implement(windows::Win32::System::Com::IEnumFORMATETC)]
pub struct DragEnumFormatEtc {
  formats: Vec<FORMATETC>,
  index: Cell<usize>,
}

impl DragEnumFormatEtc {
  pub fn new(formats: &[FORMATETC]) -> Self {
    Self {
      formats: formats.to_vec(),
      index: Cell::new(0),
    }
  }
}

impl Drop for DragEnumFormatEtc {
  fn drop(&mut self) {
    unsafe {
      for fmt in &mut self.formats {
        if !fmt.ptd.is_null() {
          CoTaskMemFree(Some(fmt.ptd as *const _));
          fmt.ptd = std::ptr::null_mut();
        }
      }
    }
  }
}

impl windows::Win32::System::Com::IEnumFORMATETC_Impl for DragEnumFormatEtc_Impl {
  fn Next(&self, celt: u32, rgelt: *mut FORMATETC, pceltfetched: *mut u32) -> HRESULT {
    if rgelt.is_null() {
      return HRESULT(0x80004003u32 as i32);
    }

    let mut copied: u32 = 0;
    let mut index = self.index.get();
    let len = self.formats.len();
    let max = celt.min(u32::try_from(len.saturating_sub(index)).unwrap_or(0));

    unsafe {
      while copied < max && index < len {
        let dest = rgelt.add(copied as usize);
        let src = &self.formats[index];
        deep_copy_format_etc(&mut *dest, src);
        copied += 1;
        index += 1;
      }
      if !pceltfetched.is_null() {
        *pceltfetched = copied;
      }
    }

    self.index.set(index);

    if copied == celt { S_OK } else { S_FALSE }
  }

  fn Skip(&self, celt: u32) -> Result<()> {
    let mut index = self.index.get();
    index = index.saturating_add(celt as usize);
    self.index.set(index.min(self.formats.len()));
    Ok(())
  }

  fn Reset(&self) -> Result<()> {
    self.index.set(0);
    Ok(())
  }

  fn Clone(&self) -> Result<IEnumFORMATETC> {
    let cloned = DragEnumFormatEtc::new(&self.formats);
    cloned.index.set(self.index.get());
    Ok(cloned.into())
  }
}

fn deep_copy_format_etc(dest: &mut FORMATETC, source: &FORMATETC) {
  *dest = *source;
  if !source.ptd.is_null() {
    unsafe {
      let size = (*source.ptd).tdSize as usize;
      let new_ptd = CoTaskMemAlloc(size);
      if new_ptd.is_null() {
        dest.ptd = std::ptr::null_mut();
      } else {
        std::ptr::copy_nonoverlapping(source.ptd.cast::<u8>(), new_ptd.cast(), size);
        dest.ptd = new_ptd.cast();
      }
    }
  }
}
