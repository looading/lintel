use cef::{rc::*, *};
use iced::mouse;
use lintel_lib_iced::iced;
use std::ptr::null_mut;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::webview::BrowserId;

#[derive(Clone)]
pub struct IcyDisplayHandler {
  tx: UnboundedSender<DisplayEvent>,
}

#[derive(Debug)]
pub enum DisplayEvent {
  TitleChanged {
    browser_id: BrowserId,
    title: String,
  },
  UrlChanged {
    browser_id: BrowserId,
    url: String,
  },
  CursorChanged {
    browser_id: BrowserId,
    cursor_type: mouse::Interaction,
  },
}

impl IcyDisplayHandler {
  pub fn new() -> (Self, UnboundedReceiver<DisplayEvent>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    (Self { tx }, rx)
  }
}

pub(crate) struct DisplayHandlerBuilder {
  object: *mut RcImpl<sys::_cef_display_handler_t, Self>,
  display_handler: IcyDisplayHandler,
}

impl DisplayHandlerBuilder {
  pub(crate) fn build(display: IcyDisplayHandler) -> DisplayHandler {
    DisplayHandler::new(Self {
      object: null_mut(),
      display_handler: display,
    })
  }
}

impl WrapDisplayHandler for DisplayHandlerBuilder {
  fn wrap_rc(&mut self, object: *mut RcImpl<sys::_cef_display_handler_t, Self>) {
    self.object = object;
  }
}

impl Rc for DisplayHandlerBuilder {
  fn as_base(&self) -> &sys::cef_base_ref_counted_t {
    unsafe {
      let base = &*self.object;
      std::mem::transmute(&base.cef_object)
    }
  }
}

impl Clone for DisplayHandlerBuilder {
  fn clone(&self) -> Self {
    let object = unsafe {
      let rc_impl = &mut *self.object;
      rc_impl.interface.add_ref();
      rc_impl
    };

    Self {
      object,
      display_handler: self.display_handler.clone(),
    }
  }
}

impl ImplDisplayHandler for DisplayHandlerBuilder {
  fn get_raw(&self) -> *mut sys::_cef_display_handler_t {
    self.object.cast()
  }

  fn on_cursor_change(
    &self,
    browser: Option<&mut Browser>,
    #[cfg(target_os = "windows")] _cursor: sys::HCURSOR,
    #[cfg(target_os = "macos")] _cursor: *mut u8,
    type_: CursorType,
    _custom_cursor_info: Option<&CursorInfo>,
  ) -> ::std::os::raw::c_int {
    let Some(browser) = browser else {
      return false as _;
    };
    _ = self
      .display_handler
      .tx
      .send(DisplayEvent::CursorChanged {
        browser_id: browser.identifier().into(),
        cursor_type: map_cursor(type_),
      })
      .inspect_err(|err| {
        tracing::error!(?err, "cannot send cursor changed event");
      });
    true as _
  }

  fn on_address_change(&self, browser: Option<&mut Browser>, _frame: Option<&mut Frame>, url: Option<&CefString>) {
    let Some(browser) = browser else {
      return;
    };

    let Some(url) = url else {
      return;
    };
    let event = DisplayEvent::UrlChanged {
      browser_id: browser.identifier().into(),
      url: url.to_string(),
    };
    if let Err(err) = self.display_handler.tx.send(event) {
      tracing::error!(%err, "cannot send loading end event");
    }
  }

  fn on_title_change(&self, browser: Option<&mut Browser>, title: Option<&CefString>) {
    let Some(browser) = browser else {
      return;
    };
    let Some(title) = title else {
      return;
    };
    let event = DisplayEvent::TitleChanged {
      browser_id: browser.identifier().into(),
      title: title.to_string(),
    };
    if let Err(err) = self.display_handler.tx.send(event) {
      tracing::error!(%err, "cannot send loading end event");
    }
  }

  fn on_console_message(
    &self,
    browser: Option<&mut Browser>,
    level: LogSeverity,
    message: Option<&CefString>,
    source: Option<&CefString>,
    line: ::std::os::raw::c_int,
  ) -> ::std::os::raw::c_int {
    // 正式包命令行不打印console log
    let output_to_console = !cfg!(debug_assertions) as _;
    let Some(browser) = browser else {
      return output_to_console;
    };
    let Some(message) = message else {
      return output_to_console;
    };
    use cef::sys::cef_log_severity_t;
    let browser_id = browser.identifier();
    let source = match source {
      Some(source) => format!("{source}:{line}"),
      _ => format!("Unknown:{line}"),
    };

    static LOG: &str = "renderer::console::log";
    match level.as_ref() {
      cef_log_severity_t::LOGSEVERITY_ERROR | cef_log_severity_t::LOGSEVERITY_FATAL => {
        let url = browser
          .main_frame()
          .as_ref()
          .map(|frame| CefString::from(&frame.url()).to_string())
          .unwrap_or_else(|| "Unknown".into());
        #[cfg(not(debug_assertions))]
        tracing::error!(url, browser_id, error = %message, source, LOG);
      }
      cef_log_severity_t::LOGSEVERITY_WARNING => {
        //tracing::warn!(browser_id, %message, source, LOG);
      }
      _ => {
        //tracing::info!(browser_id, %message, source, LOG);
      }
    }
    output_to_console
  }
}

