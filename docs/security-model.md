# Security model

This document expands the security constraints introduced in [`design.md`](design.md). Read it before changing capabilities, identity, policy, credentials, providers, evidence, audit behavior, or external effects.

## Foundational invariant

> A model may propose an invocation, but only the broker may turn it into an authorized invocation.

A capability name in an agent spec permits the agent to propose that operation. It does not grant process authority, credentials, or permission to call a provider directly.

## Security-relevant stages

1. **Model proposal** — untrusted model output names a capability and supplies untrusted arguments in a `ProposedInvocation`.
2. **Authorization decision** — the privileged broker authenticates the transport, derives the actor/workload from trusted mapping, evaluates policy and current context, then either denies the proposal or creates a constrained `AuthorizedInvocation` inside its execution boundary.
3. **External effect** — the broker consumes that authorization state while a narrow provider executes only the authorized capability using broker-held credentials and enforced constraints.
4. **Evidence** — policy decisions and provider execution produce digests or bounded records that support later verification.
5. **Audit record** — the broker links proposal, trusted identity, policy revision, authorization receipt, effect outcome, and evidence under an invocation and trace ID.

The local daemon-to-broker request carries only a proposal over an authenticated Unix connection, not identity claims or an `AuthorizedInvocation` for the broker to trust. The broker does not return transferable authorization to `dekopond`; a serialized authorization representation is inert audit/evidence data rather than a bearer grant.

Rust's private, non-cloneable `AuthorizedInvocation` fields and intentional absence of deserialization make accidental in-process fabrication or reuse harder. `AuthorizationGate::new` is public so a broker adapter can own the transition; constructing that handle does not authenticate a caller or evaluate policy. This is defense in depth only. The real authority boundary depends on separate processes, authenticated and replay-resistant requests, policy enforcement, authorization bound to execution, isolated credentials, provider sandboxing, and durable audit integrity.

## Trust boundaries

Trusted inputs are expected to include:

- principal and workload identity derived from authenticated transport plus owner-controlled mapping;
- broker configuration installed by an authorized operator;
- the owner-only Cedar policy file, read under the configuration's own hygiene rules (server-owned, single-link, not group/world writable, no symlink following, byte-capped) — it is authorization input in exactly the sense the configuration is;
- owner-authored execution constraint sets, validated at startup against loaded provider manifests, component-host ceilings, and the credential store;
- provider directories held to the same standard as every other trusted input the broker reads: owned by the expected UID and not group- or world-writable, because anyone who can write such a directory can add a component the broker compiles and runs. Every file a scan yields is then validated individually exactly as a directly-named one is, and there is no implicit search path — a directory the broker loads code from is named in its owner-only configuration or nowhere;
- a `strict` startup posture choosing whether configuration that *cannot apply* refuses startup or is reported and ignored. It governs complaint, never enforcement: a capability no loaded provider routes is denied `unconstrained-capability` before Cedar is consulted in either mode, and a policy naming one is registered as a schema-only phantom that no constraint set can bind and no route can reach. An undeclared *principal* stays fatal in both modes, because principals come from owner-authored configuration rather than from a loaded component;
- broker-generated authorization receipts and audit sequencing;
- secrets obtained by the broker from an approved secret store.

Explicitly untrusted inputs include:

- model output, reasoning, tool names, and tool arguments;
- repository files, pull-request text, issues, comments, diffs, and fetched web content;
- provider responses until validated and bounded;
- identity claims embedded inside model text or repository content;
- local config supplied from an untrusted checkout.

A model or repository document cannot self-assert a trusted `Actor`. For the local broker, the connected peer UID and strict owner-controlled mapping own identity attribution; invocation payloads have no identity fields.

