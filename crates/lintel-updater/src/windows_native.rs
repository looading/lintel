//! Narrow Windows process/ACL boundary; never terminates the application being updated.

use anyhow::{Context, Result, bail, ensure};
use std::{
  collections::BTreeSet,
  ffi::OsString,
  fs,
  os::windows::ffi::{OsStrExt, OsStringExt},
  path::{Path, PathBuf},
  time::{Duration, Instant},
};
use windows_sys::Win32::{
  Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT},
  Globalization::{CSTR_EQUAL, CompareStringOrdinal},
  System::{
    Diagnostics::ToolHelp::{
      CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    },
    Threading::{
      OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW,
      WaitForSingleObject,
    },
  },
};

struct Handle(HANDLE);
impl Drop for Handle {
  fn drop(&mut self) {
    // SAFETY: constructors retain only successful owned process/snapshot handles.
    unsafe {
      CloseHandle(self.0);
    }
  }
}

fn open(pid: u32) -> Result<Handle> {
  // SAFETY: no borrowed buffers; no inheritance, mutation, or termination access is requested.
  let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
  if handle.is_null() {
    return Err(std::io::Error::last_os_error()).with_context(|| format!("open process {pid}"));
  }
  Ok(Handle(handle))
}

fn image(process: &Handle) -> Result<PathBuf> {
  let mut buffer = vec![0_u16; 32768];
  let mut length = buffer.len() as u32;
  // SAFETY: the process handle is live; buffer and length are writable for this call.
  ensure!(
    unsafe { QueryFullProcessImageNameW(process.0, 0, buffer.as_mut_ptr(), &mut length) } != 0,
    "query process image: {}",
    std::io::Error::last_os_error()
  );
  ensure!((length as usize) <= buffer.len(), "process path length exceeds buffer");
  PathBuf::from(OsString::from_wide(&buffer[..length as usize]))
    .canonicalize()
    .context("resolve process image")
}

fn process_identity(handle: HANDLE, pid: u32) -> Result<super::health::ProcessIdentity> {
  use windows_sys::Win32::{Foundation::FILETIME, System::Threading::GetProcessTimes};
  let mut created = FILETIME {
    dwLowDateTime: 0,
    dwHighDateTime: 0,
  };
  let mut exit = created;
  let mut kernel = created;
  let mut user = created;
  // SAFETY: borrowed live process handle; all four FILETIME outputs are writable and initialized.
  ensure!(
    unsafe { GetProcessTimes(handle, &mut created, &mut exit, &mut kernel, &mut user) } != 0,
    "query process creation time: {}",
    std::io::Error::last_os_error()
  );
  Ok(super::health::ProcessIdentity {
    pid,
    created: ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64,
  })
}

pub(super) fn child_identity(child: &std::process::Child) -> Result<super::health::ProcessIdentity> {
  use std::os::windows::io::AsRawHandle;
  process_identity(child.as_raw_handle(), child.id())
}

pub(super) fn current_process_identity() -> Result<super::health::ProcessIdentity> {
  // SAFETY: GetCurrentProcess returns a borrowed pseudo-handle; it is deliberately not closed.
  process_identity(
    unsafe { windows_sys::Win32::System::Threading::GetCurrentProcess() },
    std::process::id(),
  )
}

pub(super) fn pipe_available(handle: HANDLE) -> Result<u32> {
  let mut available = 0;
  // SAFETY: borrowed pipe handle, no read/consumption, one initialized integer output.
  ensure!(
    unsafe {
      windows_sys::Win32::System::Pipes::PeekNamedPipe(
        handle,
        std::ptr::null_mut(),
        0,
        std::ptr::null_mut(),
        &mut available,
        std::ptr::null_mut(),
      )
    } != 0,
    "readiness pipe closed/unavailable: {}",
    std::io::Error::last_os_error()
  );
  Ok(available)
}

/// Raw counterpart of pipe_available: Rust's buffered StdinLock must not consume these bytes.
pub(super) fn read_pipe(handle: HANDLE, buffer: &mut [u8]) -> Result<usize> {
  let mut read = 0;
  let length: u32 = buffer.len().try_into()?;
  // SAFETY: borrowed live pipe handle, exclusive SDK input contract, initialized writable buffer;
  // caller has just peeked at least this many available bytes, and no OVERLAPPED I/O is requested.
  ensure!(
    unsafe {
      windows_sys::Win32::Storage::FileSystem::ReadFile(
        handle,
        buffer.as_mut_ptr(),
        length,
        &mut read,
        std::ptr::null_mut(),
      )
    } != 0,
    "read raw startup challenge: {}",
    std::io::Error::last_os_error()
  );
  ensure!(read <= length, "native pipe read exceeds buffer");
  Ok(read as usize)
}

