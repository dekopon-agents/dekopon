# dekopon-brokerd

`dekopon-brokerd` is the separately deployed privileged Unix service for Dekopon provider components. It derives caller identity from Unix peer credentials, evaluates a deny-by-default Cedar policy set against owner-authored execution constraints, restores replay identifiers from a verified owner-only audit chain, maintains a separate atomic audit checkpoint, executes only statically linked Dekopon host interfaces, and can explicitly bind the unauthenticated `GET`/`HEAD`-only `dekopon-webui` operational view.

Authorization and execution constraints are two separate files on purpose. `policiesPath` decides *who may do what*; `constraintSets` decides *how narrowly the broker then does it*. A policy edit can never widen a timeout, reach a new host, or bind a credential that was not already bound.

The owner-only socket intentionally supports one Unix UID trust domain. Every process running under that UID can act as its configured principal/actor; use a dedicated service/client UID when process-level separation matters. Request payloads cannot provide or override identity, policy, constraints, credentials, or authorization.

## Configuration

The configuration must be a regular single-link file owned by the server UID and must not be group/world writable. Socket, audit, checkpoint, and checkpoint-lock parent directories must be owner-only. Provider components must be regular single-link files owned by the server UID and must not be group/world writable; their canonical parent directories must also be server-owned and not group/world writable. Writable non-sticky path ancestors are rejected.

```yaml
# broker.yaml
apiVersion: dekopon.dev/brokerd/v1alpha1
socketPath: /home/dekopon/.local/run/dekopon/broker.sock
auditPath: /home/dekopon/.local/state/dekopon/audit.jsonl
checkpointPath: /home/dekopon/.local/state/dekopon/audit-checkpoint.json
checkpointLockPath: /home/dekopon/.local/state/dekopon/audit-checkpoint.lock
brokerPrincipal: local-broker
policyRevision: policy-2026-01
policiesPath: /home/dekopon/.config/dekopon/policies.cedar
providers:
  - /home/dekopon/lib/dekopon/echo-provider.wasm
  - /opt/dekopon/providers          # a directory loads every *.wasm directly inside it
identities:
  - uid: 1000
    principal: local-user
    actor:
      type: human
      principal: local-user
constraintSets:
  echo.echo:
    provider: echo
    effect: read-only
    risk: Low
    idempotency: idempotent
    constraints:
      timeoutMs: 30000
      maxOutputBytes: 1048576
```

```cedar
// policies.cedar — chmod 0600, owner-owned, single-link, 1 MiB maximum
@id("local-user-echo")
permit(principal == Dekopon::Principal::"local-user",
       action == Dekopon::Action::"echo.echo",
       resource == Dekopon::Provider::"echo")
unless { context has via };
```

`policiesPath` is read under the configuration's own rules: the path is canonicalized, then opened
without following symlinks and required to be a regular server-owned single-link file that is not
group/world writable, at most 1 MiB, and valid UTF-8. It is
required once any `constraintSets` entry exists — a broker that declares executable capabilities
and no policy would refuse every request while looking configured. An absent path means an empty
policy set, which permits nothing.

## Policy

