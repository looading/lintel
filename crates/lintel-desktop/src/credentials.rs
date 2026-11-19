use zeroize::Zeroizing;

use crate::{Result, error::invalid, model::validate_text};

/// A secret string whose owned buffer is zeroed on drop and whose Debug output is redacted.
/// Callers must also protect any copies produced by `expose_secret`.
pub struct Secret(Zeroizing<String>);

impl Secret {
  pub fn new(value: String) -> Self {
    Self(Zeroizing::new(value))
  }
  pub fn expose_secret(&self) -> &str {
    &self.0
  }
}

impl std::fmt::Debug for Secret {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("Secret([REDACTED])")
  }
}

/// OS secure storage, never a plaintext or in-memory fallback. Operations can block or prompt for
/// access, so invoke them from a worker rather than the UI dispatch thread.
#[derive(Debug, Clone)]
pub struct CredentialStore {
  service: String,
}

impl CredentialStore {
  pub fn new(service: impl Into<String>) -> Result<Self> {
    let service = service.into();
    validate_text("credential service", &service, 256)?;
    if service.is_empty() {
      return Err(invalid("credential service", "must not be empty"));
    }
    Ok(Self { service })
  }

  #[cfg(all(
    feature = "credentials",
    any(target_os = "macos", target_os = "windows", target_os = "linux")
  ))]
  fn entry(&self, account: &str) -> Result<keyring::Entry> {
    validate_account(account)?;
    #[cfg(target_os = "linux")]
    {
      // Select persistent Secret Service explicitly so feature unification cannot silently
      // choose an ephemeral kernel keyring or a mock builder.
      let credential = keyring::secret_service::SsCredential::new_with_target(None, &self.service, account)
        .map_err(|error| crate::error::native("open Secret Service credential", error))?;
      Ok(keyring::Entry::new_with_credential(Box::new(credential)))
    }
    #[cfg(not(target_os = "linux"))]
    {
      keyring::Entry::new(&self.service, account).map_err(|e| crate::error::native("open credential entry", e))
    }
  }

  pub fn set(&self, account: &str, secret: &Secret) -> Result<()> {
    validate_account(account)?;
    #[cfg(all(
      feature = "credentials",
      any(target_os = "macos", target_os = "windows", target_os = "linux")
    ))]
    {
      self
        .entry(account)?
        .set_password(secret.expose_secret())
        .map_err(|e| crate::error::native("store credential", e))
    }
    #[cfg(not(all(
      feature = "credentials",
      any(target_os = "macos", target_os = "windows", target_os = "linux")
    )))]
    {
      let _ = secret;
      Err(unavailable())
    }
  }

  pub fn get(&self, account: &str) -> Result<Option<Secret>> {
    validate_account(account)?;
    #[cfg(all(
      feature = "credentials",
      any(target_os = "macos", target_os = "windows", target_os = "linux")
    ))]
    {
      match self.entry(account)?.get_password() {
        Ok(value) => Ok(Some(Secret::new(value))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(crate::error::native("read credential", error)),
      }
    }
    #[cfg(not(all(
      feature = "credentials",
      any(target_os = "macos", target_os = "windows", target_os = "linux")
    )))]
    {
      Err(unavailable())
    }
  }

  /// Returns false when the account had no credential.
  pub fn delete(&self, account: &str) -> Result<bool> {
    validate_account(account)?;
    #[cfg(all(
      feature = "credentials",
      any(target_os = "macos", target_os = "windows", target_os = "linux")
    ))]
    {
      match self.entry(account)?.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(error) => Err(crate::error::native("delete credential", error)),
      }
    }
    #[cfg(not(all(
      feature = "credentials",
      any(target_os = "macos", target_os = "windows", target_os = "linux")
    )))]
    {
      Err(unavailable())
    }
  }

  pub fn service(&self) -> &str {
    &self.service
  }
}

fn validate_account(account: &str) -> Result<()> {
  validate_text("credential account", account, 256)?;
  if account.is_empty() {
    return Err(invalid("credential account", "must not be empty"));
  }
  Ok(())
}

#[allow(dead_code)]
fn unavailable() -> crate::DesktopError {
  crate::error::unsupported(
    "secure credential storage",
    "enable credentials on macOS/Windows/Linux; Linux additionally needs an available Secret Service session",
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn secrets_are_redacted_and_invalid_names_fail_before_os_access() {
    assert_eq!(format!("{:?}", Secret::new("sensitive".into())), "Secret([REDACTED])");
    assert!(CredentialStore::new("").is_err());
    let store = CredentialStore::new("com.example.lintel.tests").unwrap();
    assert!(store.get("").is_err());
    assert!(store.delete("bad\0account").is_err());
  }
}
