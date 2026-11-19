use iced::{Color, color};

use lintel_lib_iced::{self as iced};

pub fn convert_color(input: &str) -> Color {
  let input = input.trim().to_lowercase();
  let input = input.as_str();

  match input {
    "black" => Color::BLACK,
    "white" => Color::WHITE,
    "red" => color!(255, 0, 0),
    "green" => color!(0, 255, 0),
    "blue" => color!(0, 0, 255),
    "yellow" => color!(255, 255, 0),
    "gray" | "grey" => color!(0x666666),
    "transparent" => Color::TRANSPARENT,
    rgba if rgba.starts_with("rgba(") => {
      // 处理 rgba 格式
      let rgba = &input[5..input.len() - 1]; // 去掉 "rgba(" 和 ")"
      let parts: Vec<&str> = rgba.split(',').map(|s| s.trim()).collect();

      if parts.len() == 4 {
        // 提取 rgba 各部分
        let r = parts[0].parse::<u8>().unwrap_or(0);
        let g = parts[1].parse::<u8>().unwrap_or(0);
        let b = parts[2].parse::<u8>().unwrap_or(0);
        let a = parts[3].parse::<f32>().unwrap_or(1.0);
        color!(r, g, b, a)
      } else {
        // 格式错误，返回默认值
        color!(0, 0, 0, 1.0)
      }
    }
    rgb if rgb.starts_with("rgb(") => {
      let rgb = &input[4..input.len() - 1];
      let parts: Vec<&str> = rgb.split(',').map(|s| s.trim()).collect();

      if parts.len() == 3 {
        let r = parts[0].parse::<u8>().unwrap_or(0);
        let g = parts[1].parse::<u8>().unwrap_or(0);
        let b = parts[2].parse::<u8>().unwrap_or(0);
        color!(r, g, b, 1.0)
      } else {
        color!(0, 0, 0, 1.0)
      }
    }
    hex if hex.starts_with('#') => {
      let hex = hex.trim_start_matches('#');
      match hex.len() {
        // 简写形式：#f00 → #ff0000
        3 => {
          let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).unwrap_or(0);
          let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).unwrap_or(0);
          let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).unwrap_or(0);
          Color::from_rgb8(r, g, b)
        }
        // 标准形式：#ff0000
        6 => {
          let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
          let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
          let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
          Color::from_rgb8(r, g, b)
        }
        // RGBA 形式：#ff0000ff（忽略 alpha，或可扩展支持）
        8 => {
          let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
          let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
          let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
          Color::from_rgb8(r, g, b)
        }
        // 无效十六进制，默认黑色
        _ => Color::BLACK,
      }
    }
    _ => color!(0, 0, 0, 1.0),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use iced::Color;

  #[test]
  fn test_convert_color() {
    // 颜色关键字
    assert_eq!(convert_color("black"), Color::BLACK);
    assert_eq!(convert_color("white"), Color::WHITE);
    assert_eq!(convert_color("red"), color!(255, 0, 0));
    assert_eq!(convert_color("green"), color!(0, 255, 0));
    assert_eq!(convert_color("blue"), color!(0, 0, 255));
    assert_eq!(convert_color("yellow"), color!(255, 255, 0));
    assert_eq!(convert_color("gray"), color!(0x66, 0x66, 0x66));
    assert_eq!(convert_color("grey"), color!(0x66, 0x66, 0x66));
    assert_eq!(convert_color("transparent"), Color::TRANSPARENT);

    // 带空格和大小写
    assert_eq!(convert_color("  Black  "), Color::BLACK);
    assert_eq!(convert_color("RED"), color!(255, 0, 0));

    // RGB 格式
    assert_eq!(convert_color("rgb(255, 0, 0)"), color!(255, 0, 0, 1.0));
    assert_eq!(convert_color("rgb(0, 255, 0)"), color!(0, 255, 0, 1.0));
    assert_eq!(convert_color("rgb(0, 0, 255)"), color!(0, 0, 255, 1.0));

    // RGBA 格式
    assert_eq!(convert_color("rgba(255, 0, 0, 0.5)"), color!(255, 0, 0, 0.5));
    assert_eq!(convert_color("rgba(0, 255, 0, 1.0)"), color!(0, 255, 0, 1.0));
    assert_eq!(convert_color("rgba(0, 0, 255, 0.0)"), color!(0, 0, 255, 0.0));

    // 十六进制 3 位
    assert_eq!(convert_color("#f00"), Color::from_rgb8(255, 0, 0));
    assert_eq!(convert_color("#0f0"), Color::from_rgb8(0, 255, 0));
    assert_eq!(convert_color("#00f"), Color::from_rgb8(0, 0, 255));
    assert_eq!(convert_color("#fff"), Color::from_rgb8(255, 255, 255));

    // 十六进制 6 位
    assert_eq!(convert_color("#ff0000"), Color::from_rgb8(255, 0, 0));
    assert_eq!(convert_color("#00ff00"), Color::from_rgb8(0, 255, 0));
    assert_eq!(convert_color("#0000ff"), Color::from_rgb8(0, 0, 255));
    assert_eq!(convert_color("#ffffff"), Color::from_rgb8(255, 255, 255));

    // 十六进制 8 位（忽略 alpha）
    assert_eq!(convert_color("#ff0000ff"), Color::from_rgb8(255, 0, 0));
    assert_eq!(convert_color("#00ff0080"), Color::from_rgb8(0, 255, 0));

    // 错误格式返回默认黑色
    assert_eq!(convert_color("invalid"), color!(0, 0, 0, 1.0));
    assert_eq!(convert_color("rgb(255, 0)"), color!(0, 0, 0, 1.0));
    assert_eq!(convert_color("rgba(255, 0, 0)"), color!(0, 0, 0, 1.0));
    assert_eq!(convert_color("#zzz"), Color::BLACK);
    assert_eq!(convert_color("#zzzzzz"), Color::BLACK);
  }
}
