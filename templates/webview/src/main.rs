fn main() -> lintel::Result<()> {
  lintel::webview_app()
    .id(concat!("dev.example.", env!("CARGO_PKG_NAME")))
    .title("Lintel WebView")
    .local_page(include_str!("../ui/index.html"))
    .command("greet", greet)
    .run(env!("CARGO_PKG_VERSION"))
}

fn greet(name: String) -> lintel::Result<String> {
  Ok(format!("Hello, {}! Sent from Rust.", name.trim()))
}
