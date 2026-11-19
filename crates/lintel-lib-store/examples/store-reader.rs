use std::{
  io::{Error, ErrorKind},
  path::PathBuf,
};

use lintel_lib_crypto::aes::{HexString, decryp_aes_128_ecb};
use rkv::{
  Manager, Rkv, SingleStore, StoreOptions, Value as RkvValue,
  backend::{SafeMode, SafeModeEnvironment},
};

const STORE_META_KEY: &str = "__x_lib_store_meta__";

#[derive(Debug, Default)]
struct Args {
  db_path: PathBuf,
  key: Option<String>,
  secret_key: Option<[u8; 16]>,
  pretty: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let args = parse_args(std::env::args().skip(1))?;
  let output = if let Some(key) = args.key.as_deref() {
    read_one(&args.db_path, key, &args.secret_key)?
  } else {
    dump_all(&args.db_path, &args.secret_key)?
  };

  if args.pretty {
    println!("{}", serde_json::to_string_pretty(&output)?);
  } else {
    println!("{}", serde_json::to_string(&output)?);
  }

  Ok(())
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, Box<dyn std::error::Error>> {
  let mut parsed = Args::default();

  while let Some(arg) = args.next() {
    match arg.as_str() {
      "--db-path" => {
        let value = args
          .next()
          .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "missing value for --db-path"))?;
        parsed.db_path = PathBuf::from(value);
      }
      "--key" => {
        let value = args
          .next()
          .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "missing value for --key"))?;
        parsed.key = Some(value);
      }
      "--secret-key-hex" => {
        let value = args
          .next()
          .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "missing value for --secret-key-hex"))?;
        parsed.secret_key = Some(parse_secret_key_hex(&value)?);
      }
      "--pretty" => {
        parsed.pretty = true;
      }
      other => {
        return Err(Error::new(ErrorKind::InvalidInput, format!("unknown arg: {other}")).into());
      }
    }
  }

  if parsed.db_path.as_os_str().is_empty() {
    return Err(Error::new(ErrorKind::InvalidInput, "missing required arg --db-path").into());
  }

  Ok(parsed)
}

fn parse_secret_key_hex(raw: &str) -> Result<[u8; 16], Box<dyn std::error::Error>> {
  if raw.len() != 32 {
    return Err(
      Error::new(
        ErrorKind::InvalidInput,
        "secret key hex length must be exactly 32 chars (16 bytes)",
      )
      .into(),
    );
  }

  let decoded = decode_hex(raw)?;
  let key: [u8; 16] = decoded
    .try_into()
    .map_err(|_| Error::new(ErrorKind::InvalidInput, "secret key must decode to 16 bytes"))?;
  Ok(key)
}

fn decode_hex(raw: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
  let mut bytes = Vec::with_capacity(raw.len() / 2);
  let raw = raw.as_bytes();

  let mut i = 0;
  while i < raw.len() {
    let hi = decode_hex_char(raw[i])?;
    let lo = decode_hex_char(raw[i + 1])?;
    bytes.push((hi << 4) | lo);
    i += 2;
  }

  Ok(bytes)
}

fn decode_hex_char(byte: u8) -> Result<u8, Box<dyn std::error::Error>> {
  match byte {
    b'0'..=b'9' => Ok(byte - b'0'),
    b'a'..=b'f' => Ok(byte - b'a' + 10),
    b'A'..=b'F' => Ok(byte - b'A' + 10),
    _ => Err(Error::new(ErrorKind::InvalidInput, format!("invalid hex char: {}", byte as char)).into()),
  }
}

fn open_store(
  db_path: &PathBuf,
) -> Result<
  (
    std::sync::Arc<std::sync::RwLock<Rkv<SafeModeEnvironment>>>,
    SingleStore<rkv::backend::SafeModeDatabase>,
  ),
  Box<dyn std::error::Error>,
> {
  let shared_env = Manager::<SafeModeEnvironment>::singleton()
    .write()
    .unwrap_or_else(|e| e.into_inner())
    .get_or_create(db_path.as_path(), Rkv::new::<SafeMode>)?;

  let single_store = {
    let env = shared_env.read().unwrap_or_else(|e| e.into_inner());
    env.open_single("data", StoreOptions::create())?
  };

  Ok((shared_env, single_store))
}

fn read_one(
  db_path: &PathBuf,
  key: &str,
  secret_key: &Option<[u8; 16]>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
  let (shared_env, single_store) = open_store(db_path)?;
  let env = shared_env.read().unwrap_or_else(|e| e.into_inner());
  let reader = env.read()?;

  let value = match single_store.get(&reader, key)? {
    Some(RkvValue::Blob(bytes)) => Some(decode_value(bytes, secret_key)?),
    Some(other) => {
      return Err(
        Error::new(
          ErrorKind::InvalidData,
          format!("unsupported value type for key {key}: {other:?}"),
        )
        .into(),
      );
    }
    None => None,
  };

  Ok(serde_json::json!({
    "success": true,
    "dbPath": db_path,
    "key": key,
    "value": value
  }))
}

fn dump_all(db_path: &PathBuf, secret_key: &Option<[u8; 16]>) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
  let (shared_env, single_store) = open_store(db_path)?;
  let env = shared_env.read().unwrap_or_else(|e| e.into_inner());
  let reader = env.read()?;
  let mut values = serde_json::Map::new();

  let iter = single_store.iter_start(&reader)?;
  for item in iter {
    let (key_bytes, value) = item?;
    let key = std::str::from_utf8(key_bytes)
      .map_err(|e| Error::new(ErrorKind::InvalidData, format!("invalid utf8 key: {e}")))?;
    if key == STORE_META_KEY {
      continue;
    }

    match value {
      RkvValue::Blob(bytes) => {
        values.insert(key.to_string(), decode_value(bytes, secret_key)?);
      }
      other => {
        return Err(
          Error::new(
            ErrorKind::InvalidData,
            format!("unsupported value type for key {key}: {other:?}"),
          )
          .into(),
        );
      }
    }
  }

  Ok(serde_json::json!({
    "success": true,
    "dbPath": db_path,
    "values": values
  }))
}

fn decode_value(bytes: &[u8], secret_key: &Option<[u8; 16]>) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
  let bytes = if let Some(key) = secret_key {
    let decrypted = decryp_aes_128_ecb(HexString::from_vec(bytes.to_vec()), key)
      .map_err(|e| Error::new(ErrorKind::InvalidData, format!("decrypt failed: {e}")))?;
    HexString(decrypted)
      .to_vec()
      .map_err(|e| Error::new(ErrorKind::InvalidData, format!("hex decode failed: {e}")))?
  } else {
    bytes.to_vec()
  };

  let value = serde_json::from_slice(&bytes)
    .map_err(|e| Error::new(ErrorKind::InvalidData, format!("json decode failed: {e}")))?;
  Ok(value)
}
