use reqwest::Client;
use rodio::{Decoder, OutputStreamBuilder};
use std::error::Error;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
// use tokio;

// 音频播放器结构体：包含状态标记和同步锁
#[derive(Clone)]
pub struct AudioPlayer {
  is_playing: Arc<Mutex<bool>>, // 原子状态：是否正在播放
  client: Client,               // 复用 HTTP 客户端
}

impl AudioPlayer {
  pub fn new() -> Self {
    AudioPlayer {
      is_playing: Arc::new(Mutex::new(false)),
      client: Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("创建 HTTP 客户端失败"),
    }
  }

  /// 内部实现：下载并播放音频
  pub async fn download_and_play(&self, url: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
    // 1. 下载音频
    let response = self.client.get(url).send().await?;
    if !response.status().is_success() {
      return Err(format!("下载失败，状态码：{}", response.status()).into());
    }
    let audio_bytes = response.bytes().await?;

    tokio::task::spawn_blocking(move || {
      let cursor = Cursor::new(audio_bytes);
      let stream_handle = OutputStreamBuilder::open_default_stream()?;
      let sink = rodio::play(&stream_handle.mixer(), cursor)?;
      sink.sleep_until_end();
      Ok::<(), Box<dyn Error + Send + Sync>>(())
    })
    .await??;
    Ok(())
  }
}
