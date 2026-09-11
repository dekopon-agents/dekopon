# Running Dekopon

**Status: current.** This is the operator's index, not a second copy of the manuals. Dekopon keeps
each implementation contract beside its code, so the authoritative text for running the privileged
broker is [`crates/dekopon-brokerd/README.md`](../crates/dekopon-brokerd/README.md) and for the
gateway it is [`dekopond.md`](dekopond.md). This page exists so an operator can find them by the
question they arrived with, rather than by guessing that a crate README is the operations manual.

## Audit

`dekopon-brokerd` records every decision as a metadata-only `broker.decision` or `broker.execution`
log record inside the caller's trace: a JSON line on stdout always, and an OTLP log record when
`broker.yaml` has a `telemetry` block. Without `telemetry`, audit lasts as long as whatever keeps
the broker's stdout — `kubectl logs` for a pod — so a deployment that must keep audit past a pod
restart configures `telemetry`. Losing the exporter loses audit.

Audit records do not establish whether retrying an external effect is safe: the broker suppresses
no duplicate, so a resubmitted invocation identifier runs again. Tamper-detection, rollback
protection, crash recovery, and duplicate-effect defence are [non-goals](design.md#non-goals).

Record fields and correlation:
[`observability.md` § The broker audit record](observability.md#the-broker-audit-record). Where the
daemon sends them: [`crates/dekopon-brokerd/README.md`](../crates/dekopon-brokerd/README.md#audit).

## By the question you arrived with

### Starting, stopping, and upgrading

| Question | Read |
|---|---|
| What files and directories must exist, and with what ownership and modes? | [`dekopon-brokerd` § Configuration](../crates/dekopon-brokerd/README.md#configuration) |
| How do I resolve, materialize, list, or verify a managed provider set? | [`dekopon-brokerd` § Managed provider sets](../crates/dekopon-brokerd/README.md#managed-provider-sets) — normal startup, `list`, and `verify` are offline; successful lock changes apply after restart |
| Why did a managed provider refuse to load? | The same section distinguishes desired references, the generated manifest/component lock, installed blob hygiene, and complete host validation. A digest proves bytes, not publisher provenance. |
| Why did the broker refuse to start? | [`dekopon-brokerd` § Configuration](../crates/dekopon-brokerd/README.md#configuration) for path and permission refusals; [`dekopon-brokerd` contract § Startup validation](../crates/dekopon-brokerd/README.md#catalog-ownership-at-policy-startup) for policy refusals |
| Why did the gateway refuse to start? | [`dekopond.md` § Startup fails closed](dekopond.md#startup-fails-closed) |
| What does shutdown actually do, and how long may it take? | [`dekopon-brokerd` § Configuration](../crates/dekopon-brokerd/README.md#configuration) — signals, draining, and the grace that must cover one host deadline plus two frame deadlines |
| Why does startup take so long, and can a restart skip recompiling every component? | [`dekopon-brokerd` § Compilation cache and the concurrent memory budget](../crates/dekopon-brokerd/README.md#compilation-cache-and-the-concurrent-memory-budget) — `compileCachePath` is optional; absent, Cranelift recompiles every component before the socket binds, which is what the chart's startup probe budget ([`charts/dekopon/README.md` § Probes](../charts/dekopon/README.md#probes)) is sized to cover |
| In what order do I restart the two daemons? | [`upgrading.md`](upgrading.md#restart-the-broker-first-and-stop-it-last) |
| Configuration changed between versions — what do I edit? | [`upgrading.md`](upgrading.md) |
| Can I run a newer broker against an older gateway? | No. [`dekopon-brokerd` contract § Version and compatibility](../crates/dekopon-broker-protocol/README.md#version-and-compatibility) |

### Authority, policy, and credentials

The current `credential`/`credentialByAgent` bindings discussed below will be replaced by public
DRNs. This is committed direction, not an upgrade required today
([migration requirements](design.md#legacy-credential-bindings)).

| Question | Read |
|---|---|
| Who may drive which agent, and where is that written? | [`dekopon-brokerd` § Policy](../crates/dekopon-brokerd/README.md#policy) |
| How narrowly does an authorized invocation actually run? | [`dekopon-brokerd` contract § Broker HTTP enforcement](../crates/dekopon-http-host/README.md#request-and-credential-boundary) |
| Where do provider credentials live, and how are they bound to a destination? | [`dekopon-brokerd` contract § Broker HTTP enforcement](../crates/dekopon-http-host/README.md#request-and-credential-boundary) and [`dekopon-brokerd` § One capability, one token per agent](../crates/dekopon-brokerd/README.md#one-capability-one-token-per-agent) |
| How may an agent name a secret without seeing it, and which stores can back it? | [`secrets.md`](secrets.md) — DRNs, dual policy, private bindings, source adapters, path scope, bootstrap, rotation and reflection limits |
| Why did a DRN return `secret-denied`? | The same document: unknown, unbound, wrong-sink/username and policy-denied names intentionally share one result; inspect broker-side policy/map validation rather than probing names. |
| A grant looks right and every session is denied. | Check the agent name. [`dekopon-brokerd` contract § Startup validation](../crates/dekopon-brokerd/README.md#catalog-ownership-at-policy-startup) — agent literals are the one class that is not proved at startup |
| What does an agent's catalog entry actually decide? | [`catalog.md`](catalog.md) |
| How do I get a ChatGPT credential onto a host or into a pod? | [`chatgpt-credential.md`](chatgpt-credential.md), and [`1password-eso.md`](1password-eso.md) for the secret store |

### Seeing what is happening

| Question | Read |
|---|---|
| What do the traces, spans, and audit-safe logs contain? | [`observability.md`](observability.md) |
| A client got a failure code — is it safe to resubmit? | [`dekopon-brokerd` contract § Failure codes](../crates/dekopon-broker-protocol/README.md#failure-codes) |
| An invocation may have taken effect and was not recorded. | `outcome-unaudited`, in the same table. The terminal audit record may be missing; the effect may have happened. Do not resubmit under any identifier |

### Deploying

| Question | Read |
|---|---|
| Kubernetes | [`charts/dekopon/README.md`](../charts/dekopon/README.md) |
| The container image — what is in it and what does it assume? | [`container-image.md`](container-image.md) |
| Getting an ordinary daemon file or a projection-backed DRN source into a pod | [`1password-eso.md`](1password-eso.md) and [`secrets.md` § `kubernetesProjection`](secrets.md#kubernetesprojection) |
| Optional provider storage and durable chat memory | [`dekopon-brokerd` § Optional provider storage and chat memory](../crates/dekopon-brokerd/README.md#optional-provider-storage-and-chat-memory) |

## The boundaries an operator must not paper over

These are invariants, not defaults, and no operational convenience overrides them. The complete list
is [`dekopon-brokerd` § Boundaries](../crates/dekopon-brokerd/README.md#boundaries); the two that
most often come up while operating are:

- **IPC group membership is not identity.** The [current local process boundary](security-model.md#current-local-process-boundary)
  separates gateway and broker UIDs; the broker maps the real peer UID, not its group.
  Each mapped UID remains its own trust domain, not independent process attestation.
- **Audit is a log record, not tamper-proof or crash-durable storage.** A restart is not
  permission to retry an effect. See [Audit](#audit).

[`security-model.md`](security-model.md) is the full statement of what is trusted, what is not, and
what is presently out of scope.
