use cef::ImplDragData;
use std::mem::ManuallyDrop;
use windows::Win32::Foundation::{BOOL, E_NOTIMPL, POINT, S_OK};
use windows::Win32::Foundation::{DV_E_FORMATETC, HGLOBAL};
use windows::Win32::System::Com::{
  DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA,
  IStream, STGMEDIUM, STGMEDIUM_0, StructuredStorage, TYMED_HGLOBAL,
};
use windows::Win32::System::Memory::{GLOBAL_ALLOC_FLAGS, GMEM_MOVEABLE, GMEM_ZEROINIT};
use windows::Win32::System::Ole::{CF_HDROP, CF_TEXT, CF_UNICODETEXT, ReleaseStgMedium};
use windows::Win32::UI::Shell::{DROPFILES, DragQueryFileW, HDROP};
use windows::core::{Error, HRESULT, Result, implement};
use windows_strings::{PCSTR, PCWSTR};

use super::cf_html;
use super::clipboard_formats::{get_file_contents_format, get_file_desc_format, get_html_format, get_moz_url_format};
use super::drag_data::DraggedFile;
use super::enum_format::DragEnumFormatEtc;
use super::storage::{
  dup_global_mem, get_storage_for_ansi_string, get_storage_for_bytes, get_storage_for_file_descriptor,
  get_storage_for_wide_string,
};
use super::win32_memory::{GlobalAlloc, GlobalLock, GlobalUnlock};

#[implement(windows::Win32::System::Com::IDataObject)]
pub struct DataObjectWin {
  formats: Vec<FORMATETC>,
  storage: Vec<STGMEDIUM>,
}

impl DataObjectWin {
  pub fn new(formats: Vec<FORMATETC>, storage: Vec<STGMEDIUM>) -> Self {
    Self { formats, storage }
  }

  fn lookup_format_etc(&self, target: &FORMATETC) -> Option<usize> {
    self
      .formats
      .iter()
      .position(|f| f.cfFormat == target.cfFormat && f.dwAspect == target.dwAspect && (f.tymed & target.tymed) != 0)
  }
}

fn stg_from_hglobal(h: HGLOBAL) -> STGMEDIUM {
  STGMEDIUM {
    tymed: TYMED_HGLOBAL.0 as u32,
    u: STGMEDIUM_0 { hGlobal: h },
    pUnkForRelease: ManuallyDrop::new(None::<windows::core::IUnknown>),
  }
}

fn format_etc(cf: u16) -> FORMATETC {
  FORMATETC {
    cfFormat: cf,
    ptd: std::ptr::null_mut(),
    dwAspect: DVASPECT_CONTENT.0,
    lindex: -1,
    tymed: TYMED_HGLOBAL.0 as u32,
  }
}

impl IDataObject_Impl for DataObjectWin_Impl {
  fn GetData(&self, pformatetcin: *const FORMATETC) -> Result<STGMEDIUM> {
    if pformatetcin.is_null() {
      return Err(Error::from(DV_E_FORMATETC));
    }
    let fmt = unsafe { &*pformatetcin };
    let idx = match self.lookup_format_etc(fmt) {
      Some(i) => i,
      None => return Err(Error::from(DV_E_FORMATETC)),
    };

    let stored = &self.storage[idx];
    if stored.tymed != TYMED_HGLOBAL.0 as u32 {
      return Err(Error::from(DV_E_FORMATETC));
    }

    let src = unsafe { stored.u.hGlobal };
    let dup = dup_global_mem(src)?;
    Ok(stg_from_hglobal(dup))
  }

  fn GetDataHere(&self, _pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> Result<()> {
    Err(Error::from(E_NOTIMPL))
  }

  fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
    if pformatetc.is_null() {
      return DV_E_FORMATETC;
    }
    let fmt = unsafe { &*pformatetc };
    if self.lookup_format_etc(fmt).is_some() {
      S_OK
    } else {
      DV_E_FORMATETC
    }
  }

  fn GetCanonicalFormatEtc(&self, _pformatectin: *const FORMATETC, pformatetcout: *mut FORMATETC) -> HRESULT {
    if !pformatetcout.is_null() {
      unsafe {
        (*pformatetcout).ptd = std::ptr::null_mut();
      }
    }
    E_NOTIMPL
  }

  fn SetData(&self, pformatetc: *const FORMATETC, pmedium: *const STGMEDIUM, frelease: BOOL) -> Result<()> {
    Ok(())
  }

  fn EnumFormatEtc(&self, dwdirection: u32) -> Result<IEnumFORMATETC> {
    if dwdirection != DATADIR_GET.0 as u32 {
      return Err(Error::from(DV_E_FORMATETC));
    }
    let enum_etc = DragEnumFormatEtc::new(&self.formats);
    Ok(enum_etc.into())
  }

