//! Build and package desktop applications from a Cargo project.

use std::{collections::BTreeMap, path::PathBuf};

#[cfg(target_os = "macos")]
mod bundle_dependencies;
mod cargo;
mod link;
#[cfg(target_os = "macos")]
mod macho;
#[cfg(target_os = "macos")]
mod macos;
mod native_libraries;
#[cfg(target_os = "macos")]
mod notarization;
mod prepare;
mod project;
mod service_processes;
#[cfg(target_os = "macos")]
mod signing;
#[cfg(target_os = "macos")]
mod signing_components;
mod third_party;
mod windows;
mod windows_cef;
mod windows_distribution;
mod windows_protocol;
pub use windows_distribution::{
  WindowsDistributionInfo, WindowsDistributionLease, WindowsDistributionOptions, WindowsDistributionPlan,
  inspect_windows_distribution, lease_windows_distribution, plan_windows_distribution, stage_windows_distribution,
};
mod windows_signing;
pub use windows_signing::{
  AuthenticodeOptions, AuthenticodePlan, AuthenticodeReceipt, execute_windows_authenticode,
  plan_windows_authenticode_bundle, plan_windows_authenticode_file,
};

pub use cargo::{BuildArtifact, BuildOptions, BundleMetadata, TargetSelection, build, build_with_service_processes};
pub use link::configure_macos_cef;
pub use prepare::prepare_public_iced;
#[cfg(feature = "portable")]
pub use project::{EmbeddedStarter, create_portable_project};
pub use project::{ProjectTemplate, create_project};
pub use service_processes::{
  ServiceMetadata, ServiceProcess, build_service_processes, mojo_runtime_library, validate_service_name,
};
pub use third_party::{Library, LibraryKind, add_library, libraries, remove_library, update_library};
pub use windows::copy_windows_bundle_verified;
pub use windows::validate_windows_filename;
pub use windows::{
  WindowsBundleInfo, bundle_windows_with_extensions, copy_windows_bundle, validate_windows_bundle,
  validate_windows_tool,
};
pub use windows::{
  WindowsBundleUnsupported, add_windows_library, bundle_windows, bundle_windows_with_helpers,
  bundle_windows_with_libraries, remove_windows_library, update_windows_library, windows_libraries,
};
pub use windows_cef::{WindowsCefArchiveVerification, inspect_windows_cef_layout_with_archive};
pub use windows_cef::{WindowsCefAssessment, inspect_windows_cef_layout};
pub use windows_protocol::{WindowsProtocolRegistration, register_windows_protocols, unregister_windows_protocols};
pub use windows_protocol::{WindowsProtocolRemoval, unregister_matching_windows_protocols};

/// Inputs for assembling an application bundle from an existing executable.
pub struct BundleOptions {
  pub executable: PathBuf,
  pub name: String,
  pub identifier: String,
  pub version: String,
  pub output_dir: PathBuf,
  pub cef_dir: Option<PathBuf>,
  pub helper: Option<PathBuf>,
  pub dawn_root: Option<PathBuf>,
  /// Optional standalone update installer copied into Contents/Helpers.
  pub updater_helper: Option<PathBuf>,
}

/// An extra standalone process copied and verified before the enclosing app is signed.
/// `name` is a logical basename without `.exe`; Windows adds that suffix.
#[derive(Clone, Debug)]
pub struct BundleHelper {
  pub name: String,
  pub executable: PathBuf,
}

/// Additional process files and bundle-local protocol declarations.
#[derive(Clone, Debug, Default)]
pub struct BundleExtensions {
  pub helpers: Vec<BundleHelper>,
  pub protocol_schemes: Vec<String>,
  /// CEF's own local-only Crashpad configuration; mutually exclusive with native crash helpers.
  pub cef_crash_reports: bool,
}

