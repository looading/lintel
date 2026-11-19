// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::{
  aessafe,
  blockmodes::{CbcDecryptor, CbcEncryptor, CtrModeX8, EcbDecryptor, EcbEncryptor, PaddingProcessor},
  symmetriccipher::{Decryptor, Encryptor, SynchronousStreamCipher},
};

/// AES key size
#[derive(Clone, Copy)]
pub enum KeySize {
  KeySize128,
  KeySize192,
  KeySize256,
}

pub fn ecb_encryptor<X: PaddingProcessor + Send + 'static>(
  key_size: KeySize,
  key: &[u8],
  padding: X,
) -> Box<dyn Encryptor> {
  match key_size {
    KeySize::KeySize128 => {
      let aes_enc = aessafe::AesSafe128Encryptor::new(key);
      let enc = Box::new(EcbEncryptor::new(aes_enc, padding));
      enc
    }
    KeySize::KeySize192 => {
      let aes_enc = aessafe::AesSafe192Encryptor::new(key);
      let enc = Box::new(EcbEncryptor::new(aes_enc, padding));
      enc
    }
    KeySize::KeySize256 => {
      let aes_enc = aessafe::AesSafe256Encryptor::new(key);
      let enc = Box::new(EcbEncryptor::new(aes_enc, padding));
      enc
    }
  }
}

pub fn ecb_decryptor<X: PaddingProcessor + Send + 'static>(
  key_size: KeySize,
  key: &[u8],
  padding: X,
) -> Box<dyn Decryptor> {
  match key_size {
    KeySize::KeySize128 => {
      let aes_dec = aessafe::AesSafe128Decryptor::new(key);
      let dec = Box::new(EcbDecryptor::new(aes_dec, padding));
      dec
    }
    KeySize::KeySize192 => {
      let aes_dec = aessafe::AesSafe192Decryptor::new(key);
      let dec = Box::new(EcbDecryptor::new(aes_dec, padding));
      dec
    }
    KeySize::KeySize256 => {
      let aes_dec = aessafe::AesSafe256Decryptor::new(key);
      let dec = Box::new(EcbDecryptor::new(aes_dec, padding));
      dec
    }
  }
}

pub fn cbc_encryptor<X: PaddingProcessor + Send + 'static>(
  key_size: KeySize,
  key: &[u8],
  iv: &[u8],
  padding: X,
) -> Box<dyn Encryptor + 'static> {
  match key_size {
    KeySize::KeySize128 => {
      let aes_enc = aessafe::AesSafe128Encryptor::new(key);
      let enc = Box::new(CbcEncryptor::new(aes_enc, padding, iv.to_vec()));
      enc
    }
    KeySize::KeySize192 => {
      let aes_enc = aessafe::AesSafe192Encryptor::new(key);
      let enc = Box::new(CbcEncryptor::new(aes_enc, padding, iv.to_vec()));
      enc
    }
    KeySize::KeySize256 => {
      let aes_enc = aessafe::AesSafe256Encryptor::new(key);
      let enc = Box::new(CbcEncryptor::new(aes_enc, padding, iv.to_vec()));
      enc
    }
  }
}

pub fn cbc_decryptor<X: PaddingProcessor + Send + 'static>(
  key_size: KeySize,
  key: &[u8],
  iv: &[u8],
  padding: X,
) -> Box<dyn Decryptor + 'static> {
  match key_size {
    KeySize::KeySize128 => {
      let aes_dec = aessafe::AesSafe128Decryptor::new(key);
      let dec = Box::new(CbcDecryptor::new(aes_dec, padding, iv.to_vec()));
      dec
    }
    KeySize::KeySize192 => {
      let aes_dec = aessafe::AesSafe192Decryptor::new(key);
      let dec = Box::new(CbcDecryptor::new(aes_dec, padding, iv.to_vec()));
      dec
    }
    KeySize::KeySize256 => {
      let aes_dec = aessafe::AesSafe256Decryptor::new(key);
      let dec = Box::new(CbcDecryptor::new(aes_dec, padding, iv.to_vec()));
      dec
    }
  }
}

pub fn ctr(key_size: KeySize, key: &[u8], iv: &[u8]) -> Box<dyn SynchronousStreamCipher + 'static> {
  match key_size {
    KeySize::KeySize128 => {
      let aes_dec = aessafe::AesSafe128EncryptorX8::new(key);
      let dec = Box::new(CtrModeX8::new(aes_dec, iv));
      dec
    }
    KeySize::KeySize192 => {
      let aes_dec = aessafe::AesSafe192EncryptorX8::new(key);
      let dec = Box::new(CtrModeX8::new(aes_dec, iv));
      dec
    }
    KeySize::KeySize256 => {
      let aes_dec = aessafe::AesSafe256EncryptorX8::new(key);
      let dec = Box::new(CtrModeX8::new(aes_dec, iv));
      dec
    }
  }
}
