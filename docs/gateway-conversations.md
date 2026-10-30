# Gateway routing, sessions and conversations

How a message reaches an agent session, what the gateway keeps between turns, and how each turn is authorized.

## Routing

First match wins on (transport name, conversation, subjects). A conversation is where the message was posted: its `kind` — `directMessage`, `groupDirectMessage`, `channel`, or `thread` — plus the `container` above it (Slack team, Discord guild, WhatsApp `waba:phoneNumberId`; Telegram has none) and the `id` the service names. For a `thread`, the `id` is its **parent**, so `kind: [channel]` claims a channel and excludes the threads under it while `kind: [channel, thread]` claims both, and `kind: any` claims every kind the transport produces. A private Discord thread inherits its parent's grants: the grant direction is safe, because a thread's audience is a subset of its parent's.

`ids` is optional. Naming channels does not scale: one route per channel means enumerating service-native identifiers and editing this file again every time somebody creates a channel, and until an operator notices and redeploys, the bot is silent in the new one while appearing to be deployed workspace-wide. An absent `ids` says "wherever I am invited", which is membership the chat service already controls.

`subjects:` restricts a direct-message route to named canonical subjects and never widens one. It is accepted only beside `kind: [directMessage]`, because a per-person *channel* route — "in #general, Simon gets agent X" — is an access-control list by another name and the first thing somebody will trust as one; authority stays the broker's subject mapping and Cedar.

A DM route **without** `subjects:` answers strangers when the gateway has an attestor grant that permits their subject. An omitted `namespaces` list permits every subject, including under the chart default gateway grant `attestor: {}`; an explicit list permits only subjects within its namespaces. Subjects outside an explicit list keep the opaque unauthorized refusal and produce no owner notice. This grant condition discloses the unmapped cause without mapping a principal or granting any capability. An unmapped sender permitted by the attestor grant gets a broker round trip and the fixed reply “I only talk to the Lange family. I've let my owner know you stopped by.” The owner notice is the structured `gateway.session.rejected` audit record described in [observability](observability.md#unmapped-sender-notices), not a chat post.

Every broker-confirmed unmapped refusal emits the owner notice, including repeated messages and unrelated strangers during the reply cooldown; the incoming transport rate bounds these records. One gateway-wide slot admits at most one stranger reply per second. The slot stores only a timestamp, never a subject, message, grant or backlog, and every attempt still opens a fresh broker leg. Follow turns stay silent and GitHub's reply driver stays silent.

A mapped principal denied permission to start this agent, or returning an empty grant with a principal name, gets “Sorry <principal>, you don't have permission for this agent.” Each refusal emits one content-free structured gateway rejection record with transport, route, conversation, subject and reason, without the owner-notice selector. Here the name is the broker's validated principal ID and “this agent” is the display label for the gateway session permission; an empty grant does not imply `agent.prompt` itself was denied. Legacy refusals without typed cause or principal attribution, and other authentication failures, keep the fixed unauthorized reply. No refusal calls a model or a capability, or retains the message in memory or a journal.

Unmatched messages are ignored with a debug-level `gateway_message_ignored { reason: unrouted, conversation.kind, conversation.container }` — bots see ambient traffic, and silence is the correct answer, but which kind and which container went unclaimed is what answers "why did the bot not reply in here".

**Declaration order is the whole precedence rule.** Routes are consulted top to bottom, so a named-channel route written above a catch-all keeps that channel for itself while the catch-all takes everything else — special handling in `#incidents`, the default everywhere else. Nothing sorts by specificity: a hidden ranking is how an operator ends up unable to say which route answered.

Every kind but `directMessage` — a group DM and a thread included — initially requires the bot to be addressed: `<@BOT_USER_ID>` on Slack, a structured `mentions[].id` match or the bot's managed role in `mention_roles` on Discord, or `@botname` on Telegram. **A route decides which agent answers; an explicit address decides whether a new channel conversation starts**, and widening the first leaves the second exactly where it stood. Telegram and Slack Classic retain that rule on every message; an invited Discord channel/thread route with a live follow ticket accepts unmentioned messages after its bounded debounce. Slack Agent has one bounded exception: after an explicitly addressed message is freshly authorized, the same authenticated sender may continue without another mention inside that exact owned root thread. A continuation starting a new turn is authorized again and may decline to post; a same-sender steer joins the running leg. All non-owned channel history is dropped before routing. The Agent manifest therefore receives ambient public/private channel events, while the classic manifest remains mention-only.

The shared subject vocabulary accepts test-only `fauxlack.<tenant>.u<id>` people and
`fauxlack.<tenant>.c<id>` spaces. The shared chat-scope kind `fauxlack` supports all four
conversation kinds, with the container equal to the subject tenant. Channel and thread
scopes require an `m<n>` thread; DM scopes have no thread. The published gateway has no
in-process fauxlack service. Its test-only transport configuration parses in every build but
refuses boot without that service. Mappings and broker grants remain required.

### Being available in a channel is not authority

A route matching every channel widens no authority whatsoever, and an operator reading "available in all channels" must not read it as "available to everyone". Every session opens an attested broker leg naming the sender's canonical subject; the broker maps that subject to a principal, requires policy permitting `agent.prompt` for it, and refuses an unmapped sender before any model call is made. A catch-all route changes *where* the mapped people can reach the bot. It does not change who they are, and somebody the owner never mapped gets the same refusal in a catch-all channel that they would have got in a named one.