Policies are [Cedar](https://cedarpolicy.com), validated at startup against a schema generated from
this configuration. Everything a policy may name has to exist: principals come from `identities`
and `identityMappings`, providers and capability actions come from the loaded provider manifests,
and `agent.prompt` is fixed. A policy naming anything else refuses startup rather than becoming
policy that can never match.

| Cedar name | Comes from |
| --- | --- |
| `Dekopon::Principal::"…"` | an `identities` entry or an `identityMappings` principal |
| `Dekopon::Provider::"…"` | a loaded provider manifest |
| `Dekopon::Action::"…"` | a loaded capability, or the fixed `agent.prompt` |
| `Dekopon::Agent::"…"` | any agent name; the catalog belongs to the gateway, not the broker |
| `Dekopon::Secret::"drn:…"` | a public DRN declared by the owner-only secret map |
| `Dekopon::Action::"secret.use"` | fixed separate permission to consume one exact DRN |

Capability actions carry a context of `{ via?, subject?, agent?, effect, risk, idempotency }`;
`agent.prompt` carries `{ via?, subject?, agent? }`. Every value is derived by the broker from
authenticated transport state or this configuration — never from a request payload, and never from
message content or provider input.

An optional `@id("…")` annotation names a policy. That name is what audit records carry as
`policy_ids`, so it is worth writing; without it Cedar names policies positionally (`policy0`,
`policy1`, …) and inserting a policy renumbers the ones below it. Names must be unique.

Each `providers` entry is a component file or a directory of them. A directory loads every
`*.wasm` directly inside it — not recursively — in **filename order**, which matters because the
registry builds its capability route table in load order: readdir order would let two runs over one
directory disagree about which provider claimed a duplicate capability. A directory must be owned
by this UID and not group- or world-writable, because anyone who can write it can add a provider
the broker will compile and run. Every file the scan yields is then checked on its own exactly as a
directly-named one is. An empty directory is an error rather than a silent zero providers; it
almost always means a mount that did not happen or a build that did not run.

There is no implicit provider search path. The broker loads code, so every directory it loads from
is named in this owner-only file and nowhere else — pointing at a shipped directory is one line,
and that line is the record of the decision.

## Managed provider sets

**Status: current, exact-reference foundation.** `dekopon-brokerd` also contains its provider
manager, so resolving an OCI reference, fetching its component, validating it, and serving the
locked bytes need no `wkg`, ORAS, Docker CLI, shell, or package manager beside the broker binary.
Provider management is a separate operator mode. It exits after changing local state; normal daemon
startup remains network-free, deterministic, and startup-fixed.

The operator authors exact references:

```yaml
# providers.yaml
apiVersion: dekopon.dev/provider-set/v1alpha1
providers:
  - source: ghcr.io/dekopon-agents/provider-gh:0.1.0
  - source: ghcr.io/dekopon-agents/provider-curl@sha256:0123...cdef
```

A source must carry a fully qualified registry and either an explicit tag or a canonical lowercase
SHA-256 **manifest** digest. A tag that looks like `1.2.3` is still one exact OCI tag; it is never
silently interpreted as a SemVer range. An unchanged tag keeps the manifest digest already in the
lock. To request another resolution, change the authored reference. Explicit SemVer requirements,
networked outdated checks, and `update` are not in this first format.

Resolve and materialize the set:

```console
dekopon-brokerd provider sync \
  --provider-set /etc/dekopon/providers.yaml \
  --lock-file /etc/dekopon/providers.lock.yaml \
  --store /var/lib/dekopon/provider-store

# Recreate missing local bytes from the existing immutable lock, without resolving a tag:
dekopon-brokerd provider sync --locked \
  --provider-set /etc/dekopon/providers.yaml \
  --lock-file /etc/dekopon/providers.lock.yaml \
  --store /var/lib/dekopon/provider-store

# Both are offline; list reports byte state/reason, verify also runs complete host validation:
dekopon-brokerd provider list \
  --lock-file /etc/dekopon/providers.lock.yaml \
  --store /var/lib/dekopon/provider-store
dekopon-brokerd provider verify \
  --lock-file /etc/dekopon/providers.lock.yaml \
  --store /var/lib/dekopon/provider-store
```

`--output json` gives deterministic machine-readable command results. Successful lock changes say
that they apply on the next broker restart; there is no hot reload.

Operator commands never take `--config` or `--http-bind`; combining them is a usage error.
`provider` requires `--lock-file` and `--store`, `sync` also `--provider-set`, and `audit verify`
requires `--audit-path`. Usage errors exit 2; a failed command exits 1. Operator modes print
results on stdout and text diagnostics on stderr at `warn` (override with `RUST_LOG`); the daemon
logs JSON on stdout at `info`.

Resolution accepts one OCI image manifest with schema 2, exact artifact type
`application/vnd.dekopon.provider.v1+wasm`, the standard empty OCI config, and exactly one positive,
bounded `application/wasm` layer. Manifest, token, error, and component streams have independent
byte ceilings and deadlines. Public registries use anonymous OCI Bearer challenge flow. Private
registry credentials and custom certificate roots are deliberately not accepted yet; TLS
verification cannot be disabled, ambient proxy environment variables are ignored, redirects may
never downgrade to unapproved plaintext, and plain HTTP is available only to an exact literal
loopback authority named with `--plaintext-loopback-registry <HOST[:PORT]>` (repeatable, `provider`
subcommands only), for development and tests.

Fetched bytes land at:

```text
<store>/blobs/sha256/<component-digest>.wasm
```

The manager serializes competing store and activation writers with owner-only advisory locks, writes a temporary blob
on the destination filesystem, bounds and hashes the stream, synchronizes it, publishes without
clobbering an existing content address, and synchronizes the parent. It validates the **complete**
proposed set with the broker host before atomically replacing the generated lock. A failed
multi-provider validation can leave an unreachable blob, but never a partially activated lock.
The blob directory has a hard lifetime ceiling of 4 GiB and 1,024 files (stale temporaries count),
checked under the store lock before another download, so repeated failed or changed resolutions
cannot grow it without bound. There is no `prune` command yet; reaching that ceiling requires
operator-reviewed cleanup until orphan deletion has its own safe lifecycle contract.

The generated lock is strict, byte-capped, source-sorted, timestamp-free, and records both identities:

```yaml
apiVersion: dekopon.dev/provider-lock/v1alpha1
providers:
  - source: ghcr.io/dekopon-agents/provider-gh:0.1.0
    resolvedVersion: 0.1.0
    manifestDigest: sha256:...
    componentDigest: sha256:...
    componentBytes: 585394
    providerId: gh
```

Activate it in daemon configuration instead of `providers`:

```yaml
providerSet:
  lockPath: /etc/dekopon/providers.lock.yaml
  storePath: /var/lib/dekopon/provider-store
```

`providerSet` and legacy `providers` are mutually exclusive. The lock, store, blob directories, and
blob files are trusted broker input: they must be owned by the broker UID, have protected parents,
be regular/single-link where applicable, and not be group/world writable. The daemon derives every
blob path from the locked component digest and performs no registry request. Most importantly, the
broker host compares the locked component length and SHA-256 against the **same single read buffer**
it passes to Wasmtime, then compares the bounded `describe` provider ID with the lock. A preflight
hash of a different read would not provide that guarantee.

The implementation uses the lower-level OCI reference and bounded HTTP machinery that a package
tool such as `wkg` is built from, not `wasm-pkg-client` itself. That client models standard
`namespace:package@version` WIT packages and the standard OCI-Wasm config/layer layout; Dekopon's
custom provider artifact type and `application/wasm` layer are intentionally different. Embedding
that higher-level client would accept the wrong package contract while adding another binary solved
nothing.

A digest proves byte identity, not publisher identity. This manager does **not** yet verify GitHub
release provenance or OCI attestations. It therefore does not replace the provenance checks in
`ci/stage-image-context.sh`, and the Dockerfile remains network-free. Container staging still
fetches each component at a pinned release, checks its SHA-256, and verifies its provenance with
`gh attestation verify`; it does not use `provider sync --locked`, and a switch would have to keep
verifying provenance for each downloaded component, because this manager checks byte identity only.

At decision time a capability with no constraint set is denied `unconstrained-capability` before
Cedar is consulted at all. That refusal is unconditional and is what actually enforces anything.

`strict` (default `false`) decides whether startup *also* complains. Left alone, a policy naming a
capability no loaded provider offers, and a constraint set naming one, are both warnings: the
deployment starts, and each is logged as an `audit.event` so the mismatch is visible in traces.
This is what lets you ship policy for a provider you have not dropped in yet. Set `strict: true`
for a deployment whose provider set is fixed, where a mismatch means someone made a mistake — then
every one of those warnings is the startup refusal it used to be.

One thing stays fatal in both modes: a policy naming a principal that no `identities` or
`identityMappings` entry declares. Principals come from this file rather than from a loaded
component, so an undeclared one is always a typo.

Bounds are startup-fixed: 1 MiB of source, 1024 policies, no templates, Cedar strict validation.
Evaluation errors deny.

An optional `credentialsPath` names a second, stricter owner-only file (`0600`, single-link,
byte-capped) holding legacy implicitly selected provider credentials. The secret values live only there — never in this
configuration — and a constraint set binds one by symbolic name with `credential:`. Startup fails
closed if a named credential is missing, the constraint set grants no HTTP authority, or any
`allowedHosts` entry is absent from the credential's `destinations`; at execution the native
engine injects `authorization: <scheme> <secret>` only after guest headers were validated and
only for destinations inside the binding. Evidence and audit record `credentialInjected: true`,
never the value. The terminal audit record also names which credential the invocation selected —
the symbolic name from this file, never the secret.

```yaml
# broker.yaml
credentialsPath: /home/dekopon/.config/dekopon/broker-credentials.yaml
constraintSets:
  gh.pull-request.approve:
    provider: gh
    effect: external-write
    risk: High
    idempotency: conditional
    credential: github-pat
    constraints:
      timeoutMs: 15000
      maxOutputBytes: 8192
      http:
        allowedHosts: [api.github.com]
        allowedMethods: [GET, POST]
        maxRequests: 2
        maxRequestBytes: 16384
        maxResponseBytes: 262144
        allowPlaintextLoopback: false
```

```yaml
# broker-credentials.yaml — chmod 0600
apiVersion: dekopon.dev/broker-credentials/v1alpha1
credentials:
  - name: github-pat
    kind: bearerToken
    scheme: Bearer
    destinations: [api.github.com]
    secret: github_pat_...
```

### Public DRNs and private sources

`secretMapPath` names a separate owner-only `dekopon.dev/secret-map/v1alpha1` document. A model may
propose one public logical DRN through the sandboxed curl Basic/Bearer forms, but possession grants
nothing: the broker requires ordinary capability policy, a separate `secret.use` Cedar statement,
and an exact private binding before one source snapshot is fetched. Providers receive neither DRN
nor bytes. Current adapters cover secure files, Kubernetes projections/API objects, 1Password
Connect, Vault KV v1/v2, AWS Secrets Manager/SSM, GCP Secret Manager, and Azure Key Vault.

```yaml
secretMapPath: /etc/dekopon/secret-map.yaml
```

Map descriptors are validated without network at startup. Resolution is per authorized invocation,
with no stale fallback. Basic/Bearer rendering, path/query scope, injection limits and direct
reflection checks live in the native HTTP host. See [`../../docs/secrets.md`](../../docs/secrets.md)
for the strict map schema, source fields, bootstrap-file hygiene, policies, examples and current
non-goals. `credentialsPath` and `secretMapPath` may coexist.

### One capability, one token per agent

`credential:` is the default for every caller. `credentialByAgent:` overrides it per acting agent,
which is what lets one capability reach two organizations without being duplicated under a second
capability namespace:

```yaml
# broker.yaml
constraintSets:
  gh.issue.comment:
    provider: gh
    effect: external-write
    risk: Medium
    idempotency: non-idempotent
    credential: github-pat                     # every agent that has no entry below
    credentialByAgent:
      nestedset-github: github-pat-scientist-hq
    constraints:
      timeoutMs: 15000
      maxOutputBytes: 8192
      http:
        allowedHosts: [api.github.com]
        allowedMethods: [GET, POST]
        maxRequests: 2
        maxRequestBytes: 16384
        maxResponseBytes: 262144
        allowPlaintextLoopback: false
```

The key is the agent, because a route already binds a transport and a match to an agent: one Slack
workspace or one channel selects the agent that answers, and the agent selects the token. The name
comes from the attested context the broker derived from this file's own `attestor` grant and
`identityMappings`, so it is trusted configuration selecting on trusted identity — a request
payload cannot ask for a different token. A caller with no agent, such as a direct `dekopon-run`
peer, matches no override and takes the default.

`credential:` may be omitted while `credentialByAgent:` is present, and then an agent with no entry
transacts unauthenticated exactly as a set with no credential at all always has.

Every credential the set can select is validated at startup, not just the default: an override
naming a credential the store does not hold, or one whose `destinations` do not cover every
`allowedHosts` entry of *this* set, refuses startup with the same errors the default does. An
override naming an agent no policy can reach is not an error — the broker holds no agent catalog,
and the name is inert until a route and a policy exist for it.

A peer identity may carry an optional `attestor` grant, which lets it propose on behalf of an
authenticated external chat identity. `identityMappings` is the other half: it is the only place a
canonical subject becomes a principal.

```yaml
# broker.yaml
identities:
  - uid: 1000
    principal: dekopond-gateway
    actor:
      type: service
      principal: dekopond-gateway
    attestor:
      namespaces: [slack.t0123abc]     # segment-boundary prefixes, service name first
      chatScopes:                      # required by chat-scoped gateway operations
        - breadth: transportWide
          kind: slack
          transport: scientist-slack
identityMappings:
  - subject: slack.t0123abc.u9xyz      # canonical: lowercase dotted segments
    principal: maintainer              # the only place a subject becomes a principal
constraintSets:
  # One entry per capability the policy below may reach; the reads are elided here.
  gh.pull-request.comment:
    provider: gh
    effect: external-write
    risk: Medium
    idempotency: conditional
    credential: github-pat
    constraints:
      timeoutMs: 15000
      maxOutputBytes: 8192
      http:
        allowedHosts: [api.github.com]
        allowedMethods: [GET, POST]
        maxRequests: 2
        maxRequestBytes: 16384
        maxResponseBytes: 262144
        allowPlaintextLoopback: false
```

```cedar
// policies.cedar — the canonical attested workflow.
//
// Two statements, because they answer two questions. The first is the session gate: may this
// person drive this agent at all, and through which gateway. The second is what that session may
// then reach. Neither implies the other.

@id("boss-may-prompt-conditional-writer")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"xaviers-conditional-writer")
when { context has via && context.via == "dekopond-gateway" };

@id("conditional-writer-surface")
permit(principal == Dekopon::Principal::"cpetersen",
       action in [Dekopon::Action::"http-probe.fetch",
                  Dekopon::Action::"http-probe.conditional-write"],
       resource == Dekopon::Provider::"http-probe")
when { context has agent && context.agent == "xaviers-conditional-writer"
    && context has via && context.via == "dekopond-gateway" };
```

A grant is not a capability. It only lets the broker derive an attested context; what that context
may then do is a policy statement, and `context.via` is how a policy keeps attested and direct
authority disjoint. A policy that requires `context has via && context.via == "dekopond-gateway"`
cannot authorize a directly connected peer, and one that requires `unless { context has via }`
cannot authorize an attested proposal. Adding a gateway therefore cannot widen a grant that already
existed.

The gateway names a subject and never a principal; `identityMappings` is the only thing that
resolves one, and an unmapped subject resolves to nothing. Refusals are audited denials recorded
against the gateway's own principal, with reason `attestation-denied` (no grant, or a subject
outside its namespaces), `unmapped-subject` (granted, but no mapping names that subject), or
`agent-denied` (attested and mapped, but no policy lets that principal drive that agent). Startup
rejects duplicate mapping subjects and malformed namespaces.

