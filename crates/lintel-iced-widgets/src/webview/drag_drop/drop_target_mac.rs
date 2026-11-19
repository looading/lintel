use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::ptr::NonNull;

use cef::{ImplBrowser, ImplBrowserHost};
use lintel_lib_iced::iced;
use objc2::rc::{Retained, Weak};
use objc2::runtime::ProtocolObject;
use objc2::{
  AnyThread, ClassType, DeclaredClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, msg_send_id,
};
use objc2_app_kit::{
  NSAutoresizingMaskOptions, NSDragOperation, NSDraggingContext, NSDraggingDestination, NSDraggingInfo, NSDraggingItem,
  NSDraggingSession, NSDraggingSource, NSImage, NSPasteboard, NSPasteboardItem, NSPasteboardTypeFileURL,
  NSPasteboardTypeHTML, NSPasteboardTypeString, NSPasteboardTypeURL, NSView,
};
use objc2_foundation::NSObjectProtocol;
use objc2_foundation::{NSArray, NSData, NSPoint, NSRect, NSSize, NSString, NSURL};

use crate::webview::BrowserId;

thread_local! {
  static DRAGGING_BROWSER: Cell<Option<(BrowserId, iced::Rectangle, f32)>> = const { Cell::new(None) };
  // Weak reference: the parent NSView's subview array owns the only strong
  // reference. When the parent is deallocated the overlay is freed automatically
  // and the Weak entry here simply returns None — no manual cleanup required.
  static DRAG_OVERLAYS: RefCell<HashMap<usize, Weak<CefDragOverlayView>>> =
    RefCell::new(HashMap::new());
  static DROP_TARGET_BROWSERS: RefCell<HashMap<usize, BTreeMap<BrowserId, (iced::Rectangle, f32)>>> =
    RefCell::new(HashMap::new());
}

pub(crate) fn set_dragging_browser(browser_id: BrowserId, rect: iced::Rectangle, scale_factor: f32) {
  DRAGGING_BROWSER.with(|c| c.set(Some((browser_id, rect, scale_factor))));
}

fn get_dragging_browser() -> Option<(BrowserId, iced::Rectangle, f32)> {
  DRAGGING_BROWSER.with(|c| c.get())
}

pub(crate) fn clear_dragging_browser() {
  DRAGGING_BROWSER.with(|c| c.set(None));
}

pub(crate) fn register_browser_drop_target(
  ns_view_ptr: NonNull<c_void>,
  browser_id: BrowserId,
  rect: iced::Rectangle,
  scale_factor: f32,
) {
  let key = ns_view_ptr.as_ptr() as usize;
  tracing::info!(
    ns_view_key = key,
    ?browser_id,
    ?rect,
    scale_factor,
    "mac dnd: register browser drop target"
  );
  DROP_TARGET_BROWSERS.with_borrow_mut(|browsers| {
    browsers
      .entry(key)
      .or_default()
      .insert(browser_id, (rect, scale_factor));
  });
}

pub(crate) fn unregister_browser_drop_target(browser_id: BrowserId) {
  tracing::info!(?browser_id, "mac dnd: unregister browser drop target");
  DROP_TARGET_BROWSERS.with_borrow_mut(|browsers| {
    for targets in browsers.values_mut() {
      targets.remove(&browser_id);
    }
  });
}

// ---------------------------------------------------------------------------
// Drag-operation conversions between CEF and AppKit
// ---------------------------------------------------------------------------

fn drag_op_from_cef(op: cef::DragOperationsMask) -> NSDragOperation {
  let raw = *op.as_ref();
  let mut result = NSDragOperation::None;
  if raw & cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_COPY
    != cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_NONE
  {
    result |= NSDragOperation::Copy;
  }
  if raw & cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_MOVE
    != cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_NONE
  {
    result |= NSDragOperation::Move;
  }
  if raw & cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_LINK
    != cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_NONE
  {
    result |= NSDragOperation::Link;
  }
  result
}

