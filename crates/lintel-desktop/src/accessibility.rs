//! Real AccessKit adapter for host winit windows, plus an explicit keyboard focus order.
//! The host remains responsible for publishing accurate semantics and honoring requested actions.
use std::{collections::HashSet, marker::PhantomData, rc::Rc};

pub use accesskit;
pub use accesskit_winit::{Event as AccessibilityEvent, WindowEvent as AccessibilityWindowEvent};
pub use winit;
use winit::{
  event::WindowEvent,
  event_loop::{ActiveEventLoop, EventLoopProxy},
  window::{Window, WindowId},
};

#[cfg(target_os = "macos")]
use crate::DesktopError;
use crate::{Result, error::invalid};

/// Native accessibility adapter confined to the window's UI thread. Create while the window is
/// hidden, before its first display. Forward every WindowEvent before other application handlers.
pub struct AccessibilityWindow {
  adapter: accesskit_winit::Adapter,
  window_id: WindowId,
  _ui_thread: PhantomData<Rc<()>>,
}

impl AccessibilityWindow {
  /// Installs caller-owned handlers without requiring the host event enum to implement From.
  /// Handlers may run on a platform callback thread; use a wake proxy and plain shared data.
  pub fn with_direct_handlers(
    event_loop: &ActiveEventLoop,
    window: &Window,
    activation: impl accesskit::ActivationHandler + Send + 'static,
    action: impl accesskit::ActionHandler + Send + 'static,
    deactivation: impl accesskit::DeactivationHandler + Send + 'static,
  ) -> Result<Self> {
    #[cfg(target_os = "macos")]
    if objc2::MainThreadMarker::new().is_none() {
      return Err(DesktopError::MainThreadRequired);
    }
    if window.is_visible() == Some(true) {
      return Err(invalid(
        "accessibility window",
        "install the adapter before first display",
      ));
    }
    Ok(Self {
      adapter: accesskit_winit::Adapter::with_direct_handlers(event_loop, window, activation, action, deactivation),
      window_id: window.id(),
      _ui_thread: PhantomData,
    })
  }

