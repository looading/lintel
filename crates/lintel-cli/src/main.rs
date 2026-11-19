use std::{
  collections::BTreeMap,
  ffi::OsString,
  path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use lintel_build::{
  BuildArtifact, BuildOptions, BundleExtensions, BundleHelper, BundleOptions, CEF_HELPER_ROLES, EmbeddedStarter,
  LibraryKind, NotarizeOptions, ProjectTemplate, SignOptions, TargetSelection, add_library, add_windows_library,
  build_with_service_processes, bundle_macos_for_distribution_with_extensions, bundle_windows_with_extensions,
  create_portable_project, create_project, libraries, prepare_public_iced, remove_library, remove_windows_library,
  sign_bundle, staple_bundle, update_library, update_windows_library, verify_notarization, verify_signature,
  windows_libraries,
};

mod describe;
mod notarization;
mod processes;
mod protocol;
mod updater;
mod windows;

use notarization::{BundleNotarization, NotarizationArgs, NotarizeArgs};

const FRAMEWORK: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/lintel-framework.tar.gz"));
const NATIVE_MAIN: &str = include_str!(concat!(env!("OUT_DIR"), "/templates/native/src/main.rs"));
const MULTIPROCESS_MAIN: &str = include_str!(concat!(env!("OUT_DIR"), "/templates/multiprocess/src/main.rs"));
const WEBVIEW_MAIN: &str = include_str!(concat!(env!("OUT_DIR"), "/templates/webview/src/main.rs"));
const WEBVIEW_HTML: &str = include_str!(concat!(env!("OUT_DIR"), "/templates/webview/ui/index.html"));
const WEBVIEW_MULTIPROCESS_MAIN: &str =
  include_str!(concat!(env!("OUT_DIR"), "/templates/webview-multiprocess/src/main.rs"));
const WEBVIEW_MULTIPROCESS_HTML: &str = include_str!(concat!(
  env!("OUT_DIR"),
  "/templates/webview-multiprocess/ui/index.html"
));
const WINDOWS_WEBVIEW_LIB: &str = include_str!(concat!(env!("OUT_DIR"), "/templates/windows-webview/src/lib.rs"));
const WINDOWS_WEBVIEW_HTML: &str = include_str!(concat!(env!("OUT_DIR"), "/templates/windows-webview/ui/index.html"));

#[derive(Parser)]
#[command(name = "lintel", version, about)]
struct Cli {
  #[command(subcommand)]
  command: Command,
}

#[derive(Subcommand)]
enum Command {
  /// Describe the embedded framework catalog without probing the host or running tools.
  Describe(describe::DescribeArgs),
  /// Install a native Windows portable bundle for the current user.
  Windows(windows::WindowsArgs),
  /// Register or remove current-user Windows URL schemes using ownership receipts.
  Protocol(protocol::ProtocolArgs),
  /// Generate update signing keys and authenticated application releases.
  Updater(updater::UpdaterArgs),
  /// Create a standalone Lintel application from a starter.
  New(NewArgs),
  /// Prepare the pinned public iced source for the macOS Cargo profile.
  Prepare(PrepareArgs),
  /// Build the application and its declared service processes, then print the main executable path.
  ///
  /// Framework checkout builds using the public profile require `lintel prepare`.
  Build(BuildArgs),
  /// Create a macOS app or Windows MSVC portable bundle and print its path.
  ///
  /// Pass `--executable` to bundle an existing binary, or let Cargo build the application.
  Bundle(BundleArgs),
  /// Build, bundle, and run a native macOS or Windows application for development.
  ///
  /// Pass application arguments after `--`.
  Run(RunArgs),
  /// Manage the native libraries registered for an application.
  ThirdParty(ThirdPartyArgs),
  /// Sign an existing macOS application atomically.
  Sign(SignArgs),
  /// Verify an application's code signature and nested code.
  VerifySignature(AppPath),
  /// Submit a signed app to Apple, wait for acceptance, and staple its ticket atomically.
  Notarize(NotarizeArgs),
  /// Query, wait for, or retrieve the log of an existing Apple notarization submission.
  Notarization(NotarizationArgs),
  /// Staple Apple's notarization ticket to an app atomically.
  Staple(AppPath),
  /// Verify the app signature and validate its stapled Apple notarization ticket.
  VerifyNotarization(AppPath),
}

#[derive(Args)]
struct AppPath {
  /// macOS application bundle.
  bundle: PathBuf,
}

#[derive(Args)]
struct SignArgs {
  /// macOS application bundle.
  bundle: PathBuf,
  /// Signing identity name or SHA-1. Use - for a local ad-hoc signature.
  #[arg(long, allow_hyphen_values = true)]
  identity: String,
  #[command(flatten)]
  details: SignatureDetails,
}

#[derive(Args, Default, Clone)]
struct SignatureDetails {
  /// Entitlement plist for the main application process.
  #[arg(long)]
  entitlements: Option<PathBuf>,
  /// Entitlement plist for nested Helper application processes.
  #[arg(long)]
  helper_entitlements: Option<PathBuf>,
  /// Override one CEF Helper role: default, GPU, Renderer, Plugin, or Alerts. Repeat as needed.
  #[arg(long = "helper-entitlements-for", value_name = "ROLE=PLIST", value_parser = parse_helper_entitlements)]
  helper_entitlements_overrides: Vec<HelperEntitlementsOverride>,
  /// Enable Hardened Runtime for the main application and Helpers.
  #[arg(long)]
  hardened_runtime: bool,
  /// Request an Apple secure timestamp.
  #[arg(long)]
  timestamp: bool,
}

impl SignatureDetails {
  fn options(&self, identity: &str) -> Result<SignOptions> {
    let mut helper_entitlements_overrides = BTreeMap::new();
    for entry in &self.helper_entitlements_overrides {
      ensure!(
        helper_entitlements_overrides
          .insert(entry.role.clone(), entry.path.clone())
          .is_none(),
        "duplicate Helper entitlement override for role {}",
        entry.role
      );
    }
    Ok(SignOptions {
      identity: identity.to_owned(),
      entitlements: self.entitlements.clone(),
      helper_entitlements: self.helper_entitlements.clone(),
      helper_entitlements_overrides,
      hardened_runtime: self.hardened_runtime,
      timestamp: self.timestamp,
    })
  }

  fn is_set(&self) -> bool {
    self.entitlements.is_some()
      || self.helper_entitlements.is_some()
      || !self.helper_entitlements_overrides.is_empty()
      || self.hardened_runtime
      || self.timestamp
  }
}

#[derive(Clone)]
struct HelperEntitlementsOverride {
  role: String,
  path: PathBuf,
}

fn parse_helper_entitlements(value: &str) -> std::result::Result<HelperEntitlementsOverride, String> {
  let (role, path) = value.split_once('=').ok_or("expected ROLE=PLIST")?;
  if !CEF_HELPER_ROLES.contains(&role) {
    return Err(format!(
      "unknown CEF Helper role {role:?}; expected {}",
      CEF_HELPER_ROLES.join(", ")
    ));
  }
  if path.is_empty() {
    return Err("Helper entitlement plist path must not be empty".to_owned());
  }
  Ok(HelperEntitlementsOverride {
    role: role.to_owned(),
    path: PathBuf::from(path),
  })
}

fn parse_protocol_scheme(value: &str) -> std::result::Result<String, String> {
  if value.is_empty()
    || value.len() > 128
    || !value.as_bytes()[0].is_ascii_alphabetic()
    || !value
      .bytes()
      .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
  {
    return Err("expected a URL scheme name without ':' or '/'".to_owned());
  }
  Ok(value.to_ascii_lowercase())
}

#[derive(Args)]
struct ThirdPartyArgs {
  /// Cargo manifest for the application that owns the library registry.
  #[arg(long, global = true, default_value = "Cargo.toml")]
  manifest_path: PathBuf,

  /// Library platform; defaults to the host. Use windows to register DLLs on macOS.
  #[arg(long, global = true, value_enum)]
  platform: Option<LibraryPlatform>,

  #[command(subcommand)]
  command: ThirdPartyCommand,
}

#[derive(Clone, Copy, ValueEnum)]
enum LibraryPlatform {
  Macos,
  Windows,
}

#[derive(Subcommand)]
enum ThirdPartyCommand {
  /// Register a local static library, DLL, dylib, or framework.
  Add(LibraryArgs),
  /// Replace a registered library with a local artifact.
  Update(LibraryArgs),
  /// Remove a registered library.
  Remove {
    /// Name of the registered library.
    name: String,
  },
  /// Print the name, kind, link name, version, and path of each library.
  List,
}

#[derive(Args)]
struct LibraryArgs {
  /// Name to use in the application's library registry.
  name: String,

  /// Artifact to copy, relative to the current directory when not absolute.
  #[arg(long)]
  path: PathBuf,

  /// Version to record for the artifact.
  #[arg(long)]
  version: Option<String>,
}

impl ThirdPartyArgs {
  fn run(self) -> Result<()> {
    let windows = matches!(self.platform, Some(LibraryPlatform::Windows))
      || (self.platform.is_none() && cfg!(target_os = "windows"));
    ensure!(
      windows || cfg!(target_os = "macos"),
      "macOS native libraries require a macOS host"
    );
    let manifest = self
      .manifest_path
      .canonicalize()
      .with_context(|| format!("locate application manifest {}", self.manifest_path.display()))?;
    ensure!(
      manifest.is_file(),
      "application manifest {} is not a file",
      manifest.display()
    );
    let project = manifest
      .parent()
      .context("the application manifest has no parent directory")?;

    match self.command {
      ThirdPartyCommand::Add(args) => {
        let library = if windows {
          add_windows_library(project, &args.name, &args.path, args.version.as_deref())?
        } else {
          add_library(project, &args.name, &args.path, args.version.as_deref())?
        };
        println!("{}", library.path.display());
      }
      ThirdPartyCommand::Update(args) => {
        let library = if windows {
          update_windows_library(project, &args.name, &args.path, args.version.as_deref())?
        } else {
          update_library(project, &args.name, &args.path, args.version.as_deref())?
        };
        println!("{}", library.path.display());
      }
      ThirdPartyCommand::Remove { name } => {
        if windows {
          remove_windows_library(project, &name)?;
        } else {
          remove_library(project, &name)?;
        }
        println!("{name}");
      }
      ThirdPartyCommand::List => {
        let libraries = if windows {
          windows_libraries(project)?
        } else {
          libraries(project)?
        };
        for library in libraries {
          let kind = match library.kind {
            LibraryKind::Static => "static",
            LibraryKind::Dylib => "dylib",
            LibraryKind::Framework => "framework",
          };
          println!(
            "{}\t{}\t{}\t{}\t{}",
            library.name,
            kind,
            library.link_name,
            library.version.as_deref().unwrap_or("-"),
            library.path.display()
          );
        }
      }
    }
    Ok(())
  }
}

#[derive(Args)]
struct NewArgs {
  /// Application starter.
  #[arg(long, value_enum, default_value = "native")]
  template: TemplateArg,

  /// Use a local checkout instead of the embedded starter and framework sources.
  #[arg(long, value_name = "CHECKOUT")]
  lintel_path: Option<PathBuf>,

  /// New or empty directory for the application.
  directory: PathBuf,
}

#[derive(Args)]
struct PrepareArgs {
  /// Lintel checkout to prepare.
  #[arg(long, value_name = "CHECKOUT", default_value = ".")]
  lintel_path: PathBuf,
}

impl PrepareArgs {
  fn run(self) -> Result<PathBuf> {
    prepare_public_iced(&self.lintel_path)
  }
}

#[derive(Clone, Copy, ValueEnum)]
enum TemplateArg {
  Native,
  /// Native UI, shared Mojom protocol and managed worker; explicit Mojo runtime required to run.
  Multiprocess,
  #[value(alias = "mixed")]
  Webview,
  /// CEF WebView UI and a managed Mojo worker; explicit matching CEF/Mojo runtimes required.
  WebviewMultiprocess,
  /// Experimental Windows-only App.dll source; official CEF bootstrap and native validation required.
  WindowsWebview,
}

impl NewArgs {
  fn run(self) -> Result<PathBuf> {
    let (template, rust_source, html) = match self.template {
      TemplateArg::Native => (ProjectTemplate::Native, NATIVE_MAIN, None),
      TemplateArg::Multiprocess => (ProjectTemplate::Multiprocess, MULTIPROCESS_MAIN, None),
      TemplateArg::Webview => (ProjectTemplate::Webview, WEBVIEW_MAIN, Some(WEBVIEW_HTML)),
      TemplateArg::WebviewMultiprocess => (
        ProjectTemplate::WebviewMultiprocess,
        WEBVIEW_MULTIPROCESS_MAIN,
        Some(WEBVIEW_MULTIPROCESS_HTML),
      ),
      TemplateArg::WindowsWebview => (
        ProjectTemplate::WindowsWebview,
        WINDOWS_WEBVIEW_LIB,
        Some(WINDOWS_WEBVIEW_HTML),
      ),
    };
    if let Some(checkout) = &self.lintel_path {
      return create_project(&self.directory, template, Some(checkout));
    }
    let starter = EmbeddedStarter {
      rust_source,
      html,
      framework_archive: FRAMEWORK,
    };
    create_portable_project(&self.directory, template, &starter)
  }
}

#[derive(Args)]
struct BuildArgs {
  /// Cargo manifest for the application.
  #[arg(long, default_value = "Cargo.toml")]
  manifest_path: PathBuf,

  /// Package to build in a Cargo workspace.
  #[arg(long)]
  package: Option<String>,

  /// Binary target to build.
  #[arg(long, conflicts_with = "example")]
  bin: Option<String>,

  /// Example target to build.
  #[arg(long)]
  example: Option<String>,

  /// Rust target triple.
  #[arg(long)]
  target: Option<String>,

  /// Build with Cargo's release profile.
  #[arg(long)]
  release: bool,

  /// Require Cargo to use the existing lock file.
  #[arg(long)]
  locked: bool,

  /// Comma-separated Cargo features to enable.
  #[arg(long, value_delimiter = ',')]
  features: Vec<String>,

  /// Lintel Cargo profile with an alternate lock file (requires nightly Cargo).
  #[arg(long)]
  cargo_config: Option<PathBuf>,

  /// Directory containing a CEF runtime.
  #[arg(long)]
  cef_path: Option<PathBuf>,
}

impl BuildArgs {
  fn into_options(self) -> BuildOptions {
    let selection = self
      .bin
      .map(TargetSelection::Bin)
      .or_else(|| self.example.map(TargetSelection::Example))
      .unwrap_or(TargetSelection::Default);
    BuildOptions {
      manifest_path: self.manifest_path,
      package: self.package,
      selection,
      target: self.target,
      release: self.release,
      locked: self.locked,
      features: self.features,
      cargo_config: self.cargo_config,
      cef_path: self.cef_path,
    }
  }
}

#[derive(Args)]
struct BundleArgs {
  #[command(flatten)]
  build: BuildArgs,

  /// Bundle an existing executable without running Cargo.
  ///
  /// Includes the registered libraries in the current project.
  #[arg(
    long,
    requires = "identifier",
    conflicts_with_all = [
      "manifest_path", "package", "bin", "example", "release", "locked", "features", "cargo_config"
    ]
  )]
  executable: Option<PathBuf>,

  #[command(flatten)]
  bundle: BundleSettings,

  #[command(flatten)]
  notarization: BundleNotarization,
}

