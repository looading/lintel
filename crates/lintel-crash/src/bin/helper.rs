fn main() -> anyhow::Result<()> {
  anyhow::ensure!(
    lintel_crash::helper_entry()?,
    "this executable is a private Lintel crash helper"
  );
  Ok(())
}