Attested proposals are the one sanctioned indirection, and they are deliberately narrow. A peer whose owner-configured identity carries an `attestor` grant may attach a typed `SubjectAttestation` naming a canonical external subject (`slack.t0123abc.u9xyz`, `tel.16034700182`) alongside its proposal. The subject is transport routing metadata, not a principal and not authority: the broker — never the peer — resolves it through owner-controlled `identityMappings`, and an unmapped subject resolves to nothing rather than minting a principal on demand. The grant bounds which namespaces a peer may speak for, matched on segment boundaries so `slack.t0123abc` cannot reach workspace `t0123abcx`. A refused attestation is an audited denial under the peer's own principal with a stable reason (`attestation-denied`, `unmapped-subject`), never a silent error, so a compromised or misconfigured gateway leaves a decision trail. The resulting context is bound to a `via` naming the attestor, and policy sees it as `context.via`: a policy that requires a specific attestor can never authorize a direct peer, and one written `unless { context has via }` can never authorize an attested proposal. That is what stops adding a gateway from widening any grant that already existed. Driving an agent at all is a separate `agent.prompt` statement, so a mapped subject with no such grant is refused (`agent-denied`) before any capability is considered.

The trust model this accepts should be stated plainly. In the current single-UID deployment an attestor grant adds no authority beyond what any process under the owner's UID already has, because that UID is one trust domain and every process in it can already act as the configured peer. What the mechanism buys today is attribution and blast-radius shape: subject-level audit, deny-by-default policy conditioned on `via`, and configuration that fails closed on undeclared principals or duplicate subjects. Namespace scoping and `via` become real isolation only when the gateway runs under its own UID with its own peer identity, and that deployment — along with the socket permissions it needs — remains committed direction requiring its own review.

## Capability and effect rules

- Capabilities are narrow and name one effect class.
- External writes require an explicit capability; read access never implies write access.
- Provider permissions should be least privilege and independently enforced by provider credentials.
- Authorization constraints bind timeout, output size, exact HTTP destinations/methods, call counts, and byte ceilings.
- Retries account for declared idempotency and use provider-enforced idempotency keys where available.
- Credential values do not appear in agent prompts, authored catalogs, invocation evidence, or normal logs. A credential's symbolic *name* is owner-authored configuration rather than secret material, and the broker records it in audit so an effect can be attributed to the authority that carried it.
- A component import declares a required host interface; it never grants that interface or any transitive authority.
- Broker HTTP authorization binds exact destinations, methods, host-call counts, byte limits, and deadlines to one invocation.

The example reviewer has `github.pull-request.read` and the explicit external-write `github.pull-request.comment`. It does not have, and the example does not declare, `github.pull-request.approve`.

## Current operator and immediate-runner posture

The `dekopon` catalog commands read operator-selected YAML or JSON, reject unknown fields, validate identifiers and references, and render declarations without network access. The isolated `dekopon auth` namespace is the only current exception: it manages model-account login against fixed authentication hosts. Within that namespace, `dekopon auth chatgpt export` is the single command in Dekopon that writes credential material in the clear; it is documented, gated behind a required `--expose-credential`, refused when standard output is a terminal, and warns on standard error that the exported copy is invalidated by the next refresh. Its output form is chosen explicitly, it fails rather than emitting a partial document, and it makes no network request. See [`chatgpt-credential.md`](chatgpt-credential.md). The CLI performs no model inference, provider credential resolution, authorization decisions, or external effects. Provider readiness in local config is descriptive data, not a verified connection.

The separate experimental `dekopon-run` path can contact an operator-selected OpenAI-compatible model endpoint or OpenAI's fixed ChatGPT/Codex subscription endpoints and execute read-only Wasm component functions. Its provider boundary is deliberately narrower than a real integration:

