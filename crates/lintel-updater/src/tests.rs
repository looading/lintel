use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signer, SigningKey};
use std::{
  fs,
  io::{Read, Write},
  net::TcpListener,
};

fn keys() -> (SigningKey, String) {
  let key = SigningKey::from_bytes(&[7; 32]);
  let public = STANDARD.encode(key.verifying_key().as_bytes());
  (key, public)
}

fn release(url: &str, bytes: &[u8]) -> Release {
  Release {
    schema: 1,
    app_id: "dev.lintel.test".into(),
    version: "2.0.0".into(),
    channel: "stable".into(),
    target: target().into(),
    url: url.into(),
    size: bytes.len() as u64,
    sha256: format!("{:x}", Sha256::digest(bytes)),
    expires_at: manifest::now().unwrap() + 600,
    notes: "test".into(),
  }
}

fn envelope(release: &Release) -> Vec<u8> {
  let payload = serde_json::to_vec(release).unwrap();
  let signature = keys().0.sign(&payload);
  serde_json::to_vec(
    &serde_json::json!({"payload":STANDARD.encode(payload),"signature":STANDARD.encode(signature.to_bytes())}),
  )
  .unwrap()
}

#[test]
fn authenticated_metadata_rejects_tampering_expiry_and_wrong_keys() {
  let mut release = release("https://example.test/update", b"content");
  let bytes = envelope(&release);
  let key = manifest::verifying_key(&keys().1).unwrap();
  assert_eq!(manifest::decode(&bytes, &key).unwrap().version, "2.0.0");
  let wrong = SigningKey::from_bytes(&[8; 32]).verifying_key();
  assert!(manifest::decode(&bytes, &wrong).is_err());
  let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
  release.url = "https://attacker.test/update".into();
  value["payload"] = STANDARD.encode(serde_json::to_vec(&release).unwrap()).into();
  assert!(manifest::decode(&serde_json::to_vec(&value).unwrap(), &key).is_err());
  release.expires_at = 1;
  assert!(manifest::decode(&envelope(&release), &key).is_err());
  assert!(manifest::decode(&vec![0; 65537], &key).is_err());
  assert!(manifest::verifying_key(&STANDARD.encode([0; 32])).is_err());
}

#[test]
fn cloned_configuration_does_not_panic_or_change_an_existing_client() {
  let updater = Updater::new("dev.lintel.test", "1.0.0", "https://example.test/feed", &keys().1).unwrap();
  let second = updater.clone().channel("beta").allow_local_http_for_testing();
  assert_eq!(updater.inner.channel, "stable");
  assert!(!updater.inner.local_http);
  assert_eq!(second.inner.channel, "beta");
}

#[test]
fn testing_http_is_limited_to_numeric_loopback_and_no_url_credentials() {
  for value in [
    "http://example.test/x",
    "http://localhost/x",
    "file:///tmp/x",
    "https://user:secret@example.test/x",
  ] {
    assert!(validate_url(&Url::parse(value).unwrap(), true).is_err());
  }
  assert!(validate_url(&Url::parse("http://127.0.0.1:42/x").unwrap(), false).is_err());
  assert!(validate_url(&Url::parse("http://127.0.0.1:42/x").unwrap(), true).is_ok());
  assert!(validate_url(&Url::parse("https://example.test/x").unwrap(), false).is_ok());
}

fn server(make: impl FnOnce(&str) -> Vec<Vec<u8>>) -> (String, std::thread::JoinHandle<()>) {
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  let base = format!("http://{}", listener.local_addr().unwrap());
  let replies = make(&base);
  let thread = std::thread::spawn(move || {
    for reply in replies {
      let (mut socket, _) = listener.accept().unwrap();
      socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
      let mut bytes = Vec::new();
      let mut block = [0; 1024];
      while !bytes.windows(4).any(|x| x == b"\r\n\r\n") {
        let count = socket.read(&mut block).unwrap();
        if count == 0 {
          break;
        }
        bytes.extend_from_slice(&block[..count]);
      }
      socket.write_all(&reply).unwrap();
    }
  });
  (base, thread)
}

