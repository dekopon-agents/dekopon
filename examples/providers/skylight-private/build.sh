#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd -P)
manifest="$root/examples/providers/skylight-private/Cargo.toml"
core="$root/examples/providers/skylight-private/target/wasm32-unknown-unknown/release/dekopon_skylight_private_provider.wasm"
component="$root/examples/providers/skylight-private-provider.wasm"

rust_toolchain="1.89.0"
required_rustc="rustc 1.89.0 (29483883e 2025-08-04)"
command -v rustup >/dev/null 2>&1 || {
  echo "error: rustup with Rust $rust_toolchain is required" >&2
  exit 1
}
if ! actual_rustc=$(rustup run "$rust_toolchain" rustc --version 2>/dev/null); then
  echo "error: Rust $rust_toolchain is required (rustup toolchain install $rust_toolchain --profile minimal)" >&2
  exit 1
fi
if [[ "$actual_rustc" != "$required_rustc" ]]; then
  echo "error: expected $required_rustc, found $actual_rustc" >&2
  exit 1
fi

required_wasm_tools="wasm-tools 1.236.1"
command -v wasm-tools >/dev/null 2>&1 || {
  echo "error: $required_wasm_tools is required (cargo install wasm-tools --version 1.236.1 --locked)" >&2
  exit 1
}
actual_wasm_tools=$(wasm-tools --version)
if [[ "$actual_wasm_tools" != "$required_wasm_tools" ]]; then
  echo "error: expected $required_wasm_tools, found $actual_wasm_tools" >&2
  echo "install it with: cargo install wasm-tools --version 1.236.1 --locked --force" >&2
  exit 1
fi

cargo_home=${CARGO_HOME:-"$HOME/.cargo"}
cargo_home=$(cd "$cargo_home" && pwd -P)
sysroot=$(rustup run "$rust_toolchain" rustc --print sysroot)
sysroot=$(cd "$sysroot" && pwd -P)
rustc_path=$(rustup which --toolchain "$rust_toolchain" rustc)
rustc_proxy="$root/examples/providers/skylight-private/target/deterministic-rustc"
mkdir -p "$(dirname "$rustc_proxy")"
cat >"$rustc_proxy" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

actual_rustc=${DEKOPON_BUILD_RUSTC:?}
source_root=${DEKOPON_BUILD_SOURCE_ROOT:?}
manifest_dir=${CARGO_MANIFEST_DIR-}
repository_crate=false
if [[ "$manifest_dir" == "$source_root" || "$manifest_dir" == "$source_root/"* ]]; then
  repository_crate=true
fi

args=()
crate_name=
target=host
while (($#)); do
  case $1 in
    --crate-name)
      crate_name=$2
      args+=("$1" "$2")
      shift 2
      ;;
    --target)
      target=$2
      args+=("$1" "$2")
      shift 2
      ;;
    --target=*)
      target=${1#--target=}
      args+=("$1")
      shift
      ;;
    -C)
      if (($# >= 2)) && [[ $2 == metadata=* ]] && [[ "$repository_crate" == true ]]; then
        shift 2
      else
        args+=("$1")
        shift
      fi
      ;;
    -Cmetadata=*)
      if [[ "$repository_crate" == true ]]; then
        shift
      else
        args+=("$1")
        shift
      fi
      ;;
    *)
      args+=("$1")
      shift
      ;;
  esac
done

if [[ "$repository_crate" == true && -n "$crate_name" ]]; then
  args+=(
    -C
    "metadata=dekopon-skylight-repro-v1-${CARGO_PKG_NAME}-${CARGO_PKG_VERSION}-$crate_name-$target"
  )
fi
exec "$actual_rustc" "${args[@]}"
EOF
chmod 0700 "$rustc_proxy"

# Cargo includes checkout-dependent path-package IDs in its rustc metadata arguments. The proxy
# preserves the configured RUSTC_WRAPPER (including sccache), delegates to the pinned compiler, and
# replaces only that local metadata. Bump the v1 salt in both places if the normalization changes.
printf -v encoded_rustflags '%s\x1f%s\x1f%s\x1f%s\x1f%s\x1f%s' \
  "--remap-path-prefix=$root=/dekopon/source" \
  "--remap-path-prefix=$cargo_home=/dekopon/cargo" \
  "--remap-path-prefix=$sysroot=/dekopon/rust/$rust_toolchain" \
  '--cfg=dekopon_skylight_repro_v1' \
  '--check-cfg=cfg(dekopon_skylight_repro_v1)' \
  '-Ccodegen-units=1'

rustup target add --toolchain "$rust_toolchain" wasm32-unknown-unknown
CARGO_ENCODED_RUSTFLAGS="$encoded_rustflags" \
  DEKOPON_BUILD_RUSTC="$rustc_path" \
  DEKOPON_BUILD_SOURCE_ROOT="$root" \
  RUSTC="$rustc_proxy" \
  rustup run "$rust_toolchain" cargo build \
  --locked --manifest-path "$manifest" --target wasm32-unknown-unknown --release
wasm-tools component new "$core" -o "$component"

for local_path in "$root" "$cargo_home" "$sysroot"; do
  if LC_ALL=C grep -aF -- "$local_path" "$component" >/dev/null; then
    echo "error: generated component embeds local build path: $local_path" >&2
    exit 1
  fi
done

printf 'generated %s with Rust %s and remapped build paths\n' "$component" "$rust_toolchain"
