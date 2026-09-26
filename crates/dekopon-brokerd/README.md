# dekopon-brokerd

`dekopon-brokerd` is the separately deployed privileged Unix service for Dekopon provider
components. It derives caller identity from Unix peer credentials, evaluates a deny-by-default Cedar
policy set against owner-authored execution constraints, records every decision as a structured log
event inside the caller's trace, and executes only statically linked Dekopon host interfaces.

Authorization and execution constraints are two separate files on purpose. `policiesPath` decides
*who may do what*; `capabilities` decides *how narrowly the broker then does it*. A policy edit
can never widen a timeout, reach a new host, or bind a credential that was not already bound.

The broker accepts configured Unix peer UIDs, including a dedicated gateway UID. Every process
running under a mapped UID can act as its configured principal and actor; group membership permits a
connection, not an identity grant. Request payloads cannot provide or override identity, policy,
constraints, credentials, or authorization.

## Health probe

```console
dekopon-brokerd probe --socket /run/dekopon/broker.sock
```

Run as the broker owner UID, mapped separately from the gateway in `identities`. The probe is an
ordinary authenticated client, so a configuration whose `identities` omit the broker's own UID
refuses its own health check and logs `broker_peer_unmapped` with that UID. The protocol client
verifies socket safety — the socket and its parent directory — and the live server UID against its
own effective UID, then requests capabilities with a two-second complete-exchange deadline and the
default frame ceiling. An empty authorized listing is healthy.
Success exits 0 without output; absent, refused, unmapped, wrong-server, malformed, or stalled
endpoints exit 1 with a diagnostic. Missing arguments exit 2. The probe rejects `--config`, loads no
components or credentials, invokes nothing, and initializes no telemetry.

## Check a configuration

```console
dekopon-brokerd check broker.d --provider-set providers.yaml --store ~/.cache/dekopon-check
dekopon-brokerd check broker.yaml --output json
```

`check` runs the startup validation the daemon runs — strict decoding, fragment merging, provider
loading, Cedar policy against the declared world, capability blocks, constraint sets against the
loaded manifests, frame and memory ceilings — and stops before binding anything. It prints every
problem and warning at once, one per line or as `{ "ok", "problems", "warnings" }`, and exits 0
with no problems, 1 with problems and 2 on a usage error. Configuration files may be the invoking
user's own 0644 files, as in a Git checkout; symlinks and group/world-writable files are still
refused. Runtime paths are not required to exist and are never touched: the socket and its parent,
`credentialsPath`, `secretMapPath`, `assets.rootPath`, `storage.rootPath` (a throwaway directory
stands in for it), `providerSet.lockPath`/`storePath` and `http.extraCABundles`. Secrets are not
read: each credential a capability can select and each secret a policy names is reported as a
warning naming what boot will require, and credential destination coverage and the secret map are
not proved. A `providerSet` is resolved from `--provider-set` into the private `--store` directory
(its lock and blobs, kept between checks), which fetches from the registry; a configuration naming
provider paths loads them directly. The peer-UID socket-parent check and the cwasm cache are
runtime-only and skipped, and a configuration with no `identities` (the chart renders them into
`peers.yaml`) is a warning rather than a problem.

## Configuration

The configuration must be a regular single-link file owned by the server UID and must not be
group/world writable. The socket has the separate IPC directory contract below. Provider components
must be regular single-link files owned by the server UID and must not be group/world writable;
their canonical parent directories must also be server-owned and not group/world writable. Writable
non-sticky path ancestors are rejected.

```yaml
# broker.yaml
apiVersion: dekopon.dev/brokerd/v1alpha1
socketPath: /home/dekopon/.local/run/dekopon/broker.sock
policiesPath: /home/dekopon/.config/dekopon/policies.cedar
providers:
  - /home/dekopon/lib/dekopon/cli-probe-provider.wasm
  - /opt/dekopon/providers          # a directory loads every *.wasm directly inside it
identities:
  - uid: 1000
    principal: dekopond-gateway
    actor: { type: service, principal: dekopond-gateway }
    attestor: {}                    # speaks for exactly the subjects under `principals`
principals:
  local-user:
    subjects: [slack.t0123abc.u9xyz]
capabilities:
  cli-probe:
    constraints:                    # inherited by every capability listed below
      timeoutMs: 30000
      maxOutputBytes: 1048576
    capabilities:
      cli-probe.upper: {}           # only listed capabilities run
```

```cedar
// policies.cedar — chmod 0600, owner-owned, single-link, 1 MiB maximum
@id("local-user-may-prompt-reviewer")
permit(principal == Dekopon::Principal::"local-user",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"reviewer");

@id("reviewer-probe-for-local-user")
permit(principal == Dekopon::Principal::"local-user",
       action == Dekopon::Action::"cli-probe.upper", resource)
when { context.agent == "reviewer" };
```

Every capability runs inside an attested agent session: `context.via` and `context.agent` are
always present, and a directly connected peer is granted no capability whatever the policy says.
Each capability's `effect` and `risk` come from its provider's manifest and reach a decision as
`dekopon-policy`'s [context](../dekopon-policy/README.md#context).

A capability entry may override any constraint field; a field replaces the provider's value whole,
so a list such as `allowedHosts` is never appended to. An entry may also set `route` or its own
`credential`.

### A configuration directory

