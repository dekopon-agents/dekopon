# Roadmap

Roadmap items describe sequencing, not shipped behavior or permission to bypass the invariants in [`design.md`](design.md) and [`security-model.md`](security-model.md). They are intentions rather than delivery commitments.

## 0.1 — local control and immediate provider tooling (released)

- Strict `v1alpha1` agent, capability, and provider resources.
- Local YAML/JSON discovery and cross-reference validation.
- Deterministic get, describe, validate, and config-view commands.
- Proposal/authorization typestate and documented process boundary.
- Experimental Rust provider SDK and import-free Wasmtime component host.
- One-shot direct invocation, OpenAI-compatible and ChatGPT/Codex subscription prompt tools, isolated device login, timing reports, and Chrome trace export for read-only provider computation.

## 0.2 — privileged local broker foundation (released)

- Immutable buffered `dekopon:http@1.0.0` WIT package and statically compiled Rust guest facade.
- Caller-generated provider worlds plus a checked-in HTTP-importing component that the immediate runner rejects.
- Exact per-invocation HTTP authorization constraints beneath independent native ceilings.
- Statically linked HTTP engine with bounded buffers, DNS/IP and redirect controls, and sanitized evidence metadata.
- Asynchronous broker component-host library with one shared Wasmtime engine, compiled components, fresh bounded stores, Tokio host calls, and a single-use `AuthorizedInvocation` public execution boundary.
- Deny-by-default broker authorization, authenticated-context binding, replay rejection/restoration, digest evidence, and bounded metadata-only in-memory or owner-only durable verified audit chains.
- Strict versioned length-delimited broker messages and explicit `dekopon-run broker` client commands whose invocation payload cannot carry identity or authority.
- Unix-only `dekopon-brokerd` with owner-controlled strict configuration, private socket lifecycle, peer-UID context mapping, bounded connections/draining, provider execution, durable replay restoration, and an atomic owner-only checkpoint file that detects audit rollback relative to retained state.
- Mock-backed JSONPlaceholder post-read and separately classified external-write capabilities using exact broker HTTP grants.

Version 0.2 shipped an exact-match policy evaluator. It has since been replaced by Cedar; see milestone 3 below.

Version 0.2.0 is published as 17 public crates and provenance-attested CLI archives. The broker process is deployable for one local owner-UID trust domain and has an explicit unprivileged `dekopon-run` client; at that release the operator CLI and an agent daemon remained unintegrated, and it had no provider credential resolver.

## 0.3 — Cedar, credentials, identity, and the chat gateway (released)