pub(super) fn equal_prefix(path: &Path, prefix: &Path, exact: bool) -> bool {
  let path: Vec<u16> = path.as_os_str().encode_wide().collect();
  let prefix: Vec<u16> = prefix.as_os_str().encode_wide().collect();
  if path.len() < prefix.len()
    || (exact && path.len() != prefix.len())
    || (!exact && (path.len() == prefix.len() || path[prefix.len()] != b'\\' as u16))
  {
    return false;
  }
  // SAFETY: both initialized UTF-16 slices live through the bounded ordinal comparison.
  unsafe {
    CompareStringOrdinal(
      path.as_ptr(),
      prefix.len() as i32,
      prefix.as_ptr(),
      prefix.len() as i32,
      1,
    ) == CSTR_EQUAL
  }
}

pub(super) fn processes_in_bundle(directory: &Path) -> Result<Vec<u32>> {
  let directory = directory.canonicalize()?;
  let names: BTreeSet<_> = fs::read_dir(&directory)?
    .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().to_ascii_lowercase()))
    .collect::<std::io::Result<_>>()?;
  // SAFETY: requests a read-only process snapshot and does not borrow external memory.
  let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
  ensure!(
    snapshot != INVALID_HANDLE_VALUE,
    "cannot enumerate processes: {}",
    std::io::Error::last_os_error()
  );
  let snapshot = Handle(snapshot);
  // SAFETY: PROCESSENTRY32W contains integers, pointers, and a UTF-16 array; zero is valid for all fields.
  let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
  entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
  // SAFETY: the snapshot is live and entry has the required initialized dwSize/capacity.
  let mut available = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
  let mut active = Vec::new();
  while available {
    if entry.th32ProcessID != 0 {
      let end = entry
        .szExeFile
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(entry.szExeFile.len());
      let named = names.contains(
        &OsString::from_wide(&entry.szExeFile[..end])
          .to_string_lossy()
          .to_ascii_lowercase(),
      );
      match open(entry.th32ProcessID).and_then(|process| {
        // SAFETY: handle is live; zero timeout only observes its signaled state.
        if unsafe { WaitForSingleObject(process.0, 0) } == WAIT_OBJECT_0 {
          return Ok(None);
        }
        match image(&process) {
          Ok(path) => Ok(Some(path)),
          Err(error) => {
            // SAFETY: zero-time observation of the same retained handle handles exit/query races.
            if unsafe { WaitForSingleObject(process.0, 0) } == WAIT_OBJECT_0 {
              Ok(None)
            } else {
              Err(error)
            }
          }
        }
      }) {
        Ok(Some(path)) if equal_prefix(&path, &directory, false) => active.push(entry.th32ProcessID),
        Ok(_) => {}
        Err(error)
          if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.raw_os_error() == Some(87)) => {}
        Err(error) if named => {
          return Err(error).context("cannot rule out a running application/helper with this executable name");
        }
        Err(_) => {}
      }
    }
    // SAFETY: same live snapshot and initialized writable entry as above.
    available = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
  }
  ensure!(
    std::io::Error::last_os_error().raw_os_error() == Some(18),
    "process enumeration failed before exhaustion"
  );
  Ok(active)
}

/// Maintenance must be external and confined to a private current-user installation.
pub(super) fn validate_existing_install_root(root: &Path) -> Result<PathBuf> {
  check_private(root, true)?;
  let local = local_app_data()?.canonicalize()?;
  let root = root.canonicalize()?;
  ensure!(
    equal_prefix(&root, &local, false),
    "installation is outside current-user LocalAppData"
  );
  check_existing_ancestors(&local, &root)?;
  Ok(root)
}

