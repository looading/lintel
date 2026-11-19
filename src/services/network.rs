use std::{
  path::{Path, PathBuf},
  sync::Arc,
  time::Duration,
};

use anyhow::{Context, Result, ensure};
use lintel_core::{
  context::AppContext,
  plugin::{Plugin, PluginLifecycle},
};
use reqwest::{Client, StatusCode, Url, header::HeaderMap, redirect::Policy};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// 每次请求与重定向都受同一来源清单约束。
#[derive(Clone)]
pub struct NetworkOptions {
  pub origins: Vec<String>,
  pub headers: HeaderMap,
  pub proxy: Option<String>,
  pub timeout: Duration,
  pub retries: usize,
  pub max_bytes: u64,
  pub allow_loopback_http: bool,
}

impl Default for NetworkOptions {
  fn default() -> Self {
    Self {
      origins: Vec::new(),
      headers: HeaderMap::new(),
      proxy: None,
      timeout: Duration::from_secs(30),
      retries: 2,
      max_bytes: 64 * 1024 * 1024,
      allow_loopback_http: false,
    }
  }
}

#[derive(Clone)]
pub struct NetworkClient {
  client: Client,
  options: Arc<NetworkOptions>,
}

#[derive(Debug, Clone, Copy)]
pub struct DownloadProgress {
  pub downloaded: u64,
  pub total: Option<u64>,
}

impl NetworkClient {
  pub fn new(options: NetworkOptions) -> Result<Self> {
    ensure!(options.max_bytes > 0 && options.retries <= 8, "invalid network limits");
    for origin in &options.origins {
      let url = Url::parse(origin)?;
      ensure!(
        url.origin().ascii_serialization() == *origin,
        "expected an exact URL origin"
      );
      validate_url(&url, &options)?;
    }
    let options = Arc::new(options);
    let redirects = Arc::clone(&options);
    let mut builder = Client::builder()
      .timeout(options.timeout)
      .default_headers(options.headers.clone())
      .redirect(Policy::custom(move |attempt| {
        if attempt.previous().len() >= 10 {
          attempt.error("too many redirects")
        } else if validate_url(attempt.url(), &redirects).is_err() {
          attempt.error("redirect is outside the network capability")
        } else {
          attempt.follow()
        }
      }));
    if let Some(proxy) = &options.proxy {
      builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    Ok(Self {
      client: builder.build()?,
      options,
    })
  }

  async fn response(&self, url: &str) -> Result<reqwest::Response> {
    let url = Url::parse(url)?;
    validate_url(&url, &self.options)?;
    for attempt in 0..=self.options.retries {
      match self.client.get(url.clone()).send().await {
        Ok(response) if response.status().is_success() => {
          ensure!(
            response
              .content_length()
              .is_none_or(|size| size <= self.options.max_bytes),
            "response exceeds configured size limit"
          );
          return Ok(response);
        }
        Ok(response) => {
          let retryable = response.status().is_server_error() || response.status() == StatusCode::TOO_MANY_REQUESTS;
          if !retryable || attempt == self.options.retries {
            let status = response.status();
            response.error_for_status()?;
            anyhow::bail!("unexpected HTTP status {status}");
          }
        }
        Err(error) if attempt == self.options.retries || !(error.is_connect() || error.is_timeout()) => {
          return Err(error.into());
        }
        Err(_) => {}
      }
      tokio::time::sleep(Duration::from_millis(100 * (1 << attempt))).await;
    }
    anyhow::bail!("request retry limit exhausted")
  }

  pub async fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
    let mut response = self.response(url).await?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
      ensure!(
        (bytes.len() as u64).saturating_add(chunk.len() as u64) <= self.options.max_bytes,
        "response exceeds configured size limit"
      );
      bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
  }

  /// 取消 future 时清理临时文件，只有校验成功后才替换目标。
  pub async fn download(
    &self,
    url: &str,
    destination: impl AsRef<Path>,
    expected_sha256: Option<&str>,
    mut progress: impl FnMut(DownloadProgress),
  ) -> Result<PathBuf> {
    let destination = destination.as_ref();
    let parent = destination
      .parent()
      .filter(|path| !path.as_os_str().is_empty())
      .unwrap_or(Path::new("."));
    let mut response = self.response(url).await?;
    let total = response.content_length();
    let temporary = tempfile::NamedTempFile::new_in(parent)?;
    let mut output = tokio::fs::File::from_std(temporary.reopen()?);
    let mut digest = Sha256::new();
    let mut downloaded = 0_u64;
    while let Some(chunk) = response.chunk().await? {
      downloaded = downloaded
        .checked_add(chunk.len() as u64)
        .context("download size overflow")?;
      ensure!(
        downloaded <= self.options.max_bytes,
        "download exceeds configured size limit"
      );
      output.write_all(&chunk).await?;
      digest.update(&chunk);
      progress(DownloadProgress { downloaded, total });
    }
    output.flush().await?;
    output.sync_all().await?;
    drop(output);
    if let Some(expected) = expected_sha256 {
      ensure!(
        expected.len() == 64 && expected.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid SHA-256"
      );
      ensure!(
        format!("{:x}", digest.finalize()).eq_ignore_ascii_case(expected),
        "download SHA-256 mismatch"
      );
    }
    temporary.persist(destination)?;
    Ok(destination.to_path_buf())
  }
}

