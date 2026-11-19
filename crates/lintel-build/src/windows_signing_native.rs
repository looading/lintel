//! Narrow Windows ACL and namespace boundary for SignTool staging. Parent ACLs are never rewritten.
use super::*;
use std::os::windows::{
  ffi::OsStrExt,
  fs::{MetadataExt, OpenOptionsExt},
};
use windows_permissions::{
  LocalBox, SecurityDescriptor,
  constants::{AceFlags, AceType, SeObjectType, SecurityInformation},
  utilities::current_process_sid,
  wrappers,
};

fn trusted(sid: &windows_permissions::Sid, current: &windows_permissions::Sid) -> bool {
  sid == current || trusted_system_principal(&sid.to_string())
}
/// Conservative grant audit. Other principals may read, but cannot mutate objects or DELETE_CHILD.
/// Callback/object/custom ACEs are not approximated; deny them as an unsupported ACL shape.
pub(crate) fn trusted_writers(path: &Path) -> Result<()> {
  let descriptor = wrappers::GetNamedSecurityInfo(
    path.as_os_str(),
    SeObjectType::SE_FILE_OBJECT,
    SecurityInformation::Owner | SecurityInformation::Dacl,
  )?;
  let current = current_process_sid()?;
  ensure!(
    descriptor.owner().is_some_and(|owner| trusted(owner, &current)),
    "signing path owner is not the current user or an explicitly trusted OS principal"
  );
  let dacl = descriptor.dacl().context("signing path has a null DACL")?;
  ensure!(dacl.len() > 0, "empty signing path ACL is unsupported");
  for index in 0..dacl.len() {
    let ace = dacl.get_ace(index).context("invalid signing path ACE")?;
    if ace.flags().contains(AceFlags::InheritOnly) {
      continue;
    }
    if ace.ace_type() == AceType::ACCESS_DENIED_ACE_TYPE {
      continue;
    }
    ensure!(
      ace.ace_type() == AceType::ACCESS_ALLOWED_ACE_TYPE,
      "unsupported signing path ACL entry"
    );
    let sid = ace.sid().context("signing path ACE has no SID")?;
    // WRITE_DATA/ADD_FILE, APPEND/ADD_SUBDIR, WRITE_EA, DELETE_CHILD, WRITE_ATTRIBUTES,
    // DELETE, WRITE_DAC, WRITE_OWNER, GENERIC_WRITE/ALL and MAXIMUM_ALLOWED are mutation grants.
    const MUTATE: u32 = 0x00000156 | 0x000d0000 | 0x50000000 | 0x02000000;
    ensure!(
      trusted(sid, &current) || ace.mask().bits() & MUTATE == 0,
      "signing path grants another principal mutation/delete-child access; choose a private build/SDK directory"
    );
  }
  Ok(())
}

pub(crate) struct Stage {
  path: PathBuf,
  pin: Option<fs::File>,
}
impl Stage {
  pub(crate) fn path(&self) -> &Path {
    &self.path
  }
}
impl Drop for Stage {
  fn drop(&mut self) {
    drop(self.pin.take());
    let _ = fs::remove_dir_all(&self.path);
  }
}

pub(crate) fn stage(output: &Path) -> Result<Stage> {
  trusted_writers(output)?;
  let sid = current_process_sid()?;
  let descriptor: LocalBox<SecurityDescriptor> =
    format!("O:{sid}D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)").parse()?;
  let attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
    nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
    lpSecurityDescriptor: descriptor.as_ptr().cast(),
    bInheritHandle: 0,
  };
  for _ in 0..8 {
    let mut random = [0_u8; 32];
    getrandom::fill(&mut random).map_err(|error| anyhow::anyhow!("create private signing stage nonce: {error}"))?;
    let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let path = output.join(format!(".lintel-authenticode-{name}"));
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: live NUL-terminated path and initialized SECURITY_ATTRIBUTES whose owned, valid
    // self-relative descriptor lives through CreateDirectoryW. The DACL applies at creation.
    if unsafe { windows_sys::Win32::Storage::FileSystem::CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
      let error = std::io::Error::last_os_error();
      if error.kind() == std::io::ErrorKind::AlreadyExists {
        continue;
      }
      return Err(error).context("create protected signing stage");
    }
    let result = (|| {
      let pin = fs::OpenOptions::new()
        .access_mode(0x80)
        .share_mode(3)
        .custom_flags(0x02000000 | 0x00200000)
        .open(&path)?;
      ensure!(
        pin.metadata()?.is_dir() && pin.metadata()?.file_attributes() & 0x400 == 0,
        "stage changed to reparse point"
      );
      trusted_writers(&path)?;
      Ok(Stage {
        path: path.clone(),
        pin: Some(pin),
      })
    })();
    if result.is_err() {
      let _ = fs::remove_dir(&path);
    }
    return result;
  }
  bail!("could not allocate exclusive private signing staging directory")
}