/// Reserve and hash every file before marking any for deletion. Never force-close processes.
pub(super) fn remove_owned_files(files: &[(PathBuf, String)]) -> Result<()> {
  use sha2::Digest;
  use std::{
    io::Read,
    os::windows::{
      fs::{MetadataExt, OpenOptionsExt},
      io::AsRawHandle,
    },
  };
  use windows_sys::Win32::{
    Storage::FileSystem::{
      FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_FORCE_IMAGE_SECTION_CHECK, FILE_DISPOSITION_INFO_EX,
      FileDispositionInfoEx, SetFileInformationByHandle,
    },
    System::RestartManager::{RmEndSession, RmGetList, RmRegisterResources, RmStartSession},
  };
  ensure!(files.len() <= 4096, "too many files in a maintenance batch");
  let mut session = 0_u32;
  let mut key = [0_u16; 33];
  // SAFETY: both outputs have the capacities required by Restart Manager (32 chars plus NUL).
  ensure!(
    unsafe { RmStartSession(&mut session, 0, key.as_mut_ptr()) } == 0,
    "cannot start Restart Manager inspection"
  );
  struct Session(u32);
  impl Drop for Session {
    fn drop(&mut self) {
      /* SAFETY: successful owned RM session. */
      unsafe {
        RmEndSession(self.0);
      }
    }
  }
  let _session = Session(session);
  let names: Vec<Vec<u16>> = files
    .iter()
    .map(|(path, _)| path.as_os_str().encode_wide().chain(Some(0)).collect())
    .collect();
  let pointers: Vec<_> = names.iter().map(|name| name.as_ptr()).collect();
  // SAFETY: all NUL-terminated paths remain alive throughout registration; no app/service arrays.
  ensure!(
    unsafe {
      RmRegisterResources(
        session,
        pointers.len() as u32,
        pointers.as_ptr(),
        0,
        std::ptr::null(),
        0,
        std::ptr::null(),
      )
    } == 0,
    "cannot inspect file users"
  );
  let mut needed = 0;
  let mut count = 0;
  let mut reasons = 0;
  // SAFETY: zero-capacity query only writes the three integer outputs.
  let result = unsafe { RmGetList(session, &mut needed, &mut count, std::ptr::null_mut(), &mut reasons) };
  ensure!(
    result == 0 && needed == 0,
    "owned files are in use or their liveness cannot be established (Restart Manager {result})"
  );
  let mut reserved = Vec::new();
  for (path, expected) in files {
    check_private(path, false)?;
    let mut file = fs::OpenOptions::new()
      .read(true)
      .access_mode(0x80000000 | 0x00010000)
      .share_mode(0)
      .custom_flags(0x00200000)
      .open(path)
      .with_context(|| format!("reserve {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
      metadata.is_file() && metadata.file_attributes() & 0x400 == 0,
      "reserved file is a reparse point or special file"
    );
    let mut digest = sha2::Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
      let count = file.read(&mut buffer)?;
      if count == 0 {
        break;
      }
      digest.update(&buffer[..count]);
    }
    ensure!(
      format!("{:x}", digest.finalize()) == *expected,
      "owned file changed before deletion"
    );
    reserved.push(file);
  }
  let disposition = FILE_DISPOSITION_INFO_EX {
    Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_FORCE_IMAGE_SECTION_CHECK,
  };
  // Delete through the exact reserved handle; the image-section flag refuses loaded executables/DLLs.
  // Unsupported Windows/filesystems fail closed. Never POSIX-delete or schedule deletion at reboot.
  for file in &reserved {
    // SAFETY: live DELETE-capable file handle and correctly sized initialized disposition structure.
    if unsafe {
      SetFileInformationByHandle(
        file.as_raw_handle(),
        FileDispositionInfoEx,
        (&disposition as *const FILE_DISPOSITION_INFO_EX).cast(),
        std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
      )
    } == 0
    {
      let error = std::io::Error::last_os_error();
      let cancel = FILE_DISPOSITION_INFO_EX { Flags: 0 };
      let mut cancelled = true;
      for previous in &reserved {
        // SAFETY: the same live reserved handles; clear pending delete before any handle closes.
        cancelled &= unsafe {
          SetFileInformationByHandle(
            previous.as_raw_handle(),
            FileDispositionInfoEx,
            (&cancel as *const FILE_DISPOSITION_INFO_EX).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
          )
        } != 0;
      }
      bail!("cannot mark owned file for deletion: {error}; cancelled pending deletions={cancelled}");
    }
  }
  Ok(())
}

pub(super) struct Parent(Handle);
impl Parent {
  pub(super) fn open(pid: u32, expected: &Path) -> Result<Self> {
    ensure!(pid != 0 && pid != std::process::id(), "invalid update parent PID");
    let process = open(pid)?;
    ensure!(
      equal_prefix(&image(&process)?, &expected.canonicalize()?, true),
      "update parent is not the active main executable"
    );
    Ok(Self(process))
  }

