use std::time::Instant;

pub struct ExecCost {
  start: Instant,
}

impl ExecCost {
  pub fn new() -> Self {
    Self { start: Instant::now() }
  }
}

impl ExecCost {
  pub fn current_cost_ms(&self) -> u128 {
    self.start.elapsed().as_millis()
  }

  pub fn current_cost_ms_from(&self, start: u128) -> u128 {
    self.current_cost_ms() - start
  }
}