The example uses the server's own UID because that is what the owner-only socket currently permits:
every configured peer UID must equal the server's. In that single-UID deployment a grant buys
attribution and deny-by-default scoping, not separation — any process under that UID can already
act as the configured peer. Running the gateway under its own UID, where `via` and namespace
scoping become real isolation, is committed direction rather than current behavior.

An optional `telemetry` section enables OTLP export of broker spans:

```yaml
telemetry:
  endpoint: http://rpi.localdomain
  transport: grpc            # grpc | http
  serviceName: dekopon-brokerd
  exportTimeoutMs: 5000
  telemetryPayloads: false
```

`telemetryPayloads: true` adds provider input and HTTP URLs to spans, declaring the telemetry sink in
scope for the data this broker handles. It never exposes a credential: `Redacted` values render
their marker in either mode, and durable audit records are unaffected either way.

It has no credential field by design. Ingest authentication is read from the standard
`OTEL_EXPORTER_OTLP_HEADERS` environment variable by the OpenTelemetry SDK, so a token never enters
this configuration file, the process command line, or a span attribute. Receiver routing travels
the same way: over gRPC OpenObserve reads the organization from an `organization` header and
rejects exports without it, so include `organization=<org>` alongside the token and
`stream-name`. Export failures disable
telemetry and log the reason rather than preventing startup. Broker logs are structured JSON on
stdout, filtered by `RUST_LOG`.