The config path may name a directory instead of a file. Every `*.yaml` directly inside it is a
fragment of the same document and every `*.cedar` is part of the policy set, both read in filename
order under the same file rules. `principals`, `agents`, `capabilities` and `providerSettings`
union by entry name, `identities` and `providers` concatenate, and every other key is set by exactly
one fragment. Any collision refuses startup listing each colliding key with its files, so no
fragment overrides another and file order never changes the result. `policiesPath` is not allowed
in a directory.

```text
broker.d/
  host.yaml          socket, telemetry, limits
  peers.yaml         identities
  family.yaml        principals in group family      family.cedar
  mediawiki.yaml     capabilities.mediawiki
```

`policiesPath` is read under the configuration's own rules: the path is canonicalized, then opened
without following symlinks and required to be a regular server-owned single-link file that is not
group/world writable, at most 1 MiB, and valid UTF-8. It is required once any `capabilities` entry
exists — a broker that declares executable capabilities and no policy would refuse every request
while looking configured. An absent path means an empty policy set, which permits nothing.

Each `providers` entry is a component file or a directory of them. A directory loads every `*.wasm`
directly inside it — not recursively — in **filename order**, because the registry builds its
capability route table in load order and readdir order would let two runs over one directory
disagree about which provider claimed a duplicate capability. The directory must be owned by this
UID and not group- or world-writable: anyone who can write it can add a provider the broker will
compile and run. Every file the scan yields is checked on its own exactly as a directly named one
is. An empty directory is an error, not a silent zero providers.

There is no implicit provider search path: every directory the broker loads code from is named in
this owner-only file and nowhere else.

### Plaintext HTTP destinations

The native HTTP host refuses `http://` to anything but a loopback destination: a credential
injected into a request that crosses a network in the clear is a credential on that network. A
broker owner who runs a service that speaks only plaintext on a network they control opts that
exact host out, and only that host:

```yaml
# broker.yaml
http:
  plaintextHosts:
    - rpi.lan
    - openobserve.openobserve.svc
```

Entries are exact hostnames matched case-insensitively. No wildcards, so a listed host widens
nothing beyond itself; no ports, because what this relaxes is the scheme rather than the socket;
no scheme, path, or IPv6 literal. An entry that is none of those refuses startup naming the entry,
and a non-empty list is logged once at INFO as `http plaintext hosts allowed: [...]`, so the trace
log carries the decision rather than only the consequence.

The list reaches nothing on its own. A request still needs the capability's constraint set to name
its destination in `allowedHosts` — for `http://` that means the `host:port` form, since the bare
form matches only HTTPS on 443 — and still needs that set's `allowPlaintextLoopback`. What this
decides is whether the host will speak plaintext to a destination the authorization already
permits. Empty, the default, is the loopback-only rule unchanged.

### Trace propagation to first-party destinations

`propagateTrace` belongs to an HTTP constraint set and defaults to `false`. Opt in only
first-party endpoints inside your trust boundary, in a separate constraint set from third-party
destinations. For example, an installed provider's first-party capability can have:

```yaml
capabilities:
  internal-service:
    capabilities:
      internal-service.read:
        constraints:
          timeoutMs: 5000
          maxOutputBytes: 4096
          http:
            allowedHosts: [api.example.com]
            allowedMethods: [GET]
            maxRequests: 1
            maxRequestBytes: 1024
            maxResponseBytes: 4096
            propagateTrace: true
```

The broker sends W3C `traceparent` with the current trace id, the `http.request` egress span's
id and its trace flags on every buffered or streaming request under that grant. Without an OTel
context it sends no header and the request proceeds. Injected bytes do not count against guest
request limits. `tracestate` is never sent. A provider that sets either header itself is refused
with `InvalidHeader`, even when propagation is off. Destination, method and transport restrictions
remain independent; this flag grants no network access.

### Additional CA trust and non-public HTTPS egress

These are **independent** broker-owned settings. `extraCABundles` adds PEM roots to
HTTPS certificate validation alongside the built-in public roots; it does not grant
network access or pin a CA to an authority. Each absolute file is checked at startup
(64 KiB maximum; at most eight). Mount the trust-manager public-root ConfigMap
**broker-only** and restart the broker when it changes. No private key is mounted.
`nonPublicHttps` separately permits exact DNS authorities and explicit ports to
resolve to private unicast addresses; at most eight entries. For example:

```yaml
http:
  extraCABundles:
    - /var/run/dekopon-trust/roots.pem
  nonPublicHttps:
    - openobserve-tls.openobserve.svc.cluster.local:5443
```

Every DNS answer for a listed authority must be private unicast; a mixed answer,
loopback or link-local address is refused. Other HTTPS destinations still refuse
non-public addresses. In either case a Cedar grant must separately allow the
exact authority, method and path. DNS pinning, redirect refusal, proxy refusal and
byte/time budgets are unchanged. These settings affect only the broker's provider
HTTP client, not gateway model transports, OTLP exporters or the OCI provider manager.

### Owner-configured provider settings

`providerSettings` is an optional map from provider ID to a JSON object (at most the configured
provider limit and 4 KiB each). The broker supplies only the matching object to that provider
*during invoke*, not to `describe`, `run-command`, the model or other providers. Do not put
credentials here; credential injection remains broker-owned and DRN-bound. For example:

```yaml
providerSettings:
  openobserve:
    url: https://openobserve-tls.openobserve.svc.cluster.local:5443/openobserve
    org: default
    stream: dekopon
```

