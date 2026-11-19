#[derive(Clone, Debug, Default)]
pub struct DraggedFile {
  pub full_path: String,
  pub display_name: String,
}

pub trait OsrDragEvents: Send + Sync + 'static {
  fn on_drag_enter(
    &self,
    drag_data: &cef::DragData,
    ev: cef::MouseEvent,
    allowed: cef::DragOperationsMask,
  ) -> cef::DragOperationsMask;

  fn on_drag_over(&self, ev: cef::MouseEvent, allowed: cef::DragOperationsMask) -> cef::DragOperationsMask;

  fn on_drag_leave(&self);

  fn on_drop(&self, ev: cef::MouseEvent, allowed: cef::DragOperationsMask) -> cef::DragOperationsMask;
}
