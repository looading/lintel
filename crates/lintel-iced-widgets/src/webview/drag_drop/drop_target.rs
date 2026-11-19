use cef::{ImplBinaryValue, ImplBrowser, ImplBrowserHost};
use lintel_lib_iced::iced;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;
use windows::Win32::Foundation::{COLORREF, HANDLE, HWND, POINT, POINTL, SIZE};
use windows::Win32::Graphics::Gdi::{
  BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, GetDC, GetObjectW,
  HBITMAP, ReleaseDC, ScreenToClient,
};
use windows::Win32::Graphics::Imaging::{
  CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory, WICConvertBitmapSource,
  WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CLSCTX_INPROC_SERVER, CoCreateInstance, IDataObject};
use windows::Win32::System::Ole::{
  DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE, DoDragDrop, IDropSource, IDropTarget,
  IDropTarget_Impl,
};
use windows::Win32::System::SystemServices::{
  MK_CONTROL, MK_LBUTTON, MK_MBUTTON, MK_RBUTTON, MK_SHIFT, MODIFIERKEYS_FLAGS,
};
use windows::Win32::UI::Shell::{CLSID_DragDropHelper, IDragSourceHelper, IDropTargetHelper, SHDRAGIMAGE};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::core::{Result, implement};

use crate::webview::BrowserId;

use super::DropSourceWin;
use super::data_object::{data_object_to_drag_data, drag_data_to_data_object};

fn drag_operations_to_dropeffect(op: cef::DragOperationsMask) -> DROPEFFECT {
  let mut op_effect = DROPEFFECT_NONE;
  if *op.as_ref() & cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_COPY
    != cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_NONE
  {
    op_effect |= DROPEFFECT_COPY;
  }
  if *op.as_ref() & cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_LINK
    != cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_NONE
  {
    op_effect |= DROPEFFECT_LINK;
  }
  if *op.as_ref() & cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_MOVE
    != cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_NONE
  {
    op_effect |= DROPEFFECT_MOVE;
  }
  op_effect
}

fn dropeffect_to_drag_operations(effect: DROPEFFECT) -> cef::DragOperationsMask {
  let mut operation = cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_NONE;
  if (effect & DROPEFFECT_COPY).0 != 0 {
    operation |= cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_COPY;
  }
  if (effect & DROPEFFECT_LINK).0 != 0 {
    operation |= cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_LINK;
  }
  if (effect & DROPEFFECT_MOVE).0 != 0 {
    operation |= cef::sys::cef_drag_operations_mask_t::DRAG_OPERATION_MOVE;
  }
  operation.into()
}

fn to_mouse_event(
  hwnd: HWND,
  _browser_id: BrowserId,
  webview_bounds: lintel_lib_iced::Rectangle,
  scale_factor: f32,
  pt: &POINTL,
  key_state: MODIFIERKEYS_FLAGS,
) -> Option<cef::MouseEvent> {
  fn cef_modifiers(key_state: MODIFIERKEYS_FLAGS) -> cef::EventFlags {
    let mut flags = cef::sys::cef_event_flags_t::EVENTFLAG_NONE;
    if key_state.contains(MK_CONTROL) {
      flags |= cef::sys::cef_event_flags_t::EVENTFLAG_CONTROL_DOWN;
    }
    if key_state.contains(MK_SHIFT) {
      flags |= cef::sys::cef_event_flags_t::EVENTFLAG_SHIFT_DOWN;
    }
    if key_state.contains(MK_LBUTTON) {
      flags |= cef::sys::cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON;
    }
    if key_state.contains(MK_MBUTTON) {
      flags |= cef::sys::cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON;
    }
    if key_state.contains(MK_RBUTTON) {
      flags |= cef::sys::cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON;
    }
    flags.into()
  }
  let mut p = windows::Win32::Foundation::POINT { x: pt.x, y: pt.y };
  unsafe {
    _ = ScreenToClient(hwnd, &mut p);
  }

  let x = p.x as f32 / scale_factor;
  let y = p.y as f32 / scale_factor;
  if webview_bounds.contains(iced::Point::new(x, y)) {
    cef::MouseEvent {
      x: (x - webview_bounds.x) as _,
      y: (y - webview_bounds.y) as _,
      modifiers: cef_modifiers(key_state).as_ref().0 as _,
    }
    .into()
  } else {
    None
  }
}