pub const CEF_LOCAL_CRASH_REPORTER_CONFIG: &str = "[Config]\nProductName=Lintel\nBrowserCrashForwardingEnabled=false\nServerURL=\nRateLimitEnabled=true\nMaxUploadsPerDay=1\nMaxDatabaseSizeInMb=20\nMaxDatabaseAgeInDays=7\n[CrashKeys]\nlintel_app=medium\nlintel_instance=small\nlintel_browser=small\n";

fn validate_protocol_schemes(schemes: &[String]) -> anyhow::Result<()> {
  anyhow::ensure!(schemes.len() <= 64, "too many bundle protocol schemes");
  let mut unique = std::collections::BTreeSet::new();
  for scheme in schemes {
    anyhow::ensure!(
      !scheme.is_empty()
        && scheme.len() <= 128
        && scheme.as_bytes()[0].is_ascii_alphabetic()
        && scheme
          .bytes()
          .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')),
      "invalid bundle protocol scheme {scheme:?}"
    );
    anyhow::ensure!(
      unique.insert(scheme.to_ascii_lowercase()),
      "duplicate bundle protocol scheme {scheme:?}"
    );
  }
  Ok(())
}

pub(crate) fn validate_helpers(helpers: &[BundleHelper]) -> anyhow::Result<()> {
  let mut names = std::collections::BTreeSet::new();
  for helper in helpers {
    let name = &helper.name;
    anyhow::ensure!(
      !name.is_empty()
        && name.len() <= 128
        && !name.ends_with('.')
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
          .bytes()
          .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')),
      "invalid standalone helper name {name:?}"
    );
    anyhow::ensure!(
      names.insert(name.to_ascii_lowercase()),
      "duplicate standalone helper name {name:?}"
    );
  }
  Ok(())
}

/// Check the identity, version, architecture and signatures of a macOS update.
pub fn validate_macos_update(
  candidate: &std::path::Path,
  installed: &std::path::Path,
  id: &str,
  old_version: &str,
  new_version: &str,
) -> anyhow::Result<()> {
  #[cfg(target_os = "macos")]
  return macos::validate_update(candidate, installed, id, old_version, new_version);
  #[cfg(not(target_os = "macos"))]
  {
    let _ = (candidate, installed, id, old_version, new_version);
    anyhow::bail!("update installation currently requires macOS");
  }
}

/// Reuse the bundle publisher's lock and atomic swap; roll back if relaunch fails.
pub fn install_macos_update(
  candidate: &std::path::Path,
  installed: &std::path::Path,
  id: &str,
  old_version: &str,
  new_version: &str,
  relaunch: bool,
) -> anyhow::Result<()> {
  install_macos_update_guarded(candidate, installed, id, old_version, new_version, relaunch, || Ok(()))
}

/// Run the application's final idle check under the output lock immediately before swapping.
pub fn install_macos_update_guarded(
  candidate: &std::path::Path,
  installed: &std::path::Path,
  id: &str,
  old_version: &str,
  new_version: &str,
  relaunch: bool,
  before_publish: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
  #[cfg(target_os = "macos")]
  return macos::install_update(
    candidate,
    installed,
    id,
    old_version,
    new_version,
    relaunch,
    before_publish,
  );
  #[cfg(not(target_os = "macos"))]
  {
    let _ = (
      candidate,
      installed,
      id,
      old_version,
      new_version,
      relaunch,
      before_publish,
    );
    anyhow::bail!("update installation currently requires macOS");
  }
}

/// Explicit identity and executable entitlements for macOS code signing.
pub struct SignOptions {
  pub identity: String,
  pub entitlements: Option<PathBuf>,
  pub helper_entitlements: Option<PathBuf>,
  /// Explicit per-role CEF Helper permissions, overriding the shared Helper plist.
  pub helper_entitlements_overrides: BTreeMap<String, PathBuf>,
  pub hardened_runtime: bool,
  pub timestamp: bool,
}

