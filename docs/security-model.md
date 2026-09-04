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
- when managed providers are enabled, the strict byte-capped generated provider lock and the protected content-addressed store it names. The lock records an immutable OCI manifest digest plus component digest/length and provider ID; startup derives the blob path rather than accepting one from the lock and compares those expectations with the exact one-read buffer and bounded description passed into the privileged host. The operator-authored desired set and manager registry traffic are not consulted at daemon startup;
- a `strict` startup posture choosing whether configuration that *cannot apply* refuses startup or is reported and ignored. It governs complaint, never enforcement: a capability no loaded provider routes is denied `unconstrained-capability` before Cedar is consulted in either mode, and a policy naming one is registered as a schema-only phantom that no constraint set can bind and no route can reach. An undeclared *principal* stays fatal in both modes, because principals come from owner-authored configuration rather than from a loaded component;
- broker-generated authorization receipts and audit sequencing;
- secrets obtained by the broker from an approved secret store.

Explicitly untrusted inputs include:

- model output, reasoning, tool names, and tool arguments;
- repository files, pull-request text, issues, comments, diffs, and fetched web content;
- provider responses until validated and bounded;
- identity claims embedded inside model text or repository content;
- an agent's catalog `instructions` and every skill it mounts. Both are operator-authored text handed to the model, and the model can read all of it — a skill's `SKILL.md` body and each of its resource files arrive in full through `read_skill` — so neither is a place for a secret. Both grant nothing: nothing in either can widen a capability or name a principal, and authority is only what the broker attests. The loader reads a skill whole into memory at catalog load under bounds fixed before a byte is read (64 KiB `SKILL.md`, 256 KiB per resource file, 64 resource files and 1 MiB of them per skill, four directory levels), accepts only UTF-8 regular files and directories, skips `.`-prefixed entries, and refuses a symbolic link rather than following it, so a catalog cannot pull an arbitrary file into a prompt and a session opens no file;
- local config supplied from an untrusted checkout.

A model or repository document cannot self-assert a trusted `Actor`. For the local broker, the connected peer UID and strict owner-controlled mapping own identity attribution; invocation payloads have no identity fields.

Attested proposals are the one sanctioned indirection, and they are deliberately narrow. A peer whose owner-configured identity carries an `attestor` grant may attach a typed `Attestation` naming a canonical external subject (`slack.t0123abc.u9xyz`, `discord.123456789012345678`, `whatsapp.16034700182`, `tel.16034700182`) alongside its proposal. Every one of them carries a name a real service verified before the message reached a transport; there is no service here for an identity nothing authenticated. The subject is transport routing metadata, not a principal and not authority: the broker — never the peer — resolves it through owner-controlled `identityMappings`, and an unmapped subject resolves to nothing rather than minting a principal on demand. The grant bounds which namespaces a peer may speak for, matched on segment boundaries so `slack.t0123abc` cannot reach workspace `t0123abcx`. A refused attestation is an audited denial under the peer's own principal with a stable reason (`attestation-denied`, `unmapped-subject`), never a silent error, so a compromised or misconfigured gateway leaves a decision trail. The resulting context is bound to a `via` naming the attestor, and policy sees it as `context.via`: a policy that requires a specific attestor can never authorize a direct peer, and one written `unless { context has via }` can never authorize an attested proposal. That is what stops adding a gateway from widening any grant that already existed. Driving an agent at all is a separate `agent.prompt` statement, so a mapped subject with no such grant is refused (`agent-denied`) before any capability is considered.

The trust model this accepts should be stated plainly. In the current single-UID deployment an attestor grant adds no authority beyond what any process under the owner's UID already has, because that UID is one trust domain and every process in it can already act as the configured peer. What the mechanism buys today is attribution and blast-radius shape: subject-level audit, deny-by-default policy conditioned on `via`, and configuration that fails closed on undeclared principals or duplicate subjects. Namespace scoping and `via` become real isolation only when the gateway runs under its own UID with its own peer identity, and that deployment — along with the socket permissions it needs — remains committed direction requiring its own review.

## Capability and effect rules

- Capabilities are narrow and name one effect class.
- External writes require an explicit capability; read access never implies write access.
- Provider permissions should be least privilege and independently enforced by provider credentials.
- Authorization constraints bind timeout, output size, exact HTTP destinations/methods, call counts, and byte ceilings.
- Retries account for declared idempotency and use provider-enforced idempotency keys where available.
- Credential values do not appear in agent prompts, authored catalogs, invocation evidence, or normal logs. A legacy credential's symbolic *name* is owner-authored configuration. A public DRN is inert typed proposal metadata: it is separately Cedar-authorized, matched to an owner-only use binding, and resolved only inside the broker. The broker records the symbolic name/DRN so an effect can be attributed to the authority that carried it, never the value or physical locator.
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
store. Resolution occurs once after durable decision audit and is pinned to that invocation; there
is no stale fallback or cross-invocation cache. Unknown, unbound, wrong-sink and policy-denied names
all produce `secret-denied`, avoiding a source-inventory oracle.

The private map supports strict secure files, Kubernetes AtomicWriter projections, 1Password
Connect item fields, Vault KV v1/v2, AWS Secrets Manager/SSM with an explicit SigV4 session file,
GCP Secret Manager, Azure Key Vault, and Kubernetes API Secret/ConfigMap sources. Bootstrap files
remain owner-only and are not DRN-addressable. Direct service-account/workload identity chains and
Vault dynamic leases remain outside the current lifecycle. [`secrets.md`](secrets.md) is the
complete source and configuration contract.