Nor do two people in one channel share a conversation **by default**. A persistent route whose scope is omitted or `privateConversation` keys history per authenticated subject, so the bot remembers each person separately. An operator can explicitly choose `sharedConversation`; that changes prompt audience, not authority, and carries the warnings in [Scope selects the replay audience](#scope-selects-the-replay-audience). A catch-all channel does not imply that choice. On a shared route in a mapped channel, history is keyed by the channel’s space and thread, and the space’s granted capability identifiers keep that history across speakers.

## Sessions

Each admitted turn runs one session. In a mapped channel with shared or one-shot scope, every
authorized participant’s admitted native chat message can steer that turn. Messages left queued
at the final model step start later turns, each freshly authorized under its own subject; messages
from different subjects are never folded together. A consumed steer never runs again.
Only the asker interrupts or stops it. DMs, private scope, and unmapped routes retain same-sender
steering and other-sender follow-ups; wakes and GitHub deliveries remain separate turns. On a `oneShot` route (`memory: { mode: oneShot }`) each
session starts without prior history; the `persistent` clauses in steps 4 and 5 describe replay:

1. **Admission.** A process-wide semaphore bounds concurrent work. The per-`(transport, conversation)` map is checked first: the running session learns mapped status from the asker’s fresh broker space downscope. In a mapped shared or one-shot channel, other native-chat participants join after fresh authorization; messages arriving while that first lookup is pending wait for its result. Every other route keeps same-sender steering and other-sender follow-ups. Wakes, GitHub deliveries, and input arriving after cancellation or the turn’s seal queue as follow-ups. Together steers and follow-ups hold at most 100 items, so a conversation holds at most 100 bounded messages and the gateway at most that times `sessions.maxConcurrent`. Only a full mailbox (`busy.cause = same-conversation`) or lack of a permit for a new conversation (`saturated`) refuses admission. A refusal is eligible for `I'm busy — try again shortly.` when `replyOnBusy` is set; a collected batch is eligible regardless. Sending still requires a free refusal-reply permit and successful transport delivery. Follow-ups run inline under the existing permit, with their own route, subject, fresh cancellation, broker leg and receipt trace. A conversation with a steady stream of messages keeps its permit until its mailbox is empty.
2. **Authorization.** A sealed thread is first refused with outcome `sealed`: an addressed turn gets the fixed sealed-thread sentence; a follow turn posts nothing. Other sessions open an attested broker leg with `capabilities(subject, agent, scope)`. A broker-confirmed unmapped sender emits the owner notice on every refusal and is eligible for the fixed Lange-family reply under the reply cooldown. A mapped principal denied this agent, or an empty grant with principal attribution, gets the named permission reply described in [Routing](#routing). Other authentication failures and legacy refusals without typed cause or principal attribution retain `You're not authorized to use this agent.` A follow turn posts nothing. **No model call or session progress starts** on either refusal. Queued or steered input may already have received its admission acknowledgment, which does not signify authorization.
3. **Liveness.** When the transport opted in, one session-owned policy task starts immediately after the fresh grant. The service renders everything; the model supplies no target, wording, emoji, cadence, or timing, apart from an opted-in progress note. The policy owns one message, spends the session's edit budget, seals synchronously before terminal delivery, and returns the service's own indicators to rest afterwards, so cosmetic I/O never delays the reply or holds admission. Two consecutive failures stop that surface for the session; permanent Slack installation failures additionally trip a transport-wide fallback breaker. What it shows is [Liveness, progress, and stopping a run](gateway-liveness.md#liveness-progress-and-stopping-a-run).
4. **Execution.** On a `persistent` route the session first looks up its conversation under the key in [Scope selects the replay audience](#scope-selects-the-replay-audience). An entry idle past the route's timeout, or built under a granted capability set that differs from the one this message's leg just reported, is dropped rather than used; whatever survives is seeded into the prompt ahead of the new message as compacted `(question, answer)` pairs, oldest dropped first until the window's turn and byte bounds both hold. Every person-originated turn's user text starts with a gateway-authored participant label naming the sender's broker principal in every scope, for both the current message and later replay. The lookup happens *after* step 2 because the grant comparison needs a fresh grant to compare against. Then the model client is built from the route's model, the shell runtime executes scripts on the asker’s attested leg by default; on mapped channels, `as` selects one current participant’s leg for the whole script. The credential-free `inspect_agent_config` view is built from the asker’s fresh leg and offered only where the route wrote `inspectAgentConfig: true`, the scoped asset table and request-local explicit-send slot are attached to that leg, and the prompt loop runs on a blocking task with the agent's `instructions` as the system prompt, followed by the chat-memory note when memory is granted, a note that the chat shows only PNG and JPEG on WhatsApp and Telegram, and one line the gateway renders from the route's limits: `[gateway: N steps, M capability calls and W minutes per message.]`, the minutes only when `maxDurationMs` is set. The agent's catalog skills ride the bound route — read whole into memory when the catalog loaded and shared by every session rather than re-read, so a session never touches the filesystem — and are mounted on every session on that route: a second system message after the instructions lists each by name and description, and the `read_skill` tool loads one skill's instructions, or one of its resource files, on demand. Every session offers the no-argument `wake_reason` tool, independently of `inspectAgentConfig`, returning whether chat, a GitHub webhook, a scheduled wake or a job started the turn. Chat results include `new` or `followUp` admission and the number of consumed person steers; GitHub results carry the same typed events as the attested claim, in arrival order. It grants nothing and calls no broker. Every session also offers `suggest_improvement`; what it records is written to telemetry as `agent.improvement.suggested` and is never relayed to chat, so the sender sees only the answer. Instructions are supplied fresh for each session and never stored in history; steers use that session's instructions. Shell bounds are `dekopon-process`'s defaults except `maxCapabilityCalls` and the script deadline, which both come from the route. Every model request declares a [prompt cache key](#the-prompt-cache-key) (`session_id` on OpenRouter) — the conversation's on a `persistent` route, the route's on a `oneShot` one.
5. **Answer, silence, and optional durable recording.** A required session's final bounded text and accepted provider attachments go back to chat. On required turns, with a progress message up and no streamed answer, model failures, empty answers, step limits, and lost tasks remove that message before posting the fixed line as a fresh reply; cancellations keep the existing in-place delivery. Streamed partial answers on required turns retain their appended stop or failure line. Follow turns post no fixed failure line or partial answer, except the fixed unreported-work warning when a worked turn declines on its final step. A failed follow turn leaves no stop notice; cancellation still may leave one. An inherited Slack Agent continuation or an authorized Discord follow turn may instead call `decline_chat_reply` before capability work, which commits its user-only in-process turn, removes the progress message, and sends no reply request. On a required turn's failure the sender gets the applicable fixed cause line ([liveness](gateway-liveness.md#liveness-progress-and-stopping-a-run)), or the `failed` template (`The agent could not complete this request.` by default); a follow turn instead stays silent unless capability work went unreported on its final step, when it posts the fixed audit warning but remains failed. A `PromptError` can carry model-chosen text, a provider message, or a transport diagnostic, and chat is the last place any of those belong. The operator reads the category from telemetry. A `persistent` route writes only the textual exchange back as one more in-process remembered turn, trims the window, and restarts the idle clock. A generation lease makes a commit from older in-flight work inert after grant invalidation, empty-grant removal, idle replacement, or capacity eviction, while concurrent work in the same generation appends in completion order. **Fixed stop/failure lines, the one-shot stop notice and attachment bytes are never stored in history.** A declined or failed model session records its question with nothing in the in-process answer's place, which is truthful and is what makes a later follow-up answerable; a session refused at step 2 records nothing at all. Optional durable recording happens under the conditions in [Durable memory after transport acceptance](#durable-memory-after-transport-acceptance).

`steering: abort` is the route default: a same-sender message interrupts only the in-flight model
call, never a tool, script or provider call. `boundary` waits for the current step to finish. Both
read steers before the next model call, and replace a draft answer when another step remains.
An interrupted call does not spend `maxSteps`. Each run requests at most eight model interrupts;
after that, steers wait for the next boundary. A follow-up starts with a fresh interrupt budget.
Session duration and per-call timeouts still apply.
Steers left at the last step, decline or failure fold into a follow-up under their original sender
and run before every queued follow-up. At handoff, consecutive follow-ups from one sender fold
into one turn; another sender, a job or a wake ends the run. [Harness gotchas](harness-gotchas.md)
records why the last steer check and the seal are one step.
Consumed steer text joins the turn's user text in history and the journal.
When the newest turn alone exceeds the in-process byte window, its user text is shortened to a
UTF-8-safe prefix ending in `[…]` if the intact answer and marker fit; otherwise whole-turn eviction
still applies.

Text is bounded in both directions: inbound to 16 KiB keeping the head (a chat message states its request first), outbound to 8 KiB keeping head and tail (an answer's conclusion is usually its last line). Both truncations say so in the text.

Follow-ups are memory-only and continue inline during shutdown grace. When `abort_all` ends that
grace, queued receipts become `abandoned`; queued wakes are lost and are not replayed.

At shutdown, transport readers are aborted and in-flight sessions get `shutdownGraceMs` to finish — a model call is already paid for, and abandoning it means a person watching a chat window never hears back. If the grace expires, dropping each async owner marks its synchronous prompt loop cancelled before aborting the wrapper, so no later model turn or capability call starts. A model request or provider effect already in progress remains non-rollbackable and may finish after the async owner is gone.

The gateway retains every native thread's join operation even when its async caller is aborted.
Admitted ticks still deliver their completed fires during shutdown. After resident tasks stop and
the summary dispatcher requests model cancellation, the gateway joins all admitted native work
while the runtime and assets remain available. Native thread-local destructors finish on the
bounded blocking pool, without occupying an async worker. This cleanup follows the serving
grace; the runtime's final five-second timeout applies to residual short blocking work. The
chart's termination grace covers both daemon drains plus `drainBudget.bufferSeconds`
([Kubernetes guide](kubernetes.md#draining-takes-both-graces-in-sequence)).

**Exhausting one transport is a gateway failure.** Healthy transports keep serving during a peer's
bounded recovery episode. A permanent refusal, exhausted episode, or reader-task panic stops the
whole gateway through the same bounded drain as shutdown, but with a nonzero exit. See
[Connection recovery](gatewayd.md#connection-recovery).

## Conversations

**Status: Current.** History is a trust surface rather than a feature flag; [`security-model.md`](security-model.md#conversation-memory-as-a-trust-surface) states the surface it accepts.

A `persistent` route keeps a bounded history and replays it into the next prompt, so a follow-up question can say "and the second one?" and be answered. The history is private per authenticated subject by default; `scope: sharedConversation` shares it among authenticated participants in one exact routed conversation. In a mapped channel it uses the channel’s space key, with separate windows per thread, and the space principal’s grants gate replay and journal recall across speakers. The grant comparison below uses those space grants on mapped shared routes; an asker with no grants is refused without evicting the shared window. Private routes keep their per-subject keys and grants. `persistent` with the default window is the route default; `memory: { mode: oneShot }` opts out.

### The history lives in the gateway

This subsection describes the automatic replay window, not the separate on-demand durable provider. The live window sits in the daemon's memory and is never sent to the broker. The broker holds provider credentials and decides every invocation; conversation text there would put the most sensitive content in the system inside the most privileged process. The gateway already read the message and wrote the answer, so keeping the history there adds no new reader.

`idleTimeoutMs` is how long a window stays in memory, not how long it is remembered. When a message finds no window in memory (first contact, idle expiry, capacity eviction, or a restart), the route's `recall` rebuilds one before the model runs:

- `none` starts empty. It is the default without `sessions.journal`, and it is the only behaviour older releases had.
- `journal` reads the gateway's own on-disk transcript. With `sessions.journal` set, each turn on a journal route is appended after the reply as one record to `<journal path>/<agentId>/<stem>.jsonl`, where the stem is a SHA-256 of the complete conversation key: directories `0700`, files `0600`. A record is a body line holding the turn's original items (user text, consumed steers, assistant text, reasoning, and every tool call with the result the model received) followed by a small header line with the record's grant, model identity, summary slots and plan, attachment inventory and next `chat-asset:` number, and a span index into the body; summary, pin and decider-choice changes are appended as their own short lines. Readers decode only headers and slice the body by offset, on their own file descriptor and without the journal lock. Every journal-recall turn walks the newest `2 × maxTurns + 1` headers at its start (one past twice the window, so a warm window sees the count that forces a repack), whatever the window's residency, and recall uses the newest eligible record's inventory. Only the trailing run under the current grant is recalled. Records older than `forgetAfterMs` or memory `maxAgeMs` are not replayed. File rewrites discard records past memory age, not eligible turns evicted from the prompt window, and every rewrite writes a new file and renames it, so a reader holding the old file keeps its bytes. After every delivered turn and at startup, the file sizes in each agent directory are bounded by that agent's `retention.maxBytes` (or `retention.maxBytesPerAgent` globally): oldest whole records across conversations are evicted first, with their side lines. A single record larger than the cap is not retained and evicts nothing; the reply it belongs to has already succeeded. This disk cap is independent of the prompt window. At startup and after every delivered turn, files not written for longer than the longer of memory age and journal-route forget intervals are deleted. Legacy unversioned root files without a matching `.followed` sibling are reset on open, and a file in any earlier journal format resets at startup: old history is never migrated or re-keyed, and it is removed rather than left unreachable. A reader skips a torn newest line; a corrupt file is reset after the reply, by the turn that found it, or at startup. While each file loads at startup the gateway holds a `loading.marker` naming it, so a file that crashed the previous start is reset before anything else loads.
- `platform` asks the chat service for the conversation's recent messages (Slack `conversations.replies` or `conversations.history`, Discord `GET /channels/{id}/messages`), so the model sees what the person sees, other participants included. It reads at most `min(2 × maxTurns, 100)` messages, waits at most 5 s, and a read that fails answers from an empty window. Only Slack and Discord have a history API; startup refuses `platform` on WhatsApp, Telegram, and local routes. Discord returns other people's text only when the application has the Message Content intent enabled in the Developer Portal, whether or not the transport sets `messageContent`; without it, those messages are skipped. Each author is labelled with the broker principal it maps to, `[gateway: chat history, from simon]`: for every distinct author other than the sender, the gateway asks the broker for that author's `capabilities` in this chat scope and reads only `principal`, inside the same 5 s. An author no principal names keeps `[gateway: chat history, from <service user id>]` on a private route and becomes `[gateway: chat history, from unmapped participant]` on a `sharedConversation` route, so no platform id reaches the model there. Either label is service-reported authorship, not broker authentication, and each unmapped lookup logs `broker_capabilities_refused` with `reason: unmapped-subject` in the broker.

Every journal-backed persistent route appends its record after the reply. On a route whose `recall` is not `journal` (platform recall, or a journaled `recall: none` route) the record keeps only the tool exchanges, with no user, steer, assistant or reasoning item and no summary slots; those routes keep their own window of user and answer text in memory. Journal-less persistent routes keep only their window and offer no memory tools. A failed append is reported without claiming durable retention. A stopped completion, or one stale at its lease commit, cannot append. The record is written before the conversation's admission is released, so the next turn of the conversation always finds it.

For a journal-recall route, the recalled headers are projected before the model call. The newest `hotTurns` records replay as verbatim steps: user and steer messages, assistant text, and each tool call followed by its result, all sliced from the journal. Each hot record may use `maxBytes / hotTurns`; within it, user, steer and assistant text are kept first, and tool results share what is left, cut at a safe UTF-8 and JSON-escape boundary; an argument that does not fit, or is not a JSON object, replays as `{"cut":N}`. Provider reasoning replays only to the model whose identity produced it. Older records replay at a rung: their pin, else a decider choice, else their deepest generated summary, else verbatim; summary and metadata rungs appear as envelopes (`[memory #41 · time · summary 3/3]`, the summary, and one line per tool call) in one leading `[gateway: earlier conversation]` message. When the projection is over `maxBytes`, the oldest record steps down one rung at a time (summary, metadata, then a counted omission), hot records last and whole, to three quarters of the budget; rungs change only on such a repack, when a record leaves the hot window, or when its pin changes, so consecutive requests differ only at the newest edge. With `selection: {decider: …}`, at most one broker-governed invocation per turn asks about the oldest older record that has no choice, whose summary slots are all final and which has at least one generated summary; its labels are that record's generated `summary-<n>` IDs and `metadata`, its input is the decision provider's `decide.choose` request with the configured `backend`, `model` and `requireProbabilities` and one `{tool, outcome, exitStatus?, ageMs, bytes, pinned}` evidence record per recalled record (newest 256 at most), and it never carries historical text. Its choice is written after the reply and takes effect at the next repack; a refused, ungranted, invalid or failed decision leaves the record on the ladder. The request-only `[gateway: memory]` notice names every record the window omitted, so no turn disappears silently.

After the reply, a summary engine fills a journal-recall record's summary slots in the background, part by part, through its plan's model, with the record's non-opaque text cut to `maxInputBytes` (text first, then tool exchanges). No reply, steer or next turn waits on it. Concurrency and queue are bounded by `sessions.memory.summaries`; a task that does not fit the queue marks its part and later parts as failed (`overflow`) and is counted. Each part ends `generated` or failed (`timeout`, `refused`, `modelFailed`, `empty`, `overflow`, `panicked`, `io`, `unconfigured`); a failed summary is recorded as failed, never synthesized. The model's own `timeoutMs` bounds each call, and shutdown cancels running calls without lengthening the drain; a part left running resumes from the stored original at the next start. The plan is frozen into the record when it is written, so a later reload changes only new records, and a plan whose model name was removed stops its chain as `unconfigured`.

With `sessions.journal` configured, a wall-clock seal writes only a sibling `<stem>.sealed`
file containing `{"atMs":<u64>}`, mode `0600`; it never writes a transcript line on the cancel path.
The `.sealed` marker is never consumed or cleared by a successful turn. Its mtime expires by the route's `forgetAfterMs` on read, and the longest configured forget interval bounds its directory sweep independently of the journal's memory-age file sweep (the separate `.followed` invitation uses its stored `untilMs` deadline instead), so expiry
lifts the seal. A marker that does not parse is removed at startup. A restart preserves
the seal and the fresh DM window, but loses the notice. Without a journal, seals live only in a
resident map and restart forgets them. A seal write failure is reported as `gateway_journal_seal_failed`.


On a `platform` route a window already in memory also catches up. The gateway remembers the newest chat message each window took in (the trigger, or the newest recalled message for a wake) and the messages steered or folded into its turns; a later commit never moves it back or forgets those. When a message from the chat service finds the window resident, a mention or a thread continuation alike, the gateway reads the messages strictly after that watermark and before the trigger, under the same `min(2 × maxTurns, 100)`, 5 s and `forgetAfterMs` bounds, drops the bot's own messages and any it already took in, and keeps the newest that fit in `maxBytes`. They reach the model as one turn of their own ahead of the message, the same `[gateway: chat history, from …]` block the cold read produces, attachments numbered as `chat-asset:N` like any other; in either scope that recalled turn carries no `authenticated participant` line, so other people's words never sit under the sender's. The window records that turn with the exchange, so the next mention reads only what came after. Slack bounds the read with `oldest` and `latest`; Discord pages with `after`, cuts at the trigger, and when one page of 100 does not reach the trigger, reads the newest messages before it instead. A failed read logs `gateway_recall_failed` and the turn runs without the block.

A recalled window brings its attachments with it, under the same `chat-asset:N` numbers the replayed turns use, and bytes are still fetched only on demand. An attachment that arrived before `forgetAfterMs` is not recalled. WhatsApp media ids expire after 7 days, so with a longer `forgetAfterMs` an older WhatsApp photo is named for that one recall but can no longer be opened, and the next journal line drops it.

### Scope selects the replay audience

The effective keys are intentionally exact:

- `privateConversation` (the default): `(agent, configured transport, transport-derived conversation identity, canonical subject)`;
- `sharedConversation`: `(agent, configured transport, transport-derived conversation identity)`.

The agent boundary means two agents never share transcript or attachment state even when they are routed on the same transport conversation. The configured transport boundary prevents lookalike identities from different installations or services from aliasing. Private scope adds the canonical subject from the transport envelope accepted for the fresh broker leg, so one participant claim never receives another's history. Shared scope removes **only** that subject component; it is not agent memory, team memory, a namespace shared by routes, or any replay beyond this exact conversation.

The conversation identity is the transport-minted conversation's own key — `id`, or `id:thread` when the answer lands in a thread — not mechanically `(channel, thread)`. Slack omits `thread_ts` on the message that *starts* a thread and sends it on every reply inside one, while the bot answers that first message in a thread rooted at it, so the key is the thread the answer joins rather than the raw field. What each kind keys on, and who a shared key lets in:

| kind | `privateConversation` | `sharedConversation` | audience the shared key implies |
|---|---|---|---|
| `directMessage` | (agent, transport, key, subject) | refused at startup on a `[directMessage]`-only route | none: the direct message already is the subject |
| `groupDirectMessage` | (agent, transport, key, subject) | (agent, transport, key) | every mapped member of the group |
| `channel` | (agent, transport, key, subject) | (agent, transport, key) | every mapped member who can invoke the route there; a Discord guild channel whole |
| `thread` | (agent, transport, key, subject) | (agent, transport, key) | the thread's participants; a private Discord thread inherits its parent's grants |

Choosing shared scope on a broad Discord channel can disclose one participant's prior prompt and the agent's answer to every other mapped participant who can invoke that route there. Treat that as an explicit audience expansion, not as a convenience toggle.

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

`<principal>` is the broker principal the attested sender maps to, which the broker names in its `capabilities` answer on that fresh leg; the gateway prints it and decides nothing from it. When the broker names none the line is `[gateway: unmapped participant]`, with no platform identifier. The gateway writes the first line from the transport envelope accepted for the fresh broker leg after composing any attachment reference note, and writes the same line on the recorded turn, which carries arrival lines instead of the note. User text remains untrusted and may contain a lookalike label; it cannot replace the gateway-authored first line. The labelled bytes are retained in the bounded history, so replay preserves who said each turn and the label counts against `maxBytes`. Private persistent and one-shot turns carry the same gateway-authored attribution line.

**The participant's principal name reaches the model provider on every person-originated turn in every scope.** The telemetry gate controls exports to the configured telemetry sink, not the prompt sent to the selected model endpoint. Platform identifiers such as phone numbers stay out of the label. Choose the model provider with this disclosure in mind. Enable shared scope only when sending earlier participant text and agent answers to that model provider is acceptable for the whole conversation audience.

The state key is *not* the admission key from step 1, which is `(transport, conversation key)` and has no agent, scope, or subject. The two keys answer different questions. Serialization asks "is this bot already busy on this thread"; state asks "which exact route audience owns this transcript and attachment inventory". Admission therefore does not serialize every possible shared-state race. The store gives each session a generation lease and attachment-access fence: sessions in one live generation append in completion order and reuse its inventory, while removal, replacement, or eviction makes every older lease inert and closes its asset fence. Stale in-flight work can therefore neither recreate forgotten history, rename its cache lane, publish into a replacement inventory, nor start a metadata/byte fetch through a retired one. A transport read already started while the generation was live may finish concurrently, but a final fence check discards those bytes instead of sending them to the model after retirement.

### Prior turns are compacted

A route without journal recall keeps a window of `(the user's message, the final answer)`, or the message alone when the session failed or declined an optional reply; the message includes consumed steers, each with its delivery and attribution frames, and never a gateway notice. A journal-recall route keeps one record per turn and replays it as described above: verbatim while hot, then as summaries or envelopes, never executable tool calls from an envelope. Platform recall obtains chat text from the platform and journals only tool evidence.

The number that forces this: one script's combined output can reach 256 KiB, which is `dekopon-process`'s default `max_output_bytes` in [`../crates/dekopon-process/src/lib.rs`](../crates/dekopon-process/src/lib.rs). Replaying full transcripts would let a single earlier turn cost more than the entire window budget, and it would do so most on exactly the sessions that did the most work.

Without a journal, intermediate commands and output are lost across sessions. With a journal, a record's original stays readable through `read_memory_record` for as long as the record is eligible; a record and its summaries are kept and removed together. The broker audit log is not a conversation-text store.

### The bounds

| Setting | Where | Bounds |
|---|---|---|
| `mode` | route `memory:` | `persistent` (default) or `oneShot` |
| `scope` | persistent route | `privateConversation` (default) or explicit `sharedConversation` |
| `idleTimeoutMs` | persistent route | How long an untouched conversation survives; default 900000 |
| `maxTurns` | persistent route | Exchanges the window replays; default 12 |
| `maxBytes` | persistent route | Bytes the window replays; default 65536 |
| `recall` | persistent route | Where a window not in memory is rebuilt from: `none`, `journal`, or `platform` |
| `forgetAfterMs` | persistent route | Oldest exchange or chat message `recall` may bring back; default 604800000 |
| `maxConversations` | `sessions:` | Conversations the process tracks at once; default 1024 |

`maxTurns` and `maxBytes` both apply, oldest turns dropping first until both hold. Two bounds because they fail differently: twelve one-line exchanges and twelve paragraph-length ones are the same number of turns and very different prompts.

`maxConversations` lives under `sessions:` rather than in the route block because it is a property of the process, not of a route, and `sessions:` is already where "what this daemon costs at once" is configured. It is a memory bound and not an admission bound: reaching it evicts the least recently used conversation rather than refusing a message, because a person in the middle of a conversation matters more than one who stopped an hour ago. An eviction is logged as `gateway_conversation_evicted` with a reason, so a ceiling set too low is visible as churn instead of as a bot that intermittently forgets.

Neither eviction runs on a timer. There is no sweeper task and no shutdown hook: the idle timeout is checked by the lookup that would otherwise have used the entry, and the ceiling is enforced by the write that would otherwise have exceeded it. Closing a conversation generation makes its asset metadata and byte source immediately inaccessible through every stale session; the independently bounded asset map may retain that inert metadata until its next operation prunes it. The live window dies with the process; eligible journal records survive a restart independently of window eviction.

### Participant legs

A mapped channel is one shared stream, with capabilities “done under the most accurately identified
conversants identity” (owner, 2026-10-22). A native-chat message reaching admission is authorized
before injection; this does not widen which unmentioned messages a transport ingests. Unauthorized
or empty participant legs drop the message with one `gateway.admission` refusal record. The first
line is `[gateway: authenticated participant: <principal>, handle pN]`, followed by one join line per
participant summarizing read versus write by provider from their broker answer and channel downscope.
Display names and other message text remain outside the authenticated line.

The session retains at most eight legs including the asker. Later participants still connect and
receive a fresh capability answer; an authorized temporary leg closes before its attributed text
joins, while a refusal exposes nothing. An overflow message has one admission record carrying
`participant-limit`. Join lookups belong to the asker’s turn trace, not participant effect decisions.
Every retained leg carries its opening message’s delivery coordinate; the asker’s carries the turn’s
starting message. The delivered turn remains the asker’s, with authorized steer texts appended.
Messages still queued when the final model step ends start later turns with their own participant
as the asker. Every such turn opens a fresh leg and gets the normal capability answer; an
unauthorized leftover is refused before inference. Folding combines messages only from one subject.

Each retained leg receives a gateway-minted handle: `p1` for the asker, then `p2` through `p8`
by first authorized appearance. The table dies with the turn. Overflow text has no handle.
The `bash` tool accepts an optional `as` string, for example
`{"command":"probe upper --text hello","as":"p2"}`. Every command word in that script runs on
that participant’s leg; omitting `as` uses the asker. An unknown handle is a tool error with no
broker call on mapped routes. Unmapped routes, including DMs, private scope and GitHub,
mint no handles: any supplied `as` is ignored and the script runs on the asker's leg.
The shared model-facing schema describes `as` as applying only in mapped channels.
Ignored selection is traced with `tool_call.ignored_arguments = "as"` and
`reason = "unmapped-route"`, never as an error.
All legs share the session call budget and cancellation, and close with the session.
The broker authorizes the selected participant ∩ channel on every frame. Its decision records
include `participant.handle` and that leg’s opening delivery coordinate; the label is never an
identity or policy key. The gateway’s `shell.command` spans carry the same handle.

The mapped-channel frame treats others’ text as context: act on what the turn’s owner asked or
endorsed, or what a participant asked for themselves under their own handle; say whose idea was
acted on and under whose handle. Do not ask clarifying questions about attribution. Handles in
replayed history grant no authority in the current turn.

### Authorization is never cached

Every new turn opens a fresh attested broker leg and gets a fresh chat-scoped `capabilities` answer, exactly as step 2 describes. On mapped shared or one-shot channels, another participant’s admitted native chat message gets the same fresh authorization before its text or assets reach the model; subsequent messages reuse a retained leg during that turn. Other routes keep same-sender steering on the running leg. Persistence changes nothing here: no grant is remembered, no decision is carried forward, and history is prompt text rather than authorization input.

The granted capability set is additionally **stored with the conversation** and compared at each new turn. Any difference drops the history and attachment generation and starts a fresh conversation; an empty grant removes the entry outright and closes the same asset fence. The reason is narrow and specific: output and attachment references from a session with a broad grant are sitting in the retained state, and if the owner then narrows what that subject may reach, an unchecked entry would keep replaying or fetching them after the capability that produced them was taken away. Invalidation costs a cache miss on the first new turn after a policy change, which is the right price — a narrowed grant is precisely when replaying old output is wrong. This comparison remains conservative on a shared route: if two participants receive different capability identifier sets, moving between them resets the shared transcript and inventory rather than carrying state produced under the other set. Sharing never promotes either participant to the other's grant.

Its reach is exactly the granted capability *identifiers*, which is less than it sounds like. A policy edit that keeps the same capability list but tightens its owner-authored constraint set — a narrower allowed host, a smaller output ceiling, a different credential — produces an identical grant set and does not drop the history. Text fetched under the older constraints stays in the prompt until the window or the idle timeout removes it.

### Why fifteen minutes

The idle-timeout default is pulled in two directions and loses one of them.

The ChatGPT subscription endpoint publishes no prompt-cache lifetime. Public OpenAI API policies vary by model and retention mode, so tuning a user-visible memory timeout to one guessed provider TTL would couple two mechanisms that do not share a contract. Human conversational memory runs on a longer clock: someone who asks a follow-up after a meeting expects the bot to know what they were discussing, and a bot that forgot after a brief lull is the failure people report.

The default is 15 minutes, which resolves toward the person because the user-visible point of this feature is memory, not a cache hit. **The cost control is the window, not the cache:** `maxTurns` and `maxBytes` bound what any one message pays no matter how long its conversation has been alive. [`inference.md`](inference.md#provider-retention-what-can-be-said) records the public API comparison, the undocumented subscription boundary, and why keeping a process alive does not pin a provider cache.

### The prompt cache key

Every model request carries the key: as `prompt_cache_key` on Codex and OpenAI-compatible backends, as `session_id` on OpenRouter. **It is a routing hint and never an access-control boundary.** It tells the provider which requests are likely to share a leading prefix so they can land on one cache; it authorizes nothing, isolates nothing, and hides nothing. The request carries the whole conversation either way, and a backend that ignores the field still evaluates the whole request without that affinity hint. Sharing a key grants nothing: each new session opens a fresh attested leg; authorized steers share the running session’s bounded participant table; scripts select one current participant’s handle, defaulting to the asker.

**It is a keyed HMAC, so nobody outside the gateway can recompute it.** The key is `gatewayd-conversation-` or `gatewayd-route-` followed by the first 32 hex digits of HMAC-SHA256, under a 32-byte gateway-held secret, over a domain-separation prefix and the length-prefixed fields below. Without the secret a provider cannot recompute a key or test a guessed subject against one, phone numbers included. The same conversation names the same lane after an idle or capacity eviction or a grant change, so the provider's sticky routing lands it where its instructions and tools prefix is already cached.

| Route mode | Key covers | Changes when |
|---|---|---|
| `persistent` | one scoped conversation: private `(agent, transport, conversation key, subject)` or shared `(agent, transport, conversation key)` | the agent, configured transport name, conversation or audience changes, or the secret does |
| `oneShot` | one bound route: `(agent, transport, route index)` | the route's agent, transport or position in `routes` changes, or the secret does |

With `sessions.journal` set, the secret is `prompt-cache-key.secret` in the journal directory: written once at boot with mode `0600` through a temporary file and a rename, then read on every later boot, so keys survive restarts. A file that is missing, not exactly 32 bytes, or readable by group or others is replaced with a fresh secret and logged as `gateway_cache_key_secret_reset` with its `reason`; only a failure to write the journal directory stops boot. Deleting the file is the reset path, and it resets only routing hints: every conversation starts a new lane. Without a journal the secret is generated per process, so keys rotate on every restart. The secret never reaches logs, spans or the broker.

A `oneShot` route's key is shared by **every sender that route answers**. That route's shared prefix is the agent's `instructions`, the skills listing when the agent mounts any, and the tool definitions, then this one message: the shared part is identical for everyone the route serves and contains nothing about any of them. Nothing sender-specific can hit — a different sender's message diverges from the first token that differs, and a cache key is a hint about a shared *prefix*, not a handle on somebody's answer. A fresh key per message would name a lane holding exactly one request and give up the only caching a stateless route can have.

What the key is worth is measured, not assumed — [`inference.md`](inference.md#how-to-evaluate-caching-in-a-deployment) has the counts and how to read them.

### What this means for retention

On a `persistent` route, chat text sits in `dekopon-gatewayd`'s memory for at least the idle timeout after somebody stops talking — on the default, fifteen minutes of a person's question and the agent's answer. With shared scope, that retained content and its attachment inventory belong to the exact conversation audience rather than one sender. **At least**, because eviction is lazy: an abandoned conversation is dropped by the next lookup on its key or by the ceiling displacing it, so with neither happening the bytes stay in the process until it exits. What a timed-out entry can never do is reach a prompt. Without `sessions.journal` the daemon writes none of that conversation text to disk (active attachment payload leases use private temporary files); with it, journal routes keep their windows in the journal directory until the file is compacted, evicted, or deleted by the operator; the operating system's own paging and core-dump behavior are outside what the daemon controls. Another process under the gateway UID is inside its trust domain; see the [current process boundary](gatewayd.md#current-process-boundary).

## Durable memory after transport acceptance

The gateway receives an optional `ChatMemorySurface` only when the agent is enabled and the broker
freshly permits all three exact memory capabilities under a matching subject namespace,
a canonical transport and conversation claim, a storage constraint, and Cedar context. The storage namespace's two
conversation-derived scope values are `channel := conversation.id` and
`conversation := conversation.key()`, so a Slack channel and a non-thread Discord channel keep the
namespaces they had in 0.13, while WhatsApp, Telegram topics, and Discord threads change shape and
start empty ([`CHANGELOG.md`](../CHANGELOG.md#0140---2026-09-13)). Otherwise recent/search, the `memory` word, the prompt note, durable
recording, and namespace creation are all absent.

When present, the model may retrieve on demand:

```text
memory recent --last N
memory search --query TEXT
```

It cannot resolve or invoke record. After model success, the gateway bounds the final answer once
(empty output uses the fixed normal answer), asks the transport to accept those exact bytes, and
only then opens one fresh broker client for one `recordDeliveredTurn` carrying the session's chat
attestation. The recorded user text is the original bounded sender text followed by each consumed
steer's raw text, joined by blank lines, excluding gateway timing and attachment reference notes.
If the combined user and assistant text exceeds the existing 64-KiB recording ceiling, only the
recorded user text is shortened to a UTF-8-safe prefix ending in `[…]`; space is reserved for that
marker and the entire accepted assistant text. This recording bound leaves prompt history
unchanged.
Assistant text is exactly what the transport accepted. No response,
denial, timeout, EOF, partial Discord delivery, or outcome-unaudited is retried, and none changes
the already delivered `answered` outcome.

Receipts mean complete **transport acceptance**, never human receipt: Slack and Telegram require an
HTTP success status before accepting `ok: true`; Slack also validates channel and strict canonical
timestamp, Telegram validates message/chat/topic and replies inside the topic, Discord validates
every split message and treats a later failure as partial, and local acknowledges only after
`write_all` and `flush`. The hidden request carries a tagged service-specific inbound delivery
identity (including fauxlack channel/message vocabulary for disposable tests), and the broker checks its channel/topic/transport fields against the attested scope before
namespace creation, preventing cross-transport aliases. Local identities include a 128-bit
OS-random boot nonce, connection, and sequence, so restarts do not collide.

Durable retrieval is not conversation replay. It is never automatically inserted into a later
prompt. Recording never deduplicates: a redelivered message becomes a second stored turn. There is
no deletion/export UX or encryption-at-rest claim.

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

The broker is the sole authority. `dekopon-gatewayd` supplies the subject and never the principal; a refused attestation is an audited denial recorded against the gateway's own peer identity. Driving an agent at all is its own policy statement — `Dekopon::Action::"agent.prompt"` over `Dekopon::Agent::"<name>"` — so a mapped subject the owner never permitted to use this agent is refused before the capability listing is even assembled, and a chat-attested `invoke` under such a session is the audited denial `agent-denied`. See [`security-model.md`](security-model.md) for the complete attestation contract, and note in particular that **a policy written for direct peers can never authorize an attested context and vice versa** — adding a gateway cannot widen a grant that already existed.
