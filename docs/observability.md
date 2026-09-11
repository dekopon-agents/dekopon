# Observability

`dekopond` and `dekopon-brokerd` each export their own execution traces over OTLP, using either
gRPC or HTTP with protobuf payloads. Gateway coverage is a routed chat message, model turns, and
its bounded agent/script session. Broker coverage is component loading and decoded invocations
from mapped peers. Neither collects telemetry from Kubernetes nodes or other processes.

The two daemons export **independently**: the broker cannot observe gateway model or script spans.
Their records meet in the backend, correlated by trace context. One complete trace per message is
the goal this document serves; [the constitution](design.md#constitution) states it.

## Which signal carries what

| Signal | Question | Lifetime |
|---|---|---|
| **Broker audit** | What was *authorized*? | Append-only JSONL, owner-only |
| **Traces** | What did the code do, and how long did it take? | Sampled; expires with trace retention |
| **Logs** | What could not be a span, and what must outlive one? | Retained independently |

*Committed direction:* opt-in sink, off by default; audit is a log record in the trace
([non-goals](design.md#non-goals)).

**A span is a span of time; a log is a fact that is not a duration.** A log event justifies itself
as a payload — text too large or too unbounded for a span attribute, the model and tool transcript
below — or as a survivor: costs, refusals, and errors, which outlive trace retention and sampling.
An `X.started` event is neither, because the span's start time is strictly better and carries
parent and duration besides.

## Accounting

Two events record calls that cost money or consume a rate limit:

| Event | Emitted by | Carries |
|---|---|---|
| `accounting.model.turn` | `dekopond` | turn index, duration, message and tool-call counts, token usage, outcome |
| `accounting.http.request` | `dekopon-http-host` | method, authority, status, accounted request/response bytes, outcome, and `error.code`/`error.message` on failure |

Both duplicate span fields: the span answers "why was this request slow", the accounting record
answers "how many did we make last month". Neither substitutes for broker audit, the record of what
was authorized.

Whatever the model provider reports for usage lands as `usage.input_tokens`,
`usage.cached_input_tokens`, `usage.output_tokens`, `usage.reasoning_output_tokens`, and
`usage.total_tokens` — normalized across the chat-completions and Codex Responses wire shapes — on
both the `prompt.model_turn` span and the `accounting.model.turn` record.

`accounting.http.request` carries the same sanitized set as its span and as `HttpCallEvidence`: no
URL path or query, no headers, no bodies. It fires for every attempt, including one refused before
a destination was resolved, because that attempt consumed a unit of the invocation's request
budget. A field a record cannot know — a provider's unreported token count, or a method, authority,
and status on a refusal — is absent rather than zero, so a missing field means "unreported" and
grouping by status never invents a phantom `0`.

The byte counts are `dekopon.http.request.accounted_bytes` and
`dekopon.http.response.accounted_bytes`, and not the OTel `http.*.body.size` names: the host
accounts a conservative envelope covering encoding overhead, method, URL, and headers as well as
the body, so the payload-size name would misreport transfer volume by the size of the headers.

## Refusals, errors, and outcomes

The other half of what survives trace expiry is what a process refused or could not do. These fire
in either payload mode, because each carries a fixed category rather than the untrusted text that
triggered it:

| Event | Emitted by | Carries |
|---|---|---|
| `agent.tool.rejected` | `dekopon-agent` | model turn, the tool-call index or count, and a fixed `error.type` such as `too-many-tool-calls` or `unknown-tool` — never the model's own tool name or arguments |
| `agent.provider_attachment.refused` | `dekopon-agent` | a stable `reason` — `route-disabled`, `invalid-encoding`, `unsupported-media`, `too-large`, or `per-reply-limit`; never the attachment bytes, its declared media type, or any provider text |
| `agent.chat_asset_input.refused` | `dekopon-agent` | a stable `reason` — `unknown-asset`, `unsupported-media`, `per-invocation-limit`, `session-limit`, `byte-budget`, or `unavailable`; never the attachment number, its bytes, or the sender's file name |
| `agent.asset.refused` | `dekopon-agent` | the gateway-assigned asset id and the gateway-authored refusal text the model reads back |
| `agent.asset.fetched` | `dekopon-agent` | the asset id, its media type, its byte count, and `asset.truncated` — whether a textual asset larger than the prompt's textual bound was clamped with a trailer the model reads rather than dropped or failed; never the bytes and never the sender's file name, which is untrusted text |
| `agent.skill.read` | `dekopon-agent` | model turn, tool-call index, the operator-authored `skill.name` the request matched, `skill.resource` (the resource path; empty for the skill's own instructions), `skill.bytes` of the tool result, and `skill.repeated` — `true` when that text was already in the conversation and a one-line pointer was returned instead; never the skill text and never the name the model typed |
| `agent.skill.refused` | `dekopon-agent` | model turn, tool-call index, and a stable `reason` — `unknown-skill` or `unknown-resource`; the refusal that lists what *is* mounted goes to the model as a tool result, not here |
| `agent.improvement.suggested` | `dekopon-agent` | model turn, tool-call index, `suggestion.index` (1 to 3), the enum tokens `suggestion.category` and `suggestion.confidence`, and the model-authored `suggestion.target`, `suggestion.summary`, `suggestion.evidence`, and `suggestion.proposal`, bounded to 128, 512, 2048, and 2048 bytes — see below |
| `agent.improvement.refused` | `dekopon-agent` | model turn, tool-call index, and a stable `reason` — `invalid-category`, `invalid-confidence`, `empty-field`, `field-too-long`, or `session-limit`; none of the submitted text |
| `policy.name.unresolved` | `dekopon-brokerd` | policy id, name kind, and the action or provider name no loaded provider declares, so a rule that can never match is visible at startup |
| `config.startup.warning` | `dekopon-brokerd` | the capability id and a stable `reason` — `unrouted-constraint-set` or `unconstrained-capability` |
| `command.resolve.failed` | `dekopon-brokerd` | the provider-declared command word, a stable `error.kind`, and the host error's chain, recorded when running the word (`runCommand`, or the legacy `resolveCommand`) fails rather than declines: no provider declares it, the argv plus piped value exceeded `maxInputBytes`, the guest trapped or reached for an import, or its answer would not decode |
| `policy.request.refused` | `dekopon-broker` | the capability id and a rendered `error.reason` for a Cedar request the policy schema could not admit — the caller sees plain `policy-denied` |
| `agent.command.unobserved` | `dekopon-agent` | `command.leg` (`broker` or `direct`), a low-cardinality `outcome` (`succeeded`, `operation-error`, `cancelled`, or `task-failed`), and a fixed `error.type` (`none`, the leg's own error kind, `task-cancelled`, or `task-panicked`), recorded when a command-word run's caller was dropped while its process node was joined; never the word, the argv, the piped value, or the text a provider rendered, and the failure's complete cause goes out as an ordinary error event at the same site rather than into this record |

`agent.improvement.suggested` is the exception to that sentence. Its four free-text fields are
model-authored — bounded and stripped of control characters other than newline and tab, never
reduced to a category — and they are recorded whether or not payloads are on, because a suggestion
nobody can read is not a suggestion. That is why `suggest_improvement` is offered only when the
embedder opted in, for example through `improvementSuggestions: true` on a `dekopond` route.
Enabling it is the consent that declares the log sink in scope for that text, and nothing else
widens with it: the record carries no chat text the gateway holds and no subject, only what the
model wrote into those fields.

An event name is part of this contract: CI fails a pull request that emits an `audit.event` name
this file does not mention, so a rename lands here in the same change.

## Enable OTLP export

Export is disabled unless an endpoint is configured in the daemon's `telemetry` block:

```yaml
telemetry:
  endpoint: http://127.0.0.1:5080/api/default
  transport: http
  serviceName: dekopond
  exportTimeoutMs: 5000
```

Supply authentication only through the standard header environment variables:

```console
export OTEL_EXPORTER_OTLP_HEADERS='Authorization=Basic%20<INGESTION_TOKEN>,organization=default,stream-name=dekopon'
dekopond --config gateway.yaml
```

Both transports are first-class. `http` treats the endpoint as a generic OTLP/HTTP base and appends
`/v1/traces` and `/v1/logs`. `grpc` treats it as an authority and takes its method paths from the
OTLP protobuf service definition — those paths are fixed by the protocol, so a path-routing reverse
proxy matches `/opentelemetry.proto.collector.*` rather than a path of the operator's choosing.

Both read `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_EXPORTER_OTLP_TRACES_HEADERS`, and
`OTEL_EXPORTER_OTLP_LOGS_HEADERS` directly through the exporter, in the OpenTelemetry URL-encoded
form where `%20` is the space in `Basic <token>`. There is no header CLI flag and no header
configuration field, because credentials must not be exposed in process arguments, retained in a
parsed CLI value, or written into a configuration file. Endpoint URL userinfo
(`https://user:password@host`) is rejected for the same reason.

The example header set works unchanged on both transports. On OpenObserve (observed on v0.92.0)
`stream-name` selects the stream for traces and logs alike. The organization is the one asymmetry:
HTTP reads it from the endpoint path (`/api/default`) and ignores the header, while gRPC has no
path to carry it and requires the `organization` header — without it the receiver rejects every
export with gRPC status `InvalidArgument`. Keep `organization=<org>` in the header set
unconditionally; over HTTP it is redundant, never harmful.

Standard `OTEL_RESOURCE_ATTRIBUTES` values are attached to both signals, alongside a
`service.version` carrying the exporting executable's own version. HTTPS endpoints use WebPKI roots
on both transports, and redirects are disabled so a receiver cannot forward an authorization header
to another destination. Plain HTTP suits a loopback development receiver or a trusted isolated
network only, because headers and telemetry are unencrypted.

## Export failures

The OpenTelemetry SDK has no error handler to install; its `internal-logs` feature is the only
runtime channel it reports export failures through, and the workspace enables that feature for the
API, the SDK, and the OTLP exporter. Rejected tokens, a missing `organization` header, and a
receiver that is down therefore say so, as structured JSON on stdout. `dekopon-telemetry` filters
the `opentelemetry` target off every OTLP layer it installs, whatever crate directive the binary
supplied, so an export failure can never be re-exported through the exporter that failed.

### Daemon exit and shutdown records

Both daemons report the same two facts on the way out, in the same shape:

| Event | Level | Emitted by | Carries |
|---|---|---|---|
| `gateway_exit` / `broker_exit` | error | `dekopond` / `dekopon-brokerd` | `error`: the failure and its whole source chain, rendered as one `a: b: c` line |
| `gateway_telemetry_shutdown_failed` / `broker_telemetry_shutdown_failed` | error | `dekopond` / `dekopon-brokerd` | `error`: every flush and shutdown failure raised while stopping the exporters, each naming its signal and stage |

## Broker export

`dekopon-brokerd` exports through an optional `telemetry` section in its owner-controlled
configuration. The section is absent by default, and when present every field is required, matching
every other section in that file:

```yaml
telemetry:
  endpoint: http://rpi.localdomain
  transport: grpc            # grpc | http
  serviceName: dekopon-brokerd
  exportTimeoutMs: 5000
```

There is no credential field. The broker reads `OTEL_EXPORTER_OTLP_HEADERS` like the gateway does,
so a token never enters the configuration file the broker parses, its command line, or any span
attribute. With `transport: grpc` the endpoint names no organization, so the header set must carry
`organization=<org>`.

Telemetry never blocks startup. An exporter that cannot be built disables export and logs why;
authorization and append-only audit are the service's contract, and a missing exporter must not
cost a working authority boundary. Flush failures at shutdown are logged and do not change the exit
code.

The broker's log output is structured JSON on stdout, filtered by `RUST_LOG` and defaulting to
`info`. Shipping those logs to storage is left to whatever reads stdout, so the broker holds one
credential rather than two. Both daemon JSON formatters preserve existing fields and attach native
`trace_id` and `span_id` only when the active OpenTelemetry context is valid; outside that context,
including disabled trace export, neither ID is fabricated.

## Trace context across the socket

`InvocationRequest` and `DeliveredTurnRequest` each carry a mandatory W3C `traceParent`. The shared
broker leg fills it from the span that requested the capability, and the broker opens
`broker.invocation` beneath it as a remote parent, so one trace spans both processes. The trace
identifier inside it is the only correlation identifier the system has: the broker writes it into
every audit record it appends and onto every span it opens for the call, and the invocation
identifier extends it (`<trace>-<counter>`), so one session's calls are recoverable by prefix.

It is mandatory because a record with no correlation identifier is the inverse of what the audit
log is for. A client that exports no telemetry still sends one: `tracing-opentelemetry` attaches
its layer only when an OTLP trace exporter is configured, so a non-exporting process has no span
context to read at all — absent rather than invalid — and mints a session-local trace from OS
entropy instead of omitting the field. Nothing off-box receives that minted trace; it still ties
one session's audit records to each other. A minted context carries `sampled=1`, which is an
instruction to the receiver rather than a claim about the sender: the broker adopts it as a remote
parent, no Dekopon process configures a sampler, and the OpenTelemetry SDK's default
`ParentBased(AlwaysOn)` makes every span beneath an unsampled parent non-recording — so a cleared
bit would silence an exporting broker sitting behind a non-exporting gateway.

`traceParent` is untrusted like every other request field. It reaches span parenting and audit
correlation and nothing else — never policy or routing. A malformed value, an
explicit `null`, and an omitted key are all decode failures, since attaching broker spans and audit
records to a trace that does not exist is worse than refusing the frame.

The broker span carries the invocation, capability, and trace identifiers. Provider input and output
ride the payload-gated `input` fields below; URL paths and queries, headers, and bodies stay out of
spans and audit alike.

An attested proposal adds routing fields to both spans: `broker.invocation` records the claimed
`subject` and `agent`, and `broker.authorize` records the `subject` and the `via` peer the broker
derived the context through — the same values the audit log keeps. All of them are canonical
identifiers (`slack.t0123abc.u9xyz`), never the chat message that prompted the invocation. A refusal
records the claimed subject and its `outcome` with no `via`, because no attested context was
derived.

## Gateway spans

`dekopond` opens a message's trace in the transport that received it and wraps the routed message in
two further spans of its own:

| Span | Fields |
|---|---|
| `transport.receive` | `transport.kind` (`slack`, `discord`, `telegram`, `whatsapp`, `local`), `message.id`; the trace root |
| `gateway.message` | `transport`, `agent`, `outcome` (`answered`, `declined`, `unauthorized`, `busy`, `failed`, `cancelled`, `reply-failed`) |
| `gateway.session` | `agent`, `conversation.turns`, `conversation.bytes`; wraps the broker leg and the model session |

`transport.receive` is one span per receipt, opened before the payload is parsed, so Slack's
envelope acknowledgment, WhatsApp's HMAC signature check and its 200, Telegram's `offset` advance,
Discord's loop-prevention and addressing decisions, and the local transport's line parse are all
inside the trace of the message they concern — as is routing itself, so a `gateway_message_ignored`
record says why inside the message's own trace. `gateway.message` nests under it and closes it, so
the receive span measures receipt and dispatch rather than the session it started.

`message.id` is the service's own identifier for the turn — a Slack `ts`, a Discord snowflake, a
Telegram `message_id`, a WhatsApp `wamid`, the development transport's boot-scoped counter — and is
recorded once the payload has been parsed far enough to carry one. A receipt that routes nothing
closes without it, which is the trace that answers "why did the bot not reply"; so does a refused
WhatsApp delivery. A WhatsApp delivery carrying more than one message leaves it unset and parents
every message's `gateway.message` under the one delivery. Neither the sender nor the message text
goes on this span: those stay on the `gateway.message.received` log event below.

| Event | Level | Fields |
|---|---|---|
| `gateway_reply_rate_limited` | warn | `transport=slack`, `retry_after_seconds`; no message text or credentials |

A `chat.postMessage` HTTP 429 delays the identical post once: integer `Retry-After` seconds are
capped at 60, defaulting to 5 when missing or unparsable. A second 429 uses the ordinary
reply-failure path; no other HTTP failure is retried.

The prompt loop's spans (`prompt.session`, `prompt.model_turn`, `prompt.script`, `shell.script`, `shell.command`) nest under `gateway.session`, and the broker's `broker.invocation` joins the same trace through the proposal's `traceParent` — so one trace reads from "a person asked something in Slack" to "a provider made an HTTP call". `prompt.asset_fetch` joins them whenever a model opens an attachment: one span per fetch, carrying the asset number the conversation referred to and the turn and tool-call index that asked for it, never the file's name or bytes. It is gateway-only, because only a gateway session offers the asset tool.

Neither gateway span carries chat text or a subject identifier. `outcome` is the whole answer at the
metadata level: `declined` means an optional owned-thread continuation produced no chat delivery,
`unauthorized` means the broker's chat-scoped `capabilities` returned nothing and no model or
activity call was made, `busy` means admission control refused the message, `cancelled` means an
authenticated native Stop won the race against terminal delivery, and `failed` names a category
through the `gateway_session_failed` log event. The sender's canonical subject and the message text
ride the `gateway.message.received` log event under the payload gate below. `agent.reply.declined`
records only the model-turn number. `unreported-capability-work` is a stable failure category whose
fixed chat warning directs the sender to audit before retrying.

Every transport reconnects on one jittered exponential backoff, and the jitter comes from the
operating system. `gateway_transport_jitter_unavailable` is the warn-level record of an OS that
refused entropy, carrying the `getrandom` failure and nothing else; that attempt's delay falls back
to its unjittered step, which costs a fleet its de-synchronization rather than its reconnect. Two
other records name the same refusal at their own sites: `session_trace_entropy_unavailable` (warn)
when a non-exporting session's minted trace falls back to the hasher construction, and
`gateway_cache_key_entropy_unavailable` (warn) when a prompt cache key is dropped rather than
minted from something predictable.

In-flight presentation is metadata-minimal. `gateway_activity_failed` is debug-level and carries
`operation` plus the stable transport-error category. A permanent Slack installation fallback emits
`gateway_activity_degraded` with `transport=slack` and `surface` (`agent-status` or `reaction`).
`gateway_session_stop_requested` carries the transport. None records channel, thread, message,
subject, status text, emoji, raw service response, or credential.

### The WhatsApp webhook is the one signal a stranger can drive

Every other transport's volume is bounded by a service the daemon dialed. The WhatsApp callback is
public, so an unauthenticated caller decides how many refusals happen, and this sink is a 30-day
retention claim rather than an infinite one. Refusals are therefore rate-limited rather than
per-request, and they are the only WhatsApp events at `info` or above:

| Event | Level | Fields |
|---|---|---|
| `gateway_whatsapp_webhook_refused` | warn | `transport`, `reason` (`unsigned`, `signature`, `oversize`, `malformed`, `saturated`, `timeout`, `verification`, `unavailable`), `status`, `suppressed` |
| `gateway_whatsapp_accept_failed` | debug for `kind=connection`, warn for `kind=exhausted` | `transport`, `kind`, `error` |
| `gateway_whatsapp_listener_stopped` | error | `transport`, `error` |
| `gateway_whatsapp_reply_partial` | warn | `category`, `delivered` |

`suppressed` is the count this line stands for: each reason is emitted at most once a minute, and
the next emission carries how many refusals were folded into the gap. A misconfigured app secret is
one `reason=signature` line a minute rather than one per delivery attempt, so reading the rate means
reading `suppressed` rather than counting lines. `error` on the accept and listener events is the
operating system's message for a socket call — never a request, a body, or a sender. None of these
carries a phone number, a WABA identifier, a message ID, or message text.

### What conversation history changes

A route set to `mode: persistent` — the contract is in [`dekopond.md`](dekopond.md#conversations) —
changes the meaning of a field that already exists. A route left on the `oneShot` default changes
nothing here.

**`message.count` is the field.** It appears on the `model.complete` span and on the
`accounting.model.turn` record, and it counts one exchange: the system prompt, the message a person
sent, and whatever the model and its tool have said back within this session. A session seeded with
history counts the replayed window *plus* this exchange, so the same field on the same span means
something different depending on a route's `conversation.mode`. A panel plotting it across the
switchover shows a step change that is not a regression, and averaging across both averages two
different quantities. `usage.input_tokens` rises for the same reason, and for real money.

**The history size gets its own fields.** `gateway.session` carries `conversation.turns` and
`conversation.bytes` — how many prior exchanges this message replayed and how many bytes they
occupied; on `sharedConversation` the byte count includes each retained gateway-authored
canonical-participant label. Both are zero on a `oneShot` route and on the first message of any
conversation, which makes "seeded or not" a filter rather than a guess.
`gateway_conversation_evicted` carries a reason of `idle`, `capacity`, or `grant-changed` and
nothing else, so a `maxConversations` ceiling set too low reads as eviction churn instead of as a
bot that intermittently forgets. A conversation key carries a conversation identifier and, when
private, a canonical subject; the key types do not implement `Debug`, so an incidental `?key` cannot
put either into the eviction record.

The history itself is not a new signal. It is chat text and model output, and it goes where those
already go: the session's first `agent.model.prompt` carries its opening message list, which on a
seeded session includes the replayed window. Shared scope adds canonical participant identifiers to
those prompt events, and not to spans, eviction events, or cache key events. Those labels are part
of every shared prompt sent to the selected model provider independently of telemetry.

### Reading the prompt cache

Every model request `dekopond` makes declares a `prompt_cache_key` — one per conversation on a
`persistent` route, one per bound route on a `oneShot` one.
[`dekopond.md`](dekopond.md#the-prompt-cache-key) has the contract; two things follow for telemetry.

**`usage.cached_input_tokens` is how you find out whether it works.** Plot its ratio to
`usage.input_tokens` on a conversation's second and later turns, the requests that repeat a prefix
worth caching. Do not expect a standing discount: provider prompt caches clear after minutes of
inactivity, so a gateway answering a message every few hours misses on the first turn of every
conversation regardless of the key, and what the key buys is the burst inside a live conversation. A
window trim rewrites the front of the request and costs a miss by construction, so a run of misses
on long conversations is `maxTurns` or `maxBytes` doing its job rather than a broken key.

**The key rides its own log event.** It lands on `gateway.session.cache_key`, never a span
attribute. It carries nothing about the audience by construction, but within one process it joins
one private or shared conversation's turns; it is emitted on its own event so that a key and a
canonical subject never share a record, which keeps a reader who needs only one of them from seeing
both.

## Broker execution spans

`broker.invocation` is not a flat bar. Beneath it the broker's own crates emit the spans below.
The symbolic `credential` fields describe current `credential`/`credentialByAgent` selection.
*Committed direction:* those bindings will be replaced by public DRNs, preserving refresh
observability ([migration requirements](design.md#legacy-credential-bindings)); no telemetry field
migration is implemented here.

| Span | Crate | Fields |
|---|---|---|
| `provider.compile` | `dekopon-broker-host` | `path`, `artifact_bytes`, `elapsed_ms`; emitted once per provider at startup |
| `provider.describe` | `dekopon-broker-host` | `path`, `stores`, `instantiations`, `fuel.consumed`; emitted once per provider at startup, for the manifest call |
| `provider.run_command` | `dekopon-broker-host` | provider, `word`, `command.export` (`run-command`, or the legacy `resolve-command`), `stores`, `instantiations`, `fuel.consumed` |
| `broker.authorize` | `dekopon-broker` | invocation, capability, `outcome` (`allowed`, `policy-denied`, `policy-error`, `secret-denied`, `unconstrained-capability`, `agent-denied`, `attestation-denied`, `unmapped-subject`, `chat-attestation-denied`, `chat-scope-required`, `record-operation-required`, `memory-unavailable`, `invalid-memory-input`, `invalid-turn`), `policy.errors_present`; `subject` and `via` on attested proposals |
| `broker.execute` | `dekopon-broker` | provider; `credential` — the symbolic name the invocation selected, when it selected one; `outcome` (`succeeded`, `failed`, `decision-unaudited`, `outcome-unaudited`) and `error` — the same classified reason the terminal audit record carries |
| `broker.credential.refresh` | `dekopon-brokerd` | the symbolic `credential` name, and `outcome` (`current`, `adopted`, `rotated`, `rotated-unsaved`, `failed`); emitted once per invocation that selects a credential the broker renews per use, and never any token, account identifier, or file content. `chatgpt.refresh` from `dekopon-model` nests inside it |
| `provider.invoke` | `dekopon-broker-host` | capability, provider, `stores`, `instantiations`, `fuel.consumed` |
| `http.request` | `dekopon-http-host` | `http.request.method`, `server.address`, `http.response.status_code`, `dekopon.http.request.accounted_bytes`, `dekopon.http.response.accounted_bytes`, `outcome`; `error.code` and `error.message` on failure |

`http.request` fields mirror `HttpCallEvidence` exactly: the span reports the same call the audit
log records, so URL paths and queries, request and response headers, and both bodies are absent
here for the same reason they are absent from evidence. A test in `dekopon-http-host` drives a real
loopback request whose path, query, header, and body are each a distinct sentinel and asserts that
none of them reach a span field.

`error.code` and `error.message` are the exception that keeps `outcome` honest: `outcome` collapses
DNS, connect, TLS, timeout, protocol, and setup failures into `failed`, which cannot separate a
webpki root problem from a LAN DNS blip from an expired deadline. Recording the reason is safe by
construction rather than by review — every message `dekopon-http-host` produces is a static,
pre-sanitized `&str`, and nothing in that crate may start interpolating a URL, header, or body into
one. The span is attached with `Instrument` rather than an entered guard, so a request awaiting DNS,
a connection, or a response body never re-parents whatever else the runtime polls on that worker
thread.

`provider.compile` covers component-set validation rather than per-invocation work, so it answers
"why was the broker slow to become ready" rather than "why was that call slow". Components compile
concurrently, so their spans overlap and the compile times sum to more than the wall-clock
validation. Each loaded provider also emits one info event carrying its identity, artifact digest
prefix, artifact bytes, compile milliseconds, its capability and command-word counts, and
`command_export` — `run-command`, `resolve-command`, or `none` — naming which export the host calls
for its words. The offline `dekopon-brokerd provider sync` and `verify` commands reuse the same host
validation and can emit the span to their stderr subscriber, but they install no OTLP exporter.

`stores` and `instantiations` are on all three guest-executing spans because the host resolves each
provider's imports into one `InstancePre` at load: every description, command run, and invocation
then builds exactly one fresh store and instantiates the component in it exactly once. Both read `1`
on a healthy operation. A second instantiation under one span is a call path that started rebuilding
instances per call — a regression with no other symptom than latency — and an operation that
recorded neither field was refused before a store existed, which is what the input-size and
aggregate-memory refusals look like from the outside.

`fuel.consumed` joins them on the same three spans: the Wasm instructions the guest actually burned,
read back from the store when the operation ends as the supplied ceiling minus what Wasmtime says
remains. It is how much of `DEFAULT_FUEL` a call spent, so a component approaching the ceiling is
visible before it starts trapping, and a trap is separable from a wall-clock timeout by whether the
reading arrived at the bound. It is recorded on every path a store can end on — success, trap,
rejection, and timeout alike — and a store that reports no reading records nothing rather than a
zero that would read as a component that ran for free.

All three are counts of host work, never provider content, so the storage-backed `provider.invoke`
span carries them too.

`provider.run_command` carries the provider, the command word, and the export name that served it,
never the argv or the value piped into the word. Model-authored argv and piped text are untrusted
content for the same reason `provider.invoke` omits `input`; the help page or usage error a
`run-command` guest renders travels back in the result, not in telemetry.

A `policy-denied` outcome the policy engine never evaluated additionally emits
`audit.event = "policy.request.refused"` at `WARN` with the capability and a rendered reason. The
wire result and the audit reason both stay `policy-denied`, because the taxonomy callers act on must
not shift; this event is the only place an operator learns the denial came from a request the schema
does not admit — a deployment defect — rather than from a policy that considered it and said no.

`broker.credential.refresh` exists because the renewal is work the invocation waited on that nothing
else in the trace accounts for: it is the broker's own HTTPS call, so it is absent from
`HttpCallEvidence`, from the guest's `maxRequests`, and from `accounting.http.request`, and without
this span a capability that spent a second on a token endpoint would look like a slow component. Its
`outcome` separates the four things that can happen to a rotating credential — the stored token was
still good (`current`), another process had already rotated and this one spent nothing (`adopted`),
this one rotated and persisted (`rotated`), or it rotated and could not persist, so the returned token
is the only copy that works (`rotated-unsaved`). `rotated-unsaved` is the one an operator should alert
on alongside `chatgpt_credential_save_failed`: the record on disk is the retired predecessor.

`broker.execute`'s `credential` is the owner-authored symbolic name from `broker.yaml`, never the
secret and never the header. One capability can present a different credential per acting agent, and
a trace that named none of them would make two writes to two different organizations look identical.
A `Redacted` value renders its marker in either payload mode. The legacy selection bindings
[will be replaced by public DRNs](design.md#legacy-credential-bindings); the current symbolic field
is not itself a DRN.

`broker.authorize`'s `policy.errors_present` is Cedar's evaluation-error flag, and `policy-error` is
the `outcome` and audit reason it produces. A policy that errors while deciding — an extension call
on a malformed value, say — denies exactly like a policy that does not match, so without this pair a
broken rule and a clean no-match are the same record. It stays a flag rather than the error text: an
explanation must not become a per-request channel for policy source or entity data.

## Broker failure events

Every broker failure answers its caller with a generic wire code by design, so the log line is the
only place the cause exists. These events carry it. Their symbolic `credential` names currently
refer to legacy bindings that [will be replaced by public DRNs](design.md#legacy-credential-bindings);
the refresh failure classes remain part of the migration contract.

| Event | Level | Emitted by | Carries |
|---|---|---|---|
| `broker_capabilities_refused` | warn | `dekopon-broker` | `reason` (`attestation-denied`, `unmapped-subject`, `agent-denied`, `policy-error`), `policy_ids` (the policies that determined it, empty for a refusal reached before any evaluation), canonical `subject`, `agent`, `via` |
| `broker_policy_evaluation_error` | warn | `dekopon-broker` | `invocation`, `policy.target` (`capability` or `secret`) |
| `broker_secret_resolution_failed` / `broker_secret_credential_failed` | warn | `dekopon-broker` | `invocation` and low-cardinality source/material `category`; structural credential errors are fixed value-free text. No DRN, locator, revision, value, or value-derived length. |
| `broker_credential_refresh_failed` | error when `retryable = false`, warn otherwise | `dekopon-broker` | `invocation`, the symbolic `credential` name, low-cardinality `category`, and `retryable`. The invocation's classified reason is `credential-unavailable` when permanent and `credential-refresh-failed` when not; the broker keeps serving every other capability either way. |
| `broker_chatgpt_credential_reauth_required` | error | `dekopon-brokerd` | the symbolic `credential` name, the `authFile` path, `category`, and the refresh failure's source chain. Emitted when the OAuth `error` code says the refresh-token family is retired (`invalid_grant`, `refresh_token_reused`, `refresh_token_invalidated`, `refresh_token_expired`) or the local credential is unusable: a human must run `dekopond auth chatgpt login --auth-file` again. No token, account identifier, or document content. |
| `broker_chatgpt_credential_refresh_failed` | warn | `dekopon-brokerd` | the symbolic `credential` name, low-cardinality `category` (`transport`, `token-endpoint-unavailable`, `token-endpoint-rejected`, `token-endpoint-protocol`, `invalid-material`, `refresh-task`), and the failure's source chain. The next invocation may succeed unchanged. |
| `broker_chatgpt_credential_loaded` | info | `dekopon-brokerd` | once per `chatgptSubscription` credential at startup: the symbolic `credential` name, the `authFile` path, `expires_at`, and `expired`. It is how an operator learns a seeded credential is already stale before the first invocation discovers it. |
| `secret_source_resolution_failed` / `secret_projection_failed` | warn | `dekopon-brokerd` | adapter `source_kind` and low-cardinality `category`; no DRN, locator, response body, bootstrap credential, selector, or value |
| `secret_source_cause_classified` / `secret_source_configuration_cause` | debug | `dekopon-brokerd` | safe cause classification behind the stable warn category: I/O kind/errno, HTTP timeout/connect/status, JSON class/line/column, the file-hygiene check name with the errno underneath it, or dependency-error type; URL parsing uses its fixed parser reason. Never endpoint/locator, refused path, or secret-derived bytes/offsets. |
| `broker_audit_append_failed` | error | `dekopon-broker` | `audit.stage` (`decision`, `authorized-failure`, `outcome`), `category` (`full`, `poisoned`, `record-too-large`, `sequence-overflow`, `serialize`, `io`), `invocation`, and the error's source chain |
| `broker_request_frame_invalid` | warn | `dekopon-brokerd` | `error.kind` (`timeout`, `io`, `empty-frame`, `frame-too-large`, `deserialize`, …) and the bounded protocol message |
| `broker_connection_failed` / `broker_outcome_unaudited` | warn / error | `dekopon-brokerd` | `category`, the failure's source chain, and — for an unaudited outcome — `invocation.id` |
| `broker_capacity_exhausted` | error | `dekopon-brokerd` | `category`, and the chain naming which bound was reached |
| `broker_accept_retried` | warn | `dekopon-brokerd` | `error.kind` (`process-descriptor-limit`, `system-descriptor-limit`, `kernel-memory`, `connection-aborted`, `connection-reset`, `interrupted`), `backoff_ms`, and the errno's chain |
| `broker_socket_cleanup_failed` | warn | `dekopon-brokerd` | the socket error's chain |
| `broker_peer_unmapped` | warn | `dekopon-brokerd` | `peer.uid`, the UID the refused connection authenticated as |

`broker_capabilities_refused` exists because an attested `capabilities` and an attested `runCommand`
(or legacy `resolveCommand`) answer a refused caller with the same opaque nothing whatever went
wrong — a distinguishable answer would tell an unauthorized gateway whether a subject is mapped, and
an unknown command word would disclose the surface the refusal withheld. The class, its determining
policies, and the canonical subject land on the broker's own side of the socket, which is what makes
bootstrapping an `identityMapping` for a new sender possible without reading the subject out of a
gateway span. It marks refusals, not traffic: an honored session emits nothing.

A chat-scoped `invoke` and `recordDeliveredTurn` withhold the same fact for the same reason, but
they are accounted decisions rather than unanswered inspections, so the peer receives a `Denied`
result whose reason is the one fixed literal `chat-attestation-denied` whatever the claim failed on.
`broker.authorize`'s `outcome` and the durable decision record keep the real class and its
`policy_ids`. A subject-only attested proposal answers with its own class; no chat transport takes
that path.

The source chain is the diagnosable half. `ConnectionError::Broker` renders as "broker failed" and
`AuditError::Io` as "durable audit append failed" (an error label, not a crash-durability
guarantee); the errno that says *why* — `ENOSPC` on an audit filesystem shared with anything else —
lives one or two levels down, and these events render the whole chain as one `a: b: c` line. Frame
contents never join it: a decode failure names its kind, not the bytes that failed to decode.

`broker_capacity_exhausted` and `broker_accept_retried` report a condition outside any one request.
The first says a bounded broker resource — an embedding's in-memory audit log — is full and does
not evict; every caller receives `capacity-exhausted` and no
retry clears it within that process lifetime. The durable file audit is not one of those bounds: it
bounds each record, not their number, so a full audit filesystem arrives as
`broker_audit_append_failed` with `category=io` — and, once execution has begun, as
`broker_outcome_unaudited` — rather than here. The second says the daemon survived an `accept`
failure; a steady stream of it at `error.kind=process-descriptor-limit` is a descriptor leak the
daemon absorbs silently, which is what makes it worth alerting on.

`broker_socket_cleanup_failed` is reported but does not preempt the shutdown result. A stale socket
path is a smaller problem than the failure that ended service, so the serve error and
`broker_stopped` come first.

`broker_peer_unmapped` names the UID behind an `unauthenticated` refusal, which the wire answer
withholds. It is the event to look for when a deployed broker never becomes ready: `dekopon-brokerd
probe` is an ordinary client authenticating as the broker's own UID, so a configuration whose
`identities` omit that UID refuses its own health check. The Helm chart refuses that combination at
render time; this event names it everywhere else.

## Storage telemetry and audit privacy

Storage-backed invocations do not follow the ordinary provider span shape. The `broker.execute` and
`provider.invoke` storage spans omit provider, capability, agent, subject, transport scope, logical
names, offsets, search terms, and exact bytes even when payload telemetry is enabled; `stores` and
`instantiations` are the exception, and they count host work rather than describing the call.
Storage evidence retains only invocation/operation/sync/quota counts and the largest powers-of-two
read/write bucket; that evidence and the public ceilings never contain root/key paths or opaque
tokens.

Storage audit decisions and outcomes omit principal, actor/agent, via/subject, provider, broker
principal/policy revision, policy IDs/digest, and credential. A separate keyed audit-scope
commitment is never equal to a physical namespace token. Storage decision/output/evidence values use
separate `hmac-sha256:` domains, which removes the unkeyed low-entropy dictionary oracle.
Non-storage records use `sha256:`.

A retained storage document that fails to decode emits `storage_document_decode_failed` at `WARN`
under `category = "storage"`. It carries the static document kind plus the `serde_json` failure's
class, line, and column: enough to separate a truncated write from an unknown or wrongly typed field
without exporting a logical name, a path, an opaque token, or any document content. The rejected
bytes are never echoed.

Entropy and wall/monotonic clock values from durable-files are never emitted as telemetry. A native
filesystem operation may outlive a timeout signal; `finalizationBudgetMs` prevents the next bounded
finalization step from starting after its deadline, while the base/generation leases and quota
reservation remain held until an already-started blocking job drains. Duration is therefore
observation rather than a hard native-operation deadline.

## Span payloads

Payloads are always recorded. There is no metadata-only mode and no `telemetryPayloads` key: a
telemetry store an operator can read is a store that can reconstruct the run, and a mode that
withholds half of it serves nobody the constitution recognizes ([goal
2](design.md#constitution)).

| Span | Payload field |
|---|---|
| `broker.authorize` | `input` — the untrusted proposal payload |
| `provider.invoke` | `input` — the payload passed to the component |
| `http.request` | `url.full` — the destination with its path and query |
| model/tool log events | the verbatim transcript; see below |

This is **data**, not credentials. Request and response headers and HTTP bodies stay out — a
credential is injected into a header at the native HTTP boundary — and a `Redacted` value renders
its marker wherever it is formatted, because that is a property of the value. Both exclusions are
unconditional and neither ever had a switch. Append-only audit records carry their own metadata-only
shape, unchanged by any of this.

A storage-backed `broker.authorize` and `provider.invoke` still open the blind span: identifiers and
the decision only. *Committed direction:* that arm records its input like every other one.

## Model and tool transcript

The verbatim exchange between the model and its tools rides the **log stream**, not span attributes.
A conversation is unbounded text: span attributes are the wrong container for it, every trace fetch
would drag the payload along, and a backend indexes log bodies for full-text search rather than span
fields. Both signals carry the same `trace_id` and `span_id`, so a log result pivots to the turn it
belongs to.

These events join the accounting and refusal ones:

| Event | Carries |
|---|---|
| `agent.model.prompt` | The session's opening message list on its first turn, and on every later turn only the messages appended since the previous one |
| `agent.model.answer` | Assistant text and the tool calls it requested, with arguments |
| `agent.tool.script` | The script the model authored |
| `agent.tool.output` | That script's combined output |
| `gateway.message.received` | The inbound chat text, its channel, and the sender's canonical subject |
| `gateway.session.cache_key` | The prompt cache key this session declared, and whether its route is persistent |

`agent.model.prompt` is emitted whole once per session and extended thereafter. Turn N's message
list strictly contains turn N-1's, and everything appended since was already emitted by
`agent.model.answer`, `agent.tool.script`, and `agent.tool.output` on the turn that produced it, so
re-sending the whole list every turn would cost a long session O(N²) payload bytes of near-
duplicates. `transcript.scope` says which shape an event carries (`full` or `delta`) and
`message.count` gives the size of the request actually sent, so a reader can tell a trimmed session
from a truncated log. Concatenating a session's events in order reconstructs the exact transcript.

`accounting.model.turn` fires in either mode, so turn counts, durations, and outcomes remain
available without opting in to content. `agent.config.inspected` also fires in either mode and
carries only the bounded result byte count and whether this call repeated an earlier one; it never
logs the configuration itself. With payloads enabled, the credential-free meta result appears as a
tool message inside the next `agent.model.prompt` transcript, just as script output does — once per
session, because a repeated inspection is answered with a short pointer at the copy already in the
conversation. Per-command detail lives on the `shell.command` span: the command word, its kind, its
argument count, its exit code, and its outcome — and, past the per-script span cap, on the
`shell.script` span's counters.

A mounted skill takes the same route as that meta result. The listing the model sees — names and
one-line descriptions, beginning `Skills mounted for this agent` — is a system message of its own,
placed after the standing instructions, so it rides the first turn's `full` `agent.model.prompt`.
The skill's text does not: a `read_skill` result is appended to the conversation like any other tool
message and reaches the log stream only inside the following turn's `agent.model.prompt` delta, with
payloads on. Neither `agent.tool.script` nor `agent.tool.output` fires for a skill read;
`agent.skill.read` records the name, the path, and the byte count in either mode.

## Redacting secrets

`dekopon_core::Redacted<T>` wraps a value that must never be rendered in the clear. `Debug`,
`Display`, and `Serialize` all produce a marker instead, and the value leaves only through the
conspicuous `expose`. Persisting a real credential — the ChatGPT auth file is the one case —
requires an explicit `#[serde(serialize_with = "dekopon_core::serialize_exposed")]` per field, so
the safe behaviour is what you get by default and the exception is visible in review.

The marker is padded to the character width of the value it replaces, so a redacted field keeps the
shape of the record around it:

```text
sk-live-abcdef012345       ->  [     REDACTED     ]
short                      ->  *****
```

Below the width of `[REDACTED]` the word cannot fit, so the marker degrades to asterisks rather than
truncating into something that reads like a different token. Preserving width leaks one fact — how
long the secret was — which can narrow down an issuer or credential class. It is a
readability-for-metadata trade, not a free win.

## Trace and log model

One generated OpenTelemetry trace links the command to spans such as:

- `transport.receive`, `gateway.message`, `gateway.session`, and `prompt.session`;
- `process.run` and `process.node` at `DEBUG` for process-lifecycle work;
- `prompt.model_turn` and `model.complete`, with `chatgpt.refresh` nested inside the latter whenever
  a ChatGPT subscription credential is rotated or adopted — it carries `forced`, `outcome`
  (`adopted`, `rotated`, `rotated-unsaved`, or `failed`), `duration_ms`, and the new
  `credential.expires_at`, and never any token material;
- `prompt.script`, `shell.script`, and `shell.command`; and
- `provider.compile`, `provider.describe`, and `provider.invoke`; and
- `provider.run_command` at `DEBUG`.

`process.run` carries only its private stable `run.id`. `process.node` carries that `run.id`, a
private stable `node.id`, root parent, fixed `process.kind`, the `process.interruptibility` contract
(`non-interruptible` or `cancellable`), and terminal `process.outcome` (`succeeded`,
`operation-error`, `panicked`, `cancelled` for a requested cancellation of a cancellable node, or
`task-cancelled` for a runtime-driven abort). Tokio task IDs, scripts, argv, values, diagnostics,
provider payloads, and raw operation errors are absent. These spans are `DEBUG` so INFO volume does
not grow with frontend process nodes or with the command words a script runs; a diagnostic filter
may enable them. The `broker-command` kind is one cancellable node per command word the broker leg
runs, and `dekopond` ties it to session Stop, which aborts and joins the round trip before the
script reads `session-cancelled`; external embedders may supply no signal. Nodes report `parent.id`
`root`; real parent threading remains future. The gateway's trace filter includes `dekopon_process`;
`Span::or_current` retains the current parent when a sink disables these DEBUG spans, which are not
a public telemetry contract. Broker component spans and their bounds are described in
[Broker execution spans](#broker-execution-spans).

One model turn drives at most a handful of scripts, and one script drives many capability calls, so
`prompt.script` is the span for a whole unit of model-requested work rather than for a single
capability invocation. Inside it, the interpreter opens one `shell.script` span per run, and inside
*that*, `shell.command` is one span per command word the script actually ran, in execution order — a
builtin, a capability call, a shell function, a word this shell refuses, or a word that resolved to
nothing. A trace therefore reads as the ordered list of commands a script executed, and the reading
survives constructs where one script word drives several executions: `xargs` mapping a command over
ten items produces ten `shell.command` spans nested inside its own. The interpreter emits these as
plain `tracing` spans and knows nothing about OTLP; `dekopon_shell` is named in this file's trace
and log filters. Each command span carries:

| Attribute | Value |
|---|---|
| `shell.command.name` | The command word, whoever wrote it |
| `shell.command.kind` | `builtin`, `capability`, `function`, `control`, `rejected`, `not-granted`, or `not-found` |
| `shell.command.argument_count` | How many arguments the word received, never their values; see [Exclusions](#exclusions) |
| `shell.command.exit_code` | The status the command reported |
| `outcome` | `succeeded`, `failed`, `denied`, `not-found`, `usage-error`, `timed-out`, `limit-exceeded`, or `rejected` |

Every command word gets its span, at `INFO`, however many a run executes. A model-authored `while`
loop is bounded only by the step budget (default 100,000) and the script deadline, so one bash tool
call can produce tens of thousands of them — and every one is exported: an attribute may be
truncated with a marker, a span is never dropped ([goal 2](design.md#constitution)). The
`shell.script` span carries the run's shape in constant size beside them:

| Attribute | Value |
|---|---|
| `shell.script.commands` | Command words the script executed, loop iterations and `xargs` sub-invocations included |
| `shell.script.capability_commands` | How many were a capability call or a provider command word |
| `shell.script.failed_commands` | How many reported a non-zero exit code |

`not-granted` splits the swing-and-a-miss out of `not-found`. A word that parses as a capability
identifier, in a namespace this session *does* hold but naming a capability it was not granted, is a
different fact from a typo: it is a model repeatedly reaching for something an operator may want to
grant. A trend of them in one namespace is the signal worth acting on, and the word itself says
which namespace was reached into. The script cannot tell the two apart: both print `command not
found` and exit 127, since a model that could distinguish them would have an oracle for enumerating
the deployment's capabilities one guess at a time.

`outcome` keeps a policy refusal (`denied`) distinct from a capability that ran and errored
(`failed`) and from one that is unreachable (`not-found`), mirroring the interpreter's own exit-code
mapping; flattening them would hide an authorization refusal in the noise of ordinary failures.
`rejected` and `limit-exceeded` name the two ways a command ends the whole script — a construct this
shell excludes, and an exhausted sandbox budget.

Structured log records use stable `audit.event` attributes and do not mirror spans: a command's
start, end, duration, parent, and outcome all live on its `shell.command` span, so the log stream
carries accounting, refusals, errors, and payloads. `agent.command.unobserved`
records a command-word run whose caller was dropped while the owning runtime remains alive; it
carries `command.leg`, a low-cardinality outcome and error kind, never output or argv, and its
complete failure cause is an ordinary error event beside the record. Logs inside the active trace
carry generated `trace_id` and `span_id`, so an OTLP log result pivots to the performance trace.

## Exclusions

Telemetry is not minimized toward the operator: the telemetry store is inside the operator's trust
boundary, and access to it is access to every conversation, prompt, and argument
([the constitution](design.md#constitution)). What telemetry excludes is goal 1's list and nothing
else:

- secret bytes;
- chat bot tokens;
- model keys;
- OTLP authorization headers;
- provider credentials; and
- broker socket paths.

Command arguments are still recorded as a count rather than as values, because a
`curl -d '{"apiKey":...}'` body and a `cap some.id '{"token":...}'` object are secret bytes wearing
argv's clothes, and `shell.command.argument_count` carries how many there were. The command *word*
is recorded in full whatever wrote it. *Committed direction:* argument values are recorded too, with
secret material excluded at the point it is identified rather than by withholding the whole vector
([goal 2](design.md#constitution)).

Model-selected invalid tool names are not copied into remote rejection events; a rejection records a
stable category such as `unknown-tool`. Error telemetry records stable categories rather than raw
errors, which may contain untrusted provider or transport text. Normal command stdout/stderr is a
separate output surface. The provider base world has no logging import, so telemetry records
host-observed guest lifecycle and timing rather than arbitrary text emitted from inside a component.

## OpenObserve development and CI

[`../examples/otel-traces/`](../examples/otel-traces/README.md) starts one pinned OpenObserve
container with one Docker volume, documents authenticated OTLP/HTTP export, and explains how to
inspect traces in the UI.

`examples/otel-traces/smoke-test.sh` is the repository-level black-box check. It runs real broker
and gateway processes, a stdlib model stub, and one private local-transport turn that executes an
authorized echo provider. It asserts `transport.receive`, `gateway.message`, `gateway.session`,
`broker.invocation`, `provider.compile`, and `provider.invoke`, including cross-process invocation
trace continuity from the transport's receipt onward. A
smoke-only stdout shipper checks ingestion-record counts; complete bounded remote log queries
independently correlate each daemon's native ID pair with an actual exported span. Startup
compilation may have a separate trace. Local, shipped, and remote records must exclude payload and
fake credential sentinels. CI runs the same script and its negative controls, with unconditional
owned-resource cleanup.

### Raspberry Pi storage snapshot

On the project's Raspberry Pi OpenObserve deployment, the `dekopon` streams for one simple prompt
occupied **200 KiB** of signal payload — **148 KiB of traces** and **52 KiB of logs** — and the whole
OpenObserve `stream/` tree **1.22 MiB**, indexes, metadata, and directory overhead included. These
are allocated filesystem blocks and a point-in-time development sample, not a per-prompt storage
guarantee.
