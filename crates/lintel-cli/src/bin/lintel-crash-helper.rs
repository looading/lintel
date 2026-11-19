fn main() -> anyhow::Result<()> {
  if !lintel_crash::helper_entry()? {
    anyhow::bail!("Lintel crash helper must be started by CrashReporter");
  }
  Ok(())
}
