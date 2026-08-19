# Container image

Read [`design.md`](design.md) before this document. Packaging changes distribution, not authority:
the image ships no configuration, no policy, no credentials, and no audit state, and both daemons
still read owner-owned files that the deployment provides.

**Status: current.** [`../Dockerfile`](../Dockerfile) and
[`../.github/workflows/container-image.yml`](../.github/workflows/container-image.yml) are in the
repository and build locally today against the `v0.3.0` archives. Publication runs when a release
is published, and `v0.4.0` is the first release it runs for; `v0.3.0` predates the workflow and has
no image.

## What it is

One image, `ghcr.io/dekopon-agents/dekopon`, carrying all four binaries for `linux/amd64` and
`linux/arm64`. It is a separate GHCR package from the published WIT interface packages
`dekopon/provider` and `dekopon/http`, which are OCI artifacts rather than images.

One image rather than four is a deployment fact, not a convenience. `dekopon-brokerd` binds a
`0600` Unix socket and authenticates its peer with `SO_PEERCRED`; there is no TCP transport. A
gateway can therefore only reach a broker through a shared filesystem namespace — in Kubernetes, a
shared pod. Two containers in one pod running two images that must be version-locked buys nothing
that one image with two `command`s does not.

| Path | Contents |
|---|---|
| `/usr/local/bin/dekopon` | Operator CLI |
| `/usr/local/bin/dekopon-run` | Direct runner and broker client |
| `/usr/local/bin/dekopon-brokerd` | Authenticated local capability broker |
| `/usr/local/bin/dekopond` | Unprivileged chat gateway |
| `/opt/dekopon/providers/*.wasm` | The four checked-in provider components, copied verbatim |
| `/usr/share/doc/dekopon/` | `LICENSE-APACHE`, `LICENSE-MIT` |

The image runs as UID/GID `65532:65532` — numeric, because Kubernetes `runAsNonRoot` compares a UID
and cannot resolve a name. There is no shell, no package manager, and no `ENTRYPOINT`: the command
selects the binary.

## The binaries are the release's binaries