- provider manifests are strictly decoded and may declare only `read-only` capabilities;
- duplicate provider and capability IDs are rejected before model interaction;
- model-selected function names map only to the offered capability registry and arguments must be JSON objects;
- capability schemas constrain model-facing tool declarations but are not generally enforced by the host, so providers must validate operation-specific input;
- every description and invocation uses a fresh Wasmtime store with memory, fuel, wall-clock, input, and output limits; component calls are currently serialized;
- the component linker exposes no WASI or custom imports, so guests receive no filesystem, network, clock, random, environment, credential, or external-read authority;
- an optional model bearer token is read from a named environment variable and sent only to the selected compatible endpoint;
- `dekopon auth chatgpt` uses OpenAI's Codex device flow and stores refreshable credentials in a Dekopon-owned file (`0600` on Unix); the shared model client fixes authentication and inference hosts to `auth.openai.com` and `chatgpt.com` and never imports credentials from pi, OpenClaw, or Codex;
- the ChatGPT refresh token rotates on every refresh, and the refreshed record must be written back or the model turn fails, so a credential exported for a secret store is a seed for one deployment rather than a backup, and a read-only credential mount cannot work;
- model credentials and opaque encrypted reasoning replay data are not exposed to components, output, or telemetry fields;
- optional runner OTLP export sends generated performance spans and stable lifecycle events to an operator-selected endpoint, but omits prompts, model responses, model-authored script text and its output, provider input/output, credentials, broker socket paths, and raw errors;
- immediate success output is raw untrusted JSON, not broker evidence, an authorization receipt, or an `InvocationResult`;
- immediate tool calls are not `AuthorizedInvocation` values and cannot be used for local or external writes.

Chrome and OTLP trace/log fields omit prompts, model responses, component input/output, bearer tokens, and raw untrusted errors. The operator-selected telemetry endpoint still learns execution metadata such as service/model/provider/capability identifiers, timings, outcomes, and source locations. OTLP lifecycle logs are operational audit data, not authorized invocation evidence or a substitute for the broker's durable hash-linked log. Final text and machine-readable outputs remain untrusted data. Terminal table cells in the catalog CLI continue to remove control characters.

## Current gateway posture

`dekopond` is the unprivileged process on the other side of the attestation boundary described above. Its posture is the inverse of the broker's: it holds the credentials needed to *hear* a request and to *ask a model*, and none of the credentials needed to *do* anything.

