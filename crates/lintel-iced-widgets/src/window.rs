use lintel_lib_iced::{
  Point, Size,
  iced::{
    Task,
    window::{self, Action, Id},
  },
};

pub enum CenterRet {
  Size(Size),
  Position(Option<Point>),
}

pub fn center<T>(id: Id) -> Task<T>
where
  T: Send + 'static,
{
  let ret = window::get_screen_info(id)
    .and_then(move |info| window::size(id).map(move |window_sise| (window_sise, info.size)))
    .then(move |(window_sise, screen_size)| {
      window::move_to(
        id,
        Point {
          x: (screen_size.width - window_sise.width) / 2.0,
          y: (screen_size.height - window_sise.height) / 2.0,
        },
      )
    });
  ret
}
