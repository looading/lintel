#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 ]]; then
  printf 'Usage: %s <webview example binary> <CEF distribution> [helper binary]\n' "$0" >&2
  exit 2
fi

root=$(cd "$(dirname "$0")/.." && pwd -P)
options=(--executable "$1" --cef-path "$2"
  --name 'Lintel WebView' --id dev.lintel.webview --output-dir "$root/build/macos")
if [[ $# -eq 3 ]]; then
  options+=(--helper "$3")
fi
if [[ -n ${DAWN_ROOT:-} ]]; then
  options+=(--dawn-root "$DAWN_ROOT")
fi
exec "$root/scripts/lintel.sh" bundle "${options[@]}"
