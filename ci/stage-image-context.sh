#!/usr/bin/env bash
# Stage a minimal build context for the Dekopon container image, from a published release.
#
# The image needs exactly three things: the executables a release already published, the
# checked-in provider components, and the two licences. Everything else here — twenty-one crates,
# a Cargo target directory that reaches tens of gigabytes, documentation, examples — is not a
# build input. Excluding all of it with a `.dockerignore` would be correct only for as long as
# every file added to the repository afterwards stays matched by it, which is a standing
# obligation nobody will remember and a silent failure when it lapses. So the context is
# constructed instead: this script copies in what the image needs, asserts the result is exactly
# that, and nothing else can arrive because nothing else was put there.
#
# It is also the only implementation of the fetch-and-verify path. The workflow runs this script
# and a human runs the same script; two implementations would drift, and the local one is the one
# nobody would run. Verification is not optional here, because reusing the release's binaries
# instead of compiling them is the whole point of the image.
#
# Requires: gh (authenticated), tar, and either sha256sum or shasum.
#
#   work=$(mktemp -d)
#   ci/stage-image-context.sh v0.3.0 "$work"
#   docker buildx build --platform linux/arm64 --load -t dekopon:local "$work/context"
#
# Produces:
#   <work>/archives/        the release archives and their published .sha256 sidecars
#   <work>/context/         the build context: Dockerfile, dist/<arch>/<binary>, providers/,
#                           optional-providers/, LICENSE-APACHE, LICENSE-MIT — and nothing else
#   <work>/binaries.sha256  the eight staged executables, for the byte-identity check after the
#                           image is built
set -euo pipefail

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <release-tag> <work-directory>" >&2
  echo "example: $0 v0.3.0 \"\$(mktemp -d)\"" >&2
  exit 2
fi

tag="$1"
work="$2"
repository="${DEKOPON_REPOSITORY:-dekopon-agents/dekopon}"
source_dir=$(cd "$(dirname "$0")/.." && pwd)
archives="$work/archives"
context="$work/context"

# The runtime base is Debian 12 (glibc 2.36) while the release archives are built on ubuntu-24.04.
# Nothing in the release process knows about the image, so the constraint is checked here, before
# a build can bake in a binary that cannot start.
max_glibc="2.36"

binaries="dekopon dekopon-run dekopon-brokerd dekopond"
providers="echo gh http-probe jsonplaceholder"

# macOS ships shasum, Linux ships sha256sum, and their --check flags differ. Comparing the digests
# directly avoids the difference and produces a better message than either.
if command -v sha256sum >/dev/null 2>&1; then
  digest_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  digest_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  echo "error: neither sha256sum nor shasum is available" >&2
  exit 1
fi

command -v gh >/dev/null 2>&1 || {
  echo "error: gh is required to download and verify the release archives" >&2
  exit 1
}

# glibc_exceeds <have> <maximum> — true when the first version is newer than the second.
glibc_exceeds() {
  awk -v have="$1" -v maximum="$2" 'BEGIN {
    split(have, h, "."); split(maximum, m, ".")
    for (i = 1; i <= 3; i++) {
      hi = (h[i] == "" ? 0 : h[i] + 0)
      mi = (m[i] == "" ? 0 : m[i] + 0)
      if (hi > mi) exit 0
      if (hi < mi) exit 1
    }
    exit 1
  }'
}

rm -rf "$archives" "$context"
mkdir -p "$archives" "$context/dist" "$context/providers" "$context/optional-providers"

# Derive the Linux targets from the release rather than assuming a fixed set: a target added or
# dropped upstream changes the release, and this follows it.
echo "==> downloading $tag from $repository"
gh release download "$tag" --repo "$repository" --dir "$archives" --clobber \
  --pattern '*-unknown-linux-gnu.tar.gz' \
  --pattern '*-unknown-linux-gnu.tar.gz.sha256'

