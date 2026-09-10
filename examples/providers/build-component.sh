#!/usr/bin/env bash
set -euo pipefail

if (($# != 4)); then
  echo "usage: $0 <manifest> <core-wasm> <component-wasm> <metadata-domain>" >&2
  exit 2
fi

manifest=$1
core=$2
component=$3
metadata_domain=$4
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
# The component toolchain pin lives in rust-toolchain.toml and ci/toolchain.env only.
rust_toolchain=$(sed -n 's/^channel = "\(.*\)"$/\1/p' "$root/rust-toolchain.toml")
# shellcheck source=/dev/null
source "$root/ci/toolchain.env"

command -v rustup >/dev/null 2>&1 || {
  echo "error: rustup with Rust $rust_toolchain is required" >&2
  exit 1
}
if ! actual_rustc=$(rustup run "$rust_toolchain" rustc --version 2>/dev/null); then
  echo "error: Rust $rust_toolchain is required (rustup toolchain install $rust_toolchain --profile minimal)" >&2
  exit 1
fi
if [[ "$actual_rustc" != "rustc $rust_toolchain "* ]]; then
  echo "error: expected rustc $rust_toolchain, found $actual_rustc" >&2
  exit 1
fi

command -v wasm-tools >/dev/null 2>&1 || {
  echo "error: wasm-tools $WASM_TOOLS_VERSION is required (cargo install wasm-tools --version $WASM_TOOLS_VERSION --locked)" >&2
  exit 1
}
actual_wasm_tools=$(wasm-tools --version)
actual_wasm_tools_version=${actual_wasm_tools#wasm-tools }
actual_wasm_tools_version=${actual_wasm_tools_version%% *}
if [[ "$actual_wasm_tools_version" != "$WASM_TOOLS_VERSION" ]]; then
  echo "error: expected wasm-tools $WASM_TOOLS_VERSION, found $actual_wasm_tools" >&2
  exit 1
fi

cargo_home=${CARGO_HOME:-"$HOME/.cargo"}
cargo_home=$(cd "$cargo_home" && pwd -P)
sysroot=$(rustup run "$rust_toolchain" rustc --print sysroot)
sysroot=$(cd "$sysroot" && pwd -P)
rustc_path=$(rustup which --toolchain "$rust_toolchain" rustc)
target_root=$(dirname "$(dirname "$(dirname "$core")")")
rustc_proxy="$target_root/deterministic-rustc"
mkdir -p "$target_root"
cat >"$rustc_proxy" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

actual_rustc=${DEKOPON_BUILD_RUSTC:?}
source_root=${DEKOPON_BUILD_SOURCE_ROOT:?}
metadata_domain=${DEKOPON_BUILD_METADATA_DOMAIN:?}
manifest_dir=${CARGO_MANIFEST_DIR-}
repository_crate=false
if [[ "$manifest_dir" == "$source_root" || "$manifest_dir" == "$source_root/"* ]]; then
  repository_crate=true
fi

target=host
expect_target=false
for argument in "$@"; do
  if [[ "$expect_target" == true ]]; then
    target=$argument
    expect_target=false
    continue
  fi
  case $argument in
    --target)
      expect_target=true
      ;;
    --target=*)
      target=${argument#--target=}
      ;;
  esac
done

normalize_metadata=$repository_crate
if [[ "$target" == wasm32-unknown-unknown ]]; then
  normalize_metadata=true
fi

args=()
crate_name=
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
      if (($# >= 2)) && [[ $2 == metadata=* ]] && [[ "$normalize_metadata" == true ]]; then
        shift 2
      else
        args+=("$1")
        shift
      fi
      ;;
    -Cmetadata=*)
      if [[ "$normalize_metadata" == true ]]; then
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

if [[ "$normalize_metadata" == true && -n "$crate_name" && -n "${CARGO_PKG_NAME-}" && -n "${CARGO_PKG_VERSION-}" ]]; then
  args+=(
    -C
    "metadata=$metadata_domain-${CARGO_PKG_NAME}-${CARGO_PKG_VERSION}-$crate_name-$target"
  )
fi
# Darwin SIP clears rustup's loader environment across this shell proxy. Re-enter rustup so
# the pinned compiler's dynamically linked rust-lld can find its matching LLVM library.
exec rustup run "${DEKOPON_BUILD_RUST_TOOLCHAIN:?}" "$actual_rustc" "${args[@]}"
EOF
chmod 0700 "$rustc_proxy"

printf -v encoded_rustflags '%s\x1f%s\x1f%s\x1f%s\x1f%s\x1f%s' \
  "--remap-path-prefix=$root=/dekopon/source" \
  "--remap-path-prefix=$cargo_home=/dekopon/cargo" \
  "--remap-path-prefix=$sysroot=/dekopon/rust/$rust_toolchain" \
  '--cfg=dekopon_provider_repro_v1' \
  '--check-cfg=cfg(dekopon_provider_repro_v1)' \
  '-Ccodegen-units=1'

rustup target add --toolchain "$rust_toolchain" wasm32-unknown-unknown
CARGO_ENCODED_RUSTFLAGS="$encoded_rustflags" \
  DEKOPON_BUILD_RUSTC="$rustc_path" \
  DEKOPON_BUILD_RUST_TOOLCHAIN="$rust_toolchain" \
  DEKOPON_BUILD_SOURCE_ROOT="$root" \
  DEKOPON_BUILD_METADATA_DOMAIN="$metadata_domain" \
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
