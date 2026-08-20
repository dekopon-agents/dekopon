#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
manifest="$root/examples/providers/memory-chat/Cargo.toml"
core="$root/examples/providers/memory-chat/target/wasm32-unknown-unknown/release/dekopon_memory_chat_provider.wasm"
component="$root/examples/providers/memory-chat-provider.wasm"
required="wasm-tools 1.236.1"
[[ "$(wasm-tools --version 2>/dev/null || true)" == "$required" ]] || {
  echo "error: $required is required" >&2
  exit 1
}
rustup target add wasm32-unknown-unknown
cargo build --locked --manifest-path "$manifest" --target wasm32-unknown-unknown --release
wasm-tools component new "$core" -o "$component"
printf 'generated %s\n' "$component"
