use std::sync::{Arc, Mutex};

use lintel_lib_event::{EventCallback, EventManager, EventPayload};
use serde::{Deserialize, Serialize};
use strum::IntoStaticStr;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvType {
  Development,
  Staging,
  #[default]
  Production,
}

impl EnvType {
  pub fn is_production(&self) -> bool {
    EnvType::Production == *self
  }

  pub fn is_development(&self) -> bool {
    EnvType::Development == *self
  }
  pub fn is_staging(&self) -> bool {
    EnvType::Staging == *self
  }
}

#[derive(Debug, Clone)]
pub struct Env {
  env_type: Arc<Mutex<EnvType>>,
  event: EventManager,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr)]
pub enum EnvEvent {
  Changed,
}

impl EventPayload for EnvType {
  fn get_data(&self) -> Box<dyn std::any::Any> {
    Box::new(*self)
  }
}

impl Env {
  pub fn new(env_type: EnvType) -> Self {
    Self {
      env_type: Arc::new(Mutex::new(env_type)),
      event: EventManager::new(),
    }
  }

  pub fn get(&self) -> EnvType {
    *self.env_type.lock().unwrap()
  }

  pub fn update(&self, env_type: EnvType) {
    let mut lock = self.env_type.lock().unwrap();
    if *lock != env_type {
      *lock = env_type;
      // 提前释放
      drop(lock);
      self.event.emit(EnvEvent::Changed.into(), Arc::new(env_type));
    }
  }

  pub fn on_changed(&self, callback: EventCallback) -> Uuid {
    self.event.on(EnvEvent::Changed.into(), callback)
  }

  pub fn off_changed(&self, id: &Uuid) {
    self.event.off(EnvEvent::Changed.into(), id);
  }
}
lazy_static::lazy_static! {
  pub static ref ENV: Env = Env::new(EnvType::Production);
}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::LazyLock<Mutex<()>> = std::sync::LazyLock::new(|| Mutex::new(()));

#[cfg(test)]
mod tests {
  use std::sync::atomic::{AtomicUsize, Ordering};

  use lintel_lib_event::EventCallbackImpl;

  use super::*;

  pub struct EnvChangedCallback {
    calls: Arc<AtomicUsize>,
  }

  impl EventCallbackImpl for EnvChangedCallback {
    fn call(&self, payload: Arc<dyn EventPayload>) -> anyhow::Result<Box<dyn std::any::Any>> {
      let data = payload.get_data().downcast::<EnvType>().unwrap();
      assert_eq!(*data, EnvType::Development);
      self.calls.fetch_add(1, Ordering::Relaxed);
      Ok(Box::new(false))
    }
  }

  #[test]
  fn test_env() {
    let _guard = TEST_ENV_LOCK.lock().unwrap();
    ENV.update(EnvType::Production);
    let env = ENV.get();
    assert_eq!(env, EnvType::Production);

    let calls = Arc::new(AtomicUsize::new(0));
    let callback_id = ENV.on_changed(EventCallback::new(Arc::new(EnvChangedCallback {
      calls: calls.clone(),
    })));

    ENV.update(EnvType::Development);
    assert_eq!(ENV.get(), EnvType::Development);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    {
      let env = ENV.get();
      assert_eq!(env, EnvType::Development);
    }

    ENV.update(EnvType::Development);
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    ENV.off_changed(&callback_id);
    ENV.update(EnvType::Staging);
    assert_eq!(ENV.get(), EnvType::Staging);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(ENV.get().is_staging());
    assert!(!ENV.get().is_production());
    assert!(!ENV.get().is_development());

    ENV.update(EnvType::Production);
  }

  #[test]
  fn test_env_type_helpers() {
    assert!(EnvType::Production.is_production());
    assert!(!EnvType::Production.is_development());
    assert!(!EnvType::Production.is_staging());
    assert!(EnvType::Development.is_development());
    assert!(EnvType::Staging.is_staging());
    assert_eq!(EnvType::default(), EnvType::Production);
  }

  #[test]
  fn environment_names_have_generic_serialization() {
    for (environment, name) in [
      (EnvType::Development, "development"),
      (EnvType::Staging, "staging"),
      (EnvType::Production, "production"),
    ] {
      assert_eq!(serde_yml::from_str::<EnvType>(name).unwrap(), environment);
      assert_eq!(serde_yml::to_string(&environment).unwrap().trim(), name);
    }
  }

  #[test]
  fn test_env_off_changed_unregisters_callback() {
    let env = Env::new(EnvType::Production);
    let calls = Arc::new(AtomicUsize::new(0));
    let id = env.on_changed(EventCallback::new(Arc::new(EnvChangedCallback {
      calls: calls.clone(),
    })));

    env.off_changed(&id);
    env.update(EnvType::Staging);

    assert_eq!(env.get(), EnvType::Staging);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
  }
}
