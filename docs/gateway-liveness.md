# Gateway liveness, progress and stopping a run

What the gateway shows while a run works and how a person stops it. [`chat-progress.md`](chat-progress.md) holds the design.

## Liveness, progress, and stopping a run

A long turn used to look like a hang. Layered `indicators:` determine what a routed conversation
shows while a session runs; `stopWords:` and `limits.maxDurationMs` end a run early.
An optional leaf resolves from built-in defaults, `sessions.indicators`, transport
`indicators`, its matching `indicators.conversations.<kind>`, then route `indicators`.
The last authored value wins, even when an earlier layer set `off`; absent and `null`
inherit without resetting unrelated leaves. Resolved `mode: off` suppresses all indicators.
For every route/kind, boot and each activated reload emit one bounded `gateway_indicators_effective` record
with route index, transport name, kind, enum settings and booleans only, not message text,
identity, coordinates or credentials.

**One message per session.** The policy posts once and then edits: a keep-alive is an
edit, and a streamed answer grows in place. The answer finalizes the same message unless a
steer was staged after it was posted. In that case, the progress or stream message is deleted
and the answer is posted fresh below the steer, to the same DM or thread. A steer staged before
any progress message existed keeps finalization in place. With `stream: true`, the stream is
the surface and no separate progress line is posted.

**Opt-in by default.** An absent indicator block or `mode: off` disables all intermediate
presentation. Both `reaction.working` and `reaction.seen` default to `off`. With
`mode: native`, omitted `progress` means `auto`, which prefers enabled native status,
then typing. `reaction.working: on` adds a reaction beside those surfaces, while `auto`
falls back only when neither is active; `status: off` and `typing: off` disable their
respective surfaces. Healthy Slack Agent status or Telegram/Discord typing
therefore needs no placeholder message; the complete answer is a fresh reply. WhatsApp's
working indicator is typing-only; its independently opted-in seen acknowledgement is 👀.
Slack's definitive native-status refusal exposes its configured reaction fallback in
the same session without changing the configured conversation experience. Transient errors do not
disable native status for future sessions.

Auto permits delayed progress only without a working indicator and where an editable surface exists.
Explicit `progress: message` keeps message progress alongside the ambient indicators; `progress: off`
forbids progress prose, not indicators or answer streaming. An explicit `cancelButton: true` with Auto
also permits a message-backed Stop control alongside the indicator. Startup refuses that control
with progress Off and no stream, or on a detail-Off route without streaming, including effective
conversation-kind overrides. Existing transport-specific button/stream refusals remain.

**When prose appears.** An eligible progress message is posted on the first of a text delta, a capability call starting, a
turn that drove one, or the 15-second keep-alive tick — never on the first model turn alone, so a
fast one-turn answer never leaves a "Working on it…" message behind the reply that obsoleted it a
second later. With the stream off the text itself is never shown, so the first delta posts the
working line and later ones change nothing: a model that has started writing is the news, not what
it wrote. Every later event edits that message, coalesced to the newest state inside the
transport's own minimum edit interval, with 60 edits and 10 keep-alives per session as the bounds.

**Detail levels.** `progressDetail` is per route, because the same event stream serves a family
Discord and an operations channel:

| Level | What the message says |
|---|---|
| `off` | No progress prose. Native indicators and explicitly requested answer streaming remain enabled. |
| `plain` | The verbs only — `Working on it…`, `Running gpt-image…` — with elapsed seconds on keep-alive ticks, where the number is fresh by construction rather than frozen since the last event. |
| `detailed` | The same verbs plus turn, capability-call, and elapsed counters on every edit. |

Detail controls only eligible progress prose, not the answer stream. A transport without an editable
surface shows no prose at any level.

**Limits.** Auto avoids submitting placeholder messages, not a guarantee of notification delivery.
Message progress and streaming may notify on creation and may not notify on final edits. Existing
Discord typing cooldowns can report success without a wire send, temporarily suppressing Auto's
message fallback even when no typing is visible. Lease visibility and actual notifications are not
verified by loopback tests; no cooldown or reaction-ownership redesign is included.

