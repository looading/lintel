//! Small audited platform boundary. The public desktop crate remains unsafe-free.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub mod macos;
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
pub mod windows;
