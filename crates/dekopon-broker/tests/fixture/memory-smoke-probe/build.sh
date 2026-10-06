#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../../.." && pwd)
manifest="$root/crates/dekopon-broker/tests/fixture/memory-smoke-probe/Cargo.toml"
core="$root/crates/dekopon-broker/tests/fixture/memory-smoke-probe/target/wasm32-unknown-unknown/release/dekopon_memory_smoke_probe_provider.wasm"
component="$root/crates/dekopon-broker/tests/fixture/memory-smoke-probe-provider.wasm"

"$root/examples/providers/build-component.sh" \
  "$manifest" "$core" "$component" \
  "dekopon-provider-repro-v1"