/// CEF Helper roles emitted by Lintel's macOS bundle builder.
pub const CEF_HELPER_ROLES: [&str; 5] = ["default", "GPU", "Renderer", "Plugin", "Alerts"];

/// Reference to credentials already stored by Apple's notarytool in a keychain.
pub struct NotaryCredentials {
  pub keychain_profile: String,
  pub keychain: Option<PathBuf>,
}

/// Credentials and polling limit for a complete notarization workflow.
pub struct NotarizeOptions {
  pub credentials: NotaryCredentials,
  pub timeout_seconds: u64,
}

/// Apple's submission identifier and processing status.
#[derive(Debug, serde::Deserialize)]
pub struct NotarySubmission {
  pub id: String,
  pub status: String,
  pub message: Option<String>,
}

/// Upload a signed app snapshot without waiting or altering the original app.
pub fn submit_notarization(
  bundle: &std::path::Path,
  credentials: &NotaryCredentials,
) -> anyhow::Result<NotarySubmission> {
  #[cfg(target_os = "macos")]
  return notarization::submit_notarization(bundle, credentials);
  #[cfg(not(target_os = "macos"))]
  {
    let _ = (bundle, credentials);
    anyhow::bail!("Apple notarization requires macOS");
  }
}

/// Submit, wait for acceptance, staple and verify before replacing the app.
pub fn notarize_bundle(bundle: &std::path::Path, options: &NotarizeOptions) -> anyhow::Result<NotarySubmission> {
  #[cfg(target_os = "macos")]
  return notarization::notarize_bundle(bundle, options);
  #[cfg(not(target_os = "macos"))]
  {
    let _ = (bundle, options);
    anyhow::bail!("Apple notarization requires macOS");
  }
}

/// Read the current state of a previously uploaded submission.
pub fn notarization_info(id: &str, credentials: &NotaryCredentials) -> anyhow::Result<NotarySubmission> {
  #[cfg(target_os = "macos")]
  return notarization::notarization_info(id, credentials);
  #[cfg(not(target_os = "macos"))]
  {
    let _ = (id, credentials);
    anyhow::bail!("Apple notarization requires macOS");
  }
}

/// Wait for an existing submission and return success only for Accepted status.
pub fn wait_for_notarization(id: &str, options: &NotarizeOptions) -> anyhow::Result<NotarySubmission> {
  #[cfg(target_os = "macos")]
  return notarization::wait_for_notarization(id, options);
  #[cfg(not(target_os = "macos"))]
  {
    let _ = (id, options);
    anyhow::bail!("Apple notarization requires macOS");
  }
}

/// Retrieve Apple's JSON diagnostic log for a submission.
pub fn notarization_log(id: &str, credentials: &NotaryCredentials) -> anyhow::Result<String> {
  #[cfg(target_os = "macos")]
  return notarization::notarization_log(id, credentials);
  #[cfg(not(target_os = "macos"))]
  {
    let _ = (id, credentials);
    anyhow::bail!("Apple notarization requires macOS");
  }
}

/// Staple and verify the ticket on a copy, then atomically replace the app.
pub fn staple_bundle(bundle: &std::path::Path) -> anyhow::Result<PathBuf> {
  #[cfg(target_os = "macos")]
  return notarization::staple_bundle(bundle);
  #[cfg(not(target_os = "macos"))]
  {
    let _ = bundle;
    anyhow::bail!("Apple ticket stapling requires macOS");
  }
}

/// Verify the code signature and Apple's stapled notarization ticket.
pub fn verify_notarization(bundle: &std::path::Path) -> anyhow::Result<()> {
  #[cfg(target_os = "macos")]
  return notarization::verify_notarization(bundle);
  #[cfg(not(target_os = "macos"))]
  {
    let _ = bundle;
    anyhow::bail!("Apple notarization ticket validation requires macOS");
  }
}

