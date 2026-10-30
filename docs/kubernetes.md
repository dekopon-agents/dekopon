# Run Dekopon on Kubernetes

Kubernetes is the preferred deployment. People moving from OpenClaw or Hermes to a Mac mini can
[run on a Mac](run-on-mac.md) instead.

The operator reference for the [Helm chart](../charts/dekopon/), for readers who know Kubernetes and
Helm. The chart places files and sets their owners and modes; their contents are defined by the
[broker configuration reference](../crates/dekopon-brokerd/README.md) and the
[gateway configuration reference](gatewayd.md). The [container image guide](container-image.md)
covers the image.

The chart runs `dekopon-brokerd` and `dekopon-gatewayd` as one pod under separate UIDs. It is
published to `oci://ghcr.io/dekopon-agents/charts/dekopon`; the chart and the image are public GHCR
packages. The reference deployment is a single-node arm64 k3s cluster that runs the
[`-wasmtime-optimization` image](container-image.md#publication) and consumes the chart from Argo CD
by registry path.

Start with [Install](#install). The sections from [What it deploys](#what-it-deploys) to
[Draining](#draining-takes-both-graces-in-sequence) define the pod. The
[ChatGPT credential](#the-chatgpt-credential-is-seeded-once),
[managed provider set](#the-managed-provider-set-comes-from-the-claim) and its
[sync hook](#syncing-the-set-with-the-charts-hook-job),
[configuration check](#checking-configuration-before-a-sync) and
[operator console](#operator-console) are opt-in. The rest is reference, ending with
[What is not proven](#what-is-not-proven).

## Install

Before installing:

- Use Kubernetes **1.29 or newer**, for native sidecars, and Helm **3.8 or newer**, for OCI
  registries.
- Set a persistent `state.storageClassName` (default `local-path`) or an existing
  `state.existingClaim`. Read [Storage, uninstall, and recovery](#storage-uninstall-and-recovery)
  before replacing a deployment.
- Use a namespace that admits a root init container: Pod Security `restricted` rejects the pod and
  `baseline` admits it.
- Write `my-values.yaml` from the [chart values](../charts/dekopon/values.yaml) or the example below,
  and create the Secrets and ConfigMaps it references in the release namespace first.

The defaults start only the broker, with no executable capabilities. A chat deployment also needs
`gateway.enabled: true`, broker policy and identities, gateway configuration, an agent catalog, and
chat and model credentials. [ChatGPT credentials](#the-chatgpt-credential-is-seeded-once) and
[managed provider sets](#the-managed-provider-set-comes-from-the-claim) need extra steps.

From the registry, using **chart version `0.61.0`**, not application version `0.74.0`:

```console
helm show chart oci://ghcr.io/dekopon-agents/charts/dekopon --version 0.61.0
helm upgrade --install dekopon oci://ghcr.io/dekopon-agents/charts/dekopon \
  --version 0.61.0 -n dekopon --create-namespace -f my-values.yaml
```

The Helm CLI takes the `oci://` prefix; an [Argo CD](#consuming-it-from-argocd) chart source does
not.

From a checkout, at the repository root:

```console
helm lint charts/dekopon
helm template dekopon charts/dekopon
helm template dekopon charts/dekopon -f charts/dekopon/values-pr-summarizer-linter.yaml
helm upgrade --install dekopon charts/dekopon -n dekopon --create-namespace \
  -f my-values.yaml
```

[`values-pr-summarizer-linter.yaml`](../charts/dekopon/values-pr-summarizer-linter.yaml) is the
[PR summarizer and linter](https://github.com/dekopon-agents/dekopon-provider-gh/blob/main/examples/pr-summarizer-linter/README.md)
deployment as chart values: Slack Agent sessions, one agent, six narrow `gh` capabilities, a
broker-injected token from an existing Secret, and every broker decision exported as an audit log
record over OTLP. It may post one review comment and cannot approve, request changes or merge.

## What it deploys

One `Deployment` with `replicas: 1` and `strategy: Recreate`, neither configurable
([why](#the-chatgpt-credential-is-seeded-once)). The chart creates no `Ingress`; an optional
ClusterIP `Service` exposes the gateway's webhook or VM proxy port
([Gateway Service and VM proxy](#gateway-service-and-vm-proxy)). The broker has no TCP listener.

| Container | Kind | Runs |
|---|---|---|
| `prepare-files` | init, **root** | `busybox`: copies broker files into a memory `emptyDir` and prepares the claim's subdirectories |
| `precompile` | init, only with `broker.providerSet.enabled` | `dekopon-brokerd provider … precompile`: compiles the managed provider set |
| `broker` | native sidecar (`restartPolicy: Always`) when the gateway is enabled, otherwise the pod's only regular container | `dekopon-brokerd --config /etc/dekopon/broker.yaml` (`/etc/dekopon/broker.d` with `broker.configDirectory.configMap`) |
| `gateway` | regular container, only when `gateway.enabled` | `dekopon-gatewayd --config /etc/dekopon-gateway/gatewayd.yaml` (the directory `/etc/dekopon-gateway` with `gateway.configDirectory.configMap`) |
| `console` | regular container, only when `console.enabled` | `dekopon-console --idle` |

`broker.providerSync.enabled` adds the [sync hook Job](#syncing-the-set-with-the-charts-hook-job),
and `configCheck.enabled` the [configuration check hook Job](#checking-configuration-before-a-sync).

As a sidecar, the broker starts and passes its startup probe before the gateway starts, and stops
only after the gateway exits; `dekopon-gatewayd` exits non-zero when the broker does not answer at
startup. That ordering is why [draining takes both graces](#draining-takes-both-graces-in-sequence).
`Chart.yaml` declares `kubeVersion: ">=1.29.0-0"`, because an older cluster drops the sidecar field
and the pod deadlocks on an init container that never exits.

## The broker init container

`dekopon-brokerd` validates every path it touches, in tiers, and refuses to serve when one is
wrong:

| Tier | Applies to | Rule |
|---|---|---|
| A | `broker-credentials.yaml`, `secret-map.yaml` | rejected if `mode & 0o077 != 0` |
| B | `broker.yaml`, `policies.cedar`, provider `.wasm` files and their parents | rejected if `mode & 0o022 != 0` |
| C | `providerSet.storePath` (the parent of generated `cwasm`) and the parent of a `chatgptSubscription` `authFile` | must be `0700` and owned by the runtime UID |
| D | every ancestor up to `/` | must be a directory that is not group- or world-writable unless sticky |
| E | gateway config, catalog and skills | root or daemon UID, no group/world write; projected symlinks only on a trusted read-only mount |

Tier A and B also require `uid == geteuid()`, `nlink == 1` and an open with `O_NOFOLLOW`. Broker
IPC uses the owner, group and mode checks of the
[current boundary](security-model.md#current-local-process-boundary) instead.

No Kubernetes volume can hold an A or B file. A `secret` or `configMap` key is a symlink, which
`O_NOFOLLOW` refuses with `ELOOP`; a `subPath` file is `root:root` `0644`; and `fsGroup` adds the
group-read bit that Tier A rejects, so setting `podSecurityContext.fsGroup` is a render refusal.
`prepare-files` instead copies each broker file from `/dekopon-source`, a projected volume that only
init mounts:

```sh
install -m 0600 -o 65532 -g 65532 /dekopon-source/broker.yaml /etc/dekopon/broker.yaml
```

Each copy is a fresh regular file with one link, mode `0600`, owned by `65532`. Init asserts each
with `stat` before handing its directory over, so a wrong file fails init by name. Init runs as root
with only `CHOWN` and `FOWNER`, not `DAC_OVERRIDE`, and reclaims each directory to `root` before
writing, which keeps it idempotent when an `emptyDir` survives an in-place restart.

Broker files are copied at pod start. Inline `broker.config`, `broker.policies`, `broker.secretMap`
and `broker.credentials` roll the pod through `checksum/` annotations. An existing broker Secret or
a `broker.configDirectory` ConfigMap, including a `host.yaml` fragment, needs an operator-managed
restart such as [Reloader](#configuration-directories).

Gateway configuration is not copied. The gateway mounts its Secret or ConfigMap read-only at
`paths.gatewayConfigDir`, without `subPath`, and trusts only a root- or daemon-owned projection with
no group or world write on a read-only mount. It re-reads the source every two seconds and swaps in
`models`, `extraCABundles`, `routes`, `stopWords`, transport and session `indicators`, and the
catalog (agents and skills). It refuses an edit to `broker`, `transports`, `sessions`,
`shutdownGraceMs`, `telemetry`, `metering` or `proxy` as restart-required
(`gateway_reload_refused`) and keeps its current configuration; the operator must roll the pod. The
gateway sees neither broker policy nor credential projections.

These need no init copy:

- **Chat, model and image-model credentials.** `gatewayd.yaml` names environment variables, never
  values, so these are ordinary `secretKeyRef` entries under `gateway.env`.
- **OTLP ingest auth.** The OpenTelemetry SDK reads it from `OTEL_EXPORTER_OTLP_HEADERS`; the
  broker's `telemetry` block has no credential field. For a receiver on a private CA,
  `otlp.caBundle.configMap` and `otlp.caBundle.key` name PEM roots that both daemons read as
  `OTEL_EXPORTER_OTLP_CERTIFICATE`.
- **Agent catalog and skills.** The catalog ConfigMap mounts at `paths.catalogDir`.
  `gateway.skills.configMap` with `gateway.skills.items` maps flat keys to nested paths such as
  `pull-request-review/SKILL.md` under `paths.skillsDir`, read-only in the gateway and console and
  absent from the broker. The skill loader accepts only the root-owned, read-only AtomicWriter
  `..data` chain, so set no group- or world-writable `mode`. Skills are public model text, never
  credentials. Edits apply on the next gateway reload.
- **Provider components.** The image bakes `/opt/dekopon/providers/*.wasm` owned by `65532:65532`
  in a `65532`-owned `0755` directory, which satisfies Tier B.
- **ChatGPT credentials.** Each daemon writes its credential back, so it needs a writable directory
  on the claim instead: [The ChatGPT credential is seeded once](#the-chatgpt-credential-is-seeded-once).

## UIDs

| Container | UID:GID |
|---|---|
| `prepare-files` and the hooks' root init steps | `0:0`, adding only `CHOWN` and `FOWNER` |
| `broker`, `precompile`, provider sync, broker check (the pod default) | `65532:65532` |
| `gateway`, gateway check | `65533:65533` |
| `console` | `65535:65535` |

Every container carries supplementary group `65534`, which reaches only the broker's `0660` socket
in its `0710` IPC directory. The [current local process boundary](security-model.md#current-local-process-boundary)
states what this separation protects.

- Broker UID/GID `65532:65532` is fixed: the image's providers are owned by `65532`, and the broker
  loads a provider only when its owner equals the broker's euid. The chart refuses to render a
  changed broker UID/GID, any `fsGroup`, or `supplementalGroups` other than `[65534]`, even with a
  custom image. Each container sets its own UID, so the shared `securityContext` cannot override it.
- Gateway configuration must pin `broker.serverUid: 65532`. The chart refuses inline gateway
  configuration without it; an existing Secret or ConfigMap directory is opaque, so its owner must
  set it.
- Broker `identities` must map gateway UID `65533` and, separately, `65532` for the probes. The
  chart refuses an inline `broker.yaml` that does not map `65532`;
  [`broker.configDirectory.peers`](#configuration-directories) renders both.
- The gateway receives no broker configuration, secret or storage mount; the broker receives no
  gateway configuration, model credential or gateway `/tmp`.

## Paths the chart owns

Your `broker.yaml` and `gatewayd.yaml` must name files at these paths. The chart places files; it
does not rewrite configuration.

- `paths.configDir` (`/etc/dekopon`) is a memory-backed `emptyDir` (`volumeSizes.config`, 8Mi) that
  init fills and hands to `65532` as `0700`, so broker credentials never reach the node's disk.
- `paths.runtimeDir` (`/run/dekopon`) is a memory-backed `emptyDir` (`volumeSizes.runtime`, 1Mi),
  `65532:65534` `0710`, holding the socket.
- `paths.stateDir` (`/var/lib/dekopon`) is the state claim, which retains the model credentials. Its
  root stays root-owned `0700` and no daemon mounts it: each daemon mounts only its own
  subdirectories by `subPath`, so neither can rename the other's. Private directories are `0700`;
  private files are `0600` with one link.
- `paths.gatewayConfigDir` (`/etc/dekopon-gateway`) is the gateway's read-only source: one Secret key
  projected as `gatewayd.yaml`, or a ConfigMap fragment directory mounted intact.
- Broker, gateway and console each get their own `/tmp`. The broker's and console's are
  memory-backed (`volumeSizes.tmp` and `volumeSizes.consoleTmp`, 16Mi each); the gateway's is a
  disk-backed `emptyDir` (`volumeSizes.gatewayTmp`, 1152Mi).

| File | Path | Tier | Written by |
|---|---|---|---|
| `broker.yaml` | `/etc/dekopon/broker.yaml` | B | init container |
| `policies.cedar` | `/etc/dekopon/policies.cedar` | B | init container |
| `broker-credentials.yaml` | `/etc/dekopon/broker-credentials.yaml` | A | init container |
| private `secret-map.yaml` | `/etc/dekopon/secret-map.yaml` | A | init container; broker only |
| broker private files | `/etc/dekopon/private/<file>` (`broker.privateFiles`) | A, in a broker-owned `0700` directory | init container; broker only |
| optional secret source projection | operator-selected absolute path | source-specific | mounted read-only into broker only through `broker.secretSourceVolumes` |
| `gatewayd.yaml` | `/etc/dekopon-gateway/gatewayd.yaml` | gateway projection | read-only Secret mount, gateway only |
| broker socket | `/run/dekopon/broker.sock` | protected IPC | the broker, at bind |
| broker assets | `/var/lib/dekopon-assets` (`brokerAssets.rootPath`) | owned `0700` dir, broker-only disk `emptyDir` (`volumeSizes.brokerAssets`, 1152Mi) | init sets ownership; broker writes spools and outputs; match `brokerAssets.rootPath` and `brokerAssets.maxInFlightBytes` in the broker's `assets` section |
| agent catalog | `/etc/dekopon-catalog/dekopon.yaml` | E | read-only ConfigMap mount |
| gateway ChatGPT credential | `/var/lib/dekopon/chatgpt/chatgpt-auth.json` | none | init container, **once**; then `dekopon-gatewayd` owns it |
| broker ChatGPT credential | `/var/lib/dekopon/broker-chatgpt/chatgpt-auth.json` | A + writable parent | init container, **once**; then `dekopon-brokerd` owns it |
| OAuth2 refresh records | `broker.oauth2Records.mountPath` (default `/var/lib/dekopon/oauth2-records`), `broker.oauth2Records.subdir` on the claim | owned `0700` dir, broker-only, always mounted | the directory by the init container; records by `dekopon-brokerd` at an `oauth2Refresh` `recordPath` |
| providers | `/opt/dekopon/providers/*.wasm` | B | baked into the image |
| managed provider set | `/var/lib/dekopon/providers/` | owned `0700` dir, broker-only | the directory by the init container; compiled components by `precompile`; the lock and the store by the sync step |
| gateway conversation journal | `gateway.journal.mountPath` (default `/var/lib/dekopon-gatewayd/journal`), `gateway.journal.subdir` on the claim | owned `0700` dir, gateway-only | the directory by the init container when `gateway.journal.enabled`; `0600` files by `dekopon-gatewayd`. Set `sessions.journal.path` in `gatewayd.yaml` to the mount path |
| gateway chat assets | `gateway.assets.mountPath` (default `/var/lib/dekopon-gatewayd/assets`), `gateway.assets.subdir` on the claim | owned `0700` dir, gateway-only | the directory by the init container when `gateway.assets.enabled`. Set `sessions.assets.path` to the mount path |
| gateway wake store | `gateway.wakes.mountPath` (default `/var/lib/dekopon-gatewayd/wakes`), `gateway.wakes.subdir` on the claim | owned `0700` dir, gateway-only | the directory by the init container when `gateway.wakes.enabled`. Set `sessions.wakes.path` to a file under the mount path |

`volumeSizes.gatewayTmp` holds the gateway's default 1 GiB attachment budget
(`sessions.assetRetentionBytes`) plus scratch headroom. `volumeSizes.brokerAssets` holds the same
budget plus `brokerAssets.maxInFlightBytes` (64 MiB), because unlinked broker outputs stay on it until
the gateway closes them. Raise each with its budgets. The chart refuses `gateway.journal.enabled`
without `gateway.assets.enabled`, and the reverse; with a journal, attachments survive restarts on
the claim, and without one they stay in gateway `/tmp` ([Chat assets](gatewayd.md#chat-assets)).
Kubelet enforces `emptyDir.sizeLimit` by evicting the pod; it reserves nothing.

## Probes

The image has no shell and the broker no HTTP endpoint, so broker probes exec
`dekopon-brokerd probe --socket /run/dekopon/broker.sock` as UID `65532`. The probe authenticates
over the real socket with `SO_PEERCRED` in both directions, pins the server to its own euid, and
gets back the capability list policy exposes to it, emitting **no audit record**
([health probe](../crates/dekopon-brokerd/README.md#health-probe)).

- **`startupProbe`**: every 5 s, 10 s timeout, 60 failures (five minutes). The broker binds its
  socket only after every component validates, so an answer means fully started. The budget covers
  validation, and compilation too when `compileOnLoad: true`. With a managed provider set,
  `precompile` builds or repairs the compiled cache before the broker starts, even with
  `compileOnLoad: true`; keep `providerSet.storePath`, which holds that cache, off the memory-backed
  `/tmp`. Cache faults and resetting `store/cwasm` are in the
  [compilation cache reference](../crates/dekopon-brokerd/README.md#compilation-cache-and-the-concurrent-memory-budget).
- **Broker `readinessProbe`**: every 30 s, 10 s timeout, 3 failures. It keeps pod readiness truthful
  and keeps the optional Service from routing to an unavailable broker.
- **Gateway `readinessProbe`**, only with `gateway.service.enabled`: TCP on the webhook port every
  10 s. It fails closed when `gatewayd.yaml` binds loopback, names a different port, or configures no
  inbound listener.
- **No `livenessProbe`.** An automatic restart could kill a broker mid-invocation or lose an
  acknowledged in-memory webhook delivery.

## Draining takes both graces, in sequence

The kubelet stops the gateway first and starts stopping the broker sidecar only once the gateway is
gone, so `terminationGracePeriodSeconds` (default **270**) must cover the sum, not the larger, of
the gateway's `shutdownGraceMs`, the broker's `serverLimits.shutdownGraceMs` (both default `120000`)
and `drainBudget.bufferSeconds` (default 30). Whatever is still draining at the deadline is
`SIGKILL`ed: a model turn someone is waiting on, or a broker invocation in progress. The chart
asserts the sum at template time:

```console
$ helm template dekopon charts/dekopon -f charts/dekopon/values-pr-summarizer-linter.yaml \
    --set terminationGracePeriodSeconds=180
Error: ... terminationGracePeriodSeconds is 180, but the containers stop in sequence: dekopon-gatewayd
drains for 120000 ms and then dekopon-brokerd drains for 120000 ms ... That needs 270 seconds.
```

The buffer covers `SIGTERM` delivery, the kubelet's per-container bookkeeping, and each daemon's
telemetry flush, which can spend its whole `exportTimeoutMs` against a restarting collector. Without
the gateway, only the broker's drain and the buffer count.

Each grace comes from the daemon's **inline** `shutdownGraceMs`, else
`drainBudget.assumedBrokerShutdownGraceMs` or `drainBudget.assumedGatewayShutdownGraceMs` (default
`null`), else the daemon default `120000`. A daemon on `configDirectory` must set its assumption, or
the chart refuses to render, because it cannot read that directory. Set one too for an
`existingSecret` that uses a non-default grace. The assumptions only inform the assertion; they
configure neither daemon.

## The ChatGPT credential is seeded once

`dekopon-gatewayd auth chatgpt login` is a device-authorization flow that waits for a human with a
browser, so a `kind: chatgptSubscription` model is seeded from a credential exported after a local
login with `dekopon-gatewayd auth chatgpt export --expose-credential`. The
[ChatGPT credential guide](chatgpt-credential.md) is the full lifecycle. Point `gateway.chatgpt` at
the Secret:

```yaml
gateway:
  chatgpt:
    enabled: true
    existingSecret: dekopon-chatgpt-auth   # what `dekopon-gatewayd auth chatgpt export --expose-credential --namespace <ns>` emits
```

Init places `/var/lib/dekopon/chatgpt/chatgpt-auth.json`, `0600`, owned by `65533`, in a `0700`
directory owned by `65533`, **and never touches it again**. The gateway rotates the refresh token on
every refresh and writes the new record back, so after one refresh the Secret holds a retired token
and copying it back in would break a working gateway. `install` overwrites unconditionally, so this
guard is the whole mechanism:

```sh
[ -d /var/lib/dekopon/chatgpt ] || mkdir -p /var/lib/dekopon/chatgpt
chown 0:0 /var/lib/dekopon/chatgpt && chmod 0700 /var/lib/dekopon/chatgpt
if [ ! -e /var/lib/dekopon/chatgpt/chatgpt-auth.json ]; then
  install -m 0600 -o 65533 -g 65533 \
    /dekopon-source/chatgpt-auth.json /var/lib/dekopon/chatgpt/chatgpt-auth.json
fi
chmod 0700 /var/lib/dekopon/chatgpt && chown 65533:65533 /var/lib/dekopon/chatgpt
```

- `-e`, not `-f` or `-s`: anything at the path, even zero bytes or a crash leftover, counts as
  seeded.
- The directory is reclaimed to `root` first because root without `DAC_OVERRIDE` cannot enter the
  previous run's `65533`-owned `0700` directory to run the test.
- The daemon needs a writable `0700` directory, not a writable file: it writes
  `chatgpt-auth.tmp-<pid>` beside the target and renames it over, so a `subPath` mount of the file
  alone breaks rotation. A failed write-back logs `chatgpt_credential_save_failed` and leaves the
  retired token on disk.
- `gateway.chatgpt.subdir` is one path segment joined onto `paths.stateDir`, so the credential
  always lives on the claim; in an `emptyDir` it would be re-seeded on every reschedule. The gateway
  mounts only that subdirectory, by `subPath`.
- The chart sets `DEKOPON_CHATGPT_AUTH_FILE` on the gateway to that path. Without it, a model with no
  `authFile` falls back to `$XDG_CONFIG_HOME`, then `$HOME`, on the read-only root filesystem.
  Naming `authFile` in `gatewayd.yaml` is clearer.
- No `checksum/` annotation covers either credential Secret: the copy in the volume is
  authoritative, so a changed Secret must not restart the pod.
- `replicas: 1` and `strategy: Recreate` are fixed. Holders in one pod serialize refreshes through
  an advisory lock on a sibling `.lock` file, but two pods sharing the claim would race the rotation.
- The `prepare-files` log says whether a start seeded the file (`seeded …`) or left it
  (`… already exists; leaving the live credential alone`).

### The broker's family

`broker.chatgpt.*` seeds the same kind of file for `dekopon-brokerd`, for a
`kind: chatgptSubscription` entry in `broker-credentials.yaml`:

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

It is seeded once in the same way, `0600` owned by `65532` in a `0700` directory owned by `65532`.
Three things differ:

- **It is a separate token family.** Produce it with a second
  `dekopon-gatewayd auth chatgpt login --auth-file <path>` against the same ChatGPT account, then
  `dekopon-gatewayd auth chatgpt export --expose-credential --auth-file <path>`. Never point both
  daemons at one file: each refresh retires its predecessor, so two holders eventually revoke the
  family for both ([second family](chatgpt-credential.md#a-second-family-for-the-broker)). The chart
  refuses equal `broker.chatgpt.subdir` and `gateway.chatgpt.subdir`, and file names that would
  collide on one projected path.
- **The broker mounts only its own subdirectories.** It never mounts the claim root: this
  directory, `broker.oauth2Records` and the managed provider set are separate `subPath` mounts. The
  gateway gets no path to the broker's family; `charts/dekopon/ci/verify-init-permissions.sh`
  asserts that against the rendered manifest.
- **`dekopon-brokerd` checks the file at startup.** It reads the `authFile` as Tier A (regular,
  owner-owned, `mode & 0o077 == 0`, one link, `O_NOFOLLOW`) and requires an owner-only, writable
  parent, or refuses to start, naming the cause.

### Re-seeding and rotation

`gateway.chatgpt.reseed: true` (`broker.chatgpt.reseed: true` for the broker's family) discards the
credential in the volume, the live and almost certainly newer one, and copies the Secret in again.
Use it only after a local re-login and re-export. It is not self-clearing: every start re-seeds
while it is `true`, so set it back to `false` once the pod has rolled. Changing it rolls the pod,
because it changes the init container's arguments.

Nothing detects or repairs a stale exported copy. It exists to seed a new deployment and is not a
backup: restoring it over a live credential breaks a working pod
([drift](chatgpt-credential.md#the-exported-copy-drifts-and-that-is-expected)). To rotate, log in
locally, re-export and update the Secret, then either delete the file in the volume and restart, or
set `reseed` for one roll.

## The managed provider set comes from the claim

A `broker.yaml` names either `providers`, absolute paths to components baked into the image, or
`providerSet`, a generated lock plus a content-addressed blob store. They are mutually exclusive in
the daemon. `broker.providerSet.*` mounts the second:

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

- `subdir` is one path segment on the claim, and `mountPath` is where the broker sees it; they
  differ because the broker never mounts the claim root. The gateway gets no mount.
- The pod never writes the lock or source blobs. `dekopon-brokerd provider sync` must finish before
  the pod rolls, through [the hook Job](#syncing-the-set-with-the-charts-hook-job) or your own step
  against the same claim.
- `prepare-files` hands that subdirectory to `65532` as `0700`, not recursively, so the blobs the
  sync wrote keep their owner. Then `precompile`, with the broker image, UID and `broker.resources`
  and only the provider-set mount, writes compiled components before the broker starts. Both steps
  are required: the broker refuses a lock or store it does not own and any non-sticky group- or
  world-writable ancestor, and a fresh `local-path` volume arrives root-owned and `0777`.
- It is off by default; enabling it means removing `providers` from `broker.yaml`. The chart refuses
  a `subdir` that is not one non-dot path segment or that equals a ChatGPT subdirectory, and a
  `mountPath` that is not canonical absolute or overlaps another of the broker's mounts.

## Syncing the set with the chart's hook Job

`broker.providerSync.enabled` renders a hook Job that runs `dekopon-brokerd provider sync` with the
Deployment's image, through the same `image.digest` or `image.tag` helper, so the sync and the broker
that reads its lock cannot drift apart. It compiles new components, so it takes `broker.resources`.
It needs `broker.providerSet.enabled` and a ConfigMap whose `providers.yaml` key holds the desired
set:

```yaml
broker:
  providerSync:
    enabled: true
    configMap: dekopon-config
```

- The Job mounts `providerSet.subdir` at `providerSet.mountPath`, as the broker does, and writes
  `providers.yaml`, `providers.lock.yaml` and `store/` there. Point `broker.yaml` at
  `<mountPath>/providers.lock.yaml` and `<mountPath>/store`.
- It runs before the first pod exists, so a root init container repeats the Deployment's step,
  reclaiming the claim root as `0:0` `0700` and handing the subdirectory to `65532` as `0700`. A
  `65532` init container then copies the ConfigMap key in as a `0600` regular file. The sync runs as
  `65532` with no ServiceAccount token.
- `backoffLimit: 2`, `activeDeadlineSeconds: 600`, and the Job is removed a day after it finishes. A
  failed hook fails the release and the old pod keeps serving. Every sync reruns it; with the lock
  unchanged and the blobs present it fetches nothing.
- The running broker notices a changed lock within seconds, drains under its shutdown grace and
  exits; only the broker container restarts, on the new lock, and the gateway stays up. An unchanged
  lock restarts nothing.
- The claim and the ConfigMap must exist before the hook runs. The chart-created claim does not on a
  first install, so under the default Helm hook the chart refuses to render without
  `state.existingClaim`.

`broker.providerSync.annotations` replaces the hook annotations entirely; empty means
`helm.sh/hook: pre-install,pre-upgrade` with `before-hook-creation`. **Argo CD** maps those to
`PreSync`, which runs before every sync wave, before a ConfigMap or claim in an earlier wave exists,
so Argo users set:

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
Deployment's image. A problem fails the hook: the release stops before anything changes, and the old
pod keeps serving. Each check is the function its daemon runs at boot
([gateway](gatewayd.md#check-a-configuration), [broker](../crates/dekopon-brokerd/README.md#check-a-configuration)).

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

- The ConfigMaps must hold the **candidate** files when the hook runs. The live ones do not: the
  hook runs before the sync applies them, and once they apply, Reloader rolls the pod whatever the
  hook says. Render a second set from the same files (a kustomize generator per directory,
  `dekopon-check-*`), annotated as hooks of the same phase with a lower sync wave than the Job.
- Set `configCheck.broker.configMap`, `configCheck.gateway.configMap` or both. `gateway.configMap`
  requires `gateway.catalogConfigMap`, and `broker.workflowsConfigMap` requires
  `broker.providerSetConfigMap`.
- `againstConfigMap` names the live gateway ConfigMap: only a model absent from it or changed in it
  is probed, so an unchanged model costs no vendor call. With it set, the check also mounts the
  Deployment's live catalog read-only at `paths.catalogDir`, where the live gateway configuration
  must resolve its catalog.
- **Broker check.** A root init container copies the broker ConfigMaps into a `0700` directory owned
  by `65532`, as `0600` regular files, because `check` refuses ConfigMap symlinks. When the pod gets the chart-rendered `peers.yaml` (`broker.configDirectory.configMap` with
  `broker.configDirectory.peers`), the hook copies the same file from a `<fullname>-check-peers` hook
  ConfigMap and refuses a candidate `peers.yaml` key, as the Deployment does. The check runs as
  `65532` and resolves `providerSetConfigMap`'s `providers.yaml` into `<paths.stateDir>/config-check`
  on the claim, kept between runs; under the default Helm hook that needs `state.existingClaim`. It
  loads providers without the compiled cache, so every run compiles every provider; its run time on
  the Raspberry Pi is unmeasured.
- **Gateway check.** Candidates are mounted read-only exactly as the Deployment mounts them, so a
  mode production would refuse fails the hook too. It runs as `65533` with `configCheck.env`, which
  should name each probed model's `apiKeyEnv` and nothing else. No ChatGPT login is mounted, since
  `check` never probes a subscription model.
- `backoffLimit: 0`, since a refusal is deterministic and a retry pays for every probe twice;
  `activeDeadlineSeconds: 600`.

`configCheck.broker.workflowsConfigMap` names a ConfigMap of essential workflows, every `*.yaml` key,
passed to the broker check as `--workflows`. Each step's argv runs offline through its provider's
`run-command` and the serving broker's attestation and Cedar decisions; the format and the checks
are in the [brokerd README](../crates/dekopon-brokerd/README.md#check-a-configuration). A refused
step is one problem, `would break essential workflows: <workflow>: <step>: <check>: <detail>`, and
fails the hook. Empty, the default, runs no workflow check.

A refusing hook blocks every later sync, a fix's included. To override, unset `workflowsConfigMap`
(or set it to `""`) in the commit that carries the fix: the hook renders from the target revision,
so that sync runs the base check without the workflow phase. Never override with a sync that skips
hooks, such as a selective sync or `--strategy apply`: it skips the base check and the provider-sync
hook too.

`configCheck.annotations` replaces the hook annotations entirely; empty means
`helm.sh/hook: pre-install,pre-upgrade` with `before-hook-creation`, which Argo CD runs as
`PreSync`. **Argo CD's selective sync runs no hooks**, so run both checks by hand on the rendered
candidate first.

## Operator console

`console.enabled` adds a container that runs `dekopon-console --idle` as UID `65535`. It requires
`gateway.enabled`, because it reads the gateway's agent catalog. With empty `console.image.tag` and
`console.image.digest` it uses the main image, including its variant or digest pin; an explicit tag
or digest uses `console.image.repository` (default `ghcr.io/dekopon-agents/dekopon`). It mounts the
runtime directory, the catalog and any skills read-only, plus a small memory `/tmp`; it reaches the
broker only through IPC group `65534` and gets no model, provider or gateway credential.
`console.subject` sets `DEKOPON_CONSOLE_SUBJECT`; when it is empty, pass `--subject`. Exec starts a
fresh process configured by the container environment:

```sh
kubectl exec -it deploy/dekopon -c console -- dekopon-console
```

The broker decides what it may do. `broker.yaml` needs an identity for UID `65535` whose attestor
namespaces cover `console.subject` (`broker.configDirectory.peers` renders it), and Cedar statements
that name that identity's principal as `context.via`. Without them the console lists agents and is
refused everything else.

## Storage, uninstall, and recovery

While `state.keepOnUninstall` is true (the default), the state claim carries
`helm.sh/resource-policy: keep` and `argocd.argoproj.io/sync-options: Prune=false,Delete=false`, so
neither `helm uninstall` nor Argo CD deletes the live model credentials. Removing retained state is
a separate operator action. Argo CD ignores Helm's annotation and, syncing with `prune: true`,
deletes any live object the chart stops rendering, as this claim does when `state.existingClaim` is
set or the chart source fails to resolve. `Prune=false` covers the sync and `Delete=false` covers
deleting the `Application`; Argo reads both off the live object.

On the reference cluster `local-path` is the only StorageClass. It is RWO and
**`ALLOWVOLUMEEXPANSION` is `false`**, so `state.size` is final for the life of the volume, and
replacing the claim means moving the live model credentials to the new volume.

`state.existingClaim` points the pod at a claim you manage. Init keeps its root at `0:0:0700` and
gives each daemon only its own private subdirectories, as in [Paths the chart owns](#paths-the-chart-owns).

## Optional provider-storage claim

`providerStorage.enabled` creates (or, with `providerStorage.existingClaim`, mounts) a claim
physically separate from `state`, mounted only into the broker at `providerStorage.rootPath`
(default `/var/lib/dekopon-provider-storage`). A generated claim always carries
`helm.sh/resource-policy: keep` and `argocd.argoproj.io/sync-options: Prune=false,Delete=false`;
an existing claim remains operator-owned. Rendering fails when the state and provider-storage claim
names resolve to the same claim.

Every chart mount path must be canonical absolute, with safe non-dot segments and no repeated or
trailing slash. The storage root must not equal, contain or sit inside another chart mount, the
projected init sources, or an image path (`/opt/dekopon/providers`,
`/opt/dekopon/optional-providers`, `/usr/local/bin`, `/usr/share/doc/dekopon`). These fail at
`helm template`, before a volume can shadow configuration, packaged providers or init-script text.

No memory provider is bundled ([baked components](container-image.md#baked-provider-components));
supply one through a reviewed provider set. The filesystem must support retained
directory-descriptor-relative no-follow opens, advisory locks, same-directory rename, and file and
directory sync. Directory names under the root are unkeyed SHA-256 derivations of the chat scope,
and the store makes no encryption-at-rest claim. A stuck native syscall may exceed the configured
shutdown grace, and malicious same-UID filesystem mutation is outside the claim.

## seccomp and read-only root

The pod and the init container use `seccompProfile: RuntimeDefault`, which permits what Wasmtime's
Cranelift JIT needs: `mmap` with `PROT_EXEC`, `mprotect`, `memfd_create`, and `SIGSEGV`/`SIGBUS`
handlers for guard-page traps. CI's init proof compiles a provider under Docker's equivalent default
profile. Do not narrow it to a hand-written profile without proving a component loads: the failure
is a trap inside the JIT, not a clean error.

`readOnlyRootFilesystem` is `true` for every container, and neither daemon writes outside its
mounted volumes.

## Two version numbers

The chart and the application it deploys are versioned independently.

| | Where it comes from | What it means |
|---|---|---|
| **chart version** (`Chart.yaml: version`) | a `dekopon-chart-*` Git tag | the version of *this chart* — its templates, defaults, and documentation |
| **appVersion** (`Chart.yaml: appVersion`) | the application release the chart deploys | what `image.tag` defaults to, and what the pod actually runs |

A templating fix ships as a `dekopon-chart-*` tag and changes no `appVersion`; an application release
moves `appVersion` and, with it, the image the chart pulls. `v*.*.*` tags publish crates, release
archives and the container image; `dekopon-chart-*` tags publish only the chart. The
[maintainer release process](../README.md#maintainer-release-process) says how both move.

The chart renders `appVersion` as `app.kubernetes.io/version` on every object, so it must name the
image the pod runs; otherwise every query and dashboard on that label reports a version nothing is
running.

The image workflow publishes under the Git tag, so an empty `image.tag` renders `v` + `appVersion`:

```
ghcr.io/dekopon-agents/dekopon:v0.74.0
```

There is no `latest`. `image.digest`, empty by default, pins the `linux/amd64` + `linux/arm64`
index and wins over a tag; the index digest is what `gh attestation verify` attests.

## Publishing

[`.github/workflows/chart-publish.yml`](../.github/workflows/chart-publish.yml) runs on
`dekopon-chart-*` tags only. Prepare the release as the
[maintainer release process](../README.md#maintainer-release-process) says, writing the dated
`[dekopon-chart-<VERSION>]` section in [`CHANGELOG.md`](../CHANGELOG.md) from
`git log dekopon-chart-<PREV>..HEAD --format=%B | grep '^Changelog: '`. Pull-request CI requires
that section to match `Chart.yaml`. The workflow refuses a tag whose version `Chart.yaml` does not
declare, repeats the changelog check, packages the chart, lints and renders the **tarball** rather
than the working tree, and only then pushes:

```console
helm package charts/dekopon --version "$VERSION"
helm push "dekopon-$VERSION.tgz" oci://ghcr.io/dekopon-agents/charts
```

It never passes `--app-version`: a chart tag must not move which application release the chart
deploys. It logs in with `GITHUB_TOKEN`, so the chart adds no credential, and Helm 3.8+ speaks OCI
natively, so there is no plugin, chart index or repository server. CI's chart job also diffs the
packaged archive's renders against the source tree's. The published coordinates are:

```
oci://ghcr.io/dekopon-agents/charts/dekopon
```

A GHCR package is **private when it is first published**. Make each public once, in its package
settings: `ghcr.io/dekopon-agents/dekopon` (the image) and `ghcr.io/dekopon-agents/charts/dekopon`
(this chart). Downstream assumes anonymous pulls: the chart has no `imagePullSecrets` and Argo CD
needs no registry credential. An anonymous pull of a private package reports *not found*, not
*forbidden*, so it reads like a typo in the chart name or version.

## Consuming it from ArgoCD

This applies to **Argo CD 3.3**. The reference deployment pairs this registry source with a Git
source for its configuration:

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
    targetRevision: 0.61.0                    # the CHART version, not appVersion
    helm:
      valueFiles: []
      values: |
        ...
  destination:
    server: https://kubernetes.default.svc
    namespace: dekopon
```

- **`repoURL` is the bare registry path.** A Helm chart source (`spec.source.chart` set) takes no
  scheme; Argo CD 3.3's documentation says "the repository URL should not contain the OCI scheme
  prefix `oci://`". `oci://` belongs to the plain OCI artifact source (`spec.source.path`, no
  `chart`, registered with `--type oci`), which reads manifests and does not run Helm.
- **Register the repository even though it is public.** Argo CD decides OCI mode from the
  repository's `EnableOCI` field, not the URL, and treats an unregistered URL as a classic HTTP Helm
  repository. Being public removes only the need for a username and password.

An `Application` therefore needs this repository Secret beside it, with no credential fields:

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

`syncPolicy.automated.prune: true` cannot take the live model credentials: see
[Storage, uninstall, and recovery](#storage-uninstall-and-recovery).

## Configuration values

`serviceAccount.create` (default `false`) creates an account named by `serviceAccount.name` or the
release fullname, with `serviceAccount.annotations` (default `{}`); otherwise the pod uses
`serviceAccount.name`, or `default` when empty. The pod and a chart-created account disable token
automounting; a broker-only projection through [`broker.secretSourceVolumes`](#broker-secret-sources)
is the opt-in. The chart grants no RBAC permissions.

Each operator-supplied file is either inline, in which case the chart writes a Secret, or a
reference to an object that already exists. Setting both is an error.

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

Use `existingSecret` for credentials and the private map: an inline value is stored in the release,
returned by `helm get values`, and usually committed.

The default `broker.config.inline` is the broker README's cli-probe example on the chart's paths: a
deny-by-default configuration that starts, loads the baked `cli-probe-provider.wasm`, and maps the
probe UID without granting an executable capability. Replace it. It sets
`serverLimits.maxFrameBytes` to 2 MiB explicitly; raise or omit that field when raising payload
bounds ([broker configuration](../crates/dekopon-brokerd/README.md#configuration)). `gateway.enabled`
is `false` by default because the chart cannot supply a gateway's transport, model endpoint, agent
catalog or credentials.

The chart refuses to render, with a message, when:

- a [UID](#uids) rule it can check is broken;
- a required file has no source, or one file has both sources;
- an inline `broker.yaml` names `policiesPath`, `credentialsPath`, `secretMapPath` or
  `capabilities` with no corresponding value supplied;
- two chart paths (`paths.*`, `brokerAssets.rootPath`) overlap, or one is not canonical absolute;
- a daemon uses `configDirectory` without its `drainBudget` assumption, or
  `terminationGracePeriodSeconds` is shorter than the two drains in sequence plus the buffer.

Each feature's section names its own refusals. Most of these mistakes would otherwise show only as a
pod that starts and never becomes ready.

### Configuration directories

`broker.configDirectory.configMap` replaces `broker.yaml` and `policies.cedar` with a ConfigMap of
fragments. Init copies every `*.yaml` and `*.cedar` key into `<paths.configDir>/broker.d` as
`65532`-owned `0600` files, asserts each one, hands the directory to `65532` as `0700`, and the
broker starts with `--config <paths.configDir>/broker.d`. Clear `broker.config.inline` (it has a
default) and leave `broker.config.existingSecret` and `broker.policies.*` empty, or the chart refuses
to render. Credentials, the secret map and bootstrap files still land beside `broker.d`, the ChatGPT
seed still lands on the claim, and a fragment names each by path
(`credentialsPath: /etc/dekopon/broker-credentials.yaml`).

`broker.configDirectory.peers` (on by default) adds a chart-rendered `peers.yaml` with the pod's own
identities: `65532` as `dekopon-probe`; `65533` as `dekopon-gatewayd` with `attestor: {}` when the
gateway is enabled; and `65535` as `dekopon-console` with `attestor.namespaces: [<console.subject>]`
when the console is enabled with a subject. While it is on, a ConfigMap key named `peers.yaml` fails
the init container and the config-check hook.

`gateway.configDirectory.configMap` mounts the ConfigMap directly at `paths.gatewayConfigDir`;
`--config` points to that directory, and `gateway.config` must be empty. No gateway edit rolls the
pod: the gateway swaps in the live set named in [The broker init container](#the-broker-init-container)
and refuses every other edit until the operator rolls the pod.

The chart cannot hash an external broker ConfigMap, so keep broker fragments, including `host.yaml`,
on the operator's restart path. For Reloader, annotate that broker ConfigMap
`reloader.stakater.com/match: "true"` and set `reloader.stakater.com/search: "true"` in
`deploymentAnnotations`; do not put gateway-only ConfigMaps on the shared pod's Reloader list.

### Broker secret sources

`broker.secretBootstrapFiles` copies operator-managed Secret keys through the root init container
into `<paths.configDir>/<file>` as `65532`-owned `0600` files. This is how token and session files
for the 1Password, Vault, AWS, GCP, Azure and Kubernetes API adapters reach the broker. Each entry
needs `file`, `existingSecret` and `existingSecretKey`; `file` is one non-dot path segment that
collides with no other managed file.

`broker.secretSourceVolumes` mounts explicitly named Kubernetes volumes read-only into the broker and
nowhere else. A Secret or ConfigMap volume keeps its AtomicWriter symlink layout and is read through
[`source.kind: kubernetesProjection`](secrets.md#kubernetesprojection), whose `root` must equal the
entry's `mountPath`. Names, canonical absolute mount paths, volume maps, and overlap with chart and
image paths are checked at template time. A bootstrap token that needs the ordinary `0600` loader
uses `broker.secretBootstrapFiles` instead; no custom init container is needed. A Secret source
mount:

```yaml
broker:
  secretSourceVolumes:
    - name: api-secret
      mountPath: /var/run/dekopon-secrets/api
      volume:
        secret:
          secretName: api-secret
```

A projected ServiceAccount token uses the same value with no init copy, so the broker, which reads it
per invocation, sees kubelet's rotation. The token volume is mounted only into the broker, never the
gateway or the copy init container:

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

### Annotations

Two annotation maps ship, and they are not interchangeable:

| Value | Rendered on | Read by |
|---|---|---|
| `deploymentAnnotations` | the Deployment object's own `metadata.annotations` | a controller that watches the workload object, such as a Secret-watching restarter — Stakater Reloader's `reloader.stakater.com/*` keys belong here |
| `podAnnotations` | the pod template, so every pod the Deployment creates carries them | anything that reads a running pod: scrapers, meshes, admission webhooks |

A restarter configured through the wrong map silently does nothing. Both default to empty.

### Gateway Service and VM proxy

`gateway.service.enabled` creates a ClusterIP Service and a matching gateway container port named
`webhook` (`gateway.service.port`, default 9080) for an inbound webhook transport such as WhatsApp
Cloud API. A Service cannot reach loopback, so the transport must bind
`0.0.0.0:<gateway.service.port>`; the gateway's TCP readiness probe keeps the endpoint unavailable
while the configuration and the chart port disagree. The chart creates no Ingress: the operator
routes only the configured callback path and terminates public TLS outside the pod.

`gateway.proxy.enabled`, which requires `gateway.enabled`, adds a private ClusterIP port `vm-proxy`
(`gateway.proxy.port`, default 9090) and mounts `gateway.proxy.tlsSecret` read-only at
`/etc/dekopon-proxy-tls` in the gateway and the gateway config check. The Secret must contain
`tls.crt`, `tls.key` and `ca.crt`; the daemon's `proxy.tls` must name those paths, and `proxy.bind`
must use the same port on a non-loopback address. The chart refuses enablement without the Secret,
and for inline configuration without a `proxy:` block or with a different bind port. For opaque
configuration, only the config check's candidate validation loads the TLS files and validates
`proxy`, so disabling the check skips it. The broker never mounts this Secret.

## What is not proven

CI's chart job lints and renders the chart, validates the main renders with `kubeconform -strict`
against Kubernetes 1.33.0, asserts the render refusals it exercises, and proves the packaged archive
renders like the source. On `linux/amd64` it runs
[`verify-init-permissions.sh`](../charts/dekopon/ci/verify-init-permissions.sh), which executes the
rendered init commands verbatim, under their rendered `securityContext`, against a projected-volume
fixture. That script asserts the file tiers and mounts as each UID, runs distinct-UID processes
against the IPC layout through a Python socket fixture, proves seed-once and re-seed for both
ChatGPT families across a simulated rotation and restarts, and runs the run's own
`dekopon-brokerd check` on the image's runtime base.

CI does not prove:

- an install into a cluster, a pod boot, or a published image;
- a live token refresh: seed-once is exercised at the file level as each family's UID, not against
  OpenAI;
- a provider-sync or config-check hook completing in a cluster, or `dekopon-gatewayd check --probe`
  calling a vendor;
- `linux/arm64`, the reference platform, or Kubernetes versions other than 1.33;
- cross-UID isolation between real daemons, which `ipc_process.rs` proves only with
  `DEKOPON_REQUIRE_CROSS_UID=1` in a root container
  ([IPC directory](../crates/dekopon-brokerd/README.md#ipc-directory-and-distinct-peer-uids)).
