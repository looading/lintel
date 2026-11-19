//! Submit signed macOS applications to Apple and atomically attach accepted tickets.

use std::{
  ffi::OsString,
  fs,
  io::Cursor,
  path::{Path, PathBuf},
  process::{Command, Output, Stdio},
};

use anyhow::{Context, Result, bail, ensure};
use plist::Value;
use serde::Deserialize;

use crate::{
  NotarizeOptions, NotaryCredentials, NotarySubmission,
  macos::{lock_output, publish_locked},
  signing_components::{Component, ComponentKind, components},
};

const DEVELOPER_ID_REQUIREMENT: &str = "=anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists";

trait Runner {
  fn output(&self, command: &mut Command) -> Result<Output>;
}

struct System;

impl Runner for System {
  fn output(&self, command: &mut Command) -> Result<Output> {
    command
      .output()
      .with_context(|| format!("run {:?}", command.get_program()))
  }
}

/// Upload a stable signed app snapshot without waiting or modifying the original.
pub fn submit_notarization(bundle: &Path, credentials: &NotaryCredentials) -> Result<NotarySubmission> {
  validate_credentials(credentials)?;
  with_staged_copy(bundle, false, &System, |staged| submit(staged, credentials, &System))
}

/// Submit, wait, staple, and publish only after signature and ticket verification.
pub fn notarize_bundle(bundle: &Path, options: &NotarizeOptions) -> Result<NotarySubmission> {
  validate_options(options)?;
  with_staged_copy(bundle, true, &System, |staged| notarize(staged, options, &System))
}

/// Complete notarization inside a private bundle staging directory owned by the caller.
pub(crate) fn notarize_staged(bundle: &Path, options: &NotarizeOptions) -> Result<NotarySubmission> {
  validate_options(options)?;
  let bundle = checked_bundle(bundle)?;
  notarize(&bundle, options, &System)
}

/// Query a previously uploaded submission using a stored Keychain credential profile.
pub fn notarization_info(id: &str, credentials: &NotaryCredentials) -> Result<NotarySubmission> {
  validate_id(id)?;
  validate_credentials(credentials)?;
  let mut command = notary_command("info", credentials);
  command.arg(id);
  let output = checked(&System, &mut command).with_context(|| format!("query notarization {id}"))?;
  parse_submission(&output.stdout, Some(id), true)
}

/// Wait for an accepted result; timeouts and rejected submissions retain their ID in the error.
pub fn wait_for_notarization(id: &str, options: &NotarizeOptions) -> Result<NotarySubmission> {
  validate_id(id)?;
  validate_options(options)?;
  wait(id, options, &System)
}

/// Fetch Apple's full JSON log for a completed submission.
pub fn notarization_log(id: &str, credentials: &NotaryCredentials) -> Result<String> {
  validate_id(id)?;
  validate_credentials(credentials)?;
  let mut command = notary_command("log", credentials);
  command.arg(id);
  let output = checked(&System, &mut command).with_context(|| format!("fetch notarization log {id}"))?;
  let value: serde_json::Value = serde_json::from_slice(&output.stdout).context("invalid notarization log JSON")?;
  ensure!(value.is_object(), "notarization log must be a JSON object");
  if let Some(log_id) = value.get("jobId").and_then(serde_json::Value::as_str) {
    ensure!(
      id.eq_ignore_ascii_case(log_id),
      "notarization log has a different submission ID"
    );
  }
  String::from_utf8(output.stdout).context("notarization log is not UTF-8")
}

/// Attach an existing Apple ticket on a private copy and publish the verified app atomically.
pub fn staple_bundle(bundle: &Path) -> Result<PathBuf> {
  let bundle = checked_bundle(bundle)?;
  with_staged_copy(&bundle, true, &System, |staged| {
    preflight(staged, &System)?;
    staple(staged, &System)
  })?;
  Ok(bundle)
}

/// Verify nested code signatures and the locally attached notarization ticket.
pub fn verify_notarization(bundle: &Path) -> Result<()> {
  let bundle = checked_bundle(bundle)?;
  verify_code(&bundle, &System)?;
  validate_ticket(&bundle, &System)
}