/// Sign an existing app atomically after signing and verifying its nested code.
pub fn sign_bundle(bundle: &std::path::Path, options: &SignOptions) -> anyhow::Result<PathBuf> {
  #[cfg(target_os = "macos")]
  return signing::sign_bundle(bundle, options);
  #[cfg(not(target_os = "macos"))]
  {
    let _ = (bundle, options);
    anyhow::bail!("macOS applications must be signed on macOS");
  }
}

/// Verify the integrity of the app signature and its nested code.
pub fn verify_signature(bundle: &std::path::Path) -> anyhow::Result<()> {
  #[cfg(target_os = "macos")]
  return signing::verify_signature(bundle);
  #[cfg(not(target_os = "macos"))]
  {
    let _ = bundle;
    anyhow::bail!("macOS code signatures must be verified on macOS");
  }
}

/// Assemble and atomically publish an application bundle on macOS.
pub fn bundle_macos(options: &BundleOptions) -> anyhow::Result<PathBuf> {
  bundle_macos_with_libraries(options, &[])
}

/// Assemble an app bundle with the verified third-party inputs from its Cargo build.
pub fn bundle_macos_with_libraries(options: &BundleOptions, libraries: &[Library]) -> anyhow::Result<PathBuf> {
  bundle_macos_with_signing(options, libraries, None)
}

/// Build a validated bundle and optionally sign it before publishing.
pub fn bundle_macos_with_signing(
  options: &BundleOptions,
  libraries: &[Library],
  signing: Option<&SignOptions>,
) -> anyhow::Result<PathBuf> {
  bundle_macos_for_distribution(options, libraries, signing, None)
}

/// Assemble, optionally sign and notarize, and publish only a validated app.
pub fn bundle_macos_for_distribution(
  options: &BundleOptions,
  libraries: &[Library],
  signing: Option<&SignOptions>,
  notarization: Option<&NotarizeOptions>,
) -> anyhow::Result<PathBuf> {
  bundle_macos_for_distribution_with_helpers(options, libraries, signing, notarization, &[])
}

/// Assemble an app with additional process helpers, verifying them before signing/publication.
pub fn bundle_macos_for_distribution_with_helpers(
  options: &BundleOptions,
  libraries: &[Library],
  signing: Option<&SignOptions>,
  notarization: Option<&NotarizeOptions>,
  helpers: &[BundleHelper],
) -> anyhow::Result<PathBuf> {
  bundle_macos_for_distribution_with_extensions(
    options,
    libraries,
    signing,
    notarization,
    &BundleExtensions {
      helpers: helpers.to_vec(),
      protocol_schemes: vec![],
      cef_crash_reports: false,
    },
  )
}

/// Assemble with verified helpers and CFBundleURLTypes declarations before signing.
pub fn bundle_macos_for_distribution_with_extensions(
  options: &BundleOptions,
  libraries: &[Library],
  signing: Option<&SignOptions>,
  notarization: Option<&NotarizeOptions>,
  extensions: &BundleExtensions,
) -> anyhow::Result<PathBuf> {
  let helpers = &extensions.helpers;
  if extensions.cef_crash_reports {
    anyhow::ensure!(options.cef_dir.is_some(), "CEF crash reports require a CEF bundle");
    anyhow::ensure!(
      !helpers
        .iter()
        .any(|helper| helper.name.eq_ignore_ascii_case("lintel-crash-helper")),
      "CEF Crashpad and native crash helpers are mutually exclusive"
    );
  }
  validate_helpers(helpers)?;
  validate_protocol_schemes(&extensions.protocol_schemes)?;
  if notarization.is_some() {
    let signing = signing
      .ok_or_else(|| anyhow::anyhow!("notarization requires signing with a Developer ID Application identity"))?;
    anyhow::ensure!(
      !signing.identity.trim().is_empty(),
      "notarization requires an explicit Developer ID Application identity"
    );
    anyhow::ensure!(
      signing.identity.trim() != "-",
      "notarization requires a Developer ID Application identity, not ad-hoc signing"
    );
    anyhow::ensure!(
      signing.hardened_runtime && signing.timestamp,
      "notarization requires Hardened Runtime and secure timestamps"
    );
  }
  #[cfg(target_os = "macos")]
  if let Some(notarization) = notarization {
    notarization::validate_options(notarization)?;
  }
  #[cfg(target_os = "macos")]
  return macos::bundle(options, libraries, signing, notarization, extensions);

  #[cfg(not(target_os = "macos"))]
  {
    let _ = (options, libraries, signing, notarization, extensions);
    anyhow::bail!("macOS application bundles must be assembled on macOS");
  }
}

