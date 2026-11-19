# lintel-lib-config

Generic runtime configuration, environment selection, and event-backed
environment changes for Lintel applications. This crate has no GUI dependency.

```toml
[dependencies]
lintel-lib-config = "0.1.0"
```

```rust
use lintel_lib_config::runtime_config::RuntimeConfig;

let config = RuntimeConfig::from_str(r#"
env: staging
proxy:
  headers:
    x-environment: staging
  urlsFilter:
    - https://api.example.test/*
"#);
assert!(config.env.is_staging());
assert_eq!(config.match_proxy_headers("https://api.example.test/items")
    .and_then(|headers| headers.get("x-environment")).map(String::as_str),
    Some("staging"));
```

`RuntimeConfig::from_config_dir` reads only `lintel-runtime.yml` beneath the
chosen directory. Missing or invalid files return the default configuration and
emit a diagnostic; there is no fallback filename. `from_file` and `from_str`
provide explicit sources. Parsing does not write files, start watchers, create a
network proxy, or connect to an MCP server.

`EnvType` has `Development`, `Staging`, and `Production` variants, serialized as
`development`, `staging`, and `production`; production is the default. The
`is_development`, `is_staging`, and `is_production` helpers use these same values.
`env::Env` exposes change subscriptions through `lintel-lib-event`; repeated
updates to the current value do not emit a change event.

The runtime schema contains environment/debug settings, generic UI performance
options, MCP endpoint settings, and proxy rules. Supply valid absolute URLs and
URL patterns. `match_proxy_headers` returns matching headers only outside
production; returning headers does not authorize or send a network request.
Inline MCP tokens are redacted by `Debug`, but configuration files remain
application-owned sensitive data. This library does not authenticate callers or
enforce a process sandbox.

Licensed under MIT. See [LICENSE](LICENSE).