  fn DAdvise(&self, _pformatetc: *const FORMATETC, _advf: u32, _padvsink: Option<&IAdviseSink>) -> Result<u32> {
    Err(Error::from(E_NOTIMPL))
  }

  fn DUnadvise(&self, _dwconnection: u32) -> Result<()> {
    Err(Error::from(E_NOTIMPL))
  }

  fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
    Err(Error::from(E_NOTIMPL))
  }
}

pub fn drag_data_to_data_object(drag_data: &cef::DragData) -> Result<IDataObject> {
  let moz_url_format = get_moz_url_format();
  let html_format = get_html_format();
  let file_desc_format = get_file_desc_format();
  let file_contents_format = get_file_contents_format();

  let mut formats = Vec::<FORMATETC>::new();
  let mut storage = Vec::<STGMEDIUM>::new();

  if drag_data.is_fragment() == 1 {
    let text = cef::CefString::from(&drag_data.fragment_text()).to_string();
    if !text.is_empty() {
      let h = get_storage_for_wide_string(&text)?;
      formats.push(format_etc(CF_UNICODETEXT.0));
      storage.push(stg_from_hglobal(h));
    }
  }

  if drag_data.is_link() == 1 {
    let url = cef::CefString::from(&drag_data.link_url()).to_string();
    if !url.is_empty() {
      let title = cef::CefString::from(&drag_data.link_title()).to_string();
      let mut moz = url.clone();
      moz.push('\n');
      moz.push_str(&title);
      let h = get_storage_for_wide_string(&moz)?;
      formats.push(format_etc(moz_url_format));
      storage.push(stg_from_hglobal(h));
    }
  }

  if drag_data.is_fragment() == 1 {
    let html = cef::CefString::from(&drag_data.fragment_html()).to_string();
    if !html.is_empty() {
      let base = cef::CefString::from(&drag_data.fragment_base_url()).to_string();
      let cf_html = cf_html::html_to_cf_html(&html, &base);
      let h = get_storage_for_ansi_string(&cf_html)?;
      formats.push(format_etc(html_format));
      storage.push(stg_from_hglobal(h));
    }
  }

  /*
  if let (Some(name), Some(contents)) = (&drag_data.file_name(), &drag_data.file_contents()) {
    if !contents.is_empty() {
      let h_desc = get_storage_for_file_descriptor(name)?;
      formats.push(format_etc(file_desc_format));
      storage.push(stg_from_hglobal(h_desc));

      let h_contents = get_storage_for_bytes(contents)?;
      formats.push(format_etc(file_contents_format));
      storage.push(stg_from_hglobal(h_contents));
    }
  }
  */

  /*
  if !drag_data.files().is_empty() {
    let hdrop = create_hdrop_from_files(&drag_data.files)?;
    formats.push(format_etc(CF_HDROP.0));
    storage.push(stg_from_hglobal(hdrop));
  }
  */

  let obj = DataObjectWin::new(formats, storage);
  Ok(obj.into())
}

