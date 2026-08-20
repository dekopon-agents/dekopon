#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
manifest="$root/examples/providers/storage-probe/Cargo.toml"
core="$root/examples/providers/storage-probe/target/wasm32-unknown-unknown/release/dekopon_storage_probe_provider.wasm"
component="$root/examples/providers/storage-probe-provider.wasm"
[[ "$(wasm-tools --version 2>/dev/null || true)" == "wasm-tools 1.236.1" ]] || { echo "error: wasm-tools 1.236.1 is required" >&2; exit 1; }
rustup target add wasm32-unknown-unknown
cargo build --locked --manifest-path "$manifest" --target wasm32-unknown-unknown --release
wasm-tools component new "$core" -o "$component"
printf 'generated %s\n' "$component"
