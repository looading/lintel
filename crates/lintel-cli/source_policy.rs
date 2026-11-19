//! Source publication checks. Organization-specific literals stay in an external policy file.

use std::path::{Component, Path};

struct Rule {
  id: String,
  literal: String,
}

#[derive(Default)]
pub struct SourcePolicy {
  rules: Vec<Rule>,
}

impl SourcePolicy {
  /// Each non-comment line is `rule-id<TAB>literal`; failures never include the literal.
  pub fn parse(source: &str) -> Result<Self, String> {
    let mut rules = Vec::new();
    for (line_number, line) in source.lines().enumerate() {
      if line.trim().is_empty() || line.trim_start().starts_with('#') {
        continue;
      }
      let Some((id, literal)) = line.split_once('\t') else {
        return Err(format!("invalid source policy rule at line {}", line_number + 1));
      };
      if id.is_empty()
        || id.len() > 64
        || !id
          .bytes()
          .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        || literal.is_empty()
        || literal.len() > 4096
        || rules.len() >= 256
      {
        return Err(format!("invalid source policy rule at line {}", line_number + 1));
      }
      rules.push(Rule {
        id: id.into(),
        literal: literal.to_lowercase(),
      });
    }
    Ok(Self { rules })
  }

  pub fn check_path(path: &Path) -> Result<(), String> {
    for component in path.components() {
      let Component::Normal(name) = component else {
        return Err("source entry path must be a plain relative path".into());
      };
      let lowercase = name.to_string_lossy().to_ascii_lowercase();
      if matches!(lowercase.as_str(), ".git" | ".gitconfig" | ".gitmodules") {
        return Err(format!("repository-metadata: {}", path.display()));
      }
      if lowercase.ends_with(".dsym") {
        return Err(format!("debug-symbol-artifact: {}", path.display()));
      }
    }
    let name = lowercase_name(path);
    let extension = path
      .extension()
      .map(|extension| extension.to_string_lossy().to_ascii_lowercase());
    let cargo_credentials = path.parent().is_some_and(|parent| lowercase_name(parent) == ".cargo")
      && matches!(name.as_str(), "credentials" | "credentials.toml");
    if matches!(
      name.as_str(),
      ".env" | ".netrc" | ".npmrc" | ".git-credentials" | "credentials.json" | "id_rsa" | "id_ed25519"
    ) || name.starts_with(".env.")
      || cargo_credentials
      || matches!(extension.as_deref(), Some("p12" | "pfx"))
    {
      return Err(format!("credential-file: {}", path.display()));
    }
    if matches!(extension.as_deref(), Some("pdb" | "dwarf" | "dwo" | "dwp" | "debug")) {
      return Err(format!("debug-symbol-artifact: {}", path.display()));
    }
    Ok(())
  }

  pub fn check(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
    Self::check_path(path)?;
    let source = String::from_utf8_lossy(bytes);
    let vendor = starts_with_component(path, "vendor");
    // Preserve upstream authorship and legal notices without exempting credentials,
    // repository metadata, debug symbols or dependency source checks.
    if !vendor {
      for (line_number, line) in source.lines().enumerate() {
        let lowercase = line.to_lowercase();
        for rule in &self.rules {
          if lowercase.contains(&rule.literal) {
            return Err(format!("{}: {}:{}", rule.id, path.display(), line_number + 1));
          }
        }
      }
    }
    if let Some(line) = private_key_line(&source) {
      return Err(format!("private-key-block: {}:{line}", path.display()));
    }
    let name = lowercase_name(path);
    if matches!(name.as_str(), "cargo.toml" | "cargo.lock") || starts_with_component(path, ".cargo") {
      check_git_sources(path, &source)?;
    }
    Ok(())
  }
}

fn lowercase_name(path: &Path) -> String {
  path
    .file_name()
    .map(|name| name.to_string_lossy().to_ascii_lowercase())
    .unwrap_or_default()
}

fn starts_with_component(path: &Path, name: &str) -> bool {
  matches!(path.components().next(), Some(Component::Normal(component)) if component.to_string_lossy().eq_ignore_ascii_case(name))
}

