//! 将公开 winit 尚未提供的 Dock Reopen 回调接入既有激活队列。
use std::{
  cell::RefCell,
  collections::VecDeque,
  ffi::CString,
  panic::{AssertUnwindSafe, catch_unwind},
  rc::{Rc, Weak},
  sync::Arc,
};

use anyhow::{Context, Result, ensure};
use objc2::{
  Encode, MainThreadMarker,
  runtime::{AnyObject, Bool, Imp, Sel},
  sel,
};
use objc2_app_kit::NSApplication;

const CAPACITY: usize = 64;
type Wake = Arc<dyn Fn() + Send + Sync>;

thread_local! {
  static SUBSCRIBER: RefCell<Weak<State>> = const { RefCell::new(Weak::new()) };
}

struct State {
  delegate: usize,
  events: RefCell<VecDeque<bool>>,
  wake: Wake,
}

impl State {
  fn receive(&self, has_visible_window: bool) -> bool {
    {
      let mut events = self.events.borrow_mut();
      if events.len() == CAPACITY {
        return false;
      }
      events.push_back(has_visible_window);
    }
    // 不在队列借用期间执行唤醒，回调可以安全地重新进入事件处理。
    (self.wake)();
    true
  }
}

/// 注册随当前运行时释放的主线程订阅；不替换 delegate 或已有方法。
pub(super) struct ReopenBridge(Rc<State>);

impl ReopenBridge {
  pub(super) fn install(wake: impl Fn() + Send + Sync + 'static) -> Result<Self> {
    let mtm = MainThreadMarker::new().context("Dock reopen bridge requires the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    let delegate = app.delegate().context("winit application delegate is not installed")?;
    let delegate: &AnyObject = (*delegate).as_ref();
    let class = delegate.class();
    ensure!(
      class.name() == c"WinitApplicationDelegate",
      "Dock reopen bridge requires the selected winit delegate"
    );
    let selector = sel!(applicationShouldHandleReopen:hasVisibleWindows:);
    let implementation = reopen_implementation();
    if let Some(existing) = class.instance_method(selector) {
      ensure!(
        std::ptr::fn_addr_eq(existing.implementation(), implementation),
        "Dock reopen selector already has another implementation"
      );
    } else {
      let encoding = CString::new(format!("{}@:@{}", Bool::ENCODING, Bool::ENCODING))?;
      // SAFETY: 只对已验证的 winit delegate 类添加缺失 selector，不替换已有实现。
      // 回调参数为 self、_cmd、NSApplication*、BOOL，返回 BOOL；编码使用当前架构的
      // objc2 Bool。函数是静态 C-unwind 入口，内部捕获 Rust panic，不借用实例内存。
      let added = unsafe {
        objc2::ffi::class_addMethod(class as *const _ as *mut _, selector, implementation, encoding.as_ptr())
      };
      ensure!(added.as_bool(), "cannot add Dock reopen selector");
    }
    let state = Rc::new(State {
      delegate: delegate as *const _ as usize,
      events: RefCell::new(VecDeque::new()),
      wake: Arc::new(wake),
    });
    SUBSCRIBER.with(|subscriber| {
      let mut subscriber = subscriber.borrow_mut();
      ensure!(subscriber.upgrade().is_none(), "Dock reopen bridge is already active");
      *subscriber = Rc::downgrade(&state);
      Ok(Self(state))
    })
  }
}

impl Drop for ReopenBridge {
  fn drop(&mut self) {
    SUBSCRIBER.with(|subscriber| {
      let mut subscriber = subscriber.borrow_mut();
      if subscriber.ptr_eq(&Rc::downgrade(&self.0)) {
        *subscriber = Weak::new();
      }
    });
  }
}

pub(super) fn drain() -> Vec<bool> {
  let state = SUBSCRIBER.with(|subscriber| subscriber.borrow().upgrade());
  state.map_or_else(Vec::new, |state| state.events.borrow_mut().drain(..).collect())
}

fn reopen_implementation() -> Imp {
  // SAFETY: Objective-C 的 Imp 擦除函数签名；class_addMethod 绑定的 selector 编码
  // 与 reopen 的静态函数签名完全一致。只供 Objective-C runtime 按该编码调用。
  unsafe { std::mem::transmute::<extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, Bool) -> Bool, Imp>(reopen) }
}

extern "C-unwind" fn reopen(delegate: *mut AnyObject, _selector: Sel, _app: *mut AnyObject, visible: Bool) -> Bool {
  let handled = catch_unwind(AssertUnwindSafe(|| {
    if MainThreadMarker::new().is_none() {
      return false;
    }
    let state = SUBSCRIBER.with(|subscriber| subscriber.borrow().upgrade());
    state.is_some_and(|state| state.delegate == delegate as usize && state.receive(visible.as_bool()))
  }))
  .unwrap_or(false);
  // 应用处理已入队的事件；无订阅或队列满时保留 AppKit 的默认行为。
  Bool::new(!handled)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::atomic::{AtomicUsize, Ordering};

  #[test]
  fn reopen_queue_is_bounded_and_preserves_visibility() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let count = wakes.clone();
    let state = Rc::new(State {
      delegate: 0,
      events: RefCell::new(VecDeque::new()),
      wake: Arc::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
      }),
    });
    for index in 0..CAPACITY {
      assert!(state.receive(index % 2 == 0));
    }
    assert!(!state.receive(false));
    assert_eq!(wakes.load(Ordering::SeqCst), CAPACITY);
    SUBSCRIBER.with(|subscriber| *subscriber.borrow_mut() = Rc::downgrade(&state));
    let bridge = ReopenBridge(state.clone());
    assert_eq!(drain(), (0..CAPACITY).map(|index| index % 2 == 0).collect::<Vec<_>>());
    assert!(state.receive(true));
    drop(bridge);
    assert!(drain().is_empty());
  }
}
