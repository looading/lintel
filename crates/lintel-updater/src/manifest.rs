use std::{
  fs,
  path::Path,
  time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 签名覆盖的完整发布信息，客户端不会信任清单之外的安装参数。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
  pub schema: u32,
  pub app_id: String,
  pub version: String,
  pub channel: String,
  pub target: String,
  pub url: String,
  pub size: u64,
  pub sha256: String,
  pub expires_at: u64,
  pub notes: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
  payload: String,
  signature: String,
}

pub fn now() -> Result<u64> {
  Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

pub fn verifying_key(encoded: &str) -> Result<VerifyingKey> {
  let bytes: [u8; 32] = STANDARD
    .decode(encoded.trim())
    .context("decode update public key")?
    .try_into()
    .map_err(|_| anyhow::anyhow!("update public key must have 32 bytes"))?;
  let key = VerifyingKey::from_bytes(&bytes)?;
  ensure!(!key.is_weak(), "weak update public key");
  Ok(key)
}

pub fn decode(bytes: &[u8], key: &VerifyingKey) -> Result<Release> {
  let release = decode_history(bytes, key)?;
  ensure!(release.expires_at > now()?, "update manifest has expired");
  Ok(release)
}

// Historical ownership proofs stay authenticated after expiry; never use this to authorize an install.
pub(crate) fn decode_history(bytes: &[u8], key: &VerifyingKey) -> Result<Release> {
  ensure!(bytes.len() <= 65536, "update manifest exceeds 64 KiB");
  let envelope: Envelope = serde_json::from_slice(bytes).context("read signed update envelope")?;
  let payload = STANDARD.decode(envelope.payload)?;
  let signature = Signature::from_slice(&STANDARD.decode(envelope.signature)?)?;
  key
    .verify_strict(&payload, &signature)
    .context("invalid update manifest signature")?;
  let release: Release = serde_json::from_slice(&payload).context("read verified update release")?;
  validate(&release)?;
  Ok(release)
}

pub fn validate(release: &Release) -> Result<()> {
  ensure!(release.schema == 1, "unsupported update schema");
  ensure!(
    release.app_id.contains('.')
      && release
        .app_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b)),
    "invalid application ID"
  );
  semver::Version::parse(&release.version).context("invalid release version")?;
  ensure!(
    !release.channel.is_empty()
      && release
        .channel
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
    "invalid update channel"
  );
  ensure!(
    !release.target.is_empty() && !release.target.chars().any(char::is_control),
    "invalid update target"
  );
  ensure!(
    release.size > 0 && release.size <= 2 * 1024 * 1024 * 1024,
    "update archive size is outside the 2 GiB limit"
  );
  ensure!(
    release.sha256.len() == 64 && release.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
    "invalid archive SHA-256"
  );
  ensure!(release.notes.len() <= 8192, "release notes exceed 8 KiB");
  Ok(())
}

/// 离线生成签名密钥；私钥文件必须保存在发行环境。
pub fn generate_keypair(private: &Path, public: &Path) -> Result<()> {
  let mut seed = [0; 32];
  getrandom::fill(&mut seed).map_err(|error| anyhow::anyhow!("generate update key: {error}"))?;
  let key = SigningKey::from_bytes(&seed);
  let mut options = fs::OpenOptions::new();
  options.write(true).create_new(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
  }
  use std::io::Write;
  let mut file = options
    .open(private)
    .context("create private key without overwriting an existing file")?;
  file.write_all(STANDARD.encode(seed).as_bytes())?;
  file.sync_all()?;
  let public_result = (|| -> Result<()> {
    let mut public = fs::OpenOptions::new().write(true).create_new(true).open(public)?;
    public.write_all(STANDARD.encode(key.verifying_key().as_bytes()).as_bytes())?;
    public.sync_all()?;
    Ok(())
  })();
  if let Err(error) = public_result {
    let _ = fs::remove_file(private);
    return Err(error);
  }
  Ok(())
}

/// 对原始 JSON 字节签名，读取时先验签再解析元数据。
pub fn sign_release(release: &Release, private: &Path) -> Result<Vec<u8>> {
  validate(release)?;
  ensure!(release.expires_at > now()?, "release expiry must be in the future");
  let seed: [u8; 32] = STANDARD
    .decode(fs::read_to_string(private)?.trim())?
    .try_into()
    .map_err(|_| anyhow::anyhow!("private key must contain 32 bytes"))?;
  let payload = serde_json::to_vec(release)?;
  let signature = SigningKey::from_bytes(&seed).sign(&payload);
  Ok(serde_json::to_vec_pretty(&Envelope {
    payload: STANDARD.encode(payload),
    signature: STANDARD.encode(signature.to_bytes()),
  })?)
}

pub fn archive_digest(path: &Path) -> Result<(u64, String)> {
  use std::io::Read;
  let mut file = fs::File::open(path)?;
  let mut hash = Sha256::new();
  let mut total = 0u64;
  let mut buffer = [0; 65536];
  loop {
    let size = file.read(&mut buffer)?;
    if size == 0 {
      break;
    }
    total += size as u64;
    hash.update(&buffer[..size]);
  }
  Ok((total, format!("{:x}", hash.finalize())))
}