fn notarize(bundle: &Path, options: &NotarizeOptions, runner: &impl Runner) -> Result<NotarySubmission> {
  let submission = submit(bundle, &options.credentials, runner)?;
  let accepted = wait(&submission.id, options, runner)?;
  staple(bundle, runner).with_context(|| {
    format!(
      "notarization {} was accepted but ticket attachment failed; retry with lintel staple",
      submission.id
    )
  })?;
  Ok(accepted)
}

fn submit(bundle: &Path, credentials: &NotaryCredentials, runner: &impl Runner) -> Result<NotarySubmission> {
  preflight(bundle, runner)?;
  let archive_dir = tempfile::Builder::new().prefix("lintel-notary-upload-").tempdir()?;
  let archive = archive_dir.path().join("application.zip");
  checked(
    runner,
    Command::new("/usr/bin/ditto")
      .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
      .arg(bundle)
      .arg(&archive)
      .stdin(Stdio::null()),
  )
  .context("create signed application ZIP for notarization")?;
  let mut command = notary_command("submit", credentials);
  command.arg(&archive).arg("--no-wait");
  let output = runner
    .output(&mut command)
    .context("submit application ZIP to Apple notarization")?;
  let submission = parse_submission(&output.stdout, None, false);
  // Emit this before checking exit status: a failed client may have already
  // created a remote job, which must remain resumable without another upload.
  if let Ok(submission) = &submission {
    eprintln!("Submitted notarization {}", submission.id);
  }
  ensure_success(&command, &output).with_context(|| match &submission {
    Ok(submission) => format!(
      "upload returned notarization ID {} but notarytool failed; inspect with lintel notarization info {} --keychain-profile <PROFILE>",
      submission.id, submission.id
    ),
    Err(_) => "submit application ZIP to Apple notarization".to_owned(),
  })?;
  submission
}

fn wait(id: &str, options: &NotarizeOptions, runner: &impl Runner) -> Result<NotarySubmission> {
  let mut command = notary_command("wait", &options.credentials);
  command
    .arg(id)
    .arg("--timeout")
    .arg(format!("{}s", options.timeout_seconds));
  let output = runner
    .output(&mut command)
    .with_context(|| format!("wait for notarization {id}"))?;
  // Some notarytool versions return failure for Invalid while still emitting useful JSON.
  if let Ok(submission) = parse_submission(&output.stdout, Some(id), true) {
    if submission.status == "In Progress" {
      bail!(
        "notarization {id} is still processing; resume with lintel notarization wait {id} --keychain-profile <PROFILE>; {}",
        String::from_utf8_lossy(&output.stderr).trim()
      );
    }
    ensure!(
      output.status.success() && submission.status == "Accepted",
      "notarization {id} is {}; inspect with lintel notarization log {id} --keychain-profile <PROFILE>; {}",
      submission.status,
      String::from_utf8_lossy(&output.stderr).trim()
    );
    return Ok(submission);
  }
  ensure_success(&command, &output).with_context(|| {
    format!("wait for notarization {id}; Apple may still be processing it; resume with lintel notarization wait {id} --keychain-profile <PROFILE>")
  })?;
  parse_submission(&output.stdout, Some(id), true).with_context(|| format!("read notarization result {id}"))
}

fn staple(bundle: &Path, runner: &impl Runner) -> Result<()> {
  checked(
    runner,
    Command::new("/usr/bin/xcrun")
      .args(["stapler", "staple", "-v"])
      .arg(bundle)
      .stdin(Stdio::null()),
  )
  .context("staple Apple's notarization ticket")?;
  verify_code(bundle, runner)?;
  validate_ticket(bundle, runner)
}

fn validate_ticket(bundle: &Path, runner: &impl Runner) -> Result<()> {
  checked(
    runner,
    Command::new("/usr/bin/xcrun")
      .args(["stapler", "validate", "-v"])
      .arg(bundle)
      .stdin(Stdio::null()),
  )
  .context("validate the attached notarization ticket")?;
  Ok(())
}

fn notary_command(operation: &str, credentials: &NotaryCredentials) -> Command {
  let mut command = Command::new("/usr/bin/xcrun");
  command
    .args([
      "notarytool",
      operation,
      "--output-format",
      "json",
      "--no-progress",
      "--keychain-profile",
    ])
    .arg(&credentials.keychain_profile)
    .stdin(Stdio::null());
  if let Some(keychain) = &credentials.keychain {
    command.arg("--keychain").arg(keychain);
  }
  command
}