Provider settings do not authorize network access. An independent capability grant and its
HTTP constraints still restrict destination, method, path, request count and credential use.

Host, broker, and server limits have conservative defaults, including a 2 MiB frame ceiling, when
their entire sections are omitted. `hostLimits` and `brokerLimits` also default field by field, so a
partial section keeps the absent-section value for everything it does not name — which is what lets
a deployment set `maxTotalMemoryBytes` alone. `serverLimits` is all-or-nothing:
when it is present every field is required. Unknown fields and unknown API versions are rejected.

Startup also requires aggregate provider metadata, every mapped peer's capability response, and the
*widest* response any session could receive to fit the frame ceiling. That last bound is what
matters in a gateway deployment: the connecting peer is typically granted nothing itself, while the
principals its `principals` name hold the capability sets that reach the wire through an
attested `capabilities`.

```console
chmod 0700 /home/dekopon/.local/run/dekopon
chmod 0600 /path/to/broker.yaml
dekopon-brokerd --config /path/to/broker.yaml
```

SIGINT and SIGTERM stop Unix acceptance, drain bounded in-flight connections under one shutdown
grace, log `broker_stopped`, and remove only the Unix socket inode created by this process.
Shutdown grace must cover one configured host deadline plus two complete frame deadlines, and it is
one grace for the whole process: all Unix connections drain under that one deadline rather than
each taking a fresh grace period.

### Provider credentials