fn map_cursor(type_: cef::CursorType) -> mouse::Interaction {
  use cef::sys::cef_cursor_type_t;
  match type_.as_ref() {
    cef_cursor_type_t::CT_HAND => mouse::Interaction::Pointer,
    cef_cursor_type_t::CT_WAIT => mouse::Interaction::Wait,
    cef_cursor_type_t::CT_HELP => mouse::Interaction::Help,
    cef_cursor_type_t::CT_EASTWESTRESIZE | cef_cursor_type_t::CT_WESTRESIZE | cef_cursor_type_t::CT_EASTRESIZE => {
      mouse::Interaction::ResizingHorizontally
    }
    cef_cursor_type_t::CT_NORTHWESTRESIZE
    | cef_cursor_type_t::CT_SOUTHEASTRESIZE
    | cef_cursor_type_t::CT_NORTHWESTSOUTHEASTRESIZE => mouse::Interaction::ResizingDiagonallyDown,
    cef_cursor_type_t::CT_NORTHSOUTHRESIZE | cef_cursor_type_t::CT_SOUTHRESIZE | cef_cursor_type_t::CT_NORTHRESIZE => {
      mouse::Interaction::ResizingVertically
    }
    cef_cursor_type_t::CT_NORTHEASTSOUTHWESTRESIZE
    | cef_cursor_type_t::CT_SOUTHWESTRESIZE
    | cef_cursor_type_t::CT_NORTHEASTRESIZE => mouse::Interaction::ResizingDiagonallyUp,
    cef_cursor_type_t::CT_CELL => mouse::Interaction::Cell,
    cef_cursor_type_t::CT_IBEAM => mouse::Interaction::Text,
    cef_cursor_type_t::CT_MOVE => mouse::Interaction::Move,
    cef_cursor_type_t::CT_COPY => mouse::Interaction::Copy,
    cef_cursor_type_t::CT_NOTALLOWED => mouse::Interaction::NotAllowed,
    cef_cursor_type_t::CT_ZOOMIN => mouse::Interaction::ZoomIn,
    cef_cursor_type_t::CT_ZOOMOUT => mouse::Interaction::ZoomOut,
    cef_cursor_type_t::CT_GRAB => mouse::Interaction::Grab,
    cef_cursor_type_t::CT_GRABBING => mouse::Interaction::Grabbing,
    cef_cursor_type_t::CT_CROSS => mouse::Interaction::Crosshair,
    cef_cursor_type_t::CT_COLUMNRESIZE => mouse::Interaction::ResizingColumn,
    cef_cursor_type_t::CT_ROWRESIZE => mouse::Interaction::ResizingRow,
    cef_cursor_type_t::CT_VERTICALTEXT => mouse::Interaction::VerticalText,
    cef_cursor_type_t::CT_ALIAS => mouse::Interaction::Alias,
    cef_cursor_type_t::CT_NODROP => mouse::Interaction::NoDrop,
    cef_cursor_type_t::CT_PROGRESS => mouse::Interaction::Progress,
    cef_cursor_type_t::CT_DND_NONE
    | cef_cursor_type_t::CT_DND_MOVE
    | cef_cursor_type_t::CT_DND_COPY
    | cef_cursor_type_t::CT_DND_LINK => mouse::Interaction::DndAsk,
    cef_cursor_type_t::CT_MIDDLE_PANNING_VERTICAL
    | cef_cursor_type_t::CT_MIDDLEPANNING
    | cef_cursor_type_t::CT_EASTPANNING
    | cef_cursor_type_t::CT_NORTHPANNING
    | cef_cursor_type_t::CT_NORTHEASTPANNING
    | cef_cursor_type_t::CT_NORTHWESTPANNING
    | cef_cursor_type_t::CT_SOUTHPANNING
    | cef_cursor_type_t::CT_SOUTHEASTPANNING
    | cef_cursor_type_t::CT_SOUTHWESTPANNING
    | cef_cursor_type_t::CT_WESTPANNING
    | cef_cursor_type_t::CT_CONTEXTMENU
    | cef_cursor_type_t::CT_MIDDLE_PANNING_HORIZONTAL
    | cef_cursor_type_t::CT_CUSTOM
    | cef_cursor_type_t::CT_NUM_VALUES
    | _ => mouse::Interaction::None,
  }
}
