use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize, de};

pub fn cbor4ii_to_vec<T>(data: T) -> Result<Vec<u8>>
where
  T: Serialize,
{
  let ret = cbor4ii::serde::to_vec(Vec::new(), &data).map_err(|err| anyhow!("serialize faild: {:#}", err))?;
  Ok(ret)
}

pub fn cbor4ii_from_slice<T>(data: &[u8]) -> Result<T>
where
  T: for<'a> serde::Deserialize<'a>,
{
  let ret = cbor4ii::serde::from_slice(data).map_err(|err| anyhow!("deserialize faild: {:#}", err))?;
  Ok(ret)
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum NumberOrStringNumber<T = i32>
where
  T: std::fmt::Display + std::str::FromStr + Default + Copy,
{
  Number(T),
  String(String),
}
impl<T> NumberOrStringNumber<T>
where
  T: std::fmt::Display + std::str::FromStr + Default + Copy,
{
  pub fn as_string(&self) -> String {
    match self {
      NumberOrStringNumber::Number(s) => s.to_string(),
      NumberOrStringNumber::String(s) => s.to_string(),
    }
  }

  pub fn as_number(&self) -> T {
    match self {
      NumberOrStringNumber::Number(s) => *s,
      NumberOrStringNumber::String(s) => s.parse().unwrap_or_default(),
    }
  }
}

pub fn de_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
  D: de::Deserializer<'de>,
{
  struct V;

  impl<'de> de::Visitor<'de> for V {
    type Value = f64;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
      formatter.write_str("a number or a numeric string")
    }

    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
      Ok(v)
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
      Ok(v as f64)
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
      Ok(v as f64)
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
      E: de::Error,
    {
      v.trim().parse::<f64>().map_err(E::custom)
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
      E: de::Error,
    {
      v.trim().parse::<f64>().map_err(E::custom)
    }
  }

  deserializer.deserialize_any(V)
}

pub fn de_opt_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
  D: de::Deserializer<'de>,
{
  Ok(Some(de_f64(deserializer)?))
}