  pub fn new<T: From<AccessibilityEvent> + Send + 'static>(
    event_loop: &ActiveEventLoop,
    window: &Window,
    proxy: EventLoopProxy<T>,
  ) -> Result<Self> {
    #[cfg(target_os = "macos")]
    if objc2::MainThreadMarker::new().is_none() {
      return Err(DesktopError::MainThreadRequired);
    }
    if window.is_visible() == Some(true) {
      return Err(invalid(
        "accessibility window",
        "create the adapter before the window is shown for the first time",
      ));
    }
    let adapter = accesskit_winit::Adapter::with_event_loop_proxy(event_loop, window, proxy);
    Ok(Self {
      adapter,
      window_id: window.id(),
      _ui_thread: PhantomData,
    })
  }

  pub fn process_event(&mut self, window: &Window, event: &WindowEvent) -> Result<()> {
    if window.id() != self.window_id {
      return Err(invalid("accessibility window", "event belongs to another window"));
    }
    self.adapter.process_event(window, event);
    Ok(())
  }

  /// The first update after InitialTreeRequested must contain the full tree. Later updates may be
  /// incremental; include the actual current focus in every update. This callback runs only when
  /// the native accessibility client activated the tree.
  pub fn update_if_active(&mut self, updater: impl FnOnce() -> accesskit::TreeUpdate) {
    self.adapter.update_if_active(updater);
  }
  pub fn window_id(&self) -> WindowId {
    self.window_id
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDirection {
  Next,
  Previous,
}

/// Explicit host tab order. Include only controls that are currently enabled, visible, and
/// focusable; commit returned focus to both the widget state and the accessibility tree.
#[derive(Debug, Clone, Default)]
pub struct FocusOrder {
  order: Vec<accesskit::NodeId>,
  focused: Option<accesskit::NodeId>,
}

impl FocusOrder {
  pub fn new(order: Vec<accesskit::NodeId>) -> Result<Self> {
    let mut state = Self::default();
    state.replace(order)?;
    Ok(state)
  }
  /// Preserve current focus when possible; clear it if the host removed that control.
  pub fn replace(&mut self, order: Vec<accesskit::NodeId>) -> Result<()> {
    let unique: HashSet<_> = order.iter().copied().collect();
    if order.len() > 65536 || unique.len() != order.len() {
      return Err(invalid(
        "keyboard focus order",
        "expected at most 65536 distinct node IDs",
      ));
    }
    if self.focused.is_some_and(|focused| !unique.contains(&focused)) {
      self.focused = None;
    }
    self.order = order;
    Ok(())
  }
  pub fn focus(&mut self, target: accesskit::NodeId) -> Result<()> {
    if !self.order.contains(&target) {
      return Err(invalid(
        "keyboard focus target",
        "control is not in the current focus order",
      ));
    }
    self.focused = Some(target);
    Ok(())
  }
  pub fn focused(&self) -> Option<accesskit::NodeId> {
    self.focused
  }
  pub fn focused_or_root(&self, root: accesskit::NodeId) -> accesskit::NodeId {
    self.focused.unwrap_or(root)
  }
  pub fn clear(&mut self) {
    self.focused = None;
  }

  /// Map Tab to Next and Shift+Tab to Previous. `wrap = false` keeps focus at a boundary so the
  /// host can hand traversal to an enclosing container when appropriate.
  pub fn advance(&mut self, direction: FocusDirection, wrap: bool) -> Option<accesskit::NodeId> {
    let count = self.order.len();
    if count == 0 {
      self.focused = None;
      return None;
    }
    let position = self
      .focused
      .and_then(|current| self.order.iter().position(|id| *id == current));
    let target = match (direction, position) {
      (FocusDirection::Next, None) => 0,
      (FocusDirection::Previous, None) => count - 1,
      (FocusDirection::Next, Some(position)) if position + 1 < count => position + 1,
      (FocusDirection::Previous, Some(position)) if position > 0 => position - 1,
      (FocusDirection::Next, Some(position)) => {
        if wrap {
          0
        } else {
          position
        }
      }
      (FocusDirection::Previous, Some(position)) => {
        if wrap {
          count - 1
        } else {
          position
        }
      }
    };
    self.focused = Some(self.order[target]);
    self.focused
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use accesskit::NodeId;
  static_assertions::assert_not_impl_any!(AccessibilityWindow: Send, Sync);

  #[test]
  fn keyboard_focus_handles_reverse_entry_boundaries_wrap_and_removed_controls() {
    let mut focus = FocusOrder::new(vec![NodeId(1), NodeId(2), NodeId(3)]).unwrap();
    assert_eq!(focus.advance(FocusDirection::Previous, false), Some(NodeId(3)));
    assert_eq!(focus.advance(FocusDirection::Next, false), Some(NodeId(3)));
    assert_eq!(focus.advance(FocusDirection::Next, true), Some(NodeId(1)));
    focus.replace(vec![NodeId(1), NodeId(3)]).unwrap();
    assert_eq!(focus.focused(), Some(NodeId(1)));
    focus.replace(vec![NodeId(3)]).unwrap();
    assert_eq!(focus.focused(), None);
    assert!(focus.focus(NodeId(2)).is_err());
    focus.replace(vec![]).unwrap();
    assert_eq!(focus.advance(FocusDirection::Next, true), None);
    assert_eq!(focus.focused_or_root(NodeId(0)), NodeId(0));
  }

  #[test]
  fn invalid_focus_order_does_not_replace_current_order_or_focus() {
    let mut focus = FocusOrder::new(vec![NodeId(1)]).unwrap();
    focus.focus(NodeId(1)).unwrap();
    assert!(focus.replace(vec![NodeId(2), NodeId(2)]).is_err());
    assert_eq!(focus.focused(), Some(NodeId(1)));
    assert_eq!(focus.advance(FocusDirection::Next, true), Some(NodeId(1)));
  }
}
