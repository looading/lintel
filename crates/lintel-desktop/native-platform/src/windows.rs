//! Main-thread native menu ownership and message translation. The host supplies an owned window,
//! so the HWND remains backed by that owner until detach and unregister have completed.
use std::{
  cell::RefCell,
  collections::HashMap,
  marker::PhantomData,
  rc::Rc,
  sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
  },
};

use muda::ContextMenu;
pub use raw_window_handle::HasWindowHandle;
use raw_window_handle::{HandleError, RawWindowHandle, WindowHandle};
use windows_sys::Win32::{
  Foundation::{HWND, POINT},
  System::Threading::{GetCurrentProcessId, GetCurrentThreadId},
  UI::WindowsAndMessaging::{
    GetMenu, GetWindowThreadProcessId, IsChild, IsWindow, MSG, SetMenu, TranslateAcceleratorW,
  },
};
use winit::{event_loop::EventLoopBuilder, platform::windows::EventLoopBuilderExtWindows};

static NEXT_WINDOW: AtomicU64 = AtomicU64::new(1);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowToken(u64);
impl WindowToken {
  pub fn as_u64(self) -> u64 {
    self.0
  }
}

struct OwnedWindow<W: ?Sized>(Arc<W>);
impl<W: HasWindowHandle + ?Sized> HasWindowHandle for OwnedWindow<W> {
  fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
    self.0.window_handle()
  }
}

struct Binding {
  owner: Arc<dyn HasWindowHandle + Send + Sync>,
  hwnd: isize,
  menu: Option<muda::Menu>,
  mounted_menu: isize,
}
impl Binding {
  fn validate(&self) -> Result<HWND, String> {
    let current = hwnd_for(self.owner.as_ref())?;
    if current as isize != self.hwnd {
      return Err("window owner no longer exposes the registered HWND".into());
    }
    Ok(current)
  }
  fn detach(&mut self) -> Result<(), String> {
    let Some(menu) = self.menu.take() else {
      return Ok(());
    };
    let mounted = std::mem::replace(&mut self.mounted_menu, 0);
    let hwnd = match self.validate() {
      Ok(hwnd) => hwnd,
      Err(error) => {
        // A foreign native caller invalidated an owned handle outside the expected lifecycle.
        // Keep the upstream menu allocation alive rather than free a live subclass refcon or
        // invoke its destructor against an untrusted/reused HWND. Normal owned-window teardown
        // unregisters first and never takes this conservative leak path.
        std::mem::forget(menu);
        return Err(error);
      }
    };
    // SAFETY: validate checked that the retained owner's current live HWND belongs to this
    // process and UI thread. No native owner is released until this detach has returned.
    unsafe {
      let native = GetMenu(hwnd);
      menu.detach_menu_subclass_from_hwnd(self.hwnd);
      menu.remove_for_hwnd(self.hwnd).map_err(|error| error.to_string())?;
      if !native.is_null() && native as isize != mounted {
        // Preserve a foreign menu if another host explicitly replaced ours.
        if SetMenu(hwnd, native) == 0 {
          return Err(std::io::Error::last_os_error().to_string());
        }
      }
    }
    Ok(())
  }
}
impl Drop for Binding {
  fn drop(&mut self) {
    let _ = self.detach();
  }
}

#[derive(Default)]
struct State {
  bindings: HashMap<WindowToken, Binding>,
  closed: bool,
}

/// Shared message processor; !Send/!Sync ensures native menus remain on their UI thread.
#[derive(Clone, Default)]
pub struct WindowsMenuDispatcher {
  state: Rc<RefCell<State>>,
}

/// Win32 message copied by the trusted host message hook, with no borrowed native pointer.
#[derive(Debug, Clone, Copy)]
pub struct WindowsMessage {
  pub window: usize,
  pub message: u32,
  pub wparam: usize,
  pub lparam: isize,
  pub time: u32,
  pub position: (i32, i32),
}

impl WindowsMenuDispatcher {
  /// Call before normal TranslateMessage/DispatchMessage. True means an accelerator consumed it.
  pub fn process_message(&self, message: WindowsMessage) -> bool {
    if !matches!(message.message, 0x0100 | 0x0104) {
      return false;
    } // WM_KEYDOWN / WM_SYSKEYDOWN
    let candidates: Vec<_> = self
      .state
      .borrow()
      .bindings
      .values()
      .filter_map(|binding| {
        binding
          .menu
          .as_ref()
          .map(|menu| (binding.owner.clone(), binding.hwnd, menu.clone()))
      })
      .collect();
    for (owner, expected, menu) in candidates {
      let Ok(hwnd) = hwnd_for(owner.as_ref()) else {
        continue;
      };
      if hwnd as isize != expected {
        continue;
      }
      // SAFETY: IsChild accepts arbitrary HWND values and returns false for unrelated/invalid
      // handles; the registered parent itself was validated above and remains retained.
      let related = message.window == hwnd as usize || unsafe { IsChild(hwnd, message.window as HWND) != 0 };
      if !related {
        continue;
      }
      let native = MSG {
        hwnd: message.window as HWND,
        message: message.message,
        wParam: message.wparam,
        lParam: message.lparam,
        time: message.time,
        pt: POINT {
          x: message.position.0,
          y: message.position.1,
        },
      };
      // SAFETY: HWND and HACCEL are kept alive by this owned snapshot; MSG is initialized owned
      // data. No RefCell borrow is held while Win32 can synchronously deliver WM_COMMAND.
      if unsafe { TranslateAcceleratorW(hwnd, menu.haccel() as _, &native) } != 0 {
        return true;
      }
    }
    false
  }
}