**Steering acknowledgment.** Accepted steers and queued non-wake follow-ups get a best-effort 👀 on their own
inbound message only with resolved `mode: native` and `reaction.seen: on`. The default route
sends no reaction or steer 👀. Slack uses `eyes` independently of working reactions
but honors reaction-scope availability; Discord, Telegram and WhatsApp send 👀. Local keeps its
`{"reaction": true}` frame. Slack does not track eyes for progress cleanup. Acknowledgment calls run
outside the admission lock with a two-second bound; failure is debug-only
(`gateway_steer_ack_failed`, with `error = deadline` when the bound expires), never a reply.
Wakes and messages without a liveness target receive none.

**Keep-alive.** `keepAlive: { atSeconds: [15, 45], everySeconds: 60, max: 10 }` is the default: two
early ticks that answer "did it hear me", then one a minute, ten times, then silence. Each tick is
an edit of the one message. Each tick is also scheduled from the one before it, so a task that woke
late ticks late rather than firing every offset it slept through at once.

**Nothing cosmetic can hold the answer.** Every call the policy makes — reaction, typing, status,
post, edit, delete, stream, finalize — has a two-second deadline of its own. A service that accepts
the connection and then answers nothing costs the waiting person those two seconds and no more: the
call counts as that rung's failure, two consecutive failures stop that rung for the session, and the
answer is delivered either way. `gateway_progress_rendered` reports it with `category=deadline`. The
call that creates the surface — the first `post`, or the first stream render — stops its rung on the
first deadline miss instead: the transport may still land it, and a second attempt would post a
second message beside one this session holds no reference to.

**Templates.** Seven operator strings, each overridable per transport, default to the sentences
this daemon ships. `tool` permits `{word}`; `working`, `tool`, `keepAlive`, `note`, and `noteEta`
permit `{turn}`, `{of}`, `{calls}`, `{calls_max}`, and `{elapsed_s}`. Only `note`/`noteEta` permit
`{note}`, and only `noteEta` permits `{eta_s}`; neither permits `{word}`. `stopped` and `failed`
render no placeholders. `stopped` covers person and operator stops; `failed` covers failures without one of the fixed cause lines below. A placeholder a field cannot render is a startup refusal naming it.
The command word comes from a provider manifest and is bounded to 32 characters. A bounded note
on a route with `progressNotes: true` is the one model-authored text admitted to a progress line;
no prompt, other capability argument, or provider result is admitted. Note text is inserted as a
literal value, never expanded as a template: a note containing `{elapsed_s}` shows those characters.
Slack, Discord and Telegram progress posts and edits disable link previews/unfurls. Discord
explicitly restores previews when the progress message becomes the answer; Telegram's edit defaults
already restore them. Ordinary answer and stream paths keep their existing behavior.

**Streaming.** With enabled liveness, `stream: true` and a driver that implements it, the model's answer appears as it
is written, cut to the transport's own character ceiling with a trailing `…` where it was cut, and
finalized in place when the turn ends. That is the transport half; the model half is `stream:` on
an `openaiCompatible` model, which is on by default and is also what lets a stop interrupt a turn
instead of waiting out `timeoutMs`. Turning it off is for the endpoint that claims
chat-completions compatibility and gets streaming wrong — a proxy that buffers the whole body, a
server that drops `usage` — and costs that model both properties; `kind: chatgptSubscription`
always streams and refuses the field.
During a reasoning phase the model may send no visible text. Model HTTP sends and reads observe
cancellation even then; remote work already accepted is not rolled back. A model-only steering
interrupt resets the local streamed partial, but text already visible in chat remains until the
next delta replaces it.

**Stopping a run.** Three ways, all of them authenticated, and all of them reaching the same
single decision:

