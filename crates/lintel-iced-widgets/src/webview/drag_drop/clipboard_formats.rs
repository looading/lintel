use std::sync::OnceLock;
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::core::PCWSTR;
use windows_strings::w;

pub fn get_moz_url_format() -> u16 {
  static FMT: OnceLock<u16> = OnceLock::new();
  *FMT.get_or_init(|| unsafe { RegisterClipboardFormatW(PCWSTR(w!("text/x-moz-url").as_ptr())) as u16 })
}

pub fn get_html_format() -> u16 {
  static FMT: OnceLock<u16> = OnceLock::new();
  *FMT.get_or_init(|| unsafe { RegisterClipboardFormatW(PCWSTR(w!("HTML Format").as_ptr())) as u16 })
}

pub fn get_file_desc_format() -> u16 {
  static FMT: OnceLock<u16> = OnceLock::new();
  *FMT.get_or_init(|| unsafe { RegisterClipboardFormatW(PCWSTR(w!("FileGroupDescriptorW").as_ptr())) as u16 })
}

pub fn get_file_contents_format() -> u16 {
  static FMT: OnceLock<u16> = OnceLock::new();
  *FMT.get_or_init(|| unsafe { RegisterClipboardFormatW(PCWSTR(w!("FileContents").as_ptr())) as u16 })
}
