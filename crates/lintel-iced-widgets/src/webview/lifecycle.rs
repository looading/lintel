use std::fmt::Debug;

use url::Url;

use crate::webview::RequestEvent;

/// Lifecyele is the trait that handles the lifecycle events of the webview.
///
/// # Example
///
/// ```
/// use lintel_iced_widgets::webview::{ClientLifecyeleImpl, RequestEvent};
///
///
/// impl ClientLifecyeleImpl for MyLifecyele {
///   fn before_browser(&mut self, event: RequestEvent) -> bool {
///     true
///   }
/// }
/// ```
pub trait ClientLifecyeleImpl: Send + Sync + Debug {
  /// 在 CEF 的主框架加载开始回调内同步通知，早于异步 UI 加载消息。
  /// 用于撤销旧文档的受管请求；实现必须保持简短，不操作借用的 UI 状态。
  fn on_main_frame_load_start(&self) {}

  /// 在原生 JS 入口处取得可信文档代号，默认 None 保留普通 widget 的既有接入。
  /// 该值仅在 browser 进程内部传递，不能从 JS 参数读取。
  fn bridge_generation(&self) -> Option<u64> {
    None
  }

  /// before_browser is called before the browser is navigated to the new url.
  ///
  /// # Arguments
  ///
  /// * `event` - The request event.
  ///
  /// # Returns
  ///
  /// * `bool` - Whether to continue the navigation.
  fn before_browser(&self, event: RequestEvent) -> bool {
    true
  }

  /// Return whether CEF may navigate the given frame to this URL.
  fn allows_navigation(&self, _url: &str, _is_main_frame: bool) -> bool {
    true
  }

  /// Return whether an invocation from this CEF frame may be sent to the native bridge.
  fn allows_js_invocation(&self, _url: &str, _is_main_frame: bool, _from_renderer: bool, _payload_size: usize) -> bool {
    true
  }

  /// Opt into request-context filtering for every resource load.
  fn restricts_resources(&self) -> bool {
    false
  }

  /// Return whether the request context may load a resource when filtering is enabled.
  fn allows_resource(&self, _url: &str) -> bool {
    true
  }
}

const MAX_LOCAL_BRIDGE_PAYLOAD: usize = 1024 * 1024;

/// Allow one local main-frame document and explicitly registered local subresources.
/// Only the document may access the native bridge.
#[derive(Debug)]
pub struct LocalPagePolicy {
  page: Url,
  assets: Vec<Url>,
}

impl LocalPagePolicy {
  /// Register the page URL. Same-document fragment changes remain allowed.
  pub fn new(url: Url) -> Self {
    Self {
      page: without_fragment(url),
      assets: Vec::new(),
    }
  }

  /// Register an exact local file URL for a subresource, such as a packaged script.
  pub fn with_asset(mut self, url: Url) -> Self {
    if is_local_file(&url) {
      self.assets.push(without_fragment(url));
    }
    self
  }
}

fn is_local_file(url: &Url) -> bool {
  url.scheme() == "file" && url.host_str().is_none()
}

fn without_fragment(mut url: Url) -> Url {
  url.set_fragment(None);
  url
}

impl ClientLifecyeleImpl for LocalPagePolicy {
  fn allows_navigation(&self, url: &str, is_main_frame: bool) -> bool {
    is_main_frame && is_local_file(&self.page) && Url::parse(url).is_ok_and(|url| without_fragment(url) == self.page)
  }

  fn allows_js_invocation(&self, url: &str, is_main_frame: bool, from_renderer: bool, payload_size: usize) -> bool {
    from_renderer && payload_size <= MAX_LOCAL_BRIDGE_PAYLOAD && self.allows_navigation(url, is_main_frame)
  }

  fn restricts_resources(&self) -> bool {
    true
  }

  fn allows_resource(&self, url: &str) -> bool {
    is_local_file(&self.page)
      && Url::parse(url).is_ok_and(|url| {
        let url = without_fragment(url);
        url == self.page || self.assets.contains(&url)
      })
  }
}

#[cfg(test)]
mod tests {
  use super::{ClientLifecyeleImpl, LocalPagePolicy, MAX_LOCAL_BRIDGE_PAYLOAD};
  use url::Url;

  const LOCAL: &str = "file:///app/ui/index.html";

  #[test]
  fn local_page_navigation_only_accepts_registered_main_frame_and_fragments() {
    let policy = LocalPagePolicy::new(Url::parse(LOCAL).unwrap());
    assert!(policy.allows_navigation(LOCAL, true));
    assert!(policy.allows_navigation("file:///app/ui/index.html#status", true));
    assert!(!policy.allows_navigation(LOCAL, false));
    for url in [
      "file:///app/ui/other.html",
      "file:///app/ui/index.html?foreign=1",
      "file://server/app/ui/index.html",
      "https://example.invalid/index.html",
      "about:blank",
      "invalid",
    ] {
      assert!(!policy.allows_navigation(url, true), "{url}");
    }
  }

  #[test]
  fn local_page_bridge_requires_renderer_main_frame_and_bounded_payload() {
    let policy = LocalPagePolicy::new(Url::parse(LOCAL).unwrap());
    assert!(policy.allows_js_invocation(LOCAL, true, true, MAX_LOCAL_BRIDGE_PAYLOAD));
    assert!(!policy.allows_js_invocation(LOCAL, true, true, MAX_LOCAL_BRIDGE_PAYLOAD + 1));
    assert!(!policy.allows_js_invocation(LOCAL, true, false, 1));
    assert!(!policy.allows_js_invocation(LOCAL, false, true, 1));
    assert!(!policy.allows_js_invocation("file:///other.html", true, true, 1));
  }

  #[test]
  fn local_page_policy_denies_nonlocal_registrations() {
    for url in ["https://example.invalid/index.html", "file://server/app/ui/index.html"] {
      let policy = LocalPagePolicy::new(Url::parse(url).unwrap());
      assert!(!policy.allows_navigation(url, true));
      assert!(!policy.allows_js_invocation(url, true, true, 1));
      assert!(!policy.allows_resource(url));
    }
  }

  #[test]
  fn local_page_resources_only_accept_the_page_and_registered_local_assets() {
    let asset = "file:///app/ui/lintel.js";
    let remote_script = "https://example.invalid/remote.js";
    let policy = LocalPagePolicy::new(Url::parse(LOCAL).unwrap())
      .with_asset(Url::parse(asset).unwrap())
      .with_asset(Url::parse(remote_script).unwrap());
    assert!(policy.restricts_resources());
    assert!(policy.allows_resource(LOCAL));
    assert!(policy.allows_resource(asset));
    assert!(!policy.allows_resource(remote_script));
    assert!(!policy.allows_resource("file:///app/ui/other.js"));
    assert!(!policy.allows_resource("file:///app/ui/lintel.js?foreign=1"));
    assert!(!policy.allows_navigation(asset, true));
    assert!(!policy.allows_js_invocation(asset, true, true, 1));
  }
}