Host, broker, and server limits have conservative defaults (including a 2 MiB frame ceiling) when their entire sections are omitted. `hostLimits` and `brokerLimits` also default field by field, so a partial section keeps the absent-section value for everything it does not name — which is what lets a deployment set `maxTotalMemoryBytes` or `maxReplayIds` alone. `serverLimits` stays all-or-nothing: when it is present every field is required. Unknown fields and unknown API versions are rejected. Startup also requires aggregate provider metadata, every mapped peer's capability response, and the *widest* response any session could receive to fit the frame ceiling. That last bound is the one that matters in a gateway deployment: the connecting peer is typically granted nothing itself, while the principals its `identityMappings` name hold the capability sets that actually reach the wire through an attested `capabilities`. The agent catalog belongs to the gateway, so those contexts cannot be enumerated here and are bounded instead. Shutdown grace must cover one configured host deadline plus two complete frame deadlines, and it is one grace for the whole process: the Unix drain, the provider-storage GC drain, and the web-UI drain share a single deadline rather than taking one each.

`maxReplayIds` should be at least `auditMaxRecords`; the Helm chart's default configuration sets both to 200 000. The built-in default does not satisfy this: `brokerLimits.maxReplayIds` defaults to 100 000 while `serverLimits.auditMaxRecords` defaults to 200 000, so a configuration that omits `brokerLimits` refuses every invocation with `capacity-exhausted` at half its audit budget; set `brokerLimits: { maxReplayIds: 200000 }` explicitly. Both bounds are permanent when reached — the ledger never evicts, is restored from durable history on restart, and the audit log does not rotate — and a denial spends one audit record but a full ledger slot, so an undersized ledger refuses every invocation with `capacity-exhausted` long before the audit bound it was meant to outlast.

