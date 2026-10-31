# Gateway routing, sessions and conversations

How a chat message reaches an agent session, what the gateway keeps between turns, and how each
turn is authorized. Read [Routing](#routing) and [Sessions](#sessions) to run a route, and
[Conversations](#conversations) before changing a route's `memory:` block.

- `gatewayd.md` owns every key named here and its default;
  memory settings owns the journal, summary and retention keys.
- [`security-model.md`](security-model.md#conversation-memory-as-a-trust-surface) owns the threats
  conversation memory accepts; [`inference.md`](inference.md#how-scoped-conversation-memory-works)
  owns what reaches the model request.
- `gateway-liveness.md` owns progress,
  stops, seals and the fixed stop lines; `gateway-transports.md` owns each
  chat service's behaviour.

## Routing

A route picks the agent that answers a message. It grants no authority.

- **First match wins.** Routes are checked top to bottom on transport name, `conversation:` and
  `subjects:`; nothing sorts by specificity. Put a named-channel route above a catch-all.
- **Conversation.** A message's conversation is its `kind` (`directMessage`, `groupDirectMessage`,
  `channel` or `thread`), its `container` (Slack team, Discord guild, WhatsApp
  `waba:phoneNumberId`; Telegram has none) and its `id`. A thread's `id` is its parent's, so
  `kind: [channel]` excludes the threads under a channel, `kind: [channel, thread]` claims both and
  `kind: any` claims every kind the transport produces. A private Discord thread therefore inherits
  its parent channel's grants.
- **`ids` is optional.** Without it a route matches every conversation the bot is invited to, so a
  new channel needs no configuration change.
- **`subjects:`** narrows a direct-message route to named canonical subjects and is refused beside
  any other kind. Other senders skip this route and fall through to later routes. Authority stays with the broker's
  subject mapping and Cedar.
- **Unmatched messages** are ignored with debug `gateway_message_ignored` (`reason = unrouted`,
  `conversation.kind`, `conversation.container`).
- **Addressing.** Every kind except `directMessage` needs the bot addressed to start a turn:
  `<@BOT_USER_ID>` on Slack, the bot user (`mentions`) or its managed role (`mention_roles`) on
  Discord, `@botname` on Telegram. A route never changes that. Transports admit some unaddressed
  messages: Slack Agent owned-thread continuation,
  thread follow,
  pull-request links, and the rest of a Telegram
  album whose first message was addressed. Others are ignored with `reason = not-addressed`.
- **fauxlack.** Test-only subjects are `fauxlack.<server>.u<id>` people and
  `fauxlack.<server>.c<id>` spaces. A fauxlack conversation's container is the subject's server;
  channel and thread conversations carry an `m<n>` thread and direct messages none.

A catch-all route changes where mapped people can reach the bot, not who they are. Every session
opens an attested broker leg; the broker maps the sender to a principal and requires `agent.prompt`
before any model call ([Authorization flow](#authorization-flow)). Two people in one channel keep
separate histories unless the route sets [`sharedConversation`](#scope-selects-the-replay-audience).

## Sessions

Each admitted turn runs one session through five steps. On a `oneShot` route
(`memory: { mode: oneShot }`) each session starts without history.

1. **Admission.** Running work is keyed by `(transport, conversation key)`.
   - With nothing running, a new turn takes a permit from the process-wide `sessions.maxConcurrent`
     pool, or is refused as busy (`busy.cause = saturated`).
   - With a session running, the asker's next native chat message steers it. On a mapped channel
     with a `sharedConversation` or `oneShot` route, another participant's native chat message also
     steers it once freshly authorized ([Participant legs](#participant-legs)); it waits while the
     first mapping lookup is pending. Everything else queues as a follow-up, including other
     senders' messages on any other route, wakes, GitHub deliveries, and input arriving after the
     turn is cancelled or sealed.
   - Steers and follow-ups hold at most 100 items per conversation. A full mailbox is refused as
     busy (`busy.cause = same-conversation`) and logs `gateway_steer_refused`
     (`reason = mailbox-full`).
   - A refusal posts `I'm busy — try again shortly.` when `replyOnBusy` is set (a collected batch
     always may), a refusal-reply permit is free and the transport accepts it.
   - Follow-ups run inline under the same permit, each with its own route, subject, cancellation,
     broker leg and trace. A conversation keeps its permit until its mailbox is empty.
2. **Authorization.** A sealed thread is
   refused first. Every other session opens an attested broker leg with
   `capabilities(subject, agent, scope)`; a refusal replies as in [Refusals](#refusals) and starts
   no model call and no progress. An admission acknowledgement on queued input does not mean it was
   authorized.
3. **Liveness.** Where the transport opted in, one session-owned task shows progress from the fresh
   grant onward (liveness).
4. **Execution.**
   - On a `persistent` route the session looks up its [conversation](#conversations) after step 2,
     because the grant comparison needs the fresh grant, and seeds what survives ahead of the new
     message.
   - The model is the route's `model`. Scripts run on the asker's broker leg, or on one current
     participant's leg chosen with `as` on a mapped channel.
   - The system prompt is the agent's `instructions`, then the [durable memory](#durable-memory-after-transport-acceptance)
     note when granted, a note on Telegram and WhatsApp that the chat shows only PNG and JPEG, the
     mapped-channel participant frame, a pull-request-link turn's review instructions, and one line
     rendered from the route's limits: `[gateway: N steps, M capability calls and W minutes per message.]`.
     The duration renders in seconds when `maxDurationMs` is not whole minutes and is absent when
     it is unset. Instructions are read fresh each session and never stored in history.
   - The agent's catalog skills follow in a second system message, by name and description;
     `read_skill` loads one on demand (skills).
   - Every session offers `bash`, `suggest_improvement` and the no-argument `wake_reason`, which
     reports whether chat, a GitHub webhook, a wake or a job started the turn and grants nothing.
     `inspect_agent_config` is offered only where the route sets `inspectAgentConfig: true`.
     `suggest_improvement` writes `agent.improvement.suggested` to telemetry and never to chat
     (improvement).
   - Shell bounds are the shell's defaults, except `maxCapabilityCalls` and `scriptTimeoutMs`,
     which come from the route. Every model request carries a [prompt cache key](#the-prompt-cache-key).
5. **Answer.**
   - The bounded final text and accepted provider attachments go to chat.
   - A failed required turn posts the fixed line from
     liveness, or the `failed` template:
     appended to a streamed partial answer, otherwise as a fresh reply after removing any progress
     message. Failure detail goes to `gateway_session_failed` (`category`), never to chat.
   - A stop finalizes the progress message in place, except that a steer staged after the post gets
     a fresh reply; follow turns post nothing.
   - A successful turn with empty text and no delivered image posts `[empty response]`, which durable
     memory records; an empty answer with nothing queued gets the empty-answer stop line.
   - A follow turn posts no failure line or partial answer. When capability work went unreported,
     every turn posts `The agent attempted capability work but could not report the result. Check the audit before retrying.`
   - A follow batch or an inherited Slack Agent continuation may call `decline_chat_reply` before
     capability work; the turn then posts nothing
     (continuation boundary).
   - A `persistent` route stores the textual exchange as one turn, trims the window and restarts
     the idle clock. A declined or failed session stores its question with no answer; a session
     refused at step 2 stores nothing. Stop and failure lines, stop notices and attachment bytes
     never enter history.
   - A generation lease makes a commit from older in-flight work inert after a grant change,
     empty-grant removal, idle replacement or capacity eviction; work in one live generation appends
     in completion order.

### Refusals

| Case | Reply | Record |
|---|---|---|
| Unmapped sender | `I only talk to the Lange family. I've let my owner know you stopped by.`, at most one per second gateway-wide | owner notice `gateway.session.rejected` on every refusal (unmapped sender notices) |
| Mapped principal denied this agent, or an empty grant naming a principal | `Sorry <principal>, you don't have permission for this agent.` | `gateway_session_rejected`, `reason` `agent-denied` or `unauthorized` |
| Attestation refused, or an empty grant naming no principal | `You're not authorized to use this agent.` | `gateway_session_rejected`, `reason` `attestation-refused` or `unauthorized` |

- The broker names the unmapped or denied cause only for subjects its attestor grant's
  `namespaces` permit; every other subject gets the last row ([trust boundaries](security-model.md#trust-boundaries)).
- `<principal>` is the broker's principal ID. An empty grant does not mean `agent.prompt` itself was
  denied.
- Follow turns, space wakes and GitHub deliveries post no refusal; a pull-request-link turn posts
  none for an unmapped or denied sender.
- No refusal calls a model or capability, or keeps the message in memory or the journal.

### Steering, text and shutdown

- **Steering.** `steering: abort` (the default) interrupts only the in-flight model call, never a
  tool, script or provider call; `boundary` waits for the current step. Both stage steers before
  the next model call and replace a draft answer when another step remains. An interrupted call
  does not spend `maxSteps`. A run requests at most eight interrupts, then steers wait for the
  boundary; each follow-up starts a fresh budget. Only the asker's steers interrupt.
  Harness gotchas owns staging, leftovers and folding.
- **Text.** Inbound text is bounded to 16 KiB keeping the head; answers to 8 KiB keeping head and
  tail. Both truncations say so in the text.
- **Shutdown.** Transport readers stop and in-flight sessions, with their queued follow-ups, get
  `shutdownGraceMs` to finish. When the grace expires the gateway logs `gateway_sessions_abandoned`
  and aborts them: no later model or capability call starts, queued receipts become `abandoned`
  and queued wakes are lost; admitted ticks still deliver their completed fires. A model request or provider effect already in progress may still
  finish. Native work then drains on the blocking pool (runtime threads);
  the chart's termination grace covers both daemon drains plus `drainBudget.bufferSeconds`
  ([Kubernetes](kubernetes.md#draining-takes-both-graces-in-sequence)). A transport that exhausts
  connection recovery, or whose reader task panics, stops the
  gateway through the same drain.

## Conversations

**Status: Current.** A `persistent` route, the default, keeps a bounded history and replays it into
the next prompt, so a follow-up can say "and the second one?". `memory: { mode: oneShot }` opts out.

- History is private to each authenticated subject unless the route sets
  `scope: sharedConversation`, which shares it among participants in one exact routed conversation.
- On a mapped channel a shared route keys history by the channel's space, with a separate window
  per thread, and the space's grants gate replay and journal recall for every speaker. An asker
  with no grants is refused without evicting the shared window.

### The history lives in the gateway

The live window is in `dekopon-gatewayd`'s memory and never reaches the broker
([where the text goes](security-model.md#where-the-text-goes)). `idleTimeoutMs` bounds how long a
window stays in memory, not how long it is remembered: when a message finds no window, the route's
`recall` rebuilds one before the model runs, and a `journal` route recalls on every turn, resident
or not.

| `recall` | Recalls from |
|---|---|
| `none` | nothing; the window starts empty |
| `journal` | the gateway's own transcript under `sessions.journal` |
| `platform` | the chat service's recent messages; Slack and Discord only, refused at startup elsewhere |

**Journal.** With `sessions.journal` set:

- Every persistent route appends one record per turn after the reply, to
  `<journal path>/<agentId>/<stem>.jsonl`, where the stem is a SHA-256 of the complete conversation
  key. Directories are `0700`, files `0600`.
- A record holds the turn's original items (user text, consumed steers, assistant text, reasoning,
  and each tool call with the result the model received), then a header line with its grant, model
  identity, summary slots and plan, attachment inventory and next `chat-asset:` number. Summary,
  pin and decider-choice changes append as short lines.
- On a route whose `recall` is not `journal`, the record keeps only the tool exchanges; the route's
  own window holds user and answer text.
- The record is written before the conversation's admission is released, so the next turn finds
  it. A stopped completion, or one stale at its lease commit, writes nothing. A failed append is
  reported without claiming retention.
- Recall walks the newest `2 × maxTurns + 1` headers and keeps the trailing run written under the
  current grant, inside both `forgetAfterMs` and memory `maxAgeMs`, and after any seal. Readers
  skip a torn newest line and never take the journal lock; every rewrite writes a new file and
  renames it.
- After every delivered turn and at startup, each agent directory is held to its
  `retention.maxBytes` (else `retention.maxBytesPerAgent`) by evicting the oldest whole records
  across conversations. A record larger than the cap is not stored and evicts nothing
  (`gateway_memory_retention_failed`). Files untouched for longer than the longer of memory
  `maxAgeMs` and the journal routes' `forgetAfterMs` are deleted.
- A file the binary cannot load is deleted, never migrated: another format, a corrupt line, or the
  file `loading.marker` names after a start crashed while loading it.
- A wall-clock seal writes
  `<stem>.sealed` in the journal root, holding `{"atMs":<u64>}` with mode `0600`, and no transcript
  line. No turn clears it; its mtime expires it after the route's `forgetAfterMs`, the longest
  configured `forgetAfterMs` bounds its sweep, and a marker that does not parse is removed. A
  restart keeps the seal. A failed write logs `gateway_journal_seal_failed`. Without a journal,
  seals live in memory only.

**Platform recall.**

- A read takes at most `min(2 × maxTurns, 100)` messages and 5 s, author lookups included. A failed
  or late read logs `gateway_recall_failed` and the turn runs without it.
- Discord returns other people's text only when the application has the Message Content intent in
  the Developer Portal, whatever the transport's `messageContent`; without it those messages are
  skipped.
- Each message is labelled `[gateway: chat history, from <principal>]`: the sender's principal
  comes from its fresh leg, every other author's from a broker `capabilities` lookup in this chat
  scope. An author no principal names keeps its service user ID on a private route and becomes
  `unmapped participant` on a shared one. The label is service-reported authorship, not broker
  authentication, and each unmapped lookup logs broker `broker_capabilities_refused`
  (`reason: unmapped-subject`).
- A window already in memory catches up: the gateway reads the messages after the newest one the
  window took in and before the trigger, under the same bounds, drops the bot's own and any
  already taken in, and keeps the newest that fit in `maxBytes`. They reach the model as one turn
  ahead of the message, with no participant line. Slack bounds the read with `oldest` and
  `latest`; Discord pages with `after` and, when one page of 100 does not reach the trigger, reads
  the newest messages before it instead.

**Attachments.** A recalled window brings its attachments under the same `chat-asset:N` numbers;
bytes are fetched on demand. An attachment older than `forgetAfterMs` is not recalled. WhatsApp
media IDs expire after 7 days, so an older WhatsApp photo is named but cannot be opened.

### Scope selects the replay audience

| `scope` | Key |
|---|---|
| `privateConversation` (default) | `(agent, configured transport, conversation key, canonical subject)` |
| `sharedConversation` | `(agent, configured transport, conversation key)` |

- The agent and configured transport keep two agents, or two installations, from sharing history
  or attachments in the same chat conversation. Shared scope removes only the subject; it is not
  agent, team or cross-conversation memory.
- The conversation key is the transport's container, `id` and thread: on Slack, the thread the
  answer joins, so a message that starts a thread shares a key with its replies.

| kind | `sharedConversation` audience |
|---|---|
| `directMessage` | refused at startup on a `[directMessage]`-only route |
| `groupDirectMessage` | every mapped member of the group |
| `channel` | every mapped member who can invoke the route there; on Discord, the whole guild channel |
| `thread` | the thread's participants; a private Discord thread inherits its parent's grants |

Shared scope can show one participant's earlier prompt and the agent's answer to every other
participant; treat it as an audience expansion
([shared scope](security-model.md#shared-scope-widens-audience-as-well-as-duration)).

On mapped channels, retained participants’ person-originated messages open with:

```text
[gateway: authenticated participant: <principal>, handle pN]
<existing user text>
```

On other routes and for authorized overflow participants without a retained leg, the attribution is handle-free:

```text
[gateway: authenticated participant: <principal>]
<existing user text>
```

- `<principal>` is the broker principal the attested sender maps to on its fresh leg; the gateway
  prints it and decides nothing from it. When the broker names none, the line is
  `[gateway: unmapped participant]`, with no platform identifier.
- The gateway writes this first line on the prompt and on the stored turn; user text cannot
  replace it. The label is kept in history and counts against `maxBytes`.
- **The participant's principal name reaches the model provider on every person-originated turn in
  every scope.** The telemetry gate controls exports only. Choose the model provider, and shared
  scope, with that in mind.

The state key is not the admission key from step 1, which has no agent, scope or subject, so
admission does not serialize every shared-state race. A generation lease and an attachment fence
close it: removal, replacement or eviction makes older leases inert, and stale work can neither
recreate history nor fetch attachments through a retired generation
([concurrency](security-model.md#the-second-order-case-history-outliving-its-grant)).

### Prior turns are compacted

A window route keeps `(user message, final answer)` pairs, or the message alone when the session
failed or declined. The message includes consumed steers and never a gateway notice. One script's
output can reach 256 KiB (`dekopon-process`'s default `max_output_bytes` in
[`../crates/dekopon-process/src/lib.rs`](../crates/dekopon-process/src/lib.rs)), so replaying full
transcripts would let one turn exceed the whole window.

A journal-recall route projects its recalled records before the model call:

- The newest `hotTurns` records replay verbatim: user and steer messages, assistant text, and each
  tool call followed by its result. Each may use `maxBytes / hotTurns`; text is kept first, and
  tool results share the rest, cut at a safe UTF-8 and JSON-escape boundary. An argument that does
  not fit, or is not a JSON object, replays as `{"cut":N}`.
- Provider reasoning replays only to the model that produced it.
- An older record replays at its pin, else its decider choice, else its deepest generated summary,
  else verbatim. Summary and metadata rungs are envelopes (`[memory #41 · time · summary 3/3]`, the
  summary, one line per tool call). These envelopes start in one leading
  `[gateway: earlier conversation]` user message and never replay executable tool calls.
  The `asset` target and attachment pins are available only when `summaries.attachments` enables
  images or documents. Each attachment pinned to `native` follows its record's envelope as a
  separate native user message; later envelopes continue after it.
- A repack runs when the projection exceeds `maxBytes`, holds more than `2 × maxTurns` records, or
  is absent while at least `2 × maxTurns` records exist.
  It keeps the newest `maxTurns` records, then steps the oldest down a rung (summary, metadata,
  counted omission), hot records last and whole, until the projection fits three quarters of
  `maxBytes`. Rungs change only at a repack, when a record leaves the hot window, or when its pin
  changes, so consecutive requests differ only at the newest edge.
- The request-only `[gateway: memory]` notice names every record the window omitted.
- With `selection: {decider: …}`, at most one broker-governed invocation per turn asks about the
  oldest older record that has no choice, all summary slots final and at least one generated
  summary. Its labels are that record's `summary-<n>` IDs and `metadata`; its input is the decision
  provider's `decide.choose` request with the configured `backend`, `model` and
  `requireProbabilities` and one `{tool, outcome, exitStatus?, ageMs, bytes, pinned}` evidence row
  per recalled record (newest 256), never historical text. The choice is written after the reply
  and applies at the next repack; a refused, ungranted, invalid or failed decision leaves the record on the
  ladder.

After the reply, the summary engine fills a journal-recall record's slots in the background, part
by part, through its plan's model, with the record's non-opaque text cut to `maxInputBytes`:

- No reply, steer or next turn waits on it. `sessions.memory.summaries` bounds concurrency and
  queue; a part that does not fit the queue fails as `overflow`, with every later part.
- A part ends generated or failed with its cause, never synthesized
  (history records).
- The model's `timeoutMs` bounds each call. Shutdown cancels running calls without lengthening the
  drain; a part left running resumes at the next start.
- The plan is frozen into the record, so a reload changes only new records, and a plan whose
  model was removed stops as `unconfigured`.

Without a journal, intermediate commands and output are lost across sessions. With one, a record's
original stays readable through `read_memory_record` while the record is eligible; a record and its
summaries are kept and removed together. The broker audit log stores no conversation text.

### The bounds

`gatewayd.md` owns `idleTimeoutMs`, `maxTurns`, `maxBytes`, `recall`,
`forgetAfterMs` and `sessions.maxConversations`, with their defaults.

- `maxTurns` and `maxBytes` both apply, oldest turns dropping first until both hold. When the newest
  turn alone exceeds `maxBytes`, its user text is cut to a UTF-8-safe prefix ending in `[…]` if the
  whole answer and the marker fit; otherwise the whole turn goes.
- The window, not a provider cache, bounds what each message pays. `idleTimeoutMs` defaults to 15
  minutes to serve a person's follow-up, not any provider cache lifetime
  ([provider retention](inference.md#provider-retention-what-can-be-said)).
- `sessions.maxConversations` bounds memory, not admission: reaching it evicts the least recently
  used conversation. `gateway_conversation_evicted` names the reason, so a ceiling set too low shows
  as churn.
- No timer evicts. The lookup that would use an idle entry drops it, and the write that would
  exceed the ceiling evicts. Closing a generation makes its attachment metadata and bytes
  unreachable at once; the asset map prunes the inert metadata on its next operation.
- The live window dies with the process; eligible journal records survive a restart.

### Participant legs

A mapped channel is one shared stream: each script runs on one current participant's broker leg,
the asker's by default.

- A retained participant's later messages and the asker's steers reuse that participant's leg for
  the turn; an overflow participant is authorized on every message before it joins. This does not
  widen which unmentioned messages a transport admits. An unauthorized or empty leg drops the
  message with one `gateway.admission` refusal record.
- Each newly authorized participant gets the [labelled first line](#scope-selects-the-replay-audience),
  then a join line summarizing read and write by provider from their broker answer and channel
  downscope. Display names stay outside the authenticated line.
- The session retains at most eight legs, the asker's included. Later participants are still
  authorized; their temporary leg closes before their text joins, with no handle, and their
  admission record carries `participant-limit`. Join lookups belong to the asker's turn trace.
- Retained legs get gateway-minted handles: `p1` for the asker, then `p2` to `p8` by first
  authorized appearance. The table dies with the turn.
- `bash` accepts an optional `as`, for example `{"command":"probe upper --text hello","as":"p2"}`:
  every command word in that script runs on that participant's leg. An unknown handle is a tool
  error with no broker call. Routes without participant legs (DMs, private scope, unmapped
  channels, GitHub) ignore `as`, run on the asker's leg and trace
  `tool_call.ignored_arguments = "as"` with `reason = "unmapped-route"`.
- All legs share the session's call budget and cancellation and close with it. The broker
  authorizes the selected participant ∩ channel on every frame and records `participant.handle`
  and that leg's opening delivery coordinate; a handle is never an identity or policy key.
- Messages still queued when the final model step ends start later turns under their own
  participant, each with a fresh leg. Folding combines messages from one subject only.
- The participant frame tells the model to act on what the turn's owner asked or endorsed, or what
  a participant asked for themselves under their own handle, and to say whose idea it acted on.
  Handles in replayed history grant nothing in the current turn.

### Authorization is never cached

Every new turn opens a fresh attested broker leg and gets a fresh chat-scoped `capabilities` answer.
History is prompt text, never authorization input.

- The granted capability set is stored with the conversation and compared at each new turn,
  against the space's grants on a mapped shared route. Any difference drops the history and
  attachment generation and starts afresh.
- An empty grant removes the entry and closes its attachments, except on a mapped shared route,
  where it refuses the asker and keeps the window.
- On an unmapped shared route, participants with different capability sets reset the window between
  them; sharing never promotes one to the other's grant.
- The comparison covers capability identifiers only. Tightening a capability's constraints or
  credential under the same identifier keeps the history
  ([history outliving its grant](security-model.md#the-second-order-case-history-outliving-its-grant)).

### The prompt cache key

Every model request carries the key: `prompt_cache_key` on Codex and OpenAI-compatible backends,
`session_id` on OpenRouter. **It is a routing hint and never an access-control boundary.** It tells
the provider which requests likely share a prefix; it authorizes, isolates and hides nothing, and
each new session still opens a fresh attested leg.

The key is `gatewayd-conversation-` or `gatewayd-route-` followed by the first 32 hex digits of
HMAC-SHA256, under a 32-byte gateway-held secret, over a domain-separation prefix and the
length-prefixed fields below. Without the secret a provider cannot recompute a key or test a guessed
subject against one. A conversation keeps its key across idle and capacity eviction and a grant
change.

| Route mode | Key covers | Changes when |
|---|---|---|
| `persistent` | one scoped conversation: private `(agent, transport, conversation key, subject)` or shared `(agent, transport, conversation key)` | the agent, configured transport name, conversation or audience changes, or the secret does |
| `oneShot` | one bound route: `(agent, transport, route index)` | the route's agent, transport or position in `routes` changes, or the secret does |

- With `sessions.journal` set, the secret is `prompt-cache-key.secret` in the journal directory,
  written once with mode `0600` through a temporary file and a rename, so keys survive restarts.
- A file that is missing, not exactly 32 bytes, or readable by group or others is replaced and
  logged as `gateway_cache_key_secret_reset` with its `reason`; a failure to write it stops boot.
  Deleting the file resets only routing hints.
- Without a journal the secret is generated per process, so keys change on every restart. The
  secret never reaches logs, spans or the broker.
- A `oneShot` route's key is shared by every sender the route answers. The shared prefix is the
  agent's instructions, skills listing and tool definitions, which name nobody; a sender's own text
  diverges where it differs, so nothing sender-specific can hit.

[`inference.md`](inference.md#how-to-evaluate-caching-in-a-deployment) shows how to measure what the
key is worth.

### Retention

On a `persistent` route, chat text stays in `dekopon-gatewayd`'s memory for at least `idleTimeoutMs`
after the last message, and until a lookup on its key or the ceiling evicts it; a timed-out entry
never reaches a prompt.

- Without `sessions.journal` the daemon writes no conversation text to disk; active attachment
  payload leases use private temporary files. With it, journal records stay on disk until
  [journal retention](#the-history-lives-in-the-gateway) removes them.
- With shared scope, retained text and attachments belong to the whole conversation audience.
- Another process under the gateway UID is inside its trust domain
  (current process boundary); paging and core dumps are
  outside the daemon's control.

## Durable memory after transport acceptance

The gateway receives a `ChatMemorySurface` only when the agent is enabled for chat memory and the
broker freshly authorizes all three memory routes for this sender, agent and conversation
([durable chat memory](security-model.md#provider-storage-and-durable-on-demand-chat-memory)). The
storage namespace's conversation values are `channel := conversation.id` and
`conversation := conversation.key()`. Without the surface, recent/search, the `memory` word, the
prompt note, recording and namespace creation are absent.

When present, the model may retrieve on demand:

```text
memory recent --last N
memory search --query TEXT
```

- The model cannot resolve or invoke record. After model success the gateway bounds the answer
  once, asks the transport to accept those exact bytes, and only then opens one fresh broker client
  for one `recordDeliveredTurn` carrying the session's chat attestation.
- Only a turn started by a native chat message is recorded; any other skips with debug
  `gateway_memory_record_skipped`.
- The recorded user text is the bounded sender text followed by each consumed steer's raw text,
  joined by blank lines, without gateway timing or attachment notes. When user and assistant text
  exceed the 64 KiB recording ceiling, only the user text is cut to a UTF-8-safe prefix ending in
  `[…]`; prompt history is unchanged.
- The assistant text is exactly what the transport accepted. No response, denial, timeout, EOF,
  partial delivery or `outcome-unaudited` is retried, and none changes the delivered `answered`
  outcome; failures log `gateway_memory_record_failed`.
- Acceptance means the service accepted every part of the answer, never that a person read it.
  Slack and Telegram need an HTTP success status before `ok: true`, and Telegram also checks the
  topic (Slack,
  Telegram); the local transport accepts after
  `write_all` and `flush`.
- The request carries a service-specific inbound delivery identity, which the broker checks against
  the attested scope before creating the namespace, so one transport cannot alias another. Local
  identities hold a 128-bit OS-random boot nonce, the connection and a sequence, so restarts do not
  collide.

Durable retrieval is not conversation replay: a stored turn never enters a later prompt on its own.
Recording never deduplicates, so a redelivered message becomes a second stored turn. There is no
deletion or export UX and no encryption-at-rest claim.

## Authorization flow

```text
chat service            authenticates the sender
      |
      v
dekopon-gatewayd                subject = ExternalSubject::{slack,discord,telegram,whatsapp}(...), or the local caller's declared subject (routing metadata, not authority)
      |                 agent   = the route's catalog agent
      |
      | capabilities(subject, agent, scope)  ── empty ⇒ refuse, no model call
      | invoke(proposal, subject, agent, scope)
      v
dekopon-brokerd         attestor grant bounds the namespace
                        principals turn the subject into a principal
                        policy must permit agent.prompt for that principal and agent
                        policy conditioned on context.via decides what it may then reach
                        credentials resolve, the provider executes, audit records it
```

- The broker is the sole authority. `dekopon-gatewayd` supplies the subject and never the
  principal; a refused attestation is an audited denial against the gateway's own peer identity.
- Driving an agent is its own policy statement, `Dekopon::Action::"agent.prompt"` over
  `Dekopon::Agent::"<name>"`. A mapped subject the owner never permitted is refused before the
  capability listing is assembled, and a chat-attested `invoke` in such a session is the audited
  denial `agent-denied`.
- **A policy written for direct peers can never authorize an attested context and vice versa**, so
  adding a gateway cannot widen an existing grant. [`security-model.md`](security-model.md) owns the
  full attestation contract.