fn drag_op_to_cef(op: NSDragOperation) -> cef::DragOperationsMask {
  let mut result = cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_NONE;
  if op.contains(NSDragOperation::Copy) {
    result |= cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_COPY;
  }
  if op.contains(NSDragOperation::Move) {
    result |= cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_MOVE;
  }
  if op.contains(NSDragOperation::Link) {
    result |= cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_LINK;
  }
  result.into()
}

// ---------------------------------------------------------------------------
// Coordinate helpers
// ---------------------------------------------------------------------------

/// Convert AppKit dragging-info location (window-relative, bottom-left origin)
/// Convert a dragging-info location into CEF view-relative coordinates
/// (top-left origin, logical points).
///
/// `draggingLocation()` returns window-coordinate points (already logical,
/// bottom-left origin). `convertPoint_fromView(pt, None)` converts to view-
/// local points; because the view declares `isFlipped = true` the returned y
/// is measured from the top of the view – no manual flip needed.
/// AppKit points == iced logical pixels, so no division by scale_factor.
fn dragging_location_to_cef(
  info: &ProtocolObject<dyn NSDraggingInfo>,
  overlay: &CefDragOverlayView,
  bounds: iced::Rectangle,
) -> (i32, i32) {
  let window_pt = unsafe { info.draggingLocation() };
  // isFlipped = true → view_pt.y is already measured from the top of the view.
  let view_pt = unsafe { overlay.convertPoint_fromView(window_pt, None) };
  let x = (view_pt.x as f32 - bounds.x) as i32;
  let y = (view_pt.y as f32 - bounds.y) as i32;
  (x, y)
}

fn dragging_location_in_overlay(
  info: &ProtocolObject<dyn NSDraggingInfo>,
  overlay: &CefDragOverlayView,
) -> iced::Point {
  let window_pt = unsafe { info.draggingLocation() };
  let view_pt = unsafe { overlay.convertPoint_fromView(window_pt, None) };
  iced::Point::new(view_pt.x as f32, view_pt.y as f32)
}

fn get_drop_target_browser(
  info: &ProtocolObject<dyn NSDraggingInfo>,
  overlay: &CefDragOverlayView,
) -> Option<(BrowserId, iced::Rectangle, f32)> {
  let point = dragging_location_in_overlay(info, overlay);
  let ns_view_key = overlay.ivars().ns_view_key.get();
  DROP_TARGET_BROWSERS.with_borrow(|browsers| {
    let Some(targets) = browsers.get(&ns_view_key) else {
      tracing::warn!(
        ns_view_key,
        ?point,
        "mac dnd: no registered browser targets for overlay"
      );
      return None;
    };

    let result = targets.iter().rev().find_map(|(browser_id, (bounds, scale_factor))| {
      bounds
        .contains(point)
        .then_some((*browser_id, *bounds, scale_factor.max(1.0)))
    });

    tracing::trace!(
      ns_view_key,
      ?point,
      target_count = targets.len(),
      ?result,
      "mac dnd: resolve drop target browser"
    );
    result
  })
}

fn get_active_browser(
  info: &ProtocolObject<dyn NSDraggingInfo>,
  overlay: &CefDragOverlayView,
) -> Option<(BrowserId, iced::Rectangle, f32)> {
  if let Some(browser) = get_dragging_browser() {
    tracing::trace!(browser_id = ?browser.0, "mac dnd: use cef-origin dragging browser");
    Some(browser)
  } else {
    get_drop_target_browser(info, overlay)
  }
}