fn quoted_value(value: &str) -> Option<&str> {
  let value = value.trim_start();
  let quote = value.chars().next()?;
  if quote != '"' && quote != '\'' {
    return None;
  }
  value[1..].split_once(quote).map(|(value, _)| value)
}

fn field_value<'a>(line: &'a str, field: &str) -> Option<&'a str> {
  line.match_indices(field).find_map(|(offset, _)| {
    let before = &line[..offset];
    if !before.is_empty() && !before.ends_with([' ', '\t', '{', ',']) {
      return None;
    }
    line[offset + field.len()..].trim_start().strip_prefix('=')
  })
}

fn without_toml_comment(line: &str) -> &str {
  let mut quote = None;
  let mut escape = false;
  for (offset, character) in line.char_indices() {
    if escape {
      escape = false;
    } else if quote == Some('"') && character == '\\' {
      escape = true;
    } else if Some(character) == quote {
      quote = None;
    } else if quote.is_none() && matches!(character, '"' | '\'') {
      quote = Some(character);
    } else if quote.is_none() && character == '#' {
      return &line[..offset];
    }
  }
  line
}

fn check_git_sources(path: &Path, source: &str) -> Result<(), String> {
  let mut git_table = false;
  for (line_number, line) in source.lines().enumerate() {
    let line = without_toml_comment(line.trim());
    if line.starts_with('#') {
      continue;
    }
    if line.starts_with('[') {
      git_table = false;
    }
    let lock_source = line
      .strip_prefix("source")
      .and_then(|value| value.trim_start().strip_prefix('='))
      .and_then(quoted_value)
      .and_then(|value| value.strip_prefix("git+"));
    let manifest_source = field_value(line, "git").and_then(quoted_value);
    if let Some(uri) = lock_source.or(manifest_source) {
      git_table = manifest_source.is_some() && !line.contains('{');
      if !uri.starts_with("https://github.com/")
        || uri.strip_prefix("https://github.com/").is_none_or(|path| {
          let mut components = path.split('/');
          components.next().is_none_or(str::is_empty) || components.next().is_none_or(str::is_empty)
        })
      {
        return Err(format!("public-git-source: {}:{}", path.display(), line_number + 1));
      }
      // A lock entry already pins the full commit hash, including entries whose URL
      // records a public tag or branch. Do not confuse registry sources with Git.
      if lock_source.is_none() && field_value(line, "branch").is_some() {
        return Err(format!("pinned-git-source: {}:{}", path.display(), line_number + 1));
      }
    } else if git_table && field_value(line, "branch").is_some() {
      return Err(format!("pinned-git-source: {}:{}", path.display(), line_number + 1));
    }
  }
  Ok(())
}

fn private_key_line(source: &str) -> Option<usize> {
  let lines: Vec<_> = source.lines().collect();
  for (start, line) in lines.iter().enumerate() {
    let header = line.trim();
    if !matches!(
      header,
      "-----BEGIN PRIVATE KEY-----"
        | "-----BEGIN RSA PRIVATE KEY-----"
        | "-----BEGIN EC PRIVATE KEY-----"
        | "-----BEGIN ENCRYPTED PRIVATE KEY-----"
        | "-----BEGIN OPENSSH PRIVATE KEY-----"
    ) {
      continue;
    }
    let footer = header.replace("BEGIN", "END");
    let mut encoded = String::new();
    for body in &lines[start + 1..] {
      if body.trim() == footer {
        if let Some(decoded) = decode_base64(&encoded) {
          let der = valid_der_sequence(&decoded);
          let openssh = decoded.starts_with(b"openssh-key-v1\0");
          if der || openssh {
            return Some(start + 1);
          }
        }
        break;
      }
      let body = body.trim();
      if body.is_empty()
        || !body
          .bytes()
          .all(|byte| byte.is_ascii_alphanumeric() || b"+/=".contains(&byte))
      {
        break;
      }
      encoded.push_str(body);
      if encoded.len() > 64 * 1024 {
        break;
      }
    }
  }
  None
}

