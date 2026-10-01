#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
"$root/examples/providers/build-component.sh" \
  "$root/examples/providers/http-raw-probe/Cargo.toml" \
  "$root/examples/providers/http-raw-probe/target/wasm32-unknown-unknown/release/dekopon_http_raw_probe_provider.wasm" \
  "$root/examples/providers/http-raw-probe-provider.wasm" \
  "dekopon-provider-repro-v1"
