use anyhow::Result;
use std::{
  any::{Any, TypeId, type_name},
  borrow::Cow,
  collections::{BTreeMap, HashMap, btree_map::Entry},
  sync::{Arc, Mutex, RwLock},
};

use crate::context::service::ServiceSlot;

pub trait Resource: Any + Send + Sync {
  fn name(&self) -> Cow<'_, str> {
    type_name::<Self>().into()
  }
  fn release(self: Arc<Self>) {}
}

impl<T> Resource for T where T: Any + Send + Sync {}

impl dyn Resource {
  #[inline(always)]
  fn is<T: Resource>(&self) -> bool {
    self.type_id() == TypeId::of::<T>()
  }

  #[inline(always)]
  pub(crate) fn downcast_arc<T: Resource>(self: Arc<Self>) -> Option<Arc<T>> {
    let resource: Arc<dyn Any + Send + Sync> = self;
    resource.downcast::<T>().ok()
  }
}

#[derive(Debug, thiserror::Error)]
pub enum ResourceError {
  #[error("resource: {0} not found")]
  ResourceNotFound(ResourceId),
  #[error("resource type not match")]
  ResourceTypeNotMatch,
}

pub type ResourceId = i32;

pub struct AppResources(Mutex<BTreeMap<ResourceId, Arc<dyn Resource>>>);

impl AppResources {
  pub fn create_resource_id() -> ResourceId {
    rand::random()
  }
  pub fn new() -> Self {
    Self(Mutex::new(BTreeMap::new()))
  }

  pub fn insert<T: Resource>(&self, resource: T) -> ResourceId {
    self.insert_from_id(Self::create_resource_id(), resource)
  }

  fn insert_from_id<T: Resource>(&self, initial: ResourceId, resource: T) -> ResourceId {
    let resource: Arc<dyn Resource> = Arc::new(resource);
    let mut resources = self.0.lock().unwrap_or_else(|error| error.into_inner());
    let mut id = initial;
    loop {
      if let Entry::Vacant(entry) = resources.entry(id) {
        entry.insert(resource);
        return id;
      }
      // 在同一临界区内探测空位，随机候选 ID 冲突时不能覆盖已有资源。
      id = id.wrapping_add(1);
      if id == initial {
        drop(resources);
        panic!("resource ID space is exhausted");
      }
    }
  }

  pub fn get<T: Resource>(&self, id: ResourceId) -> Result<Arc<T>, ResourceError> {
    let resource = self
      .0
      .lock()
      .unwrap_or_else(|error| error.into_inner())
      .get(&id)
      .cloned()
      .ok_or(ResourceError::ResourceNotFound(id))?;
    resource.downcast_arc::<T>().ok_or(ResourceError::ResourceTypeNotMatch)
  }

  pub fn take<T: Resource>(&self, id: ResourceId) -> Result<Arc<T>, ResourceError> {
    let resource = {
      let mut resources = self.0.lock().unwrap_or_else(|error| error.into_inner());
      let resource = resources.get(&id).ok_or(ResourceError::ResourceNotFound(id))?;
      if !resource.as_ref().is::<T>() {
        return Err(ResourceError::ResourceTypeNotMatch);
      }
      // 类型检查与取出必须原子完成，错误类型不得移除资源。
      resources.remove(&id).ok_or(ResourceError::ResourceNotFound(id))?
    };
    resource.downcast_arc::<T>().ok_or(ResourceError::ResourceTypeNotMatch)
  }

  pub fn replace<T: Resource>(&self, id: ResourceId, resource: T) {
    let replacement: Arc<dyn Resource> = Arc::new(resource);
    let previous = {
      let mut resources = self.0.lock().unwrap_or_else(|error| error.into_inner());
      if !resources.contains_key(&id) {
        drop(resources);
        panic!("cannot replace missing resource {id}");
      }
      resources.insert(id, replacement)
    };
    // 旧值的析构可能再次访问资源表。
    drop(previous);
  }