- **What terminates in the daemon.** Chat bot credentials (Slack app-level and bot tokens, Telegram bot tokens) and model credentials (an OpenAI-compatible API key, or Dekopon's own ChatGPT device-flow credential file). All of them are named in configuration as *environment variable names*, never values, and load into `Redacted` wrappers; a missing one is a startup failure naming the variable and never its value. A name that is not a valid variable name is rejected at startup, so a token pasted into that field is a refusal rather than a plaintext secret in a config file.
- **What never enters it.** Provider credentials, policy, and authorization state. Every external effect is an attested proposal to `dekopon-brokerd`; the daemon supplies the sender's canonical subject and the agent name and nothing else, and the broker maps, decides, resolves, and executes.
- **What is untrusted.** Message text end to end, bounded to 16 KiB before it reaches a model and 8 KiB on the way back out. The names and media types of files attached to a chat message are the same untrusted text, and the 16 KiB bound covers the message and the reference note naming them together. File *contents* are untrusted in the same way and reach a model only on demand: each attachment is named in the prompt as `Chat Asset #N`, and the gateway fetches the bytes only when the model calls `fetch_chat_asset` with that number. This is the one place the gateway reads something a sender supplied out of band, and it is deliberate — an attachment is part of the message that carried it, and Slack delivers it by reference rather than by value. Resolving that reference uses the bot token the daemon already holds to hear a request; it grants no policy, no provider credential, and no way to write anything. What bounds it is arithmetic rather than authority: an allowlist of media types a model can actually be shown, 8 MiB per attachment enforced while the response streams rather than after it, four fetches per session, and a per-conversation ceiling on how many attachments stay addressable. Bytes are dropped with the request they joined, so nothing retains them. The agent's `instructions` from the catalog are untrusted model text by the same definition: they shape how an agent answers and can never assert identity, name a principal, or widen a capability. Broker policy never reads that field.
- **Authorization is a gate, not a filter.** A session calls `capabilitiesFor(subject, agent)` before any model call, and the broker answers it only if policy permits `agent.prompt` for that principal and agent. An empty answer, or a refusal, ends the session with a fixed sentence and costs nothing. Failures also answer one fixed line — a `PromptError` can carry model, provider, or transport text, and none of it reaches chat.
- **Self-inspection is deliberately narrower than configuration access.** Every authorized gateway session may call `inspect_agent_config`. Its typed result contains the catalog agent's identifier, description, model class and exact standing instructions; route limits and conversation mode; and only the capability metadata from that sender's fresh `capabilitiesFor` result. It includes no raw Cedar source, policy IDs or digest, principal, subject, transport/channel identifier, execution constraint, model or broker endpoint/path, credential reference, symbolic credential name, or credential value. The gateway never receives provider credentials or raw policy in the first place, and the view constructor has no field for the chat/model credentials it does hold. The bounded view can be materialized once per model session. Inspection consumes no capability budget, makes no broker invocation, grants nothing, and produces no durable broker audit record. Standing instructions are therefore authorized-user-visible rather than confidential; putting a credential in a prompt would already disclose it to the model and remains invalid configuration hygiene.
- **The development transport is the one deliberate exception** to "identity comes from authenticated transport": it trusts its local caller to declare a subject. It grants nothing by doing so, because the claim still has to pass the broker's attestor grant and identity mapping, and its `0600` socket under an owner-only parent keeps it inside the UID trust domain the broker socket already lives in. It is a development tool, not a production transport.

The single-UID caveat above applies unchanged: `dekopond` and `dekopon-brokerd` currently run as the same user, so the attestor grant buys attribution and blast-radius shape rather than isolation until the gateway has its own UID. See [`dekopond.md`](dekopond.md) for the complete contract.

### Informational status reporting and the web UI

`dekopond` additionally sends two bounded reports over the authenticated Unix protocol: a normalized catalog inventory (agent description, enabled/model-class flags, capability/provider identifiers, and provider permission declarations) and provider-reported model-token deltas. It never sends agent instructions, prompts, answers, subjects, principals, model/provider credentials, policy, constraints, or authorization. Only a mapped peer with an attestor grant may publish them. The broker retains the latest inventory and saturating token totals in memory, resets them on restart, and never consults either for Cedar, constraint or credential selection, routing, execution, evidence, replay, or durable audit. A compromised gateway can make this *informational display* lie; it cannot turn the lie into authority.

`dekopon-brokerd --http-bind <ADDRESS>` explicitly enables a GET/HEAD-only TCP surface from `dekopon-webui`; no TCP listener exists when the flag is absent. `/` redirects to `/ui`, there is no login, and no HTTP route mutates broker state. That does not make its contents public-safe. The pages disclose agent names and declared permissions, provider descriptions and input schemas, local component paths and digests, Wasmtime limits/activity, and the credential-free OTLP endpoint/service configuration. Header and resource-attribute values are withheld, provider and OTLP credentials never enter UI state, and all rendered authored/component strings are HTML-escaped under a closed content-security policy. The operator-selected network around the bind address is the access boundary; `0.0.0.0:8080` deliberately exposes this deployment metadata on every interface.

Persistent conversations add one more thing that terminates in this daemon — chat text that outlives the message carrying it — and the next section states that surface.

## Conversation memory as a trust surface

**Status: current.** A route set to `mode: persistent` implements the [Conversations](dekopond.md#conversations) contract: a per-sender history, bounded by a sliding window, an idle timeout, and a process-wide ceiling, held in the gateway process's memory and replayed into the next prompt. `oneShot` is the default, so a route that does not ask for memory still runs each message as an independent session that starts from an empty prompt.

### Containment is unchanged

The broker authorizes every invocation. A persistent conversation opens a fresh attested leg per message exactly as a one-shot session does — the same `capabilitiesFor` call, the same policy evaluation against the same `via`-scoped rules, the same audit record. No grant is cached, no decision is carried forward, and replayed history reaches the model as prompt text and never reaches the broker as authorization input.

So persistence widens no authority. Everything a model can do with a remembered conversation, it can already do with a single message: propose. The invariant that a proposal is not authority is untouched, and this design does not ask for an exception to it.

### What persistence widens is duration

Prompt injection is not defended against, and this is the change that matters to it.

Today an instruction embedded in a pull-request body, an issue comment, or a fetched page reaches the model, and it dies with the message that read it. The next message starts from an empty prompt, so an injection gets exactly one turn and its blast radius is one session's proposals.

With history it stays. The injected text — or the model's own answer restating it — sits in the prompt for the rest of that conversation, up to `maxTurns`, up to `maxBytes`, up to the idle timeout, and every subsequent turn in that conversation is evaluated with it still present. A person who asks three follow-ups after the poisoned message is asking all three with the injection in scope. The exposure is the same in kind and longer in dwell time, and that is worth stating here rather than leaving to be discovered when a conversation behaves oddly on its fourth turn.

The mitigations below shorten the dwell time. None of them detects the injection, because nothing in this project does. A route that keeps the `oneShot` default keeps today's one-turn dwell time exactly, which is why the mode is opt-in rather than a new default.

### The second-order case: history outliving its grant

Tool output a session fetched under a broad grant is in the history. If the owner then narrows what that subject may reach, the text is still in the prompt even though the capability that produced it is gone — a quiet way for a revocation to be less complete than the owner believes.

The mechanism that closes it: the granted capability set is stored with the conversation and compared against the fresh leg's grant on every message. Any difference drops the history and starts a new conversation; an empty grant removes the entry outright, which is the same refusal an unauthorized sender already gets, applied to what was remembered as well as to what may be done. It costs a cache miss on the first message after any policy change. That is the correct price: a narrowed grant is precisely the moment replaying old output is wrong, and paying for one extra round trip to be sure of it is a good trade.

Two honest limits on that mechanism. It compares capability *identifiers*, so a policy edit that keeps the same capability list while tightening its owner-authored constraint set — a narrower allowed host, a smaller output ceiling, a different injected credential — produces an identical grant set and does not drop the history; text fetched under the older constraints survives until the window or the idle timeout removes it. And invalidation removes text from a future prompt, never from anywhere it was already shown: the answer that quoted it is in a chat transcript the daemon does not own.

### The mitigations, as a set

None of these is sufficient alone, and the design depends on all of them:

- **Per-sender keying.** The conversation key includes the sender's canonical subject, so history is per-sender and never shared across the people in a channel. One person's exchange cannot enter another person's prompt, which also means a channel member cannot make the bot recite what it told a colleague.
- **Idle timeout.** An untouched conversation is evicted, 15 minutes by default. It bounds how long an injection or a stale tool result can persist without anyone continuing the conversation that produced it. The check is lazy — the eviction happens on the next lookup rather than on a timer — so an idle entry can outlive its timeout in memory until something asks for it or the ceiling displaces it. What it can never do is reach a prompt.
- **The window.** `maxTurns` and `maxBytes` bound what is replayed regardless of how long the conversation lives, so a long-running conversation does not accumulate an unbounded prompt and old turns fall out of scope on their own.
- **Compaction.** A stored turn is `(the user's message, the final answer)`; intermediate tool calls, model-authored scripts, and their output are dropped. Materially less untrusted repository and provider text is replayed than the session actually read, and the replayed prompt cannot grow with the size of a tool result — one script's output alone can reach 256 KiB.
- **In memory only.** History lives in the gateway process, is never written to disk by the daemon, is never sent to the broker, and dies with the process. There is no file to exfiltrate, no backup to age out of a retention policy, and nothing to recover after a restart.
- **Grant-set invalidation.** Described above: the granted capability set travels with the conversation, and a change drops it.

### Where the text deliberately does not go

Not into the broker. `dekopon-brokerd` holds provider credentials and a metadata-only hash-linked audit chain in which a provider's output survives only as a digest, and its records deliberately exclude inputs, outputs, paths, queries, headers, and bodies. Putting conversation text in that process would place the most sensitive content in the system inside the most privileged one, next to a record built specifically not to contain it, and it would turn a chain that proves what was *authorized* into a store of what was *said*. The gateway already reads the message and writes the answer, so keeping the history there adds no new reader.

Not into telemetry either. `conversation.turns` and `conversation.bytes` are a count and a byte total; the history itself follows the existing payload gate and appears only in `agent.model.prompt` under `telemetryPayloads: true`, where it makes that event both larger and older than it was. Enabling payloads on a persistent route declares the telemetry sink in scope for a conversation rather than for a message.

And not into the [prompt cache key](dekopond.md#the-prompt-cache-key). Every model request declares one so the provider's prefix cache has somewhere to route the requests that share a prefix, and it is minted from entropy rather than derived from the sender. A canonical subject can be a phone number, and a hash of one is a stable pseudonym; either would tell a model provider that two conversations months apart belong to one person, which is the linkage the metadata-only telemetry default exists to withhold and a worse thing to hand a third party than to hand your own sink. The minted key rotates whenever the conversation it names is evicted and whenever the process restarts, so it never accumulates into a durable identifier for anybody, and it is a routing hint that confers nothing: a request carrying a key is authorized exactly as one without it, which is to say by the broker, per message.

### What this does not fix

"In memory only" is a durability property and not an isolation one. In the current single-UID deployment any process running as the owner can read the gateway's memory, exactly as it can already act as the configured gateway peer. Operating-system paging and core dumps are outside what the daemon controls. And the development transport's declared subject selects a history as well as a subject, so a local caller inside that UID can address a conversation a Slack sender created and have it replayed into its own prompt. None of this is new authority; all of it is the single-UID caveat applying to a new kind of content, and a dedicated gateway UID is what turns it into a real boundary.

## Current privileged broker foundation

`dekopon-broker-host` is the privileged component library used only by the separately deployed `dekopon-brokerd` process. It links only `dekopon:http@1.0.0`, consumes one non-cloneable `AuthorizedInvocation` at its public invocation boundary, and runs each description or invocation in a fresh memory-, fuel-, input-, output-, and wall-clock-bounded asynchronous Wasmtime store. Provider description receives a linked but disabled HTTP context, and any attempted description-time call rejects the component. Policy denials remain terminal even if guest code catches the typed WIT error.

The statically linked native client enforces exact authority/port and method grants, request count and byte bounds, HTTPS by default, loopback-only explicitly authorized plaintext, DNS address validation and pinning, sensitive-header ownership, no redirects, no ambient proxy, no automatic decompression, and bounded response collection. Its evidence contains method, authorized authority, status, and byte counts—not paths, queries, headers, or bodies.

The JSONPlaceholder demonstration keeps post reads and creates in separate capability IDs with read-only/idempotent versus external-write/non-idempotent metadata. Its guest accepts only the exact production HTTPS origin or explicit literal loopback HTTP endpoints, but guest validation is not authority: broker policy independently pins the exact authority and GET/POST method. Provider tests inject responses and broker tests use ephemeral loopback servers; CI does not contact the public service. Transport error details, post inputs, outputs, paths, and bodies remain absent from audit.

`dekopon-broker` wraps that host with a transport-independent trusted context, deny-by-default Cedar authorization, owner-authored execution constraint sets validated at startup against provider metadata and host ceilings, a bounded replay ledger, single-use authorization construction, stable public outcomes, digest evidence, and metadata-only hash-linked in-memory or durable audit chains. Human/service actor principals must match transport principals; an agent actor's identity reaches policy as `context.agent`.

Authorization and execution are deliberately different files with different failure modes. `dekopon-policy` decides who may act, over a schema generated from the deployment's own declared world and validated in Cedar's strict mode; a policy naming a principal, provider, capability, or entity type nobody configured refuses startup rather than becoming policy that can never match. Constraint sets decide how narrowly the broker then executes, and Cedar cannot reach them: no policy edit can widen a timeout, an output ceiling, an allowed host or method, or a credential binding. Evaluation errors deny. Provider input and message content are not policy context, so no policy can be made to depend on a value the caller supplies.

Decisions are explainable without being leaky. Every audit record carries `policy_ids`, the identifiers of the policies that determined the outcome (an `@id("…")` annotation names them stably), and `policy_digest`, a fingerprint of the policy set and world evaluated. Policy source itself reaches an operator only through startup errors — never through a per-request decision, an audit field, or a `Debug` rendering. Inputs, provider outputs, URL paths/queries, headers, bodies, and credentials are absent from audit records. Authorization decisions are appended before execution; if terminal audit append fails, the error explicitly says provider work may already have completed. `BrokerError::unaudited_outcome` makes that distinction structural rather than a matter of error text, and `dekopon-brokerd` preserves it across the wire as the `outcome-unaudited` failure code so a client can tell "nothing executed, safe to resubmit under a fresh identifier" from "the effect may have happened, do not resubmit".

`FileAuditLog` uses an exclusively writer-locked owner-only single-link file opened without symlink following, verifies bounded JSONL records before append, synchronizes each append, rejects partial records, exposes exact chain-prefix comparison, and reconstructs replay IDs for restart. `dekopon-brokerd` compares it with a separate strict checkpoint containing record count and chain head. Every audit append precedes an atomic, synchronized checkpoint replacement; startup rejects a missing checkpoint for non-empty audit or a checkpoint that is not an exact retained prefix. An audit exactly one record ahead of its valid checkpoint is the intentionally recoverable crash window; a larger gap fails closed.

`dekopon-broker-protocol` defines strict versioned frames and an unprivileged Unix client. Invocation wire values omit principal, actor, policy, constraints, credentials, and authorization. Frame lengths have a hard ceiling before allocation, complete reads/writes time out, and the client checks owner-only socket metadata plus server peer UID.

`dekopon-brokerd` now performs server-side Unix socket acceptance and derives `AuthenticatedContext` from the connected peer UID plus exact trusted configuration. It requires a private non-symlink parent, creates an owner-only socket, refuses unsafe/live replacement, limits concurrent one-request connections, drains under a configured grace period, restores replay IDs before listening, and removes only its own socket inode. Its strict configuration and provider files must be single-link, server-owned, and not group/world writable; provider parents must also be protected, writable non-sticky ancestors are rejected, and socket/audit/checkpoint/lock parents must be owner-only. The checkpoint has a dedicated single-writer lock, rejects symlinks and hard links, and uses synchronized temporary-file replacement plus parent-directory synchronization. Because mode `0600` makes the socket one UID trust domain, every process under that UID can use its configured actor—use a dedicated UID when this matters.

The service performs no process attestation, independent remote/signed checkpoint anchoring, or non-Unix network transport. It does inject destination-bound provider credentials: secrets load from an owner-only `0600` credentials file into `Redacted` wrappers, bind to explicit destination authorities that must cover every allowed host of the constraint set naming them, and enter a request only inside the native HTTP engine after guest headers were validated — a guest-supplied `authorization` header remains rejected, never overwritten. A constraint set may name one default credential and per-agent overrides of it; every credential the set can select is proved against the store and against that set's allowed hosts at startup, so an override is exactly as validated as the default. Evidence, audit, spans, accounting logs, and public results record only that injection happened (`credentialInjected`) and, in the terminal audit record and the execution span, the credential's owner-authored symbolic name — never the value, and redaction plus destination binding are independently tested. Its checkpoint is durable and externally inspectable, but locally rolling back or deleting both the audit and checkpoint can defeat comparison unless checkpoint generations are independently retained. Its presence does not expand direct `dekopon-run`: immediate subcommands retain the separate empty linker and reject HTTP-importing fixtures. Explicit broker subcommands load no component, require a trusted server UID, validate socket metadata/peer credentials, and send proposals with no identity, policy, constraint, credential, or authorization field. Their normal dependency path stops at the lightweight protocol/provider-metadata crates; CI rejects privileged broker, native-HTTP, or broker-service dependencies in the runner binary.

## Per-agent credentials, and where their boundary stops

**Status: current.** A constraint set may present a different broker-held secret depending on which
agent is acting. Combined with the mechanisms above — a route binding a transport and a channel
match to an agent, a canonical subject mapping to its own principal, and a policy conditioned on
`context.via` and `context.agent` — this is what makes two organizations reachable from one broker
with two tokens, no capability duplicated and no token reachable from the wrong workspace.

The selection input is trusted for the same reason `via` is. The agent name lives in the
`AuthenticatedContext` the broker derived from an owner-configured attestor grant and identity
mapping; it is never a payload field, and the map from agent to credential is owner-authored
configuration in the same file as the timeouts and allowed hosts. A caller with no agent — a direct
peer carrying `Actor::Service` — matches no override and takes the default. Policy cannot reach any
of it: a policy edit can broaden who may drive an agent and can never bind a credential.

Two limits are worth stating plainly. **Policy still cannot bind a path**, because provider input is
deliberately absent from the policy context, so no statement can restrict an agent to one
repository. A credential per agent narrows *who may use a token*, not *what that token can touch*:
the token's own scope at the provider remains the boundary on which repositories are reachable, and
a deployment that needs that boundary must obtain it from the token. And **the single-UID caveat is
unchanged** — separating two organizations' tokens by agent is attribution and blast-radius shape
until the gateway runs under its own UID, because any process under the owner's UID can already act
as the configured peer.

## Threat-model limitations

The current project does not defend against a malicious process in the broker/client UID trust domain (including one forging or suppressing informational UI reports), a local user who can replace the binary, component, or owner-controlled config; a compromised host; dependency or compiler compromise; denial of service during component compilation or from adversarial model endpoints; coordinated rollback of both local audit and checkpoint state; or side channels. The Wasmtime limits reduce invocation risk but are not a production sandbox claim. Prompt injection is explicitly not defended against: a chat message reaching `dekopond` can say anything to a model, and the containment is that the model can only propose, never authorize. On a `oneShot` route an injected instruction dies with the message that read it, because each gateway message is one independent session; on a `persistent` route it stays in the prompt for the rest of that conversation instead, which lengthens its dwell time without changing that containment. The project has no provider secret-store integration, per-process/client attestation, dedicated gateway UID, independent audit checkpoint retention/signing service, external evidence store, key management, revocation, tenancy isolation, operator-CLI integration with the broker or the daemon, or incident-response automation.

Conversation memory is implemented, and the trust surface stated above is now a live limitation rather than a review of a design. Its narrowest edge is worth repeating: grant invalidation compares capability *identifiers*, so a policy edit that tightens an owner-authored constraint set while keeping the same capability list produces an identical grant set and does not drop the history. Closing that needs a broker-protocol change rather than a gateway one. Agent memory that outlives a conversation is neither designed nor implemented.

The committed first privileged-provider design is documented in [`broker-http.md`](broker-http.md). It preserves the separate broker boundary, keeps direct `dekopon-run` execution import-free, and treats HTTP imports as structural requirements rather than authority.

Future releases must threat-model confused-deputy attacks, prompt injection, credential exfiltration, provider escalation, SSRF and DNS rebinding, redirect escapes, TOCTOU between authorization and execution, duplicate external effects, malicious Wasm components, resource exhaustion, forged identity envelopes, audit tampering, and cross-tenant data leaks before claiming production readiness.
