#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"
target=${1:-$(rustc -vV | awk '/^host: / { print $2 }')}

resolved_packages() {
  cargo tree --locked -p lintel-framework --target "$target" --edges normal,build,dev --prefix none "$@" \
    | awk '{ print $1 }' | sort -u
}

native=$(resolved_packages)
webview=$(resolved_packages --features webview-cef)
if grep -E '^(cef|cef-dll-sys|download-cef|lintel-iced-widgets)$' <<< "$native"; then
  echo 'Native graph contains a WebView dependency.' >&2
  exit 1
fi
if ! grep -Fxq cef <<< "$webview"; then
  echo 'WebView graph is missing CEF.' >&2
  exit 1
fi
printf 'Dependency boundaries passed for %s (native and webview-cef).\n' "$target"