/// Convert a screen-coordinate NSPoint (e.g. from drag-session end callback)
/// into CEF view-relative coordinates (top-left origin, logical points).
fn screen_to_cef_coords(view: &CefDragOverlayView, screen_point: NSPoint, bounds: iced::Rectangle) -> (i32, i32) {
  // screen → window (logical points, bottom-left)
  let window_pt: NSPoint = unsafe {
    let window: Option<Retained<objc2_app_kit::NSWindow>> = msg_send_id![view, window];
    if let Some(w) = window {
      let rect = NSRect {
        origin: screen_point,
        size: NSSize {
          width: 0.0,
          height: 0.0,
        },
      };
      w.convertRectFromScreen(rect).origin
    } else {
      screen_point
    }
  };
  // window → view (isFlipped = true → top-left origin, logical points)
  let view_pt = unsafe { view.convertPoint_fromView(window_pt, None) };
  let x = (view_pt.x as f32 - bounds.x) as i32;
  let y = (view_pt.y as f32 - bounds.y) as i32;
  (x, y)
}

// ---------------------------------------------------------------------------
// CefDragOverlayView – transparent NSView subclass
// ---------------------------------------------------------------------------

struct CefDragOverlayViewIvars {
  ns_view_key: Cell<usize>,
  current_drag_op: Cell<NSDragOperation>,
  current_allowed_ops: Cell<NSDragOperation>,
  current_browser: Cell<Option<BrowserId>>,
  current_drag_data: RefCell<Option<cef::DragData>>,
}

impl Default for CefDragOverlayViewIvars {
  fn default() -> Self {
    Self {
      ns_view_key: Cell::new(0),
      current_drag_op: Cell::new(NSDragOperation::None),
      current_allowed_ops: Cell::new(NSDragOperation::None),
      current_browser: Cell::new(None),
      current_drag_data: RefCell::new(None),
    }
  }
}

