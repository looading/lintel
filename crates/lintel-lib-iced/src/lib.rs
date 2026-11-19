use std::hash::Hasher;
use std::{
  any::{Any, type_name},
  fmt::Debug,
  hash::Hash,
  sync::Arc,
};

pub mod ui_perf;

pub use iced::{self, *};
pub use iced_core;
#[cfg(feature = "debug")]
pub use iced_debug;
pub use iced_runtime::Action;
pub use iced_wgpu;
pub use iced_winit;

#[derive(Debug, Clone)]
pub enum IcedAppEvent {
  NewProgram(Arc<Box<dyn IcedApplication>>),
  CloseProgram(ProgramId),
  Action(Arc<dyn AnyMessage>),
}

impl Debug for dyn IcedApplication {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_tuple("IceApplication").field(&self.__id()).finish()
  }
}

pub trait ProgramIdOfMessage {
  fn program_id(&self) -> ProgramId;
}

pub trait AnyMessage: ProgramIdOfMessage + std::any::Any + std::fmt::Debug + Send + Sync + 'static {}

impl<T> AnyMessage for T where T: ProgramIdOfMessage + std::any::Any + std::fmt::Debug + Send + Sync + 'static {}

pub fn downcast_message<T: AnyMessage>(msg: Arc<dyn AnyMessage>) -> std::result::Result<T, ()> {
  // Upcast to &dyn Any (trait upcasting, stable since Rust 1.86) to avoid Arc::clone.
  let any_ref: &dyn Any = &*msg;
  if any_ref.is::<T>() {
    let ptr = Arc::into_raw(msg).cast::<T>();
    unsafe { Arc::into_inner(Arc::from_raw(ptr)) }.ok_or(())
  } else {
    debug_assert!(false, "cannot downcast message to {}", std::any::type_name::<T>());
    Err(())
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProgramId(&'static str, u64);

impl ProgramId {
  pub fn new(name: &'static str, hash: u64) -> Self {
    Self(name, hash)
  }

  pub fn name(&self) -> &'static str {
    self.0
  }
}
fn compute_program_id<T: ?Sized + 'static>() -> ProgramId {
  let mut hasher = std::hash::DefaultHasher::new();
  std::any::TypeId::of::<T>().hash(&mut hasher);
  let name = type_name::<T>();
  name.hash(&mut hasher);
  ProgramId(name, hasher.finish())
}

pub trait IcedApplication: Send + Sync + 'static {
  fn id() -> ProgramId
  where
    Self: Sized,
  {
    compute_program_id::<Self>()
  }

  fn __id(&self) -> ProgramId {
    compute_program_id::<Self>()
  }

  fn boot(&mut self) -> Task<Arc<dyn AnyMessage>> {
    Task::none()
  }

  fn window_id(&self) -> Option<window::Id>;

  /// Lists every window owned by this program. Single-window programs need no override.
  fn window_ids(&self) -> Vec<window::Id> {
    self.window_id().into_iter().collect()
  }

  /// Routes a native window to its owning program, including windows still opening.
  fn owns_window(&self, id: window::Id) -> bool {
    self.window_id() == Some(id)
  }

  /// Returns the optional title of the application's window.
  fn window_title(&self) -> Option<&str> {
    None
  }

  /// Returns the title of an individual managed window.
  fn window_title_for(&self, _id: window::Id) -> Option<String> {
    self.window_title().map(ToOwned::to_owned)
  }

  // view content
  fn view(&self, id: window::Id) -> Element<'_, Arc<dyn AnyMessage>>;

  // update program state
  fn update(&mut self, message: Arc<dyn AnyMessage>) -> Task<Arc<dyn AnyMessage>>;

  // subscribe outer events
  fn subscription(&self) -> Subscription<Arc<dyn AnyMessage>> {
    Subscription::none()
  }
}