The native host checks exact path and query scope before injection and discards a response carrying
the raw secret or complete Authorization value. This is defense in depth, not proof against a
malicious authorized endpoint: it may transform or semantically encode what it legitimately
received. Basic authentication necessarily gives the password to that endpoint. Narrow providers,
upstream credential scope and destination trust remain necessary.

## Current operator and immediate-runner posture

The `dekopon` catalog commands read operator-selected YAML or JSON plus every skill directory the catalog names (whole, at load, under the skill loader's bounds stated above), reject unknown fields, validate identifiers and references, and render declarations without network access. The isolated `dekopon auth` namespace is the only current exception: it manages model-account login against fixed authentication hosts. Within that namespace, `dekopon auth chatgpt export` is the single command in Dekopon that writes credential material in the clear; it is documented, gated behind a required `--expose-credential`, refused when standard output is a terminal unless `--allow-terminal` is passed, and warns on standard error that the exported copy is invalidated by the next refresh. Its output form is chosen explicitly, it fails rather than emitting a partial document, and it makes no network request. See [`chatgpt-credential.md`](chatgpt-credential.md). The CLI performs no model inference, provider credential resolution, authorization decisions, or external effects. Provider readiness in local config is descriptive data, not a verified connection.

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
- optional runner OTLP export sends generated performance spans and stable lifecycle events to an operator-selected endpoint, and by default (`--otel-telemetry-payloads false`) omits prompts, model responses, model-authored script text and its output, and provider input/output; `--otel-telemetry-payloads true` declares every sink the process writes — including a local `--trace` file — in scope for that data. Credentials, broker socket paths, and raw errors stay out in either mode;
- `--skill` directories are loaded under the skill loader's bounds before any model call; a directory that does not load, or a name mounted twice, is a usage failure and no model turn is spent. The session holds the text and opens no file;
- `--suggestions` offers `suggest_improvement` and prints what the model recorded on standard error. It is off by default as a deliberate exception to the omission above: each accepted call writes `agent.improvement.suggested` carrying model-authored text in either payload mode, under the terms stated for the gateway route opt-in below;
- `session list` and `session show` load no component and run no model; they, and `session replay --trace-id`, read exported records back from an operator-named OpenObserve organization base. The `Authorization` header value comes from the environment variable `--openobserve-auth-env` names (default `DEKOPON_OPENOBSERVE_AUTHORIZATION`), never from an argument; an unset variable is an error naming the variable, and a URL carrying userinfo, a query, or a fragment is refused. The client follows no redirects, so the header cannot be forwarded to a host nobody named, uses no ambient proxy, bounds each response to 32 MiB and each search to 20 pages of 500 records, and admits a trace identifier into SQL only as 1–128 characters of `[A-Za-z0-9._-]`. What comes back is untrusted data inspected field by field;
- `session replay` answers every script the replayed model writes from the recording, so by default no capability runs and no effect happens; the first script the recording never ran stops the replay and is reported as the divergence. Only when `--provider` components are supplied does that script run, and then in the same read-only, import-free direct mode as `prompt`: no broker leg, no HTTP, no credential store, and a DRN proposal is refused; the mechanism and its deliberate absences — no store, no rewriter, no grader — are [`improvement.md`](improvement.md)'s; this document states only the trust surface;
- immediate success output is raw untrusted JSON, not broker evidence, an authorization receipt, or an `InvocationResult`;
- immediate tool calls are not `AuthorizedInvocation` values and cannot be used for local or external writes.

By default, Chrome and OTLP trace/log fields omit prompts, model responses, and component input/output; the payload opt-in adds them to every sink the process writes. Bearer tokens and raw untrusted errors are omitted in both modes. The operator-selected telemetry endpoint still learns execution metadata such as service/model/provider/capability identifiers, timings, outcomes, and source locations. OTLP lifecycle logs are operational audit data, not authorized invocation evidence or a substitute for the broker's durable hash-linked log. Final text and machine-readable outputs remain untrusted data. Terminal table cells in the catalog CLI continue to remove control characters.

## Current gateway posture

`dekopond` is the unprivileged process on the other side of the attestation boundary described above. Its posture is the inverse of the broker's: it holds the credentials needed to *hear* a request and to *ask a model*, and none of the credentials needed to *do* anything.

- **What terminates in the daemon.** Chat bot credentials (Slack app-level and bot tokens, Discord and Telegram bot tokens, plus the WhatsApp app secret, webhook verification token, and sending access token) and model credentials (an OpenAI-compatible API key, Dekopon's own ChatGPT device-flow credential file, or an explicitly named OpenAI Images key). All of them are named in configuration as *environment variable names*, never values, and load into `Redacted` wrappers; a missing or blank image-generator, chat, or bound-route model credential is a startup failure naming the variable and never its value. A model's `apiKeyEnv` is optional, because a loopback endpoint needs no key; naming a variable and exporting nothing into it is not optional, and it used to become a tokenless client that started clean and answered every message with a 401. Blank counts as missing on purpose: an empty HMAC key verifies signatures anybody can compute, and an empty bearer token is still sent as a header, so an exported-but-empty variable is the absence of a credential presented as presence. A name that is not a valid variable name is rejected at startup, so a token pasted into that field is a refusal rather than a plaintext secret in a config file. WhatsApp uses its app secret only for exact-raw-body webhook HMAC and its access token only at the pinned Graph messages endpoint; neither enters prompts, broker requests, provider configuration, telemetry, or normal logs.
- **What never enters it.** Provider credentials, policy, and authorization state. Every external effect is an attested proposal to `dekopon-brokerd`; the daemon supplies the sender's canonical subject and the agent name and nothing else, and the broker maps, decides, resolves, and executes.
- **What is untrusted.** Message text end to end, bounded to 16 KiB before it reaches a model and 8 KiB on the way back out. The names and media types of files attached to a chat message are the same untrusted text, and the 16 KiB bound covers the message and the reference note naming them together. File *contents* are untrusted in the same way and reach a model only on demand: each attachment is named in the prompt as `Chat Asset #N`, and the gateway fetches the bytes only when the model calls `fetch_chat_asset` with that number. This is the one place the gateway reads something a sender supplied out of band, and it is deliberate — an attachment is part of the message that carried it, and chat services deliver it by reference rather than by value. Resolving a Slack or Telegram reference uses the bot token the daemon already holds to hear a request; a Discord CDN download carries no token and returns to pinned Discord REST only to refresh an expired signed URL. Neither path grants policy, a provider credential, or a way to write anything. What bounds it is arithmetic rather than authority: an allowlist of media types a model can actually be shown, 8 MiB per attachment enforced while the response streams rather than after it, 256 KiB of a textual file on the way into the prompt with a trailer saying it was cut, four fetches per session, and a per-conversation ceiling on how many attachments stay addressable. Bytes are dropped with the request they joined, so nothing retains them. The agent's `instructions` from the catalog are untrusted model text by the same definition: they shape how an agent answers and can never assert identity, name a principal, or widen a capability. Broker policy never reads that field. A skill the agent's catalog entry mounts is the same text by the same definition: every session on the route lists it by name and description and may read its body and resource files in full through `read_skill`, so a skill is no more a place for a secret than the instructions are.
- **Authorization is a gate, not a filter.** A session calls `capabilities` under a chat attestation of `(subject, agent, scope)` before any model call or in-flight activity write, and the broker answers it only if policy permits `agent.prompt` for that principal and agent. An empty answer, or a refusal, ends the session with a fixed sentence and costs nothing. Failures also answer one fixed line — a `PromptError` can carry model, provider, or transport text, and none of it reaches chat.
- **Generated images are bounded model output, not authority.** A route receives `generate_image` only when it opts into the one owner-configured generator. The client is fixed to OpenAI's public Images endpoint; the model supplies one prompt and never an endpoint, credential, filename, media type, or destination. One attempt may return one signature-validated PNG of at most 8 MiB. Bytes leave the prompt loop through a request-local slot and go only to the authenticated envelope's reply target; they never enter model transcripts, telemetry payloads, in-process/durable conversation memory, provider JSON, broker protocol, evidence, or audit. Image generation can incur model cost under the already authorized `agent.prompt` session, which is why it is route opt-in and one-attempt rather than ambient. Generated content remains untrusted and no content-safety claim is added beyond the configured model service's behavior.
- **In-flight activity is presentation, not authority.** It is opt-in, uses the same chat bot credential already terminating in this process, and targets only service-native channel/thread/message coordinates derived from the authenticated envelope. The model supplies no coordinate, status text, emoji, cadence, or fallback. Discord typing and Telegram chat actions expire; Slack Agent status is explicitly returned to active, while its classic/free fallback is the one fixed `:tangerine:` reaction and removes only a reaction that generation successfully added. Activity failures carry only low-cardinality categories and never affect the answer. Slack Agent Stop is authenticated transport control rather than model text: it cooperatively prevents later model/tool/broker work and suppresses history, answer delivery, generated-image delivery, and durable recording, but cannot roll back a model request, image-generation request, or provider effect already in progress.
- **Slack Agent continuation is authorization-fed routing state.** The Agent installation's channel-history scopes cause Slack to deliver ambient public/private channel events, but the transport drops them before routing, authorization, payload telemetry, or inference unless the bot was explicitly addressed or the exact authenticated workspace/channel/root-thread/sender tuple is already owned. A claim enters the 1,024-entry process-local LRU only after a fresh non-empty broker surface, refreshes only after fresh authorization, is removed on a definitive refusal, and disappears on restart. Another sender in the same thread owns nothing until separately addressed and authorized. An inherited message gets one request-scoped no-reply tool; selecting it before capability work emits no chat post or durable receipt. A decline selected alongside work runs nothing, while any earlier capability invocation makes a visible report mandatory so the model cannot conceal an effect by choosing silence; with no reporting turn left, a fixed warning directs the sender to audit before retrying.
- **Self-inspection is deliberately narrower than configuration access.** Every authorized gateway session may call `inspect_agent_config`. Its typed result contains the catalog agent's identifier, description, model class and exact standing instructions; route limits and conversation mode; mounted skills by name, description, and resource path, never their text; and only the capability metadata from that sender's fresh chat-scoped `capabilities` result. It includes no raw Cedar source, policy IDs or digest, principal, subject, transport/channel identifier, execution constraint, model or broker endpoint/path, legacy credential name, private secret-map source/selector/binding inventory, or credential value. Exact standing instructions remain visible and may intentionally contain a public inert DRN. The gateway never receives provider credentials or raw policy in the first place, and the view constructor has no field for the chat/model credentials it does hold. The bounded view is repeatable under the prompt loop's shared per-turn tool-call and model-step bounds, with no inspection-specific call limit. Inspection consumes no capability budget, makes no broker invocation, grants nothing, and produces no durable broker audit record. Standing instructions are therefore authorized-user-visible rather than confidential; putting a credential in a prompt would already disclose it to the model and remains invalid configuration hygiene.
- **Improvement suggestions are advisory telemetry, not a channel.** A route offers `suggest_improvement` only under `improvementSuggestions: true`, because an accepted call writes `agent.improvement.suggested` carrying model-authored text whether or not `telemetryPayloads` is on; enabling it is the consent that declares the log sink in scope for that text. The record holds the six fields the model wrote — enum tokens for category and confidence, and `target`, `summary`, `evidence`, and `proposal` bounded to 128, 512, 2048, and 2048 bytes, trimmed and stripped of control characters other than newline and tab — and no subject and no chat text of the daemon's own; a session records at most three, and a bound violation is `agent.improvement.refused` plus a tool result the model reads rather than a session failure. The daemon relays nothing to chat and applies nothing: a suggestion is a record a person reads, never a change to an instruction, skill, limit, or grant.
- **The development transport is the one deliberate exception** to "identity comes from authenticated transport": it trusts its local caller to declare a subject. It grants nothing by doing so, because the claim still has to pass the broker's attestor grant and identity mapping, and its `0600` socket under an owner-only parent keeps it inside the UID trust domain the broker socket already lives in. It is a development tool, not a production transport.

The first WhatsApp transport adds one deliberately public wakeup surface to the unprivileged daemon. It terminates no TLS and exposes no admin method. GET proves only knowledge of the separate verification token. POST identity is accepted only after one exact HMAC-SHA256 over the untouched bounded bytes; the signed `messages[].from`, never profile/display text, becomes `whatsapp.<wa_id>`, while the exact configured WABA and receiving-phone tuple stays transport-derived scope. Message-ID replay handling is bounded process memory, so restart can admit a redelivery and a post-200 crash can lose queued work; no durable exactly-once claim is made. Outbound Graph sends are never blindly retried because a timeout after transmission has unknown outcome.

Being public also makes the daemon's own telemetry an attacker-reachable resource, which no other transport's is. Refused requests are reported per reason once a minute with the count they stand for rather than once each, so a stranger cannot turn a wrong signature into unbounded volume in a shared log sink, and a genuinely wrong app secret is still one obvious line. A refusal never records the body, the headers, the sender, or the message ID it refused.

The single-UID caveat above applies unchanged: `dekopond` and `dekopon-brokerd` currently run as the same user, so the attestor grant buys attribution and blast-radius shape rather than isolation until the gateway has its own UID. See [`dekopond.md`](dekopond.md) for the complete contract.

### Informational status reporting and the web UI

`dekopond` additionally sends two bounded reports over the authenticated Unix protocol: a normalized catalog inventory (agent description, enabled/model-class flags, capability/provider identifiers, and provider permission declarations) and provider-reported model-token deltas. It never sends agent instructions, prompts, answers, subjects, principals, model/provider credentials, policy, constraints, or authorization. Only a mapped peer with an attestor grant may publish them. The broker retains the latest inventory and saturating token totals in memory, resets them on restart, and never consults either for Cedar, constraint or credential selection, routing, execution, evidence, replay, or durable audit. A compromised gateway can make this *informational display* lie; it cannot turn the lie into authority.

`dekopon-brokerd --http-bind <ADDRESS>` explicitly enables a GET/HEAD-only TCP surface from `dekopon-webui`; no TCP listener exists when the flag is absent. `/` redirects to `/ui`, there is no login, and no HTTP route mutates broker state. That does not make its contents public-safe. The pages disclose agent names and declared permissions, provider descriptions and input schemas, local component paths and digests, Wasmtime limits/activity, and the credential-free OTLP endpoint/service configuration. Header and resource-attribute values are withheld, provider and OTLP credentials never enter UI state, and all rendered authored/component strings are HTML-escaped under a closed content-security policy carried by every response, including the 405 an unrouted method produces. Because this listener shares the privileged broker's address space and container memory limit, it is bounded like the broker's Unix socket: sixteen concurrent connections, refused rather than queued, each with a thirty-second wall-clock budget from accept to close, so a slow-reading or slowloris client on the allowed network cannot accumulate rendered responses toward an OOM kill. The operator-selected network around the bind address is the access boundary; `0.0.0.0:8080` deliberately exposes this deployment metadata on every interface. The Helm chart reaches the same flag through `broker.httpBind`, which is empty by default and passes no argument at all when it is; setting it appends `--http-bind <address>` to the broker container's arguments and nothing else, because the chart creates neither a Service nor an Ingress for an unauthenticated listener. A cluster deployment therefore binds loopback or a cluster-internal address and routes it, if at all, outside the chart.

Persistent conversations add one more thing that terminates in this daemon — chat text that outlives the message carrying it — and the next section states that surface.

## Conversation memory as a trust surface

**Status: current.** A route set to `mode: persistent` implements the [Conversations](dekopond.md#conversations) contract: a history bounded by a sliding window, an idle timeout, and a process-wide ceiling, held in the gateway process's memory and replayed into the next prompt. `scope: privateConversation` is the persistent default and isolates it by authenticated subject. `scope: sharedConversation` is an explicit audience expansion inside one exact agent/transport/conversation key. `oneShot` remains the route default, so a route that does not ask for memory still runs each message as an independent session that starts from an empty prompt.

### Containment is unchanged

The broker authorizes every invocation. A persistent conversation opens a fresh attested leg per message exactly as a one-shot session does — the same chat-scoped `capabilities` call, the same policy evaluation against the same `via`-scoped rules, the same audit record. No grant is cached, no decision is carried forward, and replayed history reaches the model as prompt text and never reaches the broker as authorization input.

So persistence widens no authority. Everything a model can do with a remembered conversation, it can already do with a single message: propose. The invariant that a proposal is not authority is untouched, and this design does not ask for an exception to it.

### What persistence widens is duration

Prompt injection is not defended against, and this is the change that matters to it.

Today an instruction embedded in a pull-request body, an issue comment, or a fetched page reaches the model, and it dies with the message that read it. The next message starts from an empty prompt, so an injection gets exactly one turn and its blast radius is one session's proposals.

With history it stays. The injected text — or the model's own answer restating it — sits in the prompt for the rest of that conversation, up to `maxTurns`, up to `maxBytes`, up to the idle timeout, and every subsequent turn in that conversation is evaluated with it still present. A person who asks three follow-ups after the poisoned message is asking all three with the injection in scope. The exposure is the same in kind and longer in dwell time, and that is worth stating here rather than leaving to be discovered when a conversation behaves oddly on its fourth turn.

The mitigations below shorten the dwell time. None of them detects the injection, because nothing in this project does. A route that keeps the `oneShot` default keeps today's one-turn dwell time exactly, which is why the mode is opt-in rather than a new default.

### Shared scope widens audience as well as duration

`sharedConversation` deliberately lets a participant's prompt and the agent's answer survive into prompts initiated by other authenticated participants. Attachments follow the same key, so their numbered references and fetchability are shared too. This is disclosure within an owner-selected route, not a policy bypass, but disclosure is still a security effect. A participant can seed prompt injection that persists for everyone using that conversation, ask the model to repeat prior text, or cause another participant's canonical identifier to be sent again on replay.

The transport identity determines how wide "conversation" is. Slack sharing is normally rooted at the opening message and its thread replies. Discord guild messages use the channel identity, so a shared route can cover the **whole guild channel**; only a native Discord thread channel creates a different identity. Operators must evaluate the service-native audience, not assume every UI reply gesture creates a private thread.

Each shared user turn is prefixed by the gateway with `[gateway: authenticated participant: <canonical-subject>]`. The subject comes from the authenticated envelope, not message text, and a user-authored lookalike cannot replace that first line. This gives the model provenance; it does not make the following text trustworthy, defend against prompt injection, or prove that a later participant was allowed to see the earlier content outside this configuration choice. Canonical subjects can be phone numbers or service user IDs and are model-provider input regardless of the telemetry payload setting.

The scope stops at `(agent, configured transport, transport-derived conversation identity)`. It is not global agent memory, team memory, a durable shared namespace, or automatic replay across channels, threads, transports, routes, agents, restarts, or idle/capacity/grant invalidation. Every participant still starts a fresh attested leg and must independently be authorized for the agent on every message. If their capability identifier sets differ, the conservative grant comparison drops the shared window rather than carrying content across those grants.

### The second-order case: history outliving its grant

Tool output a session fetched under a broad grant is in the history. If the owner then narrows what that subject may reach, the text is still in the prompt even though the capability that produced it is gone — a quiet way for a revocation to be less complete than the owner believes.

The mechanism that closes it: the granted capability set is stored with the conversation and compared against the fresh leg's grant on every message. Any difference drops the history and starts a new conversation; an empty grant removes the entry outright, which is the same refusal an unauthorized sender already gets, applied to what was remembered as well as to what may be done. It costs a cache miss on the first message after any policy change. That is the correct price: a narrowed grant is precisely the moment replaying old output is wrong, and paying for one extra round trip to be sure of it is a good trade.

Two honest limits on that mechanism. It compares capability *identifiers*, so a policy edit that keeps the same capability list while tightening its owner-authored constraint set — a narrower allowed host, a smaller output ceiling, a different injected credential — produces an identical grant set and does not drop the history; text fetched under the older constraints survives until the window or the idle timeout removes it. And invalidation removes text from a future prompt, never from anywhere it was already shown: the answer that quoted it is in a chat transcript the daemon does not own.

Invalidation also has to survive concurrency. Each seeded session receives a monotonically generated lease for the exact store generation it read. Grant replacement, empty-grant removal, idle replacement, and capacity eviction invalidate older leases; a late model completion becomes a no-op rather than recreating or overwriting forgotten history. Sessions from the same still-live generation append in completion order, preserving the existing concurrent-follow-up behavior and the first committed cache lane.

### The mitigations, as a set

None of these is sufficient alone, and the design depends on all of them:

- **Scope-aware complete keying.** Every key includes agent, configured transport, and transport-derived conversation identity. Private scope additionally includes the canonical authenticated subject, so one person's exchange cannot enter another person's prompt. Explicit shared scope omits only that subject and intentionally accepts that participants in the exact conversation can receive one another's earlier text, answers, and attachment references.
- **Idle timeout.** An untouched conversation is evicted, 15 minutes by default. It bounds how long an injection or a stale tool result can persist without anyone continuing the conversation that produced it. The check is lazy — the eviction happens on the next lookup rather than on a timer — so an idle entry can outlive its timeout in memory until something asks for it or the ceiling displaces it. What it can never do is reach a prompt.
- **The window.** `maxTurns` and `maxBytes` bound what is replayed regardless of how long the conversation lives, so a long-running conversation does not accumulate an unbounded prompt and old turns fall out of scope on their own.
- **Compaction.** A stored turn is `(the user's message, the final answer)`; intermediate tool calls, model-authored scripts, and their output are dropped. Materially less untrusted repository and provider text is replayed than the session actually read, and the replayed prompt cannot grow with the size of a tool result — one script's output alone can reach 256 KiB.
- **In memory only.** History lives in the gateway process, is never written to disk by the daemon, is never sent to the broker, and dies with the process. There is no file to exfiltrate, no backup to age out of a retention policy, and nothing to recover after a restart.
- **Grant-set invalidation.** Described above: the granted capability set travels with the conversation, and a change drops it.

### Where the text deliberately does not go

Not into the broker. `dekopon-brokerd` holds provider credentials and a metadata-only hash-linked audit chain in which a provider's output survives only as a digest, and its records deliberately exclude inputs, outputs, paths, queries, headers, and bodies. Putting conversation text in that process would place the most sensitive content in the system inside the most privileged one, next to a record built specifically not to contain it, and it would turn a chain that proves what was *authorized* into a store of what was *said*. The gateway already reads the message and writes the answer, so keeping the history there adds no new reader.

Not into telemetry either. `conversation.turns` and `conversation.bytes` are a count and a byte total; the history itself follows the existing payload gate and appears only in `agent.model.prompt` under `telemetryPayloads: true`, where it makes that event both larger and older than it was. Enabling payloads on a persistent route declares the telemetry sink in scope for a conversation rather than for a message. This gate says nothing about model input: a shared turn always carries its gateway-authored canonical participant identifier to the selected model provider, even with `telemetryPayloads: false`, because attribution is part of the prompt rather than telemetry.

And not into the [prompt cache key](dekopond.md#the-prompt-cache-key). Every model request declares one so the provider's prefix cache has somewhere to route the requests that share a prefix, and it is minted from entropy rather than derived from the sender. A canonical subject can be a phone number, and a hash of one is a stable pseudonym; either would tell a model provider that two conversations months apart belong to one person, which is the linkage the metadata-only telemetry default exists to withhold and a worse thing to hand a third party than to hand your own sink. The minted key rotates whenever the conversation it names is evicted and whenever the process restarts, so it never accumulates into a durable identifier for anybody, and it is a routing hint that confers nothing: a request carrying a key is authorized exactly as one without it, which is to say by the broker, per message.

### What this does not fix

"In memory only" is a durability property and not an isolation one. In the current single-UID deployment any process running as the owner can read the gateway's memory, exactly as it can already act as the configured gateway peer. Operating-system paging and core dumps are outside what the daemon controls. The development transport trusts a local caller inside that UID to declare its canonical subject, but configured-transport and agent components prevent it from aliasing another transport's or agent's state key. None of this is new authority; all of it is the single-UID caveat applying to a new kind of content, and a dedicated gateway UID is what turns it into a real boundary.

## Provider storage and durable on-demand chat memory

**Status: current.** The broker may hold a separate provider-storage PVC
and a 32-byte namespace key. Components still receive no WASI, host path, environment, socket, or
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

A provider command word is deliberately ungated. `runCommand` and the legacy `resolveCommand`
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
`provider-error` reply. [`broker-http.md`](broker-http.md#runcommand-and-resolvecommand-are-deliberately-ungated)
carries the wire detail.

Recording is **model-hidden, gateway-attested transport acceptance**, not broker-proven delivery or
human receipt. Slack/Telegram/Discord receipts prove complete service acceptance; local `flush`
proves kernel acceptance. Discord partial delivery produces no receipt. The gateway submits one
fresh dedicated request after acceptance, waits once, and never retries after timeout, EOF, denial,
or outcome-unknown. Its already delivered answer remains answered.

Storage audit records omit principal, actor/agent, via/subject, provider, broker principal/policy
revision, policy IDs/digest, and credential. Separate HMAC domains keep physical paths, audit scope,
record IDs, content/dedup, evidence, authority, generation, and manifests unlinkable by equality.
Storage spans omit identity/scope/provider/capability and exact payload bytes; only operation/sync/
quota counts and powers-of-two byte buckets remain. Existing non-storage records retain their prior
serialized bytes and chain hashes.

The filesystem boundary retains directory descriptors and uses descriptor-relative no-follow
opens, scans, creates, renames, and unlinks. It detects/refuses ordinary symlinks, hard links, wrong
identities, unsafe modes, malformed transaction states, and a second conforming writer. Base then
generation lease ordering serializes authority pointers, lifecycle markers, grants, and GC; isolated
namespace corruption is quarantined while retaining quota. An actively malicious same-UID process
racing filesystem mutation is out of scope. Native filesystem operations can remain blocked after
a timeout signal; the finalization budget prevents starting the next bounded finalization step after
its deadline, while leases/reservations stay held until an already-started blocking operation
drains, so this is not a hard wall-clock guarantee. Durable-files has rollback-journal lock primitives that no I/O path
consults: reads, writes, size, truncate, and sync never inspect handle lock state, so the lock table
is well-formedness bookkeeping rather than an access control. There is still no SHM operation and no
multiprocess-database claim. A single-instance WAL engine needs neither and runs on these primitives
unchanged; the out-of-tree `turso-sql` provider ships one, calls `lock` zero times, and opens
exactly two files.
The durability boundary is the invocation transaction, not the guest's `sync` — a trap mid-write
leaves the reopened database at the last committed state.

Memory text is not encrypted by Dekopon at rest, has no deletion/export UX, and is never
automatically replayed. JSONL dedup records are permanent but finite; at the explicit record/byte
cap, new recording returns `dedup-capacity` while reads remain available.

## Current privileged broker foundation

`dekopon-broker-host` is the privileged component library; in deployment only the separately deployed `dekopon-brokerd` process runs it (directly and through `dekopon-broker`), while `dekopon-provider-sdk-testkit` embeds it in-process as a fake broker for provider tests and `dekopon-webui` consumes its metrics and loaded-provider metadata types for display. It links only versioned Dekopon HTTP and storage interfaces, consumes one non-cloneable `AuthorizedInvocation` plus an exact single-use storage grant when applicable, and runs each description or invocation in a fresh memory-, fuel-, input-, output-, and wall-clock-bounded asynchronous Wasmtime store. Provider description and command resolution receive disabled HTTP/storage contexts, and any attempted host call rejects the component. Policy/storage denials remain terminal even if guest code catches the typed WIT error.

The statically linked native client enforces exact authority/port and method grants, request count and byte bounds, HTTPS by default, loopback-only explicitly authorized plaintext, DNS address validation and pinning, sensitive-header ownership, no redirects, no ambient proxy, no automatic decompression, and bounded response collection. Its evidence contains method, authorized authority, status, and byte counts—not paths, queries, headers, or bodies.

The standalone JSONPlaceholder demonstration keeps post reads and creates in separate capability IDs with read-only/idempotent versus external-write/non-idempotent metadata. Its guest accepts only the exact production HTTPS origin or explicit literal loopback HTTP endpoints, but guest validation is not authority: broker policy independently pins the exact authority and GET/POST method. Provider tests inject responses and broker tests use ephemeral loopback servers; CI does not contact the public service. Transport error details, post inputs, outputs, paths, and bodies remain absent from audit.

`dekopon-broker` wraps that host with a transport-independent trusted context, deny-by-default Cedar authorization, owner-authored execution constraint sets validated at startup against provider metadata and host ceilings, a bounded replay ledger, single-use authorization construction, stable public outcomes, digest evidence, and metadata-only hash-linked in-memory or durable audit chains. Human/service actor principals must match transport principals; an agent actor's identity reaches policy as `context.agent`.

Authorization and execution are deliberately different files with different failure modes. `dekopon-policy` decides who may act, over a schema generated from the deployment's own declared world and validated in Cedar's strict mode; a policy naming a principal, provider, capability, or entity type nobody configured refuses startup rather than becoming policy that can never match. Constraint sets decide how narrowly the broker then executes, and Cedar cannot reach them: no policy edit can widen a timeout, an output ceiling, an allowed host or method, or a credential binding. Evaluation errors deny. Arbitrary provider input and message content are not policy context. A canonical public DRN is the one typed exception, evaluated only through the separate `secret.use` target whose private binding remains the execution ceiling.

Decisions are explainable without being leaky. Every audit record carries `policy_ids`, the identifiers of the policies that determined the outcome (an `@id("…")` annotation names them stably), and `policy_digest`, a fingerprint of the policy set and world evaluated. Policy source itself reaches an operator only through startup errors — never through a per-request decision, an audit field, or a `Debug` rendering. Inputs, provider outputs, URL paths/queries, headers, bodies, and credentials are absent from audit records. Authorization decisions are appended before execution; if terminal audit append fails, the error explicitly says provider work may already have completed. `BrokerError::unaudited_outcome` makes that distinction structural rather than a matter of error text, and `dekopon-brokerd` preserves it across the wire as the `outcome-unaudited` failure code so a client can tell "nothing executed, safe to resubmit under a fresh identifier" from "the effect may have happened, do not resubmit".

`FileAuditLog` uses an exclusively writer-locked owner-only single-link file opened without symlink following, verifies bounded JSONL records before append, synchronizes each append, rejects partial records, exposes exact chain-prefix comparison, and reconstructs replay IDs for restart. `dekopon-brokerd` compares it with a separate strict checkpoint containing record count and chain head. Every audit append precedes an atomic, synchronized checkpoint replacement; startup rejects a missing checkpoint for non-empty audit or a checkpoint that is not an exact retained prefix. An audit exactly one record ahead of its valid checkpoint is the intentionally recoverable crash window; a larger gap fails closed.

`dekopon-broker-protocol` defines strict versioned frames and an unprivileged Unix client. Invocation wire values omit principal, actor, policy, constraints, credential values, and authorization. An optional public DRN/sink proposal is inert and requires the broker's separate decision. Frame lengths have a hard ceiling before allocation, complete reads/writes time out, and the client checks owner-only socket metadata plus server peer UID.

`dekopon-brokerd` now performs server-side Unix socket acceptance and derives `AuthenticatedContext` from the connected peer UID plus exact trusted configuration. It requires a private non-symlink parent, creates an owner-only socket, refuses unsafe/live replacement, limits concurrent one-request connections, drains under a configured grace period, restores replay IDs before listening, and removes only its own socket inode. Its strict configuration and provider files must be single-link, server-owned, and not group/world writable; provider parents must also be protected, writable non-sticky ancestors are rejected, and socket/audit/checkpoint/lock parents must be owner-only. The checkpoint has a dedicated single-writer lock, rejects symlinks and hard links, and uses synchronized temporary-file replacement plus parent-directory synchronization. Because mode `0600` makes the socket one UID trust domain, every process under that UID can use its configured actor—use a dedicated UID when this matters.

The same executable has a separate provider-manager operator mode. Exact-reference `sync` is the only path that resolves a mutable tag; an unchanged desired reference preserves its existing manifest digest, while `sync --locked` can fetch a missing component directly by the locked layer digest without requesting the tag manifest. Registry manifests, token/error bodies, and component streams are independently bounded and timed. Components are staged and synchronized under their digest before complete provider-set validation; only the generated lock is activated atomically, so an orphan blob is possible and a partially validated active set is not. Installed, orphan, and stale-temporary files share a hard 4 GiB/1,024-file lifetime ceiling checked under the store lock; the absence of automatic prune can require explicit operator cleanup at the ceiling. `list`, `verify`, and daemon startup make no network request. Public anonymous OCI Bearer flow is current; registry credentials, custom certificate roots, SemVer ranges, update/remove/prune, and vulnerability/revocation response are not.

The service performs no process attestation, independent remote/signed checkpoint anchoring, or non-Unix network transport. It injects legacy destination-bound credentials from an owner-only `0600` credentials file. It also supports an owner-only private secret map: public DRNs remain untrusted proposal names until an ordinary capability decision, a separate exact `secret.use` decision, and a private binding all allow; only then does the broker resolve one source snapshot and construct native Basic/Bearer material. Both paths enter a request only inside the native HTTP engine after guest headers were validated — a guest-supplied `authorization` header remains rejected, never overwritten. A constraint set may name one default credential and per-agent overrides of it; every credential the set can select is proved against the store and against that set's allowed hosts at startup, so an override is exactly as validated as the default. Evidence, audit, spans, accounting logs, and public results record only that injection happened (`credentialInjected`) and, in the terminal audit record and the execution span, the credential's owner-authored symbolic name — never the value, and redaction plus destination binding are independently tested. Its checkpoint is durable and externally inspectable, but locally rolling back or deleting both the audit and checkpoint can defeat comparison unless checkpoint generations are independently retained. Its presence does not expand direct `dekopon-run`: immediate subcommands retain the separate empty linker and reject HTTP-importing fixtures. Explicit broker subcommands load no component, require a trusted server UID, validate socket metadata/peer credentials, and send proposals with no identity, policy, constraint, credential value, or authorization field; broker-backed shell calls may carry only the optional inert DRN/sink proposal. Their normal dependency path stops at the lightweight protocol/provider-metadata crates; CI rejects privileged broker, native-HTTP, or broker-service dependencies in the runner binary.

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

Two limits are worth stating plainly. **Legacy capability/per-agent credential policy still cannot
bind a provider-input path**, because arbitrary provider JSON is absent from policy context. A
credential per agent narrows *who may use a token*, not *what that token can touch*. A DRN binding
can additionally constrain the native HTTP path on which that one secret is injected, but it does
not interpret repository/object identity in bodies or upstream API semantics. The token's own scope
at the provider remains the final boundary. And **the single-UID caveat is
unchanged** — separating two organizations' tokens by agent is attribution and blast-radius shape
until the gateway runs under its own UID, because any process under the owner's UID can already act
as the configured peer.

## Threat-model limitations

The current project does not defend against a malicious process in the broker/client UID trust domain (including one forging or suppressing informational UI reports), a local user who can replace the binary, component, provider lock/store, or owner-controlled config; a compromised host; dependency or compiler compromise; denial of service during component compilation or from adversarial model endpoints; coordinated rollback of both local audit and checkpoint state; or side channels. The Wasmtime limits reduce invocation risk but are not a production sandbox claim. Prompt injection is explicitly not defended against: a chat message reaching `dekopond` can say anything to a model, and the containment is that the model can only propose, never authorize. On a `oneShot` route an injected instruction dies with the message that read it, because each gateway message is one independent session; on a `persistent` route it stays in the prompt for the rest of that conversation instead, which lengthens its dwell time without changing that containment. The project has no provider provenance verification in its manager, provider registry credential/custom-root support, provider vulnerability/revocation automation, leased/dynamic secret lifecycle, workload-identity secret-source bootstraps, per-process/client attestation, dedicated gateway UID, independent audit checkpoint retention/signing service, external evidence store, key management, tenancy isolation, operator-CLI integration with the broker or the daemon, or incident-response automation. A digest proves byte identity rather than publisher identity; existing container staging therefore retains its independent GitHub attestation checks and is not replaced by manager output.

Conversation replay and optional durable on-demand chat memory are implemented, and both trust
surfaces are live limitations. Immediate in-memory grant invalidation still compares capability
*identifiers*. Durable `authority-bound` continuity instead commits effective capability metadata,
constraints, selected legacy symbolic credentials, effective DRN use bindings/private-map revision,
provider artifact bytes, host/storage ceilings, backend, and memory limits and rotates a non-reusing
random generation when that semantic surface changes.
`stable` explicitly keeps prior text reachable across those changes after fresh authorization.
Neither mechanism detects prompt injection or makes recalled text trusted authorization input.

The committed first privileged-provider design is documented in [`broker-http.md`](broker-http.md). It preserves the separate broker boundary, keeps direct `dekopon-run` execution import-free, and treats HTTP imports as structural requirements rather than authority.

Future releases must threat-model confused-deputy attacks, prompt injection, credential exfiltration, provider escalation, SSRF and DNS rebinding, redirect escapes, TOCTOU between authorization and execution, duplicate external effects, malicious Wasm components, resource exhaustion, forged identity envelopes, audit tampering, and cross-tenant data leaks before claiming production readiness.