define_class!(
  // SAFETY: NSView imposes no extra subclassing requirements.
  #[unsafe(super(NSView))]
  #[thread_kind = MainThreadOnly]
  #[ivars = CefDragOverlayViewIvars]
  struct CefDragOverlayView;

  // Declare the inherited NSObjectProtocol conformance.
  unsafe impl NSObjectProtocol for CefDragOverlayView {}

  impl CefDragOverlayView {
    /// 普通鼠标事件继续穿透；拖拽事件需要命中 overlay 才能触发 NSDraggingDestination。
    #[unsafe(method(hitTest:))]
    fn hit_test(&self, point: NSPoint) -> *mut NSView {
      let bounds = unsafe { self.bounds() };
      let inside = point.x >= bounds.origin.x
        && point.y >= bounds.origin.y
        && point.x < bounds.origin.x + bounds.size.width
        && point.y < bounds.origin.y + bounds.size.height;
      tracing::trace!(
        ns_view_key = self.ivars().ns_view_key.get(),
        point_x = point.x,
        point_y = point.y,
        bounds_x = bounds.origin.x,
        bounds_y = bounds.origin.y,
        bounds_w = bounds.size.width,
        bounds_h = bounds.size.height,
        inside,
        "mac dnd: overlay hitTest"
      );
      if !inside {
        return std::ptr::null_mut();
      }

      let is_drag_event = unsafe {
        let mtm = MainThreadMarker::new_unchecked();
        let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        app.currentEvent().is_some_and(|event| {
          let event_type: isize = msg_send![&*event, type];
          // NSEventTypeDraggingEntered ... NSEventTypeDraggingEnded
          (14..=18).contains(&event_type)
        })
      };

      if is_drag_event {
        tracing::trace!(ns_view_key = self.ivars().ns_view_key.get(), "mac dnd: overlay hitTest accepts drag event");
        self as *const CefDragOverlayView as *mut NSView
      } else {
        std::ptr::null_mut()
      }
    }

    #[unsafe(method(isFlipped))]
    fn is_flipped(&self) -> bool {
      true
    }
  }

  // ---- NSDraggingDestination -----------------------------------------------

  unsafe impl NSDraggingDestination for CefDragOverlayView {
    #[unsafe(method(draggingEntered:))]
    fn dragging_entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
      let ns_op = unsafe { sender.draggingSourceOperationMask() };
      self.ivars().current_drag_op.set(NSDragOperation::Copy);
      tracing::info!(
        ns_view_key = self.ivars().ns_view_key.get(),
        ns_op = ns_op.bits(),
        "mac dnd: dragging entered"
      );

      let (browser, bounds, _scale_factor) = match get_active_browser(sender, self) {
        Some(v) => v,
        None => {
          tracing::warn!("mac dnd: dragging entered without active browser");
          return NSDragOperation::None;
        }
      };
      let host = match cef::browser_host_get_browser_by_identifier(browser.inner())
        .and_then(|b: cef::Browser| b.host())
      {
        Some(h) => h,
        None => {
          tracing::warn!(?browser, "mac dnd: dragging entered browser host missing");
          return NSDragOperation::None;
        }
      };

      let (cx, cy) = dragging_location_to_cef(sender, self, bounds);
      let mouse_event = cef::MouseEvent { x: cx, y: cy, modifiers: 0 };
      let allowed = drag_op_to_cef(ns_op);

      let pasteboard = unsafe { sender.draggingPasteboard() };
      let mut drag_data = pasteboard_to_cef_drag_data(&pasteboard);
      *self.ivars().current_drag_data.borrow_mut() = drag_data.as_ref().and_then(cef::ImplDragData::clone);

      host.drag_target_drag_enter(drag_data.as_mut(), Some(&mouse_event), allowed);
      host.drag_target_drag_over(Some(&mouse_event), allowed);

      self.ivars().current_browser.set(Some(browser));
      self.ivars().current_drag_op.get()
    }

    #[unsafe(method(draggingUpdated:))]
    fn dragging_updated(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
      let ns_op = unsafe { sender.draggingSourceOperationMask() };
      tracing::trace!(
        ns_view_key = self.ivars().ns_view_key.get(),
        ns_op = ns_op.bits(),
        "mac dnd: dragging updated"
      );

      let (browser, bounds, _scale_factor) = match get_active_browser(sender, self) {
        Some(v) => v,
        None => {
          // 当前不在任何 WebView 上，通知 CEF 离开并清空状态。
          if let Some(previous_browser) = self.ivars().current_browser.take()
            && let Some(host) = cef::browser_host_get_browser_by_identifier(previous_browser.inner())
              .and_then(|b: cef::Browser| b.host())
          {
            host.drag_target_drag_leave();
          }
          tracing::trace!("mac dnd: dragging updated outside all webviews");
          return NSDragOperation::None;
        }
      };
      if let Some(previous_browser) = self.ivars().current_browser.get()
        && previous_browser != browser
        && let Some(host) = cef::browser_host_get_browser_by_identifier(previous_browser.inner())
          .and_then(|b: cef::Browser| b.host())
      {
        host.drag_target_drag_leave();
      }
      let host = match cef::browser_host_get_browser_by_identifier(browser.inner())
        .and_then(|b: cef::Browser| b.host())
      {
        Some(h) => h,
        None => return NSDragOperation::None,
      };

      let (cx, cy) = dragging_location_to_cef(sender, self, bounds);
      let mouse_event = cef::MouseEvent { x: cx, y: cy, modifiers: 0 };
      let allowed = drag_op_to_cef(ns_op);

      host.drag_target_drag_over(Some(&mouse_event), allowed);
      self.ivars().current_browser.set(Some(browser));
      tracing::trace!(?browser, cef_x = cx, cef_y = cy, "mac dnd: forward drag over to cef");
      self.ivars().current_drag_op.get()
    }

    #[unsafe(method(draggingExited:))]
    fn dragging_exited(&self, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
      tracing::info!(
        ns_view_key = self.ivars().ns_view_key.get(),
        current_browser = ?self.ivars().current_browser.get(),
        "mac dnd: dragging exited"
      );
      *self.ivars().current_drag_data.borrow_mut() = None;
      if let Some(browser) = self.ivars().current_browser.take().or_else(|| get_dragging_browser().map(|(browser, ..)| browser)) {
        if let Some(host) = cef::browser_host_get_browser_by_identifier(browser.inner())
          .and_then(|b: cef::Browser| b.host())
        {
          host.drag_target_drag_leave();
        }
      }
    }

    #[unsafe(method(prepareForDragOperation:))]
    fn prepare_for_drag_operation(
      &self,
      _sender: &ProtocolObject<dyn NSDraggingInfo>,
    ) -> bool {
      tracing::info!(
        ns_view_key = self.ivars().ns_view_key.get(),
        current_browser = ?self.ivars().current_browser.get(),
        "mac dnd: prepare for drag operation"
      );
      true
    }

    #[unsafe(method(performDragOperation:))]
    fn perform_drag_operation(
      &self,
      sender: &ProtocolObject<dyn NSDraggingInfo>,
    ) -> bool {
      tracing::info!(
        ns_view_key = self.ivars().ns_view_key.get(),
        current_browser = ?self.ivars().current_browser.get(),
        "mac dnd: perform drag operation"
      );
      let result = (|| -> Option<()> {
        let (browser, bounds, _scale_factor) = get_active_browser(sender, self)?;
        let host = cef::browser_host_get_browser_by_identifier(browser.inner())
          .and_then(|b: cef::Browser| b.host())?;

        let (cx, cy) = dragging_location_to_cef(sender, self, bounds);
        let mouse_event = cef::MouseEvent { x: cx, y: cy, modifiers: 0 };
        let ns_op = unsafe { sender.draggingSourceOperationMask() };
        let allowed = drag_op_to_cef(ns_op);

        host.drag_target_drag_over(Some(&mouse_event), allowed);
        host.drag_target_drop(Some(&mouse_event));
        tracing::info!(?browser, cef_x = cx, cef_y = cy, "mac dnd: forward drop to cef");
        self.ivars().current_browser.set(None);
        *self.ivars().current_drag_data.borrow_mut() = None;
        Some(())
      })();
      if result.is_none() {
        tracing::warn!("mac dnd: perform drag operation failed before cef drop");
      }
      result.is_some()
    }
  }

  // ---- NSDraggingSource ----------------------------------------------------

  unsafe impl NSDraggingSource for CefDragOverlayView {
    #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
    fn dragging_session_source_operation_mask(
      &self,
      _session: &NSDraggingSession,
      _context: NSDraggingContext,
    ) -> NSDragOperation {
      self.ivars().current_allowed_ops.get()
    }

    #[unsafe(method(draggingSession:endedAtPoint:operation:))]
    fn dragging_session_ended(
      &self,
      _session: &NSDraggingSession,
      screen_point: NSPoint,
      operation: NSDragOperation,
    ) {
      if let Some((browser, bounds, _scale_factor)) = get_dragging_browser() {
        if let Some(host) = cef::browser_host_get_browser_by_identifier(browser.inner())
          .and_then(|b: cef::Browser| b.host())
        {
          let (cx, cy) = screen_to_cef_coords(self, screen_point, bounds);
          let cef_op = drag_op_to_cef(operation);
          host.drag_source_ended_at(cx, cy, cef_op);
          host.drag_source_system_drag_ended();
        }
      }
      clear_dragging_browser();
    }
  }
);

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Register the winit window's NSView for drag & drop.
///
/// # Safety
/// `ns_view_ptr` must be a valid, live `NSView *` pointer owned by a winit window.
pub(crate) unsafe fn register_drag_types(ns_view_ptr: NonNull<c_void>, mtm: MainThreadMarker) {
  tracing::info!(
    ns_view_key = ns_view_ptr.as_ptr() as usize,
    "mac dnd: register drag types"
  );
  let parent =
    unsafe { Retained::retain(ns_view_ptr.as_ptr().cast::<NSView>()).expect("ns_view_ptr must be a valid NSView") };

  let parent_bounds = unsafe { parent.bounds() };
  tracing::info!(
    ns_view_key = ns_view_ptr.as_ptr() as usize,
    bounds_x = parent_bounds.origin.x,
    bounds_y = parent_bounds.origin.y,
    bounds_w = parent_bounds.size.width,
    bounds_h = parent_bounds.size.height,
    "mac dnd: create overlay"
  );

  let overlay: Retained<CefDragOverlayView> = unsafe {
    let alloc = CefDragOverlayView::alloc(mtm);
    match msg_send_id![alloc, initWithFrame: parent_bounds] {
      Some(v) => v,
      None => {
        tracing::error!("CefDragOverlayView initWithFrame: returned nil");
        return;
      }
    }
  };

  unsafe {
    let mask = NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable;
    overlay.setAutoresizingMask(mask);
    parent.addSubview(&overlay);
  }

  let key = ns_view_ptr.as_ptr() as usize;
  overlay.ivars().ns_view_key.set(key);

  // Rust 2024 要求 unsafe fn 内读取 extern static 时也显式标记 unsafe。
  let pasteboard_types = unsafe {
    [
      NSPasteboardTypeString,
      NSPasteboardTypeHTML,
      NSPasteboardTypeURL,
      NSPasteboardTypeFileURL,
    ]
  };
  let types = NSArray::from_slice(&pasteboard_types);
  unsafe { overlay.registerForDraggedTypes(&types) };

  // Store only a weak reference. The parent NSView's subviews array holds the
  // strong reference; when the parent is deallocated the overlay is freed
  // automatically without any manual unregister call.
  DRAG_OVERLAYS.with(|m| m.borrow_mut().insert(key, Weak::from_retained(&overlay)));
  DROP_TARGET_BROWSERS.with_borrow_mut(|browsers| {
    browsers.entry(key).or_default();
  });
}