#[cfg(all(test, target_os = "macos"))]
mod distribution_tests {
  use super::*;

  #[test]
  fn mojo_relative_self_id_preserves_bytes_for_absolute_dlopen_and_never_allows_relative_dependencies() {
    use std::{fs, process::Command};
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("runtime.c");
    let image = root.path().join("libmojo_rs_native.dylib");
    fs::write(&source, "int mojo_fixture_answer(void) { return 42; }\n").unwrap();
    let compile_runtime = |install_name: &str| {
      assert!(
        Command::new("/usr/bin/clang")
          .arg("-dynamiclib")
          .arg(&source)
          .arg("-Wl,-install_name")
          .arg(format!("-Wl,{install_name}"))
          .arg("-o")
          .arg(&image)
          .status()
          .unwrap()
          .success()
      );
    };
    compile_runtime("./libmojo_rs_native.dylib");
    for name in crate::service_processes::MOJO_NOTICE_FILES {
      fs::write(root.path().join(name), format!("fixture upstream {name}\n")).unwrap();
    }
    let original = fs::read(&image).unwrap();
    let main_source = root.path().join("main.c");
    let main = root.path().join("main");
    fs::write(
      &main_source,
      "#include <dlfcn.h>\nint main(int argc, char **argv) {\n\
       if (argc != 2 || argv[1][0] != '/') return 2;\n\
       void *library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL); if (!library) return 3;\n\
       int (*answer)(void) = (int (*)(void))dlsym(library, \"mojo_fixture_answer\");\n\
       return answer && answer() == 42 ? 0 : 4; }\n",
    )
    .unwrap();
    assert!(
      Command::new("/usr/bin/clang")
        .arg(&main_source)
        .arg("-o")
        .arg(&main)
        .status()
        .unwrap()
        .success()
    );
    let mut options = BundleOptions {
      executable: main,
      name: "Mojo Runtime Fixture".into(),
      identifier: "dev.lintel.mojo-runtime-fixture".into(),
      version: "1.0.0".into(),
      output_dir: root.path().join("output"),
      cef_dir: None,
      helper: None,
      dawn_root: None,
      updater_helper: None,
    };
    let library = crate::mojo_runtime_library(&image, None).unwrap();
    let bundle = bundle_macos_for_distribution_with_extensions(
      &options,
      std::slice::from_ref(&library),
      None,
      None,
      &BundleExtensions::default(),
    )
    .unwrap();
    let installed = bundle.join("Contents/Frameworks/libmojo_rs_native.dylib");
    assert_eq!(fs::read(&image).unwrap(), original);
    assert_eq!(fs::read(&installed).unwrap(), original);
    assert!(
      Command::new(bundle.join("Contents/MacOS/Mojo Runtime Fixture"))
        .arg(&installed)
        .current_dir(root.path())
        .status()
        .unwrap()
        .success()
    );
    for name in crate::service_processes::MOJO_NOTICE_FILES {
      assert_eq!(
        fs::read(bundle.join("Contents/Resources/licenses/mojo-ipc").join(name)).unwrap(),
        fs::read(root.path().join(name)).unwrap()
      );
    }
    let mut ordinary = library.clone();
    ordinary.name = "ordinary".into();
    assert!(
      bundle_macos_for_distribution_with_extensions(&options, &[ordinary], None, None, &BundleExtensions::default())
        .is_err()
    );
    // Linking embeds the same relative string as LC_LOAD_DYLIB, which remains forbidden.
    let linked_source = root.path().join("linked.c");
    let linked = root.path().join("linked");
    fs::write(
      &linked_source,
      "extern int mojo_fixture_answer(void); int main(void) { return mojo_fixture_answer() == 42 ? 0 : 1; }\n",
    )
    .unwrap();
    assert!(
      Command::new("/usr/bin/clang")
        .arg(&linked_source)
        .arg("-L")
        .arg(root.path())
        .arg("-lmojo_rs_native")
        .arg("-o")
        .arg(&linked)
        .status()
        .unwrap()
        .success()
    );
    options.executable = linked;
    let error = bundle_macos_for_distribution_with_extensions(
      &options,
      std::slice::from_ref(&library),
      None,
      None,
      &BundleExtensions::default(),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("unsupported non-relocatable install name"));
    assert_eq!(fs::read(&installed).unwrap(), original);
    compile_runtime("./other-runtime.dylib");
    assert!(
      bundle_macos_for_distribution_with_extensions(&options, &[library], None, None, &BundleExtensions::default())
        .is_err()
    );
    assert_eq!(fs::read(&installed).unwrap(), original);
  }