#[derive(Args)]
struct BundleSettings {
  #[command(flatten)]
  processes: processes::ProcessBundleArgs,
  /// Bundle name. Defaults to package metadata, the package name, or the executable stem.
  #[arg(long)]
  name: Option<String>,

  /// Bundle identifier. Defaults to package.metadata.lintel.id for a Cargo application.
  #[arg(long = "id")]
  identifier: Option<String>,

  /// Bundle version, defaulting to the package version or 0.1.0.
  #[arg(long)]
  bundle_version: Option<String>,

  /// Directory for the bundle. Defaults to build/<platform> or build/<platform>/run.
  #[arg(long)]
  output_dir: Option<PathBuf>,

  /// Separate executable for the CEF helper processes.
  #[arg(long)]
  helper: Option<PathBuf>,
  /// Include the macOS standalone updater installed beside lintel.
  #[arg(long, requires = "sign")]
  updater: bool,

  /// Copy this crash helper before dependency verification and signing.
  #[arg(long, conflicts_with = "crash_reporter")]
  crash_helper: Option<PathBuf>,

  /// Include the installed lintel-crash-helper executable beside lintel.
  #[arg(long, conflicts_with = "crash_helper")]
  crash_reporter: bool,

  /// Copy a native crash helper for explicit Mojo worker reporting; does not enable browser reporting.
  #[arg(long, value_name = "PATH")]
  worker_crash_helper: Option<PathBuf>,

