# Chat progress, liveness, and streaming

One vocabulary in the agent loop, one policy in the gateway, one driver per chat transport, and
one cancellation path. This document is the design of record for that feature: what the types are,
where each event is emitted, what each transport can natively show, and which limits are accepted
rather than solved.

**Status: Current.** Every shape named here exists in the tree. The provider limits in the tables
are what the services documented at the time of writing; the ones marked as open are recorded
under [Accepted limits](#accepted-limits).

## The shape in one paragraph

The prompt loop threads synchronous observers through `dekopon_agent::prompt::SessionInputs`. A
third one, `ProgressSink`, receives a `ProgressEvent` wherever a turn, a fragment of text, a
command word, or an attachment starts or ends. The gateway's adapter writes each event to the
message's W3C trace and hands it to a per-session policy task, which owns timing, budget, and
every decision about what a person sees. The policy renders through `ChatDriver`, one trait per
transport whose capability accessors return an implementation or nothing. `dekopon-model` streams
Server-Sent Events on the same blocking thread the loop runs on and calls back per event, which is
also what makes a model turn interruptible between reads instead of only between turns.
Cancellation is one `CancelSource` with three origins feeding the compare-and-swap the session
already had.

## Three rules every production chat bot converges on

1. **Progress never notifies.** Reactions, in-place edits, and platform status fields sit outside
   message history. A new message is the floor, used once, only where nothing else exists.
2. **Progress is ephemeral and id-addressed; only the answer is durable.** The progress surface is
   finalized into the answer or deleted; nothing about it survives a restart.
3. **Keep-alive is a lease renewal, not prose.** Typing indicators expire in seconds and are
   renewed on a timer. Text cadence is an order of magnitude slower than lease cadence.

A fourth, from every agent protocol surveyed: cancel is a terminal outcome, not an error. Dekopon's
is cooperative, at the next boundary, and never rolls back. Streaming moves that boundary from
"after the model call" to "after the next stream event".

## What each surface can natively show

| Surface | Liveness | Edit in place | Streamed text | Cancel affordance |
|---|---|---|---|---|
| Slack agent experience | native session status, with the service's own Stop control | `chat.update`, 4,000 characters | `chat.startStream` / `appendStream` / `stopStream`, thread replies only | the service's Stop |
| Slack classic | reaction on the inbound message | `chat.update` | the same streaming methods, in a thread | a Block Kit button, acknowledged over the socket |
| Discord | typing lease, renewed inside 10 s; bot reactions | message edit, 2,000 characters | cumulative edits on a 2 s floor | a button, acknowledged within 3 s |
| Telegram | chat action, renewed inside 5 s; bot reactions | `editMessageText`, 4,096 UTF-16 units | cumulative edits on a 3 s floor | an inline keyboard, answered within the query's deadline |
| WhatsApp Cloud | typing indicator fused onto mark-as-read, auto-dismissed at 25 s | none exists | none | none native; the stop word only |
| Local | one JSON line per signal | re-emit under the same id | delta lines | a `{"stop": true}` line |

Reactions are advertised wherever the service has them, because a reaction is the cheapest signal
that exists at t=0 and it is the only thing that covers the first seconds of a slow first turn.

## Vocabulary

`dekopon-agent` owns it, in `src/progress.rs`, because the loop is the source. The gateway
consumes it.

```rust
pub enum ProgressEvent {
    Started { agent: String, max_steps: u32 },
    ModelTurn { turn: u32, of: u32 },
    TextDelta { turn: u32, text: ModelText, cumulative_chars: usize },
    Answered { turn: u32, tool_calls: u32, duration: Duration, first_delta: Option<Duration> },
    ToolStarted { word: CommandWord, argument_count: u32, calls_used: u32, calls_max: u32 },
    ToolFinished { word: CommandWord, outcome: ToolOutcome, duration: Duration },
    Attachment { index: u32, media_type: String, bytes: u64 },
    KeepAlive { elapsed: Duration, count: u32 },
    Cancelled { by: CancelSource },
    Failed { class: FailureClass },
    Finished { outcome: SessionOutcome, elapsed: Duration, turns: u32, tool_calls: u32 },
}

pub trait ProgressSink: Send + Sync {
    fn emit(&self, event: ProgressEvent);
}
```

The enum is deliberately **not** `#[non_exhaustive]`. Both crates are in one workspace, and a new
variant *should* break every consumer: a wildcard arm in the gateway's fold is exactly the silent
ignore that a reserved variant would produce. When a backend's reasoning summaries are worth
showing, adding `Reasoning { summary }` is a deliberate break, not a hole left open for it.

Who emits what:

- **`Started`** — the gateway, once, when the grant arrives. It owns the grant; the loop never
  sees it.
- **`ModelTurn`, `TextDelta`, `Answered`, `Cancelled`, `Failed`, `Finished`** — the prompt loop.
- **`ToolStarted`, `ToolFinished`, `Attachment`** — the broker leg, where the word, the outcome,
  and the acceptance already are. One started/finished pair per tool use, reported under the
  command word and started before its run. A command that proposes a capability call finishes with
  that call's outcome, or `Failed` when the call never happens (the grant is missing, the budget or
  the deadline ran out); one that ends without a proposal (help, a usage error, a failure, a stop)
  finishes with its own.
