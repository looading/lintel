use anyhow::Ok;
use lintel_lib_utils::{
  prost::{from_bytes, to_bytes},
  serde::{cbor4ii_from_slice, cbor4ii_to_vec},
};
use serde::{Deserialize, Serialize};

// js 与 Rust 之间传输的数据类型
// jsonstring & arraybuffer
#[derive(Deserialize, Serialize, Debug)]
pub enum TransferData {
  String(String),
  Prost(Vec<u8>),
  Cbor(Vec<u8>),
}

impl Default for TransferData {
  fn default() -> Self {
    TransferData::String(String::default())
  }
}

impl TransferData {
  pub fn is_json(&self) -> bool {
    match self {
      TransferData::String(_) => true,
      TransferData::Prost(_) => false,
      TransferData::Cbor(_) => false,
    }
  }

  pub fn is_prost(&self) -> bool {
    match self {
      TransferData::String(_) => false,
      TransferData::Prost(_) => true,
      TransferData::Cbor(_) => false,
    }
  }

  pub fn is_cbor(&self) -> bool {
    match self {
      TransferData::String(_) => false,
      TransferData::Prost(_) => false,
      TransferData::Cbor(_) => true,
    }
  }

  // 将内部的数据 反序列化成 json 格式
  pub fn json<T>(&self) -> anyhow::Result<T>
  where
    T: for<'a> serde::Deserialize<'a>,
  {
    match self {
      TransferData::String(s) => {
        serde_json::from_str(s).map_err(|err| anyhow::anyhow!("deserialize faild: {}", err.to_string()))
      }
      TransferData::Prost(_) => anyhow::bail!("prost data not support json"),
      TransferData::Cbor(_) => anyhow::bail!("binary data not support json"),
    }
  }

  // 获取 json 格式的字符串
  pub fn json_value(self) -> Option<String> {
    match self {
      TransferData::String(s) => Some(s),
      TransferData::Prost(_) => None,
      TransferData::Cbor(_) => None,
    }
  }

  // 将内部的数据 反序列化成 prost 格式
  pub fn prost<T>(&self) -> anyhow::Result<T>
  where
    T: prost::Message + Default,
  {
    match self {
      TransferData::String(_) => anyhow::bail!("string data not support prost"),
      TransferData::Prost(d) => from_bytes(d).map_err(|err| anyhow::anyhow!("deserialize faild: {}", err.to_string())),
      TransferData::Cbor(_) => anyhow::bail!("binary data not support prost"),
    }
  }

  // 获取 prost 格式的的原始二进制数据
  pub fn prost_value(self) -> Option<Vec<u8>> {
    match self {
      TransferData::String(_) => None,
      TransferData::Prost(d) => Some(d),
      TransferData::Cbor(_) => None,
    }
  }

  // 将内部的数据 反序列化成 cbor 格式
  pub fn cbor<T>(&self) -> anyhow::Result<T>
  where
    T: for<'a> serde::Deserialize<'a>,
  {
    match self {
      TransferData::String(_) => anyhow::bail!("string data not support binary"),
      TransferData::Prost(_) => anyhow::bail!("prost data not support binary"),
      TransferData::Cbor(d) => cbor4ii_from_slice(d.as_slice()),
    }
  }

  // 获取 cbor 格式的的原始二进制数据
  pub fn cbor_value(self) -> Option<Vec<u8>> {
    match self {
      TransferData::String(_) => None,
      TransferData::Prost(_) => None,
      TransferData::Cbor(d) => Some(d),
    }
  }
}

impl TransferData {
  pub fn from_json<T>(json: T) -> anyhow::Result<Self>
  where
    T: Serialize,
  {
    Ok(TransferData::String(serde_json::to_string(&json).map_err(|err| {
      anyhow::anyhow!("json serialize faild: {}", err.to_string())
    })?))
  }

  pub fn from_prost<T>(data: T) -> anyhow::Result<Self>
  where
    T: prost::Message + Default,
  {
    Ok(TransferData::Prost(to_bytes(&data).map_err(|err| {
      anyhow::anyhow!("prost serialize faild: {}", err.to_string())
    })?))
  }

  pub fn from_bytes(data: bytes::Bytes) -> anyhow::Result<Self> {
    cbor4ii_from_slice(&data)
  }

  pub fn to_bytes(self) -> anyhow::Result<bytes::Bytes> {
    Ok(bytes::Bytes::from(cbor4ii_to_vec(self)?))
  }

  pub fn from_cbor<T>(data: T) -> anyhow::Result<Self>
  where
    T: Serialize,
  {
    Ok(Self::Cbor(cbor4ii_to_vec(data)?))
  }
}

impl TransferData {
  pub fn cbor_encode<T>(data: T) -> anyhow::Result<Vec<u8>>
  where
    T: Serialize,
  {
    Ok(cbor4ii_to_vec(data)?)
  }

  pub fn cbor_decode<T>(data: &[u8]) -> anyhow::Result<T>
  where
    T: for<'a> serde::Deserialize<'a>,
  {
    cbor4ii_from_slice(data)
  }

  pub fn json_encode<T>(data: T) -> anyhow::Result<Vec<u8>>
  where
    T: Serialize,
  {
    Ok(serde_json::to_vec(&data).map_err(|err| anyhow::anyhow!("json serialize faild: {}", err.to_string()))?)
  }

  pub fn json_decode<T>(data: &[u8]) -> anyhow::Result<T>
  where
    T: for<'a> serde::Deserialize<'a>,
  {
    serde_json::from_slice(data).map_err(|err| anyhow::anyhow!("json deserialize faild: {}", err.to_string()))
  }

  pub fn prost_encode<T>(data: T) -> anyhow::Result<Vec<u8>>
  where
    T: prost::Message + Default,
  {
    Ok(to_bytes(&data).map_err(|err| anyhow::anyhow!("prost serialize faild: {}", err.to_string()))?)
  }

  pub fn prost_decode<T>(data: &[u8]) -> anyhow::Result<T>
  where
    T: prost::Message + Default,
  {
    from_bytes(data).map_err(|err| anyhow::anyhow!("prost deserialize faild: {}", err.to_string()))
  }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LintelInvokeAsyncParams {
  pub client_id: String,
  pub command_id: String,
  pub payload: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LintelInvokeAsyncResponse {
  pub has_error: bool,
  pub payload: Vec<u8>,
}

impl LintelInvokeAsyncResponse {
  pub fn new(has_error: bool, payload: Vec<u8>) -> Self {
    Self { has_error, payload }
  }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LintelPushPayload {
  pub client_id: String,
  pub payload: Vec<u8>,
}

impl LintelPushPayload {
  pub fn new(client_id: String, payload: Vec<u8>) -> Self {
    Self { client_id, payload }
  }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CryptoClientId(String, String);

impl CryptoClientId {
  pub fn new(encrypted: String, decrypted: String) -> Self {
    Self(encrypted, decrypted)
  }

  pub fn encrypted(&self) -> String {
    self.1.clone()
  }

  pub fn decrypted(&self) -> String {
    self.0.clone()
  }
}
