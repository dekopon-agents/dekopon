#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
manifest="$root/examples/providers/storage-probe/Cargo.toml"
core="$root/examples/providers/storage-probe/target/wasm32-unknown-unknown/release/dekopon_storage_probe_provider.wasm"
component="$root/examples/providers/storage-probe-provider.wasm"
"$root/examples/providers/build-component.sh" \
  "$manifest" "$core" "$component" \
  "1.97.0" "rustc 1.97.0 (2d8144b78 2026-07-07)" \
  "dekopon-provider-repro-v1"