  /// Declare URL schemes in bundle metadata; Windows HKCU registration is an explicit install step.
  #[arg(long = "protocol-scheme", value_name = "SCHEME", value_parser = parse_protocol_scheme)]
  protocol_schemes: Vec<String>,

  /// Add CEF's local-only Crashpad config; only valid for CEF bundles, never uploads.
  #[arg(long, conflicts_with_all = ["crash_helper", "crash_reporter"])]
  cef_crash_reports: bool,

  /// Dawn runtime directory for bundles that link Dawn dynamically.
  #[arg(long)]
  dawn_root: Option<PathBuf>,
  /// Sign the completed bundle before publishing. Use - for local ad-hoc signing.
  #[arg(long, value_name = "IDENTITY", allow_hyphen_values = true)]
  sign: Option<String>,
  #[command(flatten)]
  signature: SignatureDetails,
}

impl BundleSettings {
  fn bundle_extensions(&self) -> Result<BundleExtensions> {
    Ok(BundleExtensions {
      helpers: self.standalone_helpers()?,
      protocol_schemes: self.protocol_schemes.clone(),
      cef_crash_reports: self.cef_crash_reports,
    })
  }

  fn standalone_helpers(&self) -> Result<Vec<BundleHelper>> {
    let executable = match &self.crash_helper {
      Some(path) => Some(path.clone()),
      None => installed_crash_helper(self.crash_reporter)?,
    };
    let mut helpers: Vec<_> = executable
      .into_iter()
      .map(|executable| BundleHelper {
        name: "lintel-crash-helper".to_owned(),
        executable,
      })
      .collect();
    if let Some(executable) = &self.worker_crash_helper {
      helpers.push(BundleHelper {
        name: "lintel-worker-crash-helper".to_owned(),
        executable: executable.clone(),
      });
    }
    Ok(helpers)
  }

