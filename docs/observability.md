# Observability

`dekopon-run`, `dekopond`, and `dekopon-brokerd` each export their own execution traces over OTLP,
using either gRPC or HTTP with protobuf payloads. Runner coverage is the one-shot runner process,
model turns, immediate Wasm compilation/description/invocation, model-authored script execution in
both `prompt` and `shell` modes, and explicit broker-client calls. Gateway coverage is one routed
chat message and the bounded agent session it drives. Broker coverage is one span per decoded
invocation from a mapped peer. None collects telemetry from Kubernetes nodes or other Rust
processes; host-level collection remains separate work.

The three processes export **independently**. The broker only ever observes broker-mediated
invocations, so it cannot stand in for the runner: a broker-only deployment loses every model turn,
every direct-mode capability call, and every script span. They are separate emitters that meet in
the backend, correlated by trace context rather than by one relaying for the other.

This is operational observability. It does not replace broker policy evidence, authorized
invocation results, or the broker's durable hash-linked audit log.

## Which signal carries what

Dekopon emits three signals, and they answer different questions. Keeping them separate is what
stops the log stream from becoming a worse copy of the trace.

| Signal | Question | Lifetime |
|---|---|---|
| **Broker audit** | What was *authorized*? | Permanent, hash-chained, owner-only |
| **Traces** | What did the code do, and how long did it take? | Sampled; expires with trace retention |
| **Logs** | What could not be a span, and what must outlive one? | Retained independently |

The rule that follows from this: **a span is a span of time; a log is a fact that is not a
duration.** A log event has to justify itself as one of two things.

- **A payload.** Text too large or too unbounded to belong in a span attribute — the model and tool
  transcript below.
- **A survivor.** Something still needed after traces expire or when traces are sampled — costs,
  refusals, and errors.

An `X.started` event is never either: the span's start time is strictly better, and it carries
parent and duration besides. Dekopon used to emit a started/completed log pair for every span,
which predated the OTLP exporter — when nothing exported traces, those pairs *were* the telemetry.
They are gone. What remains is accounting, refusals, errors, and payloads.

## Accounting

The unprivileged harness owns one mandatory checkpointed `TokenTracker` for each opaque job.
These are operational facts, not broker authority, a delivery receipt, a billing reconciliation,
or estimated dollars. Subscription token reports do not imply public-API prices.

| Event and matching span | Aggregation level | Contract |
|---|---|---|
| `accounting.model.call` | One logical chat completion or image generation | `accounting.version=1`, `job.id`, `call.sequence`, `event.sequence`, `segment.sequence`, `model.turn`, `model.kind`, configured/backend/model/effort identity, duration, outcome, fixed reason, optional usage and a typed `accounting` JSON body with bounded attempt observations |
| `accounting.model.transition` | Snapshot, **not additional spend** | Same version/job/event coordinates, transition sequence and a typed body containing immutable from/requested/to identity, requesting-call/attempt/decision linkage, application or refusal outcome, pre-transition cumulative/per-model/segment totals and resulting segment |
| `accounting.model.job` | Consume-once terminal snapshot, **not additional spend** | Same version/job/event coordinates, generation outcome/reason separately from delivery disposition, invalidity, and typed cumulative/per-model/segment totals |
| `accounting.http.request` | Broker provider HTTP attempt, not model inference | Method, authority, status, accounted request/response bytes, outcome and sanitized failure category |

A logical call is not an HTTP attempt. Every inference transmission reserves one attempt before
sending, including the subscription client's single explicit-401 retry. Refresh/adoption is not
inference. Built-in clients record `kind=http`; non-HTTP adapters explicitly record `kind=adapter`.
A call whose adapter supplied no attempt has `attempts_complete=false` and an `unobserved_calls`
marker; this is unknown transmission/spend, not an invented HTTP request. A job records at most 129
calls in total — the ceiling is on calls, not on kinds, so 129 chat calls and no image is as valid
as 128 and one — and two attempts per call. There is no retry of uncertain
inference. Attempt sequence is local to its call; call/event sequences and segments are job-local.

A provider that reports usage for one attempt twice, differently, does not fence the job. Duplicate
`"usage"` keys in one JSON object are legal, and a stream can report an interim count and then a
different terminal one; the terminal report wins outright, and two reports of equal standing that
disagree leave exactly the fields they disagree about unknown. The
`conflicting-usage-observation` warning names those fields. A field the ledger cannot trust is
reported to the broker as unreported for those calls rather than blanking the other four, under the
`accounting-field-unreported` warning.
Opaque job IDs correlate spans across resume without exposing sender/conversation coordinates.

`usage.input_tokens` includes the cached-input subset; `usage.output_tokens` includes the reasoning
subset. Neither subset is added again. Provider `usage.total_tokens` is independent, not an inferred
input+output sum. Each total field retains checked `known` sums, `unreported` counts and `invalid`
flags. An overflowing known sum becomes null with invalidity, never saturation. Inconsistent subsets
and totals are retained and flagged, never clamped. Complete flat usage fields are absent when
unreported or invalid; input+output is derivable only when both fields are complete and addition
fits. Partial known sums remain available in the typed totals, not mislabeled complete spend.

Usage is observed independently before tool/content/image validation, including failed/incomplete
SSE, JSON usage preceding a later decode/read error and JSON error bodies. Identical observations
are idempotent; a terminal report supersedes an interim one; and two reports of equal standing that
disagree leave only the fields they disagree about unknown, fencing neither the call nor the job.
A later Stop, tool error, persistence failure or reply failure cannot erase observed tokens. Live
Stop drains synchronous bounded inference before terminal accounting. Dropping the last finalizer
after workers settle records abandoned/unknown delivery; process death can leave an unterminated,
unknown job and never proves complete spend. Terminal flags are checkpointed; failed persistence
fences the retained store copy, while live observations still reach accounting. Memory checkpoints
promise no crash durability or exactly-once effects/export. A repeated restore does not recount
calls, reset consumed limits or open another segment merely to reauthorize the restored client.

Transitions, including denied/failed/local refusals, snapshot spend without fabricating a model
call. Only applied model/effort changes create segments; return-to-model totals include earlier
segments. Exporters must choose **one aggregation level**: summing call, transition and terminal
records together counts the same spend several times. Calls and transitions are parented to their
matching accounting spans under the job span; terminal events belong to that retained job span.
`prompt.session` also nests under `accounting.model.job`, so script and interpreter work retains
job ancestry after the model-call span has closed.
No prompt, reasoning, tool arguments, generated bytes, endpoints or credentials enter these events.

The former `accounting.model.turn`, separate image-generation accounting and success-only
`ModelUsageObserver` emitter are removed. Historical text-only turn records remain readable by the replay
reader; new listing queries use call records. Recording files now retain independent job/call
accounting, including failed/no-answer and image calls, with unknown-aware totals. Transcript prompts carry `transcript.version=2`,
`context.revision` and explicit full/delta scope. The replay reader retains ordered portable request
fragments separately from answered turns and accounting. Full rebuilds replace context rather than
recounting repeated tool groups. Conflicting exports, orphan results and invalid revision ordering fail.

Provider HTTP accounting retains `dekopon.http.request.accounted_bytes` and
`dekopon.http.response.accounted_bytes`: conservative envelopes, not HTTP payload sizes.
Method/authority/status are absent when unknown, and paths, queries, headers and bodies stay out.

### The broker-hosted live token view

`ModelUsageReport` is a best-effort delta projection of the harness tracker, not another accumulator.
Its historical `modelCalls`/`unreportedCalls` fields count attempt observations (including explicitly
unknown adapter operations), including failed and cancelled calls and images. A checkpointed report
cursor prevents reporting old observations again on resume, and it advances only after every field
of the delta has been decided. A field the tracker cannot trust — an untrusted or overflowed known
sum — is reported as unreported calls for that field rather than dropping the whole delta, so one
bad field never converts the other four into silence; nothing else is refused here, and the broker
refuses only a structurally malformed report. A full/closed queue or failed export loses UI data,
never spend in the tracker. The broker UI remains process-local, self-reported, non-authoritative,
and resets on restart; it is not billing reconciliation or durable authorization audit.

