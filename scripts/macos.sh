#!/usr/bin/env bash
set -euo pipefail

mode=${1:-}
case "$mode" in
  native|webview) shift ;;
  *) echo "Usage: $0 {native|webview} [--smoke]" >&2; exit 2 ;;
esac

root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"
case "$(uname -s)/$(uname -m)" in
  Darwin/arm64) arch=aarch64 ;;
  Darwin/x86_64) arch=x86_64 ;;
  *) echo 'This example launcher requires macOS.' >&2; exit 1 ;;
esac

options=(--locked --target "$arch-apple-darwin")
scripts/lintel.sh prepare --lintel-path "$root" >&2
options+=(--cargo-config "$root/.cargo/macos-public.toml" --features public-iced)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target/macos-public}

if [[ "$mode" == native ]]; then
  executable=$(scripts/lintel.sh build "${options[@]}" --example multi_window)
  exec "$executable" "$@"
fi

cached="$root/third_party/prebuilts/cef-public/macos/$arch"
if [[ -z ${CEF_PATH:-} && -d "$cached" ]]; then
  CEF_PATH=$cached
fi
if [[ -n ${CEF_PATH:-} ]]; then
  verified_cef=$(python3 scripts/public-cef-runtime.py --arch "$arch" --path "$CEF_PATH")
  options+=(--cef-path "$verified_cef")
else
  unset CEF_PATH
fi
options+=(--features public-cef)

exec scripts/lintel.sh run "${options[@]}" --example webview --name 'Lintel WebView' --id dev.lintel.webview \
  --output-dir "$root/build/macos" -- "$@"
