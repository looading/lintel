use sha2::{Digest, Sha256};

use crate::aes::HexString;

pub fn sha256_encode(input: String) -> String {
  // 创建一个 Sha256 实例
  let mut hasher = Sha256::new();

  // 向 hasher 中写入数据
  hasher.update(input);

  // 完成哈希计算
  let result = hasher.finalize();

  HexString::from_vec(result.to_vec()).0
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_sha256_encode() {
    let input = "Hello, World!".to_string();
    let expected = "DFFD6021BB2BD5B0AF676290809EC3A53191DD81C7F70A4B28688A362182986F"
      .to_string()
      .to_lowercase();
    let result = sha256_encode(input);
    assert_eq!(result, expected);
  }
}
