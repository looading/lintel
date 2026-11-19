//! Optional main-thread integrations installed before a native window is displayed.
use std::sync::Arc;
use winit::{event::WindowEvent, event_loop::ActiveEventLoop, window::Window};

/// The factory is shareable; integrations themselves remain confined to the UI thread.
pub trait NativeWindowFactory: Send + Sync + 'static {
    /// Creates an integration while the native window is still hidden.
    fn create(&self, event_loop: &ActiveEventLoop, window: Arc<Window>, id: crate::core::window::Id,
        title: &str) -> Result<Box<dyn NativeWindowIntegration>, String>;
}

/// The runner releases this object before releasing its native window owner.
pub trait NativeWindowIntegration: 'static {
    /// Return true only when an integration consumed this input event.
    fn before_event(&mut self, event: &WindowEvent) -> bool;
    /// Called after iced processed a redraw; may enqueue a targeted widget operation.
    fn after_redraw(&mut self);
}
