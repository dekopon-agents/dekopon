#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
manifest="$root/examples/providers/gh/Cargo.toml"
core="$root/examples/providers/gh/target/wasm32-unknown-unknown/release/dekopon_gh_provider.wasm"
component="$root/examples/providers/gh-provider.wasm"

required_wasm_tools="wasm-tools 1.236.1"
command -v wasm-tools >/dev/null 2>&1 || {
  echo "error: $required_wasm_tools is required (cargo install wasm-tools --version 1.236.1 --locked)" >&2
  exit 1
}
actual_wasm_tools=$(wasm-tools --version)
if [[ "$actual_wasm_tools" != "$required_wasm_tools" ]]; then
  echo "error: expected $required_wasm_tools, found $actual_wasm_tools" >&2
  echo "install it with: cargo install wasm-tools --version 1.236.1 --locked --force" >&2
  exit 1
fi

rustup target add wasm32-unknown-unknown
cargo build --locked --manifest-path "$manifest" --target wasm32-unknown-unknown --release
wasm-tools component new "$core" -o "$component"
printf 'generated %s\n' "$component"
