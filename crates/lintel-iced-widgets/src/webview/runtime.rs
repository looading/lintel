use std::num::NonZero;

use tokio::runtime::Runtime;

pub static CEF_RUNTIME: once_cell::sync::Lazy<Runtime> = once_cell::sync::Lazy::new(|| {
  tokio::runtime::Builder::new_multi_thread()
    .worker_threads(
      std::thread::available_parallelism()
        .unwrap_or(unsafe { NonZero::new_unchecked(4) })
        .get(),
    )
    .thread_name("lintel-cef-task")
    .enable_all()
    .build()
    .unwrap()
});
