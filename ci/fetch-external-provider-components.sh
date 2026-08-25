#!/usr/bin/env bash
# Fetch exact standalone provider v0.1.0 release assets for tests, packaging, or image staging.
# Source and generated Wasm are intentionally not tracked in the Dekopon core repository.
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 DESTINATION [echo|jsonplaceholder|memory-chat ...]" >&2
  exit 2
fi

destination=$1
shift
providers=("$@")
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)
tracked=$(git -C "$root" ls-files -- \
  examples/providers/echo examples/providers/echo-provider.wasm \
  examples/providers/echo-provider.wasm.sha256 \
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
  providers=(echo jsonplaceholder memory-chat)
fi

seen=' '
for provider in "${providers[@]}"; do
  case "$provider" in
    echo|jsonplaceholder|memory-chat) ;;
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
  local repository asset expected_sha expected_size signer source_ref source_digest
  case "$provider" in
    echo)
      repository=dekopon-agents/dekopon-provider-echo
      asset=echo-provider.wasm
      expected_sha=c15e88cf50726e8a80d1f73f8167563242d59ea80c1af026014e30054ac786b1
      expected_size=150036
      signer="$repository/.github/workflows/recover-v0.1.0.yml"
      source_ref=refs/heads/main
      source_digest=71efdf591285e4d9349e59e6fd62d7f9752696d9
      ;;
    jsonplaceholder)
      repository=dekopon-agents/dekopon-provider-jsonplaceholder
      asset=jsonplaceholder-provider.wasm
      expected_sha=9562744e6c209a447cafcfe09d11a50ea1926945a4b52099714c7328c2fd5e5d
      expected_size=277153
      signer="$repository/.github/workflows/release.yml"
      source_ref=refs/tags/v0.1.0
      source_digest=dc925dd23240d2dbd3bd9c534347fd33552bbdf6
      ;;
    memory-chat)
      repository=dekopon-agents/dekopon-provider-memory-chat
      asset=memory-chat-provider.wasm
      expected_sha=65f82d6a422b0500269333b79be06c4155d7793df1f80ced12a8b214acb53a6b
      expected_size=248638
      signer="$repository/.github/workflows/release.yml"
      source_ref=refs/tags/v0.1.0
      source_digest=564abc55c8e01657ddb0e10938b9f62101e558ae
      ;;
    *)
      echo "error: unknown external provider: $provider" >&2
      exit 2
      ;;
  esac

  local provider_work="$work/$provider"
  local base="https://github.com/$repository/releases/download/v0.1.0"
  mkdir -p "$provider_work"
  curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
    "$base/$asset" --output "$provider_work/$asset"
  curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
    "$base/$asset.sha256" --output "$provider_work/$asset.sha256"

  local published actual size
  published=$(awk 'NF == 2 { print $1 ":" $2 }' "$provider_work/$asset.sha256")
  [[ "$published" == "$expected_sha:$asset" ]] || {
    echo "error: $repository v0.1.0 published an unexpected checksum sidecar" >&2
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

  printf 'verified %s v0.1.0: %s bytes, sha256 %s\n' \
    "$repository" "$size" "$expected_sha"
}

publish_provider() {
  local provider=$1
  local asset
  case "$provider" in
    echo) asset=echo-provider.wasm ;;
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