- **`KeepAlive`** — the policy's own clock. Nothing else synthesizes it.

`FailureClass::of` derives a class from `PromptError` with a `match` naming every variant, so a new
error kind is a compile error rather than a silent `Internal`. It answers `None` for a
cancellation, including a model stream that reported `Interrupted`: a stop is an outcome, and the
loop reports it as `Cancelled { by }` instead.

### Nothing else can travel

`ProgressEvent` carries metadata, with one deliberate exception and one bounded string:

- `TextDelta` carries `dekopon_model::ModelText`, which only the model client can build from bytes.
  A consumer may concatenate, measure, and shorten a value it already holds; it cannot manufacture
  one from a string it chose. Reasoning, tool-call arguments, and tool output are not that type and
  have no variant to arrive in.
- `CommandWord` is the provider-authored capability identifier or command word — the same
  vocabulary the trace already carries — bounded to 32 characters with a marker at construction,
  because it is rendered to a person.
- `ProgressText`, the gateway's own type, is built only from operator-authored templates plus
  numeric state, and drivers see nothing else.

A prompt, a shell argument, a provider result, and attachment bytes have no field anywhere in this
vocabulary. That is goal 1 enforced by the type system rather than by review.

## The driver

`ChatDriver` is one trait on the same per-transport objects that used to implement two, one for
replying and one for transient signals. Its capability accessors return an implementation or
nothing; `Some` means implemented, so there is no descriptor to keep in agreement with the methods
and no `Unsupported` error variant that only unreachable defaults would construct.

```rust
#[async_trait]
pub(crate) trait ChatDriver: Send + Sync {
    async fn reply(&self, target: &ReplyTarget, reply: OutboundReply) -> Result<(), TransportError>;
    fn typing(&self) -> Option<&dyn TypingLease> { None }
    fn status(&self) -> Option<&dyn NativeStatus> { None }
    fn progress(&self) -> Option<&dyn ProgressMessage> { None }
    fn stream(&self) -> Option<&dyn TextStream> { None }
    fn reaction(&self) -> Option<&dyn InboundReaction> { None }
    fn cancel_button(&self) -> Option<&dyn CancelButton> { None }
}
```

`reply` keeps its existing signature and contract: `Ok` means every chunk and attachment reached
service acceptance. Turning a progress surface into the answer is `finalize(message, reply)` on the
progress or stream object instead, so the one call whose success means "delivered" never hides a
fallback. When `finalize` fails the policy deletes the surface and falls back to `reply`.