### Compilation cache and the concurrent memory budget

```yaml
compileCachePath: /var/lib/dekopon/compile-cache
hostLimits:
  # …every other field…
  maxTotalMemoryBytes: 268435456
```

`compileCachePath` is optional. Absent, Cranelift compiles every component at every start and the
socket binds only after that work finishes — the cost a startup probe has to cover. Present, the
broker keeps Wasmtime's content-addressed cache there and a restart reads compiled code back
instead. The directory holds code this privileged process executes, so its parent must be
owner-only under the same rule as the socket and audit paths; the broker creates the directory
itself. Components already compile concurrently rather than one at a time either way.

`hostLimits.maxMemoryBytes` bounds one invocation. Nothing bounds all of them at once, so the worst
case is `serverLimits.maxConnections` × `maxMemoryBytes` — 64 × 64 MiB = 4 GiB at the defaults,
which no small container survives. The broker states that product in one startup line so it is
budgeted rather than discovered. Optional `hostLimits.maxTotalMemoryBytes` enforces it: a store that
cannot reserve its share is refused before it exists, turning an OOM kill into a failed invocation.
It must be at least `maxMemoryBytes`, and it is deliberately absent from the authority commitment —
it is a concurrency budget, not a ceiling an authorization could narrow, so changing it does not
rotate stored authority.