fn get_overlay(ns_view_ptr: NonNull<c_void>) -> Option<Retained<CefDragOverlayView>> {
  let key = ns_view_ptr.as_ptr() as usize;
  // Attempt to upgrade the weak reference; returns None if the view was freed.
  DRAG_OVERLAYS.with(|m| m.borrow().get(&key).and_then(|w| w.load()))
}

/// Initiate a CEF-originated drag session on the AppKit main thread.
pub(crate) fn start_dragging(
  raw_handle: &crate::webview::instance::RawHandle,
  browser_id: BrowserId,
  bounds: iced::Rectangle,
  scale_factor: f32,
  drag_data: &cef::DragData,
  allowed: cef::DragOperationsMask,
) -> anyhow::Result<()> {
  let ns_view_ptr = raw_handle.handle();

  let overlay = get_overlay(ns_view_ptr)
    .ok_or_else(|| anyhow::anyhow!("no CefDragOverlayView for this window; was register_dnd called?"))?;

  set_dragging_browser(browser_id, bounds, scale_factor);
  overlay.ivars().current_allowed_ops.set(drag_op_from_cef(allowed));

  let mtm = unsafe { MainThreadMarker::new_unchecked() };
  let app = unsafe { objc2_app_kit::NSApplication::sharedApplication(mtm) };
  let event = unsafe { app.currentEvent() };
  let Some(event) = event else {
    clear_dragging_browser();
    return Err(anyhow::anyhow!("no current NSEvent available for drag session"));
  };

  let pboard_items = build_dragging_items(drag_data);
  let items_to_use = if pboard_items.is_empty() {
    // Fallback: empty pasteboard item so the session can start.
    let dummy = unsafe {
      let pb_item = NSPasteboardItem::new();
      pb_item.setString_forType(&NSString::from_str(""), NSPasteboardTypeString);
      let writing: &ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting> = ProtocolObject::from_ref(&*pb_item);
      NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), writing)
    };
    vec![dummy]
  } else {
    pboard_items
  };

  // AppKit requires each NSDraggingItem to have a non-zero draggingFrame.
  // Use the CEF drag image (PNG) if available, otherwise a transparent placeholder.
  // All dimensions here are in logical points (view coordinate space).
  {
    use cef::ImplDragData;
    let location_in_window = unsafe { event.locationInWindow() };
    let location_in_view = unsafe { overlay.convertPoint_fromView(location_in_window, None) };
    let hotspot = drag_data.image_hotspot();
    // Hotspot from CEF is in logical (CSS) pixels = AppKit points.
    let hx = hotspot.x as f64;
    let hy = hotspot.y as f64;

    let ns_image = build_ns_image(drag_data, scale_factor);

    // After build_ns_image, NSImage.size() is already in logical points.
    let (img_w, img_h) = match &ns_image {
      Some(img) => {
        let sz = unsafe { img.size() };
        (sz.width.max(1.0), sz.height.max(1.0))
      }
      None => (32.0_f64.max(hx * 2.0), 32.0_f64.max(hy * 2.0)),
    };

    let drag_frame = NSRect {
      origin: NSPoint {
        x: location_in_view.x - hx,
        y: location_in_view.y - hy,
      },
      size: objc2_foundation::NSSize {
        width: img_w,
        height: img_h,
      },
    };

    for item in &items_to_use {
      unsafe {
        item.setDraggingFrame_contents(
          drag_frame,
          ns_image.as_deref().map(|img| img as &objc2::runtime::AnyObject),
        );
      }
    }
  }

  let arr = NSArray::from_retained_slice(&items_to_use);
  let source: &ProtocolObject<dyn NSDraggingSource> = ProtocolObject::from_ref(&*overlay);
  unsafe { overlay.beginDraggingSessionWithItems_event_source(&arr, &event, source) };

  Ok(())
}