Limits live on the capability object that enforces them — a progress message's character ceiling
and minimum edit interval, a stream's minimum interval — which is the only definition of each.
`LivenessTarget` says where a transient signal goes, and `MessageRef` names a message the gateway
posted and may edit, stream into, finalize, or delete. There is exactly **one** `MessageRef` per
session per transport: with streaming on, the stream is the surface and no separate status message
is posted, because an append-only stream cannot carry a status line underneath the text.

## The policy

One task per session in the gateway, holding the running/sealing/finished coordination its
predecessor had — sealing synchronously before terminal delivery, finishing in the background
after, "an issued call is never cancelled", and a two-consecutive-failures breaker — plus an event
inbox and a clock.

Discrete events ride a bounded channel; overflow is counted, never silently dropped. Cumulative
streamed text is a *value*, not an event, so it rides a watch channel: the policy reads the latest
value once per minimum interval, which is what it would do anyway, and no coalescing code exists.

Rendering, for whichever capability objects the driver returns, at the route's detail level:

1. Reaction and typing at `Started`; native status `Working` at `Started` and `Idle` at terminal.
2. A progress message posted on the first of `TextDelta`, `ToolStarted`, `Answered` with tool
   calls, or the 15 s keep-alive tick — never on `ModelTurn { turn: 1 }` alone, so a fast one-turn
   answer never gets one.
3. Every later event edits that message, coalesced to the latest state under the driver's minimum
   edit interval, 60 edits per session, 10 keep-alives.
4. Keep-alive at 15 s, 45 s, then every 60 s, always an edit and never a new post.
5. With streaming on and a `TextStream` present, the stream is the surface.

Detail levels are per route: `off` renders nothing but typing, status, and reaction; `plain` shows
verbs only, with elapsed reaching it on keep-alive ticks and nowhere else; `detailed` adds turn and
call counts, and elapsed on every edit. The defaults are what `plain` is shaped around: a counter
frozen between edits reads as a hang, which is the opposite of what the surface is for, and turn and
call counts are route budgets that mean nothing to the person waiting, so they stay on the trace. A
route that asks for `detailed` is spending that staleness deliberately, for an operator watching
their own agent rather than a person waiting on an answer.

Default templates are operator strings overridable per transport: `Working on it…`,
`Running {word}…`, `Still working ({elapsed_s} s)…`, `Stopped.`, and one fixed failure line.

Terminal handling has exactly **one** writer, the policy task:

| Outcome | What the person is left with |
|---|---|
| Answered | the surface finalized in place as the answer; attachments follow on the reply path |
| Stopped | the partial text kept, with the fixed stopped trailer, then the terminal stopped line |
| Failed | a progress message deleted and the fixed failure line as the reply; a streamed surface closed in place, with the partial answer above that line |
| Declined | a progress message removed and no reply at all; a streamed surface closed on exactly the text already on screen |

Two tasks writing the same conversation on cancel is the one race worth naming: the session's
cancelled branch hands the event to the policy rather than replying itself, and the policy does
finalize-then-reply in order through one driver. Nothing else posts a stopped line.

Every progress call has a 2 s deadline, honors the transport's rate-limit cooldown, never takes a
transport's reply lock, and cannot fail the session. A failed edit is dropped and counted; the next
tick renders the latest state. Two calls are exceptions, because a missed deadline is this task
giving up on a call the transport may still land. The call that *creates* the surface — the first
`post`, or the first stream render — stops that rung on one deadline miss rather than counting
toward two: a second attempt would post a second message beside one the session holds no reference
to. A `finalize` that misses its deadline is not followed by a delete either; the answer goes out
beside the surface, because a person can read a second copy of an answer and cannot read a deleted
one. Nothing is persisted: a restart forgets every progress message.

## Streaming the model

`dekopon-model` stays blocking and synchronous. `ChatModel::complete` takes a callback:

