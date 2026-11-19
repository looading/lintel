use std::{
  any::{Any, TypeId},
  sync::Arc,
};

pub trait ServiceGetter: Send + Sync {
  fn call(&self) -> Box<dyn Any + Send + Sync>;
}

impl<F> ServiceGetter for F
where
  F: Fn() -> Box<dyn Any + Send + Sync> + Send + Sync + 'static,
{
  fn call(&self) -> Box<dyn Any + Send + Sync> {
    self()
  }
}
pub struct ServiceSlotInner(Arc<dyn ServiceGetter>);

impl ServiceSlotInner {
  pub fn new(service: Arc<dyn ServiceGetter>) -> Self {
    Self(service)
  }

  pub fn get_service_getter(&self) -> Option<Arc<dyn ServiceGetter>> {
    let a = self.0.clone();
    Some(a)
  }
}

#[derive(Clone)]
pub struct ServiceSlot(Arc<ServiceSlotInner>);

impl ServiceSlot {
  pub fn new(service: Arc<dyn ServiceGetter>) -> Self {
    Self(Arc::new(ServiceSlotInner::new(service)))
  }
}

impl ServiceSlot {
  pub fn get_service<S>(&self) -> Option<Arc<S>>
  where
    S: Send + Sync + 'static + ?Sized,
  {
    let service_getter = self.0.get_service_getter()?;
    let service = service_getter.call();
    let service = service.downcast::<Arc<S>>().ok()?;
    Some(*service)
  }
}

#[cfg(test)]
mod tests {
  use std::{
    any::{Any, TypeId},
    collections::BTreeMap,
    sync::Arc,
  };

  use super::ServiceSlot;

  struct People {
    name: String,
  }

  trait SpotApi: Send + Sync {
    fn jump(&self);
  }

  trait AttrApi: Send + Sync {
    fn info(&self);
  }

  impl SpotApi for People {
    fn jump(&self) {}
  }

  impl AttrApi for People {
    fn info(&self) {}
  }

  #[test]
  fn it_works() {
    let mut manager = BTreeMap::<TypeId, ServiceSlot>::new();
    let p = Arc::new(People {
      name: "bob".to_string(),
    });

    // let getter = move || -> Arc<dyn SpotApi> { p.clone() };

    fn register<F, S>(f: F) -> ServiceSlot
    where
      F: Fn() -> Arc<S> + Send + Sync + 'static,
      S: Send + Sync + ?Sized + 'static,
    {
      let handler = Arc::new(move || -> Box<dyn Any + Send + Sync> { Box::new(f()) });
      let slot = ServiceSlot::new(handler);
      slot
    }

    let slot = register(move || -> Arc<dyn SpotApi> { p.clone() });
    // let slot3 = ServiceSlot::new(p.clone());
    manager.insert(TypeId::of::<dyn SpotApi>(), slot);
    // manager.insert(TypeId::of::<dyn AttrApi>(), slot3);

    let slot = manager.get(&TypeId::of::<dyn SpotApi>()).unwrap();
    if let Some(ret) = slot.get_service::<dyn SpotApi>() {
      ret.jump();
    } else {
      panic!("SpotApi service not found");
    }
  }
}
