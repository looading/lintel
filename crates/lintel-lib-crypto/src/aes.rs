use aes::{
  Aes128,
  cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray},
};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

pub use crate::aes_raw::*;

pub fn encrypt_aes_128_ecb(data: &str, key: &[u8; 16]) -> Result<HexString> {
  let key = GenericArray::from_slice(key);
  let data = pkcs7_padding(data.as_bytes(), 16);

  let mut blocks = Vec::new();

  let cipher = Aes128::new(key);

  for chunk in data.chunks(16) {
    let mut block = [0u8; 16];

    for (i, &v) in chunk.iter().enumerate() {
      block[i] = v;
    }

    let mut ret = GenericArray::from(block);
    cipher.encrypt_block(&mut ret);
    blocks.push(ret.to_vec());
  }

  let blocks = blocks.iter().flatten().copied().collect::<Vec<u8>>();

  Ok(HexString::from_vec(blocks))
}

pub fn decryp_aes_128_ecb(data: HexString, key: &[u8; 16]) -> Result<String> {
  let key = GenericArray::from_slice(key);
  let data = data.to_vec()?;

  let cipher = Aes128::new(key);
  let mut blocks = Vec::new();
  for chunk in data.chunks(16) {
    let mut block = [0u8; 16];

    for (i, &v) in chunk.iter().enumerate() {
      block[i] = v;
    }

    let mut ret = GenericArray::from(block);
    cipher.decrypt_block(&mut ret);
    blocks.push(ret.to_vec());
  }

  let blocks = blocks.iter().flatten().copied().collect::<Vec<u8>>();

  let blocks = pkcs7_unpadding(&blocks);

  Ok(String::from_utf8_lossy(&blocks).to_string())
}

fn pkcs7_padding(input: &[u8], block_size: usize) -> Vec<u8> {
  let padding_length = block_size - (input.len() % block_size);
  let mut padded = input.to_vec();
  padded.extend(std::iter::repeat_n(padding_length as u8, padding_length));
  padded
}

fn pkcs7_unpadding(padded: &[u8]) -> Vec<u8> {
  let padding_length = padded[padded.len() - 1] as usize;
  padded[..padded.len() - padding_length].to_vec()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HexString(pub String);

impl HexString {
  pub fn from_vec(data: Vec<u8>) -> Self {
    Self(
      data
        .iter()
        .map(|c| format!("{c:02x}"))
        .collect::<Vec<String>>()
        .join(""),
    )
  }

  pub fn to_vec(&self) -> Result<Vec<u8>> {
    use anyhow::Context;
    let mut out = vec![];
    for i in 0..self.0.len() {
      if i.is_multiple_of(2) {
        let a = self
          .0
          .get(i..i + 2)
          .ok_or(anyhow!("HexString to string failed"))?
          .to_string();
        let b = u8::from_str_radix(a.as_str(), 16).with_context(|| "HexString to string failed")?;
        out.push(b);
      }
    }
    Ok(out)
  }
}

#[cfg(test)]
mod tests {
  use aes_gcm::{
    AeadCore, Aes128Gcm, Aes256Gcm, Key, KeyInit,
    aead::{Aead, OsRng},
    aes::Aes128,
  };
  use hex::ToHex;
  use rand::RngCore;

  use crate::aes::{decryp_aes_128_ecb, encrypt_aes_128_ecb, pkcs7_padding};

  use super::HexString;

  fn test_aes_128(text: &str, key: &[u8; 16]) {
    assert_eq!(
      decryp_aes_128_ecb(encrypt_aes_128_ecb(text, key).unwrap(), key).unwrap(),
      text
    );
  }

  fn gen_key() -> [u8; 16] {
    let mut key = [0u8; 16];
    let mut rng = rand::thread_rng();

    rng.fill_bytes(&mut key);
    key
  }

  #[test]
  fn test_gen_key() {
    gen_key();
  }
  #[test]
  fn test_aes128() {
    // let key = b"s5v8y/B?E(G+KbPe";

    let key = &gen_key();
    test_aes_128("hello world", key);
    test_aes_128("shdiad\\12313", key);
    test_aes_128("sdajkldjlak", key);
    test_aes_128("====", key);
    test_aes_128("  ", key);
    test_aes_128("撒大蘇打", key);
  }

  #[test]
  fn test_hex_string() {
    let data = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

    let hex_s = HexString::from_vec(data.to_vec());

    let d = hex_s.to_vec();
  }

  #[test]
  fn test_1() {
    let key = Aes256Gcm::generate_key(OsRng);

    // Transformed from a byte array:
    let key: &[u8; 19] = &[42; 19];

    let key = pkcs7_padding(key, 16);
    let key: &Key<Aes256Gcm> = key.as_slice().into();

    // // Note that you can get byte array from slice using the `TryInto` trait:
    // let key: &[u8] = &[42; 32];
    // let key: [u8; 32] = key.try_into().unwrap();

    // // Alternatively, the key can be transformed directly from a byte slice
    // // (panicks on length mismatch):
    // let key = Key::<Aes256Gcm>::from_slice(key);

    let cipher = Aes256Gcm::new(&key);
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng); // 96-bits; unique per message
    let ciphertext = cipher.encrypt(&nonce, b"plaintext message".as_ref()).unwrap();
    let plaintext = cipher.decrypt(&nonce, ciphertext.as_ref()).unwrap();
    assert_eq!(&plaintext, b"plaintext message");
  }
}