  pub fn remove(&self, id: ResourceId) -> Result<Arc<dyn Resource>, ResourceError> {
    let resource = self.0.lock().unwrap_or_else(|error| error.into_inner()).remove(&id);
    resource.ok_or(ResourceError::ResourceNotFound(id))
  }

  pub fn release(&self, id: ResourceId) -> Result<(), ResourceError> {
    let resource = self.remove(id)?;
    resource.release();
    Ok(())
  }

  pub fn clear(&self) {
    let resources = std::mem::take(&mut *self.0.lock().unwrap_or_else(|error| error.into_inner()));
    // 资源析构可能再次访问上下文，必须先释放锁。
    drop(resources);
  }
}

pub struct ServiceStore {
  pub services: RwLock<HashMap<TypeId, ServiceSlot>>,
}

impl ServiceStore {
  pub fn new() -> Self {
    Self {
      services: RwLock::new(HashMap::new()),
    }
  }
}

impl ServiceStore {
  pub fn clear(&self) {
    let services = std::mem::take(&mut *self.services.write().unwrap_or_else(|error| error.into_inner()));
    drop(services);
  }

  pub fn register_service<F, S>(&self, service_getter: F)
  where
    F: Fn() -> Arc<S> + Send + Sync + 'static,
    S: Send + Sync + ?Sized + 'static,
  {
    fn register<F, S>(f: F) -> ServiceSlot
    where
      F: Fn() -> Arc<S> + Send + Sync + 'static,
      S: Send + Sync + ?Sized + 'static,
    {
      let handler = Arc::new(move || -> Box<dyn Any + Send + Sync> { Box::new(f()) });
      let slot = ServiceSlot::new(handler);
      slot
    }

    let slot = register(service_getter);
    let type_id = TypeId::of::<S>();
    let previous = self.services.write().unwrap().insert(type_id, slot);
    drop(previous);
  }

  pub fn get_service<S>(&self) -> Option<Arc<S>>
  where
    S: Send + Sync + ?Sized + 'static,
  {
    let type_id = TypeId::of::<S>();
    let service_slot = self.services.read().unwrap().get(&type_id)?.clone();
    // getter 可能注册其他服务，调用时不能持有服务表的读锁。
    let service = service_slot.get_service::<S>()?;
    Some(service)
  }

  pub fn remove_service<S>(&self)
  where
    S: Send + Sync + ?Sized + 'static,
  {
    let type_id = TypeId::of::<S>();
    let previous = self.services.write().unwrap().remove(&type_id);
    drop(previous);
  }
}

