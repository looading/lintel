//! Signed updates with application-controlled shutdown and an isolated installer.

mod archive;
mod install;
mod manifest;
mod publish;
#[cfg(test)]
mod tests;
mod windows_install;

use anyhow::{Context, Result, ensure};
use reqwest::{Client, Url};
use semver::Version;
use sha2::{Digest, Sha256};
use std::{io::Write, sync::Arc, time::Duration};

pub use archive::create_archive;
pub use install::{InstallOutcome, InstallReceipt, PreparedUpdate, run_installer_helper};
pub use manifest::{Release, archive_digest, generate_keypair, sign_release};
pub use publish::{ReleaseOptions, publish};
pub use windows_install::{WindowsInstallOptions, install_windows_app, run_windows_launcher};
pub use windows_install::{
  WindowsMaintenanceReport, WindowsRetainedPath, inspect_windows_installation, prune_windows_versions,
  uninstall_windows_app,
};
pub use windows_install::{WindowsStartupHealth, confirm_windows_startup};
pub use windows_install::{install_verified_windows_distribution, install_windows_distribution};

/// 与 UI 无关的更新客户端，可放入业务状态或服务容器。
#[derive(Clone)]
pub struct Updater {
  inner: Arc<Settings>,
}

#[derive(Clone)]
struct Settings {
  app_id: String,
  current: Version,
  feed: Url,
  key: ed25519_dalek::VerifyingKey,
  channel: String,
  local_http: bool,
}

/// 已验签且适用于当前应用的更新；字段不可由调用方修改。
pub struct Update {
  settings: Arc<Settings>,
  release: Release,
  signed_manifest: Vec<u8>,
}

/// 已完成大小和 SHA-256 校验的私有临时更新包。
pub struct DownloadedUpdate {
  archive: tempfile::NamedTempFile,
  settings: Arc<Settings>,
  release: Release,
  signed_manifest: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
pub struct DownloadProgress {
  pub downloaded: u64,
  pub total: u64,
}

impl Updater {
  /// 从已打包的 macOS 主程序读取应用 ID 和当前版本，避免业务重复维护。
  pub fn for_current_app(feed: &str, public_key: &str) -> Result<Self> {
    let (id, version) = install::current_identity()?;
    Self::new(&id, &version, feed, public_key)
  }
  pub fn new(app_id: &str, current_version: &str, feed: &str, public_key: &str) -> Result<Self> {
    Ok(Self {
      inner: Arc::new(Settings {
        app_id: app_id.to_owned(),
        current: Version::parse(current_version)?,
        feed: Url::parse(feed)?,
        key: manifest::verifying_key(public_key)?,
        channel: "stable".to_owned(),
        local_http: false,
      }),
    })
  }

  pub fn channel(mut self, channel: &str) -> Self {
    Arc::make_mut(&mut self.inner).channel = channel.to_owned();
    self
  }

  /// 仅用于显式的本地验收；仍然要求数字 loopback 地址和有效签名。
  pub fn allow_local_http_for_testing(mut self) -> Self {
    Arc::make_mut(&mut self.inner).local_http = true;
    self
  }

  /// 没有更高版本时返回 None；网络或签名错误直接返回错误。
  pub async fn check(&self) -> Result<Option<Update>> {
    let mut response = fetch(&self.inner.feed, self.inner.local_http).await?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
      ensure!(bytes.len() + chunk.len() <= 65536, "update manifest exceeds 64 KiB");
      bytes.extend_from_slice(&chunk);
    }
    let release = manifest::decode(&bytes, &self.inner.key)?;
    ensure!(
      release.app_id == self.inner.app_id,
      "update belongs to a different application"
    );
    ensure!(
      release.channel == self.inner.channel,
      "update belongs to a different channel"
    );
    ensure!(release.target == target(), "update belongs to a different platform");
    ensure!(
      self.inner.channel != "stable" || Version::parse(&release.version)?.pre.is_empty(),
      "prerelease cannot be installed on stable channel"
    );
    validate_url(&Url::parse(&release.url)?, self.inner.local_http)?;
    if !Version::parse(&release.version)?
      .cmp_precedence(&self.inner.current)
      .is_gt()
    {
      return Ok(None);
    }
    Ok(Some(Update {
      settings: self.inner.clone(),
      release,
      signed_manifest: bytes,
    }))
  }
}