  #[test]
  fn standalone_process_helpers_are_signed_and_bad_inputs_keep_the_existing_app() {
    use std::{fs, os::unix::fs::symlink, process::Command};

    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("main.c");
    let executable = root.path().join("main");
    fs::write(&source, "int main(void) { return 0; }\n").unwrap();
    assert!(
      Command::new("/usr/bin/clang")
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .status()
        .unwrap()
        .success()
    );
    let options = BundleOptions {
      executable: executable.clone(),
      name: "Standalone Helper".into(),
      identifier: "dev.lintel.standalone-helper".into(),
      version: "1.0.0".into(),
      output_dir: root.path().join("output"),
      cef_dir: None,
      helper: None,
      dawn_root: None,
      updater_helper: None,
    };
    let signing = SignOptions {
      identity: "-".into(),
      entitlements: None,
      helper_entitlements: None,
      helper_entitlements_overrides: BTreeMap::new(),
      hardened_runtime: false,
      timestamp: false,
    };
    let helpers = [BundleHelper {
      name: "lintel-crash-helper".into(),
      executable: executable.clone(),
    }];
    let extensions = BundleExtensions {
      helpers: helpers.to_vec(),
      protocol_schemes: vec!["Lintel-Fixture".into()],
      cef_crash_reports: false,
    };
    let bundle =
      bundle_macos_for_distribution_with_extensions(&options, &[], Some(&signing), None, &extensions).unwrap();
    let info = plist::Value::from_file(bundle.join("Contents/Info.plist")).unwrap();
    let url_type = &info.as_dictionary().unwrap()["CFBundleURLTypes"].as_array().unwrap()[0];
    assert_eq!(
      url_type.as_dictionary().unwrap()["CFBundleURLName"].as_string(),
      Some("dev.lintel.standalone-helper")
    );
    assert_eq!(
      url_type.as_dictionary().unwrap()["CFBundleURLSchemes"]
        .as_array()
        .unwrap()[0]
        .as_string(),
      Some("lintel-fixture")
    );
    let installed = bundle.join("Contents/Helpers/lintel-crash-helper");
    let original = fs::read(&installed).unwrap();
    assert!(
      crate::signing_components::components(&bundle)
        .unwrap()
        .iter()
        .any(
          |component| component.kind == crate::signing_components::ComponentKind::Executable
            && component.path.ends_with("Helpers/lintel-crash-helper")
        )
    );
    verify_signature(&bundle).unwrap();

    let linked = root.path().join("linked-helper");
    symlink(&executable, &linked).unwrap();
    assert!(
      bundle_macos_for_distribution_with_helpers(
        &options,
        &[],
        None,
        None,
        &[BundleHelper {
          name: "lintel-crash-helper".into(),
          executable: linked
        }]
      )
      .is_err()
    );
    assert_eq!(fs::read(&installed).unwrap(), original);

    let invalid = root.path().join("wrong-machine");
    let mut bytes = fs::read(&executable).unwrap();
    let current = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let other = if current == 0x0100000c {
      0x01000007_u32
    } else {
      0x0100000c
    };
    bytes[4..8].copy_from_slice(&other.to_le_bytes());
    fs::write(&invalid, bytes).unwrap();
    fs::set_permissions(&invalid, fs::metadata(&executable).unwrap().permissions()).unwrap();
    assert!(
      bundle_macos_for_distribution_with_helpers(
        &options,
        &[],
        None,
        None,
        &[BundleHelper {
          name: "lintel-crash-helper".into(),
          executable: invalid
        }]
      )
      .is_err()
    );
    assert_eq!(fs::read(&installed).unwrap(), original);
    verify_signature(&bundle).unwrap();
  }

