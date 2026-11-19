use perf_mon::PerfMon;

pub mod perf_mon;

#[derive(Default)]
pub struct ShellContext {
  pub perf_mon: PerfMon,
}

pub struct ShellContextBuilder {
  pub perf_mon: Option<PerfMon>,
}
