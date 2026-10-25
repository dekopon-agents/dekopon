# Run Dekopon on Kubernetes

The [Helm chart](../charts/dekopon/) runs `dekopon-brokerd` and `dekopon-gatewayd` as one pod
with separate runtime UIDs. Its reference deployment is a single-node arm64 k3s cluster. It is
published to `oci://ghcr.io/dekopon-agents/charts/dekopon` and consumed from ArgoCD by registry
path, not by Git path; both GHCR packages are public. See
[Two version numbers](#two-version-numbers) and [Publishing](#publishing).

The reference deployment is
[rpi-homelab at `034c61cc`](https://github.com/xrl/rpi-homelab/blob/034c61cc27d0c22cededcbd4072eeb0a4b3fe5d7/apps/dekopon.yaml),
which declares chart **0.37.4** and a digest-pinned **0.50.4 optimized-Wasmtime** image.
Its [rollout PR](https://github.com/xrl/rpi-homelab/pull/291) records the previous 0.50.3 deployment
as Synced/Healthy; its 0.50.4 post-sync checks are gates, not recorded results. Git configuration,
rendered manifests and recorded rollout evidence are distinct from independently observed runtime
state. See [verification limits](#what-is-not-proven).

Read [the broker configuration reference](../crates/dekopon-brokerd/README.md) and
[the gateway configuration reference](gatewayd.md) first. The chart places files and sets
permissions; it does not define or validate their contents, and the two daemons' own documentation
is the only description of what goes in them.

The chart boundary requires lockstep daemon binaries carrying distinct-UID IPC support. Select an
image built from that source with `image.tag` or `image.digest`; the `appVersion` default alone does
not prove those binaries are present.

Use this guide for [installation](#install), [ArgoCD](#consuming-it-from-argocd),
[configuration values](#configuration-values), [storage and recovery](#storage-uninstall-and-recovery),
and [chart publishing](#publishing). The [container image guide](container-image.md) covers the
image's contents, build, and provenance.

## Install

Before installing:

- Use Kubernetes **1.29 or newer**, as required by the chart's native broker sidecar, and
  Helm **3.8 or newer** for OCI registry support
- Select a persistent StorageClass with `state.storageClassName` (default `local-path`) or supply
  an existing claim with `state.existingClaim`; review [state retention](#storage-uninstall-and-recovery)
  before replacing an existing deployment
- Use a namespace that permits the root copy init container. The Pod Security `restricted`
  profile rejects it; see [What is not proven](#what-is-not-proven)
- Prepare `my-values.yaml` from the [chart values](../charts/dekopon/values.yaml) or the worked
  example below. Create the referenced Secrets and ConfigMaps in the release namespace first.
  Keep credentials in existing Secrets, as described in [Configuration values](#configuration-values)

The default values start only the broker, with no executable capabilities. A chat deployment also
needs `gateway.enabled: true`, broker policy and identity mappings, gateway configuration, a catalog,
and its chat/model credentials. The chart supplies file placement and permissions; the linked daemon
references define their contents. [ChatGPT subscription credentials](#the-chatgpt-credential-is-seeded-once)
and [managed provider sets](#the-managed-provider-set-comes-from-the-claim) have additional preparation steps.

From the registry, using **chart version `0.41.0`**, not application version `0.54.0`:

```console
helm show chart oci://ghcr.io/dekopon-agents/charts/dekopon --version 0.41.0
helm upgrade --install dekopon oci://ghcr.io/dekopon-agents/charts/dekopon \
  --version 0.41.0 -n dekopon --create-namespace -f my-values.yaml
```

`helm` itself takes the `oci://` prefix here — that is the Helm CLI's own registry syntax and it is
unrelated to what an ArgoCD `Application` accepts in `repoURL`, which is the bare path.

From a checkout, run these commands from the repository root:

```console
helm lint charts/dekopon
helm template dekopon charts/dekopon
helm template dekopon charts/dekopon -f charts/dekopon/values-pr-summarizer-linter.yaml
helm upgrade --install dekopon charts/dekopon -n dekopon --create-namespace \
  -f my-values.yaml
```

[`values-pr-summarizer-linter.yaml`](../charts/dekopon/values-pr-summarizer-linter.yaml) is the
[PR summarizer and linter](https://github.com/dekopon-agents/dekopon-provider-gh/blob/main/examples/pr-summarizer-linter/README.md) deployment expressed as chart values: Slack Agent sessions with tangerine reaction degradation, one
agent, six narrow `gh` capabilities, a broker-injected token by reference, and every broker decision
exported as an audit log record over OTLP. It may post one review comment and has no approval, request-changes, or merge capability.

## What it deploys

One `Deployment`, `replicas: 1`, `strategy: Recreate`, and no chart-owned `Ingress`. An opt-in
ClusterIP `Service` exposes only a configured gateway webhook port for operator-owned exact-path
routing; it is disabled by default. The broker never receives a TCP surface.

| Container | Kind | Runs |
|---|---|---|
| `prepare-files` | init, runs to completion, **root** | `busybox`, copies configuration into an `emptyDir` with the right owner and mode |
| `broker` | native sidecar (`restartPolicy: Always`) when the gateway is enabled, otherwise the pod's only regular container | `dekopon-brokerd --config /etc/dekopon/broker.yaml` |
| `gateway` | regular container, only when `gateway.enabled` | `dekopon-gatewayd --config /etc/dekopon-gateway/gatewayd.yaml` (the directory `/etc/dekopon-gateway` with `gateway.configDirectory.configMap`) |

`broker.providerSync.enabled` adds a hook `Job` that runs `dekopon-brokerd provider sync`; see
[Syncing the set with the chart's hook Job](#syncing-the-set-with-the-charts-hook-job).

The chart enforces the [current local process boundary](security-model.md#current-local-process-boundary).
Pod defaults and broker stay `65532:65532`; the gateway container is `65533:65533`.
Supplementary group `65534` reaches only the broker's `0660` socket in its `0710` IPC
directory. Gateway configuration must explicitly pin `broker.serverUid: 65532` and broker
`identities` must map gateway UID `65533`; keep a separate UID `65532` mapping for broker
probes. Inline gateway configuration without the pin is refused; existing Secrets are opaque
and their owner must supply it. The gateway receives no broker configuration, secret, or storage
mount, and the broker receives no gateway configuration, model credential, or temporary volume.

The broker is a native sidecar rather than a second regular container because ordering matters in
both directions. `dekopon-gatewayd` probes the broker once at startup and exits non-zero when the socket
does not answer, so a plain second container would crash-loop its way to a working state; a sidecar
with a startup probe means Kubernetes does not start `dekopon-gatewayd` at all until the broker answers a
real request. Termination runs the other way — the gateway drains first, the broker second — which
lets the gateway's in-flight invocations finish against a broker that is still serving, and is the
reason the pod's grace is a sum rather than a maximum;
see [Draining takes both graces, in sequence](#draining-takes-both-graces-in-sequence). This needs
Kubernetes 1.29 or newer, and `Chart.yaml` declares `kubeVersion: ">=1.29.0-0"` so an older cluster
refuses the install instead of deadlocking on an init container that never exits.

## Why the broker still needs an init container

`dekopon-brokerd` validates every path it touches, in tiers, and refuses to serve when one is
wrong:

| Tier | Applies to | Rule |
|---|---|---|
| A | `broker-credentials.yaml`, `secret-map.yaml` | rejected if `mode & 0o077 != 0` |
| B | `broker.yaml`, `policies.cedar`, provider `.wasm` files and their parents | rejected if `mode & 0o022 != 0` |
| C | `providerSet.storePath` (the parent of generated `cwasm`) and the parent of a `chatgptSubscription` `authFile` | must be `0700` and owned by the runtime UID |
| D | every ancestor up to `/` | must be a directory that is not group- or world-writable unless sticky |
| E | gateway config, catalog and skills | root or daemon UID, no group/world write; projected symlinks only on a trusted read-only mount |

Broker IPC instead uses the exact owner/group/mode checks in the [current boundary](security-model.md#current-local-process-boundary); local chat remains private.

Tier A and B additionally require `uid == geteuid()`, `nlink == 1`, and an open with `O_NOFOLLOW`.

No Kubernetes volume can present a broker file that satisfies A or B:

| Mount shape | What the daemon sees | Result |
|---|---|---|
| `secret` / `configMap` volume | `key -> ..data/key -> ..2026_…/key` | broker's `O_NOFOLLOW` returns `ELOOP` before any mode is examined |
| the same with `subPath` | a real file, `root:root`, `0644` | owner is not the daemon's euid; A also fails on `0o044` |
| any of the above plus `fsGroup` | `65532:2000`, `0640` | passes B, fails A — `0o040` is exactly what `mode & 0o077` rejects |

`fsGroup` is worth calling out on its own, because it is the reflex fix for "the pod cannot read its
volume" and here it is the thing that breaks the credentials file specifically. It is absent from
`podSecurityContext`, and adding it is a chart render refusal.

The chart gathers broker copy sources in `/dekopon-source`, visible only to init, and copies:

```sh
install -m 0600 -o 65532 -g 65532 /dekopon-source/broker.yaml /etc/dekopon/broker.yaml
```

which produces a fresh regular file, one link, mode `0600`, owned by `65532` — a shape that
satisfies A and B at once. Broker copies are read at startup: `checksum/config` rolls the pod
when inline broker configuration changes. An external broker Secret or a broker `host.yaml`
fragment still needs an operator-managed restart (for example, Reloader) when edited. Gateway
configuration instead mounts a distinct read-only Secret or ConfigMap directory directly at
`paths.gatewayConfigDir`, with no `subPath`: the gateway trusts only a root- or daemon-owned,
non-group/world-writable projection on a read-only mount. It re-reads that projection every two
seconds and swaps in only `models`, `routes`, `stopWords`, transport and session `indicators`, and
the catalog (agents and skills). An edit to `broker`, `transports`, `sessions`, `shutdownGraceMs`,
`telemetry`, `metering` or `proxy` is refused as restart-required (`gateway_reload_refused`) and
the gateway keeps running the configuration it has. Nothing rolls the pod for those edits: the
operator must roll it.
Neither broker policy nor credential projections are visible in the gateway container.

The init container is the only thing in the chart that runs as root, and it holds `CHOWN` and
`FOWNER` and nothing else. `CHOWN` is what lets it hand a directory to `65532`; `FOWNER` is what
lets it `chmod` a directory owned by another UID after a restart. It does **not** hold
`DAC_OVERRIDE`: it reclaims the directories to `root` first, which is also why it is idempotent
across an in-place pod restart where the `emptyDir` holds the previous run's `0700` directory. It
asserts its own output with `stat` before exiting, so a wrong file is an init failure naming that
file rather than a broker that starts and then refuses to serve.

### What does not need any of this

- **Chat-service, chat-model, and optional image-model credentials.** `gatewayd.yaml` names environment variable *names*, never values,
  so those are ordinary `secretKeyRef` entries under `gateway.env` with no file hygiene at all.
- **`OTEL_EXPORTER_OTLP_HEADERS`.** The broker's `telemetry` block has no credential field by
  design; the OpenTelemetry SDK reads ingest auth from that variable, so a token never enters
  `broker.yaml`. A receiver on a private CA needs `otlp.caBundle` (a ConfigMap and key of PEM
  roots), which both daemons read as `OTEL_EXPORTER_OTLP_CERTIFICATE`.
- **Agent catalog and skills.** The catalog ConfigMap mounts directly at `paths.catalogDir`.
  For agents declaring `spec.skills`, use `gateway.skills.configMap` and `gateway.skills.items`
  to map flat keys to nested paths such as `pull-request-review/SKILL.md`. The gateway and
  console mount the ConfigMap directly and read-only at `paths.skillsDir`; the broker has no
  mount. The skill loader accepts only the root-owned, read-only AtomicWriter `..data` chain
  inside that projection; arbitrary or escaping symlinks, writable files and untrusted owners
  refuse. Do not add `fsGroup` or a group/world-writable `defaultMode`. Skills are public model
  text, never credentials. Edits become available on successful gateway reconciliation without
  an init copy or pod roll. The chart adds no gateway-only checksum trigger.
- **Provider components.** The image already bakes `/opt/dekopon/providers/*.wasm` owned by
  `65532:65532` under a `65532`-owned `0755` directory, which is what Tier B wants.
- **The ChatGPT credentials**, for the opposite reason from all of these. `load_credentials` is a
  plain `File::open` — no `O_NOFOLLOW`, no owner comparison, no mode check — so a symlink farm
  would read perfectly well. Reading was never the problem there; *writing* is, and that is a
  different problem with a different answer:
  [The ChatGPT credential is seeded once](#the-chatgpt-credential-is-seeded-once). The broker's own
  family is the exception to the exception: `dekopon-brokerd` puts every `authFile` through the
  Tier A check *and* requires its parent directory to be owner-only and writable, so a symlinked or
  group-readable seed is a startup refusal there.

## The UID is not a preference

`podSecurityContext.runAsUser` is `65532` because the image bakes the provider components under that
UID and `validate_owned_file` compares a provider's owner against the broker's own euid. Any other
value makes every provider fail to load, which fails startup. The chart refuses to render when
the broker UID/GID is changed, even with a custom image. Gateway container UID/GID is pinned separately; shared `securityContext` cannot override either identity.

### Private VM proxy Service

`gateway.proxy.enabled: true` adds a private ClusterIP port (`gateway.proxy.port`, default
9090) and mounts `gateway.proxy.tlsSecret` read-only at `/etc/dekopon-proxy-tls` in the gateway
and the gateway config-check container. The Secret must contain `tls.crt`, `tls.key`, and
`ca.crt`; daemon `proxy.tls` must name those paths and `proxy.bind` must use the same port
on a non-loopback address. The chart refuses enablement without a Secret or, for inline daemon
configuration, without a `proxy:` block or with a bind port different from the chart port.
For opaque ConfigMaps the daemon's candidate check loads the TLS files and validates proxy
configuration; the chart cannot inspect those bytes. Disabling config-check for opaque config
skips that validation. The broker never mounts this Secret. No Ingress or external route is created.

## Paths the chart owns

Your `broker.yaml` and `gatewayd.yaml` must name files inside these directories. The chart places
files; it does not rewrite configuration.

The broker's `paths.configDir` is a private tmpfs populated by init as UID 65532.
The gateway alone mounts its separate read-only source at `paths.gatewayConfigDir`
(default `/etc/dekopon-gateway`); init does not copy its config. A single Secret key is
projected as `gatewayd.yaml`, or the ConfigMap fragment directory is mounted intact.
The state claim root stays root-owned `0700` and neither daemon mounts it. With `broker.chatgpt`
enabled the broker mounts only its own credential subdirectory, and the gateway mounts only the
configured ChatGPT subdirectory. Neither daemon can rename the other's directory.
Private subdirectories are `0700`; files are `0600` with one link. Each daemon gets a separate
`/tmp` volume. The broker alone mounts provider storage. The broker alone mounts broker assets.

Gateway `/tmp` is **disk-backed** `emptyDir`, sized by `volumeSizes.gatewayTmp` (default 320Mi).
Existing `volumeSizes.tmp` overrides now affect only broker scratch (still tmpfs, default 16Mi);
set `gatewayTmp` explicitly when migrating a custom scratch limit. Chat assets hold at most 256 MiB
of live payload bytes per gateway process, independently enforced before writing. The extra space
is headroom for filesystem overhead and other gateway scratch consumers, not an eviction guarantee.
Kubernetes `emptyDir.sizeLimit` is kubelet-enforced, not a reservation or synchronous write quota;
node ephemeral-storage pressure can still evict the pod. Outside this chart, configure the process
temporary directory on real disk: a tmpfs-backed `TMPDIR` does not reduce retained RAM. Existing
rollouts and operator overrides require separate verification; changing this source deploys nothing.

**Upgrade with both daemons stopped:** an `audit.jsonl` on the claim, at its root or under
`broker/`, is inert; the broker keeps no audit file, so delete it once any records you want are
copied off. Change an existing ChatGPT directory and its live credential to `65533:65533`, keeping
directory `0700` and file `0600`; do not reseed a rotated token. Update the gateway peer mapping and
explicit server pin in operator configuration. These are offline operator steps, not a live-cluster
action performed by this chart.

| File | Path | Tier | Written by |
|---|---|---|---|
| `broker.yaml` | `/etc/dekopon/broker.yaml` | B | init container |
| `policies.cedar` | `/etc/dekopon/policies.cedar` | B | init container |
| `broker-credentials.yaml` | `/etc/dekopon/broker-credentials.yaml` | A | init container |
| private `secret-map.yaml` | `/etc/dekopon/secret-map.yaml` | A | init container; broker only |
| optional secret source projection | operator-selected absolute path | source-specific | mounted read-only into broker only through `broker.secretSourceVolumes` |
| `gatewayd.yaml` | `/etc/dekopon-gateway/gatewayd.yaml` | gateway projection | read-only Secret mount, gateway only |
| broker socket | `/run/dekopon/broker.sock` | protected IPC | the broker, at bind |
| broker assets | `/var/lib/dekopon-assets` (`brokerAssets.rootPath`) | owned `0700` dir, broker-only disk `emptyDir` | init sets ownership; broker writes spools and outputs; match `brokerAssets` in operator inline `assets` config |
| agent catalog | `/etc/dekopon-catalog/dekopon.yaml` | E | read-only ConfigMap mount |
| gateway ChatGPT credential | `/var/lib/dekopon/chatgpt/chatgpt-auth.json` | none | init container, **once**; then `dekopon-gatewayd` owns it |
| broker ChatGPT credential | `/var/lib/dekopon/broker-chatgpt/chatgpt-auth.json` | A + writable parent | init container, **once**; then `dekopon-brokerd` owns it |
| providers | `/opt/dekopon/providers/*.wasm` | B | baked into the image |
| managed provider set | `/var/lib/dekopon/providers/` | owned `0700` dir, broker-only | the directory by the init container; the lock and the store by the operator's sync step |
| gateway conversation journal | `gateway.journal.mountPath` (default `/var/lib/dekopon-gatewayd/journal`), `gateway.journal.subdir` on the claim | owned `0700` dir, gateway-only | the directory by the init container when `gateway.journal.enabled`; `0600` files by `dekopon-gatewayd`. Set `sessions.journal.path` in `gatewayd.yaml` to the mount path |

`/etc/dekopon` and `/run/dekopon` are memory-backed `emptyDir`s, so the credentials file and the
socket never reach the node's disk. `/var/lib/dekopon` is the retained model-credential claim.

## Probes

There is no HTTP health endpoint, and the image is distroless with no shell, so an `exec` probe can
run `dekopon-brokerd probe --socket /run/dekopon/broker.sock`, which connects
over the real socket, passes `SO_PEERCRED` in both directions, and gets back the capability list
policy exposes to this peer. It is evaluated from the constraint catalog and the policy set and
emits **no audit record**.

- **`startupProbe`**, 5 s period, 60 failures — five minutes. The broker loads and validates every
  component before binding the socket, so "the socket answers" is exactly "fully started".
  Managed providers default to verified mmap-backed cwasm on the provider store's persistent disk;
  `compileOnLoad: true` bypasses it. The precompile init container builds or repairs a cold cache
  before the broker starts; with the cache enabled, startup only verifies and maps artifacts.
  `compileOnLoad: true` does not skip the init container; a cache precompile cannot read is reset
  by deleting `store/cwasm` on the claim before the pod restarts.
  Do not put the cache on the chart's memory-backed `/tmp`. The probe budget covers startup
  validation, and must also cover compilation when `compileOnLoad: true`.
- **Broker `readinessProbe`**, 30 s period. It keeps pod readiness truthful and, when the optional
  webhook Service is enabled, prevents traffic while the broker is unavailable.
- **Gateway `readinessProbe`**, only with `gateway.service.enabled`. A TCP probe gates the Service
  on the configured webhook port. It fails closed when `gatewayd.yaml` binds loopback, names a
  different port, or does not configure an inbound listener.
- **No `livenessProbe`.** An automatic restart could kill a broker mid-invocation or lose an
  acknowledged in-memory webhook delivery. Process failure and readiness already remain visible.

The probe runs as broker UID 65532, separately mapped from gateway UID 65533, and pins the
server to its own effective UID. Its authenticated exchange has a two-second deadline and the
protocol frame ceiling; absent, refused, unauthenticated or wrong-server sockets fail nonzero.
It loads no configuration, components or credentials and initializes no telemetry.

## Draining takes both graces, in sequence

The sidecar ordering that makes startup work also makes shutdown serial. The kubelet stops the
gateway's container first and only starts stopping the broker sidecar once that container is gone,
so the pod's `terminationGracePeriodSeconds` has to cover **both** drains added together — not the
larger of the two. `dekopon-gatewayd` drains in-flight sessions for its own `shutdownGraceMs`,
`dekopon-brokerd` then drains connections for `serverLimits.shutdownGraceMs`, and both default to
`120000`.

Whatever is draining when the pod's grace expires is `SIGKILL`ed. For the gateway that
abandons a model turn somebody is waiting on; for the broker it lands mid-invocation, which is the
one place this chart cannot promise a clean failure.

`terminationGracePeriodSeconds` therefore defaults to **270** — 120 + 120 +
`drainBudget.bufferSeconds` — and the chart asserts the arithmetic at template time. A grace shorter
than the two configured graces plus the buffer is a refused `helm template` naming both numbers, not
a kill nobody notices until an invocation is cut off:

```console
$ helm template dekopon charts/dekopon -f charts/dekopon/values-pr-summarizer-linter.yaml \
    --set terminationGracePeriodSeconds=180
Error: ... terminationGracePeriodSeconds is 180, but the containers stop in sequence: dekopon-gatewayd
drains for 120000 ms and then dekopon-brokerd drains for 120000 ms ... That needs 270 seconds.
```

The buffer — 30 s by default — is everything the two graces do not name: `SIGTERM` delivery and the
kubelet's per-container bookkeeping, and each daemon's telemetry flush, since an OTLP exporter can
burn its whole `exportTimeoutMs` against a collector that is itself restarting.

The chart first reads `shutdownGraceMs` out of an **inline** config. If the key is absent, it uses
`drainBudget.assumedBrokerShutdownGraceMs` or `drainBudget.assumedGatewayShutdownGraceMs` when set.
Both assumptions default to `null`. Each daemon using `configDirectory` requires its assumption:
the chart cannot read that directory and refuses to render without an explicit value. Otherwise,
an unset assumption falls back to the daemon's `120000` ms default. An `existingSecret` is also
opaque, so set the corresponding assumption if your Secret uses a non-default grace. These values
only inform the assertion; they do not configure either daemon.

A longer shutdown window does not promise crash recovery.

## The ChatGPT credential is seeded once

`dekopon-gatewayd auth chatgpt login` is a device-authorization flow: it prints a URL and a short code and
waits for a human with a browser. Nothing in a pod can do that, so a `kind: chatgptSubscription`
model has to be handed a credential exported from a local login.
`dekopon-gatewayd auth chatgpt export --expose-credential` produces it, and
[the ChatGPT credential guide](chatgpt-credential.md)
is the full lifecycle.

Set `gateway.chatgpt.enabled` and point it at the Secret:

```yaml
gateway:
  chatgpt:
    enabled: true
    existingSecret: dekopon-chatgpt-auth   # what `dekopon-gatewayd auth chatgpt export --expose-credential --namespace <ns>` emits
```

The chart then places `/var/lib/dekopon/chatgpt/chatgpt-auth.json`, `0600`, owned by `65533`, in a
`0700` directory owned by `65533`, **and never touches it again**.

### Why this one file is different

Every other file the init container writes is overwritten on every start, because the daemons only
read them. This one the daemon *writes*. The refresh token rotates on every refresh —
`refresh_credentials` builds a complete replacement record and there is no path that keeps the old
token — and the rotated record is written back atomically: same-directory temporary file, `fsync`,
rename, directory `fsync`. A write that fails does not fail the turn, because by then the
authorization server has already retired the predecessor and the in-memory token is the only one
that works; it logs `chatgpt_credential_save_failed` and continues, which leaves the retired token
on disk for the next process to spend. That is the failure mode the writable `0700` directory
exists to prevent.

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
  install -m 0600 -o 65533 -g 65533 \
    /dekopon-source/chatgpt-auth.json /var/lib/dekopon/chatgpt/chatgpt-auth.json
fi
chmod 0700 /var/lib/dekopon/chatgpt && chown 65533:65533 /var/lib/dekopon/chatgpt
```

`-e`, not `-f` and not `-s`: anything at that path — zero bytes, odd type, a leftover from a crash —
means *seeded*, and a credential this chart cannot interpret is not a credential it should
overwrite. The directory is reclaimed to `root` first for the same reason the other directories
are: after a previous run it is `0700` and owned by `65533`, and root without `DAC_OVERRIDE` cannot
read through it to run the test at all.

### It lives on the claim, and that is the point

`gateway.chatgpt.subdir` is a single path segment joined onto `paths.stateDir`, not a free path. An
`emptyDir` dies with the pod, so a credential seeded there would be re-seeded on every reschedule —
the same bug, just rarer and harder to see. Composing the location rather than configuring it means
it cannot be pointed somewhere ephemeral by accident.

The gateway mounts that directory with `subPath`, so it gets the credential directory and nothing
else on the claim. The broker mounts its separate sibling; neither daemon mounts the claim
root, and distinct UID ownership additionally denies access to the other's private files.

`DEKOPON_CHATGPT_AUTH_FILE` is set on the gateway container to that path. Without it, a model with
no explicit `authFile` falls back to `$XDG_CONFIG_HOME` and then `$HOME`, which is on the read-only
root filesystem, where `save_credentials` cannot create its temporary sibling. Naming `authFile` in
`gatewayd.yaml` is clearer, and then neither the environment nor the fallback matters.

### The broker has a second, independent family

*Committed direction:* the broker's `credential`/`agents.<id>.credentials` bindings will be replaced by
public DRNs. The shared refresh implementation and independent token families described here must
survive that migration ([requirements](design.md#legacy-credential-bindings)); the chart
continues to seed the current credential kind.

`broker.chatgpt.*` seeds the same kind of file for `dekopon-brokerd`, which is what a
`kind: chatgptSubscription` entry in `broker-credentials.yaml` points its `authFile` at:

```yaml
broker:
  chatgpt:
    enabled: true
    existingSecret: dekopon-chatgpt-auth-broker
```

```yaml
# broker-credentials.yaml
- name: chatgpt-gpt-image
  kind: chatgptSubscription
  authFile: /var/lib/dekopon/broker-chatgpt/chatgpt-auth.json
  destinations: [chatgpt.com]
```

Everything above applies to it unchanged: seeded once into the claim, `0600` owned by `65532`, in a
`0700` directory owned by `65532`, and never overwritten again. Three things differ.

**It is a separate token family, and must be.** Produce it with a second
`dekopon-gatewayd auth chatgpt login --auth-file <path>` against the same ChatGPT account, then
`dekopon-gatewayd auth chatgpt export --expose-credential --auth-file <path>`. Never point the broker and the
gateway at one file: the refresh token rotates and the authorization server retires its predecessor,
so two independent holders of one file eventually present a retired token and revoke the family for
both. The chart refuses to render when `broker.chatgpt.subdir` equals `gateway.chatgpt.subdir`, and
refuses when the two file names would collide on one projected path. Neither subdir may be `broker`:
that segment is the broker's own state subPath, and a credential directory named for it would mount
one claim subPath twice on one container.

**The broker mounts it as a sibling.** The broker's state mount is the `broker` subPath of the
claim, not the claim root, so `<paths.stateDir>/<broker.chatgpt.subdir>` is not reachable through it
and gets a second `subPath` mount of its own — the same shape the gateway has for its family. The
gateway mounts only its *own* credential subdirectory, so it never receives a path to the broker's
family — `charts/dekopon/ci/verify-init-permissions.sh` asserts exactly that against the rendered manifest.

**`dekopon-brokerd` checks the file itself at startup.** It reads the `authFile` through
`read_trusted_file(.., FileTier::Private, ..)` — regular, owner-owned, `mode & 0o077 == 0`, one hard
link, `O_NOFOLLOW` — requires the parent directory to be owner-only *and writable*, parses the
document, logs how long the access token has left, and refuses to start naming the cause when any of
that fails. A misconfigured credential is a broker that does not come up, not a capability that is in
service and denies every invocation.

### Re-seeding is explicit

`gateway.chatgpt.reseed: true` (and `broker.chatgpt.reseed: true` for the broker's family)
discards whatever is in the volume and copies the Secret in again.
It is separate because it is destructive: the credential in the volume is the live one, and the
exported copy is almost certainly older. Use it after a local re-login and re-export.

It is not self-clearing — while it is `true`, every restart re-seeds — so set it back to `false`
once the pod has rolled. It rolls the pod on its own, because it changes the init container's
arguments, which are part of the pod template.

There is **no `checksum/` annotation for either credential Secret**, unlike every other file the chart
writes. Those annotations exist to push a changed file into a pod that only reads at startup. This
is the one file whose entire purpose is *not* to be pushed in: the copy in the volume is
authoritative, so an annotation would restart a working gateway to achieve nothing at all.

### One replica

`replicas: 1` and `strategy: Recreate` are forced by the rotating credential:
`CredentialFile` serializes refreshes behind a per-process snapshot and an advisory lock on a sibling
`.lock` file, so two holders in one pod coordinate — the gateway's model client and the broker's
credential kind both go through it — but two *pods* sharing one claim would race the rotation and the
loser would be left holding an invalidated refresh token. Neither value is exposed.

### The exported copy goes stale, and that is correct

Nothing detects the drift and nothing repairs it. The exported copy has one job — seeding a *new*
deployment. It is not a backup, and restoring it over a live credential is a way to break a working
pod, not to fix one. Rotation is: log in locally again, re-export, update the Secret,
then either delete the file in the volume and restart, or set `reseed` for one roll.

The broker configuration's legacy `credential`/`agents.<id>.credentials` selection is
[planned to be replaced by public DRNs](design.md#legacy-credential-bindings); that does not
change seed-once behavior or turn the vault copy into the live token.

## The managed provider set comes from the claim

A `broker.yaml` names either `providers` — absolute paths to components baked into the image — or
`providerSet`, a generated lock plus a content-addressed blob store. They are mutually exclusive in
the daemon. `broker.providerSet.*` is what makes the second reachable:

```yaml
broker:
  providerSet:
    enabled: true
    subdir: providers
    mountPath: /var/lib/dekopon/providers
```

```yaml
# broker.yaml
providerSet:
  lockPath: /var/lib/dekopon/providers/providers.lock.yaml
  storePath: /var/lib/dekopon/providers/store
```

`subdir` is a single segment on the state claim, `mountPath` is where the broker container sees it,
and they are separate values because the broker does not mount the claim root — the path on the
claim is not the path the daemon reads. The broker mounts that subdirectory by `subPath`, a sibling
of its ChatGPT credential mount rather than a child of it. The gateway gets no mount for it, which
is the same boundary the two credential directories have.

**The pod does not write the lock or source blobs.** `dekopon-brokerd provider sync` does, and it has
to have completed before the pod rolls: either `broker.providerSync` below, or your own step against
the same claim. `prepare-files` creates the subdirectory if absent and hands it to `65532` as `0700`;
then `precompile` writes compiled artifacts before the broker starts. These steps are
load-bearing, because the broker refuses a lock or a store that is not owned by its own UID and
refuses any ancestor that is group- or world-writable without the sticky bit, and a fresh
`local-path` volume arrives root-owned and `0777`. The chown names that subtree only, and is not
recursive: no other subdirectory of the claim is touched, the gateway's credential directory keeps
its own `65533`, and the blobs the sync step wrote are already `65532`. Precompile mounts only the
provider-set subPath, runs with the broker UID and resource limits, and uses the broker image; pin an
image that includes `provider precompile` (v0.32.0 or later; see `appVersion` in
[Chart.yaml](../charts/dekopon/Chart.yaml) for the current default).

Off by default. A release that leaves `broker.providerSet.enabled` false renders exactly what it
rendered before, and a release that enables it must also stop naming `providers` in its
`broker.yaml`. The chart refuses to render when `subdir` is not one non-dot path segment, when it
collides with either ChatGPT subdirectory, or when `mountPath` is not canonical absolute or would
overlap another of the broker's own mounts.

### Syncing the set with the chart's hook Job

`broker.providerSync.enabled` renders a hook Job that runs `dekopon-brokerd provider sync` with the
image the Deployment runs — the same `image.digest` or `image.tag`, through the same helper — so the
package manager and the broker that consumes its lock cannot drift apart. It compiles new components,
so it takes `broker.resources`, the same limits as the precompile init container. It needs
`broker.providerSet.enabled` and a ConfigMap whose `providers.yaml` key holds the desired set:

```yaml
broker:
  providerSync:
    enabled: true
    configMap: dekopon-config
```

The Job mounts the claim's `providerSet.subdir` at `providerSet.mountPath`, exactly as the broker
does, and writes `providers.yaml`, `providers.lock.yaml` and `store/` there. Point `broker.yaml` at
`<mountPath>/providers.lock.yaml` and `<mountPath>/store`. A root init container reclaims the claim
root as `0:0` `0700`, creates the subdirectory and hands it to `65532` as `0700`, the Deployment's
own step, because the Job runs before the first pod exists; a `65532` init container copies the ConfigMap key in as a `0600`
regular file. The fetch runs as `65532` with no ServiceAccount token. `backoffLimit: 2`,
`activeDeadlineSeconds: 600`, and the Job is removed a day after it finishes. A failed hook fails
the release and the old pod keeps serving. Every sync re-runs it; with the lock unchanged and the blobs present it fetches nothing. The running broker
sees a changed lock within seconds, drains under its shutdown grace and exits; `restartPolicy: Always`
restarts only the broker container on the new lock, and the pod and the gateway stay up. An unchanged
lock restarts nothing.

The hook runs before the release's ordinary objects exist, so the claim and the ConfigMap must
already be there. The chart-created state claim is not, on a first install, so the default Helm hook
refuses to render without `state.existingClaim`.

`broker.providerSync.annotations` replaces the hook annotations entirely. Empty means
`helm.sh/hook: pre-install,pre-upgrade` with `before-hook-creation`. **Argo CD** maps those Helm
hooks to `PreSync`, which runs before every sync wave — before a ConfigMap or claim in an earlier
wave exists — so Argo users set:

```yaml
broker:
  providerSync:
    annotations:
      argocd.argoproj.io/hook: Sync
      argocd.argoproj.io/sync-wave: "-2"
      argocd.argoproj.io/hook-delete-policy: BeforeHookCreation
```

with the ConfigMap in an earlier wave and the claim in the same wave, so a `WaitForFirstConsumer`
claim binds to the hook's pod.

## Checking configuration before a sync

`configCheck.enabled` renders a hook Job that runs `dekopon-brokerd check` and
`dekopon-gatewayd check --probe` on the configuration a release is about to apply, with the
Deployment's image. A problem fails the hook, the release stops before anything changes and the
old pod keeps serving. Each check is the function its daemon runs at boot; see
[`gatewayd.md`](gatewayd.md#check-a-configuration) and the brokerd README.

```yaml
configCheck:
  enabled: true
  broker:
    configMap: dekopon-check-broker-d
    providerSetConfigMap: dekopon-check-config
    workflowsConfigMap: dekopon-check-workflows-d
  gateway:
    configMap: dekopon-check-gatewayd-d
    catalogConfigMap: dekopon-check-agents-d
    skillsConfigMap: dekopon-check-skills
    againstConfigMap: dekopon-gatewayd-d
  env:
    - name: OPENROUTER_API_KEY
      valueFrom:
        secretKeyRef: {name: dekopon-openrouter, key: OPENROUTER_API_KEY}
```

The ConfigMaps it reads must hold the **candidate** files when the hook runs. The live ones do not:
a hook runs before the sync applies them, and once they are applied, Reloader rolls the pod
whatever the hook later says. Render a second set from the same files (a kustomize generator per
directory, `dekopon-check-*`), annotated as hooks of the same phase with a lower sync wave than the
Job, so they exist before it starts. `againstConfigMap` names the live gateway ConfigMap: only a
model absent from it or changed in it is probed, so an unchanged model costs no vendor call.
The check also mounts the Deployment's live catalog read-only at `paths.catalogDir`, using
`gateway.catalog.existingConfigMap` and its key projection or the chart-rendered catalog.
The live gateway configuration must resolve its catalog at that chart-configured path.

A root init container copies the broker ConfigMaps into a `0700` directory owned by `65532`, as
`0600` regular files, because `dekopon-brokerd check` refuses the symlinks a ConfigMap mount is made
of. Wherever the pod gets the chart-rendered `peers.yaml` (`broker.configDirectory.configMap` with
`broker.configDirectory.peers`), the hook copies that same rendered file beside the candidates, from
a `<fullname>-check-peers` hook ConfigMap, and refuses a candidate `peers.yaml` key as the
Deployment does, so the check sees the attestor grants the pod will run with. The gateway
candidates are mounted read-only exactly as the Deployment mounts them, so a mode
production would refuse (a group-writable skill, say) fails the hook too. The broker check runs as
`65532`, resolving `providerSetConfigMap`'s `providers.yaml` into
`<paths.stateDir>/config-check` on the state claim, so blobs persist between runs; that path needs
`state.existingClaim` under the default Helm hook. The gateway check runs as `65533` with `env`,
which should name each probed model's `apiKeyEnv` and nothing else; no ChatGPT login is mounted,
since `check` never probes a subscription model. `backoffLimit: 0`, since a refusal is
deterministic and a retry pays for every probe twice; `activeDeadlineSeconds: 600`.

One cost: the broker check loads providers without the cwasm cache, so every run compiles every
provider in the set; on the RPi that run time is not yet measured.

`configCheck.broker.workflowsConfigMap` (requires `providerSetConfigMap`) names a ConfigMap of
essential workflows, every `*.yaml` key, passed to the broker check as `--workflows`; the file
format is in the brokerd README. After the candidate providers and policy load, each step's argv
runs through its provider's `run-command` and the serving broker's attestation and Cedar
decisions, offline: no model, no HTTP request, no credential. A step is refused as `parse` when the
argv does not propose the step's capability, as `grant` when the attestation or the capability is
denied, and as `budget` when it declares more `requests` than the capability's
`constraints.http.maxRequests`. Each refused step is one problem,
`would break essential workflows: <workflow>: <step>: <check>: <detail>`, and fails the hook.
Empty, the default, runs no workflow check.

A refusing hook blocks every later sync, a fix's included. The override is unsetting
`workflowsConfigMap` (or setting it to `""`) in the commit that carries the fix: the hook renders
from the target revision, so that sync runs the base check without the workflow phase. Never
override with a sync that skips hooks, such as a selective sync or `--strategy apply`: it skips
the base check and the provider-sync hook too.

`configCheck.annotations` replaces the hook annotations entirely. Empty means
`helm.sh/hook: pre-install,pre-upgrade` with `before-hook-creation`, which Argo CD runs as
`PreSync`. **Argo CD's selective sync runs no hooks**: a selective sync is unchecked unless you run
both checks by hand on the rendered candidate first.

## Operator console

`console.enabled` adds a third container that runs `dekopon-console --idle` as UID 65535 from
`console.image`, which the deployment must point at the Dekopon image
`ghcr.io/dekopon-agents/dekopon` at the matching release (including the memory-optimized variant
when selected). The image includes the console on PATH. It mounts the runtime directory and the gateway catalog read-only and a small
memory `/tmp`; it reaches the broker only through IPC group 65534 and gets no model, provider or
gateway credential. Exec starts a fresh process configured by the container environment:

```sh
kubectl exec -it deploy/dekopon -c console -- dekopon-console
```

The broker decides what it may do. `broker.yaml` needs an identity for UID 65535 whose attestor
namespaces cover `console.subject` (`broker.configDirectory.peers` renders it), and Cedar statements that name that identity's principal as
`context.via`. With no such statements the console lists agents and is refused everything else.

## Storage, uninstall, and recovery

The claim carries `helm.sh/resource-policy: keep`, so `helm uninstall` leaves the live model
credentials intact. Removing retained state is a separate operator action.

It carries `argocd.argoproj.io/sync-options: Prune=false,Delete=false` for the same reason, because
Helm's annotation means nothing to a GitOps controller. Argo CD syncing with `prune: true` deletes
any live object the rendered desired state stops containing, and this claim stops being rendered the
moment somebody sets `state.existingClaim` or the chart source fails to resolve. `Prune=false`
covers the sync, `Delete=false` covers the `Application` itself being deleted, and Argo reads both
off the live object. Both annotations follow `state.keepOnUninstall`.

`local-path` is the target cluster's only StorageClass, it is RWO, and **`ALLOWVOLUMEEXPANSION` is
`false`**. `state.size` is final for the life of the volume.

`state.existingClaim` points the pod at a claim you manage. Init keeps its root at `0:0:0700`
and gives each daemon only its own private subdirectory, as in [Paths the chart owns](#paths-the-chart-owns).

## seccomp

`RuntimeDefault`, on the pod and on the init container. Wasmtime JITs through Cranelift and needs
`mmap` with `PROT_EXEC`, `mprotect`, `memfd_create`, and `SIGSEGV`/`SIGBUS` handlers for guard-page
traps. `RuntimeDefault` permits all of that, and a provider has been proven to load under Docker's
equivalent default profile. Do not narrow this to a hand-written profile without proving a component
loads; the failure mode is a trap inside the JIT, not a clean error.

`readOnlyRootFilesystem` is `true` for every container. Neither daemon writes outside its mounted
volumes. Broker `/tmp` remains memory-backed; gateway `/tmp` stores private, short-lived chat payloads
on disk. The ChatGPT credential, when that model kind is in use, gets its own writable directory
on the claim; see
[The ChatGPT credential is seeded once](#the-chatgpt-credential-is-seeded-once).

## Two version numbers

The chart and the application it deploys are versioned independently, and both numbers are real.

| | Where it comes from | What it means |
|---|---|---|
| **chart version** (`Chart.yaml: version`) | a `dekopon-chart-*` Git tag | the version of *this chart* — its templates, defaults, and documentation |
| **appVersion** (`Chart.yaml: appVersion`) | the application release the chart deploys | what `image.tag` defaults to, and what the pod actually runs |

They move for different reasons. A templating fix ships as a `dekopon-chart-*` tag and changes no
`appVersion`; a new application release moves `appVersion` and, with it, the image the chart pulls.
`v*.*.*` tags publish crates, release archives, and the container image; `dekopon-chart-*` tags
publish only the chart. That is the whole reason for two tag namespaces — a chart bug must not
force an application release, and an application release must not republish an unchanged chart.

The release-prep [Chart.yaml](../charts/dekopon/Chart.yaml) declares the chart version and
`appVersion` together with the workspace version, and `image.digest` defaults to empty, so the
chart renders the `v<appVersion>` tag of the same commit. The application version is not decorative: `dekopon.labels` renders it as
`app.kubernetes.io/version` on every object, so an `appVersion` behind the image is a cluster
answering `kubectl get pods -l app.kubernetes.io/version` with a version nothing is running, and
every dashboard and alert built on that label reporting the same wrong number. It has to move in
the application's release-prep commit, alongside the workspace version. Its floor is `0.4.0`:
below that no container image exists, so the chart's default would pull nothing.

The image workflow publishes under the Git tag, so the tag carries a `v`. An empty `image.tag`
therefore renders `v` + `appVersion`:

```
ghcr.io/dekopon-agents/dekopon:v0.54.0
```

There is no `latest`. `image.digest` is empty by default; set it to pin across the
`linux/amd64` + `linux/arm64` index, and the index digest is what `gh attestation verify` attests.

## Publishing

[`.github/workflows/chart-publish.yml`](../.github/workflows/chart-publish.yml)
triggers on `dekopon-chart-*` tags only. Before tagging, write a dated
`[dekopon-chart-<VERSION>]` section in [`CHANGELOG.md`](../CHANGELOG.md) from
`git log dekopon-chart-<PREV>..HEAD --format=%B | grep '^Changelog: '`. Pull-request
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

CI packages the chart on every run and diffs the archive's rendered output against the source
tree's, so a pushed tarball is complete and renders identically to the checkout.

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

The chart source form below was documented against **ArgoCD v3.3.6** on k3s v1.36.3.
The pinned homelab Application above uses this registry source alongside its Git configuration source:

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
    targetRevision: 0.41.0                    # the CHART version, not appVersion
    helm:
      valueFiles: []
      values: |
        ...
  destination:
    server: https://kubernetes.default.svc
    namespace: dekopon
```

Two mechanics that are easy to get wrong:

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
remove the need to register. So an `Application` needs this prerequisite alongside it:

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

`syncPolicy.automated.prune: true` cannot take the live model credentials: see
[Storage, uninstall, and recovery](#storage-uninstall-and-recovery).

## Configuration values

`serviceAccount.create` defaults to `false`; `serviceAccount.name` selects an existing account,
or `default` when empty. With `create: true`, the chart creates an account named by
`serviceAccount.name` or the release fullname and applies `serviceAccount.annotations` (default `{}`).
The Deployment uses that account. Both the pod and a chart-created account disable automatic token
mounting; explicit broker-only projections are the opt-in below. The chart grants no RBAC permissions.

Each operator-supplied file is either inline, in which case the chart writes a Secret, or a reference
to an object that already exists. Setting both is an error.

| Value | Inline | Existing object |
|---|---|---|
| `broker.yaml` | `broker.config.inline` | `broker.config.existingSecret` / `.existingSecretKey` |
| `policies.cedar` | `broker.policies.inline` | `broker.policies.existingSecret` / `.existingSecretKey` |
| `broker-credentials.yaml` | `broker.credentials.inline` | `broker.credentials.existingSecret` / `.existingSecretKey` |
| private secret map | `broker.secretMap.inline` | `broker.secretMap.existingSecret` / `.existingSecretKey` |
| `gatewayd.yaml` | `gateway.config.inline` | `gateway.config.existingSecret` / `.existingSecretKey` |
| agent catalog | `gateway.catalog.inline` | `gateway.catalog.existingConfigMap` / `.existingConfigMapKey` |
| gateway ChatGPT credential | `gateway.chatgpt.inline` | `gateway.chatgpt.existingSecret` / `.existingSecretKey` |
| broker ChatGPT credential | `broker.chatgpt.inline` | `broker.chatgpt.existingSecret` / `.existingSecretKey` |

Use `existingSecret` for credentials and the private map. An inline value is stored in the release,
returned by `helm get values`, and usually committed.

### Configuration directories

`broker.configDirectory.configMap` replaces `broker.yaml` and `policies.cedar` with a ConfigMap of
fragments: the init container copies every `*.yaml` and `*.cedar` key into
`<paths.configDir>/broker.d` as `65532`-owned `0600` files, asserts each one, hands the directory
to `65532` as `0700`, and the broker starts with `--config <paths.configDir>/broker.d`. Clear
`broker.config.inline` (it has a default) and leave `broker.config.existingSecret` and
`broker.policies.*` empty; the chart refuses to render otherwise. Credentials, the secret map,
bootstrap files and the ChatGPT seed still land beside `broker.d`, and a fragment names them by
path (`credentialsPath: /etc/dekopon/broker-credentials.yaml`).

`broker.configDirectory.peers` (on by default) adds a chart-rendered `peers.yaml` with the pod's
own identities: `65532` as `dekopon-probe`, `65533` as `dekopon-gatewayd` with `attestor: {}` when
the gateway is enabled, and `65535` as `dekopon-console` with `attestor.namespaces:
[<console.subject>]` when the console is enabled with a subject. A ConfigMap key named
`peers.yaml` fails the init container, and the config-check hook, while this is on.

`gateway.configDirectory.configMap` mounts the ConfigMap directly at `paths.gatewayConfigDir`;
`--config` points to that directory, and `gateway.config` must be empty. A projected key is
root-owned and non-group/world-writable. No `subPath` or init copy pins its contents, and no
gateway edit rolls the pod: the gateway swaps in only the live set named in
[Why the broker still needs an init container](#why-the-broker-still-needs-an-init-container),
refuses every other edit as restart-required, and the operator rolls the pod for those. The chart
cannot hash an external broker ConfigMap: keep broker fragments (including `host.yaml`) on the
operator's restart path.
For Reloader, annotate that broker ConfigMap `reloader.stakater.com/match: "true"` and set
`reloader.stakater.com/search: "true"` in `deploymentAnnotations`; do not include gateway-only
ConfigMaps on the shared-pod Reloader list. Secret material still requires its established
restart/credential lifecycle.

`broker.secretBootstrapFiles` copies operator-managed Secret keys through the existing root init
boundary into `<paths.configDir>/<file>` as broker-UID-owned `0600` files, which is how token/session
files for 1Password, Vault, AWS, GCP, Azure, or Kubernetes API adapters reach the stock chart.
`broker.secretSourceVolumes` separately mounts explicitly named Kubernetes volumes read-only into the broker
and nowhere in the gateway. A Secret/ConfigMap volume keeps its AtomicWriter symlink layout and is
consumed through `source.kind: kubernetesProjection`; the map's root must equal the corresponding
`mountPath`. Names, canonical absolute mount paths, required volume maps, and overlap with chart
paths are checked at template time. Bootstrap tokens requiring the ordinary `0600` loader use `broker.secretBootstrapFiles`; no custom
init container is needed. See [`secrets.md`](secrets.md).

A Secret source mount:

```yaml
broker:
  secretSourceVolumes:
    - name: api-secret
      mountPath: /var/run/dekopon-secrets/api
      volume:
        secret:
          secretName: api-secret
```

A projected ServiceAccount token uses the same value, with no init copy so kubelet rotation stays
visible. The token volume is mounted only into the broker, never the gateway or copy init container:

```yaml
serviceAccount:
  create: true
broker:
  secretSourceVolumes:
    - name: api-token
      mountPath: /var/run/dekopon-secrets/api-token
      volume:
        projected:
          sources:
            - serviceAccountToken:
                audience: <aud>
                expirationSeconds: 600
                path: token
```

Replace `<aud>` with the receiving service's audience. In the private secret map, use:

```yaml
source:
  kind: kubernetesProjection
  root: /var/run/dekopon-secrets/api-token
  key: token
  declaredOrigin: serviceAccountToken
```

This origin requires broker `0.23.0` or later, supported by the chart's default `appVersion`.
The broker reads per invocation rather than caching the token.

The chart refuses to render, with a message, when: broker UID/GID is changed or `fsGroup` is set; a required file has no source; both sources are set for one file; an inline `broker.yaml`
names `policiesPath`, `credentialsPath`, `secretMapPath`, or `capabilities` with no corresponding value supplied;
an inline `broker.yaml`'s `identities` never map the broker's own UID, which the startup and
readiness probes connect as; `paths.catalogDir` is inside `paths.configDir`; a daemon uses
`configDirectory` without its `drainBudget.assumedBrokerShutdownGraceMs` or
`drainBudget.assumedGatewayShutdownGraceMs` value; or `terminationGracePeriodSeconds` is shorter than
the two drains it has to cover in sequence. When provider storage is enabled, its root and key directory must also be absolute, disjoint from one
another, and pairwise non-overlapping with every chart-owned mount (`config`, runtime, the state claim,
catalog, `/tmp`, and both projected configuration/key sources); a nested mount would otherwise
shadow or destructively replace those files. Every one of those is a mistake whose only other
symptom is a pod that starts and never becomes ready.

The chart's default `broker.config.inline` is the cli-probe example from the broker's own README,
moved onto these paths: a real deny-by-default configuration that starts, loads the baked
`cli-probe-provider.wasm`, and maps the pod's probe UID without granting an executable capability. It is
there so you can install the chart and watch a broker become ready before you give it anything that
matters. Replace it. `serverLimits`, `brokerLimits` and `hostLimits` default each field independently;
an omitted `serverLimits.maxFrameBytes` is derived from the configured payload bounds with a 2 MiB
floor. The default inline example explicitly sets a 2 MiB ceiling; increase or omit that field when
raising payload bounds. See the
[broker configuration contract](../crates/dekopon-brokerd/README.md#configuration).
`gateway.enabled` is `false` by default because a gateway needs transport configuration, a model
endpoint, an agent catalog and any credentials those integrations require; the chart supplies none
of those choices.

Two annotation maps ship, and they are not interchangeable:

| Value | Rendered on | Read by |
|---|---|---|
| `deploymentAnnotations` | the Deployment object's own `metadata.annotations` | a controller that watches the workload object, such as a Secret-watching restarter — Stakater Reloader's `reloader.stakater.com/*` keys belong here |
| `podAnnotations` | the pod template, so every pod the Deployment creates carries them | anything that reads a running pod: scrapers, meshes, admission webhooks |

An annotation on the pod template is invisible to a controller watching Deployments, and one on the
Deployment never reaches a pod, so a restarter configured through the wrong one silently does
nothing. Both default to empty and neither changes a rendered manifest until it is set.

`gateway.service` optionally creates a ClusterIP Service and matching named gateway container port.
The chart does not create an Ingress: the operator must route only the configured
callback path and terminate public TLS outside the pod. A Kubernetes Service cannot reach loopback,
so the corresponding transport must bind `0.0.0.0:<gateway.service.port>`. The TCP readiness probe
keeps the Service endpoint unavailable when the configuration and chart port disagree.

## What is not proven

The reference deployment above establishes declared configuration and a dated rollout report, not
a fresh verification of every feature or this release. The following local/CI checks are recorded
by this guide; they were not rerun as part of the 2026-10-23 documentation review.

- Local render evidence is not a cluster apply. The chart has been linted, rendered, and schema-validated
  with `kubeconform` against Kubernetes 1.29, 1.33 and 1.36, and the init container's rendered
  command has been run verbatim on `linux/arm64` and `linux/amd64` under its rendered
  `securityContext` against a fixture built to match a projected volume's symlink layout.
- Local fixtures do not prove the complete current provider/model deployment. The
  real-daemon Linux `ipc_process.rs` suite proves mapped/unmapped cross-UID connections and
  server pin/live-peer refusals. `charts/dekopon/ci/verify-init-permissions.sh` executes actual rendered init
  commands, checks mounts/identities, and runs distinct-UID OS processes against that layout;
  its Python socket fixture is not a real daemon, and its config-check step runs the
  `dekopon-brokerd` the same CI run built, on the image's runtime base, not a published image. The image an empty `image.tag`
  resolves to does exist — `ghcr.io/dekopon-agents/dekopon` carries a tag for every application
  release — but those local checks alone do not observe a pod boot.
- The reference Application and its registry registration now exist in rpi-homelab. The recorded
  previous Synced/Healthy rollout supersedes the original claim that no Application had been
  created; this documentation review did not independently run an anonymous `helm pull`, inspect
  ArgoCD or verify the current imageID.
- The ChatGPT seed-once behaviour is proven against the rendered init container across a cold
  start, a simulated rotation, two restart shapes, and the gated re-seed — including a negative
  control confirming that removing the `[ -e ]` test does revert the credential — for both the
  gateway's family and the broker's, plus the assertions that the two are separate documents, that
  re-seeding one leaves the other alone, and that the gateway's rendered mounts reach neither the
  broker's credential directory nor the claim as a whole. But no real daemon has refreshed a real
  token through it. What was exercised is the file-level contract (`0600`, one link, a `0700`
  writable directory, temp sibling plus rename) as each family's own daemon UID — `65533` for the
  gateway's, `65532` for the broker's — not a live refresh against OpenAI.
- No current provider-sync hook completion was independently observed in this review. Its recorded
  rendered init and sync checks ran in Docker under their rendered users and capabilities, against a fresh root-owned `0777` volume and
  a `65532`-owned `0700` claim root, mounted by `subPath` as the Job mounts it: the sync fetched every
  provider, and a second run fetched nothing.
- No current config-check hook completion was independently observed in this review. The guide
  records checks on deployment configuration outside a cluster, including `--probe` against
  OpenRouter; those historical checks do not prove current credentials or model availability.
- The `PodSecurity` `restricted` profile would reject this pod: the init container runs as root.
  `baseline` is fine.

## Optional provider-storage claim

`providerStorage.enabled` creates (or mounts) a claim physically separate from `state`, mounted only
into the broker at `/var/lib/dekopon-provider-storage`. A generated claim always carries
`helm.sh/resource-policy: keep` and `argocd.argoproj.io/sync-options: Prune=false,Delete=false`, so
neither an uninstall nor a GitOps prune takes durable provider data; an existing claim remains
operator-owned. Rendering fails when the resolved state and provider-storage claim names are equal.

The gateway does not mount the provider-storage PVC. Every chart mount path must be a canonical
absolute sequence of safe non-dot segments (no repeated slash). The storage root must not overlap
another chart mount, projected init sources, or image-reserved paths including
`/opt/dekopon/providers` and `/opt/dekopon/optional-providers`. The chart still reserves the
optional path even though the current image creates no such directory. Invalid combinations fail
during `helm template` before a volume can shadow configuration, packaged providers, or init-script
text. Neither `storage-probe` nor the malicious `memory-reservation-probe` fixture is packaged,
and no memory-chat component is bundled. Supply a compatible memory provider explicitly through
a reviewed provider set if one is needed.

Directory names under the root are unkeyed SHA-256 derivations of the chat scope, and the store
makes no encryption-at-rest claim. The provider-storage filesystem must support retained
directory-descriptor-relative no-follow opens, advisory locks, same-directory rename, and
file/directory sync. A stuck native syscall may exceed
the configured shutdown grace, and malicious same-UID filesystem mutation is outside the claim.
