#!/usr/bin/env bash
# Fetch exact standalone provider release assets for tests, packaging, or image staging.
# Each provider pins its own release tag, checksum, and size below.
# Source and generated Wasm are intentionally not tracked in the Dekopon core repository.
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 DESTINATION [jsonplaceholder|memory-chat ...]" >&2
  exit 2
fi

destination=$1
shift
providers=("$@")
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
tracked=$(git -C "$root" ls-files -- \
  examples/providers/jsonplaceholder examples/providers/jsonplaceholder-provider.wasm \
  examples/providers/jsonplaceholder-provider.wasm.sha256 \
  examples/providers/memory-chat examples/providers/memory-chat-provider.wasm \
  examples/providers/memory-chat-provider.wasm.sha256)
[[ -z "$tracked" ]] || {
  echo 'error: standalone provider source or generated Wasm is tracked in core:' >&2
  printf '%s\n' "$tracked" >&2
  exit 1
}
if [[ ${#providers[@]} -eq 0 ]]; then
  providers=(jsonplaceholder memory-chat)
fi

seen=' '
for provider in "${providers[@]}"; do
  case "$provider" in
    jsonplaceholder|memory-chat) ;;
    *)
      echo "error: unknown external provider: $provider" >&2
      exit 2
      ;;
  esac
  if [[ "$seen" == *" $provider "* ]]; then
    echo "error: duplicate provider requested: $provider" >&2
    exit 2
  fi
  seen+="$provider "
done

for command in curl git install mktemp mv; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "error: $command is required" >&2
    exit 1
  }
done
if command -v sha256sum >/dev/null 2>&1; then
  digest_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  digest_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  echo 'error: sha256sum or shasum is required' >&2
  exit 1
fi

verify_attestations=${DEKOPON_VERIFY_PROVIDER_ATTESTATIONS:-0}
if [[ "$verify_attestations" != 0 && "$verify_attestations" != 1 ]]; then
  echo 'error: DEKOPON_VERIFY_PROVIDER_ATTESTATIONS must be 0 or 1' >&2
  exit 1
fi
if [[ "$verify_attestations" == 1 ]]; then
  command -v gh >/dev/null 2>&1 || {
    echo 'error: gh is required when attestation verification is enabled' >&2
    exit 1
  }
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/dekopon-external-providers.XXXXXX")
publish_temps=()
cleanup() {
  rm -rf "$work"
  if [[ ${#publish_temps[@]} -gt 0 ]]; then
    rm -f -- "${publish_temps[@]}"
  fi
}
trap cleanup EXIT

fetch_provider() {
  local provider=$1
  local repository asset release expected_sha expected_size signer source_ref source_digest
  case "$provider" in
    jsonplaceholder)
      repository=dekopon-agents/dekopon-provider-jsonplaceholder
      asset=jsonplaceholder-provider.wasm
      release=v0.3.0
      expected_sha=b20e20c675bfaa357623bfe443c2c4793938548e4cd1addc90dc23e9fe3c65c1
      expected_size=470999
      signer="$repository/.github/workflows/release.yml"
      source_ref=refs/tags/v0.3.0
      source_digest=2deb4e8931c025055ec2f7ba96ca124d894093f5
      ;;
    memory-chat)
      repository=dekopon-agents/dekopon-provider-memory-chat
      asset=memory-chat-provider.wasm
      release=v0.2.0
      expected_sha=417b9cd7a21f0cd5bf03f05ad159753f56463add6865860ecfeb33af8938776f
      expected_size=253670
      signer="$repository/.github/workflows/release.yml"
      source_ref=refs/tags/v0.2.0
      source_digest=5aa6eac2aa07b0691682a532cb16fc93144eb358
      ;;
    *)
      echo "error: unknown external provider: $provider" >&2
      exit 2
      ;;
  esac

  local provider_work="$work/$provider"
  local base="https://github.com/$repository/releases/download/$release"
  mkdir -p "$provider_work"
  curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
    "$base/$asset" --output "$provider_work/$asset"
  curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
    "$base/$asset.sha256" --output "$provider_work/$asset.sha256"

  local published actual size
  published=$(awk 'NF == 2 { print $1 ":" $2 }' "$provider_work/$asset.sha256")
  [[ "$published" == "$expected_sha:$asset" ]] || {
    echo "error: $repository $release published an unexpected checksum sidecar" >&2
    exit 1
  }
  actual=$(digest_of "$provider_work/$asset")
  [[ "$actual" == "$expected_sha" ]] || {
    echo "error: $asset digest mismatch: expected $expected_sha, got $actual" >&2
    exit 1
  }
  size=$(wc -c <"$provider_work/$asset" | tr -d '[:space:]')
  [[ "$size" == "$expected_size" ]] || {
    echo "error: $asset size mismatch: expected $expected_size, got $size" >&2
    exit 1
  }
  if [[ "$verify_attestations" == 1 ]]; then
    gh attestation verify "$provider_work/$asset" \
      --repo "$repository" \
      --predicate-type https://slsa.dev/provenance/v1 \
      --signer-workflow "$signer" \
      --source-ref "$source_ref" \
      --source-digest "$source_digest" >/dev/null
  fi

  printf 'verified %s %s: %s bytes, sha256 %s\n' \
    "$repository" "$release" "$size" "$expected_sha"
}

publish_provider() {
  local provider=$1
  local asset
  case "$provider" in
    jsonplaceholder) asset=jsonplaceholder-provider.wasm ;;
    memory-chat) asset=memory-chat-provider.wasm ;;
  esac

  local component_temp sidecar_temp
  component_temp=$(mktemp "$destination/.$asset.XXXXXX")
  publish_temps+=("$component_temp")
  sidecar_temp=$(mktemp "$destination/.$asset.sha256.XXXXXX")
  publish_temps+=("$sidecar_temp")
  install -m 0644 "$work/$provider/$asset" "$component_temp"
  install -m 0644 "$work/$provider/$asset.sha256" "$sidecar_temp"
  mv -f -- "$sidecar_temp" "$destination/$asset.sha256"
  mv -f -- "$component_temp" "$destination/$asset"
}

# Do not change the destination until every requested asset has downloaded and verified.
for provider in "${providers[@]}"; do
  fetch_provider "$provider"
done
mkdir -p "$destination"
for provider in "${providers[@]}"; do
  publish_provider "$provider"
done