#[cfg(test)]
mod tests {
  use std::{
    collections::BTreeSet,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
      Arc, Barrier, Weak,
      atomic::{AtomicBool, AtomicUsize, Ordering},
      mpsc,
    },
    thread,
    time::Duration,
  };

  use super::{AppResources, ResourceError};

  #[derive(Debug)]
  struct Test {
    pub name: i32,
  }

  struct A {}
  #[test]
  fn it_works() {
    let mut resources = super::AppResources::new();
    let r_1 = resources.insert(Test { name: 1 });
    let r_2 = resources.insert(Test { name: 2 });

    let a = resources.get::<Test>(r_1).unwrap();
    // let b:  = resources.get(r_2);
    let b = resources.get::<Test>(r_2).unwrap();

    assert_eq!(a.name, 1);
    assert_eq!(b.name, 2);
  }

  #[test]
  fn resource_id_collision_probes_an_empty_slot_without_overwriting_and_wraps_safely() {
    let resources = AppResources::new();
    let first = resources.insert_from_id(i32::MAX, "first".to_owned());
    let second = resources.insert_from_id(i32::MAX, "second".to_owned());
    assert_eq!(first, i32::MAX);
    assert_eq!(second, i32::MIN);
    assert_eq!(&*resources.get::<String>(first).unwrap(), "first");
    assert_eq!(&*resources.get::<String>(second).unwrap(), "second");
  }

  #[test]
  fn resource_operations_wait_for_contention_instead_of_panicking() {
    let resources = Arc::new(AppResources::new());
    let guard = resources.0.lock().unwrap();
    let (started, start) = mpsc::channel();
    let (finished, finish) = mpsc::channel();
    let worker_resources = resources.clone();
    let worker = thread::spawn(move || {
      started.send(()).unwrap();
      let id = worker_resources.insert(42_u32);
      finished.send(id).unwrap();
    });
    start.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(matches!(
      finish.recv_timeout(Duration::from_millis(50)),
      Err(mpsc::RecvTimeoutError::Timeout)
    ));
    drop(guard);
    let id = finish.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.join().unwrap();
    assert_eq!(*resources.get::<u32>(id).unwrap(), 42);
  }

  #[test]
  fn concurrent_resource_inserts_and_gets_keep_every_live_id_unique() {
    let resources = Arc::new(AppResources::new());
    let barrier = Arc::new(Barrier::new(8));
    let workers: Vec<_> = (0..8_u32)
      .map(|worker| {
        let resources = resources.clone();
        let barrier = barrier.clone();
        thread::spawn(move || {
          barrier.wait();
          (0..100)
            .map(|index| {
              let value = worker * 100 + index;
              let id = resources.insert(value);
              assert_eq!(*resources.get::<u32>(id).unwrap(), value);
              (id, value)
            })
            .collect::<Vec<_>>()
        })
      })
      .collect();
    let values: Vec<_> = workers.into_iter().flat_map(|worker| worker.join().unwrap()).collect();
    assert_eq!(values.iter().map(|(id, _)| *id).collect::<BTreeSet<_>>().len(), 800);
    for (id, value) in values {
      assert_eq!(*resources.take::<u32>(id).unwrap(), value);
    }
    assert!(resources.0.lock().unwrap().is_empty());
  }

  #[test]
  fn concurrent_typed_take_has_exactly_one_owner() {
    let resources = Arc::new(AppResources::new());
    let id = resources.insert(73_u32);
    let barrier = Arc::new(Barrier::new(16));
    let workers: Vec<_> = (0..16)
      .map(|_| {
        let resources = resources.clone();
        let barrier = barrier.clone();
        thread::spawn(move || {
          barrier.wait();
          resources.take::<u32>(id)
        })
      })
      .collect();
    let mut owners = 0;
    for worker in workers {
      match worker.join().unwrap() {
        Ok(value) => {
          owners += 1;
          assert_eq!(*value, 73);
        }
        Err(ResourceError::ResourceNotFound(missing)) => assert_eq!(missing, id),
        Err(error) => panic!("unexpected take error: {error}"),
      }
    }
    assert_eq!(owners, 1);
  }

  #[test]
  fn typed_take_and_get_preserve_a_wrong_type_and_keep_arc_identity() {
    let resources = AppResources::new();
    let id = resources.insert("retained".to_owned());
    assert!(matches!(
      resources.get::<u32>(id),
      Err(ResourceError::ResourceTypeNotMatch)
    ));
    assert!(matches!(
      resources.take::<u32>(id),
      Err(ResourceError::ResourceTypeNotMatch)
    ));
    let first = resources.get::<String>(id).unwrap();
    let second = resources.take::<String>(id).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(&*second, "retained");
    assert!(matches!(
      resources.get::<String>(id),
      Err(ResourceError::ResourceNotFound(_))
    ));
  }

  struct ReentrantResource {
    resources: Weak<AppResources>,
    unlocked: Arc<AtomicBool>,
    dropped: Arc<AtomicUsize>,
  }

  impl Drop for ReentrantResource {
    fn drop(&mut self) {
      if let Some(resources) = self.resources.upgrade() {
        let unlocked = resources.0.try_lock().is_ok();
        self.unlocked.store(unlocked, Ordering::SeqCst);
        if unlocked {
          // 实际重入公开 API；若仍持锁，则记录失败而不让回归测试死锁。
          let id = resources.insert(99_u8);
          assert_eq!(*resources.get::<u8>(id).unwrap(), 99);
          drop(resources.remove(id).unwrap());
        }
      }
      self.dropped.fetch_add(1, Ordering::SeqCst);
    }
  }

  fn reentrant_resource(resources: &Arc<AppResources>) -> (ReentrantResource, Arc<AtomicBool>, Arc<AtomicUsize>) {
    let unlocked = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicUsize::new(0));
    (
      ReentrantResource {
        resources: Arc::downgrade(resources),
        unlocked: unlocked.clone(),
        dropped: dropped.clone(),
      },
      unlocked,
      dropped,
    )
  }

  #[test]
  fn release_replace_remove_and_clear_drop_resources_outside_the_lock() {
    for operation in ["release", "replace", "remove", "clear"] {
      let resources = Arc::new(AppResources::new());
      let (resource, unlocked, dropped) = reentrant_resource(&resources);
      let id = resources.insert(resource);
      match operation {
        "release" => resources.release(id).unwrap(),
        "replace" => resources.replace(id, 1_u32),
        "remove" => drop(resources.remove(id).unwrap()),
        "clear" => resources.clear(),
        _ => unreachable!(),
      }
      assert!(
        unlocked.load(Ordering::SeqCst),
        "{operation} destroyed a resource under the lock"
      );
      assert_eq!(dropped.load(Ordering::SeqCst), 1);
      if operation == "replace" {
        assert_eq!(*resources.get::<u32>(id).unwrap(), 1);
      }
    }
  }

  #[test]
  fn replacing_an_unknown_id_does_not_insert_or_poison_and_drops_the_input_unlocked() {
    let resources = Arc::new(AppResources::new());
    let existing = resources.insert_from_id(11, 42_u32);
    let (resource, unlocked, dropped) = reentrant_resource(&resources);
    assert!(catch_unwind(AssertUnwindSafe(|| resources.replace(12, resource))).is_err());
    assert!(unlocked.load(Ordering::SeqCst));
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    assert!(!resources.0.is_poisoned());
    assert_eq!(*resources.get::<u32>(existing).unwrap(), 42);
    assert!(matches!(
      resources.get::<ReentrantResource>(12),
      Err(ResourceError::ResourceNotFound(_))
    ));
  }

  struct People {
    name: String,
  }

  trait SpotApi: Send + Sync {
    fn jump(&self);
  }

  impl SpotApi for People {
    fn jump(&self) {}
  }

  #[test]
  fn test_service_store() {
    let store = super::ServiceStore::new();
    let p = Arc::new(People {
      name: "test".to_string(),
    });
    let p = p.clone();
    store.register_service(move || -> Arc<dyn SpotApi> { p.clone() });
    let a = store.get_service::<dyn SpotApi>();
    if let Some(a) = a.as_ref() {
      a.jump();
    } else {
      panic!("SpotApi service not found");
    }

    store.remove_service::<dyn SpotApi>();

    let c = store.get_service::<dyn SpotApi>();
    assert!(c.is_none());

    if let Some(a) = a {
      a.jump();
    } else {
      panic!("SpotApi service not found");
    }
  }

  #[test]
  fn service_getters_can_register_other_services_without_holding_the_table_lock() {
    let store = Arc::new(super::ServiceStore::new());
    let weak = Arc::downgrade(&store);
    store.register_service(move || {
      let store = weak.upgrade().unwrap();
      assert!(
        store.services.try_write().is_ok(),
        "getter was invoked under the service table lock"
      );
      store.register_service(|| Arc::new(17_u32));
      Arc::new("ready".to_owned())
    });
    assert_eq!(&*store.get_service::<String>().unwrap(), "ready");
    assert_eq!(*store.get_service::<u32>().unwrap(), 17);
  }
}