  fn signing_options(&self) -> Result<Option<SignOptions>> {
    if let Some(identity) = &self.sign {
      return Ok(Some(self.signature.options(identity)?));
    }
    ensure!(!self.signature.is_set(), "signature options require --sign <IDENTITY>");
    Ok(None)
  }

  fn distribution_signing_options(&self, notarize: bool) -> Result<Option<SignOptions>> {
    let mut signing = self.signing_options()?;
    if notarize {
      let options = signing
        .as_mut()
        .context("notarization requires --sign <Developer ID Application identity>")?;
      ensure!(
        options.identity.trim() != "-",
        "notarization requires a Developer ID Application identity, not ad-hoc signing"
      );
      options.hardened_runtime = true;
      options.timestamp = true;
    }
    Ok(signing)
  }

  fn bundle_built_application(
    self,
    mut artifact: BuildArtifact,
    service_helpers: Vec<BundleHelper>,
    default_output: PathBuf,
    notarization: Option<&NotarizeOptions>,
    platform: &BundlePlatform,
  ) -> Result<(PathBuf, String)> {
    let mut extensions = self.bundle_extensions()?;
    let has_services = self
      .processes
      .append_helpers(&mut extensions.helpers, service_helpers)?;
    let metadata = artifact.service_metadata()?;
    let target = match platform {
      BundlePlatform::Windows(target) => Some(target.as_str()),
      BundlePlatform::Macos => None,
    };
    self.processes.append_runtime(
      &mut artifact.third_party,
      &artifact.package_dir,
      metadata.mojo_runtime,
      target,
      has_services,
    )?;
    let signing = self.distribution_signing_options(notarization.is_some())?;
    let (metadata_id, metadata_name) = if self.identifier.is_none() || self.name.is_none() {
      let metadata = artifact.bundle_metadata()?;
      (metadata.id, metadata.name)
    } else {
      (None, None)
    };
    let identifier = self
      .identifier
      .or(metadata_id)
      .context("provide --id or set package.metadata.lintel.id in the selected package's Cargo.toml")?;
    let options = BundleOptions {
      executable: artifact.executable,
      name: self.name.or(metadata_name).unwrap_or(artifact.package_name),
      identifier,
      version: self.bundle_version.unwrap_or(artifact.version),
      output_dir: self.output_dir.unwrap_or(default_output),
      cef_dir: artifact.cef_dir,
      helper: self.helper,
      dawn_root: self.dawn_root,
      updater_helper: installed_updater(self.updater)?,
    };
    let bundle = assemble_bundle(
      &options,
      &artifact.third_party,
      signing.as_ref(),
      notarization,
      platform,
      &extensions,
    )?;
    Ok((bundle, options.name))
  }
}

enum BundlePlatform {
  Macos,
  Windows(String),
}

impl BundlePlatform {
  fn resolve(target: Option<&str>) -> Result<Self> {
    let configured = std::env::var("CARGO_BUILD_TARGET").ok();
    let target = target.or(configured.as_deref());
    match target {
      Some(target @ ("i686-pc-windows-msvc" | "x86_64-pc-windows-msvc" | "aarch64-pc-windows-msvc")) => {
        Ok(Self::Windows(target.to_owned()))
      }
      Some(target) if target.contains("-windows-") => {
        Err(lintel_build::WindowsBundleUnsupported::Target(target.to_owned()).into())
      }
      Some(target) if target.ends_with("-apple-darwin") && cfg!(target_os = "macos") => Ok(Self::Macos),
      Some(target) => anyhow::bail!("unsupported bundle target {target:?} on this host"),
      None if cfg!(target_os = "macos") => Ok(Self::Macos),
      None if cfg!(target_os = "windows") => {
        let architecture = if std::env::consts::ARCH == "x86" {
          "i686"
        } else {
          std::env::consts::ARCH
        };
        Ok(Self::Windows(format!("{architecture}-pc-windows-msvc")))
      }
      None => anyhow::bail!("specify a supported Windows --target, or bundle macOS apps on macOS"),
    }
  }