fn preflight(bundle: &Path, runner: &impl Runner) -> Result<()> {
  verify_code(bundle, runner)?;
  for component in components(bundle)? {
    let executable = component_executable(&component)?;
    let output = checked(runner, Command::new("/usr/bin/lipo").arg("-archs").arg(&executable))?;
    let architectures = std::str::from_utf8(&output.stdout).context("non-UTF-8 Mach-O architecture list")?;
    ensure!(
      !architectures.trim().is_empty(),
      "code has no architectures: {}",
      executable.display()
    );
    for architecture in architectures.split_whitespace() {
      let mut verify = component_command(&component, &["--verify", "--strict"]);
      verify
        .arg("--architecture")
        .arg(architecture)
        .arg("--test-requirement")
        .arg(DEVELOPER_ID_REQUIREMENT);
      checked(runner, &mut verify).with_context(|| {
        format!(
          "notarization requires a valid Developer ID Application signature for {} ({architecture})",
          component.path.display()
        )
      })?;
      let mut display = component_command(&component, &["--display", "--verbose=4"]);
      display.arg("--architecture").arg(architecture);
      let output = checked(runner, &mut display)?;
      let details = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
      );
      let process = matches!(
        component.kind,
        ComponentKind::MainApp | ComponentKind::HelperApp | ComponentKind::Executable
      );
      validate_signature_details(&details, process)
        .with_context(|| format!("notarization preflight {} ({architecture})", component.path.display()))?;
      let mut entitlements = component_command(&component, &["--display", "--entitlements", "-", "--xml"]);
      entitlements.arg("--architecture").arg(architecture);
      let output = checked(runner, &mut entitlements)?;
      validate_entitlements(&output.stdout).with_context(|| {
        format!(
          "notarization entitlements {} ({architecture})",
          component.path.display()
        )
      })?;
    }
  }
  Ok(())
}

fn component_executable(component: &Component) -> Result<PathBuf> {
  let (root, info, fallback) = match component.kind {
    ComponentKind::MainApp | ComponentKind::HelperApp => {
      let contents = component.path.join("Contents");
      (contents.join("MacOS"), contents.join("Info.plist"), None)
    }
    ComponentKind::Framework => {
      let root = match &component.bundle_version {
        Some(version) => component.path.join("Versions").join(version),
        None => component.path.clone(),
      };
      let fallback = component
        .path
        .file_stem()
        .and_then(|name| name.to_str())
        .map(str::to_owned);
      (root.clone(), root.join("Resources/Info.plist"), fallback)
    }
    ComponentKind::MachO | ComponentKind::Executable => return Ok(component.path.clone()),
  };
  let info = if info.is_file() {
    Some(Value::from_file(&info)?)
  } else {
    None
  };
  let name = info
    .as_ref()
    .and_then(Value::as_dictionary)
    .and_then(|value| value.get("CFBundleExecutable"))
    .and_then(Value::as_string)
    .map(str::to_owned)
    .or(fallback)
    .context("signed code bundle has no executable name")?;
  root
    .join(name)
    .canonicalize()
    .context("locate signed bundle executable")
}

fn validate_signature_details(text: &str, process: bool) -> Result<()> {
  let field = |name: &str| text.lines().find_map(|line| line.strip_prefix(name));
  ensure!(
    field("Signature=") != Some("adhoc"),
    "ad-hoc signatures cannot be notarized"
  );
  ensure!(
    field("TeamIdentifier=").is_some_and(|team| !team.trim().is_empty() && team.trim() != "not set"),
    "notarization requires a certificate-backed TeamIdentifier"
  );
  ensure!(
    field("Timestamp=").is_some_and(|timestamp| !timestamp.trim().is_empty() && timestamp.trim() != "none"),
    "notarization requires a secure timestamp on every code signature"
  );
  if process {
    let runtime = text
      .lines()
      .filter(|line| line.starts_with("CodeDirectory "))
      .any(|line| {
        line
          .split_once("flags=0x")
          .and_then(|(_, flags)| flags.split(['(', ' ']).next())
          .and_then(|flags| u32::from_str_radix(flags, 16).ok())
          .is_some_and(|flags| flags & 0x10000 != 0)
      });
    ensure!(
      runtime,
      "notarization requires Hardened Runtime for every executable process"
    );
  }
  Ok(())
}

