use std::time::{SystemTime, UNIX_EPOCH};

use chrono::prelude::{DateTime, Utc};

pub fn get_current_time() -> i64 {
  let utc: DateTime<Utc> = Utc::now();
  utc.timestamp()
}

pub fn get_now_second() -> Result<i64, std::num::ParseIntError> {
  let now: DateTime<Utc> = Utc::now();
  let second = now.format("%S").to_string();
  second.parse::<i64>()
}

/// 获取当前时间戳（微秒）
#[must_use]
pub fn get_current_timestamp_microsecond() -> i64 {
  let utc: DateTime<Utc> = Utc::now();
  utc.timestamp_nanos_opt().unwrap() / 1000
}

pub fn get_current_time_millis() -> i64 {
  let utc: DateTime<Utc> = Utc::now();
  utc.timestamp() * 1000 + i64::from(utc.timestamp_subsec_millis())
}

/// 将毫秒时长格式化为可读字符串，负值表示超时。
pub fn get_timer_str(diff: i64) -> String {
  const ONE_MINUTE: i64 = 60_000;
  const THREE_MINUTES: i64 = 180_000;
  const ONE_HOUR: i64 = 3_600_000;
  if diff < 0 {
    "已超时".to_string()
  } else if diff < ONE_MINUTE {
    format!("{}秒", diff / 1000)
  } else if diff < THREE_MINUTES {
    let counts_of_60s = diff / ONE_MINUTE;
    let residue_of_60s = (diff / 1000) % 60;
    if residue_of_60s == 0 {
      format!("{}分钟", counts_of_60s)
    } else {
      format!("{}分{}秒", counts_of_60s, residue_of_60s)
    }
  } else if diff < ONE_HOUR {
    format!("{}分钟", diff / ONE_MINUTE)
  } else {
    format!("{}小时", diff / ONE_HOUR)
  }
}

pub fn truncate_with_ellipsis(s: &str, ellipsis_num: usize) -> String {
  if s.chars().count() <= ellipsis_num {
    s.to_string()
  } else {
    let truncated: String = s.chars().take(ellipsis_num).collect();
    format!("{}...", truncated)
  }
}

pub fn timestamp_to_hm(timestamp: i64) -> String {
  // 转换为本地时间（需要时区偏移量，这里简化为 +8 小时）
  let local_seconds = timestamp + 8 * 3600; // UTC+8 时区

  // 计算小时和分钟
  let hours = (local_seconds / 3600) % 24;
  let minutes = (local_seconds / 60) % 60;

  // 格式化为字符串
  format!("{:02}:{:02}", hours, minutes)
}

pub fn current_timestamp_ms() -> i64 {
  // 获取当前系统时间
  let now = SystemTime::now();

  // 计算与 Unix 纪元的时间差
  let duration = now.duration_since(UNIX_EPOCH).expect("系统时间不能早于 Unix 纪元");

  // 转换为毫秒（i64 类型）
  duration.as_millis() as i64
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn current_time_uses_unix_seconds() {
    let before = Utc::now().timestamp();
    let actual = get_current_time();
    let after = Utc::now().timestamp();
    assert!((before..=after).contains(&actual));
  }

  #[test]
  fn current_time_millis_uses_unix_milliseconds() {
    let before = Utc::now().timestamp_millis();
    let actual = get_current_time_millis();
    let after = Utc::now().timestamp_millis();
    assert!((before..=after).contains(&actual));
  }

  #[test]
  fn current_timestamp_microsecond_uses_unix_microseconds() {
    let before = Utc::now().timestamp_micros();
    let actual = get_current_timestamp_microsecond();
    let after = Utc::now().timestamp_micros();
    assert!((before..=after).contains(&actual));
  }

  #[test]
  fn current_second_is_in_clock_range() {
    assert!((0..60).contains(&get_now_second().unwrap()));
  }
}
