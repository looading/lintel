pub use self::column::column;
pub use self::row::row;
use iced::Point;
use lintel_lib_iced::{self as iced};
pub mod column;
pub mod row;

#[derive(Debug, Clone)]
pub enum Action {
  Idle,
  Picking {
    index: usize,
    origin: Point,
  },
  Dragging {
    index: usize,
    origin: Point,
    last_cursor: Point,
  },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DropPosition {
  Before,
  Swap,
  After,
}

#[derive(Debug, Clone, Copy)]
pub enum DragDirection {
  Horizontal,
  Vertical,
  Unrestricted,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DragEvent {
  Picked {
    index: usize,
  },
  Dropped {
    index: usize,
    target_index: usize,
    drop_position: DropPosition,
  },
  Canceled {
    index: usize,
  },
}