  fn directory(&self) -> &'static str {
    match self {
      Self::Macos => "macos",
      Self::Windows(_) => "windows",
    }
  }
}

fn validate_windows_options(
  platform: &BundlePlatform,
  bundle: &BundleSettings,
  signing: Option<&SignOptions>,
  notarization: Option<&NotarizeOptions>,
) -> Result<()> {
  if matches!(platform, BundlePlatform::Windows(_)) {
    ensure!(
      signing.is_none() && notarization.is_none(),
      "Apple signing/notarization options cannot be used for a Windows portable bundle"
    );
    if bundle.updater {
      return Err(lintel_build::WindowsBundleUnsupported::Updater.into());
    }
    if bundle.helper.is_some() || bundle.dawn_root.is_some() || bundle.cef_crash_reports {
      return Err(lintel_build::WindowsBundleUnsupported::Cef.into());
    }
  }
  Ok(())
}

fn assemble_bundle(
  options: &BundleOptions,
  libraries: &[lintel_build::Library],
  signing: Option<&SignOptions>,
  notarization: Option<&NotarizeOptions>,
  platform: &BundlePlatform,
  extensions: &BundleExtensions,
) -> Result<PathBuf> {
  match platform {
    BundlePlatform::Macos => {
      bundle_macos_for_distribution_with_extensions(options, libraries, signing, notarization, extensions)
    }
    BundlePlatform::Windows(target) => {
      ensure!(
        signing.is_none() && notarization.is_none(),
        "Apple signing/notarization cannot be used for Windows"
      );
      bundle_windows_with_extensions(options, target, libraries, extensions)
    }
  }
}

impl BundleArgs {
  fn run(self) -> Result<PathBuf> {
    let platform = BundlePlatform::resolve(self.build.target.as_deref())?;
    let notarization = self.notarization.options()?;
    let signing = self.bundle.distribution_signing_options(notarization.is_some())?;
    validate_windows_options(&platform, &self.bundle, signing.as_ref(), notarization.as_ref())?;
    let mut extensions = self.bundle.bundle_extensions()?;
    let default_output = self
      .build
      .manifest_path
      .parent()
      .unwrap_or_else(|| Path::new("."))
      .join(format!("build/{}", platform.directory()));
    let Some(executable) = self.executable else {
      let build_options = self.build.into_options();
      let (artifact, service_helpers) = build_with_service_processes(&build_options)?;
      let (bundle, _) = self.bundle.bundle_built_application(
        artifact,
        service_helpers,
        default_output,
        notarization.as_ref(),
        &platform,
      )?;
      return Ok(bundle);
    };

    let name = self
      .bundle
      .name
      .or_else(|| executable.file_stem()?.to_str().map(str::to_owned))
      .context("provide --name when the executable has no UTF-8 file stem")?;
    let identifier = self.bundle.identifier.context("provide --id when using --executable")?;
    let mut third_party = if Path::new("Cargo.toml").is_file() {
      match &platform {
        BundlePlatform::Macos => libraries(Path::new("."))?,
        BundlePlatform::Windows(_) => windows_libraries(Path::new("."))?,
      }
    } else {
      Vec::new()
    };
    let has_services = self
      .bundle
      .processes
      .append_helpers(&mut extensions.helpers, Vec::new())?;
    let target = match &platform {
      BundlePlatform::Windows(target) => Some(target.as_str()),
      BundlePlatform::Macos => None,
    };
    self
      .bundle
      .processes
      .append_runtime(&mut third_party, Path::new("."), None, target, has_services)?;
    assemble_bundle(
      &BundleOptions {
        executable,
        name,
        identifier,
        version: self.bundle.bundle_version.unwrap_or_else(|| "0.1.0".to_owned()),
        output_dir: self.bundle.output_dir.unwrap_or(default_output),
        cef_dir: self.build.cef_path,
        helper: self.bundle.helper,
        dawn_root: self.bundle.dawn_root,
        updater_helper: installed_updater(self.bundle.updater)?,
      },
      &third_party,
      signing.as_ref(),
      notarization.as_ref(),
      &platform,
      &extensions,
    )
  }
}

#[derive(Args)]
struct RunArgs {
  #[command(flatten)]
  build: BuildArgs,

  #[command(flatten)]
  bundle: BundleSettings,

  /// Arguments to pass to the bundled application.
  #[arg(last = true, value_name = "ARGS")]
  arguments: Vec<OsString>,
}

impl RunArgs {
  fn run(self) -> Result<()> {
    let platform = BundlePlatform::resolve(self.build.target.as_deref())?;
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
      let _ = (self.build, self.bundle, self.arguments);
      anyhow::bail!("macOS application bundles must be run on macOS");
    }

    #[cfg(target_os = "macos")]
    {
      use std::os::unix::process::CommandExt;

      ensure!(
        matches!(platform, BundlePlatform::Macos),
        "a Windows portable bundle must be run on Windows"
      );
      self.bundle.signing_options()?;
      let build_options = self.build.into_options();
      let (artifact, service_helpers) = build_with_service_processes(&build_options)?;
      let package_dir = artifact.package_dir.clone();
      let (bundle, name) = self.bundle.bundle_built_application(
        artifact,
        service_helpers,
        package_dir.join("build/macos/run"),
        None,
        &BundlePlatform::Macos,
      )?;
      let executable = bundle.join("Contents/MacOS").join(name);
      let mut command = std::process::Command::new(&executable);
      command.current_dir(package_dir).args(self.arguments);
      for variable in [
        "CEF_PATH",
        "DAWN_ROOT",
        "DYLD_LIBRARY_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "DYLD_FALLBACK_FRAMEWORK_PATH",
        "DYLD_INSERT_LIBRARIES",
      ] {
        command.env_remove(variable);
      }
      eprintln!("Running {}", executable.display());
      Err(command.exec()).with_context(|| format!("launch bundled application {}", executable.display()))
    }