fn valid_der_sequence(bytes: &[u8]) -> bool {
  if bytes.len() <= 16 || bytes.first() != Some(&0x30) {
    return false;
  }
  let length = bytes[1];
  if length < 128 {
    return usize::from(length) + 2 == bytes.len();
  }
  let count = usize::from(length & 0x7f);
  if !(1..=4).contains(&count) || count + 2 >= bytes.len() {
    return false;
  }
  let length = bytes[2..2 + count]
    .iter()
    .fold(0_usize, |length, byte| length * 256 + usize::from(*byte));
  length.checked_add(2 + count) == Some(bytes.len())
}

fn decode_base64(encoded: &str) -> Option<Vec<u8>> {
  if !encoded.len().is_multiple_of(4) {
    return None;
  }
  let mut output = Vec::new();
  for chunk in encoded.as_bytes().chunks_exact(4) {
    let mut value = 0_u32;
    let mut padding = 0;
    for &byte in chunk {
      let digit = match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        b'=' => {
          padding += 1;
          0
        }
        _ => return None,
      };
      if padding > 0 && byte != b'=' {
        return None;
      }
      value = (value << 6) | u32::from(digit);
    }
    if padding > 2 {
      return None;
    }
    output.extend_from_slice(&value.to_be_bytes()[1..4 - padding]);
  }
  Some(output)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn synthetic_private_key_block() -> String {
    // A DER sequence filled with zeros exercises framing; it contains no cryptographic key.
    [
      "-----BEGIN PRIVATE KEY-----",
      "MB4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
      "-----END PRIVATE KEY-----",
    ]
    .join("\n")
  }

  #[test]
  fn supplied_rules_apply_to_source_and_archive_bytes_without_echoing_the_literal() {
    let policy = SourcePolicy::parse("organization-domain\tinternal.example.invalid\n").unwrap();
    for path in ["README.md", "crates/example/src/lib.rs"] {
      let error = policy
        .check(Path::new(path), b"visit https://INTERNAL.example.invalid/private")
        .unwrap_err();
      assert!(error.contains("organization-domain"));
      assert!(!error.contains("internal.example.invalid"));
    }
    assert!(
      policy
        .check(
          Path::new("vendor/upstream/LICENSE"),
          b"internal.example.invalid authors"
        )
        .is_ok()
    );
  }

  #[test]
  fn actual_git_sources_require_public_https_but_registry_sources_and_schema_fields_are_accepted() {
    let policy = SourcePolicy::default();
    for source in [
      "source = \"registry+https://github.com/rust-lang/crates.io-index\"",
      "source = \"git+https://github.com/example/project?branch=main#0123456789012345678901234567890123456789\"",
      "[package.metadata.application]\nbranch = \"demo\"\n",
      "library = { git = \"https://github.com/example/project\", rev = \"0123456789012345678901234567890123456789\" }",
      "library = { git = \"https://github.com/example/branch-tools\", rev = \"0123456789012345678901234567890123456789\" } # branch = documentation",
      "[dependencies.library]\ngit = \"https://github.com/example/project\"\nbranch_name = \"schema-field\"\n# branch = documentation",
    ] {
      assert!(policy.check(Path::new("Cargo.toml"), source.as_bytes()).is_ok());
    }
    for source in [
      "library = { git = \"ssh://git@internal.example.invalid/project\" }",
      "library = { git = \"https://user:credential@github.com/example/project\" }",
      "library = { git = \"https://github.com/example/project\", branch = \"main\" }",
      "[dependencies.library]\ngit = \"https://github.com/example/project\"\nbranch = \"main\"",
    ] {
      assert!(policy.check(Path::new("Cargo.toml"), source.as_bytes()).is_err());
    }
  }

  #[test]
  fn credential_artifacts_and_realistic_pem_are_rejected_but_api_schema_and_placeholders_are_not() {
    let policy = SourcePolicy::default();
    for path in [
      ".env",
      "credentials.json",
      "signing.p12",
      ".git/config",
      "app.pdb",
      "app.dSYM/Contents/Resources/DWARF/app",
    ] {
      assert!(policy.check(Path::new(path), b"placeholder").is_err());
    }
    let pem = synthetic_private_key_block();
    assert!(policy.check(Path::new("signing.pem"), pem.as_bytes()).is_err());
    for source in [
      "pub struct Api { pub token: String, pub password: String }",
      "-----BEGIN PRIVATE KEY-----\nplaceholder\n-----END PRIVATE KEY-----",
    ] {
      assert!(policy.check(Path::new("src/lib.rs"), source.as_bytes()).is_ok());
    }
  }

  #[test]
  fn vendor_legal_text_exception_does_not_admit_credentials_private_keys_or_unsafe_git_sources() {
    let policy = SourcePolicy::parse("organization-domain\tinternal.example.invalid\n").unwrap();
    assert!(
      policy
        .check(
          Path::new("vendor/upstream/LICENSE"),
          b"internal.example.invalid copyright"
        )
        .is_ok()
    );
    for path in [
      "vendor/upstream/.env",
      "vendor/upstream/credentials.json",
      "vendor/upstream/signing.p12",
      "vendor/upstream/.cargo/credentials",
      "vendor/upstream/.cargo/credentials.toml",
      "vendor/upstream/.git-credentials",
    ] {
      let error = policy.check(Path::new(path), b"token = placeholder-only").unwrap_err();
      assert!(error.starts_with("credential-file:"));
      assert!(!error.contains("placeholder-only"));
    }
    let pem = synthetic_private_key_block();
    let error = policy
      .check(Path::new("vendor/upstream/signing.pem"), pem.as_bytes())
      .unwrap_err();
    assert!(error.starts_with("private-key-block:"));
    let mut binary_prefix = vec![0xff, b'\n'];
    binary_prefix.extend_from_slice(pem.as_bytes());
    assert!(
      policy
        .check(Path::new("vendor/upstream/opaque.dat"), &binary_prefix)
        .is_err()
    );
    for source in [
      "library = { git = \"ssh://git@internal.example.invalid/project\" }",
      "library = { git = \"https://user:placeholder-only@github.com/example/project\" }",
      "library = { git = \"https://github.com/example/project\", branch = \"main\" }",
    ] {
      assert!(
        policy
          .check(Path::new("vendor/upstream/Cargo.toml"), source.as_bytes())
          .is_err()
      );
    }
    assert!(policy.check(Path::new("vendor/upstream/Cargo.toml"),
      b"library = { git = \"https://github.com/example/project\", rev = \"0123456789012345678901234567890123456789\" }").is_ok());
  }

  #[test]
  fn reserved_paths_are_case_insensitive_for_source_and_vendor_archives() {
    let policy = SourcePolicy::default();
    for prefix in ["", "vendor/upstream/"] {
      for suffix in [
        ".GIT/config",
        ".GitConfig",
        ".GITMODULES",
        "App.DsYm/Contents/Resources/DWARF/app",
        "App.PDB",
        "App.DWARF",
        "App.DWO",
        "App.DWP",
        "App.DEBUG",
        ".ENV",
        ".Env.Local",
        ".NetRc",
        ".NPMRC",
        "CREDENTIALS.JSON",
        "ID_RSA",
        "ID_ED25519",
        "Signing.PFX",
        "Signing.P12",
        ".GIT-CREDENTIALS",
        ".CARGO/CREDENTIALS",
        ".Cargo/Credentials.Toml",
      ] {
        let path = format!("{prefix}{suffix}");
        assert!(
          SourcePolicy::check_path(Path::new(&path)).is_err(),
          "path accepted: {path}"
        );
        assert!(
          policy.check(Path::new(&path), b"placeholder-only").is_err(),
          "path accepted: {path}"
        );
      }
    }
    for path in [
      "src/credentials.rs",
      "docs/credentials.md",
      "vendor/upstream/src/credentials.rs",
    ] {
      assert!(
        policy
          .check(Path::new(path), b"pub struct Api { token: String, password: String }")
          .is_ok()
      );
    }
  }

  #[test]
  fn malformed_policy_errors_do_not_include_any_policy_values() {
    for source in ["unseparated-value", "invalid id\tvalue", "rule\t"] {
      let error = SourcePolicy::parse(source).err().unwrap();
      assert_eq!(error, "invalid source policy rule at line 1");
    }
  }
}
