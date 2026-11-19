//! Disposable real OS secure-store probe; never touches existing application service names.
use lintel_desktop::{CredentialStore, Secret};

struct Cleanup<'a> {
  store: &'a CredentialStore,
  account: &'a str,
  armed: bool,
}
impl Drop for Cleanup<'_> {
  fn drop(&mut self) {
    if self.armed {
      if let Err(error) = self.store.delete(self.account) {
        eprintln!(
          "Disposable credential cleanup failed: {error}; service namespace: {}",
          self.store.service()
        );
      }
    }
  }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  // This executable is a separate process, so the process-wide UI guard cannot affect the host app.
  #[cfg(target_os = "macos")]
  let _interaction_guard = security_framework::os::macos::keychain::SecKeychain::disable_user_interaction()?;
  let mut random = [0_u8; 32];
  getrandom::fill(&mut random)?;
  let unique: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
  let store = CredentialStore::new(format!(
    "org.lintel.disposable-keychain-probe.{}.{unique}",
    std::process::id()
  ))?;
  let account = "disposable-probe";
  if store.get(account)?.is_some() {
    return Err("random probe namespace already exists; refusing to modify it".into());
  }
  let mut cleanup = Cleanup {
    store: &store,
    account,
    armed: true,
  };
  let secret = Secret::new(unique);
  store.set(account, &secret)?;
  let retrieved = store.get(account)?.ok_or("probe credential was not persisted")?;
  if retrieved.expose_secret() != secret.expose_secret() {
    return Err("credential roundtrip did not match".into());
  }
  if !store.delete(account)? {
    return Err("probe credential disappeared before cleanup".into());
  }
  if store.get(account)?.is_some() {
    return Err("probe credential still exists after deletion".into());
  }
  cleanup.armed = false;
  println!("Disposable OS credential set/get/delete smoke passed; unique entry removed");
  Ok(())
}
