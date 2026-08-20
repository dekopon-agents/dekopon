#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
manifest="$root/examples/providers/memory-reservation-probe/Cargo.toml"
core="$root/examples/providers/memory-reservation-probe/target/wasm32-unknown-unknown/release/dekopon_memory_reservation_probe_provider.wasm"
component="$root/examples/providers/memory-reservation-probe-provider.wasm"

required_wasm_tools_version="1.236.1"
command -v wasm-tools >/dev/null 2>&1 || {
  echo "error: wasm-tools $required_wasm_tools_version is required" >&2
  exit 1
}
actual_wasm_tools=$(wasm-tools --version)
actual_wasm_tools_version=${actual_wasm_tools#wasm-tools }
actual_wasm_tools_version=${actual_wasm_tools_version%% *}
if [[ "$actual_wasm_tools_version" != "$required_wasm_tools_version" ]]; then
  echo "error: expected wasm-tools $required_wasm_tools_version, found $actual_wasm_tools" >&2
  exit 1
fi

rustup target add wasm32-unknown-unknown
cargo build --locked --manifest-path "$manifest" --target wasm32-unknown-unknown --release
wasm-tools component new "$core" -o "$component"
printf 'generated %s\n' "$component"
