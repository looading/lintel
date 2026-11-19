//! Explicit per-user URL registration. Existing foreign associations are never overwritten.

use std::{
  collections::{BTreeMap, BTreeSet},
  path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

type Tree = BTreeMap<String, BTreeMap<String, String>>;

#[derive(Clone, Debug, Default, Serialize)]
pub struct WindowsProtocolRemoval {
  pub removed: Vec<String>,
  pub absent: Vec<String>,
  pub retained_foreign: Vec<String>,
}

/// Persist this receipt before registering; it authorizes only its exact association tree.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowsProtocolRegistration {
  schema: u32,
  app_id: String,
  executable: String,
  schemes: Vec<String>,
  owner: String,
}

// Static installability policy shared by association receipts and signed distribution inputs.
// Empty schemes are valid for a distribution that requests no registration.
pub(crate) fn validate_windows_protocol_schemes(schemes: &[String]) -> Result<()> {
  crate::validate_protocol_schemes(schemes)?;
  for scheme in schemes {
    crate::windows::validate_windows_filename(scheme)?;
    ensure!(
      ![
        "http",
        "https",
        "file",
        "ftp",
        "mailto",
        "data",
        "javascript",
        "about",
        "shell",
        "search-ms",
        "ms-settings",
        "ms-windows-store"
      ]
      .contains(&scheme.to_ascii_lowercase().as_str()),
      "system protocol scheme {scheme:?} cannot be claimed"
    );
  }
  Ok(())
}

impl WindowsProtocolRegistration {
  /// The executable must accept `--lintel-protocol URL`; the managed launcher implements it.
  pub fn prepare(app_id: &str, executable: &Path, schemes: &[String]) -> Result<Self> {
    #[cfg(windows)]
    {
      let executable = crate::windows::checked_protocol_executable(executable)?;
      let mut random = [0_u8; 32];
      getrandom::fill(&mut random).map_err(|error| anyhow::anyhow!("generate association owner: {error}"))?;
      let plan = Self {
        schema: 1,
        app_id: app_id.to_owned(),
        executable: executable
          .to_str()
          .context("protocol executable must be Unicode")?
          .to_owned(),
        schemes: schemes.iter().map(|scheme| scheme.to_ascii_lowercase()).collect(),
        owner: random.iter().map(|byte| format!("{byte:02x}")).collect(),
      };
      plan.validate()?;
      Ok(plan)
    }
    #[cfg(not(windows))]
    {
      let _ = (app_id, executable, schemes);
      bail!("native per-user Windows protocol registration requires Windows");
    }
  }

  pub fn app_id(&self) -> &str {
    &self.app_id
  }
  pub fn schemes(&self) -> &[String] {
    &self.schemes
  }
  pub fn executable(&self) -> &Path {
    Path::new(&self.executable)
  }