fn response(body: &[u8]) -> Vec<u8> {
  let mut bytes = format!(
    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
    body.len()
  )
  .into_bytes();
  bytes.extend_from_slice(body);
  bytes
}

#[tokio::test]
async fn checks_signed_feed_downloads_and_reports_exact_progress() {
  let (base, thread) = server(|base| {
    vec![
      response(&envelope(&release(&format!("{base}/update"), b"archive"))),
      response(b"archive"),
    ]
  });
  let updater = Updater::new("dev.lintel.test", "1.0.0", &format!("{base}/feed"), &keys().1)
    .unwrap()
    .allow_local_http_for_testing();
  let update = updater.check().await.unwrap().unwrap();
  let mut progress = Vec::new();
  let downloaded = update.download(|p| progress.push(p)).await.unwrap();
  assert_eq!(fs::read(downloaded.archive.path()).unwrap(), b"archive");
  assert_eq!(progress.last().unwrap().downloaded, 7);
  thread.join().unwrap();
}

#[tokio::test]
async fn rejects_hash_size_platform_channel_and_old_version() {
  for case in ["hash", "size", "platform", "channel", "old", "app", "prerelease"] {
    let (base, thread) = server(|base| {
      let mut release = release(&format!("{base}/update"), b"archive");
      match case {
        "hash" => release.sha256 = "0".repeat(64),
        "size" => release.size = 8,
        "platform" => release.target = "other".into(),
        "channel" => release.channel = "beta".into(),
        "old" => release.version = "0.1.0".into(),
        "app" => release.app_id = "dev.other.app".into(),
        "prerelease" => release.version = "2.0.0-beta.1".into(),
        _ => unreachable!(),
      }
      let mut replies = vec![response(&envelope(&release))];
      if ["hash", "size"].contains(&case) {
        replies.push(response(b"archive"));
      }
      replies
    });
    let updater = Updater::new("dev.lintel.test", "1.0.0", &base, &keys().1)
      .unwrap()
      .allow_local_http_for_testing();
    let check = updater.check().await;
    match case {
      "old" => assert!(check.unwrap().is_none()),
      "platform" | "channel" | "app" | "prerelease" => assert!(check.is_err()),
      _ => assert!(check.unwrap().unwrap().download(|_| {}).await.is_err()),
    }
    thread.join().unwrap();
  }
}

#[tokio::test]
async fn refuses_redirect_to_non_https_non_loopback_origin() {
  let (base, thread) = server(|_| {
    vec![
      b"HTTP/1.1 302 Found\r\nLocation: http://example.test/unsafe\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        .to_vec(),
    ]
  });
  assert!(fetch(&Url::parse(&base).unwrap(), true).await.is_err());
  thread.join().unwrap();
}

#[test]
fn key_generation_does_not_overwrite_or_leave_a_private_key_on_public_collision() {
  let dir = tempfile::tempdir().unwrap();
  let private = dir.path().join("private");
  let public = dir.path().join("public");
  fs::write(&public, b"existing").unwrap();
  assert!(generate_keypair(&private, &public).is_err());
  assert!(!private.exists());
  assert_eq!(fs::read(&public).unwrap(), b"existing");
  fs::remove_file(&public).unwrap();
  generate_keypair(&private, &public).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(fs::metadata(&private).unwrap().permissions().mode() & 0o777, 0o600);
  }
  let bytes = fs::read(&private).unwrap();
  assert!(generate_keypair(&private, &public).is_err());
  assert_eq!(fs::read(&private).unwrap(), bytes);
}