// ---------------------------------------------------------------------------
// Helpers: build NSDraggingItems from cef::DragData
// ---------------------------------------------------------------------------

/// Build an `NSImage` from the CEF drag image, encoded as PNG at the given
/// scale factor.  Returns `None` if there is no image or encoding fails.
fn build_ns_image(drag_data: &cef::DragData, scale_factor: f32) -> Option<Retained<NSImage>> {
  use cef::{ImplBinaryValue, ImplDragData, ImplImage};

  if drag_data.has_image() == 0 {
    return None;
  }
  let cef_image = drag_data.image()?;

  let mut pw = 0i32;
  let mut ph = 0i32;
  let bv = cef_image.as_png(scale_factor, 1, Some(&mut pw), Some(&mut ph))?;

  let byte_count = bv.size();
  if byte_count == 0 {
    return None;
  }
  let mut png_bytes: Vec<u8> = vec![0u8; byte_count];
  bv.data(Some(&mut png_bytes), 0);

  unsafe {
    let ns_data = NSData::dataWithBytes_length(png_bytes.as_ptr().cast(), byte_count as objc2_foundation::NSUInteger);
    let ns_image = NSImage::initWithData(NSImage::alloc(), &ns_data)?;
    // PNG from CEF has no DPI metadata so NSImage reports pixel dimensions as points.
    // Set the logical (point) size explicitly by dividing by the scale factor.
    if pw > 0 && ph > 0 {
      ns_image.setSize(NSSize {
        width: pw as f64 / scale_factor as f64,
        height: ph as f64 / scale_factor as f64,
      });
    }
    Some(ns_image)
  }
}

