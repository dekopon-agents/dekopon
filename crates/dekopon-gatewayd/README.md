# dekopon-gatewayd

The unprivileged Dekopon chat gateway and agent daemon. It connects to chat services, waits for a
wakeup, routes each authenticated message to a named agent from the catalog, runs one bounded model
session with the sandboxed shell plus safe on-demand meta tools, and replies with the answer.

- **Transports** — Slack Socket Mode and Discord Gateway over outbound WebSockets, Telegram long
  polling, a raw-body-HMAC-authenticated text/image WhatsApp Cloud API webhook with pinned Graph
  replies, and an owner-only Unix development socket. WhatsApp is the only public wakeup surface;
  it expects operator-owned TLS termination and exact-path routing.
- **Connection recovery** — all five adapters share a composable receive/connection extension:
  30-second attempts, 500 ms exponential backoff capped at 60 seconds plus up to 249 ms jitter,
  ten failures per episode, reset only after five continuously connected minutes. Healthy peers
  serve during recovery. Any terminal reader failure drains sessions and exits nonzero so the
  supervisor can restart the gateway container. No outbound effects are replayed. See
  [policy and lifecycle](../../docs/gatewayd.md#connection-recovery).
- **Routing** — first match wins on (transport, direct message or channel), and a channel
  route names one channel or, with the name left out, any channel the bot is invited to.
  Declaration order is the precedence rule: a named channel written above a catch-all keeps
  its own traffic. Unmatched traffic is ignored, and a channel initially requires the bot to be
  @-mentioned. In Slack Agent mode, fresh authorization claims that exact sender/thread so later
  unmentioned follow-ups can continue; every other ambient channel message remains ignored.
- **Chat assets** — Slack, Discord, and Telegram photos/files plus WhatsApp PNG/JPEG photos become bounded references
  that a model opens on demand. Discord signed CDN URLs are host-checked, streamed under the same
  8 MiB ceiling, and refreshed from the exact source message after expiry.
- **WhatsApp photo bursts** — collect until 5000ms of quiet, bounded by 15000ms from the first
  receipt by default. Photos arriving during the sender's run become ordinary steers after
  collection, using that run's asset access; downloads remain on demand.
- **Steering** — same-sender input interrupts only the model (`steering: abort`, default) or waits
  for the step boundary (`boundary`). Other senders and wakes queue their own turns. The shared
  mailbox holds 100 items; busy replies mean a full mailbox or process-wide saturation, not an
  ordinary follow-up. User stops drop only that sender's queued input.
- **Asset handles** — exact proposal references carry read-only descriptors, never expanded bytes.
  Successful typed outputs join the scoped disk LRU; attaching does not send. Broker-authorized
  `asset.send` queues at most four files per turn, with persistent sent flags and bounded failure
  notices. Slack/Discord/local accept concrete valid media types; Telegram/WhatsApp send PNG/JPEG.
  WhatsApp retains its 5,000,000-byte ceiling. See the [asset contract](../../docs/gatewayd.md#asset-handles-and-delivery).
- **Indicators** — layered `sessions`/transport/conversation-kind/route settings default off;
  after fresh authorization, `progress: auto` prefers native status, then typing. Working
  and steer reactions default off. `working: on` adds to status or typing;
  `working: auto` falls back when neither is active. WhatsApp refuses working reactions
  until a verified clear exists. Explicit
  `progress: message` retains one delayed editable surface finalized as the answer. Auto also permits
  that surface for an explicit Stop button or when no indicator works. `progressDetail` controls
  prose only: explicitly requested `stream` remains independent of progress/detail Off.
  Best-effort steer/queue 👀 is separate and requires `reaction.seen: on`; it defaults off.
  See [presentation limits](../../docs/gatewayd.md#liveness-progress-and-stopping-a-run).
  A stop word, a cancel button, or
  `limits.maxDurationMs` ends a run early. Cosmetic failures never alter the terminal reply.
- **Sessions** — a process-wide concurrency ceiling plus per-conversation serialization,
  bounded model turns, bounded capability calls, a per-script wall-clock deadline a route sets with
  `limits.scriptTimeoutMs` (default 30000), cooperative Stop checks, and one fixed line on
  failure. An unaddressed owned-thread follow-up also offers `decline_chat_reply`, which ends a
  no-work session without sending anything to chat instead of making the agent take the last word.
- **Authorization** — every session opens an *attested* broker leg naming the sender's
  canonical subject. An empty capability set ends the session before any model call.
- **Conversations** — `persistent` is the route default, with per-subject history under
  `privateConversation`; explicit `sharedConversation` shares one exact agent/transport/conversation
  window. `memory: { mode: oneShot }` starts each session without prior history.
  `sessions.memory` resolves retention/arena bounds and per-agent representation policies;
  journal-backed routes may override the agent policy with `routes[].memory.policy`. A `decider`
  selection also names the decision provider's `backend`, `model` and `requireProbabilities`.
  Without a journal, persistent routes remain window-only and config-check warns once.
  `githubWebhook` configuration requires `name`, `bind` and `secretEnv` (the name of a nonblank
  environment credential); its path is fixed at `/webhooks/github`. A GitHub route must set
  nonempty, unique `githubEvents` from `pull_request` and `workflow_run`, a canonical
  `conversation` thread selector and `memory: { mode: oneShot }`. Non-GitHub routes cannot set
  `githubEvents`. First route match precedes event filtering. The verified GitHub POST mount
  shares WhatsApp's listener when both use the same bind; GitHub alone binds that port. It
  verifies raw-body HMAC-SHA256 before parsing repository identity, acknowledges accepted,
  ignored and recent-duplicate deliveries, and retries queue saturation with HTTP 503. Replies
  are silent; a route still requires a fresh broker-authorized `agent.prompt` session before inference. A signed run report alone authorizes no fix or post, and the transport never posts even on failure. See [gateway configuration](../../docs/gatewayd.md).
  Every person-originated turn, in every scope, carries a gateway-authored participant label naming the sender's broker principal,
  and those names reach the model provider. History is compacted and bounded;
  transcript commits, attachment inventory/publication/fetch, and opaque cache-lane lifetime share
  one generation that is retired on idle/LRU/grant change or empty-grant removal. It caches no
  authorization.
- **Skills** — the agent's catalog skills ride its bound route, read whole into memory when the
  catalog loads and shared by every session on that route, so a session never touches the
  filesystem. When any are mounted, a second system message after the instructions lists each by
  name and description only, and the `read_skill` tool returns one skill's instructions, or one
  of its resource files, on demand; a repeat read is answered with a one-line pointer to the
  earlier result. An unknown name or resource path is a refusal the model reads, naming the
  mounted skills or the skill's resource paths, and the session continues. Skill text is
  untrusted model text exactly like `instructions`: it shapes answers and grants nothing.
- **Improvement suggestions** — every session offers `suggest_improvement`, a bounded channel
  for the model to tell the operator what to fix, or what cost it an awkward extra step even on
  a task that succeeded, at most three notes per session. Each note is written to telemetry as
  `agent.improvement.suggested` like every other model-authored record. A suggestion is advisory
  by construction — no instruction, skill, limit, or grant moves because a model asked — and the
  gateway never relays it to chat.
- **Self-inspection** — every authorized session on a route that has written
  `inspectAgentConfig: true` offers `inspect_agent_config`, returning its
  standing prompt, mounted skills by name, description, and resource file paths (never their
  text; `skills` is absent when nothing is mounted), route limits, and fresh subject-specific
  effective Cedar grants. The fixed shape omits raw policy, identity, endpoints, broker paths,
  skill directories, and all credential names and values. Calls are repeatable under the prompt
  loop's shared bounds, with no inspection-specific counter; a repeat points at the copy already
  in the conversation instead of appending a second one.

## Authority

`dekopon-gatewayd` has none. It holds chat bot credentials and model credentials — the things it
needs to hear a question and to ask a model — and it holds no policy or authorization, and no
provider credential except the shared DekoponVille Slack bot token named in
[the security model](../../docs/security-model.md#current-gateway-posture). Every provider effect
a session drives is submitted to `dekopon-brokerd` as an on-behalf-of proposal, and the broker alone maps the subject to a
principal, decides what it may do, resolves credentials, and executes it. Its normal dependency
graph excludes `dekopon-broker`, `dekopon-broker-host`, `dekopon-brokerd`, `dekopon-http-host`,
`dekopon-storage-host`, and `dekopon-policy`, and CI's `cargo tree` gate enforces that; only its
tests link `dekopon-brokerd` and `dekopon-storage-host`, as dev-dependencies.

Producing and sending assets are separately broker-authorized effects. The gateway holds no image
credential, accepts no provider path and changes no proposal reference into bytes. Typed metadata
and read-only descriptors cross broker IPC; outputs are retained without a copy and explicit sends
use only authenticated reply coordinates. Provider bytes remain outside shell, model transcripts,
history and asset trace fields.

Message text is untrusted end to end, and so are the agent's own standing orders and mounted
skills from the catalog: none of them can assert identity, name a principal, or widen a grant. An
authorized sender can ask the agent to quote those standing orders through self-inspection and to
read any mounted skill in full through `read_skill`, so neither is confidential.
Standing orders, chat content, subjects, and credentials remain excluded from informational status
reports.

The development transport is the one exception to "identity comes from
authenticated transport": it trusts its local caller to declare a subject. It grants
nothing by doing so — the broker's attestor grant and identity mapping gate everything — but it
is a development tool, not a production transport.

Configuration, transport semantics, session bounds, telemetry, the conversation contract,
and the distinct-UID deployment boundary are documented in
[`docs/gatewayd.md`](../../docs/gatewayd.md).

WhatsApp media-first collection uses `debounceMs` (5000ms quiet by default) and
`debounceMaxWaitMs` (15000ms maximum from the first receipt). Zero quiet time bypasses collection;
an enabled maximum must be at least the quiet interval. See the
[multi-message contract](../../docs/gatewayd.md#multi-message-media-inputs) for bounds and isolation.

## Run

```console
dekopon-gatewayd --config /path/to/gatewayd.yaml
```

Part of the [Dekopon](https://github.com/dekopon-agents/dekopon) workspace; see
`docs/design.md` for the authority model and `docs/security-model.md` for the trust
boundaries this daemon sits outside of.

## Durable memory receipts

When the broker returns an effective all-three memory surface, the prompt notes only the on-demand
`memory recent` and `memory search` forms. After model success, the gateway bounds the answer once,
requires complete service/kernel transport acceptance, and sends exactly one fresh hidden record
request containing the original bounded sender text plus consumed raw steer texts and the exact
accepted answer. Gateway timing/asset notes are excluded. It never retries;
record failure cannot change an already delivered answer. Receipts do not prove human receipt.

## Isolated model authentication

`dekopon-gatewayd auth chatgpt {login,status,logout,export}` runs before gateway configuration,
telemetry, transports, or runtime creation. It uses only Dekopon's isolated model credential;
ordinary serving requires `--config PATH`. See [`docs/cli.md`](../../docs/cli.md) for auth-only flags, output, exit codes and both export guards.

Asset retention shares one `sessions.assetRetentionBytes` budget, defaulting to 1073741824 bytes
(1 GiB). With `sessions.journal`, the required `sessions.assets.path` holds content-addressed files
and a durable index; uploaded files survive restart while journal records name them. Without a
journal, files use private temporary disk and die with the process. Generated files remain
session-only in both modes. Zero disables asset retention and attachment delivery, not the bound;
text-only sessions remain usable. Weak model references cannot prevent LRU reclamation. Generated
PNG results publish gateway-owned `chat-asset:<N>` markers for successive edits; availability is
not delivery confirmation. See [chat assets](../../docs/gatewayd.md#chat-assets) for lifecycle,
limits, explicit release notices and no-refetch/no-blind-retry behavior.