/// Owns retained native windows and their mounted menu clones until explicit unregister.
#[derive(Default)]
pub struct WindowsMenuHost {
  dispatcher: WindowsMenuDispatcher,
  _ui_thread: PhantomData<Rc<()>>,
}
impl WindowsMenuHost {
  pub fn dispatcher(&self) -> WindowsMenuDispatcher {
    self.dispatcher.clone()
  }
  pub fn register<W: HasWindowHandle + Send + Sync + 'static + ?Sized>(
    &mut self,
    window: Arc<W>,
  ) -> Result<WindowToken, String> {
    if self.dispatcher.state.borrow().closed {
      return Err("window menu host has stopped".into());
    }
    let owner: Arc<dyn HasWindowHandle + Send + Sync> = Arc::new(OwnedWindow(window));
    let hwnd = hwnd_for(owner.as_ref())?;
    if self
      .dispatcher
      .state
      .borrow()
      .bindings
      .values()
      .any(|binding| binding.hwnd == hwnd as isize)
    {
      return Err("this native window is already registered".into());
    }
    let id = WindowToken(NEXT_WINDOW.fetch_add(1, Ordering::Relaxed));
    self.dispatcher.state.borrow_mut().bindings.insert(
      id,
      Binding {
        owner,
        hwnd: hwnd as isize,
        menu: None,
        mounted_menu: 0,
      },
    );
    Ok(id)
  }
  pub fn set_menu(&mut self, target: WindowToken, menu: Option<muda::Menu>) -> Result<(), String> {
    let mut binding = self
      .dispatcher
      .state
      .borrow_mut()
      .bindings
      .remove(&target)
      .ok_or("unknown window menu target")?;
    let result = (|| {
      let hwnd = binding.validate()?;
      binding.detach()?;
      if let Some(menu) = menu {
        // SAFETY: validated retained owner belongs to this UI thread. Refuse to replace a
        // foreign menu; the host must explicitly release it before this mounting operation.
        unsafe {
          if !GetMenu(hwnd).is_null() {
            return Err("native window already has a foreign menu".into());
          }
          menu.init_for_hwnd(hwnd as isize).map_err(|error| error.to_string())?;
          binding.mounted_menu = GetMenu(hwnd) as isize;
          if binding.mounted_menu == 0 {
            menu.detach_menu_subclass_from_hwnd(hwnd as isize);
            let _ = menu.remove_for_hwnd(hwnd as isize);
            return Err("Win32 did not attach the native menu".into());
          }
        }
        binding.menu = Some(menu);
      }
      Ok(())
    })();
    self.dispatcher.state.borrow_mut().bindings.insert(target, binding);
    result
  }
  pub fn unregister(&mut self, target: WindowToken) -> Result<(), String> {
    let mut binding = self
      .dispatcher
      .state
      .borrow_mut()
      .bindings
      .remove(&target)
      .ok_or("unknown window menu target")?;
    binding.detach()
  }
  pub fn clear(&mut self) {
    let mut bindings = {
      let mut state = self.dispatcher.state.borrow_mut();
      state.closed = true;
      std::mem::take(&mut state.bindings)
    };
    for binding in bindings.values_mut() {
      let _ = binding.detach();
    }
  }
}
impl Drop for WindowsMenuHost {
  fn drop(&mut self) {
    self.clear();
  }
}

/// Installed before EventLoop creation; bind the runtime's dispatcher after creating the runtime.
#[derive(Clone, Default)]
pub struct WindowsMenuMessageHook {
  dispatcher: Rc<RefCell<Option<WindowsMenuDispatcher>>>,
}
impl WindowsMenuMessageHook {
  pub fn bind(&self, dispatcher: WindowsMenuDispatcher) {
    *self.dispatcher.borrow_mut() = Some(dispatcher);
  }
  pub fn install<T: 'static>(&self, builder: &mut EventLoopBuilder<T>) {
    let hook = self.clone();
    builder.with_msg_hook(move |message| {
      if message.is_null() {
        return false;
      }
      let dispatcher = hook.dispatcher.borrow().clone();
      let Some(dispatcher) = dispatcher else {
        return false;
      };
      // SAFETY: winit documents that this hook is passed a live Win32 MSG for this callback.
      // Both bindings use the Windows SDK MSG ABI. Copy only scalar fields into owned data.
      let native = unsafe { &*message.cast::<MSG>() };
      dispatcher.process_message(WindowsMessage {
        window: native.hwnd as usize,
        message: native.message,
        wparam: native.wParam,
        lparam: native.lParam,
        time: native.time,
        position: (native.pt.x, native.pt.y),
      })
    });
  }
}

fn hwnd_for(owner: &(dyn HasWindowHandle + Send + Sync)) -> Result<HWND, String> {
  let handle = owner.window_handle().map_err(|error| error.to_string())?;
  let RawWindowHandle::Win32(window) = handle.as_raw() else {
    return Err("expected a Win32 window handle".into());
  };
  let hwnd = window.hwnd.get() as HWND;
  let mut process = 0;
  // SAFETY: these Win32 query APIs accept invalid handles and report failure. The borrowed
  // handle is live by HasWindowHandle's contract; the retained owner outlives all operations.
  unsafe {
    if IsWindow(hwnd) == 0 {
      return Err("window HWND is no longer valid".into());
    }
    let thread = GetWindowThreadProcessId(hwnd, &mut process);
    if thread == 0 || thread != GetCurrentThreadId() || process != GetCurrentProcessId() {
      return Err("window menu operation must run on the owning process UI thread".into());
    }
  }
  Ok(hwnd)
}