```console
chmod 0700 /home/dekopon/.local/run/dekopon /home/dekopon/.local/state/dekopon
chmod 0600 /path/to/broker.yaml
dekopon-brokerd --config /path/to/broker.yaml
# Explicitly expose the unauthenticated informational UI on every interface:
dekopon-brokerd --config /path/to/broker.yaml --http-bind=0.0.0.0:8080
```

SIGINT and SIGTERM stop Unix and HTTP acceptance together, drain bounded in-flight connections concurrently under one shutdown grace, synchronize audit/checkpoint appends, log the verified chain head, and remove only the Unix socket inode created by this process.

## Read-only web UI

`--http-bind <ADDRESS>` enables a second, TCP listener; without the flag the broker opens no HTTP port. `/` returns a permanent redirect to `/ui`. The HTTP router accepts only `GET`/`HEAD`, has no login and no mutation endpoint, sends `no-store`, `nosniff`, `no-referrer`, and a closed content-security policy, and escapes every authored or component-provided string.

The overview includes:

- the latest bounded catalog-agent inventory reported by `dekopond`, including declared providers, capabilities, and least-privilege provider permissions;
- provider-reported input/output token totals and explicit counts of model calls that omitted each usage field;
- a table of provider components loaded into this broker;
- host-observed Wasmtime compilation, store, instantiation, invocation, fuel, memory/table limiter, HTTP count/byte statistics, plus every configured host ceiling; and
- credential-free OTLP endpoint, transport, service name, timeout, and payload mode. Header and resource-attribute **values** are never retained or rendered.

A provider page is intentionally rustdoc-like: local artifact path, source byte count and SHA-256, Wasmtime-visible imports/exports and nested interface functions, command words, every capability's description/effect/risk/idempotency/input schema, and the complete validated manifest. The host executes local WebAssembly component bytes and reports the digest of its exact compile buffer. A managed lock separately retains the OCI source and manifest digest, but the UI is not yet given that lock context and says so rather than presenting the component digest as publisher provenance.

Agent and token state still belongs to the unprivileged gateway. A mapped attestor may publish a content-free normalized inventory and bounded usage deltas over the authenticated Unix protocol. Reports omit instructions, prompts, answers, subjects, principals, credentials, policy, constraints, and authorization; are held only in process memory; reset on broker restart; and are never consulted by Cedar, routing, execution, evidence, replay, or durable audit. Reporting is best effort, so the live totals are not billing reconciliation—use the displayed OTLP configuration and `accounting.model.call` for retained accounting.

“No login” makes the surrounding network the access boundary. Agent names, provider schemas, artifact paths/digests, OTLP endpoints, and runtime limits/activity are deployment information. `--http-bind=0.0.0.0:8080` deliberately exposes it on every interface; choose that address only when everyone who can reach it may read those facts.

## Audit checkpoint and recovery

The checkpoint is one strict, hard-4-KiB-bounded, newline-terminated JSON object with API version `dekopon.dev/audit-checkpoint/v1alpha1`, the retained record count, and the SHA-256 chain head. A dedicated owner-only lock permits one broker writer. Every audit append is synchronized before the checkpoint is written to a new owner-only file, synchronized, atomically renamed, and followed by a parent-directory synchronization.

At startup, the checkpoint must identify an exact prefix of the fully verified audit chain. This detects replacement, truncation, and valid-prefix rollback relative to the retained checkpoint. An audit that is exactly one record ahead of a valid checkpoint is the recoverable crash window and advances the checkpoint; a larger gap fails closed. A non-empty audit without a checkpoint, or any checkpoint that is not a retained prefix, fails closed and requires explicit operator recovery from trusted copies. Do not delete only one file to bypass recovery.

The backing filesystem must honor Unix no-follow opens, advisory exclusive locks, same-directory atomic rename, and file/directory synchronization. Retain or export checkpoint generations in an independently protected system if rollback by the host owner or storage administrator is in scope. Deleting or rolling back both local files together cannot be detected by local state alone.

### Verifying a chain offline

`audit verify` runs the same sequence, previous-hash, and record-hash check the daemon runs at
startup, without binding a socket, reading daemon configuration, or taking the daemon's exclusive
lock — so it also answers for a retained copy the broker is no longer serving:

```console
dekopon-brokerd audit verify \
  --audit-path /home/dekopon/.local/state/dekopon/audit.jsonl
```

