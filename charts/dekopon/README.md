# `charts/dekopon`

A Helm chart that runs `dekopon-brokerd` and `dekopond` as one pod on a single-node arm64 k3s
cluster.

**Status: current, but unapplied.** Every claim below about rendered YAML, file ownership, and file
modes was verified locally. Nothing in this chart has been installed on a cluster, and no released
container image exists for it to pull yet — see [Image and appVersion](#image-and-appversion).

Read [`../../crates/dekopon-brokerd/README.md`](../../crates/dekopon-brokerd/README.md) and
[`../../docs/dekopond.md`](../../docs/dekopond.md) first. The chart places files and sets
permissions; it does not define or validate their contents, and the two daemons' own documentation
is the only description of what goes in them.

## What it deploys

One `Deployment`, `replicas: 1`, `strategy: Recreate`, no `Service` and no `Ingress` — neither
daemon serves HTTP or binds a TCP port, so there is nothing to expose.

| Container | Kind | Runs |
|---|---|---|
| `prepare-files` | init, runs to completion, **root** | `busybox`, copies configuration into an `emptyDir` with the right owner and mode |
| `broker` | native sidecar (`restartPolicy: Always`) when the gateway is enabled, otherwise the pod's only regular container | `dekopon-brokerd --config /etc/dekopon/broker.yaml` |
| `gateway` | regular container, only when `gateway.enabled` | `dekopond --config /etc/dekopon/dekopond.yaml` |

Both daemons run as UID/GID `65532:65532` and share `/run/dekopon`, an in-memory `emptyDir` holding
the broker's `0600` Unix socket. That is the whole transport: the socket is owner-only, both ends
verify the other with `SO_PEERCRED`, and `dekopon-brokerd` refuses to start when a configured peer
UID is not its own euid, so one pod and one UID is not a simplification — it is the only shape the
broker accepts today.

The broker is a native sidecar rather than a second regular container because ordering matters in
both directions. `dekopond` probes the broker once at startup and exits non-zero when the socket
does not answer, so a plain second container would crash-loop its way to a working state; a sidecar
with a startup probe means Kubernetes does not start `dekopond` at all until the broker answers a
real request. Termination runs the other way — the gateway drains first, the broker second — which
is the order the audit chain wants. This needs Kubernetes 1.29 or newer, and `Chart.yaml` declares
`kubeVersion: ">=1.29.0-0"` so an older cluster refuses the install instead of deadlocking on an
init container that never exits.

## Why an init container, and not a volume mount

`dekopon-brokerd` validates every path it touches, in tiers, and refuses to serve when one is
wrong:

| Tier | Applies to | Rule |
|---|---|---|
| A | `broker-credentials.yaml`, the audit JSONL, the checkpoint, the checkpoint lock, every socket | rejected if `mode & 0o077 != 0` |
| B | `broker.yaml`, `policies.cedar`, `dekopond.yaml`, provider `.wasm` files and their parents | rejected if `mode & 0o022 != 0` |
| C | socket, audit and checkpoint parent directories | must be `0700` and owned by the runtime UID |
| D | every ancestor up to `/` | must be a directory that is not group- or world-writable unless sticky |
| E | `catalogPath` | no checks at all |

Tier A and B additionally require `uid == geteuid()`, `nlink == 1`, and an open with `O_NOFOLLOW`.

No Kubernetes volume can present a file that satisfies A or B:

| Mount shape | What the daemon sees | Result |
|---|---|---|
| `secret` / `configMap` volume | `key -> ..data/key -> ..2026_…/key` | `O_NOFOLLOW` returns `ELOOP` before any mode is examined |
| the same with `subPath` | a real file, `root:root`, `0644` | owner is not the daemon's euid; A also fails on `0o044` |
| any of the above plus `fsGroup` | `65532:2000`, `0640` | passes B, fails A — `0o040` is exactly what `mode & 0o077` rejects |

`fsGroup` is worth calling out on its own, because it is the reflex fix for "the pod cannot read its
volume" and here it is the thing that breaks the credentials file specifically. It is deliberately
absent from `podSecurityContext` and adding it will produce a broker that starts and then refuses.

So the chart mounts nothing the daemons read. A `projected` volume gathers every source into
`/dekopon-source`, visible only to the init container, and the init container copies:

```sh
install -m 0600 -o 65532 -g 65532 /dekopon-source/broker.yaml /etc/dekopon/broker.yaml
```

which produces a fresh regular file, one link, mode `0600`, owned by `65532` — a shape that
satisfies A and B at once. Losing update propagation costs nothing: each of these files is read
once, at startup, and the chart's `checksum/config` annotation rolls the pod when an inline value
changes. It cannot see into an `existingSecret`, so roll the pod yourself after editing one.

The init container is the only thing in the chart that runs as root, and it holds `CHOWN` and
`FOWNER` and nothing else. `CHOWN` is what lets it hand a directory to `65532`; `FOWNER` is what
lets it `chmod` a directory it no longer owns, on a restart. It deliberately does **not** hold
`DAC_OVERRIDE`: instead it reclaims the directories to `root` first, which is also why it is
idempotent across an in-place pod restart where the `emptyDir` still holds the previous run's
`0700` directory. It asserts its own output with `stat` before exiting, so a wrong file is an init
failure naming that file rather than a broker that starts and then refuses to serve.

### What does not need any of this

- **Slack and model credentials.** `dekopond.yaml` names environment variable *names*, never values,
  so those are ordinary `secretKeyRef` entries under `gateway.env` with no file hygiene at all.
- **`OTEL_EXPORTER_OTLP_HEADERS`.** The broker's `telemetry` block has no credential field by
  design; the OpenTelemetry SDK reads ingest auth from that variable, so a token never enters
  `broker.yaml`.
- **The agent catalog.** Tier E. `dekopond` reads `catalogPath` with a plain `read_to_string`, so a
  ConfigMap volume mounted straight at `paths.catalogDir` is fine and nothing is copied.
- **Provider components.** The image already bakes `/opt/dekopon/providers/*.wasm` owned by
  `65532:65532` under a `65532`-owned `0755` directory, which is what Tier B wants.

## The UID is not a preference

`podSecurityContext.runAsUser` is `65532` because the image bakes the provider components under that
UID and `validate_owned_file` compares a provider's owner against the broker's own euid. Any other
value makes every provider fail to load, which fails startup. The chart refuses to render when
`runAsUser` is changed while `image.repository` is still the stock image.

## Paths the chart owns

Your `broker.yaml` and `dekopond.yaml` must name files inside these directories. The chart places
files; it does not rewrite configuration.

| File | Path | Tier | Written by |
|---|---|---|---|
| `broker.yaml` | `/etc/dekopon/broker.yaml` | B | init container |
| `policies.cedar` | `/etc/dekopon/policies.cedar` | B | init container |
| `broker-credentials.yaml` | `/etc/dekopon/broker-credentials.yaml` | A | init container |
| `dekopond.yaml` | `/etc/dekopon/dekopond.yaml` | B | init container |
| broker socket | `/run/dekopon/broker.sock` | A + C | the broker, at bind |
| audit chain | `/var/lib/dekopon/audit.jsonl` | A + C | the broker |
| checkpoint | `/var/lib/dekopon/audit-checkpoint.json` | A + C | the broker |
| checkpoint lock | `/var/lib/dekopon/audit-checkpoint.lock` | A + C | the broker |
| agent catalog | `/etc/dekopon-catalog/dekopon.yaml` | E | ConfigMap mount |
| providers | `/opt/dekopon/providers/*.wasm` | B | baked into the image |

`/etc/dekopon` and `/run/dekopon` are memory-backed `emptyDir`s, so the credentials file and the
socket never reach the node's disk. `/var/lib/dekopon` is the claim: audit, checkpoint and lock have
to be one directory on one volume, because the checkpoint stages to a same-directory temporary file
and renames it atomically.

## Probes

There is no HTTP health endpoint, and the image is distroless with no shell, so an `exec` probe can
only run one of the four binaries. Both probes run `dekopon-run broker capabilities`, which connects
over the real socket, passes `SO_PEERCRED` in both directions, and gets back the capability list
policy exposes to this peer. It is evaluated from the constraint catalog and the policy set and
appends **no audit record**, so probing does not consume the audit log's bounded record budget.

- **`startupProbe`**, 5 s period, 60 failures — five minutes. The broker compiles every `.wasm`
  component through Cranelift at every start and there is no compilation cache, and it binds the
  socket only after that work is done, so "the socket answers" is exactly "fully started". The
  margin is large because a startup probe that gives up restarts the container, and a restart loop
  against durable audit state is the worst thing this chart can produce.
- **`readinessProbe`**, 30 s period. It gates nothing — there is no Service — but it is free and it
  is the difference between `kubectl get pod` saying `1/1` and saying something true.
- **No `livenessProbe`.** Nothing routes traffic here, so a restart fixes nothing a human would not
  fix better, and it would kill a broker mid-invocation.
- **No gateway probes.** `dekopond` binds nothing and already probes the broker once at startup,
  exiting non-zero when it does not answer. "Is it healthy" and "is the process running" are the
  same question.

One consequence worth knowing: the probe runs `dekopon-run`, which reads
`OTEL_EXPORTER_OTLP_ENDPOINT` from its environment. Do not set that variable on the broker
container, or every probe exports a trace. `OTEL_EXPORTER_OTLP_HEADERS` is safe and is what the
broker's own telemetry block needs.

## Sizing the audit volume

`local-path` is the target cluster's only StorageClass, it is RWO, and **`ALLOWVOLUMEEXPANSION` is
`false`**. `state.size` is final for the life of the volume.

The audit log does not rotate. At `auditMaxRecords` it returns `AuditError::Full` and refuses
further appends; on open it refuses outright. So the file's size is bounded, and the arithmetic
is:

| Quantity | Value |
|---|---|
| `auditMaxRecords` default | 200 000 |
| `auditMaxLineBytes` default | 64 KiB |
| Absolute ceiling at stock limits | 200 000 × 64 KiB = **12.2 GiB** |
| A measured `external-write` record with two HTTP calls, full-length hashes, compact JSONL | **1 279 bytes** |
| A full log of records that size | 200 000 × 1 279 B ≈ **244 MiB** |

12.2 GiB is not a volume you fund on an 8 GB Raspberry Pi, and 244 MiB has no margin for a chattier
record. `state.size` defaults to **2Gi**, which is ~10.7 KiB per record at the record cap — eight
times the measured record — so the daemon's own bound, not the disk, is what stops the broker.

The chart's default `broker.yaml` also sets `auditMaxLineBytes: 8192`, which pulls the absolute
ceiling to 200 000 × 8 KiB = 1.53 GiB, strictly inside a 2Gi volume. That is the point: you want the
record cap to bind before the filesystem does, because `AuditError::Full` is a clean designed
refusal and `ENOSPC` in the middle of an append is not. If you raise `auditMaxLineBytes`, raise
`state.size` in the same change — and remember you cannot raise it after the volume exists.

`serverLimits` is all-or-nothing: when the section is present every field is required.

## Storage, uninstall, and recovery

The claim carries `helm.sh/resource-policy: keep`, so `helm uninstall` leaves it. This is not
politeness. A non-empty audit with a missing checkpoint, or a checkpoint that is not an exact prefix
of the verified chain, makes `dekopon-brokerd` fail closed and demand explicit operator recovery;
deleting one of the two files to dodge that is precisely the thing the design refuses, and deleting
both together is a rollback local state cannot detect. Move the volume deliberately, with both files,
or not at all.

`state.existingClaim` points the pod at a claim you manage. The init container still takes its root
to `65532:0700`.

## seccomp

`RuntimeDefault`, on the pod and on the init container. Wasmtime JITs through Cranelift and needs
`mmap` with `PROT_EXEC`, `mprotect`, `memfd_create`, and `SIGSEGV`/`SIGBUS` handlers for guard-page
traps. `RuntimeDefault` permits all of that, and a provider has been proven to load under Docker's
equivalent default profile. Do not narrow this to a hand-written profile without proving a component
still loads; the failure mode is a trap inside the JIT, not a clean error.

`readOnlyRootFilesystem` is `true` for every container. Neither daemon writes outside its mounted
volumes, and a memory-backed `/tmp` is mounted anyway so an incidental temporary file cannot turn
into a crash. One exception: the `chatgptSubscription` model kind reads — and may refresh — a device
credential file, and this chart provisions no writable location for it. Mount your own if you use it.

## Image and appVersion

`appVersion` is `0.4.0`, which is the **first release that will have an image**, not the latest
release that exists. `v0.3.0` predates the container-image workflow and no image was ever published
for it. Setting `appVersion: 0.3.0` would ship a chart whose default pulls nothing.

The workflow publishes under the Git tag, so the tag carries a `v`. An empty `image.tag` therefore
renders `v` + `appVersion`:

```
ghcr.io/dekopon-agents/dekopon:v0.4.0
```

There is no `latest`. Prefer `image.digest` once a release exists; it pins across the
`linux/amd64` + `linux/arm64` index, and the index digest is what `gh attestation verify` attests.

## Configuration values

Each of the four operator-supplied files is either inline, in which case the chart writes a Secret,
or a reference to an object that already exists. Setting both is an error.

| Value | Inline | Existing object |
|---|---|---|
| `broker.yaml` | `broker.config.inline` | `broker.config.existingSecret` / `.existingSecretKey` |
| `policies.cedar` | `broker.policies.inline` | `broker.policies.existingSecret` / `.existingSecretKey` |
| `broker-credentials.yaml` | `broker.credentials.inline` | `broker.credentials.existingSecret` / `.existingSecretKey` |
| `dekopond.yaml` | `gateway.config.inline` | `gateway.config.existingSecret` / `.existingSecretKey` |
| agent catalog | `gateway.catalog.inline` | `gateway.catalog.existingConfigMap` / `.existingConfigMapKey` |

Use `existingSecret` for credentials. An inline value is stored in the release, returned by
`helm get values`, and usually committed.

The chart refuses to render, with a message, when: `runAsUser` is changed while the stock image is
selected; a required file has no source; both sources are set for one file; an inline `broker.yaml`
names `policiesPath`, `credentialsPath`, or `constraintSets` with no corresponding value supplied;
or `paths.catalogDir` is inside `paths.configDir`. Every one of those is a mistake whose only other
symptom is a pod that starts and never becomes ready.

The chart's default `broker.config.inline` is the echo example from the broker's own README, moved
onto these paths: a real deny-by-default configuration that starts, loads the baked
`echo-provider.wasm`, and authorizes exactly one read-only capability for the pod's own UID. It is
there so you can install the chart and watch a broker become ready before you give it anything that
matters. Replace it. `gateway.enabled` is `false` by default because a gateway needs a chat token, a
model endpoint, and an agent catalog, and the chart can invent none of them.

## Install

```console
helm lint charts/dekopon
helm template dekopon charts/dekopon
helm template dekopon charts/dekopon -f charts/dekopon/ci/rubber-stamper-values.yaml
helm upgrade --install dekopon charts/dekopon -n dekopon --create-namespace \
  -f my-values.yaml
```

[`ci/rubber-stamper-values.yaml`](ci/rubber-stamper-values.yaml) is the
[rubber-stamper](../../examples/rubber-stamper/README.md) deployment expressed as chart values:
Slack in, one agent, two `gh` capabilities, a broker-injected token by reference, and the audit
chain on its own volume.

## What is not proven

- Nothing has been applied to a cluster. The chart has been linted, rendered, and schema-validated
  with `kubeconform` against Kubernetes 1.33, and the init container's rendered command has been run
  verbatim in a `linux/arm64` container under its rendered `securityContext` against a fixture built
  to match a projected volume's symlink layout, but no `kubectl apply` has happened.
- No image exists to pull yet. The daemons have never been started from this configuration.
- The `PodSecurity` `restricted` profile would reject this pod: the init container runs as root.
  `baseline` is fine.