pub fn data_object_to_drag_data(data_object: &IDataObject) -> Option<cef::DragData> {
  let moz_url_format = get_moz_url_format();
  let html_format = get_html_format();

  let drag_data = cef::drag_data_create();

  let enum_formats = unsafe { data_object.EnumFormatEtc(DATADIR_GET.0 as u32) };
  let enum_formats = match enum_formats {
    Ok(e) => e,
    Err(_) => return drag_data,
  };

  if unsafe { enum_formats.Reset() }.is_err() {
    return drag_data;
  }

  let drag_data = drag_data?;

  loop {
    let mut fetched: u32 = 0;
    let mut fmt: FORMATETC = unsafe { std::mem::zeroed() };
    let hr = unsafe {
      enum_formats.Next(
        std::slice::from_mut(&mut fmt),
        Some(std::ptr::from_mut(&mut fetched).cast()),
      )
    };
    if hr != S_OK || fetched == 0 {
      break;
    }

    let format = fmt.cfFormat;
    if !(format == CF_UNICODETEXT.0
      || format == CF_TEXT.0
      || format == moz_url_format
      || format == html_format
      || format == CF_HDROP.0)
    {
      continue;
    }
    if (fmt.tymed & TYMED_HGLOBAL.0 as u32) == 0 {
      continue;
    }

    let mut medium = match unsafe { data_object.GetData(&fmt) } {
      Ok(m) => m,
      Err(_) => continue,
    };

    unsafe {
      if medium.tymed != TYMED_HGLOBAL.0 as u32 {
        ReleaseStgMedium(&mut medium);
        continue;
      }
      let hglobal = medium.u.hGlobal;
      if hglobal.is_invalid() {
        ReleaseStgMedium(&mut medium);
        continue;
      }

      let ptr = GlobalLock(hglobal) as *const u8;
      if ptr.is_null() {
        let _ = GlobalUnlock(hglobal);
        ReleaseStgMedium(&mut medium);
        continue;
      }

      if format == CF_UNICODETEXT.0 {
        let wptr = ptr as *const u16;
        let s = PCWSTR::from_raw(wptr);
        if let Ok(text) = s.to_string()
          && cef::CefString::from(&drag_data.fragment_text()).to_string().is_empty()
        {
          drag_data.set_fragment_text(Some(&text.as_str().into()));
        }
      } else if format == CF_TEXT.0 {
        let s = PCSTR::from_raw(ptr);
        if let Ok(text) = s.to_string()
          && cef::CefString::from(&drag_data.fragment_text()).to_string().is_empty()
        {
          drag_data.set_fragment_text(Some(&text.as_str().into()));
        }
      } else if format == moz_url_format {
        let wptr = ptr as *const u16;
        let s = PCWSTR::from_raw(wptr);
        if let Ok(text) = s.to_string() {
          if let Some(pos) = text.rfind('\n') {
            drag_data.set_link_url(Some(&cef::CefString::from(&text[..pos])));
            drag_data.set_link_title(Some(&cef::CefString::from(&text[pos + 1..])));
          } else {
            drag_data.set_link_url(Some(&cef::CefString::from(text.as_str())));
          }
        }
      } else if format == html_format {
        let s = PCSTR::from_raw(ptr);
        if let Ok(cf_html) = s.to_string() {
          let (html, base_url) = cf_html::cf_html_to_html(&cf_html);
          if let Some(h) = html {
            drag_data.set_fragment_html(Some(&cef::CefString::from(h.as_str())));
          }
          if let Some(b) = base_url {
            drag_data.set_fragment_base_url(Some(&cef::CefString::from(b.as_str())));
          }
        }
      } else if format == CF_HDROP.0 {
        let hdrop = HDROP(hglobal.0);
        const MAX_FILENAME_LEN: usize = 4096;
        let file_count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
        if file_count > 0 {
          for i in 0..file_count {
            let mut buf = vec![0u16; MAX_FILENAME_LEN];
            let len = DragQueryFileW(hdrop, i, Some(&mut buf));
            if len == 0 || len as usize > buf.len() {
              continue;
            }
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            let display = path.rsplit(['\\', '/']).next().unwrap_or(&path).to_string();
            drag_data.add_file(
              Some(&cef::CefString::from(path.as_str())),
              Some(&cef::CefString::from(display.as_str())),
            );
          }
        }
      }

      let _ = GlobalUnlock(hglobal);
      ReleaseStgMedium(&mut medium);
    }
  }

  Some(drag_data)
}

fn create_hdrop_from_files(files: &[DraggedFile]) -> Result<HGLOBAL> {
  if files.is_empty() {
    return Err(Error::from(DV_E_FORMATETC));
  }

  let mut wide_paths: Vec<Vec<u16>> = Vec::with_capacity(files.len());
  let mut total_chars: usize = 0;
  for f in files {
    let mut w: Vec<u16> = f.full_path.encode_utf16().collect();
    w.push(0);
    total_chars += w.len();
    wide_paths.push(w);
  }
  total_chars += 1;

  let header_size = std::mem::size_of::<DROPFILES>();
  let bytes = header_size + total_chars * 2;

  unsafe {
    let flags = GLOBAL_ALLOC_FLAGS(GMEM_MOVEABLE.0 | GMEM_ZEROINIT.0);
    let hglobal = GlobalAlloc(flags, bytes)?;
    let ptr = GlobalLock(hglobal) as *mut u8;
    if ptr.is_null() {
      let _ = GlobalUnlock(hglobal);
      return Err(Error::from_win32());
    }

    let df = ptr as *mut DROPFILES;
    (*df).pFiles = header_size as u32;
    (*df).pt = POINT { x: 0, y: 0 };
    (*df).fNC = BOOL(0);
    (*df).fWide = BOOL(1);

    let mut cursor = ptr.add(header_size) as *mut u16;
    for path in &wide_paths {
      std::ptr::copy_nonoverlapping(path.as_ptr(), cursor, path.len());
      cursor = cursor.add(path.len());
    }
    *cursor = 0;

    GlobalUnlock(hglobal)?;
    Ok(hglobal)
  }
}