  pub fn read_receipt(path: &Path) -> Result<Self> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
      metadata.is_file() && !metadata.is_symlink() && metadata.len() <= 65536,
      "protocol receipt must be a real file of at most 64 KiB"
    );
    #[cfg(windows)]
    {
      use std::os::windows::fs::MetadataExt;
      ensure!(
        metadata.file_attributes() & 0x400 == 0,
        "protocol receipt cannot be a reparse point"
      );
    }
    let receipt: Self = serde_json::from_slice(&std::fs::read(path)?)?;
    receipt.validate()?;
    Ok(receipt)
  }

  /// Save before committing registration, without replacing any existing file.
  pub fn write_receipt(&self, path: &Path) -> Result<()> {
    use std::io::Write;
    self.validate()?;
    let parent = path
      .parent()
      .filter(|parent| !parent.as_os_str().is_empty())
      .unwrap_or_else(|| Path::new("."));
    let mut pending = tempfile::NamedTempFile::new_in(parent)?;
    pending.write_all(&serde_json::to_vec_pretty(self)?)?;
    pending.as_file().sync_all()?;
    pending
      .persist_noclobber(path)
      .map_err(|error| error.error)
      .context("save protocol ownership receipt without overwriting")?;
    Ok(())
  }

  pub fn validate(&self) -> Result<()> {
    ensure!(self.schema == 1, "unsupported protocol receipt schema");
    ensure!(
      self.app_id.len() <= 253
        && self.app_id.contains('.')
        && self
          .app_id
          .split('.')
          .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')),
      "invalid protocol owner application ID"
    );
    ensure!(
      self.owner.len() == 64 && self.owner.bytes().all(|byte| byte.is_ascii_hexdigit()),
      "invalid protocol owner token"
    );
    let path = self.executable.strip_prefix(r"\\?\").unwrap_or(&self.executable);
    ensure!(
      path.len() <= 32760
        && path.len() >= 7
        && path.as_bytes()[0].is_ascii_alphabetic()
        && path.as_bytes()[1] == b':'
        && path.as_bytes()[2] == b'\\'
        && !path[2..].contains([':', '"', '%', '/'])
        && !path.chars().any(char::is_control)
        && path.to_ascii_lowercase().ends_with(".exe"),
      "protocol executable must be a quoted-safe local absolute Windows .exe path"
    );
    validate_windows_protocol_schemes(&self.schemes)?;
    ensure!(!self.schemes.is_empty(), "register at least one explicit custom scheme");
    Ok(())
  }

  fn tree(&self, scheme: &str) -> Tree {
    BTreeMap::from([
      (
        String::new(),
        BTreeMap::from([
          (String::new(), format!("URL:{} ({scheme})", self.app_id)),
          ("url protocol".into(), String::new()),
          ("lintelowner".into(), self.app_id.clone()),
          ("lintelownertoken".into(), self.owner.clone()),
        ]),
      ),
      (
        "defaulticon".into(),
        BTreeMap::from([(String::new(), format!("\"{}\",0", self.executable))]),
      ),
      ("shell".into(), BTreeMap::new()),
      ("shell\\open".into(), BTreeMap::new()),
      (
        "shell\\open\\command".into(),
        BTreeMap::from([(
          String::new(),
          format!("\"{}\" --lintel-protocol \"%1\"", self.executable),
        )]),
      ),
    ])
  }
}

/// Atomically register the receipt's schemes in HKCU\Software\Classes only.
pub fn register_windows_protocols(registration: &WindowsProtocolRegistration) -> Result<()> {
  registration.validate()?;
  #[cfg(windows)]
  {
    let mut transaction = native::RegistryTransaction::new()?;
    apply(registration, &mut transaction, false)?;
    transaction.commit()
  }
  #[cfg(not(windows))]
  {
    bail!("native per-user Windows protocol registration requires Windows");
  }
}

/// Remove only unchanged associations matching the receipt; conflicts leave all keys intact.
pub fn unregister_windows_protocols(registration: &WindowsProtocolRegistration) -> Result<()> {
  registration.validate()?;
  #[cfg(windows)]
  {
    let mut transaction = native::RegistryTransaction::new()?;
    apply(registration, &mut transaction, true)?;
    transaction.commit()
  }
  #[cfg(not(windows))]
  {
    bail!("native per-user Windows protocol removal requires Windows");
  }
}

/// Uninstall removes exact owned trees while preserving foreign edits, in one Registry transaction.
pub fn unregister_matching_windows_protocols(
  registration: &WindowsProtocolRegistration,
) -> Result<WindowsProtocolRemoval> {
  registration.validate()?;
  #[cfg(windows)]
  {
    let mut transaction = native::RegistryTransaction::new()?;
    let report = remove_matching(registration, &mut transaction)?;
    transaction.commit()?;
    Ok(report)
  }
  #[cfg(not(windows))]
  {
    bail!("native per-user Windows protocol removal requires Windows");
  }
}

fn remove_matching(
  registration: &WindowsProtocolRegistration,
  registry: &mut impl RegistryTransaction,
) -> Result<WindowsProtocolRemoval> {
  registration.validate()?;
  let mut report = WindowsProtocolRemoval::default();
  let mut owned = Vec::new();
  for scheme in &registration.schemes {
    let scheme = scheme.to_ascii_lowercase();
    let expected = registration.tree(&scheme);
    match registry.read(&scheme)? {
      None => report.absent.push(scheme),
      Some(actual) if actual == expected => owned.push((scheme, expected)),
      Some(_) => report.retained_foreign.push(scheme),
    }
  }
  for (scheme, expected) in owned {
    registry.delete(&scheme, &expected)?;
    report.removed.push(scheme);
  }
  Ok(report)
}