impl Update {
  pub fn release(&self) -> &Release {
    &self.release
  }

  /// 下载流写入私有文件，校验通过前不解压或执行任何内容。
  pub async fn download(self, mut progress: impl FnMut(DownloadProgress)) -> Result<DownloadedUpdate> {
    ensure!(
      self.release.expires_at > manifest::now()?,
      "update manifest has expired"
    );
    let mut response = fetch(&Url::parse(&self.release.url)?, self.settings.local_http).await?;
    if let Some(size) = response.content_length() {
      ensure!(size == self.release.size, "update Content-Length mismatch");
    }
    let mut archive = tempfile::Builder::new()
      .prefix("lintel-download-")
      .suffix(".tar.gz")
      .tempfile()?;
    let mut hash = Sha256::new();
    let mut downloaded = 0u64;
    while let Some(chunk) = response.chunk().await? {
      downloaded = downloaded
        .checked_add(chunk.len() as u64)
        .context("download size overflow")?;
      ensure!(downloaded <= self.release.size, "update download exceeds signed size");
      archive.write_all(&chunk)?;
      hash.update(&chunk);
      progress(DownloadProgress {
        downloaded,
        total: self.release.size,
      });
    }
    ensure!(downloaded == self.release.size, "incomplete update download");
    ensure!(
      format!("{:x}", hash.finalize()).eq_ignore_ascii_case(&self.release.sha256),
      "update SHA-256 mismatch"
    );
    archive.as_file().sync_all()?;
    Ok(DownloadedUpdate {
      archive,
      settings: self.settings,
      release: self.release,
      signed_manifest: self.signed_manifest,
    })
  }
}

impl DownloadedUpdate {
  pub async fn prepare(self) -> Result<PreparedUpdate> {
    tokio::task::spawn_blocking(move || install::prepare(self))
      .await
      .context("update preparation task failed")?
  }
}

pub fn target() -> &'static str {
  if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    "aarch64-apple-darwin"
  } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    "x86_64-apple-darwin"
  } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
    "x86_64-pc-windows-msvc"
  } else if cfg!(all(target_os = "windows", target_arch = "x86")) {
    "i686-pc-windows-msvc"
  } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
    "aarch64-pc-windows-msvc"
  } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    "x86_64-unknown-linux-gnu"
  } else {
    "unsupported"
  }
}

fn validate_url(url: &Url, local_http: bool) -> Result<()> {
  ensure!(
    url.username().is_empty() && url.password().is_none(),
    "credentials in update URLs are prohibited"
  );
  let loopback = url
    .host_str()
    .and_then(|host| host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
    .is_some_and(|ip| ip.is_loopback());
  ensure!(
    url.scheme() == "https" || (local_http && url.scheme() == "http" && loopback),
    "updates require HTTPS (numeric loopback HTTP must be explicitly enabled)"
  );
  Ok(())
}

async fn fetch(url: &Url, local_http: bool) -> Result<reqwest::Response> {
  validate_url(url, local_http)?;
  let client = Client::builder()
    .min_tls_version(reqwest::tls::Version::TLS_1_2)
    .connect_timeout(Duration::from_secs(15))
    .timeout(Duration::from_secs(600))
    .redirect(reqwest::redirect::Policy::custom(move |attempt| {
      if attempt.previous().len() >= 5 || validate_url(attempt.url(), local_http).is_err() {
        attempt.stop()
      } else {
        attempt.follow()
      }
    }))
    .build()?;
  let response = client.get(url.clone()).send().await.context("request update source")?;
  ensure!(
    response.status().is_success(),
    "update source returned {}",
    response.status()
  );
  Ok(response)
}