for archive in "$archives"/*-unknown-linux-gnu.tar.gz; do
  name=$(basename "$archive")
  base="${name%.tar.gz}"
  case "$base" in
    *-x86_64-unknown-linux-gnu) arch=amd64 ;;
    *-aarch64-unknown-linux-gnu) arch=arm64 ;;
    *)
      echo "error: $name is not a Linux target this image publishes" >&2
      exit 1
      ;;
  esac

  published=$(cut -d' ' -f1 < "$archive.sha256")
  actual=$(digest_of "$archive")
  if [ "$published" != "$actual" ]; then
    echo "error: $name is $actual; the release published $published" >&2
    exit 1
  fi
  # The archives carry a release attestation. An image built from unverified bytes would give up
  # the only reason to reuse them.
  gh attestation verify "$archive" --repo "$repository" >/dev/null
  echo "==> verified $name (sha256 and attestation) -> dist/$arch"

  mkdir -p "$context/dist/$arch"
  for binary in $binaries; do
    tar -xzf "$archive" -C "$context/dist/$arch" --strip-components=1 "$base/$binary"
  done
  chmod 0755 "$context/dist/$arch"/*
done

for provider in $providers; do
  component="$source_dir/examples/providers/$provider-provider.wasm"
  if [ ! -f "$component" ]; then
    echo "error: $component is missing" >&2
    exit 1
  fi
  cp "$component" "$context/providers/"
done
# Normalised rather than inherited: the Dockerfile deliberately copies the components without
# --chmod, so whatever mode they have here is the mode dekopon-brokerd will check in the image.
chmod 0644 "$context/providers"/*.wasm
# Durable memory is shipped but never joins the default scan directory. An operator must name this
# exact file or explicitly scan the optional directory.
cp "$source_dir/examples/providers/memory-chat-provider.wasm" \
  "$context/optional-providers/memory-chat-provider.wasm"
chmod 0644 "$context/optional-providers/memory-chat-provider.wasm"

cp "$source_dir/Dockerfile" "$context/Dockerfile"
cp "$source_dir/LICENSE-APACHE" "$source_dir/LICENSE-MIT" "$context/"
chmod 0644 "$context/Dockerfile" "$context/LICENSE-APACHE" "$context/LICENSE-MIT"

# The allowlist, asserted rather than described. Anything unexpected in the context — a target
# missing from the release, a component renamed, a stray file — fails here instead of reaching a
# layer or slowing every build.
expected=$(
  {
    echo "Dockerfile"
    echo "LICENSE-APACHE"
    echo "LICENSE-MIT"
    for arch in amd64 arm64; do
      for binary in $binaries; do echo "dist/$arch/$binary"; done
    done
    for provider in $providers; do echo "providers/$provider-provider.wasm"; done
    echo "optional-providers/memory-chat-provider.wasm"
  } | sort
)
staged=$(cd "$context" && find . -type f | sed 's|^\./||' | sort)
if [ "$expected" != "$staged" ]; then
  echo "error: staged context does not match the expected file set" >&2
  diff <(echo "$expected") <(echo "$staged") >&2 || true
  exit 1
fi

: > "$work/binaries.sha256"
for arch in amd64 arm64; do
  for binary in $binaries; do
    printf '%s  %s\n' "$(digest_of "$context/dist/$arch/$binary")" "dist/$arch/$binary" \
      >> "$work/binaries.sha256"
  done
done

for staged_binary in "$context"/dist/*/*; do
  # Every distinct symbol version, compared numerically. Sorting the strings and taking the last
  # would call GLIBC_2.9 newer than GLIBC_2.34 and let a real breach through.
  highest=""
  while read -r symbol; do
    version="${symbol#GLIBC_}"
    if [ -z "$highest" ] || glibc_exceeds "$version" "$highest"; then
      highest="$version"
    fi
  done < <(grep -ao 'GLIBC_2\.[0-9][0-9]*' "$staged_binary" | sort -u)
  if [ -n "$highest" ] && glibc_exceeds "$highest" "$max_glibc"; then
    echo "error: $staged_binary needs glibc $highest; the runtime base provides $max_glibc." >&2
    echo "       Move the runtime base to a newer Debian in the Dockerfile." >&2
    exit 1
  fi
done

echo "==> every binary needs at most glibc $max_glibc"
echo "==> staged context ($context):"
(
  cd "$context"
  find . -type f | sed 's|^\./||' | sort | while read -r file; do
    printf '    %10s  %s\n' "$(wc -c < "$file" | tr -d ' ')" "$file"
  done
)
echo "==> staged executables:"
sed 's/^/    /' "$work/binaries.sha256"
