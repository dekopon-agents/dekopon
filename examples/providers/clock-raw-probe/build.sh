#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
"$root/examples/providers/build-component.sh" \
  "$root/examples/providers/clock-raw-probe/Cargo.toml" \
  "$root/examples/providers/clock-raw-probe/target/wasm32-unknown-unknown/release/dekopon_clock_raw_probe_provider.wasm" \
  "$root/examples/providers/clock-raw-probe-provider.wasm" \
  "dekopon-provider-repro-v1"