  #[test]
  fn standalone_helper_unresolved_dependencies_are_rejected_before_publication() {
    use std::{fs, process::Command};

    let root = tempfile::tempdir().unwrap();
    let library_source = root.path().join("library.c");
    let helper_source = root.path().join("helper.c");
    let main_source = root.path().join("main.c");
    let library = root.path().join("libmissing-helper.dylib");
    let helper = root.path().join("helper");
    let executable = root.path().join("main");
    fs::write(&library_source, "int linked(void) { return 0; }\n").unwrap();
    fs::write(
      &helper_source,
      "extern int linked(void); int main(void) { return linked(); }\n",
    )
    .unwrap();
    fs::write(&main_source, "int main(void) { return 0; }\n").unwrap();
    assert!(
      Command::new("/usr/bin/clang")
        .arg("-dynamiclib")
        .arg(&library_source)
        .args(["-Wl,-install_name,@rpath/libmissing-helper.dylib", "-o"])
        .arg(&library)
        .status()
        .unwrap()
        .success()
    );
    assert!(
      Command::new("/usr/bin/clang")
        .arg(&helper_source)
        .arg("-L")
        .arg(root.path())
        .args(["-lmissing-helper", "-o"])
        .arg(&helper)
        .status()
        .unwrap()
        .success()
    );
    assert!(
      Command::new("/usr/bin/clang")
        .arg(&main_source)
        .arg("-o")
        .arg(&executable)
        .status()
        .unwrap()
        .success()
    );
    let options = BundleOptions {
      executable,
      name: "Helper Dependencies".into(),
      identifier: "dev.lintel.helper-dependencies".into(),
      version: "1.0.0".into(),
      output_dir: root.path().join("output"),
      cef_dir: None,
      helper: None,
      dawn_root: None,
      updater_helper: None,
    };
    let error = bundle_macos_for_distribution_with_helpers(
      &options,
      &[],
      None,
      None,
      &[BundleHelper {
        name: "lintel-crash-helper".into(),
        executable: helper,
      }],
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("missing required Mach-O dependency"));
    assert!(!options.output_dir.join("Helper Dependencies.app").exists());
  }