  pub(super) fn wait_and_check(&self, directory: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    // SAFETY: the retained handle identifies this process object even if its PID is later reused.
    match unsafe { WaitForSingleObject(self.0.0, 120000) } {
      WAIT_OBJECT_0 => {}
      WAIT_TIMEOUT => bail!("application did not exit within 120 seconds; active version retained"),
      _ => bail!("wait for application exit: {}", std::io::Error::last_os_error()),
    }
    loop {
      if processes_in_bundle(directory)?.is_empty() {
        return Ok(());
      }
      ensure!(
        Instant::now() < deadline,
        "application helpers did not exit; active version retained"
      );
      std::thread::sleep(Duration::from_millis(100));
    }
  }
}

pub(super) fn check_private(path: &Path, directory: bool) -> Result<()> {
  use std::os::windows::fs::MetadataExt;
  use windows_permissions::{
    constants::{AceType, SeObjectType, SecurityInformation},
    utilities::current_process_sid,
    wrappers,
  };
  let metadata = fs::symlink_metadata(path)?;
  ensure!(
    metadata.file_attributes() & 0x400 == 0
      && if directory {
        metadata.is_dir()
      } else {
        metadata.is_file()
      },
    "installation path is a reparse point or wrong file type"
  );
  let sid = current_process_sid()?;
  let descriptor = wrappers::GetNamedSecurityInfo(
    path.as_os_str(),
    SeObjectType::SE_FILE_OBJECT,
    SecurityInformation::Owner | SecurityInformation::Dacl,
  )?;
  let dacl = descriptor.dacl().context("null installation DACL")?;
  ensure!(
    descriptor.owner() == Some(&*sid) && dacl.len() > 0,
    "installation path is not owned by the current user"
  );
  for index in 0..dacl.len() {
    let ace = dacl.get_ace(index).context("invalid installation ACE")?;
    ensure!(
      ace.ace_type() == AceType::ACCESS_ALLOWED_ACE_TYPE && ace.sid() == Some(&*sid),
      "installation path grants access to another principal"
    );
  }
  Ok(())
}

pub(super) fn protect_new(path: &Path) -> Result<()> {
  use std::os::windows::fs::MetadataExt;
  use windows_permissions::{
    LocalBox, SecurityDescriptor,
    constants::{SeObjectType, SecurityInformation},
    utilities::current_process_sid,
    wrappers,
  };
  let sid = current_process_sid()?;
  let metadata = fs::symlink_metadata(path)?;
  ensure!(
    metadata.file_attributes() & 0x400 == 0 && (metadata.is_dir() || metadata.is_file()),
    "new protected path cannot be a reparse point or special file"
  );
  let current = wrappers::GetNamedSecurityInfo(
    path.as_os_str(),
    SeObjectType::SE_FILE_OBJECT,
    SecurityInformation::Owner,
  )?;
  ensure!(
    current.owner() == Some(&*sid),
    "new installation path is not owned by this user"
  );
  let descriptor: LocalBox<SecurityDescriptor> = format!("D:P(A;OICI;FA;;;{sid})").parse()?;
  wrappers::SetNamedSecurityInfo(
    path.as_os_str(),
    SeObjectType::SE_FILE_OBJECT,
    SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
    None,
    None,
    descriptor.dacl(),
    None,
  )?;
  check_private(path, fs::metadata(path)?.is_dir())
}

pub(super) fn create_install_root(requested: Option<&Path>, app_id: &str) -> Result<PathBuf> {
  create_install_root_checked(requested, app_id, &|_| Ok(()))
}

