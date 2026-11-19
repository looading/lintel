use lintel_lib_utils::time::get_current_time_millis;
use serde::Serialize;

#[derive(Serialize, Default)]
pub struct ImageDownloadCostEvent {
  start: i64,
  end: i64,
  cost: i64,
  success: bool,
  error: Option<String>,
  url: String,
}

impl ImageDownloadCostEvent {
  pub fn new(url: String) -> Self {
    Self {
      url,
      start: get_current_time_millis(),
      success: true,
      ..Default::default()
    }
  }

  pub fn start(&mut self) {
    self.start = get_current_time_millis();
  }

  pub fn end(&mut self) {
    self.end = get_current_time_millis();
    self.cost = self.end - self.start;
  }

  pub fn set_error(&mut self, err: String) {
    self.error = Some(err);
    self.success = false;
  }
}
