use std::path::{Component, Path, PathBuf};

/// 应用资源根目录下的通用本地页面路径，不预置业务页面或开发服务器。
pub struct WebviewPages;

impl WebviewPages {
  /// 解析 webview/pages 内的相对页面路径；不读取文件或授予文件访问能力。
  pub fn local_page(&self, relative: impl AsRef<Path>) -> anyhow::Result<PathBuf> {
    local_page_path(lintel_lib_env::LintelEnv::resources_path(), relative.as_ref())
  }
}

fn local_page_path(resources: &Path, relative: &Path) -> anyhow::Result<PathBuf> {
  anyhow::ensure!(
    !relative.as_os_str().is_empty()
      && relative
        .components()
        .all(|component| matches!(component, Component::Normal(_))),
    "local page must be a nonempty relative path without parent or root components"
  );
  Ok(resources.join("webview/pages").join(relative))
}

pub fn webview_pages() -> &'static WebviewPages {
  static PAGES: WebviewPages = WebviewPages;
  &PAGES
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn local_page_resolves_under_the_resource_root_without_io() {
    let resources = Path::new("application-resources");
    let page = local_page_path(resources, Path::new("notes/index.html")).unwrap();
    assert_eq!(page, resources.join("webview/pages/notes/index.html"));
  }

  #[test]
  fn local_page_rejects_empty_absolute_and_parent_components() {
    for relative in [
      "",
      ".",
      "..",
      "../index.html",
      "notes/../../index.html",
      "/outside.html",
    ] {
      assert!(
        local_page_path(Path::new("application-resources"), Path::new(relative)).is_err(),
        "{relative}"
      );
    }
  }

  #[cfg(unix)]
  #[test]
  fn local_page_preserves_native_non_unicode_path_components() {
    use std::os::unix::ffi::OsStrExt;
    let resources = Path::new("application-resources");
    let relative = Path::new(std::ffi::OsStr::from_bytes(b"notes-\xff/index.html"));
    assert_eq!(
      local_page_path(resources, relative).unwrap(),
      resources.join("webview/pages").join(relative)
    );
  }
}
