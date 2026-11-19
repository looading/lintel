use std::{any::Any, collections::HashMap, fmt::Debug, sync::Arc};

use parking_lot::Mutex;

use anyhow::Result;

use uuid::Uuid;

pub trait EventPayload: Send + Sync {
  fn get_data(&self) -> Box<dyn Any>;
}

pub trait EventCallbackImpl: Send + Sync {
  fn call(&self, payload: Arc<dyn EventPayload>) -> Result<Box<dyn Any>>;
}

#[derive(Clone)]
pub struct EventCallback {
  callback: Arc<dyn EventCallbackImpl>,
  id: Uuid,
}

impl EventCallback {
  pub fn new(cb: Arc<dyn EventCallbackImpl>) -> Self {
    Self {
      callback: cb,
      id: Uuid::new_v4(),
    }
  }

  pub fn call(&self, payload: Arc<dyn EventPayload>) -> Result<Box<dyn Any>> {
    self.callback.call(payload)
  }

  pub fn get_event_id(&self) -> &Uuid {
    &self.id
  }
}

/// 支持多线程并发调用
#[derive(Clone)]
pub struct EventManager {
  map: Arc<Mutex<HashMap<String, HashMap<Uuid, EventCallback>>>>,
}

impl Debug for EventManager {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("EventManager").finish()
  }
}

impl EventManager {
  #[allow(clippy::new_without_default)]
  pub fn new() -> Self {
    Self {
      map: Arc::new(Mutex::new(HashMap::new())),
    }
  }

  pub fn emit(&self, event_name: &str, payload: Arc<dyn EventPayload>) -> Vec<Result<Box<dyn Any>>> {
    let mut res = vec![];
    // 及时释放，否则可能死锁（如果 callback 内 移除事件）
    let event_map = {
      let map = self.map.lock();
      map.get(event_name).cloned()
    };

    if let Some(event_map) = event_map {
      for (_, event) in event_map.iter() {
        res.push(event.clone().call(payload.clone()));
      }
    }
    res
  }

  pub fn on(&self, event_name: &str, callback: EventCallback) -> Uuid {
    let mut map = self.map.lock();
    let event_map = map.entry(event_name.to_string()).or_default();
    let id = *callback.get_event_id();
    event_map.insert(id, callback);
    id
  }

  pub fn off(&self, event_name: &str, event_id: &Uuid) {
    let mut map = self.map.lock();
    if let Some(event_map) = map.get_mut(event_name) {
      event_map.remove(event_id);
    }
  }

  pub fn off_all(&self, event_name: Option<&str>) {
    match event_name {
      Some(event_name) => {
        let mut map = self.map.lock();
        map.remove(event_name);
      }
      None => {
        let mut map = self.map.lock();
        map.clear();
      }
    }
  }

  pub fn get_listeners_count(&self, event_name: &str) -> usize {
    let map = self.map.lock();
    if let Some(event_map) = map.get(event_name) {
      event_map.len()
    } else {
      0
    }
  }

  pub fn get_all_listeners_count(&self) -> usize {
    let map = self.map.lock();
    map.values().map(|v| v.len()).sum()
  }
}

#[cfg(test)]
mod tests {
  use std::{any::Any, mem, sync::Arc};

  use crate::{EventCallback, EventCallbackImpl, EventManager, EventPayload};
  use anyhow::Result;

  struct TestEventCallback {}

  impl EventCallbackImpl for TestEventCallback {
    fn call(&self, payload: Arc<dyn EventPayload>) -> Result<Box<dyn Any>> {
      Ok(Box::new(123123))
    }
  }

  #[derive(Clone)]
  struct TestEventPaylod {}

  impl EventPayload for TestEventPaylod {
    fn get_data(&self) -> Box<dyn Any> {
      Box::new(self.clone())
    }
  }

  #[derive(Clone)]
  struct Test2EventPaylod {
    id: i32,
  }
  impl EventPayload for Test2EventPaylod {
    fn get_data(&self) -> Box<dyn Any> {
      Box::new(self.clone())
    }
  }

  struct Test2EventCallback {
    name: String,
  }

  impl EventCallbackImpl for Test2EventCallback {
    fn call(&self, payload: Arc<dyn EventPayload>) -> Result<Box<dyn Any>> {
      let data = payload.get_data().downcast::<Test2EventPaylod>().unwrap();

      Ok(Box::new(123123))
    }
  }

  #[test]
  fn test_() {
    let manager = EventManager::new();

    let id = manager.on("test", EventCallback::new(Arc::new(TestEventCallback {})));

    assert_eq!(manager.get_listeners_count("test"), 1);

    let ret = manager.emit("test", Arc::new(TestEventPaylod {}));

    let ret = ret
      .iter()
      .map(|f| {
        f.as_ref().ok().and_then(|r| {
          let ret = r.downcast_ref::<i32>();
          ret
        })
      })
      .filter(|c| c.is_some())
      .collect::<Vec<Option<&i32>>>();

    manager.off("test", &id);
    assert_eq!(manager.get_listeners_count("test"), 0);
    assert_eq!(manager.get_listeners_count("test"), 0);

    let ret = manager.emit("31313", Arc::new(TestEventPaylod {}));

    println!("{:?}", ret);
  }

  #[test]
  fn test_remove() {
    let manager = EventManager::new();

    let id = manager.on(
      "test_1",
      EventCallback::new(Arc::new(Test2EventCallback {
        name: "Callback_A".into(),
      })),
    );
    let id_1 = manager.on(
      "test_1",
      EventCallback::new(Arc::new(Test2EventCallback {
        name: "Callback_B".into(),
      })),
    );

    let id_2 = manager.on(
      "test_2",
      EventCallback::new(Arc::new(Test2EventCallback { name: "test_2".into() })),
    );

    manager.emit("test_1", Arc::new(Test2EventPaylod { id: 1 }));
    manager.emit("test_2", Arc::new(Test2EventPaylod { id: 1 }));

    println!("remove test1 event");
    manager.off_all(Some("test_1"));
    manager.emit("test_1", Arc::new(Test2EventPaylod { id: 1 }));
    manager.emit("test_2", Arc::new(Test2EventPaylod { id: 1 }));

    println!("remove all event");
    manager.off_all(None);
    manager.emit("test_1", Arc::new(Test2EventPaylod { id: 1 }));
    manager.emit("test_2", Arc::new(Test2EventPaylod { id: 1 }));
  }
}
