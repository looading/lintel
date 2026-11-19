use semver::Version;

/// 获取版本号的number
/// 0.10.1 -> 10001
/// 0.9.18 -> 9018
/// 0.11.6 -> 11006
pub fn get_desk_version_number(version: &str) -> u64 {
  if version.is_empty() {
    return 0;
  }

  match Version::parse(version) {
    Ok(v) => v.major * 1_000_000 + v.minor * 1_000 + v.patch,
    Err(_) => 0,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_get_desk_version_number() {
    assert_eq!(get_desk_version_number("0.10.1"), 10001);
    assert_eq!(get_desk_version_number("0.9.18"), 9018);
    assert_eq!(get_desk_version_number("0.11.6"), 11006);
    assert_eq!(get_desk_version_number(""), 0);
    assert_eq!(get_desk_version_number("invalid"), 0);
  }
}
