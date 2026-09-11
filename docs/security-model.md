# Security model

This document expands the security constraints introduced in [`design.md`](design.md). Read it before changing capabilities, identity, policy, credentials, providers, evidence, audit behavior, or external effects.

## Foundational invariant

A proposal is not authority: anything may create a `ProposedInvocation`, and only the broker may create an `AuthorizedInvocation`. [`design.md`](design.md#constitution) states the invariant; this document states how each boundary holds it.

A capability name in an agent spec permits the agent to propose that operation. It does not grant process authority, credentials, or permission to call a provider directly.

## Security-relevant stages

1. **Model proposal** — untrusted model output names a capability and supplies untrusted arguments in a `ProposedInvocation`.
2. **Authorization decision** — the privileged broker authenticates the transport, derives the actor/workload from trusted mapping, evaluates policy and current context, then either denies the proposal or creates a constrained `AuthorizedInvocation` inside its execution boundary.
3. **External effect** — the broker consumes that authorization state while a narrow provider executes only the authorized capability using broker-held credentials and enforced constraints.
4. **Evidence** — policy decisions and provider execution produce digests or bounded records that support later verification. *Committed direction:* removed; the trace is the record ([design.md](design.md#core-concepts)).
5. **Audit record** — the broker links proposal, trusted identity, policy revision, authorization receipt, effect outcome, and evidence under an invocation ID and the W3C trace id the request carried, which is also the trace the decision's spans are recorded in ([goal 2](design.md#constitution)).

The local daemon-to-broker request carries only a proposal over an authenticated Unix connection, not identity claims or an `AuthorizedInvocation` for the broker to trust. The broker does not return transferable authorization to `dekopond`; a serialized authorization representation is inert audit/evidence data rather than a bearer grant.

Rust's private, non-cloneable `AuthorizedInvocation` fields and absent deserialization make accidental in-process fabrication or reuse harder. `AuthorizationGate::new` is public so a broker adapter can own the transition; constructing that handle authenticates no caller and evaluates no policy. This is defense in depth only. The real authority boundary depends on separate processes, authenticated requests, policy enforcement, authorization bound to execution, isolated credentials, provider sandboxing, and credential-safe audit linkage.

## Trust boundaries

Trusted inputs are expected to include:

- principal and workload identity derived from authenticated transport plus owner-controlled mapping;
- broker configuration installed by an authorized operator;
- the owner-only Cedar policy file, read under the configuration's own hygiene rules (server-owned, single-link, not group/world writable, no symlink following, byte-capped) — it is authorization input in exactly the sense the configuration is;
- owner-authored execution constraint sets, validated at startup against loaded provider manifests, component-host ceilings, and the credential store;
- provider directories held to the same standard as every other trusted input the broker reads: owned by the expected UID and not group- or world-writable, because anyone who can write such a directory can add a component the broker compiles and runs. Every file a scan yields is validated individually exactly as a directly-named one is, and there is no implicit search path — a directory the broker loads code from is named in its owner-only configuration or nowhere;
- when managed providers are enabled, the strict byte-capped generated provider lock and the protected content-addressed store it names. The lock records an immutable OCI manifest digest plus component digest/length and provider ID; startup derives the blob path rather than accepting one from the lock and compares those expectations with the exact one-read buffer and bounded description passed into the privileged host. The operator-authored desired set and manager registry traffic are not consulted at daemon startup;
- a `strict` startup posture choosing whether configuration that *cannot apply* refuses startup or is reported and ignored. It governs complaint, never enforcement: a capability no loaded provider routes is denied `unconstrained-capability` before Cedar is consulted in either mode, and a policy naming one is registered as a schema-only phantom that no constraint set can bind and no route can reach. An undeclared *principal* stays fatal in both modes, because principals come from owner-authored configuration rather than from a loaded component;
- broker-generated authorization receipts and audit sequencing;
- secrets obtained by the broker from an approved secret store;
- the telemetry store the operator selected.

That last one carries goal 2 ([design.md](design.md#constitution)): access to the telemetry store is access to every conversation, prompt, and argument the system has handled. What rides the trace, and the short list of what never does, is in [observability.md](observability.md#exclusions).

Explicitly untrusted inputs include:

- model output, reasoning, tool names, and tool arguments;
- repository files, pull-request text, issues, comments, diffs, and fetched web content;
- provider responses until validated and bounded;
- identity claims embedded inside model text or repository content;
- an agent's catalog `instructions` and every skill it mounts. Both are operator-authored text handed to the model, and the model can read all of it — a skill's `SKILL.md` body and each of its resource files arrive in full through `read_skill` — so neither is a place for a secret. Both grant nothing: nothing in either can widen a capability or name a principal, and authority is only what the broker attests. The loader reads a skill whole into memory at catalog load under bounds fixed before a byte is read (64 KiB `SKILL.md`, 256 KiB per resource file, 64 resource files and 1 MiB of them per skill, four directory levels), accepts only UTF-8 regular files and directories, skips `.`-prefixed entries, and refuses a symbolic link rather than following it, so a catalog cannot pull an arbitrary file into a prompt and a session opens no file;
- local config supplied from an untrusted checkout.

A model or repository document cannot self-assert a trusted `Actor`. For the local broker, the connected peer UID and strict owner-controlled mapping own identity attribution; invocation payloads have no identity fields.

Attested proposals are the one sanctioned indirection, and they are narrow. A peer whose owner-configured identity carries an `attestor` grant may attach a typed `Attestation` naming a canonical external subject (`slack.t0123abc.u9xyz`, `discord.123456789012345678`, `whatsapp.16034700182`, `tel.16034700182`) alongside its proposal. Every one of them carries a name a real service verified before the message reached a transport; there is no service here for an identity nothing authenticated. The subject is transport routing metadata, not a principal and not authority: the broker — never the peer — resolves it through owner-controlled `identityMappings`, and an unmapped subject resolves to nothing rather than minting a principal on demand. The grant bounds which namespaces a peer may speak for, matched on segment boundaries so `slack.t0123abc` cannot reach workspace `t0123abcx`. A refused attestation is an audited denial under the peer's own principal with a stable reason (`attestation-denied`, `unmapped-subject`), never a silent error, so a compromised or misconfigured gateway leaves a decision trail. The resulting context is bound to a `via` naming the attestor, and policy sees it as `context.via`: a policy that requires a specific attestor can never authorize a direct peer, and one written `unless { context has via }` can never authorize an attested proposal. That is what stops adding a gateway from widening any grant that already existed. Driving an agent at all is a separate `agent.prompt` statement, so a mapped subject with no such grant is refused (`agent-denied`) before any capability is considered.

## Current local process boundary

The chart runs the broker as UID/GID **65532:65532** and the gateway as
**65533:65533**, with supplementary IPC group **65534**. The broker owns a **0660**
socket in a **0710**, broker-owned IPC directory with that group: the gateway can
connect but cannot replace the socket or write its parent. Group membership grants
reachability, not identity or authorization. The broker maps the actual OS peer UID;
the gateway explicitly pins `broker.serverUid: 65532`, checking both filesystem owner
and live server credentials. Unmapped peers are refused.

Broker configuration, provider credentials and private storage are broker-owned and
unreachable through gateway mounts; gateway configuration and model credentials are
gateway-owned and absent from broker mounts. Private files remain **0600**, private
directories **0700**; `fsGroup` is not used to widen them. The init container alone
sees projected sources, and separate claim subdirectories and temporary volumes keep
one daemon from replacing the other's private files. See the
[chart layout](../charts/dekopon/README.md#paths-the-chart-owns).

Attestor namespaces and `via` policy constrain a distinct gateway peer rather than a
broker-UID process. A compromised gateway can speak for subjects inside its configured
attestor grant; this is not independent per-process attestation or general tenant
isolation. Each UID is its own trust domain. Broker-UID compromise and host/root
compromise are outside this boundary ([`design.md`](design.md#non-goals)). Owner-only
local broker clients are supported with a private parent and **0600** socket; the
gateway's local chat socket is **0600**, owner-only and development-only.

## Capability and effect rules

- Capabilities are narrow and name one effect class.
- External writes require an explicit capability; read access never implies write access.
- Provider permissions should be least privilege and independently enforced by provider credentials.
- Authorization constraints bind timeout, output size, exact HTTP destinations/methods, call counts, and byte ceilings.
- Credential values do not appear in agent prompts, authored catalogs, invocation evidence, or normal logs. A legacy credential's symbolic *name* is owner-authored configuration; its `credential`/`credentialByAgent` binding [will be replaced by public DRNs](design.md#legacy-credential-bindings). A public DRN is inert typed proposal metadata: it is separately Cedar-authorized, matched to an owner-only use binding, and resolved only inside the broker. The broker records the symbolic name/DRN so an effect can be attributed to the authority that carried it, never the value or physical locator.
- A component import declares a required host interface; it never grants that interface or any transitive authority.
- Broker HTTP authorization binds exact destinations, methods, host-call counts, byte limits, and deadlines to one invocation. Secret-backed calls additionally bind the exact DRN, native sink, private binding ID, canonical path/query scope, and injection count.

The example reviewer has `github.pull-request.read` and the explicit external-write `github.pull-request.comment`. It does not have, and the example does not declare, `github.pull-request.approve`.

## Public DRNs and private resolution

**Status: current.** A model may propose one canonical logical DRN only through the typed top-level
`SecretUseProposal`; it cannot place one in provider input, a WIT value, URL, header or body. The
shell recognizes exact Basic/Bearer forms and strips the marker before capability JSON exists.
Immediate invokers refuse it. A broker-backed proposal then passes four independent ceilings:

1. ordinary capability Cedar policy;
2. separate `secret.use` Cedar policy over the exact `Dekopon::Secret` resource and authenticated
   routing context;
3. an owner-authored private binding fixing capability, sink, username where applicable,
   authority, method, canonical path/query rule and injection count;
4. the capability's broader HTTP constraints and native host ceilings.

The authorized proposal and effective constraints commit to the DRN, sink and binding ID.
`dekopon-broker-host` compares the resolved credential with that commitment before creating a
store. Resolution occurs once after the decision audit record is appended and is pinned to that
invocation; there is no stale fallback or cross-invocation cache. Unknown, unbound, wrong-sink and
policy-denied names all produce `secret-denied`, avoiding a source-inventory oracle.

The native host checks exact path and query scope before injection and discards a response carrying
the raw secret or complete Authorization value. This is defense in depth, not proof against a
malicious authorized endpoint: it may transform or semantically encode what it legitimately
received, which [`design.md`](design.md#non-goals) rules out defending. Basic authentication
necessarily gives the password to that endpoint. Narrow providers, upstream credential scope and
destination trust remain necessary. [`secrets.md`](secrets.md) is the complete source and
configuration contract.

## Current operator and model posture

The isolated `dekopond auth` namespace manages model-account login against fixed authentication hosts, before gateway configuration, transports, or runtime creation. Within that namespace, `dekopond auth chatgpt export` is the single command in Dekopon that writes credential material in the clear; it is gated behind a required `--expose-credential`, refused when standard output is a terminal unless `--allow-terminal` is passed, and warns on standard error that the exported copy is invalidated by the next refresh. It fails rather than emitting a partial document, and it makes no network request. See [`chatgpt-credential.md`](chatgpt-credential.md). Auth performs no model inference, provider credential resolution, authorization decisions, or external effects. Provider readiness in local config is descriptive data, not a verified connection.

The shared model client can contact an operator-selected OpenAI-compatible endpoint or
OpenAI's fixed ChatGPT/Codex subscription endpoints. Model bearer tokens are read from named
environment variables and terminate inside that client, never inside provider components.
ChatGPT authentication and inference hosts are fixed to `auth.openai.com` and
`chatgpt.com`; no credentials are imported from pi, OpenClaw, or Codex. The isolated credential
file is `0600` on Unix. Refresh rotates the token, so its directory must be writable and a
secret-store export is a seed for one deployment, not a backup. Opaque encrypted reasoning
items stay in memory and out of telemetry and provider input.

Provider schemas are model-facing metadata, not general host input validation; each provider
must validate capability-specific input. The shared agent loop accepts only typed tool
arguments, and catalog skills are loaded under bounds before a session starts, never by the
script. Skills are untrusted model text and grant no authority.

By default, OTLP trace/log fields omit prompts, model responses, and component input/output; the payload opt-in adds them to every sink the process writes. *Committed direction:* the gate is removed; payloads always on ([goal 2](design.md#constitution)). Bearer tokens and raw untrusted errors are omitted either way. OTLP lifecycle logs are operational data rather than authorized invocation evidence. Final text and machine-readable outputs remain untrusted data. Terminal table cells in auth status remove control characters. [`observability.md`](observability.md) is the complete telemetry contract.

## Current gateway posture

`dekopond` is the unprivileged process on the other side of the attestation boundary described above. Its posture is the inverse of the broker's: it holds the credentials needed to *hear* a request and to *ask a model*, and none of the credentials needed to *do* anything.

- **What terminates in the daemon.** Chat bot credentials (Slack app-level and bot tokens, Discord and Telegram bot tokens, plus the WhatsApp app secret, webhook verification token, and sending access token) and model credentials (an OpenAI-compatible API key or Dekopon's own ChatGPT device-flow credential file). All of them are named in configuration as *environment variable names*, never values, and load into `Redacted` wrappers; a missing or blank chat or bound-route model credential is a startup failure naming the variable and never its value. A model's `apiKeyEnv` is optional, because a loopback endpoint needs no key; naming a variable and exporting nothing into it is not. Blank counts as missing on purpose: an empty HMAC key verifies signatures anybody can compute, and an empty bearer token is sent as a header anyway, so an exported-but-empty variable is the absence of a credential presented as presence. A name that is not a valid variable name is rejected at startup, so a token pasted into that field is a refusal rather than a plaintext secret in a config file. WhatsApp uses its app secret only for exact-raw-body webhook HMAC and its access token only at the pinned Graph messages endpoint; neither enters prompts, broker requests, provider configuration, or normal logs.
- **What never enters it.** Provider credentials, policy, and authorization state. Every external effect is an attested proposal to `dekopon-brokerd`; the daemon supplies the sender's canonical subject and the agent name and nothing else, and the broker maps, decides, resolves, and executes.
- **What is untrusted.** Message text end to end, bounded to 16 KiB before it reaches a model and 8 KiB on the way back out. The names and media types of files attached to a chat message are the same untrusted text, and the 16 KiB bound covers the message and the reference note naming them together. File *contents* are untrusted in the same way and reach a model only on demand: each attachment is named in the prompt as `Chat Asset #N`, and the gateway fetches the bytes only when the model calls `fetch_chat_asset` with that number. An attachment is part of the message that carried it, and chat services deliver it by reference rather than by value. Resolving a Slack or Telegram reference uses the bot token the daemon already holds to hear a request; a Discord CDN download carries no token and returns to pinned Discord REST only to refresh an expired signed URL. Neither path grants policy, a provider credential, or a way to write anything. What bounds it is arithmetic rather than authority: an allowlist of media types a model can be shown, 8 MiB per attachment enforced while the response streams rather than after it, 256 KiB of a textual file on the way into the prompt with a trailer saying it was cut, four fetches per session, and a per-conversation ceiling on how many attachments stay addressable. Bytes are dropped with the request they joined, so nothing retains them. The agent's `instructions` from the catalog are untrusted model text by the same definition: they shape how an agent answers and can never assert identity, name a principal, or widen a capability. Broker policy never reads that field. A skill the agent's catalog entry mounts is the same text by the same definition: every session on the route lists it by name and description and may read its body and resource files in full through `read_skill`, so a skill is no more a place for a secret than the instructions are.
- **Authorization is a gate, not a filter.** A session calls `capabilities` under a chat attestation of `(subject, agent, scope)` before any model call or in-flight activity write, and the broker answers it only if policy permits `agent.prompt` for that principal and agent. An empty answer, or a refusal, ends the session with a fixed sentence and costs nothing. Failures also answer one fixed line — a `PromptError` can carry model, provider, or transport text, and none of it reaches chat.
- **Provider attachments are courier work, not authority.** Producing an image is a provider effect: the broker authorizes each call, audits it, and holds the credential. The gateway holds no image credential. On a route with `providerAttachments: { maxPerReply: N }`, the broker leg strips the reserved `attachments: [{mediaType, base64}]` result key before the shell sees it. It accepts only `image/png`, valid base64 with a PNG signature, at most 8 MiB decoded per entry, and no more than the route's session-wide reply ceiling. A request-local slot carries accepted bytes only to the authenticated reply target; the model reads byte-free `attached: [{mediaType, bytes}]` metadata. A route without the opt-in strips and discards attachments too. Failed or cancelled sessions discard the slot. Bytes stay out of model transcripts and conversation memory; attachment content remains untrusted. Refusal reports a fixed sentence and an audit event, not a failed invocation: the provider effect already happened.
- **Chat-asset capability inputs are new reach, configured per route.** `chatAssetInputs` lists capabilities whose input may contain an exact `chat-asset:<N>` marker. Before proposing, the broker leg expands it to a `data:` URL using the existing metadata store and transport reader; unlisted capabilities receive the string unchanged. Only image media types are accepted, with at most three expansions and 8.5 MiB decoded per invocation, twelve expansions per session, and 8 MiB per attachment. These bounds are separate from the model's four `fetch_chat_asset` calls. Expansion precedes broker authorization, so the session bound also limits downloads for proposals the broker denies. Refusal submits no proposal, returns a fixed sentence, and emits a stable audit reason. Neither a marker nor downloaded bytes grant authority; the provider validates its own input.
- **In-flight activity is presentation, not authority.** It is opt-in, uses the same chat bot credential already terminating in this process, and targets only service-native channel/thread/message coordinates derived from the authenticated envelope. The model supplies no coordinate, status text, emoji, cadence, or fallback. Discord typing and Telegram chat actions expire; Slack Agent status is explicitly returned to active, while its classic/free fallback is the one fixed `:tangerine:` reaction and removes only a reaction that generation successfully added. Activity failures carry only low-cardinality categories and never affect the answer. Slack Agent Stop is authenticated transport control rather than model text: it cooperatively prevents later model/tool/broker work and suppresses history, answer delivery, attachment delivery, and durable recording, but cannot roll back a model request or provider effect already in progress.
- **Slack Agent continuation is authorization-fed routing state.** The Agent installation's channel-history scopes cause Slack to deliver ambient public/private channel events, but the transport drops them before routing, authorization, telemetry, or inference unless the bot was explicitly addressed or the exact authenticated workspace/channel/root-thread/sender tuple is already owned. A claim enters the 1,024-entry process-local LRU only after a fresh non-empty broker surface, refreshes only after fresh authorization, is removed on a definitive refusal, and disappears on restart. Another sender in the same thread owns nothing until separately addressed and authorized. An inherited message gets one request-scoped no-reply tool; selecting it before capability work emits no chat post or durable receipt. A decline selected alongside work runs nothing, while any earlier capability invocation makes a visible report mandatory so the model cannot conceal an effect by choosing silence; with no reporting turn left, a fixed warning directs the sender to audit before retrying.
- **Self-inspection is narrower than configuration access.** Every authorized gateway session may call `inspect_agent_config`. Its typed result contains the catalog agent's identifier, description, model class and exact standing instructions; route limits and conversation mode; mounted skills by name, description, and resource path, never their text; and only the capability metadata from that sender's fresh chat-scoped `capabilities` result. It includes no raw Cedar source, policy IDs or digest, principal, subject, transport/channel identifier, execution constraint, model or broker endpoint/path, legacy credential name, private secret-map source/selector/binding inventory, or credential value. Exact standing instructions remain visible and may intentionally contain a public inert DRN. The gateway never receives provider credentials or raw policy in the first place, and the view constructor has no field for the chat/model credentials it does hold. The bounded view is repeatable under the prompt loop's shared per-turn tool-call and model-step bounds, with no inspection-specific call limit. Inspection consumes no capability budget, makes no broker invocation, grants nothing, and produces no durable broker audit record. Standing instructions are therefore authorized-user-visible rather than confidential; putting a credential in a prompt would already disclose it to the model and is invalid configuration hygiene.
- **Improvement suggestions are advisory telemetry, not a channel.** A route offers `suggest_improvement` only under `improvementSuggestions: true`, because an accepted call writes `agent.improvement.suggested` carrying model-authored text; enabling it is the consent that declares the log sink in scope for that text. The record holds the six fields the model wrote — enum tokens for category and confidence, and `target`, `summary`, `evidence`, and `proposal` bounded to 128, 512, 2048, and 2048 bytes, trimmed and stripped of control characters other than newline and tab — and no subject and no chat text of the daemon's own; a session records at most three, and a bound violation is `agent.improvement.refused` plus a tool result the model reads rather than a session failure. The daemon relays nothing to chat and applies nothing: a suggestion is a record a person reads, never a change to an instruction, skill, limit, or grant.
- **The development transport is the one exception** to "identity comes from authenticated transport": it trusts its local caller to declare a subject. It grants nothing by doing so, because the claim has to pass the broker's attestor grant and identity mapping, and its `0600` socket under an owner-only parent keeps it inside the gateway UID trust domain. It is a development tool, not a production transport.

The WhatsApp transport adds one public wakeup surface to the unprivileged daemon. It terminates no TLS and exposes no admin method. GET proves only knowledge of the separate verification token. POST identity is accepted only after one exact HMAC-SHA256 over the untouched bounded bytes; the signed `messages[].from`, never profile/display text, becomes `whatsapp.<wa_id>`, while the exact configured WABA and receiving-phone tuple stays transport-derived scope. Message-ID replay handling is bounded process memory, so restart can admit a redelivery and a post-200 crash can lose queued work; no durable exactly-once claim is made ([`design.md`](design.md#non-goals)). Outbound Graph sends are never blindly retried because a timeout after transmission has unknown outcome.

Being public also makes the daemon's own telemetry an attacker-reachable resource, which no other transport's is. Refused requests are reported per reason once a minute with the count they stand for rather than once each, so a stranger cannot turn a wrong signature into unbounded volume in a shared log sink, and a genuinely wrong app secret is one obvious line. A refusal never records the body, the headers, the sender, or the message ID it refused.

The [current local process boundary](#current-local-process-boundary) applies to every transport. See [`dekopond.md`](dekopond.md) for the complete gateway contract.

## Conversation memory as a trust surface

**Status: current.** A route set to `mode: persistent` implements the [Conversations](dekopond.md#conversations) contract: a history bounded by a sliding window, an idle timeout, and a process-wide ceiling, held in the gateway process's memory and replayed into the next prompt. `scope: privateConversation` is the persistent default and isolates it by authenticated subject. `scope: sharedConversation` is an explicit audience expansion inside one exact agent/transport/conversation key. `oneShot` is the route default, so a route that does not ask for memory runs each message as an independent session that starts from an empty prompt.

### Containment

The broker authorizes every invocation. A persistent conversation opens a fresh attested leg per message exactly as a one-shot session does — the same chat-scoped `capabilities` call, the same policy evaluation against the same `via`-scoped rules, the same audit record. No grant is cached, no decision is carried forward, and replayed history reaches the model as prompt text and never reaches the broker as authorization input.

So persistence widens no authority. Everything a model can do with a remembered conversation, it can do with a single message: propose.

### What persistence widens is duration

Prompt injection is not defended against, and this is the change that matters to it.

On a `oneShot` route an instruction embedded in a pull-request body, an issue comment, or a fetched page reaches the model and dies with the message that read it. The next message starts from an empty prompt, so an injection gets exactly one turn and its blast radius is one session's proposals.

With history it stays. The injected text — or the model's own answer restating it — sits in the prompt for the rest of that conversation, up to `maxTurns`, up to `maxBytes`, up to the idle timeout, and every subsequent turn in that conversation is evaluated with it present. A person who asks three follow-ups after the poisoned message is asking all three with the injection in scope.

The mitigations below shorten the dwell time. None of them detects the injection, because nothing in this project does. A route that keeps the `oneShot` default keeps the one-turn dwell time, which is why the mode is opt-in.

### Shared scope widens audience as well as duration

`sharedConversation` lets a participant's prompt and the agent's answer survive into prompts initiated by other authenticated participants. Attachments follow the same complete key and live generation, so their numbered references and fetchability are shared too. This is disclosure within an owner-selected route, not a policy bypass, and disclosure is a security effect. A participant can seed prompt injection that persists for everyone using that conversation, ask the model to repeat prior text, or cause another participant's canonical identifier to be sent again on replay.

The transport identity determines how wide "conversation" is. Slack sharing is normally rooted at the opening message and its thread replies. Discord guild messages use the channel identity, so a shared route can cover the **whole guild channel**; only a native Discord thread channel creates a different identity. Operators must evaluate the service-native audience, not assume every UI reply gesture creates a private thread.

Each shared user turn is prefixed by the gateway with `[gateway: authenticated participant: <canonical-subject>]`. The subject comes from the transport envelope accepted for the fresh broker leg, not message text, and a user-authored lookalike cannot replace that first line. This gives the model provenance; it does not make the following text trustworthy, defend against prompt injection, or prove that a later participant was allowed to see the earlier content outside this configuration choice. On the local development transport, only the owner UID is authenticated and the subject is that trusted caller's declaration; the uniform wording does not claim independent service authentication of the person. Canonical subjects can be phone numbers or service user IDs and are model-provider input.

The scope stops at `(agent, configured transport, transport-derived conversation identity)`. It is not global agent memory, team memory, a durable shared namespace, or automatic replay across channels, threads, transports, routes, agents, restarts, or idle/capacity/grant invalidation. Every participant starts a fresh attested leg and must independently be authorized for the agent on every message. If their capability identifier sets differ, the conservative grant comparison drops the shared window rather than carrying content across those grants.

### The second-order case: history outliving its grant

Tool output a session fetched under a broad grant is in the history. If the owner then narrows what that subject may reach, the text remains in the prompt even though the capability that produced it is gone — a quiet way for a revocation to be less complete than the owner believes.

The mechanism that closes it: the granted capability set is stored with the conversation and compared against the fresh leg's grant on every message. Any difference drops the history and its attachment generation and starts a new conversation; an empty grant removes the entry outright and closes the same asset fence, which is the same refusal an unauthorized sender already gets, applied to what was remembered as well as to what may be done. It costs a cache miss on the first message after any policy change. That is the correct price: a narrowed grant is precisely the moment replaying old output is wrong.

Two honest limits on that mechanism. It compares capability *identifiers*, so a policy edit that keeps the same capability list while tightening its owner-authored constraint set — a narrower allowed host, a smaller output ceiling, a different injected credential — produces an identical grant set and does not drop the history; text fetched under the older constraints survives until the window or the idle timeout removes it. And invalidation removes text from a future prompt, never from anywhere it was already shown: the answer that quoted it is in a chat transcript the daemon does not own, and in the operator's telemetry store.

Invalidation also has to survive concurrency. Each seeded session receives a monotonically generated lease and attachment-access fence for the exact store generation it read. Grant replacement, empty-grant removal, idle replacement, and capacity eviction invalidate older leases and close their fences; a late model completion becomes a no-op, and stale work can neither publish attachment metadata nor resolve a source for byte fetching. A transport read that started while the generation was live may finish concurrently, but the fence is checked again and those bytes are discarded instead of entering the model after retirement. Sessions from the same live generation append in completion order, reuse one inventory, and preserve the first committed cache lane. Asset numbering is monotonic for that live generation even if the independently bounded asset map drops and later rebuilds its inventory, so a removed number cannot silently name a newer file.

### The mitigations, as a set

None of these is sufficient alone, and the design depends on all of them:

- **Scope- and generation-aware complete keying.** Every key includes agent, configured transport, and transport-derived conversation identity. Private scope additionally includes the canonical authenticated subject, so one person's exchange cannot enter another person's prompt. Explicit shared scope omits only that subject and accepts that participants in the exact conversation can receive one another's earlier text, answers, and attachment references. Transcript removal closes the matching asset-generation fence; old metadata may remain inert in the bounded map until its next lazy prune, but stale sessions cannot inventory it or fetch its bytes.
- **Idle timeout.** An untouched conversation is evicted, 15 minutes by default. It bounds how long an injection or a stale tool result can persist without anyone continuing the conversation that produced it. The check is lazy — the eviction happens on the next lookup rather than on a timer — so an idle entry can outlive its timeout in memory until something asks for it or the ceiling displaces it. What it can never do is reach a prompt.
- **The window.** `maxTurns` and `maxBytes` bound what is replayed regardless of how long the conversation lives, so a long-running conversation does not accumulate an unbounded prompt and old turns fall out of scope on their own.
- **Compaction.** A stored turn is `(the user's message, the final answer)`; intermediate tool calls, model-authored scripts, and their output are dropped. Materially less untrusted repository and provider text is replayed than the session actually read, and the replayed prompt cannot grow with the size of a tool result — one script's output alone can reach 256 KiB.
- **In gateway memory.** History lives in the gateway process, is never written to disk by the daemon, is never sent to the broker, and dies with the process. It also reaches the operator's telemetry store, for as long as that store retains it.
- **Grant-set invalidation.** Described above: the granted capability set travels with the conversation, and a change drops it.

### Where the text goes

Not into the broker. `dekopon-brokerd` holds provider credentials and a metadata-only append-only JSONL audit log in which a provider's output survives only as a digest, and its records exclude inputs, outputs, paths, queries, headers, and bodies. Putting conversation text in that process would place the most sensitive content in the system inside the most privileged one, next to a record built specifically not to contain it, and it would turn a log of what was *authorized* into a store of what was *said*.

Telemetry is the other direction. `conversation.turns` and `conversation.bytes` are a count and a byte total because a span attribute is the wrong container for unbounded text, and the history itself rides `agent.model.prompt` on the log stream, inside the operator's trust boundary described under [Trust boundaries](#trust-boundaries). There is no mode in which it does not.

And not into the [prompt cache key](dekopond.md#the-prompt-cache-key), which is minted from entropy rather than derived from the private subject or the shared conversation identifier. A canonical subject can be a phone number, and a hash of one is a stable pseudonym; either would tell a model provider that two conversations months apart belong to one person, which is a worse thing to hand a third party than to hand your own sink. The minted key rotates whenever the conversation generation it names is evicted and whenever the process restarts, so it accumulates into a durable identifier for nobody and no service-native conversation, and it confers nothing: a request carrying a key is authorized exactly as one without it, by the broker, per message.

### What this does not fix

Keeping history in memory does not protect it from another process running as the gateway UID.
The [current local process boundary](#current-local-process-boundary) separates that UID
from the broker, not from itself. Paging and core dumps are outside the daemon's control.
The development transport authenticates the local gateway UID, not the declared person;
configured transport and agent keys prevent aliasing another route's history.

## Provider storage and durable on-demand chat memory

**Status: current.** The broker may hold a separate provider-storage PVC
and a 32-byte namespace key. Components receive no WASI, host path, environment, socket, or
ambient I/O: an exact JSONL or durable-files import is linked to a single-use grant bound to host
instance, invocation, capability, provider, interface, access, chat namespace, scope commitment,
and limits. HTTP and storage authority cannot coexist in one v1 capability. Description and command
resolution receive a disabled sticky context.

Chat storage needs more than the existing subject attestation. New operations carry an
invocation-bound transport/channel/conversation claim; the owner must grant both the subject
namespace and an explicit transport-wide, exact-channel, or exact-conversation `chatScopes` entry.
The canonical scope also enters Cedar context. What is reserved is what the owner declared: a
constraint set carries a `route:` of `chatMemoryRecord`, `chatMemoryRecent`, or `chatMemorySearch`,
and legacy capability, run, resolve, and invoke operations omit and refuse exactly those capabilities and
every command word of the provider they name. Naming a capability `memory.chat.export` or a
provider `memory-chat` reserves nothing, and renaming the shipped provider drops nothing. Generic
chat invocation may reach the two retrieval routes but never the record route.

A provider command word is ungated. `runCommand` and the legacy `resolveCommand`
carry no capability to decide on, so the broker runs the declaring component's argv handling — a
pure, import-free guest call under the ordinary fuel and wall-clock bounds — before any
authorization, and authorizes only the proposal that comes back, on exactly the path a direct
`invoke` takes. Text the guest renders itself (a help page, a usage error) is provider-authored,
pre-authorization, model-visible output: it authorizes nothing, charges no capability call, and is
bounded by the host output ceiling and the shell's value and output ceilings, but it is the
provider speaking to the model, with the trust its manifest and schemas already carry. The order
of checks is what keeps the reservation above meaningful: a refused attestation and a reserved
word are both answered as an unknown word before the guest is instantiated, so a reserved provider
renders not even its help page, and a proposal that lands on a chat-memory route is refused after
the run whatever word produced it. The piped value is bounded by the client's frame ceiling before
it leaves the process and by the host's `maxInputBytes` before a store exists; the broker reports
the second only in its own `command.resolve.failed` record, and the caller sees the opaque
`provider-error` reply. [`dekopon-brokerd` contract](../crates/dekopon-broker-protocol/README.md#command-execution-refusals)
carries the wire detail.

Recording is **model-hidden, gateway-attested transport acceptance**, not broker-proven delivery or
human receipt. Slack/Telegram/Discord receipts prove complete service acceptance; local `flush`
proves kernel acceptance. Discord partial delivery produces no receipt. The gateway submits one
fresh dedicated request after acceptance, waits once, and never retries after timeout, EOF, denial,
or outcome-unknown. Its already delivered answer remains answered.

Storage audit records omit principal, actor/agent, via/subject, provider, broker principal/policy
revision, policy IDs/digest, and credential. Separate HMAC domains keep physical paths, audit scope,
record IDs, content/dedup, evidence, authority, generation, and authority pointers unlinkable by equality.
Storage spans omit identity/scope/provider/capability and exact payload bytes; only operation/sync/
quota counts and powers-of-two byte buckets remain.

The filesystem boundary retains directory descriptors and uses descriptor-relative no-follow
opens, scans, creates, renames, and unlinks. It detects/refuses ordinary symlinks, hard links, wrong
identities, unsafe modes, malformed namespace layouts, and a second conforming writer. Base then
generation lease ordering serializes authority pointers, grants, and invocation access. Startup
validates only the root. A grant that finds its namespace's authority pointer or generation corrupt
rotates that namespace to a fresh, empty generation and fails that one invocation; corruption in the
base's own shape fails the grant naming the entry. The host never chmods a directory it has
classified as untrusted. An actively malicious same-UID process
racing filesystem mutation is out of scope. Native filesystem operations can remain blocked after
a timeout signal; the finalization budget prevents starting the next bounded finalization step after
its deadline, while leases/reservations stay held until an already-started blocking operation
drains, so this is not a hard wall-clock guarantee. Durable-files has rollback-journal lock primitives that no I/O path
consults: reads, writes, size, truncate, and sync never inspect handle lock state, so the lock table
is well-formedness bookkeeping rather than an access control. There is no SHM operation and no
multiprocess-database claim. A single-instance WAL engine needs neither and runs on these primitives
unchanged; the out-of-tree `turso-sql` provider ships one, calls `lock` zero times, and opens
exactly two files.
Writes apply per host call. A trap can leave partial database/log changes; neither sync nor
invocation success promises cross-file atomicity or crash recovery ([`design.md`](design.md#non-goals)).

Memory text is not encrypted by Dekopon at rest, has no deletion/export UX, and is never
automatically replayed. JSONL dedup records are permanent but finite; at the explicit record/byte
cap, new recording returns `dedup-capacity` while reads remain available.

## Current privileged broker foundation

`dekopon-broker-host` is the privileged component library; in deployment only the separately deployed `dekopon-brokerd` process runs it (directly and through `dekopon-broker`), while `dekopon-provider-sdk-testkit` embeds it in-process as a fake broker for provider tests. It links only versioned Dekopon HTTP and storage interfaces, consumes one non-cloneable `AuthorizedInvocation` plus an exact single-use storage grant when applicable, and runs each description or invocation in a fresh memory-, fuel-, input-, output-, and wall-clock-bounded asynchronous Wasmtime store. Provider description and command resolution receive disabled HTTP/storage contexts, and any attempted host call rejects the component. Policy/storage denials remain terminal even if guest code catches the typed WIT error.

The statically linked native client enforces exact authority/port and method grants, request count and byte bounds, HTTPS by default, loopback-only explicitly authorized plaintext, DNS address validation and pinning, sensitive-header ownership, no redirects, no ambient proxy, no automatic decompression, and bounded response collection. Its evidence contains method, authorized authority, status, and byte counts—not paths, queries, headers, or bodies.

The standalone JSONPlaceholder demonstration keeps post reads and creates in separate capability IDs, one read-only and one external-write. Its guest accepts only the exact production HTTPS origin or explicit literal loopback HTTP endpoints, but guest validation is not authority: broker policy independently pins the exact authority and GET/POST method. Provider tests inject responses and broker tests use ephemeral loopback servers; CI does not contact the public service. Transport error details, post inputs, outputs, paths, and bodies remain absent from audit.

`dekopon-broker` wraps that host with a transport-independent trusted context, deny-by-default Cedar authorization, owner-authored execution constraint sets validated at startup against provider metadata and host ceilings, single-use authorization construction, stable public outcomes, digest evidence, and metadata-only bounded in-memory logs or owner-only append-only JSONL audit. *Committed direction:* opt-in sink, off by default; audit is a log record in the trace ([non-goals](design.md#non-goals)). Human/service actor principals must match transport principals; an agent actor's identity reaches policy as `context.agent`.

Authorization and execution are different files with different failure modes. `dekopon-policy` decides who may act, over a schema generated from the deployment's own declared world and validated in Cedar's strict mode; a policy naming a principal, provider, capability, or entity type nobody configured refuses startup rather than becoming policy that can never match. Constraint sets decide how narrowly the broker then executes, and Cedar cannot reach them: no policy edit can widen a timeout, an output ceiling, an allowed host or method, or a credential binding. Evaluation errors deny. Arbitrary provider input and message content are not policy context. A canonical public DRN is the one typed exception, evaluated only through the separate `secret.use` target whose private binding remains the execution ceiling.

Decisions are explainable without being leaky. Every audit record carries `policy_ids`, the identifiers of the policies that determined the outcome (an `@id("…")` annotation names them stably), and `policy_digest`, a fingerprint of the policy set and world evaluated. Policy source itself reaches an operator only through startup errors — never through a per-request decision, an audit field, or a `Debug` rendering. Inputs, provider outputs, URL paths/queries, headers, bodies, and credentials are absent from audit records. Authorization decisions are appended before execution; if terminal audit append fails, the error explicitly says provider work may already have completed. `BrokerError::unaudited_outcome` makes that distinction structural rather than a matter of error text, and `dekopon-brokerd` preserves it across the wire as the `outcome-unaudited` failure code so a client can tell "nothing executed" from "the effect may have happened".

`FileAuditLog` uses an exclusively writer-locked owner-only single-link file opened without symlink following. Startup counts newline-delimited records in bounded space for the next ordinal, enforcing the line-size limit without decoding or verifying history; unterminated tails are refused, not repaired. Appends are flushed, not fsynced. A failed or cancelled append can leave partial bytes and poisons the handle. There is no tamper-detection, rollback protection, or crash-recovery guarantee, and [`design.md`](design.md#non-goals) rules out building one. The bounded `InMemoryAuditLog` remains available for tests and embedding.

`dekopon-broker-protocol` defines strict versioned frames and an unprivileged Unix client. Invocation wire values omit principal, actor, policy, constraints, credential values, and authorization. An optional public DRN/sink proposal is inert and requires the broker's separate decision. Frame lengths have a hard ceiling before allocation, complete reads/writes time out, and the client checks protected socket metadata plus server peer UID.

`dekopon-brokerd` performs server-side Unix socket acceptance and derives `AuthenticatedContext` from the connected peer UID plus exact trusted configuration. It requires a broker-owned non-symlink parent, creates the socket under the [current local process boundary](#current-local-process-boundary), refuses unsafe/live replacement, limits concurrent one-request connections, drains under a configured grace period, and removes only its own socket inode. Its strict configuration and provider files must be single-link, server-owned, and not group/world writable; provider parents must also be protected, writable non-sticky ancestors are rejected, and audit parents must be owner-only. Every process under a mapped UID can use its configured actor; group access alone supplies no actor.

The same executable has a separate provider-manager operator mode. Exact-reference `sync` is the only path that resolves a mutable tag; an unchanged desired reference preserves its existing manifest digest, while `sync --locked` can fetch a missing component directly by the locked layer digest without requesting the tag manifest. Registry manifests, token/error bodies, and component streams are independently bounded and timed. Components are staged and synchronized under their digest before complete provider-set validation; only the generated lock is activated atomically, so an orphan blob is possible and a partially validated active set is not. Installed, orphan, and stale-temporary files share a hard 4 GiB/1,024-file lifetime ceiling checked under the store lock; the absence of automatic prune can require explicit operator cleanup at the ceiling. `list`, `verify`, and daemon startup make no network request. Public anonymous OCI Bearer flow is supported; registry credentials, custom certificate roots, SemVer ranges, update/remove/prune, and vulnerability/revocation response are not.

The service performs no process attestation or non-Unix network transport. It injects legacy destination-bound credentials from an owner-only `0600` credentials file. The `credential`/`credentialByAgent` selection [will be replaced by public DRNs](design.md#legacy-credential-bindings), preserving broker-owned refresh and injection. It also supports an owner-only private secret map: public DRNs remain untrusted proposal names until an ordinary capability decision, a separate exact `secret.use` decision, and a private binding all allow; only then does the broker resolve one source snapshot and construct native Basic/Bearer material. Both paths enter a request only inside the native HTTP engine after guest headers were validated — a guest-supplied `authorization` header is rejected, never overwritten. A constraint set may name one default credential and per-agent overrides of it; every credential the set can select is proved against the store and against that set's allowed hosts at startup, so an override is exactly as validated as the default. Evidence, audit, spans, accounting logs, and public results record only that injection happened (`credentialInjected`) and, in the terminal audit record and the execution span, the credential's owner-authored symbolic name — never the value, and redaction plus destination binding are independently tested. The gateway uses the lightweight protocol client, validates socket metadata and server peer credentials, and submits proposals without policy, constraints, credential values, or authorization state; an optional DRN/sink is inert proposal data. CI excludes privileged broker machinery from its normal dependencies.

## Per-agent credentials, and where their boundary stops

**Status: current.** A constraint set may present a different broker-held secret depending on which
agent is acting. Combined with the mechanisms above — a route binding a transport and a channel
match to an agent, a canonical subject mapping to its own principal, and a policy conditioned on
`context.via` and `context.agent` — this is what makes two organizations reachable from one broker
with two tokens, no capability duplicated and no token reachable from the wrong workspace.
*Committed direction:* `credential`/`credentialByAgent` bindings will be replaced by public DRNs,
preserving per-agent isolation ([migration requirements](design.md#legacy-credential-bindings)).
The following describes the current implementation.

The selection input is trusted for the same reason `via` is. The agent name lives in the
`AuthenticatedContext` the broker derived from an owner-configured attestor grant and identity
mapping; it is never a payload field, and the map from agent to credential is owner-authored
configuration in the same file as the timeouts and allowed hosts. A caller with no agent — a direct
peer carrying `Actor::Service` — matches no override and takes the default. Policy cannot reach any
of it: a policy edit can broaden who may drive an agent and can never bind a credential.

Two limits are worth stating plainly. **Legacy capability/per-agent credential policy cannot
bind a provider-input path**, because arbitrary provider JSON is absent from policy context. A
credential per agent narrows *who may use a token*, not *what that token can touch*. A DRN binding
can additionally constrain the native HTTP path on which that one secret is injected, but it does
not interpret repository/object identity in bodies or upstream API semantics. The token's own scope
at the provider remains the final boundary. The [current local process boundary](#current-local-process-boundary)
separates gateway and broker UIDs; per-agent credential selection does not establish a separate OS identity for each organization.

## Threat-model limitations

[`design.md`](design.md#non-goals) lists what this project has decided not to defend: a malicious process in the broker's trust domain, a compromised host or root, crash durability and audit tamper-detection, transformed reflection at an authorized endpoint, duplicate-effect defense, end-user privacy from the operator, and production-sandbox claims for Wasmtime. This section adds the limits that are specific to what is built.

The project does not defend against a local user who can replace the binary, component, provider lock/store, or owner-controlled config; dependency or compiler compromise; denial of service during component compilation or from adversarial model endpoints; or side channels. The Wasmtime limits reduce invocation risk. Prompt injection is not defended against: a chat message reaching `dekopond` can say anything to a model, and the containment is that the model can only propose, never authorize. On a `oneShot` route an injected instruction dies with the message that read it, because each gateway message is one independent session; on a `persistent` route it stays in the prompt for the rest of that conversation, which lengthens its dwell time without changing that containment.

The project has no provider provenance verification in its manager, provider registry credential/custom-root support, provider vulnerability/revocation automation, leased/dynamic secret lifecycle, workload-identity secret-source bootstraps, per-process/client attestation, external evidence store, key management, tenancy isolation, operator-CLI integration with the broker or the daemon, or incident-response automation. A digest proves byte identity rather than publisher identity; container staging therefore retains its independent GitHub attestation checks and is not replaced by manager output.

Conversation replay and optional durable on-demand chat memory are both live trust surfaces. Immediate in-memory grant invalidation compares capability *identifiers*. Durable `authority-bound` continuity instead commits effective capability metadata, constraints, selected legacy symbolic credentials, effective DRN use bindings/private-map revision, provider artifact bytes, host/storage ceilings, backend, and memory limits, and rotates a non-reusing random generation when that semantic surface changes. `stable` explicitly keeps prior text reachable across those changes after fresh authorization. Neither mechanism detects prompt injection or makes recalled text trusted authorization input.

Provider execution boundaries are in the [`dekopon-brokerd` contract](../crates/dekopon-brokerd/README.md#boundaries), which preserves the separate broker boundary and treats HTTP imports as structural requirements rather than authority.

Before any production-readiness claim, threat-model confused-deputy attacks, prompt injection, credential exfiltration, provider escalation, SSRF and DNS rebinding, redirect escapes, TOCTOU between authorization and execution, malicious Wasm components, resource exhaustion, forged identity envelopes, and cross-tenant data leaks.
