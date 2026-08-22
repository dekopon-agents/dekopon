# `charts/dekopon`

A Helm chart that runs `dekopon-brokerd` and `dekopond` as one pod on a single-node arm64 k3s
cluster.

**Status: published, but never applied to a cluster.** Those are two separate claims and only one of
them limits you.

*Published* is settled. `dekopon-chart-0.1.0` shipped the chart to
`oci://ghcr.io/dekopon-agents/charts/dekopon:0.1.0`, and application tags from `v0.4.0` onward
publish the container image it pulls, so `helm install` from the registry has everything it needs.
The chart is consumed from ArgoCD by registry path, not by Git path, and both GHCR packages are
public. See [Two version numbers](#two-version-numbers) and [Publishing](#publishing).

*Never applied* is the real caveat. Every claim below about rendered YAML, file ownership, and file
modes was verified against `helm template` and the CI render checks, not against a running cluster.
Nothing here has been installed on a live Kubernetes API server, so treat the manifests as reviewed
rather than as field-proven.

Read [`crates/dekopon-brokerd/README.md`](https://github.com/dekopon-agents/dekopon/blob/main/crates/dekopon-brokerd/README.md) and
[`docs/dekopond.md`](https://github.com/dekopon-agents/dekopon/blob/main/docs/dekopond.md) first. The chart places files and sets
permissions; it does not define or validate their contents, and the two daemons' own documentation
is the only description of what goes in them.

## What it deploys

One `Deployment`, `replicas: 1`, `strategy: Recreate`, and no chart-owned `Ingress`. An opt-in
ClusterIP `Service` exposes only a configured gateway webhook port for operator-owned exact-path
routing; it is disabled by default. The broker never receives a TCP surface.

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

- **Chat-service, chat-model, and optional image-model credentials.** `dekopond.yaml` names environment variable *names*, never values,
  so those are ordinary `secretKeyRef` entries under `gateway.env` with no file hygiene at all.
- **`OTEL_EXPORTER_OTLP_HEADERS`.** The broker's `telemetry` block has no credential field by
  design; the OpenTelemetry SDK reads ingest auth from that variable, so a token never enters
  `broker.yaml`.
- **The agent catalog.** Tier E. `dekopond` reads `catalogPath` with a plain `read_to_string`, so a
  ConfigMap volume mounted straight at `paths.catalogDir` is fine and nothing is copied.
- **Provider components.** The image already bakes `/opt/dekopon/providers/*.wasm` owned by
  `65532:65532` under a `65532`-owned `0755` directory, which is what Tier B wants.
- **The ChatGPT credential**, for the opposite reason from all of these. `load_credentials` is a
  plain `File::open` — no `O_NOFOLLOW`, no owner comparison, no mode check — so a symlink farm
  would read perfectly well. Reading was never the problem there; *writing* is, and that is a
  different problem with a different answer:
  [The ChatGPT credential is seeded once](#the-chatgpt-credential-is-seeded-once).

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
| ChatGPT credential | `/var/lib/dekopon/chatgpt/chatgpt-auth.json` | none | init container, **once**; then `dekopond` owns it |
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
- **Broker `readinessProbe`**, 30 s period. It keeps pod readiness truthful and, when the optional
  webhook Service is enabled, prevents traffic while the broker is unavailable.
- **Gateway `readinessProbe`**, only with `gateway.service.enabled`. A TCP probe gates the Service
  on the configured webhook port. It fails closed when `dekopond.yaml` binds loopback, names a
  different port, or does not configure an inbound listener.
- **No `livenessProbe`.** An automatic restart could kill a broker mid-invocation or lose an
  acknowledged in-memory webhook delivery. Process failure and readiness already remain visible.

One consequence worth knowing: the probe runs `dekopon-run`, which reads
`OTEL_EXPORTER_OTLP_ENDPOINT` from its environment. Do not set that variable on the broker
container, or every probe exports a trace. `OTEL_EXPORTER_OTLP_HEADERS` is safe and is what the
broker's own telemetry block needs.

## The ChatGPT credential is seeded once

`dekopon auth chatgpt login` is a device-authorization flow: it prints a URL and a short code and
waits for a human with a browser. Nothing in a pod can do that, so a `kind: chatgptSubscription`
model has to be handed a credential exported from a local login. `dekopon auth chatgpt export`
produces it — that command lands with the auth-export change, and
[`docs/chatgpt-credential.md`](https://github.com/dekopon-agents/dekopon/blob/main/docs/chatgpt-credential.md)
is the full lifecycle.

Set `gateway.chatgpt.enabled` and point it at the Secret:

```yaml
gateway:
  chatgpt:
    enabled: true
    existingSecret: dekopon-chatgpt-auth   # what `dekopon auth chatgpt export` emits
```

The chart then places `/var/lib/dekopon/chatgpt/chatgpt-auth.json`, `0600`, owned by `65532`, in a
`0700` directory owned by `65532`, **and never touches it again**.

### Why this one file is different

Every other file the init container writes is overwritten on every start, because the daemons only
read them. This one the daemon *writes*. The refresh token rotates on every refresh —
`refresh_credentials` builds a complete replacement record and there is no path that keeps the old
token — and `refresh_if_needed` **returns** the result of writing it back, so a failed write does not
degrade into a retry, it fails the model turn.

Two consequences the chart is built around:

- **After one refresh, the copy in your vault is a dead token.** Copying it back in would hand a
  working gateway a credential the provider has already retired. An unguarded `install` would do
  exactly that on every restart, and the symptom would not appear until the first reschedule — the
  worst possible shape for this bug.
- **The daemon needs a writable directory, not a writable file.** `save_credentials` creates a
  sibling temporary file — `chatgpt-auth.json` becomes `chatgpt-auth.tmp-<pid>`, the extension is
  *replaced*, not appended — writes and `sync_all`s it, then renames it over the target. A
  `subPath` mount of a single file satisfies none of that. The `0700` **directory** mode is
  load-bearing.

So the guard is the whole mechanism, because `install` overwrites unconditionally:

```sh
[ -d /var/lib/dekopon/chatgpt ] || mkdir -p /var/lib/dekopon/chatgpt
chown 0:0 /var/lib/dekopon/chatgpt && chmod 0700 /var/lib/dekopon/chatgpt
if [ ! -e /var/lib/dekopon/chatgpt/chatgpt-auth.json ]; then
  install -m 0600 -o 65532 -g 65532 \
    /dekopon-source/chatgpt-auth.json /var/lib/dekopon/chatgpt/chatgpt-auth.json
fi
chmod 0700 /var/lib/dekopon/chatgpt && chown 65532:65532 /var/lib/dekopon/chatgpt
```

`-e`, not `-f` and not `-s`: anything at that path — zero bytes, odd type, a leftover from a crash —
means *seeded*, and a credential this chart cannot interpret is not a credential it should
overwrite. The directory is reclaimed to `root` first for the same reason the other directories
are: after a previous run it is `0700` and owned by `65532`, and root without `DAC_OVERRIDE` cannot
read through it to run the test at all.

### It lives on the claim, and that is the point

`gateway.chatgpt.subdir` is a single path segment joined onto `paths.stateDir`, not a free path.
That is deliberate: an `emptyDir` dies with the pod, so a credential seeded there would be re-seeded
on every reschedule — the same bug, just rarer and harder to see. Making the location composed
rather than configured means it cannot be pointed somewhere ephemeral by accident.

The gateway mounts that directory with `subPath`, so it gets the credential directory and nothing
else on the claim — it never holds a path to the audit chain. Same UID, so this is reachability
rather than isolation, but the unprivileged half has no business being able to open the broker's
durable state.

`DEKOPON_CHATGPT_AUTH_FILE` is set on the gateway container to that path. Without it, a model with
no explicit `authFile` falls back to `$XDG_CONFIG_HOME` and then `$HOME`, which is on the read-only
root filesystem, where `save_credentials` cannot create its temporary sibling. Naming `authFile` in
`dekopond.yaml` is clearer still, and then neither the environment nor the fallback matters.

### Re-seeding is deliberate

`gateway.chatgpt.reseed: true` discards whatever is in the volume and copies the Secret in again.
It is separate because it is destructive: the credential in the volume is the live one, and the
exported copy is almost certainly older. Use it after a deliberate local re-login and re-export.

It is not self-clearing — while it is `true`, every restart re-seeds — so set it back to `false`
once the pod has rolled. It rolls the pod on its own, because it changes the init container's
arguments, which are part of the pod template.

There is deliberately **no `checksum/` annotation for the credential Secret**, unlike every other
file the chart writes. Those annotations exist to push a changed file into a pod that only reads at
startup. This is the one file whose entire purpose is *not* to be pushed in: the copy in the volume
is authoritative, so an annotation would restart a working gateway to achieve nothing at all.

### One replica, now for two reasons

`replicas: 1` and `strategy: Recreate` were already forced by the broker's exclusive `flock` on the
audit log and checkpoint. With this model kind they are load-bearing a second time:
`ChatGptCodexModel` serializes refreshes behind a per-process mutex and cannot coordinate across
processes, so two pods sharing one credential file would race the rotation and the loser would be
left holding an invalidated refresh token. Neither value is exposed.

### The exported copy goes stale, and that is correct

Nothing detects the drift and nothing repairs it. The exported copy has one job — seeding a *new*
deployment. It is not a backup, and restoring it over a live credential is a way to break a working
pod, not to fix one. Deliberate rotation is: log in locally again, re-export, update the Secret,
then either delete the file in the volume and restart, or set `reseed` for one roll.

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

### Size `maxReplayIds` with it

`auditMaxRecords` is not the only bound that ends in a permanent refusal, and it is not the first
one a busy deployment reaches. The broker's replay ledger holds `brokerLimits.maxReplayIds`
invocation identifiers (stock **100 000**), never evicts, and is restored from durable history at
startup — one entry per Decision event — so it is cumulative across restarts exactly like the audit
file. A *denial* costs one audit record and one full ledger slot, while an executed invocation costs
two audit records and one slot, so with the stock ledger against `auditMaxRecords: 200000` a
denial-heavy history exhausts the ledger at half the audit budget, before the designed
`AuditError::Full` refusal ever fires.

Either bound reached answers every client `capacity-exhausted` and logs
`broker_capacity_exhausted`. Neither is recoverable by retry or by restart. Set `maxReplayIds` to at
least `auditMaxRecords`. The ledger holds one bounded identifier string per entry, so matching
200 000 costs tens of MiB of resident memory — cheaper than a broker that refuses every invocation
until someone edits a values file and rolls the pod.

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
into a crash. The one thing a daemon does write — the ChatGPT credential, when that model kind is
in use — gets its own writable directory on the claim; see
[The ChatGPT credential is seeded once](#the-chatgpt-credential-is-seeded-once).

## Two version numbers

The chart and the application it deploys are versioned independently, and both numbers are real.

| | Where it comes from | What it means |
|---|---|---|
| **chart version** (`Chart.yaml: version`) | a `dekopon-chart-*` Git tag | the version of *this chart* — its templates, defaults, and documentation |
| **appVersion** (`Chart.yaml: appVersion`) | the application release the chart deploys | what `image.tag` defaults to, and what the pod actually runs |

They move for different reasons. A templating fix ships as `dekopon-chart-0.1.1` and changes no
`appVersion`; a new application release moves `appVersion` and, with it, the image the chart pulls.
`v*.*.*` tags publish crates, release archives, and the container image; `dekopon-chart-*` tags
publish only the chart. That is the whole reason for two tag namespaces — a chart bug should not
force an application release, and an application release should not republish an unchanged chart.

`appVersion` is `0.4.0`, the **first release the container-image workflow runs for**. `v0.3.0`
predates that workflow and no image was ever published for it, so setting `appVersion: 0.3.0` would
ship a chart whose default pulls nothing.

The image workflow publishes under the Git tag, so the tag carries a `v`. An empty `image.tag`
therefore renders `v` + `appVersion`:

```
ghcr.io/dekopon-agents/dekopon:v0.4.0
```

There is no `latest`. Prefer `image.digest` once a release exists; it pins across the
`linux/amd64` + `linux/arm64` index, and the index digest is what `gh attestation verify` attests.

## Publishing

[`.github/workflows/chart-publish.yml`](https://github.com/dekopon-agents/dekopon/blob/main/.github/workflows/chart-publish.yml)
triggers on `dekopon-chart-*` tags only. Before tagging, move the chart's completed bullets from
[`CHANGELOG.md`](../../CHANGELOG.md) into a dated `[dekopon-chart-<VERSION>]` section. Pull-request
CI requires that section to match `Chart.yaml`, and the publish workflow repeats the check. It then
takes the version from the tag (`VERSION="${GITHUB_REF_NAME#dekopon-chart-}"`), refuses to continue
unless `Chart.yaml` declares that same version, packages the chart, lints and renders the **tarball**
rather than the working tree, and only then pushes:

```console
helm package charts/dekopon --version "$VERSION"
helm push "dekopon-$VERSION.tgz" oci://ghcr.io/dekopon-agents/charts
```

Helm 3.8+ speaks OCI natively, so there is no plugin, no chart index, and no repository server —
the registry is the repository. Authentication is the same `GITHUB_TOKEN` login the container image
workflow already uses, so the chart adds no credential. It never passes `--app-version`: a chart tag
must not silently move which application release the chart deploys.

The published coordinates are:

```
oci://ghcr.io/dekopon-agents/charts/dekopon
```

Chart `0.1.0` is published there. The packaging half is checked on every CI run, which packages the
chart and diffs the archive's rendered output against the source tree's, and the `dekopon-chart-0.1.0`
tag ran the push. What remains unproven is the *pull*: no cluster has installed the published chart,
so a first install should be treated as the first exercise of this path.

### Both GHCR packages are public, and that is a manual step

A GHCR package is **private when it is first published** and stays private until someone changes it
in the repository's package settings. That has to be done once per package, after the first publish:

- `ghcr.io/dekopon-agents/dekopon` — the container image
- `ghcr.io/dekopon-agents/charts/dekopon` — this chart

Both are meant to be public, and everything downstream is designed for anonymous pull: the chart
carries no `imagePullSecrets` and needs no registry credential in ArgoCD. Do the flip early, because
a private package fails confusingly — an anonymous pull of a private GHCR package is reported as
*not found*, not as *forbidden*, so it reads like a typo in the chart name or the version.

## Consuming it from ArgoCD

Verified against the target cluster, which runs **ArgoCD v3.3.6** on k3s v1.36.3:

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: dekopon
  namespace: argocd
spec:
  project: default
  source:
    repoURL: ghcr.io/dekopon-agents/charts   # bare: no oci:// prefix on this source type
    chart: dekopon
    targetRevision: 0.1.0                    # the CHART version, not appVersion
    helm:
      valueFiles: []
      values: |
        ...
  destination:
    server: https://kubernetes.default.svc
    namespace: dekopon
```

Two mechanics that are easy to get wrong, both checked against 3.3 rather than assumed:

**`repoURL` must be the bare registry path.** Argo CD 3.3 has two different OCI source shapes. A
Helm chart (`spec.source.chart` set) takes a bare path and the documentation says outright that
"the repository URL should not contain the OCI scheme prefix `oci://`". The `oci://` spelling
belongs to the *other* shape — a plain OCI artifact source with `spec.source.path` and no `chart`,
registered with `--type oci` — which reads manifests out of an artifact and does not run Helm at
all. Writing `oci://` on a chart source is not a stylistic choice this version accepts.

**A Repository registration is required even though the package is public.** Argo decides OCI mode
from the repository's `EnableOCI` field, not from the URL — `reposerver` trims an `oci://` prefix
only when matching credentials — and `db.GetRepository` returns a bare `Repository{Repo: url}` for
any URL that is not registered, so `EnableOCI` is false and the registry is treated as a classic
HTTP Helm repository. Being public removes the need for a `username` and `password`; it does not
remove the need to register. So the follow-up homelab change needs this prerequisite alongside its
`Application`:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: dekopon-charts
  namespace: argocd
  labels:
    argocd.argoproj.io/secret-type: repository
stringData:
  name: dekopon-charts
  url: ghcr.io/dekopon-agents/charts
  type: helm
  enableOCI: "true"
```

No credential fields: the package is public and the pull is anonymous.

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
| ChatGPT credential | `gateway.chatgpt.inline` | `gateway.chatgpt.existingSecret` / `.existingSecretKey` |

Use `existingSecret` for credentials. An inline value is stored in the release, returned by
`helm get values`, and usually committed.

The chart refuses to render, with a message, when: `runAsUser` is changed while the stock image is
selected; a required file has no source; both sources are set for one file; an inline `broker.yaml`
names `policiesPath`, `credentialsPath`, or `constraintSets` with no corresponding value supplied;
or `paths.catalogDir` is inside `paths.configDir`. When provider storage is enabled, its root and
key directory must also be absolute, disjoint from one another, and pairwise non-overlapping with
every chart-owned mount (`config`, runtime, audit state, catalog, `/tmp`, and both projected
configuration/key sources); a nested mount would otherwise shadow or destructively replace those
files. Every one of
those is a mistake whose only other symptom is a pod that starts and never becomes ready.

The chart's default `broker.config.inline` is the echo example from the broker's own README, moved
onto these paths: a real deny-by-default configuration that starts, loads the baked
`echo-provider.wasm`, and authorizes exactly one read-only capability for the pod's own UID. It is
there so you can install the chart and watch a broker become ready before you give it anything that
matters. Replace it. `gateway.enabled` is `false` by default because a gateway needs a chat token, a
model endpoint, and an agent catalog, and the chart can invent none of them.

`gateway.service` optionally creates a ClusterIP Service and matching named gateway container port.
The chart intentionally does not create an Ingress: the operator must route only the configured
callback path and terminate public TLS outside the pod. A Kubernetes Service cannot reach loopback,
so the corresponding transport must bind `0.0.0.0:<gateway.service.port>`. The TCP readiness probe
keeps the Service endpoint unavailable when the configuration and chart port disagree.

## Install

From the registry, once a `dekopon-chart-*` tag has been published and the package made public:

```console
helm show chart oci://ghcr.io/dekopon-agents/charts/dekopon --version 0.1.0
helm upgrade --install dekopon oci://ghcr.io/dekopon-agents/charts/dekopon \
  --version 0.1.0 -n dekopon --create-namespace -f my-values.yaml
```

`helm` itself takes the `oci://` prefix here — that is the Helm CLI's own registry syntax and it is
unrelated to what an ArgoCD `Application` accepts in `repoURL`, which is the bare path.

From a checkout:

```console
helm lint charts/dekopon
helm template dekopon charts/dekopon
helm template dekopon charts/dekopon -f charts/dekopon/values-pr-summarizer-linter.yaml
helm upgrade --install dekopon charts/dekopon -n dekopon --create-namespace \
  -f my-values.yaml
```

[`values-pr-summarizer-linter.yaml`](values-pr-summarizer-linter.yaml) is the
[PR summarizer and linter](https://github.com/dekopon-agents/dekopon/blob/main/examples/pr-summarizer-linter/README.md) deployment expressed as chart values: Slack Agent sessions with tangerine reaction degradation, one
agent, six narrow `gh` capabilities, a broker-injected token by reference, and the audit chain on its
own volume. It may post one review comment and has no approval, request-changes, or merge capability.

## What is not proven

- Nothing has been applied to a cluster. The chart has been linted, rendered, and schema-validated
  with `kubeconform` against Kubernetes 1.29, 1.33 and 1.36, and the init container's rendered
  command has been run verbatim on `linux/arm64` and `linux/amd64` under its rendered
  `securityContext` against a fixture built to match a projected volume's symlink layout, but no
  `kubectl apply` has happened.
- The daemons have never been started from this configuration. The images exist — application tags
  from `v0.4.0` onward publish `ghcr.io/dekopon-agents/dekopon:v<VERSION>` — but no pod has run one
  from these manifests.
- **The pull path is unproven.** `dekopon-chart-0.1.0` ran `chart-publish.yml` and chart `0.1.0`
  exists at `oci://ghcr.io/dekopon-agents/charts/dekopon`, and packaging is checked continuously:
  CI packages the chart, lints the archive, and diffs the archive's rendered output against the
  source tree's for both value sets, so the pushed tarball is known to be complete and to render
  identically. What has not happened is an anonymous `helm pull` or an ArgoCD sync against that
  registry path.
- The ArgoCD source form above was derived from ArgoCD 3.3's own documentation and source, checked
  against the running v3.3.6, but no `Application` has been created — the real one lands in a
  separate rpi-homelab change.
- The ChatGPT seed-once behaviour is proven against the rendered init container across a cold
  start, a simulated rotation, two restart shapes, and the gated re-seed — including a negative
  control confirming that removing the `[ -e ]` test does revert the credential. But no real
  `dekopond` has refreshed a real token through it. What was exercised is the file-level contract
  (`0600`, owner `65532`, one link, a `0700` directory, temp sibling plus rename by UID 65532), not
  a live refresh against OpenAI.
- The `PodSecurity` `restricted` profile would reject this pod: the init container runs as root.
  `baseline` is fine.

## Optional provider-storage claim and namespace key

`providerStorage.enabled` creates (or mounts) a claim physically separate from `state`, mounted only
into the broker at `/var/lib/dekopon-provider-storage`. A generated claim always carries
`helm.sh/resource-policy: keep`; an existing claim remains operator-owned. Rendering fails when the
resolved audit and provider-storage claim names are equal.

The chart never creates the namespace key. `providerStorage.existingKeySecret` is required and is
operator-managed, so uninstall cannot delete the key for retained data. The init container alone
mounts its projected symlink farm, copies the selected key into a separate broker-only tmpfs
`0700` directory as one `0600`, UID-65532, single-link regular file, and verifies it. The gateway
mounts neither key tmpfs nor provider-storage PVC. Every chart mount path must be a canonical
absolute sequence of safe non-dot segments (no repeated slash). Storage root/key paths must not
overlap each other, another chart mount, projected init sources, or baked image paths including
`/opt/dekopon/providers` and `/opt/dekopon/optional-providers`; key source/destination names must be
non-dot path segments. Invalid combinations fail during `helm template` before a volume can shadow
configuration, packaged providers, or init-script text. The `storage-probe` and malicious
`memory-reservation-probe` fixtures are not present in the image; `memory-chat-provider.wasm` is
baked only under `/opt/dekopon/optional-providers`, outside
the default scan.

The key is HMAC namespace/recovery authority, not encryption-at-rest. Losing or rotating it with
retained data makes startup fail. The provider-storage filesystem must support retained
directory-descriptor-relative no-follow opens, advisory locks, same-directory rename, and
file/directory sync. A stuck native syscall may exceed
the configured shutdown grace, and malicious same-UID filesystem mutation is outside the claim.
