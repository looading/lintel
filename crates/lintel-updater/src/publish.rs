use crate::Release;
use anyhow::{Context, Result, ensure};
use std::{fs, path::PathBuf};

/// CI 发布输入，私钥只在生成更新清单时读取。
pub struct ReleaseOptions {
  pub bundle: PathBuf,
  pub archive_url: String,
  pub channel: String,
  pub notes: String,
  pub expires_in: u64,
  pub private_key: PathBuf,
  pub output_dir: PathBuf,
  pub target: Option<String>,
}

/// 生成 update.tar.gz 与签名 latest.json，不上传任何产物。
pub fn publish(options: &ReleaseOptions) -> Result<PathBuf> {
  if options.bundle.join("lintel-bundle.json").is_file() {
    return publish_windows(options);
  }
  #[cfg(not(target_os = "macos"))]
  {
    let _ = options;
    anyhow::bail!("macOS update archives must be generated on macOS");
  }
  #[cfg(target_os = "macos")]
  {
    ensure!(!options.output_dir.exists(), "release output directory already exists");
    ensure!(options.expires_in > 0, "manifest lifetime must be positive");
    crate::validate_url(&reqwest::Url::parse(&options.archive_url)?, true)?;
    let info = plist::Value::from_file(options.bundle.join("Contents/Info.plist"))?;
    let info = info.as_dictionary().context("invalid application metadata")?;
    let string = |name| {
      info
        .get(name)
        .and_then(plist::Value::as_string)
        .with_context(|| format!("missing {name}"))
    };
    let architectures = std::process::Command::new("/usr/bin/lipo")
      .arg("-archs")
      .arg(
        options
          .bundle
          .join("Contents/MacOS")
          .join(string("CFBundleExecutable")?),
      )
      .output()?;
    let target = options.target.as_deref().unwrap_or(crate::target());
    let required_arch = match target {
      "aarch64-apple-darwin" => "arm64",
      "x86_64-apple-darwin" => "x86_64",
      _ => anyhow::bail!("unsupported macOS update target {target}"),
    };
    ensure!(
      architectures.status.success()
        && String::from_utf8_lossy(&architectures.stdout)
          .split_whitespace()
          .any(|arch| arch == required_arch),
      "update bundle does not support requested target {target}"
    );
    let parent = options
      .output_dir
      .parent()
      .filter(|p| !p.as_os_str().is_empty())
      .unwrap_or(std::path::Path::new("."));
    fs::create_dir_all(parent)?;
    let temporary = tempfile::Builder::new().prefix(".lintel-release-").tempdir_in(parent)?;
    let staged = temporary.path().join("release");
    fs::create_dir(&staged)?;
    let archive = staged.join("update.tar.gz");
    crate::create_archive(&options.bundle, &archive)?;
    let (size, sha256) = crate::archive_digest(&archive)?;
    let release = Release {
      schema: 1,
      app_id: string("CFBundleIdentifier")?.to_owned(),
      version: string("CFBundleShortVersionString")?.to_owned(),
      channel: options.channel.clone(),
      target: target.to_owned(),
      url: options.archive_url.clone(),
      size,
      sha256,
      expires_at: crate::manifest::now()?
        .checked_add(options.expires_in)
        .context("manifest expiry overflow")?,
      notes: options.notes.clone(),
    };
    fs::write(
      staged.join("latest.json"),
      crate::sign_release(&release, &options.private_key)?,
    )?;
    fs::rename(staged, &options.output_dir)?;
    Ok(options.output_dir.clone())
  }
}

fn publish_windows(options: &ReleaseOptions) -> Result<PathBuf> {
  let info = lintel_build::validate_windows_bundle(&options.bundle)?;
  ensure!(
    options.target.as_deref().is_none_or(|target| target == info.target),
    "Windows release target differs from portable bundle"
  );
  ensure!(
    !options.output_dir.try_exists()? && options.expires_in > 0,
    "release output already exists or lifetime is zero"
  );
  crate::validate_url(&reqwest::Url::parse(&options.archive_url)?, true)?;
  let parent = options
    .output_dir
    .parent()
    .filter(|parent| !parent.as_os_str().is_empty())
    .unwrap_or_else(|| std::path::Path::new("."));
  fs::create_dir_all(parent)?;
  let stage = tempfile::Builder::new().prefix(".lintel-release-").tempdir_in(parent)?;
  let output = stage.path().join("release");
  fs::create_dir(&output)?;
  let archive = output.join("update.tar.gz");
  crate::archive::create_windows_archive(&options.bundle, &info, &archive)?;
  let (size, sha256) = crate::archive_digest(&archive)?;
  let release = Release {
    schema: 1,
    app_id: info.identifier,
    version: info.version,
    channel: options.channel.clone(),
    target: info.target,
    url: options.archive_url.clone(),
    size,
    sha256,
    expires_at: crate::manifest::now()?
      .checked_add(options.expires_in)
      .context("manifest expiry overflow")?,
    notes: options.notes.clone(),
  };
  fs::write(
    output.join("latest.json"),
    crate::sign_release(&release, &options.private_key)?,
  )?;
  fs::rename(output, &options.output_dir)?;
  Ok(options.output_dir.clone())
}
