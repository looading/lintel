//! 只解析应用显式传入的目录；不创建文件、启动 helper 或加载运行库。

use std::{ffi::OsString, path::PathBuf};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct CrashOptions {
  pub service_directory: Option<PathBuf>,
  pub cef_directory: Option<PathBuf>,
}

pub fn parse(
  arguments: impl IntoIterator<Item = OsString>,
  service_enabled: bool,
  cef_enabled: bool,
) -> Result<CrashOptions, String> {
  let mut options = CrashOptions::default();
  let mut arguments = arguments.into_iter();
  while let Some(argument) = arguments.next() {
    let (name, directory) = if argument == "--service-crash-directory" {
      ("--service-crash-directory", &mut options.service_directory)
    } else if argument == "--cef-crash-directory" {
      ("--cef-crash-directory", &mut options.cef_directory)
    } else {
      continue;
    };
    if directory.is_some() {
      return Err(format!("{name} can only be supplied once"));
    }
    let value = arguments
      .next()
      .filter(|value| !value.is_empty() && !value.to_string_lossy().starts_with("--"))
      .ok_or_else(|| format!("{name} requires an absolute path"))?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
      return Err(format!("{name} requires an absolute path"));
    }
    *directory = Some(path);
  }
  if options.service_directory.is_some() && !service_enabled {
    return Err("--service-crash-directory requires building with --features service-crash-reports".into());
  }
  if options.cef_directory.is_some() && !cef_enabled {
    return Err("--cef-crash-directory requires the WebView application".into());
  }
  Ok(options)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn arguments(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
  }

  fn absolute(label: &str) -> PathBuf {
    std::env::temp_dir().join(label)
  }

  #[test]
  fn no_directory_preserves_the_default_for_all_feature_and_process_roles() {
    for service in [false, true] {
      for cef in [false, true] {
        assert_eq!(
          parse(arguments(&["--smoke", "--type=renderer"]), service, cef).unwrap(),
          CrashOptions::default()
        );
      }
    }
  }

  #[test]
  fn explicit_service_and_cef_paths_remain_owned_data_without_directory_creation() {
    let root = absolute(&format!(
      "lintel-crash-options-{}-{}",
      std::process::id(),
      std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
    ));
    let service = root.join("services");
    let cef = root.join("cef");
    let options = parse(
      [
        "--service-crash-directory".into(),
        service.clone().into_os_string(),
        "--cef-crash-directory".into(),
        cef.clone().into_os_string(),
      ],
      true,
      true,
    )
    .unwrap();
    assert_eq!(options.service_directory, Some(service));
    assert_eq!(options.cef_directory, Some(cef));
    assert!(!root.exists());
  }

  #[test]
  fn service_directory_rejects_missing_empty_relative_and_duplicate_values() {
    for values in [
      arguments(&["--service-crash-directory"]),
      arguments(&["--service-crash-directory", ""]),
      arguments(&["--service-crash-directory", "relative"]),
      arguments(&["--service-crash-directory", "--smoke"]),
    ] {
      assert_eq!(
        parse(values, true, false).unwrap_err(),
        "--service-crash-directory requires an absolute path"
      );
    }
    let path = absolute("lintel-service-crash-options").into_os_string();
    assert_eq!(
      parse(
        [
          "--service-crash-directory".into(),
          path.clone(),
          "--service-crash-directory".into(),
          path
        ],
        true,
        false
      )
      .unwrap_err(),
      "--service-crash-directory can only be supplied once"
    );
  }

  #[test]
  fn directory_opt_in_requires_the_compiled_feature_or_the_correct_application_role() {
    let path = absolute("lintel-service-crash-options").into_os_string();
    assert_eq!(
      parse(["--service-crash-directory".into(), path.clone()], false, true).unwrap_err(),
      "--service-crash-directory requires building with --features service-crash-reports"
    );
    assert_eq!(
      parse(["--cef-crash-directory".into(), path], true, false).unwrap_err(),
      "--cef-crash-directory requires the WebView application"
    );
  }

  #[test]
  fn cef_directory_uses_the_same_explicit_value_validation() {
    for values in [
      arguments(&["--cef-crash-directory"]),
      arguments(&["--cef-crash-directory", "relative"]),
    ] {
      assert_eq!(
        parse(values, false, true).unwrap_err(),
        "--cef-crash-directory requires an absolute path"
      );
    }
    let path = absolute("lintel-cef-crash-options").into_os_string();
    assert_eq!(
      parse(
        [
          "--cef-crash-directory".into(),
          path.clone(),
          "--cef-crash-directory".into(),
          path
        ],
        false,
        true
      )
      .unwrap_err(),
      "--cef-crash-directory can only be supplied once"
    );
  }

  #[cfg(unix)]
  #[test]
  fn service_directory_preserves_non_utf8_native_paths() {
    use std::os::unix::ffi::OsStringExt;
    let path = OsString::from_vec(b"/tmp/lintel-crash-\xff".to_vec());
    let options = parse(["--service-crash-directory".into(), path.clone()], true, false).unwrap();
    assert_eq!(options.service_directory, Some(PathBuf::from(path)));
  }
}
