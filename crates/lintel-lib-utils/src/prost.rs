use prost::{DecodeError, EncodeError, Message};

pub fn to_bytes<M>(m: &M) -> Result<Vec<u8>, EncodeError>
where
  M: Message + Default,
{
  Ok(m.encode_to_vec())
}

pub fn from_bytes<M>(buf: &[u8]) -> Result<M, DecodeError>
where
  M: Default + Message,
{
  let ret = M::decode(buf);
  ret
}