```rust
fn complete(
    &self,
    messages: &[ModelMessage],
    tools: &[ModelTool],
    options: &CompletionOptions,
    on_event: &mut dyn FnMut(TurnEvent) -> ControlFlow<()>,
) -> Result<AssistantTurn, ModelError>;
```

One method, not two. Streaming is not a mode a caller opts into: an implementation that cannot
stream calls `on_event` zero times and returns the same `AssistantTurn`, so there are never two
paths to keep in agreement. `ControlFlow::Break` drops the response body — which closes the
connection rather than returning it to the pool — and the call answers `ModelError::Interrupted`.

The loop's closure does three things: it appends each fragment to the turn's cumulative
`ModelText`, emits `TextDelta` with the running character count until the cumulative text passes
the outbound answer bound, and returns `Break` when the cancellation probe says the session was
stopped.

On `Interrupted` there is no `AssistantTurn` to record, so the closure's cumulative text is the
only account of the turn there will ever be: the loop records `agent.model.answer` with that
partial text and `stream.interrupted = true`, `accounting.model.turn` with `outcome = interrupted`
and no usage — the provider reports usage in the final event that never arrived — and returns the
session's cancelled error. `prompt.model_turn` carries `stream.deltas` and, when one arrived,
`stream.first_delta_ms`. Per-fragment trace records are not emitted: they carry nothing the
complete answer lacks and would put a log line on the trace per token.

`stream` is a field on OpenAI-compatible model entries only, defaulting to on; a subscription
backend whose endpoint is Server-Sent Events end to end always streams and has no such field, since
a parsed-but-unhonored field is worse than no field. The chat-completions accumulator tolerates
what real servers send: a null delta content on the role chunk, a null usage on every chunk, a
whole tool-call argument object in one fragment, and a repeated index treated as a new call once
the previous one has a name.

## Cancellation

Three origins, one path, one compare-and-swap.

| Origin | Enters at | Human sees |
|---|---|---|
| A person, native control | the service's own stop event | partial text kept with the stopped trailer, then the stopped line |
| A person, button | the transport reader, which acknowledges before the inbound send | the same |
| A person, stop word | `dispatch`, before the addressed check | the same |
| The operator | shutdown grace expiring | nothing written: the indicators return to rest and the last progress line stays as it was |
| A budget | `maxDurationMs` counted from `Started` | the fixed stopped line |

The stop-word matcher runs in `dispatch` **before** the addressed check, strips the bot mention
using the same forms the transport identity already knows, trims trailing punctuation, and matches
an operator-authored list case-insensitively (`stop` and `cancel` by default). It fires only when
the conversation has a registered session whose subject matches; anything else falls through to
normal routing, so "stop" said to an idle agent is still answered. Every session is registered,
whether or not liveness is on, because the stop word must work with progress rendering off.

Only the subject that sent the message may cancel. A button carries the conversation in its
payload, but the presser's identity comes from the interaction envelope and never from the payload;
another person's press is acknowledged so their client stops spinning and then ignored, with a
counted record.

A button acknowledgment happens inside the transport reader, before the event is handed to the
inbound channel — that send can block on a full buffer, and the service's acknowledgment deadline
is a few seconds. Where the service supports it the acknowledgment is an update of the pressed
message that replaces the button with `Stopping…` and empty components: one call, the button gone
atomically so a second press is impossible, no follow-up edit, and no reply lock taken.

An operator shutdown is the one origin that writes nothing. The grace period expiring aborts the
session task, so its locals drop — the cancellation guard first, then the progress handle — and the
policy task's biased select sees the terminal receiver closed and runs only its cleanup: the
service's indicators return to rest and the last progress line stays exactly as it was. Nothing
survives the process either, because a progress message is session state and a restart forgets it.

What cannot be interrupted: a read waiting on a silent socket, and a broker invocation already
inside the client. In both cases the person still sees the stopped line immediately, because the
policy renders on the compare-and-swap rather than when the loop notices, and the orphaned work
drains in the background with its answer suppressed.