    #[cfg(target_os = "windows")]
    {
      let signing = self.bundle.signing_options()?;
      validate_windows_options(&platform, &self.bundle, signing.as_ref(), None)?;
      let build_options = self.build.into_options();
      let (artifact, service_helpers) = build_with_service_processes(&build_options)?;
      let package_dir = artifact.package_dir.clone();
      let (bundle, name) = self.bundle.bundle_built_application(
        artifact,
        service_helpers,
        package_dir.join("build/windows/run"),
        None,
        &platform,
      )?;
      let executable = bundle.join(format!("{name}.exe"));
      let status = std::process::Command::new(&executable)
        .current_dir(package_dir)
        .args(self.arguments)
        .status()
        .with_context(|| format!("launch bundled application {}", executable.display()))?;
      ensure!(status.success(), "application exited with {status}");
      Ok(())
    }
  }
}

fn main() -> Result<()> {
  let artifact = match Cli::parse().command {
    Command::Describe(args) => return args.run(),
    Command::Windows(args) => return args.run(),
    Command::Protocol(args) => return args.run(),
    Command::Updater(args) => return args.run(),
    Command::New(args) => args.run()?,
    Command::Prepare(args) => args.run()?,
    Command::Build(args) => {
      let options = args.into_options();
      let (artifact, _) = build_with_service_processes(&options)?;
      artifact.executable
    }
    Command::Bundle(args) => args.run()?,
    Command::Run(args) => return args.run(),
    Command::ThirdParty(args) => return args.run(),
    Command::Sign(args) => sign_bundle(&args.bundle, &args.details.options(&args.identity)?)?,
    Command::Notarize(args) => return args.run(),
    Command::Notarization(args) => return args.run(),
    Command::Staple(args) => staple_bundle(&args.bundle)?,
    Command::VerifyNotarization(args) => {
      verify_notarization(&args.bundle)?;
      args.bundle.canonicalize().context("locate the notarized application")?
    }
    Command::VerifySignature(args) => {
      verify_signature(&args.bundle)?;
      args.bundle.canonicalize().context("locate the verified application")?
    }
  };

  println!("{}", artifact.display());
  Ok(())
}

fn installed_updater(enabled: bool) -> Result<Option<PathBuf>> {
  if !enabled {
    return Ok(None);
  }
  let executable = std::env::current_exe()?;
  let helper = executable
    .parent()
    .context("lintel has no installation directory")?
    .join("lintel-updater");
  ensure!(
    helper.is_file(),
    "lintel-updater is not installed beside lintel; install lintel-cli with both binaries"
  );
  Ok(Some(helper))
}

fn installed_crash_helper(enabled: bool) -> Result<Option<PathBuf>> {
  if !enabled {
    return Ok(None);
  }
  let executable = std::env::current_exe()?;
  let helper = executable
    .parent()
    .context("lintel has no installation directory")?
    .join(format!("lintel-crash-helper{}", std::env::consts::EXE_SUFFIX));
  ensure!(
    helper.is_file(),
    "lintel-crash-helper is not installed beside lintel; install lintel-cli with all binaries or provide --crash-helper PATH"
  );
  Ok(Some(helper))
}

#[cfg(test)]
mod signature_tests {
  use super::*;

  #[test]
  fn multiprocess_flags_parse_for_run_and_prebuilt_bundle_without_implicit_runtime() {
    let cli = Cli::try_parse_from([
      "lintel",
      "run",
      "--mojo-runtime",
      "native/libmojo_rs_native.dylib",
      "--service-process",
      "echo-worker=build/worker",
      "--",
      "--smoke",
    ])
    .unwrap();
    let Command::Run(args) = cli.command else {
      panic!("expected run")
    };
    assert_eq!(
      args.bundle.processes.mojo_runtime,
      Some(PathBuf::from("native/libmojo_rs_native.dylib"))
    );
    assert_eq!(args.bundle.processes.services[0].name, "echo-worker");
    assert_eq!(args.arguments, vec![OsString::from("--smoke")]);
    assert!(
      Cli::try_parse_from([
        "lintel",
        "bundle",
        "--executable",
        "main",
        "--id",
        "dev.example.app",
        "--mojo-runtime",
        "native/libmojo_rs_native.dylib",
        "--service-process",
        "worker=bin/worker"
      ])
      .is_ok()
    );
    assert!(Cli::try_parse_from(["lintel", "new", "--template", "multiprocess", "service-app"]).is_ok());
    let Command::New(mixed) =
      Cli::try_parse_from(["lintel", "new", "--template", "webview-multiprocess", "web-services"])
        .unwrap()
        .command
    else {
      panic!("expected new");
    };
    assert!(matches!(mixed.template, TemplateArg::WebviewMultiprocess));
    for value in ["../worker=bin/worker", "lintel-updater=bin/worker", "worker="] {
      assert!(Cli::try_parse_from(["lintel", "run", "--service-process", value]).is_err());
    }
  }

