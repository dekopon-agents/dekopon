# Architecture

Read [`design.md`](design.md) first for the product model and accepted invariants. This document maps that design to current crate boundaries and the deployment topology; where a piece remains committed direction (an authenticated transport for a pod boundary) it says so rather than presenting it as current.

## Published 0.12.0 foundation

The published `0.12.0` release is a structural scrub rather than a feature release: thirty-two
decided findings, one commit each, across every crate. The boundaries it moved are the ones worth
reading here. The local broker protocol is `dekopon.dev/broker/v1alpha2`, in which attestation is
one optional field on the request instead of a type-level axis multiplied across eleven request
variants, thirteen client methods, and nine broker entry points; the six `*_for` / `*_for_chat`
pairs are gone and a client upgrades in lockstep with its broker. Durable chat memory is keyed by a
typed `route:` on a constraint set, not by capability and provider names. `ProviderManagerError`
collapsed from seventy-three variants to ten. In that release, both Wasmtime hosts shared one host layer in
`dekopon_provider_sdk::host`, twelve hand-rolled trusted-file checks became one `dekopon-core`
predicate, and every process installs its telemetry subscriber from one builder. The interactive
console `0.11.0` added moved out of this tree to
[`dekopon-console`](https://github.com/dekopon-agents/dekopon-console); it was an unprivileged
broker client holding a model credential, so no responsibility moved with it. Since `0.12.0`,
unreleased on `main` and recorded under [`CHANGELOG.md`](../CHANGELOG.md#unreleased): skills
mounted from the catalog and read through `read_skill`, the opt-in
`suggest_improvement` tool.

`0.11.1` before it was a container-image patch: the CLI linked two weak-probed `GLIBC_2.39` symbols
the distroless Debian 12 runtime base (glibc 2.36) does not provide, and glibc's dynamic linker
refuses to load a binary naming a version node the runtime library lacks at all, weak reference or
not. The base moved to Debian 13 (glibc 2.41). No crate's source changed.
`0.11.0` gave `dekopon-shell` a real bash-script surface — compound commands as pipeline stages,
`[[ ... ]]`, enforced `set -e`/`-u`/`-o pipefail`, `read`/`getopts`, real parameter expansion, and
two script-addressable streams. `dekopon-provider-sdk-testkit` runs a provider component against
real storage in-process, without Cedar or the constraint catalog. The `gh` shell builtin and its
capabilities moved out of tree to `dekopon-provider-gh`, and `turso-sql`, a SQLite-compatible
engine, is now available to providers the same way.

The release otherwise retains the two deliberately separate synchronous execution surfaces introduced in 0.1, the privileged asynchronous host, authorization/evidence/audit libraries, separately deployed authenticated Unix broker, sandboxed one-tool scripting surface, and cross-process OpenTelemetry added in 0.2, and the unprivileged agent daemon `dekopond` added in 0.3, which connects to chat services and submits attested on-behalf-of proposals to the broker. Version 0.4 added distribution through release archives, a container image, a Helm chart, and a Homebrew tap; 0.5 added bounded chat attachments; 0.7 added credential-free gateway self-inspection; 0.8 added Discord Gateway transport; 0.8.1 raised the per-turn tool-call guard from four to ten; 0.9 added opt-in transport-native in-flight activity with authenticated cooperative Stop; and 0.10 added a text-only Meta WhatsApp Cloud API gateway transport, opt-in route-scoped OpenAI image generation, broker-owned namespace-bound provider storage with the optional generated `memory-chat` provider, and a 145-finding deep-review hardening pass. None moved a responsibility between processes. The unsupported, mock-only Skylight provider exploration is removed from this tree and its public standalone source is [`dekopon-provider-skylight-private`](https://github.com/dekopon-agents/dekopon-provider-skylight-private). No standalone release is claimed, and it remains absent from default catalogs, images, policies, and deployments.

The gateway executable owns model-account lifecycle without starting its daemon runtime or loading configuration:

```text
dekopond auth chatgpt
  -> fixed OpenAI device-auth host
  -> isolated Dekopon credential file
```

## Current workspace ownership

The workspace now contains 22 crates and ships only `dekopond` and `dekopon-brokerd`.
The standalone `dekopon`, `dekopon-webui`, `dekopon-run`, and `dekopon-provider-host`
crates are retired. Published versions remain available; this is not a recall or yank.
The independently shipped console remains an unprivileged consumer of shared libraries;
publication and a later console repin are separate follow-ups.

The gateway owns chat/model credentials, bounded agent sessions, and proposals, never
provider credentials or broker authority. The broker alone authorizes provider effects,
resolves destination-bound credentials or separately authorized public DRNs, and appends
owner-only JSONL audit. Replay rejection is process-local, with no audit tamper-detection
or crash-durability guarantee. No replacement general invocation CLI is introduced.

Of the crate boundaries below, the skill loader, `read_skill`, `suggest_improvement`, and `improvementSuggestions` entries are that unreleased post-`0.12.0` work. Crate boundaries are:

- `dekopon-core`: validated identifiers and dependency-light domain enums, including canonical public `SecretDrn` and typed inert `SecretUseProposal` values, and the `SkillId` name grammar a mounted skill's directory and front matter must share. It also owns the two facts separate processes must not disagree about at the filesystem: what makes a local file trusted input, and which of the two permission tiers — private, or merely not world-writable — a given file is held to.
- `dekopon-capability`: capability metadata and proposal/authorization invocation states.
- `dekopon-protocol`: strict `dekopon.dev/v1alpha1` resources and list responses.
- `dekopon-config`: discovery, parsing, duplicate detection, and reference validation, plus the bounded in-memory `Skill` loader for `SKILL.md` directories, which the catalog runs at load time, so a session never touches the filesystem for one.
- `dekopon-provider-sdk`: typed Rust guest trait, manifest/response wire types, and adapters for its default or a caller-generated provider world. Its optional non-default `host` feature additionally holds the shared manifest rules, conflict reports, store bounds, and engine construction used by the broker host, testkit, and external embeddings; a guest build never enables it.
- `dekopon-provider-http`: guest-only Rust facade for the published buffered HTTP interface.
- `dekopon-provider-storage`: feature-gated guest-only JSONL and durable-files bindings with no
  namespace/path/authority API.
- `dekopon-http-host`: statically linked native buffered HTTP engine that consumes HTTP constraints beneath independent ceilings; it equality-checks authorization-bound DRN credentials, enforces secret-specific authority/method/path/query/injection scope, renders Basic/Bearer, and refuses direct reflection.
- `dekopon-storage-host`: privileged Wasmtime-independent namespace/key/layout/quota/lease/
  direct-write/JSONL/durable-file engine. It is absent from the normal dependency tree of `dekopond`; gateway integration tests may use it as a dev-dependency. The opposite-direction CI gates also exclude orchestration/model crates from `dekopon-brokerd`.
- `dekopon-broker-host`: privileged asynchronous Wasmtime component host adapting project-owned HTTP and storage imports, accepting only authorized invocations and exact single-use storage grants.
- `dekopon-policy`: the bounded, deterministic Cedar adapter — a schema generated from the deployment's declared principals, providers, and capabilities; strict startup validation; entity literals proved against that world; deny on any evaluation error; determining policy identifiers and a policy-set digest per decision. It is consumed only by `dekopon-broker` and `dekopon-brokerd`, and it holds no execution authority: constraint sets stay outside the policy language.
- `dekopon-broker`: deny-by-default Cedar authorization over owner-authored execution constraint sets, trusted context binding, single-use authorization, process-local replay rejection, public evidence, and bounded in-memory or append-only owner-only single-writer JSONL audit coordination around the component host.
- `dekopon-broker-protocol`: lightweight strict versioned messages and an unprivileged Unix client carrying proposals/results but no identity or authorization fields; it has no broker-host or native-HTTP dependency.
- `dekopon-brokerd`: Unix-only privileged process with strict owner-controlled configuration, a separate owner-only Cedar policy file and per-capability constraint sets, private socket lifecycle, peer-UID context mapping, legacy destination-bound credentials, an owner-only public-DRN/private-source map with bounded per-invocation adapters, bounded concurrency/shutdown, process-local replay rejection, provider execution, append-only audit, and a separate `provider` command tree (sync/list/verify, the provider manager). The manager keeps operator-authored exact OCI references, generated immutable resolutions, and installed content-addressed bytes distinct; daemon startup constructs no registry client and passes expected lock identities into the component host's exact-read compile boundary.
- `dekopon-model`: bounded chat-model contract, OpenAI-compatible transport, and isolated ChatGPT/Codex authentication and Responses client. It holds no image client: producing an image is a provider effect behind the broker, not model inference.
- `dekopon-telemetry`: OTLP exporter settings and the subscriber wiring the exporting executables install. Ingest credentials are read from the standard `OTEL_EXPORTER_OTLP_HEADERS` environment variable, so no configuration file, command line, or span attribute holds one.
- `dekopon-shell`: sandboxed bash-flavored script parser and tree-walking interpreter whose command words dispatch to capabilities through one abstract seam; it links no Wasmtime, broker, HTTP, or filesystem code and owns its own step, recursion, output, deadline, and capability-call bounds. It emits a `tracing` span per command word, but links no exporter and knows no telemetry protocol: the embedding binary's subscriber decides where those go, the same way `curl` there assembles a request for a capability rather than opening a socket.
- `dekopon-process`: an unprivileged one-run/one-node Tokio lifecycle seam with one async `Process` trait. It records private stable run/node trace IDs, preserves either the typed operation result or Tokio `JoinError`, and transfers the node to a self-contained supervisor before its first await. While the owning Tokio runtime remains alive, that supervisor joins, records, and delivers the node to a required abandonment observer even if the outer `execute` caller is dropped. Its only cancellation is a cooperative `CancelHandle`/`CancelSignal` pair: the supervisor aborts a cancellable node's task at its next await and still joins it before reporting. It owns no scopes, ports, deadlines, graph scheduling, provider, Wasmtime, transport, policy, credential, retry, or persistence code. Its in-tree consumer is `dekopon-agent`'s cancellable `broker-command` node around each command word sent to the broker, the node a gateway session's Stop abandons.
- `dekopon-agent`: the shared agent session layer — the bounded scripting prompt loop with an optional cooperative cancellation probe, optional bounded meta tools (asset fetch, `inspect_agent_config`, `read_skill` over mounted skills, and `suggest_improvement`) each offered only when the embedder supplies or enables it, the script runtime that spends a session-wide capability budget on fresh interpreters, composite direct-then-broker capability dispatch, a synchronous broker-leg facade over the protocol client whose command-word runs are cancellable `dekopon-process` nodes. That leg also owns the two byte-carrying conventions: it strips a capability result's reserved `attachments` key into a request-local slot rather than a model message or prompt outcome, and expands a `chat-asset:<N>` input marker through an embedder-supplied reader for the capabilities the embedder lists, so attachment bytes never enter transcript or history in either direction. Its typed configuration view has no credential, identity, endpoint, path, constraint, or raw-policy field, and it lists mounted skills by name, description, and resource path, never their text. It holds no authority; it depends on `dekopon-config` for the `Skill` type it mounts, on `dekopon-process` for the node each broker command run executes in, and, of the broker crates, only on the client half of the protocol. `dekopond` and out-of-tree clients such as `dekopon-console` consume this layer.
- `dekopond`: Unix-only unprivileged chat gateway and agent daemon with strict owner-controlled configuration, Slack Socket Mode / Discord Gateway / Telegram long-poll / text-only Meta WhatsApp Cloud API / local development transports, first-match routing to catalog agents, route-scoped opt-in delivery of provider-produced attachments and route-scoped opt-in chat-asset capability inputs, typed text/attachment replies, authorization-fed bounded Slack Agent thread ownership, request-scoped optional no-reply decisions, opt-in transport-native in-flight activity after authorization, Slack Agent Stop handling through cooperative prompt/tool cancellation that also abandons an in-flight broker command run, admission-bounded sessions on the shared `dekopon-agent` loop, credential-free self-inspection built from gateway-owned prompt metadata plus the broker's fresh effective grant, the agent's catalog skills mounted on every session of its route with per-route opt-in `improvementSuggestions`, and attested on-behalf-of proposals to the broker. The WhatsApp listener is plain HTTP behind operator-owned TLS termination, authenticates exact raw webhook bytes before parsing, and answers text only through the pinned Graph messages endpoint without retrying outcome-unknown sends. It holds chat and model credentials, never a provider credential, and its dependency set excludes every privileged broker crate under the gateway CI dependency gate. Attachment delivery, activity, and continuation targets come only from owner configuration and authenticated transport envelopes; filenames, endpoints, credentials, status content, and its fixed Slack reaction fallback are gateway-owned rather than model-selected, and the model cannot select a thread or sender.
- `dekopon-provider-sdk-testkit`: in-process fake broker for provider test suites. It mints its own authorization through `AuthorizationGate`, skipping Cedar and the constraint catalog, and runs the real component against a real `StorageHost` over a temporary root. Consumed by out-of-tree provider repositories; it holds no authority of its own and nothing in the daemon path depends on it.
- `dekopon-test-support`: unpublished shared test scaffolding (`publish = false`), reached only as a path `[dev-dependencies]` entry so it never enters a published manifest or the CI dependency-tree gates; see [`development.md`](development.md#repository-map).

## Deployment boundary

The deployment is two separate running processes; model auth is a separate gateway command:

```text
dekopond
    chat transports, routing, model interaction, bounded sessions,
    bounded per-conversation history; optional post-acceptance durable recording
    (durable retrieval remains explicit/on demand, never automatic prompt replay)

dekopon-brokerd
    authorization, credentials, provider execution, external effects,
```

`dekopond` is unprivileged and holds no broker authority. `dekopon-brokerd` is a separate process for local Unix deployment; a future pod boundary needs a different authenticated transport. Direct requests carry no principal or actor fields: the broker derives exact context from OS peer UID and trusted configuration, while unique invocation IDs in a bounded process-local ledger provide replay rejection. Attested requests add one typed on-behalf-of claim — a canonical external subject and the agent orchestrating for it — which the broker honors only under an owner-configured attestor grant, resolves to a principal through owner-controlled mappings alone, and admits only if policy permits that principal to drive that agent.

The gateway may now also accept a public HTTPS path after an external Cloudflare Tunnel/Traefik boundary forwards plain HTTP to its exact WhatsApp callback. That is a wakeup path only: HMAC-authenticated sender and WABA/phone routing facts become an attested proposal scope, while all policy, provider credentials, authorization, and effects remain in the broker.

The chart enforces the [current local process boundary](security-model.md#current-local-process-boundary): distinct gateway and broker UIDs, private mounts, and only group-reachable IPC.

A model-facing tool call is only a proposal. The daemon-to-broker request carries that proposal, not trusted identity context or an `AuthorizedInvocation`. The broker owns the authority transition from `ProposedInvocation` to `AuthorizedInvocation`; it creates and consumes that state inside the broker-owned execution boundary while evaluating policy, attaching constraints, invoking a provider, and recording evidence. `dekopond` never receives or presents serialized authorization state as a bearer grant, and agent code never receives raw provider credentials. Its complete contract is in [`dekopond.md`](dekopond.md).

## Shared host helpers and privileged provider authority

The SDK's optional host feature preserves manifest validation, complete conflict reports,
engine construction, and store limits without recreating the retired synchronous registry.
The broker host retains compiled components and creates fresh bounded stores per operation.
Its optional compilation cache stores code, never authority. Guest builds do not enable the
SDK host feature. Provider JSON Schemas must be object-shaped, but the host is not a general
JSON Schema validator: providers still validate operation-specific input.

Model authentication terminates in the model client, separately from provider authority. ChatGPT subscription mode owns a distinct device-flow credential file, refreshes tokens only against OpenAI's fixed authentication host, and sends inference only to the fixed Codex Responses host. It does not import another application's token store or expose model credentials to a component. Every `dekopon-model` transport — subscription inference, the device-flow exchange, and the OpenAI-compatible client — is built by one agent constructor that disables redirects and ambient proxies, so an exported `HTTPS_PROXY` cannot put a credential-bearing model request on a host nobody named to Dekopon.

The privileged component-linking library and Unix provider **process** are now present. The exact fetched standalone JSONPlaceholder v0.1.0 component demonstrates the boundary with a read-only GET capability and a distinct external-write POST capability; standalone tests and core loopback integration exercise both without public network access. `dekopon-http-host` consumes exact destinations, methods, call counts, byte limits, and deadlines beneath independent ceilings. It checks and pins DNS results, disables redirects and ambient proxies, and emits sanitized metadata. `dekopon-broker-host` adds a shared async Wasmtime engine, compiled components, fresh fuel/memory-bounded stores, wall-clock cancellation, typed WIT adaptation, and rejection tracking that guest code cannot mask. It consumes one non-cloneable `AuthorizedInvocation` at its public execution boundary; an optional destination-bound credential rides alongside that authorization (never inside it) and is injected by the native engine strictly after guest-header validation.

`dekopon-broker` supplies the next in-process boundary: a transport-independent `AuthenticatedContext`, a Cedar decision from `dekopon-policy`, the capability's owner-authored constraint set, process-local replay rejection, authorization construction, provider execution, redacted public evidence, and bounded in-memory plus append-only JSONL audit implementations. The two halves are separate on purpose: a policy edit can broaden who may act and can never widen a timeout, a destination, or a credential binding. The file log counts bounded newline-delimited records without decoding history and refuses unterminated tails. Appends are flushed, not fsynced; a failed or cancelled append poisons the handle. Restart creates an empty replay ledger, with no historical integrity check or crash-recovery guarantee. Constructing a context alone does not authenticate it. `dekopon-broker-protocol` owns the shared untrusted request/capability wire types without depending on privileged broker/host machinery and adds strict one-request-per-connection framing with hard byte/deadline ceilings and a client that verifies protected socket metadata plus the server peer UID. Its invocation payload deliberately omits principal, actor, policy, constraints, credentials, and authorization. `dekopon-brokerd` accepts that socket, derives peer UID from the connected stream, applies an exact owner-controlled UID-to-context mapping, and invokes the core. Peer mapping distinguishes gateway and broker UIDs; it is not process-level attestation. Legacy provider credentials resolve from a separate owner-only `credentialsPath` file into per-capability destination-bound values, selectable per acting agent. A separate `secretMapPath` binds public DRNs to physical sources and native sinks; descriptor validation is startup-local, while one selected source lookup follows dual authorization per invocation. Evidence records injection and audit records the symbolic name/DRN, never values or physical locators. Separately, `dekopon-brokerd provider sync` uses a bounded OCI distribution client to resolve new fully qualified exact tag or manifest-digest references and atomically activate a generated lock only after complete host validation. `sync --locked` materializes only locked layer digests; `list`, `verify`, and daemon startup are offline. A managed startup derives immutable blob paths from the lock and the host compares expected byte length, SHA-256, and provider ID against the same buffer and bounded description it compiles. This first slice intentionally does not claim publisher provenance, private-registry auth, SemVer selection, pruning, or image-staging integration. The complete authority boundary is described in [`dekopon-brokerd` contract](../crates/dekopon-brokerd/README.md#boundaries), and the provider lifecycle contract in [`../crates/dekopon-brokerd/README.md`](../crates/dekopon-brokerd/README.md#managed-provider-sets).

## Slack Agent continuation boundary

The Agent manifest subscribes to public/private channel message events, but the Slack transport
forwards no ambient event. An exact workspace/channel/root-thread/sender claim enters a bounded
in-memory LRU only after that message's fresh broker capability surface is non-empty. A later
unmentioned message must match all four coordinates, is freshly authorized again, and loses the
claim on refusal; another sender and another thread remain ambient. Restart drops every claim.

Only such an inherited message marks its reply optional. The shared prompt loop adds trusted
request-scoped guidance plus the payload-free `decline_chat_reply` tool. A decline before capability
work yields a typed suppressed completion, stores the sender's text as an unanswered in-memory turn,
cleans up activity, and performs no chat delivery or durable recording. If any capability invocation
already ran, the loop executes no calls from the decline turn and requires a concise visible report.
If no reporting turn remains, the gateway instead posts a fixed warning that work was attempted and
the audit must be checked before retrying. Explicit mentions and direct messages never receive the
decline tool.

## Storage execution boundary

A storage-enabled operation follows the ordinary authorization transition and additionally consumes
one non-cloneable `StorageGrant`. The grant binds the exact component interface and access mode;
wrong-interface and denied/quota/budget calls become sticky even when guest code catches the WIT
error. A base-then-generation lease order serializes one scope's authority pointer and invocation
access while distinct opaque namespaces can overlap; grant/begin run as tracked blocking work so
a lease wait cannot stall Tokio workers. Each authorized host write applies directly. Failed
responses, traps and cancellation do not roll back completed writes. Quota headroom and leases
remain held while started native jobs drain; the host makes no invocation-wide atomicity or
crash-recovery promise.

The exact standalone memory-chat release is staged under `/opt/dekopon/optional-providers`, not the default provider
scan. The chart mounts its retained provider-storage claim and copied operator-managed namespace key
only into the broker container. The gateway receives neither mount and only sends one typed record
proposal after an opaque transport-acceptance receipt.

## Resource evolution

`dekopon.dev/v1alpha1` rejects unknown authored fields to expose typos and ignored authority settings. Transport negotiation is out of scope for the local release. A future daemon API must version resources explicitly and document any field-preservation or compatibility rules before relaxing strict decoding.

The monorepo shares versions, CI, issues, and releases while crate boundaries are still changing together. Crates should move to separate repositories only when ownership or release cadence genuinely diverges.