## Refusals, errors, and outcomes

The other half of what survives trace expiry is the record of something a process refused or could
not do. These fire in either payload mode, because each carries a fixed category rather than the
untrusted text that triggered it:

| Event | Emitted by | Carries |
|---|---|---|
| `agent.tool.rejected` | `dekopon-harness` | model turn, the tool-call index or count, and a fixed `error.type` such as `too-many-tool-calls` or `unknown-tool` — never the model's own tool name or arguments |
| `agent.image_generation.refused` | `dekopon-harness` | model turn, tool-call index, and a stable `reason` such as `session-limit`; never the model-authored generation prompt |
| `agent.asset.refused` | `dekopon-harness` | the gateway-assigned asset id and the gateway-authored refusal text the model reads back |
| `agent.asset.fetched` | `dekopon-harness` | the asset id, its media type, its byte count, and `asset.truncated` — whether a textual asset larger than the prompt's textual bound was clamped with a trailer the model reads rather than dropped or failed; never the bytes and never the sender's file name, which is untrusted text |
| `agent.skill.read` | `dekopon-harness` | model turn, tool-call index, the operator-authored `skill.name` the request matched, `skill.resource` (the resource path; empty for the skill's own instructions), `skill.bytes` of the tool result, and `skill.repeated` — `true` when that text was already in the conversation and a one-line pointer was returned instead; never the skill text and never the name the model typed |
| `agent.skill.refused` | `dekopon-harness` | model turn, tool-call index, and a stable `reason` — `unknown-skill` or `unknown-resource`; the refusal that lists what *is* mounted goes to the model as a tool result, not here |
| `agent.improvement.suggested` | `dekopon-harness` | model turn, tool-call index, `suggestion.index` (1 to 3), the enum tokens `suggestion.category` and `suggestion.confidence`, and the model-authored `suggestion.target`, `suggestion.summary`, `suggestion.evidence`, and `suggestion.proposal`, bounded to 128, 512, 2048, and 2048 bytes — see below |
| `agent.improvement.refused` | `dekopon-harness` | model turn, tool-call index, and a stable `reason` — `invalid-category`, `invalid-confidence`, `empty-field`, `field-too-long`, or `session-limit`; none of the submitted text |
| `guest.invocation.completed` | `dekopon-run` | the capability id, the provider id on success, iteration index, duration, and `outcome` for one direct-mode component invocation |
| `runner.command.failed` | `dekopon-run` | the command name and a stable `error.type`, including the `output-write` failure that has no other surface |
| `policy.name.unresolved` | `dekopon-brokerd` | policy id, name kind, and the action or provider name no loaded provider declares, so a rule that can never match is visible at startup |
| `config.startup.warning` | `dekopon-brokerd` | the capability id and a stable `reason` — `unrouted-constraint-set` or `unconstrained-capability` |
| `command.resolve.failed` | `dekopon-brokerd` | the provider-declared command word, a stable `error.kind`, and the host error's chain, recorded when running the word (`runCommand`, or the legacy `resolveCommand`) fails rather than declines: no provider declares it, the argv plus piped value exceeded `maxInputBytes`, the guest trapped or reached for an import, or its answer would not decode |
| `policy.request.refused` | `dekopon-broker` | the capability id and a rendered `error.reason` for a Cedar request the policy schema could not admit — the caller still sees plain `policy-denied` |
| `broker.leg.connected` | `dekopon-run` | the broker socket tier, the session trace identifier, and the granted-capability count — never the socket path |
| `guest.invocation.summary` | `dekopon-run` | provider and capability ids with iteration count and total/mean durations for a `--repeat` run, replacing one record per iteration |
| `agent.command.unobserved` | `dekopon-harness` | `command.leg` (`broker` or `direct`), a low-cardinality `outcome` (`succeeded`, `operation-error`, `cancelled`, or `task-failed`), and a fixed `error.type` (`none`, the leg's own error kind, `task-cancelled`, or `task-panicked`), recorded when a command-word run's caller was dropped while its process node was still joined; never the word, the argv, the piped value, or the text a provider rendered, and the failure's complete cause goes out as an ordinary error event at the same site rather than into this record |

`agent.improvement.suggested` is the deliberate exception to that sentence. Its four free-text
fields are model-authored — bounded and stripped of control characters other than newline and
tab, but never reduced to a category — and they are recorded whether or not payloads are on, because a suggestion nobody can
read is not a suggestion. That is why `suggest_improvement` is never offered unless the embedder
opted in: `dekopon-run prompt --suggestions`, `session replay --suggestions`, or
`improvementSuggestions: true` on a `dekopond` route. Enabling it is the consent that declares the
log sink in scope for that text, and nothing else widens with it: the record carries no chat text
the gateway holds and no subject, only what the model chose to write into those fields.

An event name is part of this contract: CI fails a pull request that emits an `audit.event` name
this file does not mention, so a rename lands here in the same change.

## Enable OTLP export

Export remains disabled unless an endpoint is configured:

```console
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:5080/api/default
export OTEL_EXPORTER_OTLP_HEADERS='Authorization=Basic%20<INGESTION_TOKEN>,organization=default,stream-name=dekopon'
export OTEL_SERVICE_NAME=dekopon-run

dekopon-run prompt ...
```

The global flags and environment equivalents are:

| CLI | Environment | Default |
|---|---|---|
| `--otlp-endpoint` | `OTEL_EXPORTER_OTLP_ENDPOINT` | unset; export disabled |
| `--otlp-transport` | `OTEL_EXPORTER_OTLP_PROTOCOL_KIND` | `http` |
| `--otel-service-name` | `OTEL_SERVICE_NAME` | `dekopon-run` |
| `--otel-export-timeout-ms` | `DEKOPON_OTEL_EXPORT_TIMEOUT_MS` | `5000` |

Both transports are first-class. `http` treats the endpoint as a generic OTLP/HTTP base and appends
`/v1/traces` and `/v1/logs`. `grpc` treats it as an authority and takes its method paths from the
OTLP protobuf service definition, which is what a receiver behind a path-routing reverse proxy
needs — those paths are fixed by the protocol and cannot be reassigned, so the proxy rule matches
`/opentelemetry.proto.collector.*` rather than a path of the operator's choosing.

Both read the standard `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_EXPORTER_OTLP_TRACES_HEADERS`, and
`OTEL_EXPORTER_OTLP_LOGS_HEADERS` variables directly through the exporter. Header values use the
OpenTelemetry URL-encoded form; for example, `%20` represents the space in `Basic <token>`. There is
intentionally no header CLI flag and no header configuration field, because credentials must not be
exposed in process arguments, retained in a parsed CLI value, or written into a configuration file.
Endpoint URL userinfo (`https://user:password@host`) is rejected for the same reason; use the
standard header variables.

The example header set works unchanged on both transports. On OpenObserve (observed on v0.92.0)
`stream-name` selects the stream for traces and logs alike over HTTP and gRPC, so signals land
identically regardless of transport. The organization is the one asymmetry: HTTP reads it from the
endpoint path (`/api/default`) and ignores the header, while gRPC has no path to carry it and
requires the `organization` header — without it the receiver rejects every export with gRPC status
`InvalidArgument`. Keep `organization=<org>` in the header set unconditionally; over HTTP it is
redundant, never harmful.

Standard `OTEL_RESOURCE_ATTRIBUTES` values are attached to both signals, alongside a `service.version` carrying the exporting executable's own version. HTTPS endpoints use WebPKI roots on both transports, and redirects are disabled so a receiver cannot forward an authorization header to another destination. Plain HTTP is suitable only for a loopback development receiver or an otherwise trusted isolated network because headers and telemetry are unencrypted.

## Export failures

The OpenTelemetry SDK has no error handler to install; its `internal-logs` feature is the only
runtime channel it reports export failures through, and the workspace enables that feature for the
API, the SDK, and the OTLP exporter. Rejected tokens, a missing `organization` header, and a
receiver that is down therefore say so: the runner prints them on stderr at warn/error, and both
daemons emit them as structured JSON on stdout. `dekopon-telemetry` filters the `opentelemetry`
target off every OTLP layer it installs, whatever crate directive the binary supplied, so those
records reach the local stream only and an export failure can never be re-exported through the
exporter that failed.

### Daemon exit and shutdown records

Both daemons report the same two facts on the way out, in the same shape:

| Event | Level | Emitted by | Carries |
|---|---|---|---|
| `gateway_exit` / `broker_exit` | error | `dekopond` / `dekopon-brokerd` | `error`: the failure and its whole source chain, rendered as one `a: b: c` line |
| `gateway_telemetry_shutdown_failed` / `broker_telemetry_shutdown_failed` | error | `dekopond` / `dekopon-brokerd` | `error`: every flush and shutdown failure raised while stopping the exporters, each naming its signal and stage |

`gateway_exit` used to carry both an `error` holding only the top-level `Display` and a `cause`
that omitted it, so the two daemons' exit records could not be read the same way; they now render
one `error` through the same chain renderer. The shutdown record likewise replaces a separate
flush-failed and shutdown-failed pair with one record whose text names which signal and which
stage failed.

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
  telemetryPayloads: false   # see "Span payloads" below
```

There is deliberately no credential field. The broker reads `OTEL_EXPORTER_OTLP_HEADERS` like the
runner does, so a token never enters the configuration file the broker parses, its command line, or
any span attribute — the same rule that keeps provider credentials out of prompts and audit fields.
With `transport: grpc` the endpoint is an authority that names no organization, so the header set
must carry `organization=<org>` — the receiver otherwise rejects every export, and says so through
the SDK's own diagnostics on stdout.

The web UI renders the endpoint, transport, service name, timeout, and payload setting from this section. It reports only whether standard OTLP header/resource-attribute variables are present; their values are never retained or rendered.

Telemetry never blocks startup. An exporter that cannot be built disables export and logs why;
authorization and durable audit are the service's contract, and a missing dashboard must not cost a
working authority boundary. Flush failures at shutdown are logged and do not change the exit code,
because the audit chain rather than telemetry is the record of what happened.

The broker's log output is structured JSON on stdout, filtered by `RUST_LOG` and defaulting to
`info`. Shipping those logs to storage is deliberately left to whatever reads stdout, so the broker
holds one credential rather than two.

## Trace context across the socket

`InvocationRequest` carries an optional W3C `traceParent`. The runner fills it from the span that
actually requested the capability, and the broker opens `broker.invocation` beneath it as a remote
parent, so one trace spans both processes instead of two unrelated traces appearing per run.

This is separate from `TraceId`, which continues to identify a Dekopon session in the audit chain
and replay accounting. Two identifiers, two jobs: `TraceId` is durable audit correlation and
`traceParent` is telemetry correlation.

`traceParent` is untrusted like every other request field. It reaches span parenting and nothing
else — never policy, replay rejection, routing, or audit. A malformed value is a decode failure
rather than a silent `None`, since attaching broker spans to a trace that does not exist is worse
than sending none; an absent value simply means the client exports no telemetry.

The broker span carries the invocation, capability, and trace identifiers and nothing more. Provider
input and output, URL paths and queries, headers, and bodies stay out of it for exactly the reason
they stay out of audit records: telemetry is a second egress path with none of the audit chain's
guarantees, and it must not carry what audit deliberately redacts.

An attested proposal adds routing fields to both spans: `broker.invocation` records the claimed
`subject` and `agent`, and `broker.authorize` records the `subject` and the `via` peer the broker
derived the context through — the same values the audit chain keeps, for the same reason. All of
them are canonical identifiers (`slack.t0123abc.u9xyz`), never the chat message that prompted the
invocation. A refusal records the claimed subject and its `outcome` with no `via`, because no
attested context was derived.

## Gateway spans

`dekopond` wraps each routed chat message in two spans of its own:

| Span | Fields |
|---|---|
| `gateway.message` | `transport`, `agent`, `outcome` (`answered`, `declined`, `unauthorized`, `busy`, `failed`, `cancelled`, `reply-failed`) |
| `gateway.session` | `agent`, `conversation.turns`, `conversation.bytes`; wraps the broker leg and the model session |

The prompt loop's spans (`prompt.session`, `accounting.model.call`, `prompt.script`, `shell.script`, `shell.command`) nest under `gateway.session`, and the broker's `broker.invocation` joins the same trace through the proposal's `traceParent` — so one trace reads from "a person asked something in Slack" to "a provider made an HTTP call". An image generation is a call like any other: it opens `accounting.model.call` with the same identity, duration, outcome and usage fields a chat call carries, never prompt or PNG content, and the audit event that call closes carries `model.kind=image` to tell the two apart (the span itself does not: `model.kind` is a record field, not a span attribute). Every path that opens that span — an ordinary call and the finalize sweep that closes an abandoned one — builds it with the same field set, so filtering on `model.name` cannot silently omit the calls whose outcome was in doubt. `prompt.asset_fetch` joins them whenever a model opens an attachment: one span per fetch, carrying the asset number the conversation referred to and the turn and tool-call index that asked for it, never the file's name or bytes. It is gateway-only, because only a gateway session offers the asset tool.

Neither gateway span carries chat text or a subject identifier. `outcome` is the whole answer at the metadata level: `declined` means an optional owned-thread continuation deliberately produced no chat delivery, `unauthorized` means the broker's chat-scoped `capabilities` returned nothing and no model or activity call was made, `busy` means admission control refused the message, `cancelled` means an authenticated native Stop won the race against terminal delivery, and `failed` names a category through the `gateway_session_failed` log event rather than a message. The sender's canonical subject and the message text ride the `gateway.message.received` log event under the payload gate below, never a span attribute. `agent.reply.declined` records only the model-turn number; it carries no proposed text, thread key, or subject. `unreported-capability-work` is a stable failure category whose fixed chat warning directs the sender to audit before retrying; no provider detail enters either surface.

Every transport reconnects on one jittered exponential backoff, and the jitter comes from the
operating system. `gateway_transport_jitter_unavailable` is the warn-level record of an OS that
refused entropy, carrying the `getrandom` failure and nothing else; that attempt's delay then falls
back to its unjittered step, which costs a fleet its de-synchronization rather than its reconnect.

In-flight presentation remains metadata-minimal. `gateway_activity_failed` is debug-level and carries
only `operation` plus a stable category — for a lease that is `busy` (a live generation holds the
thread), `quarantined` (an uncertain native write still does), or `capacity` (the 128-lease ceiling).
Two exceptions are warn level: `operation="quarantine"` with `cause_type="activity-quarantine-full"`,
emitted once when the separately counted 128-entry quarantine fills rather than once per refused
lease, and `operation="cleanup"` with `cause_type="activity-cleanup-abandoned"` and an `abandoned`
count, emitted once when the shutdown grace expires before the progress removals finish — the count
is how many ⌛ messages that shutdown left in their channels, which is a different operator problem
from the `gateway_sessions_abandoned` that reports sessions nobody heard back from. A permanent Slack installation fallback
emits `gateway_activity_degraded` with `transport=slack` and `surface` (`agent-status` or
`reaction`). `gateway_session_stop_requested` carries only the transport. None records channel,
thread, message, subject, status text, emoji, raw service response, or credential.

`gateway_reply_rate_limited` is the warn-level record of a channel-creating post the service rate
limited. It carries `transport`, the `method` (`chat.postMessage` or `files.completeUploadExternal`),
and `retry_after_seconds` — how long the physical channel slot stays parked, which is the stated
`Retry-After` capped at sixty seconds or five seconds when the header is absent or unparsable. It
records no channel, thread, subject, or answer text.

The informational broker reports behind the web UI are never retried, so their warning is the whole
record of a failure. `gateway_agent_inventory_report_failed` and `gateway_usage_report_failed` carry
a stable `category` — `unsafe-socket`, `connect`, `protocol`, `remote`, and the rest of the broker
client's failure surface, with `timeout` reserved for a broker that did not answer inside the
two-second report deadline. Stale inventory in the web UI is then a log query rather than a guess.

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
therefore one `reason=signature` line a minute rather than one per delivery attempt, and reading the
rate means reading `suppressed` rather than counting lines. `error` on the accept and listener
events is the operating system's message for a socket call — never a request, a body, or a sender.
None of these carries a phone number, a WABA identifier, a message ID, or message text; the sender's
canonical subject still arrives only through `gateway.message.received` under the payload gate.

### What conversation history changes

A route set to `mode: persistent` — the contract is in [`dekopond.md`](dekopond.md#conversations) —
changes the meaning of a field that already exists, and that is the kind of change a dashboard
absorbs silently and wrongly. A route left on the `oneShot` default changes nothing here.

**`message.count` is the field.** It appears on the `model.complete` span and payload `agent.model.prompt` record, and today it counts one exchange: the system prompt, the message a
person sent, and whatever the model and its tool have said back within this session. Once a session
is seeded with history it counts the replayed window *plus* this exchange, so the same field on the
same span means something different depending on a route's `conversation.mode`. A panel plotting it
across the switchover shows a step change that is not a regression, and averaging across both is an
average of two different quantities. Re-baseline deliberately rather than discovering it later; the
same caution applies to `usage.input_tokens`, which rises for the same reason and for real token consumption; no subscription-dollar estimate follows.

**The history size gets its own fields** rather than being inferred from that step change.
`gateway.session` carries `conversation.turns` and `conversation.bytes` — how many prior exchanges
this message replayed and how many bytes they occupied. Both are zero on a `oneShot` route and on
the first message of any conversation, which makes "seeded or not" a filter rather than a guess.
`gateway_conversation_evicted` is a gateway lifecycle event carrying a reason of `idle`, `capacity`,
or `grant-changed` and nothing else, so a `maxConversations` ceiling set too low reads as eviction
churn instead of as a bot that intermittently forgets. Its reason is the whole event on purpose: a
key would carry a conversation identifier and a canonical subject, which are payload fields, and an
eviction is not the place to leak them at the metadata level.

The history itself is not a new signal. It is chat text and model output, already excluded by the
data-minimization rules below, and it appears only where those already send it: with
`telemetryPayloads` enabled, the session's first `agent.model.prompt` carries its opening message
list, which on a seeded session now includes the replayed window. That event becomes larger and
older than it was — enabling payloads on a persistent route declares the sink in scope for a
conversation rather than for a message.

### Reading the prompt cache

Every model request `dekopond` makes declares a `prompt_cache_key` — one per conversation on a
`persistent` route, one per bound route on a `oneShot` one. [`dekopond.md`](dekopond.md#the-prompt-cache-key)
has the contract; two things follow for telemetry.

**`usage.cached_input_tokens` is how you find out whether it works.** It needs no new
instrumentation: whatever the provider reports already lands on the `accounting.model.call` span and on
the `accounting.model.call` record beside `usage.input_tokens`. Plot the ratio on a conversation's
second and later turns, which are the requests that repeat a prefix worth caching. Do not expect a standing discount or a subscription cache lifetime: provider retention is
undocumented for that endpoint; only reported usage demonstrates a hit. A window trim rewrites the
front of the request and costs a miss by construction, so a run of misses on long conversations is
`maxTurns` or `maxBytes` doing its job rather than a broken key. A count the provider never reported
is absent rather than zero, so a missing field means "unreported" and not "nothing was cached".

**The key itself is a payload field.** It rides `gateway.session.cache_key` with
`telemetryPayloads` enabled, never the metadata-only default and never a span attribute. It carries
nothing about the sender by construction, but within one process it does join one person's turns to
each other, which is precisely the linkage the default withholds. It is emitted on its own event so
that a key and a canonical subject never share a record.

## Broker execution spans

`broker.invocation` is not a flat bar. Beneath it the broker's own crates emit:

| Span | Crate | Fields |
|---|---|---|
| `provider.compile` | `dekopon-broker-host` | `path`, `artifact_bytes`, `elapsed_ms`; emitted once per provider at startup |
| `provider.run_command` | `dekopon-broker-host` | provider, `word`, `command.export` (`run-command`, or the legacy `resolve-command`) |
| `broker.authorize` | `dekopon-broker` | invocation, capability, `outcome` (`allowed`, `policy-denied`, `policy-error`, `secret-denied`, `unconstrained-capability`, `agent-denied`, `replayed-invocation`, `attestation-denied`, `unmapped-subject`, `chat-attestation-denied`, `chat-scope-required`, `record-operation-required`, `memory-unavailable`, `invalid-memory-input`, `invalid-turn`), `policy.errors_present`; `subject` and `via` on attested proposals |
| `broker.execute` | `dekopon-broker` | provider; `credential` — the symbolic name the invocation selected, when it selected one; `outcome` (`succeeded`, `failed`, `decision-unaudited`, `outcome-unaudited`) and `error` — the same classified reason the terminal audit record carries |
| `provider.invoke` | `dekopon-broker-host` | capability, provider |
| `http.request` | `dekopon-http-host` | `http.request.method`, `server.address`, `http.response.status_code`, `dekopon.http.request.accounted_bytes`, `dekopon.http.response.accounted_bytes`, `outcome`; `error.code` and `error.message` on failure |

`http.request` fields mirror `HttpCallEvidence` exactly, and that is deliberate rather than
incidental: the span reports the same call the audit chain records, so it carries the same sanitized
set and no more. URL paths and queries, request and response headers, and both bodies are absent
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
"why was the broker slow to become ready" rather than "why was that call slow". The offline
`dekopon-brokerd provider sync` and `verify` commands reuse that same host validation and can emit the
span to their stderr subscriber, but they install no OTLP exporter; normal command output remains on
stdout. Its fields attribute time to one component, and each loaded provider also emits one info
event carrying its identity, artifact digest prefix, artifact bytes, compile milliseconds, its
capability and command-word counts, and `command_export` — `run-command`, `resolve-command`, or
`none` — naming which export the host will call for its words.
Since components compile concurrently, their spans overlap; the compile times sum to more than the
wall-clock validation.

`provider.run_command` carries the provider, the command word, and the export name that served
it, never the argv or the value piped into the word. Model-authored argv and piped text are
untrusted content for the same reason `provider.invoke` omits `input`; the help page or usage error
a `run-command` guest renders travels back in the result, not in telemetry.

A `policy-denied` outcome the policy engine never actually evaluated additionally emits
`audit.event = "policy.request.refused"` at `WARN` with the capability and a rendered reason. The
wire result and the audit reason both stay `policy-denied`, because the request is still denied and
the taxonomy callers act on must not shift; this event is the only place an operator learns the
denial came from a request the schema does not admit — a deployment defect — rather than from a
policy that considered it and said no.

`broker.execute`'s `credential` is the owner-authored symbolic name from `broker.yaml`, never the
secret and never the header. It exists because one capability can present a different credential per
acting agent, and a trace that named none of them would make two writes to two different
organizations look identical — the same reason the terminal audit record carries it. A `Redacted`
value renders its marker in either payload mode, so the value cannot arrive by another route.

`broker.authorize`'s `policy.errors_present` is Cedar's evaluation-error flag, and `policy-error` is
the `outcome` and audit reason it produces. A policy that errors while deciding — an extension call
on a malformed value, say — denies exactly like a policy that does not match, so without this pair a
broken rule and a clean no-match are the same record. It stays a flag rather than the error text on
purpose: an explanation must not become a per-request channel for policy source or entity data.

## Core session control admission

`broker.control.decision` is emitted only after the durable `ControlDecision` audit append and
its audit-checkpoint persistence succeed. It records admission, **not application or model work**.
The event carries `control`, `job`, `session`, `request`, `generation`, `sequence`, `agent`, immutable
`from_model`, `to_model`, `from_effort`, `to_effort`, `admitted`, the broker-only refusal `reason`, and
`decision_ref`. The `broker.control` span carries control/job/request/sequence correlation.
No prompts, provider output, arbitrary context, endpoints, credentials, history or spend enter this
boundary. Public policy/target/attestation/replay refusals are uniformly `control-denied`.
`broker_audit_append_failed` additionally admits stage `control-decision`; a failed append never
returns admission. A lost or late response never proves a switch occurred and is not retried.

## Broker failure events

Every broker failure answers its caller with a deliberately generic wire code, so the log line is the
only place the cause exists. These events carry it:

| Event | Level | Emitted by | Carries |
|---|---|---|---|
| `broker_capabilities_refused` | warn | `dekopon-broker` | `reason` (`attestation-denied`, `unmapped-subject`, `agent-denied`, `policy-error`), `policy_ids` (the policies that determined it, empty for a refusal reached before any evaluation), canonical `subject`, `agent`, `via` |
| `broker_policy_evaluation_error` | warn | `dekopon-broker` | `invocation`, `policy.target` (`capability` or `secret`) |
| `broker_secret_resolution_failed` / `broker_secret_credential_failed` | warn | `dekopon-broker` | `invocation` and low-cardinality source/material `category`; structural credential errors are fixed value-free text. No DRN, locator, revision, value, or value-derived length. |
| `secret_source_resolution_failed` / `secret_projection_failed` | warn | `dekopon-brokerd` | adapter `source_kind` and low-cardinality `category`; no DRN, locator, response body, bootstrap credential, selector, or value |
| `secret_source_cause_classified` / `secret_source_configuration_cause` | debug | `dekopon-brokerd` | safe cause classification behind the stable warn category: I/O kind/errno, HTTP timeout/connect/status, JSON class/line/column, the file-hygiene check name with the errno underneath it, or dependency-error type; URL parsing uses its fixed parser reason. Never endpoint/locator, refused path, or secret-derived bytes/offsets. |
| `broker_storage_outcome_unaudited` | error | `dekopon-broker` | `invocation`, `cause` (`quota`, `timeout`, `corrupt`, `denied`, `io`), and the storage failure's source chain |
| `broker_audit_append_failed` | error | `dekopon-broker` | `audit.stage` (`decision`, `authorized-failure`, `outcome`), `category` (`full`, `poisoned`, `record-too-large`, `sequence-overflow`, `serialize`, `io`), `invocation`, and the error's source chain |
| `broker_request_frame_invalid` | warn | `dekopon-brokerd` | `error.kind` (`timeout`, `io`, `empty-frame`, `frame-too-large`, `deserialize`, …) and the bounded protocol message |
| `broker_connection_failed` / `broker_outcome_unaudited` | warn / error | `dekopon-brokerd` | `category`, the failure's source chain, and — for an unaudited outcome — `invocation.id` |
| `broker_capacity_exhausted` | error | `dekopon-brokerd` | `category`, and the chain naming which bound was reached |
| `broker_accept_retried` | warn | `dekopon-brokerd` | `error.kind` (`process-descriptor-limit`, `system-descriptor-limit`, `kernel-memory`, `connection-aborted`, `connection-reset`, `interrupted`), `backoff_ms`, and the errno's chain |
| `webui_accept_failed` | debug for `error.kind=connection`, warn otherwise | `dekopon-webui` | `error.kind` (the same names as `broker_accept_retried`, plus `connection` and `unrecoverable`), `backoff_ms`, and the errno's chain |
| `broker_checkpoint_poisoned` | error | `dekopon-brokerd` | `audit_records` and the checkpoint failure's chain |
| `broker_socket_cleanup_failed` | warn | `dekopon-brokerd` | the socket error's chain |

`broker_capabilities_refused` exists because an attested `capabilities` and an attested
`runCommand` (or legacy `resolveCommand`) answer a refused caller with the same opaque nothing whatever went wrong — a
distinguishable answer would tell an unauthorized gateway whether a subject is mapped, and an unknown
command word would disclose the surface the refusal withheld. The class, its determining policies,
and the canonical subject therefore land on the broker's own side of the socket, which is what makes
bootstrapping an `identityMapping` for a new sender possible without reading the subject out of a
payload-carrying gateway span. It marks refusals, not traffic: an honored session emits nothing.

A chat-scoped `invoke` and `recordDeliveredTurn` withhold the same fact for the same reason, but
they are accounted decisions rather than unanswered inspections, so what the peer receives is a
`Denied` result whose reason is the one fixed literal `chat-attestation-denied` whatever the claim
failed on. `broker.authorize`'s `outcome` and the durable decision record keep the real class and
its `policy_ids`. A subject-only attested proposal still answers with its own class; no chat
transport takes that path.

The source chain is the diagnosable half. `ConnectionError::Broker` renders as "broker failed" and
`AuditError::Io` as "durable audit append failed"; the errno that says *why* — `ENOSPC` on an audit
filesystem shared with anything else — lives one or two levels down, and these events render the
whole chain as one `a: b: c` line. Frame contents never join it: a decode failure names its kind, not
the bytes that failed to decode.

`broker_accept_retried` and `webui_accept_failed` classify with one shared table,
`dekopon_core::retryable_accept_error`, so the two listeners in the broker process cannot disagree
about which failures a socket recovers from. The web UI's loop cannot abort — `axum`'s `Listener`
trait has no error path — so `error.kind=unrecoverable` is what `EBADF` looks like there: named on
every attempt at the 1 s ceiling rather than retried in silence, which is what it used to be.

`broker_storage_outcome_unaudited` is the storage half of that same distinction. The guest receives
the opaque mapped WIT error whatever ended finalization, and the client receives `outcome-unaudited`,
so the `cause` here is the only thing that separates "free the disk" from "raise the quota" for the
operator who has to clear the poisoned namespace.

`broker_capacity_exhausted` and `broker_accept_retried` are the two events that report a condition
outside any one request. The first says a bounded broker resource — the replay ledger or the audit
log — is full; every caller now receives `capacity-exhausted`, no retry can clear it, and a restart
does not either, because the ledger is restored from durable history. The second says the daemon
survived an `accept` failure it used to exit on. A steady stream of it at
`error.kind=process-descriptor-limit` is the descriptor leak the exit used to hide, and it is worth
alerting on precisely because the service is no longer failing loudly.

`broker_socket_cleanup_failed` is reported but does not preempt the shutdown result. A stale socket
path is a smaller problem than the failure that ended service, so the serve error, the final audit
checkpoint, and `broker_stopped` all come first and the cleanup error surfaces only when nothing
more significant failed.

## Storage telemetry and audit privacy

Storage-backed invocations deliberately do not follow the ordinary provider span shape. The
`broker.execute` and `provider.invoke` storage spans omit provider, capability, agent, subject,
transport scope, logical names, offsets, search terms, and exact bytes even when payload telemetry
is enabled. Provider input/output byte totals receive zero for storage calls. The live UI may show
only storage invocation/operation/sync/quota counts and the largest powers-of-two read/write bucket,
plus public ceilings; it never receives root/key paths or opaque tokens.

Storage audit decisions and outcomes omit principal, actor/agent, via/subject, provider, broker
principal/policy revision, policy IDs/digest, and credential. A separate keyed audit-scope
commitment is never equal to a physical namespace token. Storage decision/output/evidence values use
separate `hmac-sha256:` domains, preventing the ordinary unkeyed low-entropy dictionary oracle.
Historical and current non-storage records retain their previous `sha256:` encoding and bytes.

A retained storage document that fails to decode emits `storage_document_decode_failed` at `WARN`
under `category = "storage"`. It carries the static document kind plus the `serde_json` failure's
class, line, and column, and nothing else: that set separates a truncated write from an unknown or
wrongly typed field without exporting a logical name, a path, an opaque token, or any document
content. The rejected bytes are never echoed.

Entropy and wall/monotonic clock values from durable-files are never emitted as telemetry. A native
filesystem operation may outlive a timeout signal; `finalizationBudgetMs` prevents the next bounded
finalization step from starting after its deadline, while the base/generation leases and quota
reservation remain held until an already-started blocking job drains. Duration is therefore
observation rather than a hard native-operation deadline.

## Span payloads

Spans are metadata-only by default. An operator who has accepted their telemetry sink as in scope
for the data a process handles can opt in to payload-bearing fields — `--otel-telemetry-payloads true`
on `dekopon-run`, `DEKOPON_OTEL_TELEMETRY_PAYLOADS`, or `telemetry.telemetryPayloads: true` in `broker.yaml`.

The opt-in is process state, not an OTLP setting: it applies to every sink the process writes,
including a local `--trace` file, and it applies whether or not an OTLP endpoint is configured. A
`--trace` run without the flag stays metadata-only.

| Span | Field added |
|---|---|
| `broker.authorize` | `input` — the untrusted proposal payload |
| `provider.invoke` | `input` — the payload passed to the component |
| `http.request` | `url.full` — path and query, which the default withholds |
| model/tool log events | the verbatim transcript; see below |

This widens **data**, not credentials. Request and response headers and HTTP bodies stay out in
both modes, and a `Redacted` value renders its marker in either mode because that is a property of
the value rather than of the mode. Durable audit records are untouched by this setting: it changes
telemetry only.

## Model and tool transcript

The verbatim exchange between the model and its tools rides the **log stream**, not span
attributes. A conversation is unbounded text: span attributes are the wrong container for it, every
trace fetch would drag the payload along, and a backend indexes log bodies for full-text search
rather than span fields. Both signals carry the same `trace_id` and `span_id`, so a log result
still pivots to the turn it belongs to.

With `telemetryPayloads` enabled, these events join the accounting and refusal ones:

| Event | Carries |
|---|---|
| `agent.model.prompt` | Version 2, job/model-turn/context-revision, full context at startup/rebuild or a delta of appended messages within one revision |
| `agent.model.answer` | Assistant text and the tool calls it requested, with arguments |
| `agent.tool.script` | The script the model authored |
| `agent.tool.output` | That script's combined output |
| `gateway.message.received` | The inbound chat text, its channel, and the sender's canonical subject |
| `gateway.session.cache_key` | The prompt cache key this session declared, and whether its route is persistent |

`agent.model.prompt` emits `transcript.version=2`, `context.revision` and `transcript.scope`
(`full` or `delta`). Within a revision deltas append; trimming, restore and switches require a new
full snapshot. `message.count` describes the actual request, not just the exported delta. Never
concatenate later full snapshots into a transcript: they can repeat or omit prior tool groups.
The replay reader preserves these fragments in `contexts`, validates matched tool groups and
correlates rebuilt results by host job/logical-call coordinates. Earlier execution summaries remain
untrusted context, not new execution receipts. Opaque provider continuation is never restored.

`accounting.model.call` fires in either mode, so turn counts, durations, and outcomes remain
available without opting in to content. `agent.config.inspected` also fires in either mode and
carries only the bounded result byte count and whether this call repeated an earlier one; it never
logs the configuration itself. When payloads are enabled, the credential-free meta result naturally
appears as a tool message inside the next `agent.model.prompt` transcript, just as script output
does — once per session, because a repeated inspection is answered with a short pointer at the copy
already in the conversation rather than a second full copy. The per-command detail that used to arrive as
`shell.command.started`/`.completed` pairs now lives on the `shell.command` span, which carries the
command word, its kind, its argument count, its exit code, and its outcome — and, past the
per-script span cap, on the `shell.script` span's counters.

A mounted skill takes the same route as that meta result. The listing the model sees — names and
one-line descriptions, beginning `Skills mounted for this agent` — is a system message of its own,
placed after the standing instructions, so it rides the first turn's `full` `agent.model.prompt`.
The skill's text does not: a `read_skill` result is appended to the conversation like any other
tool message and reaches the log stream only inside the following turn's `agent.model.prompt`
delta, with payloads on. Neither `agent.tool.script` nor `agent.tool.output` fires for a skill
read; `agent.skill.read` records the name, the path, and the byte count in either mode, and a
repeated read is answered with a one-line pointer for the same reason a repeated inspection is.

### Reading sessions back

`dekopon-run session` is the half of this contract that reads the stream back. It queries the
receiver the runner and gateway export to, contacts no broker, loads a component only for a
`replay` given `--provider`, and runs a model only for `replay`, whose scripts are answered from
the recording rather than executed. The
command reference is in [`run.md`](run.md); the loop these commands close — record a session,
inspect a bad one, change instructions or write a skill, replay, compare — is in
[`improvement.md`](improvement.md).

- `session list [--since 7d] [--limit 50] [--json]` reads `accounting.model.call` records and
  nothing else, so it lists sessions recorded metadata-only. Records are grouped by `trace_id`,
  newest first: `TRACE`, `STARTED` (the earliest record's `_timestamp`, RFC 3339 UTC to the
  second), `TURNS` (the highest `model.turn`), `TOKENS` (`usage.total_tokens` summed only when every call reports it and arithmetic fits, `-` otherwise), `OUTCOME` (`failed` when any accounted call's outcome was `failed`, `cancelled`, or `abandoned` —
  a session the Drop sweep closed reads `failed`, not `no-answer` — otherwise `answered` or
  `no-answer` from the last turn's `answer.present`), and `SERVICE` (the
  `service.name` resource attribute, stored as `service_name`). `--json` prints
  `[{traceId, service, startedUs, endedUs, modelTurns, totalTokens, failed, answered}]`.
- `session show (--trace-id ID | --from-file PATH) [--json]` fetches every record carrying the
  trace and rebuilds the message vector from `agent.model.prompt` — the `full` first-turn list,
  then each `delta` in turn order — adds the last turn's `agent.model.answer`, which no later
  prompt carries, and takes each turn's usage and `duration_ms` from `accounting.model.call` (or historical `accounting.model.turn`).
  Scripts, their outputs, and every other tool exchange are the tool-call and tool messages those
  deltas already carry, so no other event is read. `--json` prints exactly the document
  `replay --from-file` reads back — `traceId`, `system`, `history`, `prompt`, `turns`, and
  `answer` — which is how a recording is kept, edited, and replayed with no receiver in the loop.
- `session replay (--trace-id ID | --from-file PATH) --model MODEL …` puts the recorded system
  messages (or a `--system`/`--system-file` replacement), the earlier exchanges, and the prompt to
  a model again, and answers every script the model writes from the recording. **It runs no
  capability unless `--provider` is given**: the first script the recording never ran is the
  divergence, and without live components the replay stops there and reports it as `stopped`;
  with them that script runs in direct mode — import-free, read-only, no network — and the report
  says `live`. `--skill DIR` mounts skills and drops the recorded listing; `--suggestions` offers
  `suggest_improvement` to the replayed model and prints what it recorded on stderr, as `prompt`
  does. The exit code is `1` only when the replayed session failed for a reason other than a
  divergence stop.

**`show` and `replay` need a transcript.** Both require the original session to have run with
`telemetryPayloads` on. Without it the accounting records are found but no `agent.model.prompt`
is, and the command fails with `trace <ID> has <N> accounted model turn(s) but no transcript; the
session was recorded with payload telemetry off, so its prompt and scripts cannot be replayed`. A
trace no record carries fails with `no telemetry records were found for trace <ID>`, and a
transcript event that is not the shape the loop writes with `transcript for trace <ID> is
malformed: <detail>`.

**The receiver flags** are shared by `list` and by a `--trace-id` source; a `--from-file` source
needs none of them:

| Flag | Environment | Default |
|---|---|---|
| `--openobserve-url` | `DEKOPON_OPENOBSERVE_URL` | unset; the command fails with `no OpenObserve URL; pass --openobserve-url or set DEKOPON_OPENOBSERVE_URL` |
| `--openobserve-stream` | `DEKOPON_OPENOBSERVE_STREAM` | `dekopon` |
| `--openobserve-auth-env` | — | `DEKOPON_OPENOBSERVE_AUTHORIZATION` |
| `--openobserve-timeout-ms` | — | `10000` |
| `--since` | — | `7d` |

The URL is the organization base the OTLP exporter posts to — the `http://127.0.0.1:5080/api/default`
of the export example above, so one deployment's endpoint is also its query base — and it must
carry no query, fragment, or userinfo. `--openobserve-auth-env` follows the rule every other
Dekopon credential follows: it names the environment variable holding the complete
`Authorization` header value, such as `Basic <token>`, and a value never appears in an argument.
An unset variable fails with `environment variable <NAME> is not set; it must hold the OpenObserve
Authorization header value`. The stream name is `[A-Za-z0-9_]`, and `--since` is a count followed
by `s`, `m`, `h`, or `d`, never zero.

The client posts `{"query": {"sql", "start_time", "end_time", "from", "size"}}` to
`<base>/_search?type=logs` over a microsecond window ending now, follows no redirect — so the
header cannot be forwarded to a host nobody named — uses no ambient proxy, bounds every response
at 32 MiB, and pages 500 records at a time for at most 20 pages, past which it prints `warning:
the search stopped after 20 pages of 500 records; narrow --since to see the rest` on stderr. A
trace identifier is checked against `[A-Za-z0-9._-]{1,128}` before it is interpolated into
`WHERE trace_id = '…'`, which is what keeps the lookup a lookup. OpenObserve stores an attribute
named `audit.event` as `audit_event` — its field names admit letters, digits, and underscores, and
fold every other character to an underscore — so the listing query is
`SELECT * FROM "dekopon" WHERE audit_event = 'accounting.model.call' ORDER BY _timestamp DESC`,
and the reader accepts either spelling of every attribute it reads. The same fold is what makes
`SELECT * FROM "dekopon" WHERE audit_event = 'agent.improvement.suggested'` the query that reads
suggestions back.

In telemetry, `list` searches under a `runner.session.list` span carrying `session.limit`, a
`--trace-id` fetch under `runner.session.fetch`, and a replay under `runner.session.replay`, which
carries the model identifier and backend, `provider.count`, `prompt.max_steps`, `prompt.skills`,
`prompt.suggestions`, and `replay.system_replaced`. The commands report to `runner.command.failed`
as `session.list`, `session.show`, and `session.replay`, with `error.type` including
`observe-url-missing`, `observe-credential-missing`, `observe` (rejected settings or a failed
request), `observe-task`, `recording` (no records, no transcript, or a malformed one),
`recording-json` (a `--from-file` document that is not a `session show --json` transcript),
`since`, `file-read`, `file-too-large`, `file-utf8`, or `clock`.

## Redacting secrets

`dekopon_core::Redacted<T>` wraps a value that must never be rendered in the clear. `Debug`,
`Display`, and `Serialize` all produce a marker instead, and the value leaves only through the
deliberately conspicuous `expose`. Persisting a real credential — the ChatGPT auth file is the one
case — requires an explicit `#[serde(serialize_with = "dekopon_core::serialize_exposed")]` per
field, so the safe behaviour is what you get by default and the exception is visible in review.

The marker is padded to the character width of the value it replaces, so a redacted field keeps the
shape of the record around it:

```text
sk-live-abcdef012345       ->  [     REDACTED     ]
short                      ->  *****
```

Below the width of `[REDACTED]` the word cannot fit, so the marker degrades to asterisks rather
than truncating into something that reads like a different token. Preserving width is a deliberate
operator choice and it does leak one fact — how long the secret was — which can narrow down an
issuer or credential class. It is a readability-for-metadata trade, not a free win.

A short-lived runner uses batch exporters and explicitly shuts down both providers before returning. SDK-reported flush failures make the command fail instead of being silently ignored. `--trace <PATH>` can still produce a local Chrome/Perfetto trace alongside OTLP export.

## Trace and log model

One generated OpenTelemetry trace links the command to spans such as:

- `runner.command`, `runner.prompt`, `runner.shell`, and `prompt.session`;
- `process.run` and `process.node` at `DEBUG` for process-lifecycle work;
- `accounting.model.call` and `model.complete`, with `chatgpt.refresh` nested inside the latter whenever
  a ChatGPT subscription credential is rotated or adopted — it carries `forced`, `outcome`
  (`adopted`, `rotated`, `rotated-unsaved`, or `failed`), `duration_ms`, and the new
  `credential.expires_at`, and never any token material;
- `prompt.script`, `shell.script`, and `shell.command`; and
- `provider.compile`, `provider.describe`, and `provider.invoke`; and
- `provider.run_command` at `DEBUG`.

`process.run` carries only its private stable `run.id`. `process.node` carries that `run.id`, a
private stable `node.id`, root parent, fixed `process.kind`, the `process.interruptibility`
contract (`non-interruptible` or `cancellable`), and terminal `process.outcome` (`succeeded`,
`operation-error`, `panicked`, `cancelled` for a requested cancellation of a cancellable node, or
`task-cancelled` for a runtime-driven abort). Tokio task IDs, scripts, argv, values, diagnostics, provider payloads,
and raw operation errors are absent. These spans are `DEBUG` so normal INFO telemetry volume does
not grow with frontend process nodes or with the command words a script runs; a diagnostic filter
may enable them. Three kinds exist. `legacy-shell` is the one non-interruptible node
`dekopon-run shell` wraps provider loading and the whole interpreter in, keeping the existing
`shell.script`/`shell.command` spans beneath it. `direct-command` is one non-interruptible node per
provider command word a direct-mode script runs (`shell`, `prompt`, and `session replay
--provider`), nested under the command's `shell.command` span; it stays non-interruptible because
the guest call blocks a thread the supervisor could not join. `broker-command` is one cancellable
node per command word the broker leg runs: `dekopond` ties it to the session's Stop, which aborts
and joins the round trip before the script reads `session-cancelled`, while `dekopon-run prompt
--broker` supplies no signal, so its nodes are cancellable in contract only. Nested nodes still
report `parent.id` `root`; real parent threading is a follow-up. The runner's and the gateway's
trace filters both include the `dekopon_process` target; when a sink disables these DEBUG spans,
`Span::or_current` keeps existing shell/provider work under the current parent. No public process
IDs, scopes, ports, or graph telemetry contract exists yet.

The runner's own `provider.invoke` — `dekopon-provider-host`, not the broker host — carries provider,
capability, component path, `input.bytes`, `output.bytes`, and `fuel.remaining`. Counts and fuel
only; the payloads themselves are governed by the span-payload opt-in below. A call that dies rather
than returning also emits a `WARN` naming which wall it hit — the wall-clock deadline, the output
ceiling and its configured bound, or a trap inside the component — because the runner's shell seam
flattens errors to their message and would otherwise leave nothing in telemetry saying why.

The runner's `provider.run_command` — again `dekopon-provider-host` — is one span per provider
command-word run and carries `provider.id`, `provider.path`, `command.export` (`run-command`, or
the legacy `resolve-command`), `input.bytes` (argv plus the piped value), `output.bytes`, and
`fuel.remaining`; never the argv, the piped value, or the text the guest rendered. It is `DEBUG`
rather than `INFO` because a command run is not budget-bounded the way a capability call is, so
the span that runs the word carries the outcome once instead. A run that dies emits the same
`WARN` naming the wall it hit as `provider.invoke` does, and an argv-plus-stdin beyond
`max_input_bytes` is refused before a store exists. Every provider command word a direct-mode
script runs reaches this span through the runner's `direct-command` process node.

One model turn drives at most a handful of scripts, and one script drives many capability calls, so `prompt.script` is the span for a whole unit of model-requested work rather than for a single capability invocation.

Inside it, the interpreter opens one `shell.script` span per run, and inside *that*, `shell.command` is one span per command word the script actually ran, in execution order — a builtin, a capability call, a shell function, a word this shell refuses, or a word that resolved to nothing. A trace therefore reads as the ordered list of commands a script executed rather than as one opaque entry, and the reading survives constructs where one script word drives several executions: `xargs` mapping a command over ten items produces ten `shell.command` spans nested inside its own. The interpreter emits these as plain `tracing` spans and knows nothing about OTLP; `dekopon_shell` is already named in this file's trace and log filters, so they flow to every configured sink with no further wiring. Each command span carries:

| Attribute | Value |
|---|---|
| `shell.command.name` | The command word, or `<withheld>`; see data minimization below |
| `shell.command.kind` | `builtin`, `capability`, `function`, `control`, `rejected`, `not-granted`, or `not-found` |
| `capability.namespace` | Present only on `not-granted`: the provider namespace, taken from the session's own granted set |
| `shell.command.argument_count` | How many arguments the word received, never their values |
| `shell.command.exit_code` | The status the command reported |
| `outcome` | `succeeded`, `failed`, `denied`, `not-found`, `usage-error`, `timed-out`, `limit-exceeded`, or `rejected` |

Only the first 256 command words of a run get their span at `INFO`; the rest are emitted at `DEBUG`
and so are off wherever `RUST_LOG` is `info`. This is a volume bound, not a detail one. A
model-authored `while` loop is bounded only by the step budget (default 100,000) and the script
deadline, so one bash tool call can execute tens of thousands of command words, and a span for each
is tens of megabytes exported from a workload whose whole point was to be a single round trip. What
survives the cap is the `shell.script` span, whose counters describe the whole run in constant size:

| Attribute | Value |
|---|---|
| `shell.script.commands` | Command words the script executed, loop iterations and `xargs` sub-invocations included |
| `shell.script.commands_traced` | How many of those carried an `INFO` span |
| `shell.script.capability_commands` | How many were a capability call or a provider command word |
| `shell.script.failed_commands` | How many reported a non-zero exit code |

`not-granted` splits the swing-and-a-miss out of `not-found`. A word that parses as a capability
identifier, in a namespace this session *does* hold but naming a capability it was not granted, is a
different fact from a typo: it is a model repeatedly reaching for something an operator may want to
grant. A trend of them in one namespace is the signal worth acting on.

Only the **namespace** is exported, and it comes from the session's granted set rather than from the
script — a string the deployment chose, never one the model composed. The word itself stays
`<withheld>` unless payloads are enabled, because everything after the namespace is whatever the
script typed. The script cannot tell the two apart: both print `command not found` and exit 127,
since a model that could distinguish them would have an oracle for enumerating the deployment's
capabilities one guess at a time.

`outcome` keeps a policy refusal (`denied`) distinct from a capability that ran and errored (`failed`) and from one that is unreachable (`not-found`), mirroring the interpreter's own exit-code mapping; flattening them would hide an authorization refusal in the noise of ordinary failures. `rejected` and `limit-exceeded` name the two ways a command ends the whole script — a construct this shell excludes, and an exhausted sandbox budget.

Structured log records use stable `audit.event` attributes. They no longer mirror spans: a command's start, end, duration, parent, and outcome all live on its `shell.command` span, so the log stream carries only accounting, refusals, errors, and — when opted in — payloads. `runner.shell.unobserved` is the exceptional lifecycle record emitted, while the owning Tokio runtime remains alive, when a dropped `ProcessRun::execute` caller cannot receive its shell outcome: it carries only `command.name`, low-cardinality `outcome`, and `error.type`. A successful abandoned `CommandOutput` is never rendered or logged; an abandoned error's complete cause goes only through the runner's ordinary operator stderr reporter. `agent.command.unobserved` is the same record for one command-word run on either leg (`command.leg`), emitted by `dekopon-harness` for the runner and the gateway alike; its failure cause goes out as an ordinary error event beside it, never inside it. Logs emitted inside the runner trace carry its generated `trace_id` and active `span_id`, allowing an OTLP log result to pivot to the corresponding performance trace.

## Data minimization

Telemetry includes operation names, model/provider/capability identifiers, bounded counts, outcomes, durations, and source locations. It intentionally excludes:

- user and system prompts;
- model response text and reasoning replay data;
- model tool-call IDs and the script text a model authors, along with that script's output;
- provider input and output (including every durable-memory query and turn);
- inbound chat message text, durable memory content/query/scope, and canonical subject identifiers of the people who sent it;
- activity targets, owned-thread workspace/channel/thread/sender claims, native status text, reaction
  names, and raw service errors;
- chat bot tokens and the environment variable values behind every configured credential;
- command arguments, in every form and at every level;
- bearer tokens, OTLP authorization headers, and provider credentials; and
- broker socket paths.

Command arguments deserve their own line because `shell.command` is the newest place they could have leaked. A `curl -d '{"apiKey":...}'` body and a `cap some.id '{"token":...}'` object are capability input wearing argv's clothes, so only the argument *count* is recorded. The command word itself is recorded only when it came from a fixed vocabulary the interpreter owns — a builtin name, a control word, a word the shell refuses by name, or a capability identifier. A shell function's name and a word that resolved to nothing are whatever the script's author typed, so both are reported as the literal `<withheld>` and the resolution kind is left to say what happened.

Model-selected invalid tool names are not copied into remote rejection events; a rejection records a stable category such as `unknown-tool` instead. Error telemetry records stable categories rather than raw errors, which may contain untrusted provider or transport text. Normal command stdout/stderr remains a separate output surface.

The immediate Wasm world has no logging import, so this records host-observed guest lifecycle and timing rather than arbitrary text emitted from inside a component.

## OpenObserve development and CI

[`../examples/otel-traces/`](../examples/otel-traces/README.md) starts one pinned OpenObserve container with one Docker volume, documents authenticated OTLP/HTTP export, and explains how to inspect traces in the UI.

`examples/otel-traces/smoke-test.sh` is the repository-level black-box check. It starts an isolated OpenObserve instance, executes a real direct provider invocation, and searches both signal streams. The trace query asserts that the root runner span and provider spans arrived without the sentinel provider input; the log query asserts that a correlated record carrying the same trace ID arrived and that the sentinel stayed absent there too. CI runs the same script and removes the container and volume afterward.

### Raspberry Pi storage snapshot

A physical-allocation measurement on the project's Raspberry Pi OpenObserve deployment on 2026-08-16 found that the `dekopon` streams used to exercise a simple prompt occupied **200 KiB** of signal payload: **148 KiB of traces** and **52 KiB of logs**. The whole OpenObserve `stream/` tree occupied **1.22 MiB**, comprising 388 KiB of log/trace payload across all streams plus 860 KiB of indexes, metadata, bloom data, and directory overhead.

These are allocated filesystem blocks, not sparse apparent sizes, and they are a point-in-time development sample rather than a per-prompt storage guarantee. Prompt turns, script/command count, payload opt-in, backend indexing and compaction, and retention all change the result as ingestion continues. The useful conclusion is the order of magnitude: metadata-first correlated telemetry is practical on small self-hosted hardware without pretending that indexes or other streams are free.