It prints the record count and the chain head, takes the same `--output json` and the same usage
and exit rules as the provider commands above, and exits non-zero with the reason on any failure.
A broken chain is reported separately from a file that could not be read: an interrupted append
leaves an unterminated final record, which is not the same finding as a record that was edited.
The whole chain is held in memory while it is checked, so a log past the default
`auditMaxRecords` is refused rather than read.

## Boundaries

- The service accepts one strict bounded request per fresh Unix connection.
- A transient `accept` failure — descriptor or kernel-buffer exhaustion, an aborted peer, a signal — is logged as `broker_accept_retried` and retried after a short backoff. Only a fault that says the listener itself is unusable ends the process, because ending it costs a container restart and a full provider recompile before the socket rebinds.
- Peer UID mapping is trusted configuration; payload identity claims do not exist.
- Authorization decisions come from the Cedar policy set; execution bounds come from
  `constraintSets` and are validated against loaded manifests, host ceilings, and the credential
  store at startup. Neither file can widen the other.
- Audit records carry the determining `policy_ids`, the `policy_digest` of the evaluated set, and
  the symbolic name of the `credential` the invocation selected.
- Generic WASI and ambient I/O imports remain unavailable.
- The durable JSONL chain is mutation-evident and replay-restoring. The separate atomic checkpoint makes the retained head externally inspectable, but is not signed, remote, append-only, or a transparency service by itself.
- Credential resolution is destination-bound, capability-scoped, and optionally agent-scoped. Providers receive only explicitly linked Dekopon host interfaces and policy constraints; an injected credential exists solely inside the native HTTP engine and is never observable by guest code.
- Direct `dekopon-run` subcommands retain their import-free host. Only explicit `dekopon-run broker` subcommands connect as unprivileged identity-free clients.

## Optional provider storage and chat memory

Presence of `storage` requires every field; absence links storage imports only to a disabled sticky
context. `rootPath` is disjoint from every broker-owned file/provider path and
`namespaceKeyPath` is one no-follow, server-owned `0600`, single-link, ≤4 KiB document under safe
ancestors. A deployment with retained data and a missing or changed key fails closed.

```yaml
storage:
  rootPath: /var/lib/dekopon-provider-storage
  namespaceKeyPath: /etc/dekopon-storage-key/storage-key.yaml
  maxRootBytes: 2147483648
  maxNamespaces: 4096
  maxNamespaceBytes: 67108864
  maxFilesPerNamespace: 64
  maxFileBytes: 16777216
  maxOpenHandles: 256
  maxHandlesPerInvocation: 32
  maxHostCallsPerInvocation: 4096
  maxReadBytesPerCall: 262144
  maxReadBytesPerInvocation: 16777216
  maxWriteBytesPerCall: 16777216
  maxWriteBytesPerInvocation: 16777216
  maxEntropyBytesPerCall: 256
  maxEntropyBytesPerInvocation: 4096
  lockTimeoutMs: 5000
  finalizationBudgetMs: 5000
  maxPendingTransactions: 64
  startupMaxEntries: 100000
  startupMaxTransactions: 1024
  maxQuarantinedNamespaces: 128
  retiredGenerationGraceMs: 86400000
  retiredGenerationTtlMs: 604800000
  inactiveNamespaceTtlMs: 31536000000
  gcIntervalMs: 3600000
  gcMaxNamespacesPerPass: 64
  gcMaxBytesPerPass: 67108864

chatMemory:
  continuityPolicy: authority-bound # safe default; stable must be explicit
  enabledAgents: [reviewer]
  maxLookbackTurns: 200
  maxRecentTurns: 20
  maxSearchResults: 20
  maxQueryBytes: 256
  maxResultBytes: 65536
  maxTurnBytes: 32768 # complete canonical turn JSONL line, including LF
  maxDedupRecords: 16000
  maxDedupBytes: 4194304
  compactionTargetBytes: 8388608
  compactionThresholdBytes: 12582912
```

The three capabilities that make up the surface are named by their `route:`, not by their spelling.
Exactly one constraint set declares each of `chatMemoryRecord`, `chatMemoryRecent`, and
`chatMemorySearch`; they must all name one provider, and each must declare `jsonl` chat storage at
the access its role implies — read-write for record, read-only for the two reads. Every conflict is
reported together at startup rather than one per run.

```yaml
constraintSets:
  memory.chat.record:
    route: chatMemoryRecord
    provider: memory-chat
    effect: local-write
    risk: Medium
    idempotency: conditional
    constraints:
      timeoutMs: 30000
      maxOutputBytes: 131072
      storage: { interface: jsonl, access: read-write, namespace: chat }
  memory.chat.recent:
    route: chatMemoryRecent
    provider: memory-chat
    effect: read-only
    risk: High
    idempotency: idempotent
    constraints:
      timeoutMs: 30000
      maxOutputBytes: 131072
      storage: { interface: jsonl, access: read-only, namespace: chat }
  memory.chat.search:
    route: chatMemorySearch
    provider: memory-chat
    effect: read-only
    risk: High
    idempotency: idempotent
    constraints:
      timeoutMs: 30000
      maxOutputBytes: 131072
      storage: { interface: jsonl, access: read-only, namespace: chat }
```