  #[test]
  fn prebuilt_bundle_keeps_cargo_preflight_out_of_the_explicit_executable_path() {
    let root = std::env::temp_dir().join(format!(
      "lintel-prebuilt-no-cargo-{}-{}",
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ));
    let missing_manifest = root.join("missing-Cargo.toml");
    let missing_config = root.join("missing-cargo-config.toml");
    let cli = Cli::try_parse_from([
      OsString::from("lintel"),
      OsString::from("bundle"),
      OsString::from("--executable"),
      root.join("prebuilt-main").into_os_string(),
      OsString::from("--id"),
      OsString::from("dev.example.prebuilt"),
      OsString::from("--target"),
      OsString::from("x86_64-pc-windows-msvc"),
      OsString::from("--service-process"),
      OsString::from("worker=prebuilt-worker"),
    ])
    .unwrap();
    let Command::Bundle(mut args) = cli.command else {
      panic!("expected bundle");
    };
    // 解析保留 --executable 与 Cargo 选项互斥的公开契约；直接检查分支不会读取内部构建字段。
    args.build.manifest_path = missing_manifest;
    args.build.cargo_config = Some(missing_config);
    let error = args.run().unwrap_err();
    assert!(
      format!("{error:#}").contains("service processes require --mojo-runtime"),
      "{error:#}"
    );
  }

  #[test]
  fn describe_is_an_explicit_read_only_command_with_an_optional_json_flag() {
    let Command::Describe(human) = Cli::try_parse_from(["lintel", "describe"]).unwrap().command else {
      panic!("expected describe");
    };
    assert!(!human.json);
    let Command::Describe(machine) = Cli::try_parse_from(["lintel", "describe", "--json"]).unwrap().command else {
      panic!("expected describe");
    };
    assert!(machine.json);
    assert!(Cli::try_parse_from(["lintel", "describe", "--json", "unexpected-project-path"]).is_err());
  }

  #[test]
  fn windows_cef_starter_requires_explicit_template_and_preserves_existing_defaults() {
    for (value, expected) in [
      (None, "native"),
      (Some("webview"), "webview"),
      (Some("windows-webview"), "windows"),
    ] {
      let mut arguments = vec!["lintel", "new", "example-app"];
      if let Some(value) = value {
        arguments.extend(["--template", value]);
      }
      let Command::New(args) = Cli::try_parse_from(arguments).unwrap().command else {
        panic!("expected new");
      };
      assert!(match expected {
        "native" => matches!(args.template, TemplateArg::Native),
        "webview" => matches!(args.template, TemplateArg::Webview),
        _ => matches!(args.template, TemplateArg::WindowsWebview),
      });
    }
  }

  #[test]
  fn helper_entitlement_overrides_parse_exact_roles_and_preserve_paths() {
    let cli = Cli::try_parse_from([
      "lintel",
      "bundle",
      "--sign",
      "-",
      "--helper-entitlements",
      "fallback.plist",
      "--helper-entitlements-for",
      "Renderer=renderer.plist",
      "--helper-entitlements-for",
      "GPU=directory/gpu=permissions.plist",
    ])
    .unwrap();
    let Command::Bundle(args) = cli.command else {
      panic!("expected bundle");
    };
    let options = args.bundle.signing_options().unwrap().unwrap();
    assert_eq!(options.helper_entitlements, Some(PathBuf::from("fallback.plist")));
    assert_eq!(
      options.helper_entitlements_overrides["Renderer"],
      Path::new("renderer.plist")
    );
    assert_eq!(
      options.helper_entitlements_overrides["GPU"],
      Path::new("directory/gpu=permissions.plist")
    );
    for value in ["renderer=fixture.plist", "Unknown=fixture.plist", "GPU=", "GPU"] {
      assert!(
        Cli::try_parse_from([
          "lintel",
          "sign",
          "Fixture.app",
          "--identity",
          "-",
          "--helper-entitlements-for",
          value
        ])
        .is_err()
      );
    }
  }

  #[test]
  fn duplicate_or_unsigned_helper_overrides_are_rejected_before_building() {
    let cli = Cli::try_parse_from([
      "lintel",
      "bundle",
      "--sign",
      "-",
      "--helper-entitlements-for",
      "Renderer=one.plist",
      "--helper-entitlements-for",
      "Renderer=two.plist",
    ])
    .unwrap();
    let Command::Bundle(args) = cli.command else {
      panic!("expected bundle");
    };
    assert!(
      args
        .bundle
        .signing_options()
        .err()
        .unwrap()
        .to_string()
        .contains("duplicate")
    );
    let cli = Cli::try_parse_from(["lintel", "bundle", "--helper-entitlements-for", "GPU=gpu.plist"]).unwrap();
    let Command::Bundle(args) = cli.command else {
      panic!("expected bundle");
    };
    assert!(args.bundle.signing_options().is_err());
  }

  #[test]
  fn windows_executable_bundles_allow_explicit_targets_and_reject_apple_signing_before_io() {
    let cli = Cli::try_parse_from([
      "lintel",
      "bundle",
      "--executable",
      "not-built.exe",
      "--target",
      "x86_64-pc-windows-msvc",
      "--id",
      "dev.example.fixture",
      "--sign",
      "-",
    ])
    .unwrap();
    let Command::Bundle(args) = cli.command else {
      panic!("expected bundle");
    };
    assert_eq!(args.build.target.as_deref(), Some("x86_64-pc-windows-msvc"));
    assert!(args.run().unwrap_err().to_string().contains("Apple signing"));
    assert!(matches!(
      BundlePlatform::resolve(Some("i686-pc-windows-msvc")).unwrap(),
      BundlePlatform::Windows(_)
    ));
    assert!(BundlePlatform::resolve(Some("x86_64-pc-windows-gnu")).is_err());
  }