#[implement(windows::Win32::System::Ole::IDropTarget)]
pub struct DropTargetWin {
  hwnd: HWND,
  current_drag_data: RefCell<Option<cef::DragData>>,
  current_browser: RefCell<Option<BrowserId>>,
  drop_target_helper: Option<IDropTargetHelper>,
}

impl DropTargetWin {
  pub fn create(hwnd: HWND) -> IDropTarget {
    DropTargetWin {
      hwnd,
      current_drag_data: RefCell::new(None),
      current_browser: RefCell::new(None),
      drop_target_helper: unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER).ok() },
    }
    .into()
  }
}

thread_local! {
    static DRAGGING_BROWSER: std::cell::Cell<Option<(BrowserId, iced::Rectangle, f32)>> = const { std::cell::Cell::new(None) };
    static DROP_TARGET_BROWSERS: RefCell<BTreeMap<BrowserId, (isize, iced::Rectangle, f32)>> = const { RefCell::new(BTreeMap::new()) };
}

fn set_dragging_browser(browser_id: BrowserId, rect: iced::Rectangle, scale_factor: f32) {
  DRAGGING_BROWSER.with(|cell| {
    cell.set(Some((browser_id, rect, scale_factor)));
  });
}

fn get_dragging_browser() -> Option<(BrowserId, iced::Rectangle, f32)> {
  DRAGGING_BROWSER.with(|cell| cell.get())
}

fn clear_dragging_browser() {
  DRAGGING_BROWSER.with(|cell| {
    cell.set(None);
  });
}

pub(crate) fn register_browser_drop_target(
  hwnd: HWND,
  browser_id: BrowserId,
  rect: iced::Rectangle,
  scale_factor: f32,
) {
  DROP_TARGET_BROWSERS.with_borrow_mut(|browsers| {
    browsers.insert(browser_id, (hwnd.0 as isize, rect, scale_factor));
  });
}

pub(crate) fn unregister_browser_drop_target(browser_id: BrowserId) {
  DROP_TARGET_BROWSERS.with_borrow_mut(|browsers| {
    browsers.remove(&browser_id);
  });
}

fn get_drop_target_browser(hwnd: HWND, pt: &POINTL) -> Option<(BrowserId, iced::Rectangle, f32)> {
  let mut p = POINT { x: pt.x, y: pt.y };
  unsafe {
    _ = ScreenToClient(hwnd, &mut p);
  }

  DROP_TARGET_BROWSERS.with_borrow(|browsers| {
    browsers
      .iter()
      .rev()
      .find_map(|(browser_id, (target_hwnd, bounds, scale_factor))| {
        if *target_hwnd != hwnd.0 as isize {
          return None;
        }

        let scale_factor = scale_factor.max(1.0);
        let x = p.x as f32 / scale_factor;
        let y = p.y as f32 / scale_factor;
        bounds
          .contains(iced::Point::new(x, y))
          .then_some((*browser_id, *bounds, scale_factor))
      })
  })
}

fn get_active_browser(hwnd: HWND, pt: &POINTL) -> Option<(BrowserId, iced::Rectangle, f32)> {
  get_dragging_browser().or_else(|| get_drop_target_browser(hwnd, pt))
}

fn get_or_create_drag_data(
  current_drag_data: &RefCell<Option<cef::DragData>>,
  pdataobj: Option<&IDataObject>,
) -> Option<cef::DragData> {
  if let Some(existing) = current_drag_data.borrow().as_ref() {
    cef::ImplDragData::clone(existing)
  } else {
    let data = pdataobj.and_then(data_object_to_drag_data)?;
    *current_drag_data.borrow_mut() = cef::ImplDragData::clone(&data);
    Some(data)
  }
}

fn cloned_drag_data(current_drag_data: &RefCell<Option<cef::DragData>>) -> Option<cef::DragData> {
  current_drag_data.borrow().as_ref().and_then(cef::ImplDragData::clone)
}