`route:` is the only thing that reserves a capability. Omitted, it is `generic`, and the capability
is an ordinary one on every path however it is spelled: a capability called `memory.chat.export` or
a provider called `memory-chat` is reserved by nothing. Declared, it takes the capability off the
generic listing, run, resolve, and invoke paths, takes every command word of its provider out of the
non-chat vocabulary, and makes the record route reachable only through the delivered-turn
operation. Renaming the shipped provider therefore drops no reservation, and the reserved names are
the ones this deployment chose.

Each recent/search constraint set's `maxOutputBytes` must leave 1024 bytes beyond
`chatMemory.maxResultBytes` for the SDK response envelope; record must leave the same fixed envelope
headroom. Enabling `chatMemory` also requires the routed provider to declare exactly those three
capabilities and no fourth. Memory/storage composition also rounds each 256 KiB JSONL read request when checking the
invocation and host-call budgets, requires both logical files, and reserves the post-append old file,
staged replacement, permanent dedup copies, and transaction metadata. Startup accounts the
worst-case JSON escaping of a bounded search query and additionally proves that raw/decoded files
plus canonical-ABI compaction copies and fixed allocator headroom fit the independent Wasm
linear-memory ceiling.

The gateway peer's attestor additionally needs `chatScopes`. Breadth is an explicit tagged value:
`transportWide`, `exactChannel`, or `exactConversation`; each names transport kind and configured
transport ID, and narrower forms name canonical channel/conversation. A local transport must also
name `localSubjectService`. Subject namespace authority remains independently required. Scope
fields enter Cedar as optional `transportKind`, `transport`, `channel`, and `conversation`.

Filesystem cancellation cannot guarantee a stuck native `fsync` returns by a hard deadline. The
lease and reservation remain held while a started blocking job drains, and shutdown grace must
cover host timeout + lock timeout + finalization budget + two frame deadlines; a failed kernel or
filesystem may still exceed it. Hostile same-UID mutation remains out of scope.

```yaml
identities:
  - uid: 65532
    principal: dekopond-gateway
    actor: { type: service, principal: dekopond-gateway }
    attestor:
      namespaces: [slack.t0123abc]
      chatScopes:
        - breadth: exactConversation
          kind: slack
          transport: scientist-slack
          channel: c0123abc
          conversation: c0123abc:1712345678.000100
```


## Model and effort control ceiling (unreleased)

Optional startup-fixed broker configuration (omitting it disables controls):

```yaml
controlTargets:
  - model: baseline
    efforts: [providerDefault, low, high]
  - model: gpt-5.6-sol
    efforts: [providerDefault, low, high]
```

At most 16 entries; model IDs match `[a-z0-9][a-z0-9._-]{0,63}`. Duplicate models, repeated efforts,
and empty effort sets are reported together. These aliases name configured gateway clients,
never endpoints or credentials. Gateway route configuration must independently narrow candidates;
an allowlist is not a policy grant.

Both halves of a proposal must be listed here. A route whose baseline model and effort do not
themselves appear in that model's `efforts` makes *every* proposal from that route `target-denied`,
because the broker checks the `from` selection as well as the `to` one — and each of those refusals
still spends one of the job's four control attempts. So list the route's own baseline effort, not
only the ones a model may switch to. The gateway's `select_model` tool mirrors this by offering only
efforts some candidate actually carries; an effort no candidate lists would cost prompt tokens and
an attempt to be refused by the broker.

Alongside a separate `agent.prompt` permit, for example:

```cedar
permit(principal == Dekopon::Principal::"maintainer",
       action == Dekopon::Action::"agent.model.select",
       resource == Dekopon::Agent::"reviewer")
when { context has via && context.via == "gateway"
    && context.toModel == "gpt-5.6-sol" };
```

Changing effort additionally requires `agent.effort.set`; a model permit alone never covers it.
Attested controls require an explicit matching `chatScopes` grant, unlike legacy subject-only
provider operation fallback. A direct peer can control only its owner-mapped agent. Controls
share the durable replay domain and persist admission audit plus checkpoint before replying.
Lockstep protocol `v1alpha3` rejects older envelopes before dispatch. Retain audit/checkpoint,
stop gateway, drain broker, replace binaries, start broker, then gateway. Older binaries cannot
read the new `ControlDecision` audit variant. See `docs/upgrading.md` in the repository.
