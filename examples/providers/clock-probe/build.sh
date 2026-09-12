#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
manifest="$root/examples/providers/clock-probe/Cargo.toml"
core="$root/examples/providers/clock-probe/target/wasm32-unknown-unknown/release/dekopon_clock_probe_provider.wasm"
component="$root/examples/providers/clock-probe-provider.wasm"

"$root/examples/providers/build-component.sh" \
  "$manifest" "$core" "$component" \
  "dekopon-provider-repro-v1"
