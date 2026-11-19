use std::{path::PathBuf, str::FromStr};

use fs_extra::dir::create;

use crate::resource::ResourcePathImpl;

/*
use image::{open, GenericImageView, ImageBuffer, Rgba};
pub fn round(img: &mut ImageBuffer<Rgba<u8>, Vec<u8>>, radius: u32) {
  let (width, height) = img.dimensions();

  // top left
  border_radius(img, radius, |x, y| (x - 1, y - 1));
  // top right
  border_radius(img, radius, |x, y| (width - x, y - 1));
  // bottom right
  border_radius(img, radius, |x, y| (width - x, height - y));
  // bottom left
  border_radius(img, radius, |x, y| (x - 1, height - y));
}

fn border_radius(img: &mut ImageBuffer<Rgba<u8>, Vec<u8>>, r: u32, coordinates: impl Fn(u32, u32) -> (u32, u32)) {
  if r == 0 {
    return;
  }
  let r0 = r;

  // 16x antialiasing: 16x16 grid creates 256 possible shades, great for u8!
  let r = 16 * r;

  let mut x = 0;
  let mut y = r - 1;
  let mut p: i32 = 2 - r as i32;

  // ...

  let mut alpha: u16 = 0;
  let mut skip_draw = true;

  let draw = |img: &mut ImageBuffer<Rgba<u8>, Vec<u8>>, alpha, x, y| {
    debug_assert!((1..=256).contains(&alpha));
    let pixel_alpha = &mut img[coordinates(r0 - x, r0 - y)].0[3];
    *pixel_alpha = ((alpha * *pixel_alpha as u16 + 128) / 256) as u8;
  };

  'l: loop {
    // (comments for bottom_right case:)
    // remove contents below current position
    {
      let i = x / 16;
      for j in y / 16 + 1..r0 {
        img[coordinates(r0 - i, r0 - j)].0[3] = 0;
      }
    }
    // remove contents right of current position mirrored
    {
      let j = x / 16;
      for i in y / 16 + 1..r0 {
        img[coordinates(r0 - i, r0 - j)].0[3] = 0;
      }
    }

    // draw when moving to next pixel in x-direction
    if !skip_draw {
      draw(img, alpha, x / 16 - 1, y / 16);
      draw(img, alpha, y / 16, x / 16 - 1);
      alpha = 0;
    }

    for _ in 0..16 {
      skip_draw = false;

      if x >= y {
        break 'l;
      }

      alpha += y as u16 % 16 + 1;
      if p < 0 {
        x += 1;
        p += (2 * x + 2) as i32;
      } else {
        // draw when moving to next pixel in y-direction
        if y % 16 == 0 {
          draw(img, alpha, x / 16, y / 16);
          draw(img, alpha, y / 16, x / 16);
          skip_draw = true;
          alpha = (x + 1) as u16 % 16 * 16;
        }

        x += 1;
        p -= (2 * (y - x) + 2) as i32;
        y -= 1;
      }
    }
  }

  // one corner pixel left
  if x / 16 == y / 16 {
    // column under current position possibly not yet accounted
    if x == y {
      alpha += y as u16 % 16 + 1;
    }
    let s = y as u16 % 16 + 1;
    let alpha = 2 * alpha - s * s;
    draw(img, alpha, x / 16, y / 16);
  }

  // remove remaining square of content in the corner
  let range = y / 16 + 1..r0;
  for i in range.clone() {
    for j in range.clone() {
      img[coordinates(r0 - i, r0 - j)].0[3] = 0;
    }
  }
}

// 裁剪长方形图片为正方形
pub fn crop_to_square(img: &mut ImageBuffer<Rgba<u8>, Vec<u8>>) -> ImageBuffer<Rgba<u8>, Vec<u8>> {
  // 1. 获取图片尺寸
  let (width, height) = img.dimensions();

  // 2. 计算正方形的边长（取宽高的最小值）
  let square_size = width.min(height);

  // 3. 计算裁剪区域的起始坐标（居中裁剪）
  let x = (width - square_size) / 2; // 水平方向起点
  let y = (height - square_size) / 2; // 垂直方向起点

  // 4. 裁剪并转换为RGBA格式
  let res = img
    .view(x, y, square_size, square_size) // 获取子区域视图
    .to_image(); // 转换为ImageBuffer
  res
}

pub struct RoundImageInfo {
  pub pixels: Vec<u8>,
  pub width: u32,
  pub height: u32,
}

pub fn get_round_image(image_path: &str) -> RoundImageInfo {
  let mut avatar_image;

  if let Ok(img) = open(image_path.to_resource_path()) {
    avatar_image = img.into_rgba8();
  } else {
    avatar_image = open("assets/image/Logo.png".to_resource_path()).unwrap().into_rgba8();
  }

  let mut square_img = crop_to_square(&mut avatar_image);
  let (width, height) = square_img.dimensions();
  round(&mut square_img, width / 2);
  RoundImageInfo {
    pixels: square_img.into_raw(),
    width,
    height,
  }
}

*/

pub fn get_round_image(_image_path: &str) -> std::path::PathBuf {
  lintel_lib_env::LintelEnv::resources_path().join("assets/image/logo-rounded.png")
}