  #[test]
  fn final_install_guard_and_stale_version_keep_the_existing_bundle() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("main.c");
    let executable = root.path().join("main");
    std::fs::write(&source, "int main(void) { return 0; }\n").unwrap();
    assert!(
      std::process::Command::new("/usr/bin/clang")
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .status()
        .unwrap()
        .success()
    );
    let signing = SignOptions {
      identity: "-".into(),
      entitlements: None,
      helper_entitlements: None,
      helper_entitlements_overrides: BTreeMap::new(),
      hardened_runtime: false,
      timestamp: false,
    };
    let make = |version: &str, directory: &str| {
      bundle_macos_with_signing(
        &BundleOptions {
          executable: executable.clone(),
          name: "Update Guard".into(),
          identifier: "dev.lintel.guard".into(),
          version: version.into(),
          output_dir: root.path().join(directory),
          cef_dir: None,
          helper: None,
          dawn_root: None,
          updater_helper: None,
        },
        &[],
        Some(&signing),
      )
      .unwrap()
    };
    let installed = make("1.0.0", "live");
    let candidate = make("2.0.0", "candidate");
    let invoked = std::cell::Cell::new(false);
    let result = install_macos_update_guarded(
      &candidate,
      &installed,
      "dev.lintel.guard",
      "1.0.0",
      "2.0.0",
      false,
      || {
        invoked.set(true);
        anyhow::bail!("application reopened");
      },
    );
    assert!(result.is_err() && invoked.get());
    validate_macos_update(&candidate, &installed, "dev.lintel.guard", "1.0.0", "2.0.0").unwrap();
    install_macos_update_guarded(
      &candidate,
      &installed,
      "dev.lintel.guard",
      "1.0.0",
      "2.0.0",
      false,
      || Ok(()),
    )
    .unwrap();
    assert!(
      install_macos_update_guarded(
        &candidate,
        &installed,
        "dev.lintel.guard",
        "1.0.0",
        "2.0.0",
        false,
        || Ok(())
      )
      .is_err()
    );
    verify_signature(&installed).unwrap();
  }

  #[test]
  fn invalid_notary_options_are_rejected_before_building_or_signing() {
    let directory = tempfile::tempdir().unwrap();
    let options = BundleOptions {
      executable: directory.path().join("missing-executable"),
      name: "Notary Fixture".to_owned(),
      identifier: "dev.lintel.notary-fixture".to_owned(),
      version: "0.1.0".to_owned(),
      output_dir: directory.path().join("output"),
      cef_dir: None,
      helper: None,
      dawn_root: None,
      updater_helper: None,
    };
    let signing = SignOptions {
      identity: "Developer ID Application: Fixture (TEAMID)".to_owned(),
      entitlements: None,
      helper_entitlements: None,
      helper_entitlements_overrides: BTreeMap::new(),
      hardened_runtime: true,
      timestamp: true,
    };
    for (profile, timeout) in [("", 600), ("release", 0)] {
      let notarization = NotarizeOptions {
        credentials: NotaryCredentials {
          keychain_profile: profile.to_owned(),
          keychain: None,
        },
        timeout_seconds: timeout,
      };
      let error = bundle_macos_for_distribution(&options, &[], Some(&signing), Some(&notarization)).unwrap_err();
      let message = format!("{error:#}");
      assert!(
        message.contains(if profile.is_empty() { "profile" } else { "timeout" }),
        "{message}"
      );
      assert!(!options.output_dir.exists());
    }
  }

  #[test]
  fn invalid_or_duplicate_protocol_schemes_fail_before_filesystem_work() {
    let root = tempfile::tempdir().unwrap();
    let options = BundleOptions {
      executable: root.path().join("missing"),
      name: "Protocol Fixture".into(),
      identifier: "dev.lintel.protocol-fixture".into(),
      version: "1.0.0".into(),
      output_dir: root.path().join("output"),
      cef_dir: None,
      helper: None,
      dawn_root: None,
      updater_helper: None,
    };
    for schemes in [vec!["bad://".into()], vec!["lintel".into(), "LINTEL".into()]] {
      let extensions = BundleExtensions {
        protocol_schemes: schemes,
        ..BundleExtensions::default()
      };
      let error = bundle_macos_for_distribution_with_extensions(&options, &[], None, None, &extensions).unwrap_err();
      assert!(error.to_string().contains("protocol scheme"));
      assert!(!options.output_dir.exists());
    }
  }
}