An optional `credentialsPath` names a second, stricter owner-only file (`0600`, single-link,
byte-capped) holding implicitly selected provider credentials. The secret values live only there —
never in this configuration — and a constraint set binds one by symbolic name with `credential:`.
Startup fails closed if a named credential is missing, the constraint set grants no HTTP authority,
or any `allowedHosts` entry is absent from the credential's `destinations`; at execution the native
engine injects `authorization: <scheme> <secret>` only after guest headers were validated and only
for destinations inside the binding. Evidence and audit record `credentialInjected: true`, never the
value. The terminal audit record also names which credential the invocation selected — the symbolic
name from this file, never the secret. *Committed direction:* `credential`/`agents.<id>.credentials`
bindings will be replaced by public DRNs; these examples remain current until that migration ships
([migration requirements](../../docs/design.md#legacy-credential-bindings)).

```yaml
# broker.yaml
credentialsPath: /home/dekopon/.config/dekopon/broker-credentials.yaml
capabilities:
  gh:
    capabilities:
      gh.pull-request.approve:
        credential: github-pat
        constraints:
          timeoutMs: 15000
          maxOutputBytes: 8192
          http:
            allowedHosts:
              - api.github.com
            allowedMethods:
              - GET
              - POST
            maxRequests: 2
            maxRequestBytes: 16384
            maxResponseBytes: 262144
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
  - name: chatgpt-gpt-image
    kind: chatgptSubscription
    authFile: /var/lib/dekopon/broker-chatgpt/chatgpt-auth.json
    destinations: [chatgpt.com]
```

Every entry takes `name`, `kind` and `destinations`. The rest is per kind, and the file is validated
as a whole: every missing field, every field that belongs to the other kind, every malformed name, and
every duplicate name is reported in one refusal, so fixing one does not reveal the next on the
following start.

| `kind` | Required | Prohibited | Presented as |
|---|---|---|---|
| `bearerToken` | `scheme`, `secret` | `authFile` | `authorization: <scheme> <secret>` |
| `chatgptSubscription` | `authFile` (absolute) | `scheme`, `secret` | `authorization: Bearer <access>` plus `chatgpt-account-id: <accountId>` |

A `bearerToken` `secret` must be at least 16 bytes of printable ASCII with no whitespace or control
bytes. The value is also what the native HTTP host searches an authorized response for before
returning it, and a short or phrase-shaped value would deny answers that never carried the
credential.

### `chatgptSubscription`: a credential the broker renews itself

*Committed direction:* its legacy `credential`/`agents.<id>.credentials` selection will be replaced by
public DRNs, retaining the refresh and injection behavior below
([migration requirements](../../docs/design.md#legacy-credential-bindings)).

A `bearerToken` is a value an operator rotates by hand. A ChatGPT subscription token is not: the
access token expires hourly and the refresh token rotates on every renewal, so the value to present
exists only at the moment of use. `authFile` names the credential document
[`dekopond auth chatgpt login --auth-file <path>`](../../docs/chatgpt-credential.md) writes, and the
broker runs exactly the same refresh protocol the gateway's model client runs — one definition,
`dekopon_model::chatgpt::CredentialFile` — 60 s before expiry, serialized across processes on an
advisory lock on a sibling `.lock` file, with the rotated record written back atomically.

Give the broker its own login. Never point it at the file a `chatgptSubscription` *model* uses: the
authorization server retires a refresh token's predecessor, so two independent holders of one file
eventually present a retired token and revoke the family for both. A second device login against the
same ChatGPT account is the supported shape.

Startup proves the file rather than trusting it. The `authFile` must be absolute and must pass the
same Tier A check as this credentials file — regular, owned by the broker's UID, `mode & 0o077 == 0`,
one hard link, opened `O_NOFOLLOW`, under a 64 KiB ceiling — its parent directory must be owner-only
**and writable**, because a rotated record is persisted by renaming a sibling temporary file over the
target, and the document must parse as a supported Dekopon credential. Any failure refuses startup
naming the cause, and a successful load logs `broker_chatgpt_credential_loaded` with how long the
access token has left. The refresh itself is the broker's own HTTPS call: it is not charged to the
invocation's `maxRequests`, does not appear in HTTP evidence, and leaves `credentialInjected: true`
and the symbolic name in audit exactly as a `bearerToken` would.

A renewal that cannot complete fails that invocation and nothing else; the broker keeps serving every
other capability. The two classes are separate reasons because an operator acts on only one of them:
a refresh-token family the authorization server has retired (`invalid_grant`,
`refresh_token_reused`, `refresh_token_invalidated`, `refresh_token_expired`) fails the invocation as
`credential-unavailable` and logs `broker_chatgpt_credential_reauth_required`, which means someone
has to run `dekopond auth chatgpt login --auth-file <path>` again; anything else — transport, a 5xx, a
malformed token response — fails as `credential-refresh-failed` and needs nobody. A renewal that
reached the authorization server but could not be written back logs
`chatgpt_credential_save_failed` and continues on the in-memory token, because by then the record on
disk is the retired one.

### Public DRNs and private sources

`secretMapPath` names a separate owner-only `dekopon.dev/secret-map/v1alpha1` document. A model may
propose one public logical DRN through the sandboxed curl Basic/Bearer forms, but possession grants
nothing: the broker requires ordinary capability policy, a separate `secret.use` Cedar statement,
and an exact private binding before one source snapshot is fetched. Providers receive neither DRN
nor bytes. Adapters cover secure files, Kubernetes projections and API objects, 1Password Connect,
Vault KV v1/v2, AWS Secrets Manager and SSM, GCP Secret Manager, and Azure Key Vault.

```yaml
secretMapPath: /etc/dekopon/secret-map.yaml
```

Map descriptors are validated without network at startup. Resolution is per authorized invocation,
with no stale fallback. Basic/Bearer rendering, path and query scope, injection limits, and the
credential echo check live in the native HTTP host. See
[`../../docs/secrets.md`](../../docs/secrets.md) for the strict map schema, source fields,
bootstrap-file hygiene, policies, and examples. `credentialsPath` and `secretMapPath` may coexist
today; the legacy selection bindings [will be replaced by public DRNs](../../docs/design.md#legacy-credential-bindings).

### One capability, one token per agent

*Committed direction:* `credential`/`agents.<id>.credentials` will be replaced by public DRNs while
preserving per-agent isolation ([migration requirements](../../docs/design.md#legacy-credential-bindings)).
The following describes the current syntax and validation.

A constraint set's `credential:` names the token every caller uses. An agent listed under top-level
`agents:` may rebind that name to another credential, which is what lets one capability reach two
organizations without being duplicated under a second capability namespace:

```yaml
# broker.d/scientist.yaml
agents:
  nestedset-github:
    credentials: { github-pat: github-pat-scientist-hq }
capabilities:
  gh:
    capabilities:
      gh.issue.comment:
        credential: github-pat
        constraints:
          timeoutMs: 15000
          maxOutputBytes: 8192
          http:
            allowedHosts:
              - api.github.com
            allowedMethods:
              - GET
              - POST
            maxRequests: 2
            maxRequestBytes: 16384
            maxResponseBytes: 262144
```

The key is the agent because a route already binds a transport and a match to an agent: one Slack
workspace or channel selects the agent that answers, and the agent selects the token. The name comes
from the attested context the broker derived from this file's own `attestor` grant and
`principals`, so a request payload cannot ask for a different token. A rebinding applies only to a
set that names the credential being rebound.

Every credential a set can select is validated at startup: each agent's rebinding of the set's
`credential` must name a credential the store holds whose `destinations` cover every
`allowedHosts` entry of *this* set, or startup is refused. A rebinding for an agent no policy can
reach is not an error — the broker holds no agent catalog, and the name is inert until a route and
a policy exist for it.

### Attested identity

A peer identity may carry an optional `attestor` grant, which lets it propose on behalf of an
authenticated external chat identity. `principals` is the other half: it is the only place a
canonical subject becomes a principal. The example's `credential` binding is current syntax that
[will be replaced by public DRNs](../../docs/design.md#legacy-credential-bindings); identity attestation
remains separate from credential selection.

```yaml
# broker.yaml
identities:
  - uid: 1000
    principal: dekopond-gateway
    actor:
      type: service
      principal: dekopond-gateway
    attestor:
      namespaces: [slack.t0123abc]     # optional segment-boundary prefixes; omitted = every mapped subject
principals:
  maintainer:                          # the only place a subject becomes a principal
    subjects: [slack.t0123abc.u9xyz]   # canonical: lowercase dotted segments
    groups: [maintainers]              # optional; Cedar sees `principal in Dekopon::Group::"maintainers"`
capabilities:
  gh:
    capabilities:
      gh.pull-request.comment:
        credential: github-pat
        constraints:
          timeoutMs: 15000
          maxOutputBytes: 8192
          http:
            allowedHosts:
              - api.github.com
            allowedMethods:
              - GET
              - POST
            maxRequests: 2
            maxRequestBytes: 16384
            maxResponseBytes: 262144
```

```cedar
// policies.cedar — the canonical attested workflow. The session gate (may this person drive this
// agent, through which gateway) and the surface that session reaches are two statements, and
// neither implies the other.

@id("boss-may-prompt-conditional-writer")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"xaviers-conditional-writer");

@id("conditional-writer-surface")
permit(principal == Dekopon::Principal::"cpetersen",
       action in [Dekopon::Action::"http-probe.fetch",
                  Dekopon::Action::"http-probe.conditional-write"],
       resource == Dekopon::Provider::"http-probe")
when { context.agent == "xaviers-conditional-writer" };
```

A grant is not a capability. It only lets the broker derive an attested context; what that context
may then do is a policy statement. `context.via` names the attestor and is always present, so a
policy may still pin one gateway; a directly connected peer carries no `via` and is granted no
capability at all.

The gateway names a subject and never a principal; `principals` is the only thing that
resolves one, and an unmapped subject resolves to nothing. Refusals are audited denials recorded
against the gateway's own principal, with reason `attestation-denied` (no grant, or a subject
outside its namespaces), `unmapped-subject` (granted, but no mapping names that subject), or
`agent-denied` (attested and mapped, but no policy lets that principal drive that agent). Startup
rejects duplicate mapping subjects and malformed namespaces.

### IPC directory and distinct peer UIDs

A broker-owned `0700` socket parent retains a `0600` socket for owner-only clients. Configuring any
peer UID other than the broker's own under such a parent is refused at startup, naming every
unreachable UID at once: the socket it would bind admits none of them. For a distinct gateway UID,
give the broker-owned parent the shared IPC group and mode `0710` (or `0750`). Group traversal
selects a `0660` socket in that exact parent group; there is no extra configuration key. The broker
must itself belong to that group to set the socket GID. Neither group writes nor any permissions for
others are permitted on the IPC parent. A symlink parent, unsafe ancestors, wrong owner, socket
symlink, hard link, wrong group, unsafe mode, or live listener replacement is refused.

Give the gateway membership in that IPC group, map its actual UID in `identities`, and set its
`broker.serverUid` to the broker UID. The protocol client checks socket and parent ownership and
mode — the same rule this server binds under — and the live server peer UID before writing a
request. Unmapped peers receive no capabilities even if their group lets them connect. Owner-only
clients remain valid.

Keep broker config, credentials, provider files, cache, and storage in their separate broker-owned
protected paths; the IPC directory is not a credential or data directory. The gateway's
local development **chat** socket is `0600` under its own private parent. The chart's distinct
container identities and init layout are documented in the chart.

`tests/ipc_process.rs` runs real broker and client subprocesses; an unprivileged run exercises
owner-UID access, not cross-UID isolation. In a disposable Linux root container, require the full
UID-switch, unmapped-peer, wrong-server, and private-file proof:

```console
DEKOPON_REQUIRE_CROSS_UID=1 cargo test -p dekopon-brokerd --test ipc_process --locked -- --nocapture
```

### Telemetry

An optional `telemetry` section enables OTLP export of broker spans and log records, the
[audit records](#audit) among them:

```yaml
telemetry:
  endpoint: http://rpi.localdomain
  transport: grpc            # grpc | http
  serviceName: dekopon-brokerd
  exportTimeoutMs: 5000
```

Spans carry provider input and the full HTTP URL, always: there is no metadata-only mode
([goal 2](../../docs/design.md#constitution)). They never carry a credential — `Redacted` values
render their marker wherever they are formatted, and headers and bodies are excluded outright. Audit
records are unaffected. Outbound HTTP trace propagation is separately opted in per constraint set
with [`propagateTrace`](#trace-propagation-to-first-party-destinations).

The section has no credential field. Ingest authentication is read from the standard
`OTEL_EXPORTER_OTLP_HEADERS` environment variable by the OpenTelemetry SDK, so a token never enters
this configuration file, the process command line, or a span attribute. Receiver routing travels the
same way: over gRPC OpenObserve reads the organization from an `organization` header and rejects
exports without it, so include `organization=<org>` alongside the token and `stream-name`. Export
failures disable telemetry and log the reason rather than preventing startup. Broker logs are
structured JSON on stdout, filtered by `RUST_LOG`.

### Compilation cache and the concurrent memory budget

```yaml
providerSet:
  lockPath: /var/lib/dekopon/providers/providers.lock.yaml
  storePath: /var/lib/dekopon/providers/store
compileOnLoad: false # default; true bypasses cwasm entirely
hostLimits:
  maxTotalMemoryBytes: 268435456
```

Managed providers default to immutable, file-backed compiled components under
`providerSet.storePath/cwasm/v1`. A missing source/engine index compiles the verified Wasm once,
atomically publishes a raw `.cwasm` named by its own SHA-256, then maps it with Wasmtime. Warm
startup stream-verifies each selected compiled artifact's length and SHA-256 once, checks engine
compatibility, and maps it without decompression or a whole-file buffer. Calls reuse the retained
component; they neither hash nor reopen it. Source Wasm is still checked against the provider lock
at every startup. The index binds source-Wasm SHA-256 plus Wasmtime's engine compatibility
fingerprint to compiled SHA-256 and length; changing engine configuration selects a new index.

`compileOnLoad: true` disables cache reads and writes and compiles from source at every startup.
Legacy `providers:` paths and offline provider-manager commands also compile without a cache.
`compileCachePath` is removed, not aliased; remove it from old configurations. The old compressed
Wasmtime cache is neither read nor migrated. Component startup runs one at a time off Tokio to bound
compiler memory and stop scheduling on the first failure; Cranelift may parallelize within a
component. The socket binds only after the entire registry validates.

Errors are fatal: a corrupt index/artifact, missing indexed object, incompatible mapped artifact,
publication conflict, or I/O failure is reported with its cause. There is no retry, eviction,
automatic repair, or fallback. Disable the feature explicitly to get running again, or stop all
brokers using the cache and remove its generated `cwasm` directory before restarting. Never rewrite
or truncate mapped files. The filesystem and local index are trusted; adversarial same-UID file
replacement is outside this feature's model. Use one cache publisher at a time. Compiled artifacts
are capped at 512 MiB each; before publishing, the cache refuses growth beyond 1,024 objects or
2 GiB of compiled-object bytes (separate from the source-Wasm store limit). Historical objects are
not automatically pruned. Keep the cache on persistent disk, not memory-backed `/tmp`, for
reclaimable pages. Artifact hashes detect corruption, not publisher provenance.

See [startup tracing](../../docs/observability.md#broker-execution-spans) for cache status, sizes,
source verification, compile/hash/publish/map stage timings, and total registry load time. These
measure cost, not RAM saved: compare process PSS and cgroup anonymous/file memory separately.

`hostLimits.maxMemoryBytes` bounds one invocation; `hostLimits.maxTotalMemoryBytes` bounds all of
them at once. It defaults to **256 MiB**, four concurrent stores at the default 64 MiB per store: a
store that cannot reserve its share is refused before it exists, turning an OOM kill into a failed
invocation. Raise it for a container that has the memory and wants the concurrency; the broker still
states `serverLimits.maxConnections` × `maxMemoryBytes` — 64 × 64 MiB = 4 GiB at the defaults — in
one startup line, because that product is what an unbounded aggregate would cost. An explicit
`maxTotalMemoryBytes: null` restores that unbounded behavior. The value must be at least
`maxMemoryBytes`, and it is absent from the authority commitment — it is a concurrency budget, not a
ceiling an authorization could narrow, so changing it does not rotate stored authority.

## Policy

Policies are [Cedar](https://cedarpolicy.com), validated at startup against a schema generated from
this configuration. Everything a policy may name has to exist: principals come from `identities` and
`principals`, providers and capability actions come from the loaded provider manifests, and
`agent.prompt` is fixed. A policy naming anything else refuses startup rather than becoming policy
that can never match.

| Cedar name | Comes from |
| --- | --- |
| `Dekopon::Principal::"…"` | an `identities` entry or a `principals` key |
| `Dekopon::Group::"…"` | a `principals.<id>.groups` entry; a group nobody belongs to refuses startup |
| `Dekopon::Provider::"…"` | a loaded provider manifest |
| `Dekopon::Action::"…"` | a loaded capability, or the fixed `agent.prompt` |
| `Dekopon::Action::"<provider>:*"` | every capability the provider declares |
| `Dekopon::Action::"<provider>:read-only"` | the provider's `read-only` capabilities; writes are never grouped |
| `Dekopon::Agent::"…"` | any agent name; the catalog belongs to the gateway, not the broker |
| `Dekopon::Secret::"drn:…"` | a public DRN declared by the owner-only secret map |
| `Dekopon::Action::"secret.use"` | fixed separate permission to consume one exact DRN |

The context each action carries is `dekopon-policy`'s
[context](../dekopon-policy/README.md#context). Every value is derived by the broker from
authenticated transport state or this configuration — never from a request payload, and never from
message content or provider input.

An `@id("…")` annotation is required on every policy. That name is what audit records carry in
`policy.ids`; without one Cedar would name policies positionally (`policy0`, `policy1`, …) and
inserting a policy would renumber the ones below it. Names must be unique.

At decision time a capability with no constraint set is denied `unconstrained-capability` before
Cedar is consulted at all. That refusal is unconditional and is what actually enforces anything.

`strict` (default `false`) decides whether startup *also* complains. Left alone, a policy naming a
capability no loaded provider offers, and a constraint set naming one, are both warnings: the
deployment starts, and each is logged as an `audit.event` so the mismatch is visible in traces. That
is what lets you ship policy for a provider you have not dropped in yet. Set `strict: true` for a
deployment whose provider set is fixed, where a mismatch means someone made a mistake — then each of
those warnings becomes a startup refusal.

One thing is fatal in both modes: a policy naming a principal that no `identities` or
`principals` entry declares. Principals come from this file rather than from a loaded
component, so an undeclared one is always a typo.

Bounds are startup-fixed: 1 MiB of source, 1024 policies, no templates, Cedar strict validation.
Evaluation errors deny.

## Managed provider sets

`dekopon-brokerd` contains its provider manager, so resolving an OCI reference, fetching its
component, validating it, and serving the locked bytes need no `wkg`, ORAS, Docker CLI, shell, or
package manager beside the broker binary. Provider management is a separate operator mode that exits
after changing local state; daemon startup is network-free, deterministic, and startup-fixed.

The operator authors exact references:

```yaml
# providers.yaml
apiVersion: dekopon.dev/provider-set/v1alpha1
providers:
  - source: ghcr.io/dekopon-agents/provider-gh:0.1.0
  - source: ghcr.io/dekopon-agents/provider-curl@sha256:0123...cdef
```

A source must carry a fully qualified registry and either an explicit tag or a canonical lowercase
SHA-256 **manifest** digest. A tag that looks like `1.2.3` is one exact OCI tag; it is never
silently interpreted as a SemVer range. An unchanged tag keeps the manifest digest already in the
lock. To request another resolution, change the authored reference. Explicit SemVer requirements,
networked outdated checks, and `update` are not in this format.

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

`--output json` gives deterministic machine-readable command results. A successful lock change
applies on the next broker restart; there is no hot reload.

Operator commands never take `--config`; combining them is a usage error. `provider` requires
`--lock-file` and `--store`; `sync` also requires `--provider-set`. Usage errors exit 2; a failed
command exits 1. Operator modes print results on stdout and text diagnostics on stderr at `warn`
(override with `RUST_LOG`); the daemon logs JSON on stdout at `info`.

Resolution accepts one OCI image manifest with schema 2, exact artifact type
`application/vnd.dekopon.provider.v1+wasm`, the standard empty OCI config, and exactly one positive,
bounded `application/wasm` layer. Manifest, token, error, and component streams have independent
byte ceilings and deadlines. Public registries use anonymous OCI Bearer challenge flow. Private
registry credentials and custom certificate roots are not accepted; TLS verification cannot be
disabled, ambient proxy environment variables are ignored, redirects may never downgrade to
unapproved plaintext, and plain HTTP is available only to an exact literal loopback authority named
with `--plaintext-loopback-registry <HOST[:PORT]>` (repeatable, `provider` subcommands only), for
development and tests.

Fetched bytes land at:

```text
<store>/blobs/sha256/<component-digest>.wasm
```

The manager serializes competing store and activation writers with owner-only advisory locks, writes
a temporary blob on the destination filesystem, bounds and hashes the stream, synchronizes it,
publishes without clobbering an existing content address, and synchronizes the parent. It validates
the **complete** proposed set with the broker host before atomically replacing the generated lock. A
failed multi-provider validation can leave an unreachable blob, but never a partially activated
lock. The blob directory has a hard lifetime ceiling of 4 GiB and 1,024 files (stale temporaries
count), checked under the store lock before another download, so repeated failed or changed
resolutions cannot grow it without bound. There is no `prune` command; reaching that ceiling
requires operator-reviewed cleanup until orphan deletion has its own safe lifecycle contract.

The generated lock is strict, byte-capped, source-sorted, timestamp-free, and records both
identities:

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

`providerSet` and `providers` are mutually exclusive. The lock, store, blob directories, and blob
files are trusted broker input: they must be owned by the broker UID, have protected parents, be
regular and single-link where applicable, and not be group/world writable. The daemon derives every
blob path from the locked component digest and performs no registry request. Most importantly, the
broker host compares the locked component length and SHA-256 against the **same single read buffer**
it passes to Wasmtime, then compares the bounded `describe` provider ID with the lock. A preflight
hash of a different read would not provide that guarantee.

A digest proves byte identity, not publisher identity. This manager verifies no GitHub release
provenance and no OCI attestation, so it does not replace the provenance checks in
`ci/stage-image-context.sh`, and the Dockerfile remains network-free. Container staging fetches each
component at a pinned release, checks its SHA-256, and verifies its provenance with
`gh attestation verify`; it does not use `provider sync --locked`, and a switch would have to keep
verifying provenance for each downloaded component.

## Audit

Audit is the `broker.decision` and `broker.execution` log events the broker emits inside the span
that made each decision: structured JSON on stdout always, and OTLP log records when `telemetry`
names a receiver, over the same endpoint and headers as the spans. Their fields and correlation are
documented in [observability](../../docs/observability.md#the-broker-audit-record).

Nothing else keeps a copy. `run` serves with `TraceOnlyAuditLog`, which accepts every record and
stores none, so no audit failure can refuse an invocation. Without `telemetry`, audit lasts as long
as whatever keeps this process's stdout; a deployment that needs audit past a restart configures
`telemetry`. Losing the log exporter loses audit. `RUST_LOG` filters only the stdout copy, so a level
stricter than `info` on the `dekopon_broker::audit` target drops the records from it.

`run` returns `Result<(), BrokerdError>` after clean shutdown.

## Boundaries

- The service accepts one strict bounded request per fresh Unix connection.
- A transient `accept` failure — descriptor or kernel-buffer exhaustion, an aborted peer, a signal —
  is logged as `broker_accept_retried` and retried after a short backoff. Only a fault that says the
  listener itself is unusable ends the process.
- Peer UID mapping is trusted configuration; payload identity claims do not exist.
- Authorization decisions come from the Cedar policy set; execution bounds come from
  `capabilities` and are validated against loaded manifests, host ceilings, and the credential
  store at startup. Neither file can widen the other.
- Audit records carry the determining `policy.ids`, the `policy.digest` of the evaluated set, and
  the symbolic name of the `credential` the invocation selected. Its legacy selection binding
  [will be replaced by public DRNs](../../docs/design.md#legacy-credential-bindings); this is the current audit shape.
- Generic WASI and ambient I/O imports are unavailable.
- Audit records contain metadata only.
- Credential resolution is destination-bound, capability-scoped, and optionally agent-scoped.
  *Committed direction:* legacy `credential`/`agents.<id>.credentials` selection will be replaced by public
  DRNs without weakening those bounds ([migration requirements](../../docs/design.md#legacy-credential-bindings)).
  Providers receive only explicitly linked Dekopon host interfaces and policy constraints; an
  injected credential exists solely inside the native HTTP engine and is never observable by guest
  code.
- Unprivileged clients submit proposals over the authenticated protocol; only the broker executes
  providers.

## Optional provider storage and chat memory

Presence of `storage` requires every field; absence links storage imports only to a disabled sticky
context. `rootPath` is disjoint from every broker-owned file and provider path.

`maxReadBytesPerInvocation` bounds what one invocation pulls into memory: each positional
durable-file read or JSONL chunk charges the length it requests, and a JSONL append or replacement
charges the one working copy it loads. Durable-file writes, truncates, removes, and renames charge
it nothing — their lengths come from `statat` — and are bounded instead by `maxWriteBytesPerCall`,
`maxWriteBytesPerInvocation`, `maxFileBytes`, and `maxNamespaceBytes`. Size a durable-files
deployment's read ceiling for the largest result a provider reads back in one invocation, not for
the largest database it keeps.

```yaml
storage:
  rootPath: /var/lib/dekopon-provider-storage
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
capabilities:
  memory-chat:
    capabilities:
      memory.chat.record:
        route: chatMemoryRecord
        constraints:
          timeoutMs: 30000
          maxOutputBytes: 131072
          storage:
            interface: jsonl
            access: read-write
            namespace: chat
      memory.chat.recent:
        route: chatMemoryRecent
        constraints:
          timeoutMs: 30000
          maxOutputBytes: 131072
          storage:
            interface: jsonl
            access: read-only
            namespace: chat
      memory.chat.search:
        route: chatMemorySearch
        constraints:
          timeoutMs: 30000
          maxOutputBytes: 131072
          storage:
            interface: jsonl
            access: read-only
            namespace: chat
```

`route:` is the only thing that reserves a capability. Omitted, it is `generic`, and the capability
is an ordinary one on every path however it is spelled: a capability called `memory.chat.export` or
a provider called `memory-chat` is reserved by nothing. Declared, it takes the capability off the
generic listing, run, resolve, and invoke paths, takes every command word of its provider out of the
non-chat vocabulary, and makes the record route reachable only through the delivered-turn operation.
Renaming the shipped provider therefore drops no reservation, and the reserved names are the ones
this deployment chose.

Each recent and search constraint set's `maxOutputBytes` must leave 1024 bytes beyond
`chatMemory.maxResultBytes` for the SDK response envelope; record must leave the same fixed envelope
headroom. Enabling `chatMemory` also requires the routed provider to declare exactly those three
capabilities and no fourth. Memory and storage composition rounds each 256 KiB JSONL read request
when checking the invocation and host-call budgets, requires both logical files, and reserves the
direct peak: the post-append turn file, live permanent dedup file, and conservative namespace entry
metadata including authority-pointer and manifest temporaries, without staged JSONL file copies.
Startup accounts the worst-case JSON escaping of a bounded search query and proves that raw and
decoded files plus canonical-ABI compaction copies and fixed allocator headroom fit the independent
Wasm linear-memory ceiling.

A chat claim that is canonical for the sender's service enters Cedar as the optional
`transportKind` and `transport` strings and the optional `conversation` record
`{kind, container, id, thread}`; the policy that grants the memory capabilities names the
conversations it covers.

```cedar
@id("maintainers-memory-chat-c0123abc")
permit(principal in Dekopon::Group::"maintainers",
       action in Dekopon::Action::"memory-chat:*", resource)
when { context.agent == "reviewer"
       && context has conversation && context.conversation.id == "c0123abc"
       && ["channel", "thread"].contains(context.conversation.kind) };
```

Filesystem cancellation cannot guarantee a stuck native `fsync` returns by a hard deadline. The
lease and reservation remain held while a started blocking job drains, and shutdown grace must cover
host timeout + lock timeout + finalization budget + two frame deadlines; a failed kernel or
filesystem may exceed it. Hostile same-UID mutation is out of scope.

## Ephemeral broker assets

The optional strict section below enables asset writers and streamed HTTP response spools:

```yaml
assets:
  rootPath: /var/lib/dekopon-assets
  maxInFlightBytes: 67108864
```

Both keys are required when the section is present. Startup requires a broker-owned 0700 directory,
refuses overlap with broker state or providers, and empties every entry without following symlinks.
Without the section, allocation and streamed HTTP refuse with a message naming `assets.rootPath`;
reading passed descriptors and permitted table operations need no broker disk.

The byte budget covers concurrent output writers and HTTP spools; exhausted capacity or ENOSPC
fails immediately without queueing or retry. Outputs use separate read-only descriptors and are
unlinked before delivery; received files are read positionally. Only a successful invocation
returns typed attached, removed, and sent effects. The backing volume must additionally accommodate
unlinked outputs retained by the gateway; those no longer count as broker in-flight work.

## Catalog ownership at policy startup

The agent catalog belongs to the gateway. Cedar declares `Dekopon::Agent` but does not enumerate
agent instances: a misspelled agent literal can validate and then deny every session. The gateway
rejects a route naming an absent catalog agent; operators must cross-check policy agent literals
against that catalog. Principal literals are always checked; undeclared providers and capabilities
are fatal with `strict: true`, otherwise reported as schema-only phantoms. Policy cannot widen
owner-authored execution constraints or bind another credential.