fn build_dragging_items(drag_data: &cef::DragData) -> Vec<Retained<NSDraggingItem>> {
  use cef::ImplDragData;

  let text = cef::CefString::from(&drag_data.fragment_text()).to_string();
  let html = cef::CefString::from(&drag_data.fragment_html()).to_string();
  let url = cef::CefString::from(&drag_data.link_url()).to_string();

  if text.is_empty() && html.is_empty() && url.is_empty() {
    return Vec::new();
  }

  unsafe {
    let pb_item = NSPasteboardItem::new();
    if !text.is_empty() {
      pb_item.setString_forType(&NSString::from_str(&text), NSPasteboardTypeString);
    }
    if !html.is_empty() {
      pb_item.setString_forType(&NSString::from_str(&html), NSPasteboardTypeHTML);
    }
    if !url.is_empty() {
      pb_item.setString_forType(&NSString::from_str(&url), NSPasteboardTypeURL);
    }
    let writing: &ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting> = ProtocolObject::from_ref(&*pb_item);
    vec![NSDraggingItem::initWithPasteboardWriter(
      NSDraggingItem::alloc(),
      writing,
    )]
  }
}

/// Build a `cef::DragData` from an incoming macOS pasteboard (external drag into the webview).
fn pasteboard_to_cef_drag_data(pasteboard: &NSPasteboard) -> Option<cef::DragData> {
  use cef::ImplDragData;

  let data = cef::drag_data_create()?;
  let mut file_count = 0usize;
  let mut has_html = false;
  let mut has_text = false;
  let mut has_url = false;

  unsafe {
    let types = pasteboard.types()?;

    // Finder file drags also expose URL/string flavors. If those are set on the
    // CEF drag data together with files, Chromium may surface the drag as
    // text/uri-list instead of a DataTransfer.files payload.
    if types.containsObject(NSPasteboardTypeFileURL) {
      if let Some(items) = pasteboard.pasteboardItems() {
        for item in items {
          let Some(file_url) = item.stringForType(NSPasteboardTypeFileURL) else {
            continue;
          };
          if add_file_url_to_drag_data(&data, &file_url.to_string()) {
            file_count += 1;
          }
        }
      }

      if file_count == 0
        && let Some(file_url) = pasteboard.stringForType(NSPasteboardTypeFileURL)
        && add_file_url_to_drag_data(&data, &file_url.to_string())
      {
        file_count += 1;
      }
    }

    if file_count > 0 {
      return Some(data);
    }

    if types.containsObject(NSPasteboardTypeHTML) {
      if let Some(s) = pasteboard.stringForType(NSPasteboardTypeHTML) {
        let cef_str = cef::CefString::from(s.to_string().as_str());
        data.set_fragment_html(Some(&cef_str));
        has_html = true;
      }
    }
    if types.containsObject(NSPasteboardTypeString) {
      if let Some(s) = pasteboard.stringForType(NSPasteboardTypeString) {
        let cef_str = cef::CefString::from(s.to_string().as_str());
        data.set_fragment_text(Some(&cef_str));
        has_text = true;
      }
    }
    if types.containsObject(NSPasteboardTypeURL) {
      if let Some(s) = pasteboard.stringForType(NSPasteboardTypeURL) {
        let cef_str = cef::CefString::from(s.to_string().as_str());
        data.set_link_url(Some(&cef_str));
        has_url = true;
      }
    }
  }

  Some(data)
}

