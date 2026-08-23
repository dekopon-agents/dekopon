# Dekopon container image: one image, all four binaries, assembled from a published release.
#
# Nothing is compiled here. `release.yml` already builds, checksums, and provenance-attests
# `dekopon-<version>-<target>.tar.gz` for `x86_64-unknown-linux-gnu` and
# `aarch64-unknown-linux-gnu`, each carrying all four executables. The image ships exactly those
# bytes — the ones users download and can verify — rather than a second, independently compiled
# set that merely ought to match. `ci/stage-image-context.sh` verifies each archive against its
# `.sha256` sidecar and its attestation before extracting the executables into the context, and
# both the workflow and a human run that same script. Fetching stays out of this file on purpose:
# the build needs no network, the verification is auditable in the log, and the image is exactly
# what was staged.
#
# Every instruction is a COPY, so BuildKit assembles both platforms on one runner with no
# emulation and no per-architecture build.
#
# `dekopon-brokerd` and `dekopond` are separate processes but not separate deployments: the broker
# socket is `0600`, authenticates its peer with `SO_PEERCRED`, and has no TCP transport, so a
# gateway can only reach it through a shared filesystem namespace — in Kubernetes, a shared pod.
# One image whose `command` selects the binary is what that deployment needs.
#
# This file expects a staged context and is not buildable from the repository root. The context is
# constructed by `ci/stage-image-context.sh` rather than filtered out of a checkout: it contains
# the Dockerfile, `dist/<arch>/` with the four executables from each release archive, `providers/`
# plus `optional-providers/` with checked-in components, and the two licences — nothing else, because nothing else was
# put there. A `.dockerignore` denylist would have to keep excluding the rest of the repository
# correctly forever; an allowlist is true by construction.
#
#   work=$(mktemp -d)
#   ci/stage-image-context.sh v0.3.0 "$work"
#   docker buildx build --platform linux/arm64 --load -t dekopon:local "$work/context"
#
# Distroless `cc` carries glibc, libgcc, and libstdc++ and nothing else — no shell, no package
# manager. Glibc rather than musl because the release targets are `*-unknown-linux-gnu`. No CA
# bundle is needed either: `reqwest` and `ureq` use rustls with compiled-in webpki roots.
#
# The release archives are built on `ubuntu-24.04`, whose glibc is newer than the runtime base's,
# so the runtime base is a constraint the release does not know about. The staging script asserts
# that no binary requires a symbol newer than what this base provides before staging it. Debian 12
# (glibc 2.36) held through v0.10.0; the console's `dekopon` binary in v0.11.0 references
# `pidfd_spawnp`/`pidfd_getpid`, which Rust's std probes as weak symbols at GLIBC_2.39 and falls
# back from cleanly at runtime — but glibc's dynamic linker refuses to load a binary naming a
# version node the runtime library lacks at all, weak reference or not, so the weak binding does
# not exempt it from this floor. Debian 13 (glibc 2.41) covers it with room to spare.
FROM gcr.io/distroless/cc-debian13:nonroot@sha256:a77defd6fedbb3392b175ba8ea3d1c22be963c1597c248c3ba987ddd80bfb512

# BuildKit sets this per requested platform. It is the only thing that differs between the two.
ARG TARGETARCH

COPY --chmod=0755 \
     dist/${TARGETARCH}/dekopon \
     dist/${TARGETARCH}/dekopon-run \
     dist/${TARGETARCH}/dekopon-brokerd \
     dist/${TARGETARCH}/dekopond \
     /usr/local/bin/

# Provider components come from the tagged checkout rather than the archive: they are checked-in
# artifacts, not build outputs, and the archive ships only `jsonplaceholder`. They are copied
# verbatim and never regenerated. Durable memory is copied separately under `optional-providers`;
# it never joins the default scan path.
#
# `dekopon-brokerd` refuses to load a provider whose file is not owned by its own euid, is group-
# or world-writable, or has more than one link, and it stats with `symlink_metadata`, so a symlink
# is rejected outright (crates/dekopon-brokerd/src/socket.rs). It applies the owner and
# writability rule to the containing directory too, hence `--chown`. There is deliberately no
# `--chmod`: BuildKit would then apply it to the directories it creates as well, and `0644`
# directories cannot be traversed. Without it the components keep the mode they carry in the
# context, which the staging script normalises to 0644.
COPY --chown=65532:65532 \
     providers/echo-provider.wasm \
     providers/gh-provider.wasm \
     providers/http-probe-provider.wasm \
     providers/jsonplaceholder-provider.wasm \
     /opt/dekopon/providers/

COPY --chown=65532:65532 \
     optional-providers/memory-chat-provider.wasm \
     /opt/dekopon/optional-providers/

COPY LICENSE-APACHE LICENSE-MIT /usr/share/doc/dekopon/

# Numeric, not `nonroot`: Kubernetes `runAsNonRoot` compares a UID and cannot resolve a name.
USER 65532:65532
WORKDIR /home/nonroot

# No ENTRYPOINT on purpose. The command selects the binary, so `docker run <image> dekopond
# --config ...` and a Kubernetes `command: ["dekopon-brokerd"]` both work without `--entrypoint`.
CMD ["dekopon", "--help"]

LABEL org.opencontainers.image.title="dekopon" \
      org.opencontainers.image.description="Dekopon operator CLI, runner, capability broker, and chat gateway" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.source="https://github.com/dekopon-agents/dekopon" \
      org.opencontainers.image.url="https://github.com/dekopon-agents/dekopon" \
      org.opencontainers.image.documentation="https://github.com/dekopon-agents/dekopon/blob/main/docs/container-image.md"
