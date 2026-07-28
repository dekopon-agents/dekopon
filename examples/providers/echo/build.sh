#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
manifest="$root/examples/providers/echo/Cargo.toml"
core="$root/examples/providers/echo/target/wasm32-unknown-unknown/release/dekopon_echo_provider.wasm"
component="$root/examples/providers/echo-provider.wasm"

command -v wasm-tools >/dev/null 2>&1 || {
  echo "error: wasm-tools is required (cargo install wasm-tools --locked)" >&2
  exit 1
}

rustup target add wasm32-unknown-unknown
cargo build --locked --manifest-path "$manifest" --target wasm32-unknown-unknown --release
wasm-tools component new "$core" -o "$component"
printf 'generated %s\n' "$component"