- Cedar authorization in `dekopon-policy`: a schema generated from the deployment's declared world, strict startup validation, deny on any evaluation error, and the determining `policy_ids` plus a `policy_digest` in every audit record. It replaced the exact-match evaluator outright.
- Broker-owned destination-bound credentials in a separate stricter owner-only file, bound per capability constraint set with optional per-agent overrides, injected inside the native HTTP engine after guest headers were validated.
- Canonical external subjects, owner-controlled subject-to-principal mappings, per-peer attestor grants, and `via`-scoped rules that keep attested and direct authority disjoint.
- `dekopond`, the unprivileged chat gateway: Slack Socket Mode, Telegram long polling, and an owner-only development transport; first-match routing to catalog agents, including routes that match any channel the bot is summoned in; admission-bounded sessions; and attested on-behalf-of proposals.
- Bounded per-sender conversation history on `mode: persistent` routes, under a first-class per-transport conversation identity and a minted per-conversation prompt cache key.
- `dekopon-agent`, the shared bounded prompt loop and session capability dispatch consumed by both `dekopon-run` and `dekopond`, and `dekopon-run chat` for the gateway's development transport.
- The `examples/conditional-write` end-to-end walkthrough. The nineteen-capability GitHub provider and its own walkthrough ship from [`dekopon-provider-gh`](https://github.com/dekopon-agents/dekopon-provider-gh); the image fetches that component at a pinned tag.

Version 0.3.0 is published as provenance-attested CLI archives and a Git tag covering 20 public crates. Those crates were never uploaded and are being left that way: at the time, crates.io publication was a separate manual dispatch, and it was not run for `0.3.0`, so no crate carries that version. Since 0.9.0 (#121) every release tag *attempts* to publish the public crate set through crates.io trusted publishing in the order `.github/release-crates.txt` states — a run can still stop partway, as `v0.12.0`'s did — and the manual dispatch remains only to recover an interrupted tag publication; the root README's [crates.io section](../README.md#cratesio) is the current statement of what is published. What has *not* changed is the checkpoint story: there is still no independently retained, signed, or remote anchor.

## 0.4 — distribution: image, chart, and tap (released)

- A multi-architecture container image assembled from the release archives rather than compiled a second time, verified byte for byte against them before anything is pushed.
- A Helm chart running `dekopon-brokerd` and `dekopond` as one pod sharing the broker's `0600` Unix socket, versioned and tagged separately from the application.
- A Homebrew tap whose formula is regenerated from the archives each release actually published.
- `dekopon auth chatgpt export`, which prints an existing local ChatGPT subscription credential as a `v1` Secret manifest or as the credential document itself, so a containerized gateway can be seeded with a credential an interactive device flow cannot obtain in a pod.
- macOS on Intel dropped from the release matrix, leaving three archives.

Version 0.4.0 adds no crate and no privilege: the same 20 public crates, the same process boundary, the same deny-by-default broker. It is a packaging release.

## 0.5 — files in chat (released)

- Chat assets: an image or a document attached to a message becomes a numbered reference in the prompt, which a model opens on demand through a `fetch_chat_asset` tool rather than carrying on every turn. Slack and Telegram, images and the document types a model API accepts.
- Slack answers post in a Block Kit `markdown` block, so a model's CommonMark renders instead of arriving as literal punctuation.
- `dekopon-model` messages can carry content parts. A text message still serializes to exactly the bytes it did before, and the public `Serialize` became the redacted audit rendering rather than the wire shape.
- Providers declare their own command words, and those words cross the local broker protocol.
- The broker loads providers from a directory, and policy tolerates names no loaded provider declares.

Version 0.5.0 adds no crate and no process boundary, and it is the first release to move a documented authority line: the gateway now fetches the bytes of a file attached to a message it was already receiving. That is bounded by media type, per-attachment size enforced while streaming, per-session fetch count, and a per-conversation ceiling — never by policy, which the gateway still does not hold. [`security-model.md`](security-model.md) carries the argument.

## 0.6 — read-only operational web UI (released)

- `dekopon-webui` is a meaningful new crate embedded only in `dekopon-brokerd`: an explicitly bound, unauthenticated GET-only operational view of loaded provider manifests/interfaces, host-observed Wasmtime counters/ceilings, credential-free OTLP settings, and bounded informational agent/token reports from `dekopond`.
- Agent inventory and token reporting do not move orchestration into the broker. Reports omit content and authority, are accepted only from a mapped attestor, remain process-local, reset on restart, and never feed policy, constraints, credentials, execution, evidence, replay, or durable audit.

Version 0.6.0 adds the twenty-first public crate and an opt-in TCP listener, but no new effect authority. Omitting `--http-bind` leaves the listener absent; enabling it exposes deployment metadata to every client the selected network address can reach.

## 0.7 — credential-free agent self-inspection (released)

- Every authorized gateway session offers `inspect_agent_config`: a bounded model-facing view of its catalog identity, exact standing instructions, route/session and conversation limits, and the fresh sender-specific capabilities Cedar currently grants through that agent.
- The view structurally omits raw Cedar and policy identifiers, identity, execution constraints, endpoints, paths, credential references, credential names, and credential values. Denied and merely declared capabilities are absent.
- Calls are repeatable under the prompt loop's shared per-turn tool and model-step bounds. Inspection spends no capability budget, makes no broker invocation, grants nothing, and creates no durable broker audit record.

Version 0.7.0 adds no crate, process boundary, network path, credential access, or effect authority. It makes the prompt and effective grants deliberately visible to a sender already authorized to drive that agent.

## 0.8 — Discord gateway transport (released)

- `dekopond` connects to Discord Gateway v10 over an outbound WebSocket, handles DMs and explicit structured guild mentions, treats native threads as channel identities, and replies through pinned Discord REST without requiring a public endpoint or privileged Message Content intent.
- Gateway lifecycle handling covers heartbeat acknowledgements, dispatch sequence and Resume, Invalid Session, fatal close codes, session-start limits, deduplication, jittered reconnect backoff, and bounded REST rate-limit retry.
- Discord photos and files reuse the lazy chat-asset path. Signed CDN URLs are host-validated and fetched without the bot token; expired URLs refresh through the exact source message and attachment ID. Discord users map to canonical global `discord.<user id>` subjects.

Version 0.8.0 adds no crate, inbound listener, policy, provider credential, or effect authority. The gateway holds the Discord bot token needed to hear and answer messages, while the broker remains the only component that maps identity and authorizes effects. Version 0.8.1 raises the shared prompt loop's per-turn tool-call guard from four to ten so bounded multi-attachment turns can degrade through the existing four-fetch session limit instead of failing before any attachment opens.

## 0.9 — native chat activity and Slack Agent sessions (released)

- Authorized sessions can opt into transport-native activity: Slack Agent `processing`/`active`, Discord typing leases, and Telegram topic-aware chat actions. Activity begins only after the fresh broker session gate succeeds and never decides reply delivery.
- Slack's Agent experience is explicit rather than inferred from billing or API success. Permanent capability failures degrade to an opt-in fixed `:tangerine:` reaction and then no-op; a reaction is removed only after its add was confirmed.
- Slack Stop is authenticated and cooperative. It suppresses subsequent model turns, capability calls, stale answers, and history commits, but cannot roll back an already-running model request or provider effect.

Version 0.9.0 adds no crate, inbound listener, policy, provider credential, or effect authority. The gateway holds only the transport credentials it already needed, derives every activity target from authenticated envelopes, and treats every status failure as cosmetic.

## 0.10 — WhatsApp transport, image replies, provider storage, and a review hardening pass (released)

Shipped as one release in two threads. The transport:

- Strict environment-name-only app-secret, verification-token, and access-token configuration with
  explicit bind/callback, WABA, receiving phone-number, and Graph API version.
- Bounded plain-HTTP webhook behind external TLS termination; exact subscription verification and
  raw-body HMAC-SHA256 authentication before parsing.
- Batched signed text routing under canonical `whatsapp.<wa_id>` subjects and exact configured
  WABA/phone scope, with bounded process-local message-ID deduplication and acknowledgement before
  asynchronous session work.
- Pinned Graph messages endpoint, 4,096-character text bound, bounded responses, and no blind retry
  for outcome-unknown sends.

This adds an inbound listener and three gateway-held chat credentials, but no provider credential,
policy, authorization path, or effect authority. Media, templates, interactivity, reactions, status
processing, business management, embedded signup, webhook multiplexing, and TLS termination remain
out of scope.

### Generated image replies, provider storage, and durable on-demand chat memory

- Explicit route-scoped OpenAI Images generators: one bounded prompt/attempt/PNG per session,
  native Slack/Discord/Telegram uploads, and a byte-free local/text-history/durable-memory contract.
- Independent `dekopon:storage@0.1.0` JSONL and durable-files interfaces, feature-gated guest
  bindings, and a Wasmtime-independent secure native storage host.
- Exact storage interface/access authority, opaque keyed namespaces, non-reusing authority-bound
  generations, logical quotas, namespace leases, transactional commit/recovery, and bounded GC.
- Optional independently released JSONL `memory-chat` provider: hidden post-acceptance record, visible on-demand
  recent/literal search, finite permanent dedup, and compaction hysteresis.
- Invocation-bound chat attestation plus owner-authored `chatScopes` and Cedar scope context; legacy
  operations omit and refuse every capability and command word the owner routed to chat memory.
- Complete transport-acceptance receipts and exactly one no-retry record request after acceptance.
- Separate retained broker-only provider-storage PVC and operator-managed key; optional memory
  component path outside the default scan.
- Optional out-of-tree `turso-sql` provider: a pure-Rust SQLite-compatible engine (`turso_core`)
  compiled to `wasm32-unknown-unknown` from the `dekopon-agents/turso` fork, importing
  durable-files and nothing else. Indexed reads, aggregation, and schema are what it buys; write
  throughput is not, and full-text search is absent on this target.
- The 2026-08-20 gate that failed on forbidden JS/C/build paths tested the crates.io `turso`
  wrapper, whose SDK-kit dependencies are not reachable from the engine. Durable-files stays
  engine-neutral and still makes no SHM or multiprocess claim; a single-instance WAL engine needs
  neither.

Version 0.10.0 adds one bounded inbound listener — the WhatsApp webhook, HMAC-authenticated
before parsing and behind external TLS — and broker-owned provider storage behind explicit storage
grants. Identity mapping and effect authorization remain solely the broker's, and the gateway holds
only transport credentials. The same release lands a 145-finding deep-review hardening pass:
diagnosable failure causes end to end, single-pass authorization and audit serialization on the hot
path, and three recurring failure classes promoted to deny-by-default clippy lints. It adds two
public crates, `dekopon-storage-host` and `dekopon-provider-storage`, for twenty-three.

## 0.11 — bash-shaped shell and an interactive console (released)

Version 0.11.0 gives `dekopon-shell` the bash-script surface a script author expects — compound
commands as pipeline stages, `[[ ... ]]`, enforced `set -e`/`-u`/`-o pipefail`, `read`/`getopts`,
real parameter expansion, and two script-addressable streams — and adds `dekopon console`, an
interactive terminal view over a running broker. The `gh` shell builtin moves out of tree to
`dekopon-provider-gh`, joining `turso-sql` as a provider published and versioned separately from
this repository. It adds `dekopon-provider-sdk-testkit`, the in-process fake broker provider tests
run against, and `dekopon-tui`, for twenty-five. Version 0.11.1 moves the container image's
runtime base from Debian 12 to Debian 13 so it publishes at all: the CLI binary referenced two
symbols glibc's dynamic linker requires the runtime library to name even though both are
weak-probed and safely absent on an older one. No crate's source changed.

## 0.12 — structural scrub, process seam, provider manager, and secret sources (released)

Version 0.12.0 pairs a structural scrub with three foundations (below): thirty-two decided findings
from a whole-tree audit, one commit each. It bumps the local broker protocol to
`dekopon.dev/broker/v1alpha2`, where attestation is one optional request field instead of a
type-level axis multiplied across eleven request variants, thirteen client methods, and nine broker
entry points; keys durable chat memory by a typed `route:` on a constraint set rather than by
capability and provider names; collapses `ProviderManagerError` from seventy-three variants to ten;
gives both Wasmtime hosts one shared host layer; replaces twelve hand-rolled trusted-file checks
with one `dekopon-core` predicate; and moves the interactive console out of tree. A client and a
broker upgrade together.

### 0.12 — the console moved out of tree (released)

- `dekopon console` and the `dekopon-tui` crate ship from
  [dekopon-console](https://github.com/dekopon-agents/dekopon-console), the way the `gh` builtin
  moved to `dekopon-provider-gh` and `turso-sql` to its own repository. It consumes `dekopon-agent`
  and `dekopon-broker-protocol` from crates.io like any other unprivileged broker client. The move
  is `dekopon-agents/dekopon-console@adfe0560f90b45d0f5d4d93435915eec27258cd2`, which is also where
  scrub finding #32 landed: the help overlay's `o` and `r` keys got their `on_key` arms
  (`crates/dekopon-tui/src/run.rs:218-219`) and mouse capture was dropped, so nothing in this tree
  fixes them.
- Nothing loses authority: the console held a model credential and no policy, provider credential,
  or authorization. `dekopon` is a local catalog and model-account CLI again, and the operator CLI
  contacts no other process.
- `ratatui`, `crossterm`, and the five duplicate-version `deny.toml` exemptions they caused leave
  the control plane with it. So does the `dev.<surface>.<name>` subject service and the broker's
  `allowDevelopmentSubjects` opt-in: the console was the only surface that minted one, and no
  deployment set the field. See [`upgrading.md`](upgrading.md).

### 0.12 — Tokio process lifecycle foundation (released)

- `dekopon-process` provides one async operation trait and a one-run/one-node execution boundary.
  It records private payload-free trace IDs, preserves the typed operation result or Tokio task
  failure, and transfers the non-interruptible node to a supervisor that joins, records, and
  delivers it to a required abandonment observer if the outer caller is dropped while the owning
  runtime remains alive. Normal runner command execution keeps that runtime alive.
- `dekopon-run shell` is the normal consumer: provider loading and the unchanged synchronous
  interpreter run as one opaque non-interruptible blocking node.
- Structured scopes/ports, cooperative cancellation, deadlines, per-stage shell lowering,
  provider stdin, persistent workflows, and a runtime-component WIT remain later work requiring a
  real production consumer. The current lifecycle crate contains no broker, policy, credential,
  provider host, network, filesystem, retry, or audit machinery.

### 0.12 — locked provider-set foundation (released)

- `dekopon-brokerd provider sync`, `sync --locked`, `list`, and `verify` keep exact operator-authored
  OCI references, immutable manifest/component resolutions, and content-addressed installed bytes
  separate.
- Managed daemon configuration is mutually exclusive with legacy paths, performs no startup
  network access, and verifies the generated lock against the exact buffer and bounded provider
  description the host consumes.
- The first format deliberately supports exact tags and manifest digests only. SemVer requirements,
  update/install/remove/prune UX, private registries/custom roots, provenance policy, revocation,
  and container-staging adoption remain the ordered follow-ups rather than fields parsed but read by
  nothing.

### 0.12 — public DRNs and private secret sources (released)

- Canonical logical DRNs are inert typed proposal values, never provider JSON/WIT or bearer grants.
  Each use requires the normal capability decision, a separate exact Cedar `secret.use` decision,
  an owner-only use binding, and native host enforcement.
- The private map resolves one authorized invocation snapshot from secure files, Kubernetes
  projections/API objects, 1Password Connect, Vault KV v1/v2, AWS Secrets Manager/SSM, GCP Secret
  Manager, or Azure Key Vault. Existing implicit credentials remain compatible.
- Workload-identity bootstrap chains, Vault dynamic leases, custom source CAs, caching/stale
  fallback, output materialization, arbitrary secret interpolation, and transformed-reflection
  prevention remain follow-ups rather than parsed-but-unused configuration.

Version 0.12.0 adds `dekopon-process`, removes `dekopon-tui`, and leaves twenty-five public crates.

## Unreleased — skills, improvement suggestions, and session replay (implemented, not yet released)

- `spec.skills` mounts Agent Skills `SKILL.md` directories, read whole at catalog load under fixed
  bounds; a session lists them by name and description and reads a body or one resource on
  demand through `read_skill`. `dekopon-run prompt --skill` mounts them for one session.
- `suggest_improvement`, the tool an agent taps the glass with: at most three bounded structured
  notes per session, off everywhere by default, opt-in per gateway route
  (`improvementSuggestions: true`) or per runner session (`--suggestions`); a note is telemetry a
  person reads and changes nothing.
- `dekopon-run session list|show|replay` reads recorded sessions back from OpenObserve and replays
  one against a model, answering its scripts from the recording; only `--provider` makes a
  divergent script run, in the same read-only import-free direct mode as `prompt`. See
  [`improvement.md`](improvement.md).

This adds no crate, process boundary, inbound listener, provider credential access, or effect
authority. The one new network path is the runner's outbound OpenObserve query client, whose
`Authorization` header value is read from an environment variable named on the command line
rather than passed as an argument; `dekopon-agent` gains a `dekopon-config` dependency, and
`dekopon-run` still reaches no broker crate under the CI `cargo tree` gate.
[`CHANGELOG.md`](../CHANGELOG.md#unreleased) records the detail under `[Unreleased]`.

## Unreleased — provider command words as command-line facades (implemented, not yet released)

- `dekopon:provider@0.3.0` adds the `provider-cli` world and its `run-command` export: a provider's
  command word answers `--help` with its own page, a bad argv with its own usage error, reads the
  value piped into the word, or proposes a capability exactly as before. Both hosts serve it, look
  the export up by name so `0.1.0` and `0.2.0` components keep loading, and bound argv plus the
  piped value before a store exists. The SDK's opt-in `clap` feature builds the facade; the
  clap-free trait is the contract.
- The broker protocol carries the run as `runCommand`, answered with the guest's outcome intact;
  `resolveCommand` is answered for one release with rendered text degraded to a decline.
- Every command word runs as a `dekopon-process` node: the runner's direct leg serves the words
  its loaded components declare in a nested non-interruptible `direct-command` node, and the
  shared broker leg runs a word in a cancellable `broker-command` node that a gateway session's
  Stop abandons. That makes the broker leg the first cancellable consumer and supersedes the
  deferred fold-in of `dekopon-process` into `dekopon-run` below (#16): the crate now has three
  consumers in two binaries, one of them cancellable.
- Follow-ups accepted rather than dropped: real parent threading of a nested node's span under the
  node that ran it (today both record `parent.id` `root`); deadlines and ports on the process
  seam; per-stage lowering of a shell pipeline into nodes; a broker-side deadline for a command
  run independent of the guest's fuel and wall-clock bounds.

This adds no crate, process boundary, listener, credential path, or effect authority: a proposal is
authorized where it always was, rendered text authorizes nothing, and `dekopond` and `dekopon-run`
still reach no broker crate under the CI `cargo tree` gate; `dekopon-agent` and `dekopond` gain a
`dekopon-process` dependency. [`CHANGELOG.md`](../CHANGELOG.md#unreleased) records the detail under
`[Unreleased]`.

## Next milestones

1. Add independent checkpoint retention/export or signing so rollback of both local audit and checkpoint files is detectable outside the broker host.
2. ~~Add broker-owned credential resolution only after destination binding and redaction are independently tested.~~ Done: destination binding and redaction ship with independent engine-, broker-, and service-level tests.
3. ~~Introduce Cedar only after authorization inputs and explainability requirements are proven by the broker prototype.~~ Done: the exact-match evaluator proved which inputs a decision needs, and Cedar replaced it. `dekopon-policy` generates a schema from the deployment's declared world, validates the policy set in strict mode at startup, denies on any evaluation error, and reports the determining policy identifiers plus a policy-set digest in every audit record. Execution constraints stayed outside the policy language as owner-authored constraint sets, so a policy edit can broaden who may act and can never widen how far an action reaches.
4. Add identity, context, memory, observability, MCP interoperability, and multi-agent review only when each has tested user-facing behavior. Optional durable on-demand chat turns are now current; automatic replay, deletion/export UX, semantic/vector memory, shared namespaces, and task memory remain future. Broker-side identity is now current: canonical external subjects, owner-controlled subject-to-principal mappings, per-peer attestor grants, and `via`-scoped rules that keep attested and direct authority disjoint. The unprivileged agent daemon is now current too: `dekopond` connects to chat services, routes each authenticated message to a catalog agent, and submits attested proposals with no authority of its own ([`dekopond.md`](dekopond.md)). Conversation context is current too: a route set to `mode: persistent` keeps a bounded per-sender history in gateway memory, compacted to question-and-answer pairs, dropped on an idle timeout, an LRU ceiling, or a changed capability grant, with the contract in [`dekopond.md`](dekopond.md#conversations) and the trust surface it accepts in [`security-model.md`](security-model.md#conversation-memory-as-a-trust-surface). `oneShot` is the default, so a message on a route that did not ask for memory is still an independent session. Broader memory that automatically replays, shares across agents, stores tasks/facts, or provides
deletion/export remains future. The current durable store is explicit per-conversation retrieval
only. A dedicated gateway UID — the deployment in which `via` scoping is real isolation rather
than attribution — also remains future.

## Follow-ups accepted during the gateway/identity/Cedar work

Each of these was raised, deliberately scoped out, and accepted as a follow-up rather than dropped. None was committed direction in the design sense when it was raised; a struck entry has since had a design pass of its own, which is what promoted it.

- **A dedicated gateway UID and a 0660 socket transport.** The one change that turns `via` and namespace scoping from attribution into real isolation. It widens who may connect to the privileged socket, so it needs its own security review rather than a permission-bit edit.
- ~~**WhatsApp as a transport.** Unlike Socket Mode and long polling, it requires a public webhook endpoint — an inbound HTTP surface on the unprivileged daemon, with signature verification and replay handling of its own.~~ Done for ordinary inbound/outbound text: the callback is exact-path, bounded, raw-body-HMAC authenticated, and process-locally deduplicated; templates, media, and durable exactly-once remain explicit non-goals.
- **Per-principal credential overrides for one capability.** Half of this landed as `credentialByAgent`: a constraint set now binds a default credential plus per-agent overrides, which is what "one token per team, channel, or organization" needs, because a route already binds a transport and a channel match to an agent. What remains open is the principal axis — "approve as the person who asked" — which is a different trade: one entry per human in a file that otherwise declares capabilities and agents, and a per-person token to manage for each.
- ~~**Conversation memory and multi-turn threads in `dekopond`.** Each message is an independent session with no history. Memory is a new trust surface — text that persists across sessions and is replayed into a prompt — and belongs behind its own design pass.~~ Done: the design pass this item asked for is the [conversation contract](dekopond.md#conversations) and the [trust surface](security-model.md#conversation-memory-as-a-trust-surface) it accepts, and `dekopond` now implements it. History lives in gateway memory, is keyed on the transport, the conversation identity, and the sender's canonical subject, is compacted to question-and-answer pairs inside a sliding window, and is dropped on an idle timeout, an LRU ceiling, or a changed capability grant. Authorization is never cached; what persistence widens is an injected instruction's dwell time, which is why the security model states it rather than the roadmap.
- **`dekopon policy explain` and `auth can-i` operator commands.** The broker already computes the determining `policy_ids` for every decision; the missing half is an operator path to ask the question without making an effect happen. It is also the first CLI-to-broker integration and inherits that whole boundary discussion.
- **Cedar context conditioned on arbitrary provider input.** Deliberately absent: untrusted open JSON still has no settled schema. The public DRN feature is the narrow exception that proves the rule — one strongly typed top-level resource goes through a separate `secret.use` action and can never widen its owner binding.
- **Actor kind (human versus service) in policy context.** The broker knows it; policy cannot currently read it. Cheap to add and easy to add wrongly, since it invites rules that look like identity checks but are transport facts.
- **`AuditEvent` field casing.** Variants rename to camelCase while their fields stay snake_case, so a record mixes `attested_subject` with `credentialInjected`. Fixing it breaks the audit chain format, which is worth doing only alongside another change that already does.

## Deferred from the 2026-08-27 scrub

The whole-tree audit behind 0.12.0 decided thirty-five findings: the thirty-two executed ones are recorded in [`CHANGELOG.md`](../CHANGELOG.md#0120---2026-08-29)'s 0.12.0 entries, one bullet per landed finding citing its checklist number (the test-only #17, #18, #19, and #35 have no user-visible change and no entry, and the chart wiring of #25 sits under `dekopon-chart-0.3.0`), and these three were decided `DEFER` — revisit in the next scrub — rather than dropped. Each carries the audit's finding and suggested fix; paths are current, the audit's line numbers are not repeated.

- **Secret-source backends (#15, PREMATURE_GENERALIZATION).** `SecretSource` in `crates/dekopon-brokerd/src/secrets.rs` ships ten kinds — `secureFile`, `kubernetesProjection`, `kubernetesApi`, `onePasswordConnect`, `vaultKv1`, `vaultKv2`, `awsSecretsManager`, `awsSsmParameter`, `gcpSecretManager`, and `azureKeyVault` — eight of them hand-written vendor HTTP clients exercised only against in-process mock responses, and the AWS pair hand-rolls SigV4 with no known-answer signature test. No example or chart value sets `secretMapPath`, no `dekopon-brokerd` test authorizes through `secretUse` with `allowQuery: true` or `maxInjections` above one, and the deployment the audit read for context still used `credentialsPath`, including for 1Password. Suggested fix: keep `secureFile` and `kubernetesProjection`; delete the eight remote adapters, `read_aws_credentials`, the SigV4, `hmac`, and `crc32c` helpers, and the `time` and `hmac` dependencies they pull into `dekopon-brokerd`, keeping the `SecretSource` enum shape so a backend returns as one variant; add one end-to-end `dekopon-brokerd` test authorizing through `secretUse` with `allowQuery: true` and `maxInjections: 2`; and until a deployment sets `secretMapPath`, mark the feature **Exploration** in [`design.md`](design.md), or wire one live token through a `secureFile` entry and keep it Current.
- **`dekopon-process` into `dekopon-run` (#16, CRATE_FACTORIZATION).** A public crate of 309 source and 348 test lines whose `Process` trait has one implementation, its own `ProcessFn`, and one call site, `evaluate_shell` in `crates/dekopon-run/src/lib.rs`, under the kind string `"legacy-shell"`. Its abandonment-observer machinery — the supervisor's nested spawn, `OutcomeEnvelope`, and three of its six tests — exists for a dropped future the only caller cannot produce: nothing above it selects, times out, or aborts. Sibling handoff sites in `dekopond` still call `spawn_blocking` directly, and [`design.md`](design.md)'s "no empty future crates" rule and the package-namespace rule below both refuse a crate without a consuming milestone. Suggested fix: move `lib.rs` to `crates/dekopon-run/src/process.rs` as a private module; delete the crate from `[workspace.members]`, `[workspace.dependencies]`, `.github/release-crates.txt`, and its design and roadmap paragraphs; drop the `Process` trait, `ProcessFn`, `on_unobserved`, `OutcomeEnvelope`, and the three drop-path tests; rename `"legacy-shell"` to `"shell"`. Preserve the `process.run` and `process.node` spans, which `dekopon-run`'s trace tests assert and [`observability.md`](observability.md) names, and `ProcessOutcome::TaskFailed` carrying the raw `JoinError`. The crate is on the crates.io publication plan, so any version of it already published needs a yank or a deprecation pointing at `dekopon-run` when it goes. Superseded, unreleased: the crate now has three consumers in two binaries — the runner's `legacy-shell` and `direct-command` nodes and the agent layer's cancellable `broker-command` node, which the gateway's Stop drives — so the fold-in no longer applies; the unreleased command-word section above records it.
- **WhatsApp and Telegram in or out of tree (#26, SIDE_QUEST).** The deployment the audit read for context ran Slack and Discord only. WhatsApp (`crates/dekopond/src/transport/whatsapp.rs`) is the gateway's only inbound listener, its only HMAC path, and three gateway-held credentials, described in eleven Markdown documents in this tree and served by a chart `ClusterIP` Service that `gateway.service.enabled` leaves off by default. Telegram (`transport/telegram.rs`) has no example directory at all — `telegramLongPoll` appears only in [`dekopond.md`](dekopond.md), the gateway's configuration type, and unit tests — yet `ChatTransportKind::Telegram`, `DeliveryIdentity::Telegram`, and `SubjectService::Telegram` reach the trusted `dekopon-broker-protocol` and `dekopon-core` crates. Both widen the chat-scope authorization surface every change must carry. Suggested fix: decide both together — either `dekopond` grows a transport seam that lives out of tree, or delete both (`whatsapp.rs`, `telegram.rs`, `examples/whatsapp/`, the chart `gateway.service` block, the `ChatTransportKind` and `DeliveryIdentity` variants, and the broker's transport-to-subject match arms). Preserve: `SubjectService` must keep parsing `whatsapp.<id>`, `tel.<n>`, and `telegram.<id>` subjects so existing audit chains stay readable.

## Intended package namespace

`dekopon-process` is now present with a tested one-run/one-node Tokio lifecycle seam consumed by `dekopon-run` and, through `dekopon-agent`'s broker leg, by `dekopond`. `dekopon-model` is now present with tested OpenAI-compatible and ChatGPT/Codex transports plus model-account authentication. `dekopon-agent` is now present with the shared bounded prompt loop and session capability dispatch, consumed by both `dekopon-run` and `dekopond`. `dekopond` itself is now present as the unprivileged chat gateway. `dekopon-policy` is now present too, as the bounded Cedar adapter behind the broker's authorization decisions. `dekopon-webui` is now present as the tested GET-only operational view embedded in the broker service. The following remaining names are reserved for future meaningful crates. They are **not** present in the workspace and are not claimed as crates.io reservations or published packages:

- `dekopon-identity`
- `dekopon-context`
- `dekopon-memory`
- `dekopon-tribunal`
- `dekopon-mcp`
- `dekopon-observe`

A crate should be added only with meaningful, tested behavior needed by an implemented milestone. Tightly coupled crates remain in this monorepo and share one pre-1.0 release line. The gateway's conversation history needed none of these names: it is a bounded map inside the daemon, and `dekopon-memory` stays reserved for memory that outlives a process.

## Explicit non-goals for 0.1

Daemon networking, shell-completion installation, provider credential access, operator-accessible provider host I/O, policy evaluation, durable evidence/audit, and local or external effect execution are intentionally absent from 0.1. An interactive TUI was on that list; 0.11.0 built one and this tree no longer holds it, because it moved to [dekopon-console](https://github.com/dekopon-agents/dekopon-console) the way the `gh` provider moved to `dekopon-provider-gh`. It is an unprivileged broker client holding a model credential, so it acquired none of the operator-accessible provider or policy paths this sentence still rules out, and taking it out of tree acquires nothing either. Their accepted broker-mediated HTTP direction is documented in [`broker-http.md`](broker-http.md), but documentation does not make those paths current. Model-account lifecycle is exposed through `dekopon auth`; the operator CLI itself performs no model inference and loads no component. Inference lives in the explicitly experimental `dekopon-run` and in `dekopond`, and component loading in `dekopon-run` (import-free, read-only) and `dekopon-brokerd`.
