use windows::Win32::Foundation::{BOOL, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, S_OK, TRUE};
use windows::Win32::System::Ole::{DROPEFFECT, IDropSource, IDropSource_Impl};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::core::HRESULT;
use windows::core::implement;

#[implement(windows::Win32::System::Ole::IDropSource)]
pub struct DropSourceWin;

impl DropSourceWin {
  pub fn create() -> IDropSource {
    DropSourceWin.into()
  }
}

impl IDropSource_Impl for DropSourceWin_Impl {
  fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
    if fescapepressed.as_bool() {
      return DRAGDROP_S_CANCEL;
    }

    if !grfkeystate.contains(MK_LBUTTON) {
      return DRAGDROP_S_DROP;
    }

    S_OK
  }

  fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
    DRAGDROP_S_USEDEFAULTCURSORS
  }
}
