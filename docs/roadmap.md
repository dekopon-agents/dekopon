# Roadmap

Roadmap items describe sequencing, not shipped behavior or permission to bypass the invariants in [`design.md`](design.md) and [`security-model.md`](security-model.md). They are intentions rather than delivery commitments.

## 0.1 — local control and immediate provider tooling (implemented)

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

Version 0.3.0 is published as provenance-attested CLI archives and a Git tag covering 20 public crates. Those crates were never uploaded: crates.io publication is a separate manual dispatch that has not been run, so crates.io still holds the 17 packages of `0.2.0`. What has *not* changed is the checkpoint story: there is still no independently retained, signed, or remote anchor.

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
- Optional generated JSONL `memory-chat` provider: hidden post-acceptance record, visible on-demand
  recent/literal search, finite permanent dedup, and compaction hysteresis.
- Invocation-bound chat attestation plus owner-authored `chatScopes` and Cedar scope context; legacy
  operations reserve and omit all memory entry points.
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
path, and three recurring failure classes promoted to deny-by-default clippy lints.

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
- **Cedar context conditioned on provider input.** Deliberately absent: conditioning authorization on untrusted open JSON needs a settled schema treatment before any policy may depend on a caller-supplied value.
- **Actor kind (human versus service) in policy context.** The broker knows it; policy cannot currently read it. Cheap to add and easy to add wrongly, since it invites rules that look like identity checks but are transport facts.
- **`AuditEvent` field casing.** Variants rename to camelCase while their fields stay snake_case, so a record mixes `attested_subject` with `credentialInjected`. Fixing it breaks the audit chain format, which is worth doing only alongside another change that already does.

## Intended package namespace

`dekopon-model` is now present with tested OpenAI-compatible and ChatGPT/Codex transports plus model-account authentication. `dekopon-agent` is now present with the shared bounded prompt loop and session capability dispatch, consumed by both `dekopon-run` and `dekopond`. `dekopond` itself is now present as the unprivileged chat gateway. `dekopon-policy` is now present too, as the bounded Cedar adapter behind the broker's authorization decisions. `dekopon-webui` is now present as the tested GET-only operational view embedded in the broker service. The following remaining names are reserved for future meaningful crates. They are **not** present in the workspace and are not claimed as crates.io reservations or published packages:

- `dekopon-identity`
- `dekopon-context`
- `dekopon-memory`
- `dekopon-tribunal`
- `dekopon-mcp`
- `dekopon-observe`

A crate should be added only with meaningful, tested behavior needed by an implemented milestone. Tightly coupled crates remain in this monorepo and share one pre-1.0 release line. The gateway's conversation history needed none of these names: it is a bounded map inside the daemon, and `dekopon-memory` stays reserved for memory that outlives a process.

## Explicit non-goals for 0.1

Interactive TUI, daemon networking, shell-completion installation, provider credential access, operator-accessible provider host I/O, policy evaluation, durable evidence/audit, and local or external effect execution are intentionally absent from 0.1. Their accepted broker-mediated HTTP direction is documented in [`broker-http.md`](broker-http.md), but documentation does not make those paths current. Model-account lifecycle is exposed through `dekopon auth`; model inference and component loading remain confined to the explicitly experimental `dekopon-run` executable.