pub(super) fn create_install_root_checked(
  requested: Option<&Path>,
  app_id: &str,
  check_root: &impl Fn(&Path) -> Result<()>,
) -> Result<PathBuf> {
  lintel_build::validate_windows_filename(app_id)?;
  let local = local_app_data()?.canonicalize()?;
  ensure!(
    local.is_absolute() && fs::symlink_metadata(&local)?.is_dir(),
    "invalid per-user application-data root"
  );
  let local_text = local.to_str().context("per-user installation root must be Unicode")?;
  let local_text = local_text.strip_prefix(r"\\?\").unwrap_or(local_text);
  ensure!(
    local_text.len() >= 3
      && local_text.as_bytes()[0].is_ascii_alphabetic()
      && local_text.as_bytes()[1] == b':'
      && local_text.as_bytes()[2] == b'\\',
    "Windows updates require a local drive, not a UNC/network profile"
  );
  let destination = requested
    .map(Path::to_owned)
    .unwrap_or_else(|| local.join("LintelApplications").join(app_id));
  let (current, tail) = normalize_destination(&destination, &local)?;
  check_existing_ancestors(&local, &current)?;
  let current = super::create_checked_install_tail(&current, &tail, check_root, |path, last| {
    match fs::create_dir(path) {
      Ok(()) => {
        if let Err(error) = protect_new(path) {
          let _ = fs::remove_dir(path);
          return Err(error);
        }
      }
      Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && !last => check_private(path, true)?,
      Err(error) => return Err(error).context("create fresh per-user install directory"),
    }
    Ok(())
  })?;
  current.canonicalize().context("resolve new per-user installation")
}

fn local_app_data() -> Result<PathBuf> {
  use windows_sys::Win32::{
    System::Com::CoTaskMemFree,
    UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath},
  };
  let mut pointer = std::ptr::null_mut();
  // SAFETY: fixed known-folder GUID, current-user token, and a writable null output pointer.
  let result = unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, 0, std::ptr::null_mut(), &mut pointer) };
  ensure!(
    result >= 0 && !pointer.is_null(),
    "query current-user LocalAppData: HRESULT {result:#x}"
  );
  struct Allocation(*mut u16);
  impl Drop for Allocation {
    fn drop(&mut self) {
      // SAFETY: successful SHGetKnownFolderPath transfers this CoTaskMem allocation to the caller.
      unsafe {
        CoTaskMemFree(self.0.cast());
      }
    }
  }
  let allocation = Allocation(pointer);
  let mut length = 0;
  // SAFETY: the successful API result is a live NUL-terminated UTF-16 string until allocation drops.
  while unsafe { *allocation.0.add(length) } != 0 {
    length += 1;
    ensure!(length < 32768, "known-folder path is too long");
  }
  // SAFETY: length covers only initialized UTF-16 code units before the terminator.
  let value = unsafe { std::slice::from_raw_parts(allocation.0, length) };
  Ok(PathBuf::from(OsString::from_wide(value)))
}

fn check_existing_ancestors(local: &Path, ancestor: &Path) -> Result<()> {
  ensure!(
    equal_prefix(ancestor, local, true) || equal_prefix(ancestor, local, false),
    "existing install ancestor escapes LOCALAPPDATA"
  );
  let mut current = local.to_owned();
  // Both roots are canonical and prefix-equal under Windows ordinal comparison.
  // Inspect each level: a private leaf is insufficient if an outer directory grants DELETE_CHILD.
  for component in ancestor.components().skip(local.components().count()) {
    ensure!(
      matches!(component, std::path::Component::Normal(_)),
      "non-normal canonical ancestor"
    );
    current.push(component.as_os_str());
    check_private(&current, true)?;
  }
  Ok(())
}