fn add_file_url_to_drag_data(data: &cef::DragData, file_url: &str) -> bool {
  use cef::ImplDragData;

  let Some(path) = file_url_to_path(file_url) else {
    tracing::warn!(file_url, "mac dnd: cannot convert file url to path");
    return false;
  };

  let display = path.rsplit(['/', '\\']).next().unwrap_or(&path);
  let cef_path = cef::CefString::from(path.as_str());
  let cef_display = cef::CefString::from(display);
  data.add_file(Some(&cef_path), Some(&cef_display));
  true
}

fn file_url_to_path(file_url: &str) -> Option<String> {
  unsafe {
    if let Some(url) = NSURL::URLWithString(&NSString::from_str(file_url))
      && url.isFileURL()
    {
      let path_url = url.filePathURL().unwrap_or(url);
      if let Some(path) = path_url.path() {
        let path = path.to_string();
        if !path.is_empty() && !path.starts_with("/.file/") {
          return Some(path);
        }
      }
    }
  }

  if let Ok(url) = url::Url::parse(file_url)
    && url.scheme() == "file"
  {
    let path = url
      .to_file_path()
      .ok()
      .and_then(|path| path.to_str().map(ToOwned::to_owned));
    if path.as_deref().is_some_and(|path| !path.starts_with("/.file/")) {
      return path;
    }
  }

  (!file_url.is_empty()).then(|| file_url.to_string())
}