- a **stop word** in the conversation. `stopWords:` defaults to `[stop, cancel]` and is an operator
  list because the people talking to a deployment do not all speak English. A message matches only
  if it is *exactly* one of those words after the bot mention and any trailing punctuation are
  stripped, case-insensitively — so `<@U0123> stop.` in a channel matches and "we should stop doing
  that" does not. It stops that sender's running turn or removes their pending collected/queued
  input. With no matching session or pending input, "stop" follows ordinary routing; an already
  cancelled session ignores a repeat stop rather than starting another turn;
- a **cancel button** on the progress message, where `cancelButton: true` and the service has
  interactive components. Slack's Agent experience renders its own Stop instead, which is why the
  flag is refused there. The value on a Slack button and the `custom_id` on a Discord one are the
  conversation key the transport minted when the message arrived, carried on the liveness target
  rather than re-derived at press time — a second spelling of that key is a press the admission
  map does not recognise. A Discord thread's key is `parent:thread`, and because a thread is
  itself a channel the press arrives in the thread: it is matched against the key's last segment;
- `limits.maxDurationMs`, counted from the moment the agent starts working rather than from
  receipt, because waiting behind another turn is not the agent taking too long.

Only the subject who started a run may stop it. User stops also discard that subject's queued
steers and follow-ups, leaving other senders' work intact; budget or operator cancellation does not
clear the mailbox. Another person's press cannot stop the running turn and is recorded as
`gateway_session_stop_ignored`, though it can remove their own pending input. A stop is cooperative,
not rollback: local model I/O is cancelled, while remote inference or provider effects already
accepted may still finish and their result is suppressed. Cancellation suppresses that turn's model
answer. Once the loop seals the turn, a stop cannot cancel that answer, even during delivery. It still removes that sender's
pending input. A stop word that removes pending input can receive `Stopped.` through the bounded
refusal-reply path; buttons/native Stop acknowledge through their transport. With nothing pending,
a stop during completion is ignored with reason `already-ended`.

The first `Started` opens the progress surface and fixes its deadline. Later `Started` events
and steers cannot extend it. Typing, keep-alive, edit throttling and stream flushing each have
an independent schedule; terminal delivery consumes the live surface.

A stop carries its cause to one fixed chat line, appended beneath any streamed partial answer:

| Cause | Chat line |
|---|---|
| Session wall clock | Stopped: this session reached its time limit. Capability calls already made were not undone. |
| Model deadline | Stopped: the model did not answer in time. Capability calls already made were not undone. |
| Empty model answer | Stopped: the model returned an empty answer. Capability calls already made were not undone. |
| Step limit | Stopped: this session reached its step limit. Capability calls already made were not undone. |
| Lost session task | Stopped: the gateway lost this session's task. Capability calls already made were not undone. |

These lines contain no diagnostic text and are not templates. Other model errors use `failed`.
The next session in the same conversation takes one resident notice before the user text:
`[gateway: the previous turn stopped before answering: <reason>. Capability calls already made were not undone.]`
The reason is `the person stopped it`, `the gateway stopped it`, `it reached its time limit`,
`the model call failed (<kind label>)`, `the model returned an empty answer`,
`it reached its step limit`, or `the gateway lost its task`. The notice never enters history or
the journal; eviction preserves it, consumption removes it, and restart loses it. A failed follow
turn leaves no stop notice, while a cancelled follow turn does unless its wall-clock stop seals a
thread. A sealed thread cannot take another turn, so its wall-clock stop leaves no pending notice.

Only a session wall-clock stop seals the conversation. A sealed thread, including queued
follow-ups, is refused before the broker connection or model call with
`Sorry, this agent took too long and we've canceled the chat. Please feel free to start a new one with a smaller scope.`
Its outcome is `sealed` and `gateway_session_rejected` records `reason="sealed"`.
Direct messages, group direct messages and channels are never refused by a seal: their resident
window is evicted with reason `sealed`, and the next turn starts fresh with the notice.
Platform recall, including resident catch-up, keeps timestamps at or after the later of the
normal horizon and the seal. Journal recall excludes timestamps at or before the seal.
