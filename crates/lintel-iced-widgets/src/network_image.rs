use std::{sync::LazyLock, time::Duration};

use futures::FutureExt;
use lintel_lib_iced::{
  self as iced,
  widget::image::{Allocation, Error, Handle, allocate},
};
use tokio_retry::Retry;
use tracing::{Instrument, error, info, span};
use url::Url;

use crate::apm::ImageDownloadCostEvent;

static IMAGE_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);

fn allocate_image(_window: iced::window::Id, handle: Handle) -> iced::Task<Result<Allocation, Error>> {
  allocate(handle)
}

pub async fn download_image(url: String) -> anyhow::Result<(iced::widget::image::Handle, iced::Size<u32>)> {
  #[derive(serde::Deserialize)]
  struct ImageExtra {
    enc: Enc,
  }
  #[derive(serde::Deserialize)]
  struct Enc {
    #[serde(alias = "w")]
    width: u32,
    #[serde(alias = "h")]
    height: u32,
  }

  let response = IMAGE_CLIENT.get(Url::parse(&url)?).send().await?.error_for_status()?;

  let size = if let Some(ImageExtra {
    enc: Enc { width, height },
  }) = response
    .headers()
    .get("x-imagex-extra")
    .and_then(|v| serde_json::from_slice::<ImageExtra>(v.as_bytes()).ok())
  {
    // the avatar should be square
    let width = width.min(height);
    (width, width)
  } else {
    (u32::MAX, u32::MAX)
  }
  .into();
  let image_bytes = response.bytes().await?;

  Ok((iced::widget::image::Handle::from_bytes(image_bytes), size))
}

#[derive(Clone, Debug)]
pub struct Avatar {
  pub allocation: anyhow::Result<iced::widget::image::Allocation, ()>,
  pub crop_region: iced::Rectangle<u32>,
}

pub fn fetch_avatar(window: iced::window::Id, url: String) -> iced::Task<anyhow::Result<Avatar, ()>> {
  let mut cost = ImageDownloadCostEvent::new(url.clone());

  let retry_config = tokio_retry::strategy::FibonacciBackoff::from_millis(300).max_delay(Duration::from_secs(3));

  let download_image = Retry::spawn(retry_config, move || {
    let url = url.clone();
    download_image(url)
  })
  .inspect(move |ret| {
    if let Err(err) = ret {
      cost.set_error(err.to_string());
    };
    //cost.scoped();
  })
  .instrument(span!(tracing::Level::INFO, "download_image"));

  iced::Task::future(download_image)
    .and_then(move |(handle, size)| {
      allocate_image(window, handle)
        .map_err(|err| {
          error!("Failed to allocate avatar image: {err:?}");
        })
        .map(move |allocation| {
          info!(
            "Successfully fetched avatar image, size: {:?}, allocation: {:?}",
            size, allocation
          );
          Ok(Avatar {
            allocation,
            crop_region: iced::Rectangle::with_size(size),
          })
        })
    })
    .map_err(|err| {
      error!("Failed to fetch avatar image: {err:?}");
    })
}

#[derive(Clone, Debug)]
pub struct NetworkImage {
  pub allocation: anyhow::Result<iced::widget::image::Allocation, ()>,
}

pub fn fetch_img(window: iced::window::Id, url: String) -> iced::Task<anyhow::Result<NetworkImage, ()>> {
  let mut cost = ImageDownloadCostEvent::new(url.clone());

  let retry_config = tokio_retry::strategy::FibonacciBackoff::from_millis(300).max_delay(Duration::from_secs(3));

  let download_image = Retry::spawn(retry_config, move || {
    let url = url.clone();
    download_image(url)
  })
  .inspect(move |ret| {
    if let Err(err) = ret {
      cost.set_error(err.to_string());
    };
    //cost.scoped();
  })
  .instrument(span!(tracing::Level::INFO, "download_image"));

  iced::Task::future(download_image)
    .and_then(move |(handle, size)| {
      allocate_image(window, handle)
        .map_err(|err| {
          error!("Failed to allocate network image: {err:?}");
        })
        .map(move |allocation| {
          info!(
            "Successfully fetched network image, size: {:?}, allocation: {:?}",
            size, allocation
          );
          Ok(NetworkImage { allocation })
        })
    })
    .map_err(|err| {
      error!("Failed to fetch avatar image: {err:?}");
    })
}

// fn cache_dir() -> PathBuf {
//   dirs::cache_dir()
//     .unwrap_or_default()
//     .join(env!("CARGO_PKG_NAME"))
//     .join("cards")
// }

/*
fn has_rounded_corners(rgba: &image::RgbaImage) -> bool {
  rgba.get_pixel(0, 0).0[3] == 0
}

// Courtesy of ChatGPT
fn round_corners(rgba: &mut image::RgbaImage) {
  let (width, height) = rgba.dimensions();

  let radius = (width as f32 * 0.05) as u32;
  let radius_sq = radius * radius;
  let aa_span = radius / 4;

  for y in 0..height {
    for x in 0..width {
      let dist_x = if x < radius {
        radius - x
      } else if x >= width - radius {
        x - (width - radius - 1)
      } else {
        0
      };

      let dist_y = if y < radius {
        radius - y
      } else if y >= height - radius {
        y - (height - radius - 1)
      } else {
        0
      };

      let dist_sq = dist_x * dist_x + dist_y * dist_y;

      if dist_sq > radius_sq {
        let dist = (dist_sq as f32).sqrt();

        if dist <= (radius + aa_span) as f32 {
          let alpha_scale = 1.0 - (dist_sq - radius_sq) as f32 / (aa_span * aa_span) as f32;

          let pixel = rgba.get_pixel_mut(x, y);
          pixel.0[3] = (pixel.0[3] as f32 * alpha_scale) as u8;
        } else {
          let pixel = rgba.get_pixel_mut(x, y);
          pixel.0 = [0; 4];
        }
      }
    }
  }
}

*/