trait RegistryTransaction {
  fn read(&self, scheme: &str) -> Result<Option<Tree>>;
  fn machine_exists(&self, scheme: &str) -> Result<bool>;
  fn create(&mut self, scheme: &str, expected: &Tree) -> Result<()>;
  fn delete(&mut self, scheme: &str, expected: &Tree) -> Result<()>;
}

fn apply(
  registration: &WindowsProtocolRegistration,
  registry: &mut impl RegistryTransaction,
  remove: bool,
) -> Result<()> {
  registration.validate()?;
  let mut changes = Vec::new();
  // 在同一原生事务中先检查完整批次，再进行任何修改。
  for scheme in &registration.schemes {
    let scheme = scheme.to_ascii_lowercase();
    let expected = registration.tree(&scheme);
    match registry.read(&scheme)? {
      Some(actual) => {
        ensure!(
          actual == expected,
          "protocol {scheme:?} is foreign or changed; no associations were modified"
        );
        if remove {
          changes.push((scheme, expected));
        }
      }
      None if !remove => {
        ensure!(
          !registry.machine_exists(&scheme)?,
          "protocol {scheme:?} is already associated machine-wide or by explicit user choice; refusing to shadow it"
        );
        changes.push((scheme, expected));
      }
      None => {}
    }
  }
  for (scheme, expected) in changes {
    if remove {
      registry.delete(&scheme, &expected)?;
    } else {
      registry.create(&scheme, &expected)?;
    }
  }
  Ok(())
}

#[cfg(windows)]
mod native {
  use super::*;
  use std::io;
  use winreg::{
    RegKey,
    enums::*,
    transaction::Transaction,
    types::{FromRegValue, ToRegValue},
  };

  pub(super) struct RegistryTransaction {
    transaction: Transaction,
    user: RegKey,
  }

  impl RegistryTransaction {
    pub(super) fn new() -> Result<Self> {
      let registry = Self {
        transaction: Transaction::new()
          .context("Windows transactional registry unavailable; no associations changed")?,
        user: RegKey::predef(HKEY_CURRENT_USER),
      };
      for parent in [r"Software", r"Software\Classes"] {
        match registry.user.open_subkey_transacted_with_options_flags(
          parent,
          &registry.transaction,
          REG_OPTION_OPEN_LINK,
          KEY_READ | KEY_WOW64_64KEY,
        ) {
          Ok(key) => match key.get_raw_value("SymbolicLinkValue") {
            Ok(value) => ensure!(
              value.vtype != REG_LINK,
              "association parent is a registry symbolic link"
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
          },
          Err(error) if error.kind() == io::ErrorKind::NotFound => {}
          Err(error) => return Err(error.into()),
        }
      }
      Ok(registry)
    }

    pub(super) fn commit(self) -> Result<()> {
      self
        .transaction
        .commit()
        .context("commit per-user protocol association transaction")?;
      use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
      // SAFETY: association-change notification takes no item pointers with these flags.
      unsafe {
        SHChangeNotify(
          SHCNE_ASSOCCHANGED as i32,
          SHCNF_IDLIST,
          std::ptr::null(),
          std::ptr::null(),
        );
      }
      Ok(())
    }

    fn path(scheme: &str, child: &str) -> String {
      if child.is_empty() {
        format!(r"Software\Classes\{scheme}")
      } else {
        format!(r"Software\Classes\{scheme}\{child}")
      }
    }

    fn snapshot(&self, scheme: &str, child: &str, tree: &mut Tree) -> Result<()> {
      ensure!(
        tree.len() < 32 && child.len() <= 256,
        "protocol association tree exceeds limits"
      );
      let key = self.user.open_subkey_transacted_with_options_flags(
        Self::path(scheme, child),
        &self.transaction,
        REG_OPTION_OPEN_LINK,
        KEY_READ | KEY_WOW64_64KEY,
      )?;
      let info = key.query_info()?;
      ensure!(
        info.values <= 16 && info.sub_keys <= 16 && info.max_value_len <= 65536 && info.max_value_name_len <= 256,
        "protocol registry key exceeds limits"
      );
      let mut values = BTreeMap::new();
      for item in key.enum_values() {
        let (name, raw) = item?;
        ensure!(
          raw.vtype == REG_SZ,
          "protocol key contains a foreign value type or registry link"
        );
        let value = String::from_reg_value(&raw)?;
        ensure!(
          value.to_reg_value().bytes == raw.bytes,
          "non-canonical UTF-16 registry value"
        );
        ensure!(
          values.insert(name.to_ascii_lowercase(), value).is_none(),
          "ambiguous registry value name"
        );
      }
      tree.insert(child.to_ascii_lowercase(), values);
      for item in key.enum_keys() {
        let name = item?;
        ensure!(
          !name.is_empty() && !name.contains(['\\', '/']),
          "invalid association child key"
        );
        let next = if child.is_empty() {
          name
        } else {
          format!("{child}\\{name}")
        };
        self.snapshot(scheme, &next, tree)?;
      }
      Ok(())
    }
  }

