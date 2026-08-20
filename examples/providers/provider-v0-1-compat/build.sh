#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
manifest="$root/examples/providers/provider-v0-1-compat/Cargo.toml"
core="$root/examples/providers/provider-v0-1-compat/target/wasm32-unknown-unknown/release/dekopon_provider_v0_1_compat.wasm"
component="$root/examples/providers/provider-v0-1-compat-provider.wasm"
required_wasm_tools_version="1.236.1"
actual_wasm_tools=$(wasm-tools --version 2>/dev/null || true)
actual_wasm_tools_version=${actual_wasm_tools#wasm-tools }
actual_wasm_tools_version=${actual_wasm_tools_version%% *}
[[ "$actual_wasm_tools_version" == "$required_wasm_tools_version" ]] || {
  echo "error: wasm-tools $required_wasm_tools_version is required; found $actual_wasm_tools" >&2
  exit 1
}
rustup target add wasm32-unknown-unknown
cargo build --locked --manifest-path "$manifest" --target wasm32-unknown-unknown --release
wasm-tools component new "$core" -o "$component"
printf 'generated %s\n' "$component"
