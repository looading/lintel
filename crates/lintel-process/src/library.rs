use std::{
  io,
  path::{Path, PathBuf},
};

/// 经宿主明确审计的原生 SDK 镜像；构造本身不加载动态库。
#[derive(Clone, Debug)]
pub struct TrustedRuntimeLibrary(PathBuf);

impl TrustedRuntimeLibrary {
  /// 声明匹配 SDK 0.3.0 官方 Chromium pin/C ABI 的可信原生镜像。
  ///
  /// # Safety
  /// 从本函数到原生加载期间，路径必须始终指向可信、匹配当前目标的 SDK 镜像。
  /// 文件存在、包签名或 ABI 字符串检查不能代替宿主对原生代码的信任决策。
  pub unsafe fn new(path: impl AsRef<Path>) -> io::Result<Self> {
    let path = path.as_ref();
    if !path.is_absolute() {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "Mojo runtime path must be absolute",
      ));
    }
    if !path.is_file() {
      return Err(io::Error::new(
        io::ErrorKind::NotFound,
        "Mojo runtime library is not a file",
      ));
    }
    Ok(Self(path.to_path_buf()))
  }

  /// 使用构建 CLI 放入应用包的固定位置，不回退到 cwd 或环境变量。
  ///
  /// # Safety
  /// 应用包及其 Mojo 镜像必须可信且匹配本 SDK；与 `new` 的前提相同。
  pub unsafe fn bundled() -> io::Result<Self> {
    let executable = std::env::current_exe()?;
    let path = runtime_path(&executable)?;
    // SAFETY: 调用者承担应用包与匹配原生镜像的信任前提。
    unsafe { Self::new(path) }
  }

  pub fn path(&self) -> &Path {
    &self.0
  }
}

fn runtime_name() -> &'static str {
  if cfg!(target_os = "macos") {
    "libmojo_rs_native.dylib"
  } else if cfg!(target_os = "windows") {
    "mojo_rs_native.dll"
  } else {
    "libmojo_rs_native.so"
  }
}

/// CEF Helper 的入口会先构建应用配置，再进行 CEF 进程角色分发。
/// 只识别既有 bundle 的固定嵌套布局，不按文件存在性向包外搜索。
#[cfg(target_os = "macos")]
fn macos_contents(executable: &Path) -> Option<&Path> {
  let directory = executable.parent()?;
  if !matches!(directory.file_name()?.to_str()?, "MacOS" | "Helpers") {
    return None;
  }
  let contents = directory.parent()?;
  if contents.file_name()? != "Contents" {
    return None;
  }
  if directory.file_name()? == "MacOS" {
    let helper_app = contents.parent()?;
    let helper_name = helper_app
      .file_stem()
      .and_then(|name| name.to_str())
      .unwrap_or_default();
    let recognized_role = [
      " Helper",
      " Helper (GPU)",
      " Helper (Renderer)",
      " Helper (Plugin)",
      " Helper (Alerts)",
    ]
    .into_iter()
    .any(|suffix| helper_name.ends_with(suffix));
    if helper_app.extension().is_some_and(|extension| extension == "app") && recognized_role {
      if let Some(frameworks) = helper_app
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "Frameworks"))
      {
        if let Some(outer_contents) = frameworks
          .parent()
          .filter(|path| path.file_name().is_some_and(|name| name == "Contents"))
        {
          if outer_contents
            .parent()
            .is_some_and(|path| path.extension().is_some_and(|extension| extension == "app"))
          {
            return Some(outer_contents);
          }
        }
      }
    }
  }
  Some(contents)
}

pub(crate) fn runtime_path(executable: &Path) -> io::Result<PathBuf> {
  let parent = executable
    .parent()
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "executable has no parent"))?;
  #[cfg(target_os = "macos")]
  {
    if let Some(contents) = macos_contents(executable) {
      return Ok(contents.join("Frameworks").join(runtime_name()));
    }
  }
  let directory = if parent.file_name().is_some_and(|s| s == "Helpers") {
    parent
      .parent()
      .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "helper has no application root"))?
  } else {
    parent
  };
  Ok(directory.join(runtime_name()))
}

pub(crate) fn helper_path(executable: &Path, name: &str) -> io::Result<PathBuf> {
  if name.is_empty()
    || name.len() > 100
    || !name
      .bytes()
      .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
  {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "service name must be an extension-free ASCII basename",
    ));
  }
  let parent = executable
    .parent()
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "executable has no parent"))?;
  #[cfg(target_os = "macos")]
  let directory = if let Some(contents) = macos_contents(executable) {
    contents.join("Helpers")
  } else if parent.file_name().is_some_and(|s| s == "Helpers") {
    parent.to_path_buf()
  } else {
    parent.join("Helpers")
  };
  #[cfg(target_os = "windows")]
  let directory = parent.to_path_buf();
  #[cfg(not(any(target_os = "macos", target_os = "windows")))]
  let directory = if parent.file_name().is_some_and(|s| s == "Helpers") {
    parent.to_path_buf()
  } else {
    parent.join("Helpers")
  };
  Ok(directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
}

/// 解析 CLI 打包的服务可执行文件；名称不是任意路径，不搜索 PATH/cwd。
pub fn service_executable(name: &str) -> io::Result<PathBuf> {
  let path = helper_path(&std::env::current_exe()?, name)?;
  if !path.is_file() {
    return Err(io::Error::new(
      io::ErrorKind::NotFound,
      "bundled service executable is missing",
    ));
  }
  Ok(path)
}
