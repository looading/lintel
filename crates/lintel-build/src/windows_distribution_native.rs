//! Native read/share and protected-output boundary for signed managed distribution directories.
use super::*;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use windows_permissions::{
  constants::{AceFlags, AceType, SeObjectType, SecurityInformation},
  utilities::current_process_sid,
  wrappers,
};

pub(super) fn private_key(path: &Path) -> Result<()> {
  crate::windows_signing::native::trusted_writers(path)?;
  let current = current_process_sid()?;
  let descriptor = wrappers::GetNamedSecurityInfo(
    path.as_os_str(),
    SeObjectType::SE_FILE_OBJECT,
    SecurityInformation::Dacl,
  )?;
  let acl = descriptor.dacl().context("private signing key has null DACL")?;
  for index in 0..acl.len() {
    let ace = acl.get_ace(index).context("invalid private signing key ACE")?;
    if ace.flags().contains(AceFlags::InheritOnly) || ace.ace_type() == AceType::ACCESS_DENIED_ACE_TYPE {
      continue;
    }
    ensure!(
      ace.ace_type() == AceType::ACCESS_ALLOWED_ACE_TYPE,
      "unsupported private signing key ACL"
    );
    let sid = ace.sid().context("private signing key ACE lacks SID")?;
    let trusted = sid == &*current || crate::windows_signing::trusted_system_principal(&sid.to_string());
    ensure!(
      trusted || ace.mask().bits() & 0x92000001 == 0,
      "private signing key is readable by another principal"
    );
  }
  Ok(())
}
fn directories(paths: impl IntoIterator<Item = PathBuf>) -> Result<Vec<fs::File>> {
  let mut directories = BTreeSet::new();
  for path in paths {
    for ancestor in path.ancestors() {
      directories.insert(ancestor.to_owned());
    }
  }
  let mut pins = Vec::new();
  for path in directories {
    checked_path(&path, true)?;
    let file = fs::OpenOptions::new()
      .access_mode(0x80)
      .share_mode(3)
      .custom_flags(0x02000000 | 0x00200000)
      .open(&path)?;
    let metadata = file.metadata()?;
    ensure!(
      metadata.is_dir() && metadata.file_attributes() & 0x400 == 0,
      "distribution directory became a reparse point"
    );
    pins.push(file);
  }
  Ok(pins)
}
pub(super) fn pin_output(output: &Path) -> Result<Vec<fs::File>> {
  for ancestor in output.ancestors() {
    namespace_acl(ancestor)?;
  }
  crate::windows_signing::native::trusted_writers(output)?;
  let pins = directories([output.to_owned()])?;
  for ancestor in output.ancestors() {
    namespace_acl(ancestor)?;
  }
  crate::windows_signing::native::trusted_writers(output)?;
  Ok(pins)
}

// A private leaf under a shared DELETE_CHILD/WRITE_DAC parent can still be replaced before
// its handle is acquired. Audit the complete existing output namespace, without changing ACLs.
fn namespace_acl(path: &Path) -> Result<()> {
  let current = current_process_sid()?;
  let descriptor = wrappers::GetNamedSecurityInfo(
    path.as_os_str(),
    SeObjectType::SE_FILE_OBJECT,
    SecurityInformation::Owner | SecurityInformation::Dacl,
  )?;
  let trusted = |sid: &windows_permissions::Sid| {
    sid == &*current || crate::windows_signing::trusted_system_principal(&sid.to_string())
  };
  ensure!(
    descriptor.owner().is_some_and(trusted),
    "distribution output ancestor has an untrusted owner"
  );
  let acl = descriptor
    .dacl()
    .context("distribution output ancestor has null DACL")?;
  ensure!(acl.len() > 0, "distribution output ancestor ACL is unsupported");
  for index in 0..acl.len() {
    let ace = acl.get_ace(index).context("invalid distribution ancestor ACE")?;
    if ace.flags().contains(AceFlags::InheritOnly) || ace.ace_type() == AceType::ACCESS_DENIED_ACE_TYPE {
      continue;
    }
    ensure!(
      ace.ace_type() == AceType::ACCESS_ALLOWED_ACE_TYPE,
      "unsupported distribution ancestor ACL"
    );
    let sid = ace.sid().context("distribution ancestor ACE lacks SID")?;
    // ADD_SUBDIRECTORY alone is allowed on system profile ancestors. It cannot replace an
    // existing private child; directory mutation, reparse/write and security/deletion grants cannot.
    const REPLACE: u32 = 0x00000152 | 0x000d0000 | 0x50000000 | 0x02000000;
    ensure!(
      trusted(sid) || ace.mask().bits() & REPLACE == 0,
      "distribution output ancestor permits untrusted mutation/replacement; choose a private workspace"
    );
  }
  Ok(())
}
pub(super) fn pin_inputs(root: &Path, manifest: &Manifest) -> Result<Vec<fs::File>> {
  let mut pins = directories([root.join("bundle"), root.join("tools")])?;
  for relative in manifest
    .files
    .keys()
    .map(String::as_str)
    .chain(std::iter::once(MANIFEST))
  {
    let path = checked_path(&root.join(relative), false)?;
    let file = fs::OpenOptions::new()
      .read(true)
      .share_mode(1)
      .custom_flags(0x00200000)
      .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
      metadata.is_file() && metadata.file_attributes() & 0x400 == 0,
      "distribution file became a reparse point"
    );
    pins.push(file);
  }
  Ok(pins)
}

pub(super) fn same_directory(left: &Path, right: &Path) -> Result<bool> {
  use std::os::windows::io::AsRawHandle;
  use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
  fn identity(path: &Path) -> Result<(u32, u32, u32)> {
    let file = fs::OpenOptions::new()
      .access_mode(0x80)
      .share_mode(7)
      .custom_flags(0x02000000 | 0x00200000)
      .open(path)?;
    ensure!(
      file.metadata()?.is_dir() && file.metadata()?.file_attributes() & 0x400 == 0,
      "directory identity is a reparse point"
    );
    // SAFETY: this structure contains only integers and FILETIME pairs, for which zero is valid.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: live directory handle and correctly aligned writable output; no ownership transfer.
    ensure!(
      unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
      "cannot establish distribution directory identity: {}",
      std::io::Error::last_os_error()
    );
    ensure!(
      info.nFileIndexHigh != 0 || info.nFileIndexLow != 0,
      "filesystem does not expose stable directory identity"
    );
    Ok((info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow))
  }
  Ok(identity(left)? == identity(right)?)
}