  impl super::RegistryTransaction for RegistryTransaction {
    fn read(&self, scheme: &str) -> Result<Option<Tree>> {
      match self.user.open_subkey_transacted_with_options_flags(
        Self::path(scheme, ""),
        &self.transaction,
        REG_OPTION_OPEN_LINK,
        KEY_READ | KEY_WOW64_64KEY,
      ) {
        Ok(_) => {
          let mut tree = Tree::new();
          self.snapshot(scheme, "", &mut tree)?;
          Ok(Some(tree))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
      }
    }

    fn machine_exists(&self, scheme: &str) -> Result<bool> {
      match self.user.open_subkey_transacted_with_options_flags(
        format!(r"Software\Microsoft\Windows\Shell\Associations\UrlAssociations\{scheme}\UserChoice"),
        &self.transaction,
        REG_OPTION_OPEN_LINK,
        KEY_READ | KEY_WOW64_64KEY,
      ) {
        Ok(_) => return Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
      }
      let machine = RegKey::predef(HKEY_LOCAL_MACHINE);
      for view in [KEY_WOW64_64KEY, KEY_WOW64_32KEY] {
        match machine.open_subkey_transacted_with_options_flags(
          Self::path(scheme, ""),
          &self.transaction,
          REG_OPTION_OPEN_LINK,
          KEY_READ | view,
        ) {
          Ok(_) => return Ok(true),
          Err(error) if error.kind() == io::ErrorKind::NotFound => {}
          Err(error) => return Err(error.into()),
        }
      }
      Ok(false)
    }

    fn create(&mut self, scheme: &str, expected: &Tree) -> Result<()> {
      for (child, values) in expected {
        let (key, disposition) = self.user.create_subkey_transacted_with_flags(
          Self::path(scheme, child),
          &self.transaction,
          KEY_READ | KEY_WRITE | KEY_WOW64_64KEY,
        )?;
        ensure!(
          disposition == REG_CREATED_NEW_KEY,
          "association key appeared during transaction"
        );
        for (name, value) in values {
          key.set_value(name, value)?;
        }
      }
      Ok(())
    }

    fn delete(&mut self, scheme: &str, expected: &Tree) -> Result<()> {
      ensure!(
        self.read(scheme)?.as_ref() == Some(expected),
        "association changed before removal"
      );
      for child in expected.keys().rev() {
        self
          .user
          .delete_subkey_transacted_with_flags(Self::path(scheme, child), &self.transaction, KEY_WOW64_64KEY)?;
      }
      Ok(())
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[derive(Clone, Default)]
  struct Registry {
    user: BTreeMap<String, Tree>,
    machine: BTreeSet<String>,
    operations: usize,
    fail_at: Option<usize>,
  }
  impl RegistryTransaction for Registry {
    fn read(&self, scheme: &str) -> Result<Option<Tree>> {
      Ok(self.user.get(scheme).cloned())
    }
    fn machine_exists(&self, scheme: &str) -> Result<bool> {
      Ok(self.machine.contains(scheme))
    }
    fn create(&mut self, scheme: &str, tree: &Tree) -> Result<()> {
      self.operations += 1;
      ensure!(self.fail_at != Some(self.operations), "injected registry failure");
      ensure!(self.user.insert(scheme.into(), tree.clone()).is_none(), "collision");
      Ok(())
    }
    fn delete(&mut self, scheme: &str, _: &Tree) -> Result<()> {
      self.operations += 1;
      ensure!(self.fail_at != Some(self.operations), "injected registry failure");
      self.user.remove(scheme);
      Ok(())
    }
  }
  fn receipt() -> WindowsProtocolRegistration {
    WindowsProtocolRegistration {
      schema: 1,
      app_id: "dev.example.app".into(),
      executable: r"C:\Users\测试\Example App\lintel-launcher.exe".into(),
      schemes: vec!["example-one".into(), "example-two".into()],
      owner: "a".repeat(64),
    }
  }
  fn transact(registry: &mut Registry, registration: &WindowsProtocolRegistration, remove: bool) -> Result<()> {
    let mut pending = registry.clone();
    apply(registration, &mut pending, remove)?;
    *registry = pending;
    Ok(())
  }

  #[test]
  fn registration_quotes_unicode_paths_is_idempotent_and_removes_only_its_own_keys() {
    let receipt = receipt();
    let mut registry = Registry::default();
    transact(&mut registry, &receipt, false).unwrap();
    assert_eq!(
      registry.user["example-one"]["shell\\open\\command"][""],
      format!("\"{}\" --lintel-protocol \"%1\"", receipt.executable)
    );
    transact(&mut registry, &receipt, false).unwrap();
    transact(&mut registry, &receipt, true).unwrap();
    assert!(registry.user.is_empty());
    transact(&mut registry, &receipt, true).unwrap();
  }

  #[test]
  fn foreign_machine_or_modified_owned_keys_abort_the_entire_batch() {
    let receipt = receipt();
    let mut registry = Registry::default();
    registry.machine.insert("example-two".into());
    assert!(transact(&mut registry, &receipt, false).is_err());
    assert!(registry.user.is_empty());
    registry.machine.clear();
    transact(&mut registry, &receipt, false).unwrap();
    registry
      .user
      .get_mut("example-two")
      .unwrap()
      .get_mut("")
      .unwrap()
      .insert("foreign".into(), "preserve".into());
    let original = registry.user.clone();
    assert!(transact(&mut registry, &receipt, true).is_err());
    assert_eq!(registry.user, original);
    let mut other = receipt.clone();
    other.owner = "b".repeat(64);
    assert!(transact(&mut registry, &other, false).is_err());
    assert_eq!(registry.user, original);
  }

  #[test]
  fn selective_uninstall_removes_exact_owned_keys_and_preserves_foreign_edits_transactionally() {
    let receipt = receipt();
    let mut registry = Registry::default();
    transact(&mut registry, &receipt, false).unwrap();
    registry
      .user
      .get_mut("example-two")
      .unwrap()
      .get_mut("")
      .unwrap()
      .insert("foreign".into(), "keep".into());
    let foreign = registry.user["example-two"].clone();
    let result = remove_matching(&receipt, &mut registry).unwrap();
    assert_eq!(result.removed, ["example-one"]);
    assert_eq!(result.retained_foreign, ["example-two"]);
    assert!(!registry.user.contains_key("example-one"));
    assert_eq!(registry.user["example-two"], foreign);
    let result = remove_matching(&receipt, &mut registry).unwrap();
    assert_eq!(result.absent, ["example-one"]);
    assert_eq!(result.retained_foreign, ["example-two"]);
    let mut registry = Registry::default();
    transact(&mut registry, &receipt, false).unwrap();
    registry.operations = 0;
    registry.fail_at = Some(2);
    let mut pending = registry.clone();
    assert!(remove_matching(&receipt, &mut pending).is_err());
    assert_eq!(registry.user.len(), 2); // native caller commits only after the complete batch succeeds
  }

  #[test]
  fn write_failure_rolls_back_the_first_scheme_and_unsafe_commands_are_rejected() {
    let receipt = receipt();
    let mut registry = Registry {
      fail_at: Some(2),
      ..Registry::default()
    };
    assert!(transact(&mut registry, &receipt, false).is_err());
    assert!(registry.user.is_empty());
    for path in [
      r"relative.exe",
      r"C:\bad\app.exe:stream.exe",
      "C:\\app\"evil.exe",
      r"\\server\share\app.exe",
      r"C:\%USER%\app.exe",
    ] {
      let mut invalid = receipt.clone();
      invalid.executable = path.into();
      assert!(invalid.validate().is_err(), "{path}");
    }
    for scheme in ["http", "file", "ms-settings", "has space", "1bad"] {
      let mut invalid = receipt.clone();
      invalid.schemes = vec![scheme.into()];
      assert!(invalid.validate().is_err());
    }
  }
}
