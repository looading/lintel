pub mod manager;

use std::{
  any::{Any, TypeId},
  sync::Arc,
};

use global_hotkey::GlobalHotKeyEvent;

use crate::{context::AppContext, event_loop::event::LintelEvent};

pub trait PluginLifecycle: Any + Send + Sync {
  /// 可失败的启动钩子，默认兼容已有的 on_init 实现。
  /// 返回错误时框架会停止当前插件，并逆序回滚已启动的插件。
  fn on_start(&self, app_context: &AppContext) -> anyhow::Result<()> {
    self.on_init(app_context);
    Ok(())
  }

  fn on_init(&self, _app_context: &AppContext) {}
  fn on_hotkeys_event(&self, _app_context: &AppContext, _event: GlobalHotKeyEvent) {}
  /// 正常退出和启动回滚共用的停止钩子，每个启动尝试最多调用一次。
  fn on_stop(&self, app_context: &AppContext) {
    self.on_exit(app_context);
  }

  fn on_exit(&self, _app_context: &AppContext) {}
  fn on_drop(&self, _app_context: &AppContext) {}
  fn on_event(&self, _app_context: &AppContext, _event: &LintelEvent) {}
}

pub trait Plugin: PluginLifecycle {
  fn name(&self) -> &str;
}

impl dyn Plugin {
  #[inline(always)]
  fn is<T: Plugin>(&self) -> bool {
    self.type_id() == TypeId::of::<T>()
  }

  #[inline(always)]
  pub fn downcast_arc<'a, T: Plugin>(self: &'a Arc<Self>) -> Option<&'a Arc<T>> {
    if self.is::<T>() {
      let ptr = self as *const Arc<_> as *const Arc<T>;
      Some(unsafe { &*ptr })
    } else {
      None
    }
  }
}
