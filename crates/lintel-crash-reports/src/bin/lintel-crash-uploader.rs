fn main() -> std::process::ExitCode {
  match lintel_crash_reports::run_uploader() {
    Ok(()) => std::process::ExitCode::SUCCESS,
    Err(error) => {
      eprintln!("{error}");
      std::process::ExitCode::FAILURE
    }
  }
}