impl IDropTarget_Impl for DropTargetWin_Impl {
  fn DragEnter(
    &self,
    pdataobj: Option<&IDataObject>,
    grfkeystate: MODIFIERKEYS_FLAGS,
    pt: &POINTL,
    pdweffect: *mut DROPEFFECT,
  ) -> Result<()> {
    if let Some(drop_target_helper) = &self.drop_target_helper {
      unsafe {
        drop_target_helper.DragEnter(
          self.hwnd,
          pdataobj,
          std::ptr::from_ref(&POINT { x: pt.x, y: pt.y }),
          *pdweffect,
        )?;
      }
    }
    let mut drag_data = get_or_create_drag_data(&self.current_drag_data, pdataobj);
    let effect_in = unsafe { pdweffect.as_ref().copied().unwrap_or(DROPEFFECT_NONE) };
    let allowed = dropeffect_to_drag_operations(effect_in);
    if let Some((browser, webview_bounds, scale_factor)) = get_active_browser(self.hwnd, pt)
      && let Some(host) = cef::browser_host_get_browser_by_identifier(browser.inner()).and_then(|b| b.host())
      && let Some(event) = to_mouse_event(self.hwnd, browser, webview_bounds, scale_factor, pt, grfkeystate)
    {
      host.drag_target_drag_enter(drag_data.as_mut(), Some(&event), allowed);
      host.drag_target_drag_over(Some(&event), allowed);
      *self.current_browser.borrow_mut() = Some(browser);
      unsafe {
        if !pdweffect.is_null() {
          *pdweffect = if effect_in == DROPEFFECT_NONE {
            DROPEFFECT_COPY
          } else {
            effect_in
          };
        }
      }

      //let new_effect = drag_operations_to_dropeffect(result_mask);
      //unsafe {
      //  if !pdweffect.is_null() {
      //    *pdweffect = new_effect;
      //  }
      //}
    } else {
      unsafe {
        if !pdweffect.is_null() {
          *pdweffect = DROPEFFECT_NONE;
        }
      }
    }

    Ok(())
  }

  fn DragOver(&self, grfkeystate: MODIFIERKEYS_FLAGS, pt: &POINTL, pdweffect: *mut DROPEFFECT) -> Result<()> {
    if let Some(drop_target_helper) = &self.drop_target_helper {
      unsafe {
        drop_target_helper.DragOver(std::ptr::from_ref(&POINT { x: pt.x, y: pt.y }), *pdweffect)?;
      }
    }
    let effect_in = unsafe { pdweffect.as_ref().copied().unwrap_or(DROPEFFECT_NONE) };
    let allowed = dropeffect_to_drag_operations(effect_in);
    if let Some((browser, webview_bounds, scale_factor)) = get_active_browser(self.hwnd, pt) {
      if let Some(previous_browser) = *self.current_browser.borrow()
        && previous_browser != browser
        && let Some(host) = cef::browser_host_get_browser_by_identifier(previous_browser.inner()).and_then(|b| b.host())
      {
        host.drag_target_drag_leave();
      }

      let already_entered = *self.current_browser.borrow() == Some(browser);
      // 先清空旧状态，避免处理失败残留旧浏览器
      *self.current_browser.borrow_mut() = None;
      if let Some(event) = to_mouse_event(self.hwnd, browser, webview_bounds, scale_factor, pt, grfkeystate)
        && let Some(host) = cef::browser_host_get_browser_by_identifier(browser.inner()).and_then(|b| b.host())
      {
        if !already_entered {
          let mut drag_data = cloned_drag_data(&self.current_drag_data);
          host.drag_target_drag_enter(drag_data.as_mut(), Some(&event), allowed);
        }
        host.drag_target_drag_over(Some(&event), allowed);
        *self.current_browser.borrow_mut() = Some(browser);
        unsafe {
          if !pdweffect.is_null() {
            *pdweffect = if effect_in == DROPEFFECT_NONE {
              DROPEFFECT_COPY
            } else {
              effect_in
            };
          }
        }
      }
    } else if let Some(previous_browser) = self.current_browser.borrow_mut().take()
      && let Some(host) = cef::browser_host_get_browser_by_identifier(previous_browser.inner()).and_then(|b| b.host())
    {
      host.drag_target_drag_leave();
      unsafe {
        if !pdweffect.is_null() {
          *pdweffect = DROPEFFECT_NONE;
        }
      }
    }
    //let new_effect = drag_operations_to_dropeffect(result_mask);
    //unsafe {
    //  if !pdweffect.is_null() {
    //    *pdweffect = new_effect;
    //  }
    //}

    Ok(())
  }

