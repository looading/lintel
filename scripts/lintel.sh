#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd -P)
case "$(uname -s)/$(uname -m)" in
  Darwin/arm64) target=aarch64-apple-darwin ;;
  Darwin/x86_64) target=x86_64-apple-darwin ;;
  *) echo 'This Lintel checkout launcher requires macOS.' >&2; exit 1 ;;
esac

tool_target="$root/target/lintel-cli"
(
  cd "$root"
  CARGO_TARGET_DIR="$tool_target" cargo build --manifest-path crates/lintel-cli/Cargo.toml --quiet --locked --target "$target"
)
exec "$tool_target/$target/debug/lintel" "$@"