fn normalize_destination(destination: &Path, local: &Path) -> Result<(PathBuf, Vec<OsString>)> {
  use std::os::windows::fs::MetadataExt;
  use std::path::{Component, Prefix};
  let mut components = destination.components();
  ensure!(
    matches!(components.next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
      && matches!(components.next(), Some(Component::RootDir))
      && components.all(|component| matches!(component, Component::Normal(_))),
    "installation root must be a local absolute path without parent traversal"
  );
  match fs::symlink_metadata(destination) {
    Ok(_) => bail!("installation root already exists"),
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
    Err(error) => return Err(error.into()),
  }
  let mut ancestor = destination.to_owned();
  let mut tail = Vec::new();
  loop {
    match fs::symlink_metadata(&ancestor) {
      Ok(metadata) => {
        ensure!(
          metadata.is_dir() && metadata.file_attributes() & 0x400 == 0,
          "existing install parent is a file or reparse point"
        );
        break;
      }
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
        let component = ancestor
          .file_name()
          .context("install root has no valid new tail")?
          .to_owned();
        let value = component.to_str().context("install directory must be Unicode")?;
        let stem = value.split('.').next().unwrap_or("").to_ascii_uppercase();
        let device = [
          "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "CLOCK$", "COM¹", "COM²", "COM³", "LPT¹", "LPT²", "LPT³",
        ]
        .contains(&stem.as_str())
          || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|digit| digit.len() == 1 && matches!(digit.as_bytes()[0], b'1'..=b'9'));
        ensure!(
          !device
            && !value.is_empty()
            && value.len() <= 200
            && value.trim() == value
            && !value.ends_with('.')
            && !value.contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*'])
            && !value.chars().any(char::is_control),
          "unsafe new install directory component"
        );
        tail.push(component);
        ensure!(ancestor.pop(), "cannot locate existing install parent");
      }
      Err(error) => return Err(error.into()),
    }
  }
  let ancestor = ancestor.canonicalize()?;
  ensure!(
    equal_prefix(&ancestor, local, true) || equal_prefix(&ancestor, local, false),
    "Windows installation must remain inside current-user LOCALAPPDATA"
  );
  ensure!(!tail.is_empty(), "installation root must be new");
  tail.reverse();
  Ok((ancestor, tail))
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn destination_normalization_accepts_normal_extended_and_case_variants_without_escaping() {
    let temporary = tempfile::tempdir().unwrap();
    protect_new(temporary.path()).unwrap();
    let local = temporary.path().canonicalize().unwrap();
    let normal = PathBuf::from(
      local
        .to_str()
        .unwrap()
        .strip_prefix(r"\\?\")
        .unwrap_or(local.to_str().unwrap()),
    );
    let upper = PathBuf::from(normal.to_str().unwrap().to_ascii_uppercase());
    for base in [normal.clone(), local.clone(), upper] {
      let (ancestor, tail) = normalize_destination(&base.join("新应用"), &local).unwrap();
      assert!(equal_prefix(&ancestor, &local, true));
      assert_eq!(tail, [OsString::from("新应用")]);
    }
    let outside = tempfile::tempdir().unwrap();
    assert!(normalize_destination(&outside.path().join("Application"), &local).is_err());
    assert!(normalize_destination(&normal.join("..").join("escape"), &local).is_err());
    assert!(normalize_destination(Path::new(r"\\server\share\Application"), &local).is_err());
    assert!(normalize_destination(&normal.join("file:stream"), &local).is_err());
  }

  #[test]
  fn shared_outer_ancestor_is_rejected_even_when_the_nearest_parent_is_private() {
    use windows_permissions::{
      LocalBox, SecurityDescriptor,
      constants::{SeObjectType, SecurityInformation},
      utilities::current_process_sid,
      wrappers,
    };
    let temporary = tempfile::tempdir().unwrap();
    protect_new(temporary.path()).unwrap();
    let local = temporary.path().canonicalize().unwrap();
    let shared = local.join("Shared");
    let private = shared.join("PrivateChild");
    fs::create_dir(&shared).unwrap();
    protect_new(&shared).unwrap();
    fs::create_dir(&private).unwrap();
    protect_new(&private).unwrap();
    let sid = current_process_sid().unwrap();
    let descriptor: LocalBox<SecurityDescriptor> =
      format!("D:P(A;OICI;FA;;;{sid})(A;;0x00000040;;;WD)").parse().unwrap();
    wrappers::SetNamedSecurityInfo(
      shared.as_os_str(),
      SeObjectType::SE_FILE_OBJECT,
      SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
      None,
      None,
      descriptor.dacl(),
      None,
    )
    .unwrap();
    assert!(check_private(&private, true).is_ok());
    let (ancestor, _) = normalize_destination(&private.join("NewApp"), &local).unwrap();
    assert!(check_existing_ancestors(&local, &ancestor).is_err());
    assert!(!private.join("NewApp").exists());
  }
  #[test]
  fn normalized_private_source_target_veto_leaves_no_new_tail_on_windows() {
    let temporary = tempfile::tempdir().unwrap();
    protect_new(temporary.path()).unwrap();
    let local = temporary.path().canonicalize().unwrap();
    let source = local.join("distribution");
    fs::create_dir(&source).unwrap();
    protect_new(&source).unwrap();
    fs::write(source.join("keep"), b"unchanged").unwrap();
    let target = source.join("NewParent").join("NewRoot");
    let (ancestor, tail) = normalize_destination(&target, &local).unwrap();
    check_existing_ancestors(&local, &ancestor).unwrap();
    let mut created = 0;
    let result = super::super::create_checked_install_tail(
      &ancestor,
      &tail,
      |candidate| {
        ensure!(
          !equal_prefix(candidate, &source, true) && !equal_prefix(candidate, &source, false),
          "overlap veto"
        );
        Ok(())
      },
      |path, _| {
        created += 1;
        fs::create_dir(path)?;
        protect_new(path)
      },
    );
    assert!(result.is_err());
    assert_eq!(created, 0);
    assert!(!source.join("NewParent").exists());
    assert_eq!(fs::read(source.join("keep")).unwrap(), b"unchanged");
  }
}