## Configuration

```yaml
transports:
  - kind: discordGateway
    name: example
    liveness:
      mode: native                 # off | native   (typing, status, reaction)
      classicFallback: reaction    # Slack classic only
      progress: message            # off | message
      stream: true                 # default false
      cancelButton: true           # default false
      keepAlive: { atSeconds: [15, 45], everySeconds: 60, max: 10 }
      templates: { working: "Working on it…", tool: "Running {word}…" }
stopWords: [stop, cancel]
routes:
  - progressDetail: plain          # off | plain | detailed
    limits: { maxSteps: 12, maxCapabilityCalls: 16, maxDurationMs: 300000 }
```

Validation refuses `progress`, `stream`, or `cancelButton` with `mode: off`; `stream` or
`cancelButton` on a transport with no edit surface; a cancel button on a transport whose service
already owns a stop control; an unknown template placeholder; and a zero `maxDurationMs`, like every
other zero bound.

The block this replaces was renamed rather than extended, and nothing reads the old spelling:
writing it is a startup refusal naming `liveness:` — no alias, no migration read, no
warning-and-continue. [`upgrading.md`](upgrading.md) records the rename and the name it replaced.

## Telemetry

Every event rides the message's trace as a `gateway.progress` record carrying kinds, counts,
durations, and the command word — never text. Renders emit a debug record naming the transport, the
primitive, the outcome, and, for a stream render, the character count that was on screen, which is
what answers "what had the person actually read when they pressed Stop": the model's partial answer
is on `agent.model.answer`, and rendering lags it by the minimum interval and truncates at the
surface's ceiling. Degradation, exhausted budgets, dropped inbox events, and ignored stop presses
each have their own counted record. Every name is listed in
[`observability.md`](observability.md).

## Accepted limits

- **A silent stream cannot be interrupted.** A backend in a phase that emits no events — a
  reasoning phase is tens of seconds of exactly that — gives the callback nothing to run between,
  so a stop lands at the client's global deadline. The person still sees the stopped line at once.
  There is no cancel token, request handle, or socket shutdown on the HTTP client to add.
- **A restart leaves a stale surface.** A progress message says "working on it" on a run nobody is
  doing any more, and a button press while the gateway is down shows the service's own interaction
  failure. Nothing is persisted and nothing cleans it up; the trace explains it.
- **Rate limits are per app, not per session.** A workspace-wide edit tier is shared across
  concurrent sessions, so the per-session edit budget cannot prevent exhausting it. The transport's
  cooldown is the guard; progress calls never retry.
- **WhatsApp gets typing only.** No edit endpoint exists, and the typing indicator is one-shot
  unless re-posting it renews the lease, which is a live check against the real number rather than
  something the tests can answer. Until then, dead air after the indicator lapses is accepted.

## Non-goals

Reasoning summaries in chat; a second progress surface alongside the stream; keep-alive messages
and buttons on a transport with no edit surface; multi-message progress threads and checklists; a
"finish the current step" cancel mode or any rollback; and streaming tool-call arguments or
reasoning to a person, which the `ModelText` type makes structurally impossible.

## Testing

The local transport is the reference driver: it implements every capability object and its line
stream is what the gateway integration tests read. Beyond it, the fixtures that matter are a
runtime that parks inside a script after reporting a tool start, a model that emits a scripted
sequence of fragments with a rendezvous between them, a model that emits nothing at all so the
parked-stream limit above stays pinned, recorded stream transcripts for both backends, and a
recording driver with per-capability failure injection. Time is paused for budgets and ticks.

The matrix worth keeping: every cancel origin against every stage (before the first turn, between
fragments, during a parked tool, during delivery); a second cancel ignored; another subject's press
acknowledged and ignored; the same event stream rendered at each detail level on each driver; and a
trace assertion that no record carries a planted prompt, script, or tool-result string.