fn validate_url(url: &Url, options: &NetworkOptions) -> Result<()> {
  let loopback = match url.host() {
    Some(url::Host::Ipv4(address)) => address.is_loopback(),
    Some(url::Host::Ipv6(address)) => address.is_loopback(),
    _ => false,
  };
  ensure!(
    url.scheme() == "https" || (options.allow_loopback_http && url.scheme() == "http" && loopback),
    "network capability requires HTTPS"
  );
  ensure!(
    url.username().is_empty() && url.password().is_none(),
    "URL credentials are unsupported"
  );
  ensure!(
    options
      .origins
      .iter()
      .any(|origin| *origin == url.origin().ascii_serialization()),
    "URL origin is outside the network capability"
  );
  Ok(())
}

pub fn network(options: NetworkOptions) -> impl FnOnce(&mut AppContext) + Send + Sync {
  move |context| context.register_plugin(Arc::new(NetworkPlugin(options)))
}

struct NetworkPlugin(NetworkOptions);

impl Plugin for NetworkPlugin {
  fn name(&self) -> &str {
    "lintel.network"
  }
}

impl PluginLifecycle for NetworkPlugin {
  fn on_start(&self, context: &AppContext) -> Result<()> {
    let network = Arc::new(NetworkClient::new(self.0.clone())?);
    context.register_service(move || Arc::clone(&network));
    Ok(())
  }

  fn on_stop(&self, context: &AppContext) {
    context.remove_service::<NetworkClient>();
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{
    io::{Read, Write},
    net::TcpListener,
  };

  fn server(reply: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
      let (mut socket, _) = listener.accept().unwrap();
      socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
      let mut request = [0; 1024];
      socket.read(&mut request).unwrap();
      socket.write_all(reply.as_bytes()).unwrap();
    });
    origin
  }

  fn options(origin: &str) -> NetworkOptions {
    NetworkOptions {
      origins: vec![origin.to_owned()],
      allow_loopback_http: true,
      retries: 0,
      ..Default::default()
    }
  }

  #[tokio::test]
  async fn download_checks_digest_before_replacing_existing_file() {
    let origin = server("HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnew");
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("download");
    std::fs::write(&target, "old").unwrap();
    let client = NetworkClient::new(options(&origin)).unwrap();
    assert!(
      client
        .download(&origin, &target, Some(&"0".repeat(64)), |_| {})
        .await
        .is_err()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    let origin = server("HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnew");
    let client = NetworkClient::new(options(&origin)).unwrap();
    let hash = format!("{:x}", Sha256::digest(b"new"));
    client.download(&origin, &target, Some(&hash), |_| {}).await.unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"new");
  }

  #[tokio::test]
  async fn foreign_redirects_and_oversized_content_are_rejected() {
    let origin = server(
      "HTTP/1.1 302 Found\r\nLocation: https://example.org/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert!(
      NetworkClient::new(options(&origin))
        .unwrap()
        .get_bytes(&origin)
        .await
        .is_err()
    );
    let origin = server("HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnew");
    let mut config = options(&origin);
    config.max_bytes = 2;
    assert!(NetworkClient::new(config).unwrap().get_bytes(&origin).await.is_err());
    assert!(
      NetworkClient::new(NetworkOptions::default())
        .unwrap()
        .get_bytes("https://example.org")
        .await
        .is_err()
    );
  }

  #[tokio::test]
  async fn cancelling_a_stream_cleans_the_partial_file_and_preserves_destination() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept().unwrap();
      stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
      let mut request = [0; 1024];
      stream.read(&mut request).unwrap();
      stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx")
        .unwrap();
      std::thread::sleep(Duration::from_millis(300));
      let _ = stream.write_all(&[b'x'; 99]);
    });
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("data");
    std::fs::write(&target, b"old").unwrap();
    let client = NetworkClient::new(options(&origin)).unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let notice = Arc::clone(&started);
    let output = target.clone();
    let task = tokio::spawn(async move { client.download(&origin, output, None, |_| notice.notify_one()).await });
    tokio::time::timeout(Duration::from_secs(5), started.notified())
      .await
      .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(std::fs::read(&target).unwrap(), b"old");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    server.join().unwrap();
  }

  #[test]
  fn numeric_ipv6_loopback_is_supported_only_when_explicitly_allowed() {
    let mut config = options("http://[::1]:8080");
    let url = Url::parse("http://[::1]:8080/page").unwrap();
    assert!(validate_url(&url, &config).is_ok());
    config.allow_loopback_http = false;
    assert!(validate_url(&url, &config).is_err());
  }
}
