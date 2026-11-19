# lintel-lib-env

Explicit process-wide application metadata, bundle resource paths, and selected
Lintel environment settings. It has no dependency on iced, CEF, or a GUI loop.

```toml
[dependencies]
lintel-lib-env = "0.1.0"
```

```rust
use lintel_lib_env::{
    global_webview_max_concurrent, GLOBAL_WEBVIEW_MAX_CONCURRENT_ENV,
};

assert_eq!(GLOBAL_WEBVIEW_MAX_CONCURRENT_ENV, "LINTEL_WEBVIEW_MAX_CONCURRENT");
let configured_limit = global_webview_max_concurrent();
assert!(configured_limit > 0);
```

`global_webview_max_concurrent()` reads a positive integer setting and falls back
to `DEFAULT_GLOBAL_WEBVIEW_MAX_CONCURRENT` for a missing or invalid value.
`webview_test_url_override()` reads the explicit `LINTEL_WEBVIEW_TEST_URL`
setting. These helpers only return configuration; they do not create WebViews,
schedule tasks, or open a URL.

`LintelEnv::init` installs application ID/version, executable and bundle paths,
user-data/cache paths, a device identifier, and a download directory once per
process. Initialize it before using its static getters. The caller supplies the
paths; the library does not discover an application account or contact a server.
Resource resolution follows the framework's supported application bundle layouts
and may fail if required paths do not exist. Applications using another layout
should keep their own path model instead of relying on this singleton.

`init_for_test` is an explicit test utility that creates temporary fixture
directories; it is not application startup or automatic fallback initialization.

Licensed under MIT. See [LICENSE](LICENSE).
