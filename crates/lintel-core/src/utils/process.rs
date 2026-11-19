use std::path::PathBuf;

use tracing::error;

/// 获取当前进程的的可执行文件路径
pub fn current_binary() -> std::io::Result<PathBuf> {
  lintel_lib_utils::platform::current_exe()
}

pub fn get_arg_os() -> Vec<std::ffi::OsString> {
  std::env::args_os().collect()
}

#[derive(Debug, Clone)]
pub struct Env {
  arg_os: Vec<std::ffi::OsString>,
}

impl Env {
  pub fn new() -> Self {
    Self { arg_os: get_arg_os() }
  }
}

impl Default for Env {
  fn default() -> Self {
    Self::new()
  }
}

/// 创建替换进程。调用方必须先完成清理并退出事件循环，再调用此函数。
/// None 保留当前参数，Some 则完整替换参数（不包含可执行文件名）。
pub fn spawn_replacement(env: &Env, args: Option<&[String]>) -> std::io::Result<std::process::Child> {
  replacement_command(&current_binary()?, env, args)?.spawn()
}

fn replacement_command(
  current_binary: &std::path::Path,
  env: &Env,
  args: Option<&[String]>,
) -> std::io::Result<std::process::Command> {
  let mut command = std::process::Command::new(replacement_binary(current_binary)?);
  match args {
    Some(args) => {
      command.args(args);
    }
    None => {
      command.args(env.arg_os.iter().skip(1));
    }
  }
  Ok(command)
}

fn replacement_binary(current_binary: &std::path::Path) -> std::io::Result<PathBuf> {
  #[cfg(target_os = "macos")]
  if let Some(macos) = current_binary.parent()
    && macos.file_name() == Some(std::ffi::OsStr::new("MacOS"))
    && let Some(contents) = macos.parent()
    && contents.file_name() == Some(std::ffi::OsStr::new("Contents"))
  {
    // 更新可能替换了二进制名称，遵循当前 bundle 的 CFBundleExecutable。
    let info: plist::Dictionary = plist::from_file(contents.join("Info.plist"))
      .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let executable = info
      .get("CFBundleExecutable")
      .and_then(|value| value.as_string())
      .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bundle has no CFBundleExecutable"))?;
    if executable.is_empty() || std::path::Path::new(executable).file_name() != Some(std::ffi::OsStr::new(executable)) {
      return Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "invalid bundle executable name",
      ));
    }
    return Ok(macos.join(executable));
  }
  Ok(current_binary.to_owned())
}

/// 重启进程
pub fn restart(env: &Env) -> ! {
  use std::process::{Command, exit};

  if let Ok(path) = current_binary() {
    // on macOS on updates the binary name might have changed
    // so we'll read the Contents/Info.plist file to determine the binary path
    #[cfg(target_os = "macos")]
    restart_macos_app(&path, env);

    if let Err(e) = Command::new(path)
      .args(env.arg_os.iter().skip(1).collect::<Vec<_>>())
      .spawn()
    {
      error!("failed to restart app: {e}");
    }
  }

  exit(0);
}

#[cfg(target_os = "macos")]
fn restart_macos_app(current_binary: &std::path::Path, env: &Env) {
  use std::process::{Command, exit};

  if let Some(macos_directory) = current_binary.parent() {
    if macos_directory.components().last() != Some(std::path::Component::Normal(std::ffi::OsStr::new("MacOS"))) {
      return;
    }

    if let Some(contents_directory) = macos_directory.parent() {
      if contents_directory.components().last() != Some(std::path::Component::Normal(std::ffi::OsStr::new("Contents")))
      {
        return;
      }

      if let Ok(info_plist) = plist::from_file::<_, plist::Dictionary>(contents_directory.join("Info.plist")) {
        if let Some(binary_name) = info_plist.get("CFBundleExecutable").and_then(|v| v.as_string()) {
          if let Err(e) = Command::new(macos_directory.join(binary_name))
            .args(env.arg_os.iter().skip(1).collect::<Vec<_>>())
            .spawn()
          {
            error!("failed to restart app: {e}");
          }

          exit(0);
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn replacement_preserves_or_replaces_arguments_without_shell_interpretation() {
    let env = Env {
      arg_os: ["old-binary", "--existing", "value with spaces"]
        .map(Into::into)
        .to_vec(),
    };
    let path = std::path::Path::new("test-lintel-app");
    let inherited = replacement_command(path, &env, None).unwrap();
    assert_eq!(inherited.get_program(), path.as_os_str());
    assert_eq!(
      inherited.get_args().collect::<Vec<_>>(),
      ["--existing", "value with spaces"]
    );
    let explicit = ["--replacement".to_owned(), "$(literal)".to_owned()];
    let replacement = replacement_command(path, &env, Some(&explicit)).unwrap();
    assert_eq!(
      replacement.get_args().collect::<Vec<_>>(),
      ["--replacement", "$(literal)"]
    );
    assert_eq!(
      replacement_command(path, &env, Some(&[])).unwrap().get_args().count(),
      0
    );
  }

  #[test]
  fn replacement_spawn_errors_are_returned_to_the_host() {
    let missing = std::env::temp_dir().join(format!("missing-lintel-executable-{}", rand::random::<u64>()));
    let error = replacement_command(&missing, &Env { arg_os: vec![] }, None)
      .unwrap()
      .spawn()
      .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
  }
}