fn validate_entitlements(bytes: &[u8]) -> Result<()> {
  if bytes.is_empty() {
    return Ok(());
  }
  let value = Value::from_reader(Cursor::new(bytes)).context("read signed entitlements")?;
  let dictionary = value
    .as_dictionary()
    .context("signed entitlements are not a dictionary")?;
  ensure!(
    dictionary
      .get("com.apple.security.get-task-allow")
      .is_none_or(|value| *value == Value::Boolean(false)),
    "notarization prohibits enabled or non-boolean com.apple.security.get-task-allow"
  );
  Ok(())
}

fn verify_code(bundle: &Path, runner: &impl Runner) -> Result<()> {
  for component in components(bundle)? {
    checked(
      runner,
      &mut component_command(&component, &["--verify", "--strict", "--all-architectures"]),
    )
    .with_context(|| format!("verify code signature {}", component.path.display()))?;
  }
  checked(
    runner,
    Command::new("/usr/bin/codesign")
      .args(["--verify", "--deep", "--strict", "--all-architectures"])
      .arg(bundle)
      .stdin(Stdio::null()),
  )
  .context("verify complete application signature")?;
  Ok(())
}

fn component_command(component: &Component, args: &[&str]) -> Command {
  let mut command = Command::new("/usr/bin/codesign");
  command.args(args).stdin(Stdio::null());
  if let Some(version) = &component.bundle_version {
    command.arg("--bundle-version").arg(version);
  }
  command.arg(&component.path);
  command
}

fn with_staged_copy<T>(
  bundle: &Path,
  publish: bool,
  runner: &impl Runner,
  action: impl FnOnce(&Path) -> Result<T>,
) -> Result<T> {
  let bundle = checked_bundle(bundle)?;
  let parent = bundle.parent().context("application has no parent directory")?;
  let name = bundle.file_name().context("application has no filename")?;
  let mut lock_name = OsString::from(".");
  lock_name.push(name);
  lock_name.push(".lock");
  let lock = lock_output(&parent.join(lock_name))?;
  let identifier = bundle_identifier(&bundle)?;
  let directory = tempfile::Builder::new().prefix(".lintel-notary-").tempdir_in(parent)?;
  let staged = directory.path().join(name);
  checked(runner, Command::new("/usr/bin/ditto").arg(&bundle).arg(&staged))
    .context("stage application for notarization")?;
  ensure!(
    bundle_identifier(&staged)? == identifier,
    "staged application changed bundle identifier"
  );
  let result = action(&staged)?;
  if publish {
    publish_locked(&staged, &bundle, &identifier, &lock)?;
  }
  Ok(result)
}

fn checked_bundle(bundle: &Path) -> Result<PathBuf> {
  let metadata = fs::symlink_metadata(bundle).with_context(|| format!("inspect application {}", bundle.display()))?;
  ensure!(
    metadata.is_dir() && bundle.extension().is_some_and(|extension| extension == "app"),
    "expected a real application directory ending in .app: {}",
    bundle.display()
  );
  bundle.canonicalize().context("resolve application bundle")
}

fn bundle_identifier(bundle: &Path) -> Result<String> {
  let info = Value::from_file(bundle.join("Contents/Info.plist"))?;
  let identifier = info
    .as_dictionary()
    .and_then(|value| value.get("CFBundleIdentifier"))
    .and_then(Value::as_string)
    .filter(|id| !id.is_empty() && !id.chars().any(char::is_control))
    .context("application has no valid CFBundleIdentifier")?;
  Ok(identifier.to_owned())
}

fn validate_credentials(credentials: &NotaryCredentials) -> Result<()> {
  ensure!(
    !credentials.keychain_profile.trim().is_empty() && !credentials.keychain_profile.chars().any(char::is_control),
    "provide a non-empty Keychain credential profile"
  );
  if let Some(path) = &credentials.keychain {
    ensure!(path.is_file(), "Keychain path is not a file: {}", path.display());
  }
  Ok(())
}