Nothing is compiled to build this image. `release.yml` builds `dekopon-<version>-<target>.tar.gz`
for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`, publishes a `.sha256` sidecar for
each, and attests them with `actions/attest-build-provenance`. The image contains exactly those
executables.

That is the whole point of the design. A second, independently compiled set of binaries would be
artifacts nobody has verified, produced by a toolchain that can drift away from the one that built
the archives users download, at the cost of a native build matrix. Reusing the release's output
means the image and the tarball are the same bytes, and `sha256sum` proves it.

[`../ci/stage-image-context.sh`](../ci/stage-image-context.sh) verifies before it trusts: it
downloads each Linux archive, checks it against its published `.sha256`, runs `gh attestation
verify --repo dekopon-agents/dekopon` on it, and only then extracts the four executables into the
build context. The workflow runs that script and so does a human building locally — two
implementations of a verification path would drift, and the local one is the one nobody would
run. The `Dockerfile` performs no network access at all.

After building, and before anything is pushed, the workflow extracts each binary back out of both
platform images and compares its SHA-256 against the archive it came from. Eight comparisons, all
of which must match.

The runtime base is Debian 12 (glibc 2.36) while the release archives are built on `ubuntu-24.04`,
which is newer. Nothing in the release process knows about the image, so the staging script refuses
to stage a binary that requires a glibc symbol newer than 2.36. Today the highest any of them
requires is 2.34.

## The build context is constructed, not filtered

The image needs three things: the release executables, the checked-in provider components, and the
two licences. The staging script copies exactly those into a scratch directory alongside the
`Dockerfile`, asserts that the result is precisely that fifteen-file set, and builds from there.

The alternative — keeping the whole repository as the context and excluding the rest with a
`.dockerignore` — is correct only for as long as every file added later stays matched by it. That
is a standing obligation nobody remembers, and the failure is silent: a new directory quietly joins
the context, slows every build, and could reach a layer. An allowlist is true by construction.

The `Dockerfile` therefore cannot be built from the repository root, and the root `.dockerignore`
is a single `*` so that trying fails in about a second with a missing `COPY` source rather than
after uploading a Cargo target directory. It excludes everything unconditionally, so it never needs
updating.

## Baked provider components

`examples/providers/*.wasm` come from the tagged checkout rather than from the archive: they are
checked-in artifacts, not build outputs, and the archive ships only `jsonplaceholder`. They are
copied verbatim and never regenerated.

`dekopon-brokerd` refuses a provider path that is not a regular file owned by its own euid, that is
group- or world-writable, or that has more than one link, and it applies the owner and writability
rule to the containing directory as well (`crates/dekopon-brokerd/src/socket.rs`). It stats with
`symlink_metadata`, so a symlink to a valid file is still refused. The image therefore ships:

- `/opt/dekopon/providers` owned by `65532:65532`, mode `0755`
- each `.wasm` owned by `65532:65532`, mode `0644`, one link, not a symlink

The `COPY` that places them uses `--chown` and deliberately no `--chmod`, because BuildKit applies
`--chmod` to the directories it creates as well and a `0644` directory cannot be traversed. The
components therefore keep the mode they carry in the staged context, which the staging script
normalises to `0644`.

Only `echo-provider.wasm` loads on the direct runner. The other three import
`dekopon:http/client@1.0.0`, the immediate linker is empty by design, and `dekopon-run inspect`
therefore refuses to instantiate them; they are broker-only components. That is the documented
boundary in [`run.md`](run.md), not a packaging defect.

A provider mounted from a volume instead has to satisfy the same rules; a `configMap` or `secret`
mount will not, because those are symlink farms.

## Run a binary

```console
docker run --rm ghcr.io/dekopon-agents/dekopon:<VERSION> dekopon version
docker run --rm ghcr.io/dekopon-agents/dekopon:<VERSION> dekopon-run --version
docker run --rm ghcr.io/dekopon-agents/dekopon:<VERSION> dekopon-run invoke \
  --provider /opt/dekopon/providers/echo-provider.wasm echo.echo --input '{}'
docker run --rm -v /path/to/catalog:/etc/dekopon:ro \
  ghcr.io/dekopon-agents/dekopon:<VERSION> dekopon --config /etc/dekopon/dekopon.yaml validate
```

In Kubernetes the same selection is `command: ["dekopon-brokerd"]` or `command: ["dekopond"]` with
`args` carrying `--config`.

## What the image does not contain

- No broker, gateway, or catalog configuration, and no Cedar policy. Every deployment supplies its
  own owner-owned files.
- No credentials. `dekopon-brokerd` reads a credentials file; `dekopond` reads environment variables
  the deployment sets. Neither is baked.
- No socket, audit log, or checkpoint. Those are runtime state on a writable volume.
- No system CA store dependency. `reqwest` and `ureq` use rustls with compiled-in webpki roots, so
  outbound TLS does not consult `/etc/ssl`.

## Deployment notes

`dekopon-brokerd` validates its runtime directories at startup and refuses to serve if they are
wrong. The socket, audit, and checkpoint parents must be directories **owned by UID 65532 with mode
`0700`** — group or world access of any kind is refused, read included.

A bare `emptyDir` does not satisfy that: it is created root-owned and world-writable, and
`fsGroup` only changes the group and adds group access, which the check rejects for the opposite
reason. An init container running as root that creates the directory, `chown`s it to `65532:65532`,
and `chmod`s it to `0700` is the shape that works. [`../charts/dekopon/`](../charts/dekopon/README.md)
is that shape worked out in full — one pod, two containers, one UID — and is the intended consumer
of this image.

Configured peer UIDs must equal the broker's own UID — `65532` in this image — so a gateway sharing
the pod's UID is the configuration the broker accepts today. That single-UID trust domain is a
current limitation, recorded in [`security-model.md`](security-model.md), not something the image
changes.

## Publication

[`../.github/workflows/container-image.yml`](../.github/workflows/container-image.yml) is a
reusable workflow. [`release.yml`](../.github/workflows/release.yml) calls it as a job that `needs`
the job publishing the release, and passes that job's tag. It does not trigger on the `v*.*.*` tag
push, which would race the archives the image is made of, and it no longer triggers on
`release: published`, which cannot fire here at all: the release is created by `GITHUB_TOKEN`, and
GitHub does not start workflow runs from events raised by its own token. The `needs` edge is the
ordering guarantee — the release, its archives, its `.sha256` sidecars, and their attestations all
exist before this workflow starts. [`homebrew-tap.yml`](../.github/workflows/homebrew-tap.yml) is
called the same way. `workflow_dispatch` with a tag re-runs an existing release.

Because every instruction is a `COPY`, one runner assembles both platforms in a single build and
pushes a manifest list directly. There is no per-architecture matrix, no push-by-digest, no digest
hand-off, and no `imagetools` stitch — and therefore no reason to suppress buildx provenance, which
now emits provenance and an SBOM per platform. The workflow still asserts that the published tag
carries exactly `linux/amd64,linux/arm64`, reads the index digest back from the registry, requires
it to equal what the build pushed, and then attests it:

```console
gh attestation verify oci://ghcr.io/dekopon-agents/dekopon:<VERSION> \
  --repo dekopon-agents/dekopon
```

The publication job grants `artifact-metadata: write` so the attestation action can also link that
OCI digest from the organization's **Linked Artifacts** view. This records the already-published
subject; it does not grant another registry write path.

Only the release tag is published. There is no `latest`: release tags are immutable here, a moving
pointer would contradict that, and it would let a prerelease become the default pull.

Pull requests that touch the image inputs — the `Dockerfile`, the staging script, the workflow, a
licence, or a checked-in component — run every step above against the newest published release and
push nothing. That validates the part a pull request can actually break: the image's layout, the
provider ownership, and the byte-identity check.

## Build and check it locally

The image is assembled from a release, so stage one first. Any published release works; the
`Dockerfile` never cares which. This is the same script the workflow runs, with the same arguments.

```console
work=$(mktemp -d)
ci/stage-image-context.sh v0.3.0 "$work"
docker buildx build --platform linux/arm64 --load -t dekopon:local "$work/context"
docker run --rm dekopon:local dekopon version
docker run --rm dekopon:local dekopon-run invoke \
  --provider /opt/dekopon/providers/echo-provider.wasm echo.echo --input '{}'
```

The script prints what it staged and the digest of each executable, so the allowlist is visible
rather than asserted in prose:

```text
==> verified dekopon-0.3.0-aarch64-unknown-linux-gnu.tar.gz (sha256 and attestation) -> dist/arm64
==> every binary needs at most glibc 2.36
==> staged context (/tmp/tmp.AbC123/context):
          5057  Dockerfile
         10847  LICENSE-APACHE
          1064  LICENSE-MIT
       4764024  dist/amd64/dekopon
       ...
        707070  providers/gh-provider.wasm
```

Both platforms build anywhere, because nothing executes during the build:

```console
docker buildx build --platform linux/amd64 --load -t dekopon:local-amd64 "$work/context"
```

Running the foreign one needs QEMU; inspecting it does not. The image has no shell, so ownership,
mode, and content are read from outside it — which is also how the byte-identity check works:

```console
docker export "$(docker create dekopon:local unused)" > rootfs.tar
tar -tvf rootfs.tar opt/dekopon/providers
tar -xOf rootfs.tar usr/local/bin/dekopon | sha256sum
sha256sum "$work/context/dist/arm64/dekopon"
```

The last two must print the same digest. That is the assertion the whole design rests on, and
`$work/binaries.sha256` records all eight so the workflow can make it after the build.