  #[test]
  fn windows_library_registry_is_explicitly_selectable_on_any_host() {
    let cli = Cli::try_parse_from([
      "lintel",
      "third-party",
      "--platform",
      "windows",
      "add",
      "native",
      "--path",
      "vendor/native.dll",
      "--version",
      "1.2.0",
    ])
    .unwrap();
    let Command::ThirdParty(args) = cli.command else {
      panic!("expected third-party");
    };
    assert!(matches!(args.platform, Some(LibraryPlatform::Windows)));
    let ThirdPartyCommand::Add(args) = args.command else {
      panic!("expected add");
    };
    assert_eq!(args.path, Path::new("vendor/native.dll"));
    assert_eq!(args.version.as_deref(), Some("1.2.0"));
  }

  #[test]
  fn crash_helper_flags_preserve_explicit_paths_and_reject_ambiguous_selection() {
    let cli = Cli::try_parse_from(["lintel", "bundle", "--crash-helper", "helpers/lintel-crash-helper"]).unwrap();
    let Command::Bundle(args) = cli.command else {
      panic!("expected bundle");
    };
    let helpers = args.bundle.standalone_helpers().unwrap();
    assert_eq!(helpers.len(), 1);
    assert_eq!(helpers[0].name, "lintel-crash-helper");
    assert_eq!(helpers[0].executable, Path::new("helpers/lintel-crash-helper"));
    assert!(Cli::try_parse_from(["lintel", "bundle", "--crash-helper", "helper", "--crash-reporter"]).is_err());
  }

  #[test]
  fn worker_crash_helper_is_an_explicit_separate_role_compatible_with_cef_crashpad() {
    let cli = Cli::try_parse_from([
      "lintel",
      "bundle",
      "--cef-crash-reports",
      "--worker-crash-helper",
      "helpers/native-dump-helper",
    ])
    .unwrap();
    let Command::Bundle(args) = cli.command else {
      panic!("expected bundle");
    };
    let extensions = args.bundle.bundle_extensions().unwrap();
    assert!(extensions.cef_crash_reports);
    assert_eq!(extensions.helpers.len(), 1);
    assert_eq!(extensions.helpers[0].name, "lintel-worker-crash-helper");
    assert_eq!(
      extensions.helpers[0].executable,
      Path::new("helpers/native-dump-helper")
    );
    assert!(!args.bundle.crash_reporter && args.bundle.crash_helper.is_none());
    for command in ["run", "bundle"] {
      assert!(Cli::try_parse_from(["lintel", command, "--worker-crash-helper", "helper"]).is_ok());
    }
    assert!(Cli::try_parse_from(["lintel", "bundle", "--worker-crash-helper"]).is_err());
    assert!(
      processes::parse_service("lintel-worker-crash-helper=worker").is_err(),
      "a business service cannot replace the reserved worker crash helper"
    );
  }

  #[test]
  fn worker_and_browser_native_helpers_keep_distinct_names_and_reject_service_replacement() {
    let cli = Cli::try_parse_from([
      "lintel",
      "bundle",
      "--crash-helper",
      "browser-helper",
      "--worker-crash-helper",
      "worker-helper",
    ])
    .unwrap();
    let Command::Bundle(args) = cli.command else {
      panic!("expected bundle");
    };
    let mut helpers = args.bundle.standalone_helpers().unwrap();
    assert_eq!(
      helpers.iter().map(|helper| helper.name.as_str()).collect::<Vec<_>>(),
      ["lintel-crash-helper", "lintel-worker-crash-helper"]
    );
    let duplicate = vec![BundleHelper {
      name: "LINTEL-WORKER-CRASH-HELPER".into(),
      executable: "collision".into(),
    }];
    let error = args
      .bundle
      .processes
      .append_helpers(&mut helpers, duplicate)
      .unwrap_err();
    assert!(format!("{error:#}").contains("reserved by the framework"));
  }

  #[test]
  fn protocol_schemes_are_repeatable_normalized_and_windows_bundle_declarations_do_not_register() {
    let cli = Cli::try_parse_from([
      "lintel",
      "bundle",
      "--protocol-scheme",
      "LINTEL-ACTIVATION",
      "--protocol-scheme",
      "second+app",
    ])
    .unwrap();
    let Command::Bundle(args) = cli.command else {
      panic!("expected bundle");
    };
    assert_eq!(args.bundle.protocol_schemes, ["lintel-activation", "second+app"]);
    for invalid in ["", "1bad", "bad://", "has space", "a/b"] {
      assert!(Cli::try_parse_from(["lintel", "bundle", "--protocol-scheme", invalid]).is_err());
    }
    let cli = Cli::try_parse_from([
      "lintel",
      "bundle",
      "--executable",
      "missing.exe",
      "--id",
      "dev.fixture.app",
      "--target",
      "x86_64-pc-windows-msvc",
      "--protocol-scheme",
      "lintel",
    ])
    .unwrap();
    let Command::Bundle(args) = cli.command else {
      panic!("expected bundle");
    };
    assert_eq!(args.bundle.bundle_extensions().unwrap().protocol_schemes, ["lintel"]);
  }

  #[test]
  fn cef_crash_config_is_explicit_and_mutually_exclusive_with_native_helpers() {
    let cli = Cli::try_parse_from(["lintel", "bundle", "--cef-crash-reports"]).unwrap();
    let Command::Bundle(args) = cli.command else {
      panic!("expected bundle");
    };
    assert!(args.bundle.bundle_extensions().unwrap().cef_crash_reports);
    assert!(Cli::try_parse_from(["lintel", "bundle", "--cef-crash-reports", "--crash-reporter"]).is_err());
    assert!(Cli::try_parse_from(["lintel", "bundle", "--cef-crash-reports", "--crash-helper", "helper"]).is_err());
  }
}