#[inline]
pub fn bold_font() -> iced::Font {
  #[cfg(target_os = "macos")]
  return iced::Font {
    family: iced::font::Family::Name("PingFang SC"),
    weight: iced::font::Weight::Bold,
    ..Default::default()
  };

  #[cfg(target_os = "windows")]
  {
    #[cfg(not(target_vendor = "win7"))]
    let font_family = "Microsoft YaHei UI";
    #[cfg(target_vendor = "win7")]
    let font_family = "Microsoft YaHei";

    return iced::Font {
      family: iced::font::Family::Name(font_family),
      weight: iced::font::Weight::Bold,
      ..Default::default()
    };
  }

  #[cfg(not(any(target_os = "macos", target_os = "windows")))]
  return iced::Font {
    weight: iced::font::Weight::Semibold,
    ..iced::Font::DEFAULT
  };
}

#[inline]
pub fn normal_font() -> iced::Font {
  #[cfg(target_os = "macos")]
  return iced::Font {
    family: iced::font::Family::Name("PingFang SC"),
    ..Default::default()
  };

  #[cfg(target_os = "windows")]
  {
    #[cfg(not(target_vendor = "win7"))]
    let font_family = "Microsoft YaHei UI";
    #[cfg(target_vendor = "win7")]
    let font_family = "Microsoft YaHei";

    return iced::Font {
      family: iced::font::Family::Name(font_family),
      ..Default::default()
    };
  }

  #[cfg(not(any(target_os = "macos", target_os = "windows")))]
  return iced::Font::DEFAULT;
}

#[cfg(test)]
mod tests {
  use super::*;

  #[derive(Debug, PartialEq, Eq)]
  struct TestMessage {
    value: u32,
  }

  impl ProgramIdOfMessage for TestMessage {
    fn program_id(&self) -> ProgramId {
      ProgramId::new("test_message", 1)
    }
  }

  struct TestApp;

  impl IcedApplication for TestApp {
    fn window_id(&self) -> Option<window::Id> {
      None
    }

    fn view(&self, _id: window::Id) -> Element<'_, Arc<dyn AnyMessage>> {
      widget::space::horizontal().into()
    }

    fn update(&mut self, _message: Arc<dyn AnyMessage>) -> Task<Arc<dyn AnyMessage>> {
      Task::none()
    }
  }

  #[test]
  fn test_downcast_message_returns_owned_message() {
    let message: Arc<dyn AnyMessage> = Arc::new(TestMessage { value: 42 });

    let downcasted = downcast_message::<TestMessage>(message).unwrap();

    assert_eq!(downcasted, TestMessage { value: 42 });
  }

  #[test]
  fn test_program_id_uses_static_name_and_hash() {
    let program_id = ProgramId::new("demo::program", 7);

    assert_eq!(program_id.name(), "demo::program");
    assert_eq!(program_id, ProgramId::new("demo::program", 7));
  }

  #[test]
  fn test_iced_application_default_identity_and_subscription() {
    let mut app = TestApp;
    let app_ref: &dyn IcedApplication = &app;
    let id = app.__id();

    assert_eq!(TestApp::id(), id);
    assert_eq!(id.name(), std::any::type_name::<TestApp>());
    assert!(format!("{app_ref:?}").contains("IceApplication"));

    let _ = app.boot();
    let _ = app.subscription();
  }

  #[test]
  fn test_font_helpers_return_platform_fonts() {
    let normal = normal_font();
    let bold = bold_font();

    assert_eq!(bold.weight, iced::font::Weight::Bold);
    #[cfg(target_os = "macos")]
    {
      assert!(matches!(normal.family, iced::font::Family::Name("PingFang SC")));
      assert!(matches!(bold.family, iced::font::Family::Name("PingFang SC")));
    }
    #[cfg(target_os = "windows")]
    {
      assert!(matches!(
        normal.family,
        iced::font::Family::Name("Microsoft YaHei UI") | iced::font::Family::Name("Microsoft YaHei")
      ));
    }
  }
}
