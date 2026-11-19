//! The browser-side AppKit host contract from cef-rs's `SimpleApplication`.

use std::cell::Cell;

use anyhow::{Context, Result, ensure};
use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
use objc2::{
  ClassType, MainThreadMarker, MainThreadOnly, ProtocolType, define_class, extern_methods, msg_send,
  rc::Retained,
  runtime::{Bool, NSObjectProtocol},
};
use objc2_app_kit::{NSApplication, NSEvent};

thread_local! {
  // AppKit has one main-thread application object. Keep state outside the subclass because
  // AppKit's inherited sharedApplication initializes it without running a Rust ivar initializer.
  static HANDLING_SEND_EVENT: Cell<Bool> = const { Cell::new(Bool::NO) };
}

struct RestoreSendEvent<'a>(&'a Cell<Bool>, Bool);

impl Drop for RestoreSendEvent<'_> {
  fn drop(&mut self) {
    self.0.set(self.1);
  }
}

define_class!(
  // SAFETY: This subclass adds no ivars, and all AppKit methods run on the main thread.
  #[unsafe(super(NSApplication))]
  #[thread_kind = MainThreadOnly]
  #[name = "LintelCefApplication"]
  struct LintelCefApplication;

  impl LintelCefApplication {
    #[unsafe(method(sendEvent:))]
    unsafe fn send_event(&self, event: &NSEvent) {
      HANDLING_SEND_EVENT.with(|handling| {
        let _restore = RestoreSendEvent(handling, handling.replace(Bool::YES));
        // SAFETY: Call the inherited NSApplication method with the original event argument.
        let _: () = unsafe { msg_send![super(self), sendEvent: event] };
      });
    }
  }

  // SAFETY: These selectors implement CEF's main-thread sendEvent state with AppKit's Bool ABI.
  unsafe impl CrAppProtocol for LintelCefApplication {
    #[unsafe(method(isHandlingSendEvent))]
    unsafe fn is_handling_send_event(&self) -> Bool {
      HANDLING_SEND_EVENT.with(Cell::get)
    }
  }

  unsafe impl CrAppControlProtocol for LintelCefApplication {
    #[unsafe(method(setHandlingSendEvent:))]
    unsafe fn set_handling_send_event(&self, value: Bool) {
      HANDLING_SEND_EVENT.with(|handling| handling.set(value));
    }
  }

  unsafe impl CefAppProtocol for LintelCefApplication {}
);

impl LintelCefApplication {
  extern_methods! {
    // SAFETY: The inherited class method returns the singleton. It may be a different
    // NSApplication subclass if someone initialized AppKit first.
    #[unsafe(method(sharedApplication))]
    fn shared_application(mtm: MainThreadMarker) -> Retained<NSApplication>;
  }
}

pub(super) fn install() -> Result<()> {
  let mtm = MainThreadMarker::new().context("CEF AppKit host must initialize on the main thread")?;
  let app = LintelCefApplication::shared_application(mtm);
  ensure!(
    app.isKindOfClass(LintelCefApplication::class()),
    "AppKit created a different NSApplication before the Lintel CEF host"
  );
  // The Framework registers the Chromium control protocol. The empty CefAppProtocol
  // marker is declared in the SDK header but need not be emitted by the Framework.
  let protocol =
    <dyn CrAppControlProtocol as ProtocolType>::protocol().context("CEF AppKit control protocol is unavailable")?;
  ensure!(
    app.conformsToProtocol(protocol),
    "Lintel AppKit host does not conform to CrAppControlProtocol"
  );
  tracing::info!("CEF AppKit application initialized");
  Ok(())
}
