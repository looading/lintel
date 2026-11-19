//! 安装测试示例：第一次带 feed/key 参数启动，重启后记录新版本并正常结束。
#[cfg(target_os = "macos")]
use anyhow::{Context, Result};
#[cfg(target_os = "macos")]
use lintel_updater::Updater;

#[tokio::main(flavor = "current_thread")]
#[cfg(target_os = "macos")]
async fn main() -> Result<()> {
  let exe = std::env::current_exe()?;
  let app = exe
    .ancestors()
    .find(|p| p.extension().is_some_and(|e| e == "app"))
    .context("run this example from a packaged app")?;
  let info = plist::Value::from_file(app.join("Contents/Info.plist"))?;
  let info = info.as_dictionary().context("invalid app metadata")?;
  let value = |name| {
    info
      .get(name)
      .and_then(plist::Value::as_string)
      .context("missing app metadata")
  };
  let version = value("CFBundleShortVersionString")?;
  let args: Vec<_> = std::env::args().collect();
  if args.get(1).is_some_and(|arg| arg == "--held-helper") {
    std::thread::sleep(std::time::Duration::from_secs(2));
    std::fs::write(
      app
        .parent()
        .context("app has no parent")?
        .join(format!("helper-exited-{version}")),
      version,
    )?;
    return Ok(());
  }
  if args.len() == 3 {
    // 模拟随包子进程仍在回收，安装器必须等待，不得提前替换运行中的包。
    let held = std::process::Command::new(&exe).arg("--held-helper").spawn()?;
    println!("HELD_HELPER_PID {}", held.id());
    let public = std::fs::read_to_string(&args[2])?;
    let updater =
      Updater::new(value("CFBundleIdentifier")?, version, &args[1], &public)?.allow_local_http_for_testing();
    if let Some(update) = updater.check().await? {
      let prepared = update
        .download(|p| println!("download {}/{}", p.downloaded, p.total))
        .await?
        .prepare()
        .await?;
      let receipt = prepared.install_on_exit(true).await?;
      println!(
        "UPDATE_SCHEDULED {} helper_pid={}",
        receipt.result_path.display(),
        receipt.installer_pid
      );
    } else {
      println!("NO_UPDATE");
    }
  } else {
    std::fs::write(
      app.parent().context("app has no parent")?.join("launched-version.txt"),
      version,
    )?;
    println!("LAUNCHED_VERSION {version}");
  }
  // GUI 应用在交接后应调用原有 ctx.quit()，让 CEF 与窗口依次退出。
  Ok(())
}

#[cfg(not(target_os = "macos"))]
fn main() {
  eprintln!("The self-update installation example requires macOS.");
}