pub(crate) fn validate_options(options: &NotarizeOptions) -> Result<()> {
  validate_credentials(&options.credentials)?;
  ensure!(
    options.timeout_seconds > 0,
    "notarization timeout must be greater than zero"
  );
  Ok(())
}

fn validate_id(id: &str) -> Result<()> {
  ensure!(
    id.len() == 36
      && id.bytes().enumerate().all(|(position, byte)| {
        if [8, 13, 18, 23].contains(&position) {
          byte == b'-'
        } else {
          byte.is_ascii_hexdigit()
        }
      }),
    "notarization submission ID must be a UUID"
  );
  Ok(())
}

fn parse_submission(bytes: &[u8], expected_id: Option<&str>, require_status: bool) -> Result<NotarySubmission> {
  #[derive(Deserialize)]
  struct Response {
    id: String,
    status: Option<String>,
    message: Option<String>,
  }
  let response: Response = serde_json::from_slice(bytes).context("invalid notarytool response JSON")?;
  validate_id(&response.id)?;
  if let Some(expected) = expected_id {
    ensure!(
      response.id.eq_ignore_ascii_case(expected),
      "notarytool returned a different submission ID"
    );
  }
  let status = response.status.unwrap_or_else(|| {
    if require_status {
      String::new()
    } else {
      "Submitted".to_owned()
    }
  });
  ensure!(
    !status.trim().is_empty() && !status.chars().any(char::is_control),
    "notarytool response has no valid submission status"
  );
  Ok(NotarySubmission {
    id: response.id,
    status,
    message: response.message,
  })
}

fn checked(runner: &impl Runner, command: &mut Command) -> Result<Output> {
  let output = runner.output(command)?;
  ensure_success(command, &output)?;
  Ok(output)
}