fn tarball(entries: &[(&str, tar::EntryType, Option<&str>, &[u8])]) -> tempfile::NamedTempFile {
  let file = tempfile::NamedTempFile::new().unwrap();
  let writer = flate2::write::GzEncoder::new(file.reopen().unwrap(), flate2::Compression::fast());
  let mut tar = tar::Builder::new(writer);
  for (name, kind, link, body) in entries {
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o755);
    header.set_size(body.len() as u64);
    header.set_entry_type(*kind);
    header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
    if let Some(link) = link {
      header.as_mut_bytes()[157..157 + link.len()].copy_from_slice(link.as_bytes());
    }
    header.set_cksum();
    tar.append(&header, *body).unwrap();
  }
  tar.into_inner().unwrap().finish().unwrap();
  file
}

#[test]
fn extraction_preserves_internal_symlinks_and_rejects_unsafe_entries() {
  let archive = tarball(&[
    ("Test.app/Contents/file", tar::EntryType::Regular, None, b"ok"),
    ("Test.app/Contents/link", tar::EntryType::Symlink, Some("file"), b""),
  ]);
  let root = tempfile::tempdir().unwrap();
  #[cfg(unix)]
  {
    let app = crate::archive::extract(archive.path(), root.path()).unwrap();
    assert_eq!(fs::read(app.join("Contents/link")).unwrap(), b"ok");
  }
  #[cfg(not(unix))]
  assert!(crate::archive::extract(archive.path(), root.path()).is_err());
  for entries in [
    vec![("../escape", tar::EntryType::Regular, None, b"bad".as_slice())],
    vec![("Test.app/x", tar::EntryType::Symlink, Some("/tmp"), b"".as_slice())],
    vec![("Test.app/x", tar::EntryType::Symlink, Some("../escape"), b"".as_slice())],
    vec![(
      "Test.app/x",
      tar::EntryType::Link,
      Some("Contents/file"),
      b"".as_slice(),
    )],
    vec![("Test.app/x", tar::EntryType::Fifo, None, b"".as_slice())],
    vec![
      ("Test.app/x", tar::EntryType::Symlink, Some("Contents"), b"".as_slice()),
      ("Test.app/x/escape", tar::EntryType::Regular, None, b"bad".as_slice()),
    ],
    vec![
      ("Test.app/x", tar::EntryType::Regular, None, b"ok".as_slice()),
      ("Other.app/x", tar::EntryType::Regular, None, b"bad".as_slice()),
    ],
  ] {
    let archive = tarball(&entries);
    let root = tempfile::tempdir().unwrap();
    assert!(crate::archive::extract(archive.path(), root.path()).is_err());
  }
}

#[test]
fn oversized_extension_metadata_is_rejected_before_tar_allocates_its_body() {
  let file = tempfile::NamedTempFile::new().unwrap();
  let mut gzip = flate2::write::GzEncoder::new(file.reopen().unwrap(), flate2::Compression::fast());
  let mut header = tar::Header::new_gnu();
  header.set_path("././@LongLink").unwrap();
  header.set_entry_type(tar::EntryType::GNULongName);
  header.set_size(512 * 1024 * 1024);
  header.set_cksum();
  gzip.write_all(header.as_bytes()).unwrap();
  gzip.finish().unwrap();
  let directory = tempfile::tempdir().unwrap();
  let error = crate::archive::extract(file.path(), directory.path()).unwrap_err();
  assert!(
    format!("{error:#}").contains("extension metadata exceeds limit"),
    "{error:#}"
  );
}

#[test]
fn pax_size_override_cannot_hide_extension_headers_from_the_limiter() {
  let archive = tempfile::NamedTempFile::new().unwrap();
  fs::write(
    archive.path(),
    include_bytes!("../tests/fixtures/pax-size-override.tar.gz"),
  )
  .unwrap();
  let directory = tempfile::tempdir().unwrap();
  let error = crate::archive::extract(archive.path(), directory.path()).unwrap_err();
  assert!(
    format!("{error:#}").contains("PAX and sparse entries are prohibited"),
    "{error:#}"
  );
}