  fn DragLeave(&self) -> Result<()> {
    *self.current_drag_data.borrow_mut() = None;
    let current_browser = self.current_browser.borrow_mut().take();
    if let Some(drop_target_helper) = &self.drop_target_helper {
      unsafe {
        drop_target_helper.DragLeave()?;
      }
    }
    if let Some(browser) = current_browser.or_else(|| get_dragging_browser().map(|(browser, ..)| browser))
      && let Some(host) = cef::browser_host_get_browser_by_identifier(browser.inner()).and_then(|b| b.host())
    {
      host.drag_target_drag_leave();
    }
    Ok(())
  }

  fn Drop(
    &self,
    pdataobj: Option<&IDataObject>,
    grfkeystate: MODIFIERKEYS_FLAGS,
    pt: &POINTL,
    pdweffect: *mut DROPEFFECT,
  ) -> Result<()> {
    if let Some(drop_target_helper) = &self.drop_target_helper {
      unsafe {
        drop_target_helper.Drop(pdataobj, std::ptr::from_ref(&POINT { x: pt.x, y: pt.y }), *pdweffect)?;
      }
    }
    let effect_in = unsafe { pdweffect.as_ref().copied().unwrap_or(DROPEFFECT_NONE) };
    let allowed = dropeffect_to_drag_operations(effect_in);
    if let Some((browser, webview_bounds, scale_factor)) = get_active_browser(self.hwnd, pt)
      && let Some(event) = to_mouse_event(self.hwnd, browser, webview_bounds, scale_factor, pt, grfkeystate)
      && let Some(host) = cef::browser_host_get_browser_by_identifier(browser.inner()).and_then(|b| b.host())
    {
      if let Some(previous_browser) = *self.current_browser.borrow()
        && previous_browser != browser
        && let Some(previous_host) =
          cef::browser_host_get_browser_by_identifier(previous_browser.inner()).and_then(|b| b.host())
      {
        previous_host.drag_target_drag_leave();
      }

      if *self.current_browser.borrow() != Some(browser) {
        let mut drag_data = get_or_create_drag_data(&self.current_drag_data, pdataobj);
        host.drag_target_drag_enter(drag_data.as_mut(), Some(&event), allowed);
      }
      host.drag_target_drag_over(Some(&event), allowed);
      host.drag_target_drop(Some(&event));
      unsafe {
        if !pdweffect.is_null() {
          *pdweffect = if effect_in == DROPEFFECT_NONE {
            DROPEFFECT_COPY
          } else {
            effect_in
          };
        }
      }
    }

    // let result_mask = self.callback.on_drop(ev, allowed);
    // let new_effect = drag_operations_to_dropeffect(result_mask);
    // unsafe {
    //   if !pdweffect.is_null() {
    //     *pdweffect = new_effect;
    //   }
    // }
    *self.current_drag_data.borrow_mut() = None;
    *self.current_browser.borrow_mut() = None;

    Ok(())
  }
}