fn ensure_success(command: &Command, output: &Output) -> Result<()> {
  if !output.status.success() {
    bail!(
      "{:?} failed with {}: {}",
      command.get_program(),
      output.status,
      String::from_utf8_lossy(&output.stderr).trim()
    );
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{cell::RefCell, os::unix::process::ExitStatusExt};

  const ID: &str = "01234567-89ab-cdef-0123-456789abcdef";

  fn credentials() -> NotaryCredentials {
    NotaryCredentials {
      keychain_profile: "fixture-profile".to_owned(),
      keychain: None,
    }
  }

  fn options() -> NotarizeOptions {
    NotarizeOptions {
      credentials: credentials(),
      timeout_seconds: 30,
    }
  }

  fn response(status: &str) -> String {
    format!(r#"{{"id":"{ID}","status":"{status}"}}"#)
  }

  #[test]
  fn parses_uploaded_and_terminal_results_without_accepting_missing_or_mismatched_ids() {
    let uploaded = format!(r#"{{"id":"{ID}","message":"Successfully uploaded file"}}"#);
    assert_eq!(
      parse_submission(uploaded.as_bytes(), None, false).unwrap().status,
      "Submitted"
    );
    for status in ["Accepted", "Invalid", "In Progress", "Rejected"] {
      assert_eq!(
        parse_submission(response(status).as_bytes(), Some(ID), true)
          .unwrap()
          .status,
        status
      );
    }
    assert!(parse_submission(uploaded.as_bytes(), Some(ID), true).is_err());
    assert!(parse_submission(b"not JSON", None, false).is_err());
    assert!(parse_submission(b"{}", None, false).is_err());
    let injected = format!(r#"{{"id":"{ID}","status":"Accepted\tinjected"}}"#);
    assert!(parse_submission(injected.as_bytes(), Some(ID), true).is_err());
    assert!(
      parse_submission(
        response("Accepted").as_bytes(),
        Some("11234567-89ab-cdef-0123-456789abcdef"),
        true
      )
      .is_err()
    );
    assert!(validate_id("--option").is_err());
  }

  #[test]
  fn signature_preflight_requires_certificate_timestamp_and_runtime_for_processes() {
    let valid = "CodeDirectory v=20500 flags=0x10000(runtime)\nTeamIdentifier=EXAMPLE123\nTimestamp=Nov 11, 2026\n";
    validate_signature_details(valid, true).unwrap();
    validate_signature_details(&valid.replace("0x10000(runtime)", "0x0(none)"), false).unwrap();
    assert!(validate_signature_details(&valid.replace("0x10000(runtime)", "0x0(none)"), true).is_err());
    assert!(
      validate_signature_details(
        &valid.replace("Timestamp=Nov 11, 2026", "Signed Time=Nov 11, 2026"),
        false
      )
      .is_err()
    );
    assert!(
      validate_signature_details(
        &valid.replace("TeamIdentifier=EXAMPLE123", "TeamIdentifier=not set"),
        false
      )
      .is_err()
    );
    assert!(validate_signature_details(&format!("{valid}Signature=adhoc\n"), false).is_err());
  }

  #[test]
  fn entitlements_reject_debug_access_and_malformed_values() {
    validate_entitlements(&[]).unwrap();
    for value in [
      Value::Boolean(true),
      Value::String("YES".to_owned()),
      Value::Integer(1.into()),
    ] {
      let dictionary = plist::Dictionary::from_iter([("com.apple.security.get-task-allow".to_owned(), value)]);
      let mut bytes = Vec::new();
      Value::Dictionary(dictionary).to_writer_xml(&mut bytes).unwrap();
      assert!(validate_entitlements(&bytes).is_err());
    }
    let dictionary =
      plist::Dictionary::from_iter([("com.apple.security.get-task-allow".to_owned(), Value::Boolean(false))]);
    let mut bytes = Vec::new();
    Value::Dictionary(dictionary).to_writer_binary(&mut bytes).unwrap();
    validate_entitlements(&bytes).unwrap();
    assert!(validate_entitlements(b"bad plist").is_err());
  }

  #[derive(Clone, Copy)]
  enum Scenario {
    Accepted,
    UploadFailureAfterId,
    Invalid,
    InvalidSuccessfulExit,
    Timeout,
    InProgress,
    StapleFailure,
    TicketFailure,
  }

  struct Fake {
    scenario: Scenario,
    calls: RefCell<Vec<Vec<OsString>>>,
  }

  impl Fake {
    fn new(scenario: Scenario) -> Self {
      Self {
        scenario,
        calls: RefCell::new(Vec::new()),
      }
    }
  }

  fn output(success: bool, stdout: &str, stderr: &str) -> Output {
    Output {
      status: std::process::ExitStatus::from_raw(if success { 0 } else { 1 << 8 }),
      stdout: stdout.as_bytes().to_vec(),
      stderr: stderr.as_bytes().to_vec(),
    }
  }

  impl Runner for Fake {
    fn output(&self, command: &mut Command) -> Result<Output> {
      let args: Vec<_> = command.get_args().map(OsString::from).collect();
      self.calls.borrow_mut().push(args.clone());
      let has = |value: &str| args.iter().any(|arg| arg == value);
      match command.get_program().to_str().unwrap() {
        "/usr/bin/ditto" => {
          if has("-c") {
            fs::write(args.last().unwrap(), b"fake ZIP")?;
            Ok(output(true, "", ""))
          } else {
            System.output(command)
          }
        }
        "/usr/bin/lipo" => Ok(output(true, "arm64\n", "")),
        "/usr/bin/codesign" => {
          if has("--entitlements") {
            ensure!(has("--xml"), "entitlement inspection must request XML output");
            Ok(output(
              true,
              r#"<?xml version="1.0"?><plist version="1.0"><dict><key>com.apple.security.cs.allow-jit</key><true/></dict></plist>"#,
              "",
            ))
          } else if has("--verbose=4") {
            Ok(output(
              true,
              "",
              "CodeDirectory v=20500 flags=0x10000(runtime)\nTeamIdentifier=EXAMPLE123\nTimestamp=Nov 11, 2026\n",
            ))
          } else {
            Ok(output(true, "", ""))
          }
        }
        "/usr/bin/xcrun" if has("notarytool") && has("submit") => Ok(output(
          !matches!(self.scenario, Scenario::UploadFailureAfterId),
          &response("In Progress"),
          "",
        )),
        "/usr/bin/xcrun" if has("notarytool") && has("wait") => match self.scenario {
          Scenario::Invalid => Ok(output(false, &response("Invalid"), "rejected")),
          Scenario::InvalidSuccessfulExit => Ok(output(true, &response("Invalid"), "")),
          Scenario::Timeout => Ok(output(false, "", "timed out waiting")),
          Scenario::InProgress => Ok(output(true, &response("In Progress"), "")),
          _ => Ok(output(true, &response("Accepted"), "")),
        },
        "/usr/bin/xcrun" if has("staple") => {
          if matches!(self.scenario, Scenario::StapleFailure) {
            Ok(output(false, "", "ticket unavailable"))
          } else {
            fs::write(Path::new(args.last().unwrap()).join("Contents/ticket"), b"attached")?;
            Ok(output(true, "", ""))
          }
        }
        "/usr/bin/xcrun" if has("validate") => Ok(output(
          !matches!(self.scenario, Scenario::TicketFailure),
          "",
          "invalid ticket",
        )),
        _ => bail!("unexpected test command: {command:?}"),
      }
    }
  }

  fn fixture(root: &Path) -> PathBuf {
    let app = root.join("Notary Fixture.app");
    fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    let info = plist::Dictionary::from_iter([
      (
        "CFBundleIdentifier".to_owned(),
        Value::String("dev.lintel.notary-fixture".to_owned()),
      ),
      ("CFBundleExecutable".to_owned(), Value::String("Fixture".to_owned())),
    ]);
    Value::Dictionary(info)
      .to_file_xml(app.join("Contents/Info.plist"))
      .unwrap();
    let mut header = [0u8; 32];
    header[..4].copy_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]);
    header[12..16].copy_from_slice(&2u32.to_le_bytes());
    fs::write(app.join("Contents/MacOS/Fixture"), header).unwrap();
    app
  }

  #[test]
  fn accepted_pipeline_attaches_validates_and_publishes_same_bundle_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let app = fixture(directory.path());
    let runner = Fake::new(Scenario::Accepted);
    let result = with_staged_copy(&app, true, &runner, |staged| notarize(staged, &options(), &runner)).unwrap();
    assert_eq!(result.status, "Accepted");
    assert_eq!(fs::read(app.join("Contents/ticket")).unwrap(), b"attached");
    let calls = runner.calls.borrow();
    assert!(
      calls
        .iter()
        .any(|args| args.iter().any(|arg| arg == DEVELOPER_ID_REQUIREMENT))
    );
    let upload = calls
      .iter()
      .find(|args| args.iter().any(|arg| arg == "submit"))
      .unwrap();
    assert!(upload.iter().any(|arg| arg == "--no-wait"));
    assert!(
      upload
        .windows(2)
        .any(|pair| pair == ["--keychain-profile", "fixture-profile"])
    );
  }

  #[test]
  fn rejected_timed_out_or_failed_ticket_operations_preserve_original_bundle_and_submission_id() {
    for scenario in [
      Scenario::UploadFailureAfterId,
      Scenario::Invalid,
      Scenario::InvalidSuccessfulExit,
      Scenario::Timeout,
      Scenario::InProgress,
      Scenario::StapleFailure,
      Scenario::TicketFailure,
    ] {
      let directory = tempfile::tempdir().unwrap();
      let app = fixture(directory.path());
      let original = fs::read(app.join("Contents/MacOS/Fixture")).unwrap();
      let runner = Fake::new(scenario);
      let error = with_staged_copy(&app, true, &runner, |staged| notarize(staged, &options(), &runner)).unwrap_err();
      assert!(format!("{error:#}").contains(ID));
      assert_eq!(fs::read(app.join("Contents/MacOS/Fixture")).unwrap(), original);
      assert!(!app.join("Contents/ticket").exists());
      assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
        !entry
          .unwrap()
          .file_name()
          .to_string_lossy()
          .starts_with(".lintel-notary-")
      }));
    }
  }

  #[test]
  fn submit_only_keeps_original_untouched_and_never_waits_or_staples() {
    let directory = tempfile::tempdir().unwrap();
    let app = fixture(directory.path());
    let runner = Fake::new(Scenario::Accepted);
    let result = with_staged_copy(&app, false, &runner, |staged| submit(staged, &credentials(), &runner)).unwrap();
    assert_eq!(result.id, ID);
    assert!(!app.join("Contents/ticket").exists());
    assert!(
      !runner
        .calls
        .borrow()
        .iter()
        .any(|args| args.iter().any(|arg| arg == "wait" || arg == "stapler"))
    );
  }
}