/// Can only be called from the winit main thread.
pub(crate) fn start_dragging(
  raw_handle: &crate::webview::instance::RawHandle,
  browser_id: BrowserId,
  bounds: iced::Rectangle,
  scale_factor: f32,
  drag_data: &cef::DragData,
  allowed: cef::DragOperationsMask,
) -> Result<cef::DragOperationsMask> {
  let data_object: IDataObject = drag_data_to_data_object(drag_data)?;

  unsafe {
    if let Some(image) = create_drag_image(drag_data)
      && let Ok(instance) = CoCreateInstance::<_, IDragSourceHelper>(&CLSID_DragDropHelper, None, CLSCTX_ALL)
    {
      _ = instance.InitializeFromBitmap(&image, &data_object).inspect_err(|err| {
        tracing::warn!(?err, "cannot create bitmap");
      });
    }
  }
  let drop_source: IDropSource = DropSourceWin::create();
  let effect = drag_operations_to_dropeffect(allowed);
  let mut result_effect = DROPEFFECT_NONE;
  set_dragging_browser(browser_id, bounds, scale_factor);
  unsafe {
    let hr = DoDragDrop(&data_object, &drop_source, effect, &mut result_effect);
    if hr != windows::Win32::Foundation::DRAGDROP_S_DROP {
      result_effect = DROPEFFECT_NONE;
    }
    let mut p = POINT { x: 0, y: 0 };
    GetCursorPos(std::ptr::from_mut(&mut p))?;
    if let Some((browser, webview_bounds, scale_factor)) = get_dragging_browser()
      && let Some(event) = to_mouse_event(
        HWND(raw_handle.handle() as _),
        browser,
        webview_bounds,
        scale_factor,
        &POINTL { x: p.x, y: p.y },
        Default::default(),
      )
      && let Some(host) = cef::browser_host_get_browser_by_identifier(browser.inner()).and_then(|b| b.host())
    {
      host.drag_source_ended_at(event.x, event.y, allowed);
      host.drag_source_system_drag_ended();
    }
    clear_dragging_browser();
  }

  Ok(dropeffect_to_drag_operations(result_effect))
}

fn create_drag_image(drag_data: &cef::DragData) -> Option<SHDRAGIMAGE> {
  use cef::ImplDragData;
  use cef::ImplImage;

  let mut width = 0;
  let mut height = 0;
  if drag_data.has_image() == 1
    && let Some(image) = drag_data.image()
    && let Some(png) = image.as_png(1.5, true as _, Some(&mut width), Some(&mut height))
  {
    let size = png.size();
    let bytes = unsafe { std::slice::from_raw_parts(png.raw_data().cast(), size) };
    //let bytes = include_bytes!("./icon.png");
    let hbitmap = read_bytes_to_hbitmap(bytes).ok()?;
    unsafe {
      let mut bitmap: BITMAP = BITMAP::default();
      let (width, height) = if 0
        == GetObjectW(
          hbitmap,
          std::mem::size_of::<BITMAP>() as i32,
          Some(std::ptr::from_mut(&mut bitmap).cast()),
        ) {
        (128, 128)
      } else {
        (bitmap.bmWidth, bitmap.bmHeight)
      };
      //let mut cursor_pos = POINT::default();
      //GetCursorPos(std::ptr::from_mut(&mut cursor_pos));
      return Some(SHDRAGIMAGE {
        sizeDragImage: SIZE { cx: width, cy: height },
        ptOffset: POINT { x: 0, y: 0 },
        hbmpDragImage: hbitmap,
        crColorKey: COLORREF(0x00000000),
      });
    }
  }
  None
}

fn read_bytes_to_hbitmap(bytes: &[u8]) -> Result<HBITMAP> {
  unsafe {
    let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;

    let stream = factory.CreateStream()?;
    stream.InitializeFromMemory(bytes)?;

    let decoder = factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;

    let frame = decoder.GetFrame(0)?;

    let mut width: u32 = 0;
    let mut height: u32 = 0;
    frame.GetSize(&mut width, &mut height)?;
    let mut pixel_buf: Vec<u8> = vec![0; (width * height * 4) as usize];
    let pixel_format = frame.GetPixelFormat()?;
    if pixel_format != GUID_WICPixelFormat32bppPBGRA {
      let bitmap_source = WICConvertBitmapSource(&GUID_WICPixelFormat32bppPBGRA, &frame)?;
      bitmap_source.CopyPixels(std::ptr::null(), width * 4, &mut pixel_buf)?;
    } else {
      frame.CopyPixels(std::ptr::null(), width * 4, &mut pixel_buf)?;
    }

    Ok(CreateBitmap(
      width as i32,
      height as i32,
      1,
      32,
      Some(pixel_buf.as_ptr().cast()),
    ))
  }
}
