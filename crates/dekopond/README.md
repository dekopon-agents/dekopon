# dekopond

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
  [policy and lifecycle](../../docs/dekopond.md#connection-recovery).
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
  receipt by default. Persistent routes retain freshly authorized late photo references without
  restarting busy work or launching a delayed run; completion text or a separate acknowledgment
  asks whether another version is wanted. Captions keep the ordinary message path, and Stop
  cancels late intake. References download only on demand. Upgrade both daemons together for
  nullable inventory lengths; see [0.19.0 upgrading](../../docs/upgrading.md#whatsapp-burst-collection-and-unknown-lengths-0190).
- **Asset handles** — exact proposal references carry read-only descriptors, never expanded bytes.
  Successful typed outputs join the scoped disk LRU; attaching does not send. Broker-authorized
  `asset.send` queues at most four files per turn, with persistent sent flags and bounded failure
  notices. Slack/Discord/local accept concrete valid media types; Telegram/WhatsApp send PNG/JPEG.
  WhatsApp retains its 5,000,000-byte ceiling. See the [asset contract](../../docs/dekopond.md#asset-handles-and-delivery).
- **Liveness** — disabled unless opted in, and only after fresh authorization. Default `progress: auto`
  prefers native status, then typing/reaction, avoiding redundant progress messages; explicit
  `progress: message` retains one delayed editable surface finalized as the answer. Auto also permits
  that surface for an explicit Stop button or when no indicator works. `progressDetail` controls
  prose only: explicitly requested `stream` remains independent of progress/detail Off. WhatsApp
  stays typing-only. See [presentation limits](../../docs/dekopond.md#liveness-progress-and-stopping-a-run).
  A stop word, a cancel button, or
  `limits.maxDurationMs` ends a run early. Cosmetic failures never alter the terminal reply.
- **Sessions** — a process-wide concurrency ceiling plus per-conversation serialization,
  bounded model turns, bounded capability calls, a per-script wall-clock deadline a route sets with
  `limits.scriptTimeoutMs` (default 30000), cooperative Stop checks, and one fixed line on
  failure. An unaddressed owned-thread follow-up also offers `decline_chat_reply`, which ends a
  no-work session without sending anything to chat instead of making the agent take the last word.
- **Authorization** — every session opens an *attested* broker leg naming the sender's
  canonical subject. An empty capability set ends the session before any model call.
- **Conversations** — one independent session per message unless a route sets
  `mode: persistent`, whose `privateConversation` default keeps per-subject history and whose
  explicit `sharedConversation` scope shares one exact agent/transport/conversation window.
  Shared turns carry gateway-authored canonical participant labels, and those identifiers reach
  the model provider. History is compacted and bounded;
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
- **Improvement suggestions** — a route with `improvementSuggestions: true` (default `false`)
  also offers `suggest_improvement`, a bounded channel for the model to tell the operator what
  to fix, at most three notes per session. Each note is written to telemetry as
  `agent.improvement.suggested`, which is why the route flag is off by default: the record
  carries model-authored text, and setting the flag is that consent. A suggestion is advisory by construction — no instruction, skill, limit, or grant
  moves because a model asked — and the gateway never relays it to chat.
- **Self-inspection** — every authorized session on a route that has not written
  `inspectAgentConfig: false` offers `inspect_agent_config`, returning its
  standing prompt, mounted skills by name, description, and resource file paths (never their
  text; `skills` is absent when nothing is mounted), route limits, and fresh subject-specific
  effective Cedar grants. The fixed shape omits raw policy, identity, endpoints, broker paths,
  skill directories, and all credential names and values. Calls are repeatable under the prompt
  loop's shared bounds, with no inspection-specific counter; a repeat points at the copy already
  in the conversation instead of appending a second one.

## Authority

`dekopond` has none. It holds chat bot credentials and model credentials — the things it
needs to hear a question and to ask a model — and it never holds a provider credential, a
policy, or an authorization. Every effect a session drives is submitted to
`dekopon-brokerd` as an on-behalf-of proposal, and the broker alone maps the subject to a
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
[`docs/dekopond.md`](../../docs/dekopond.md).

WhatsApp media-first collection uses `debounceMs` (5000ms quiet by default) and
`debounceMaxWaitMs` (15000ms maximum from the first receipt). Zero quiet time bypasses collection;
an enabled maximum must be at least the quiet interval. See the
[multi-message contract](../../docs/dekopond.md#multi-message-media-inputs) for bounds and isolation.
Persistent WhatsApp routes retain freshly authorized late photo references without starting another
model run, then ask whether another version is wanted. See
[late photos](../../docs/dekopond.md#late-photos-on-persistent-whatsapp-routes) for completion races,
caption refusals, cancellation and temporary-retention limits.

## Run

```console
dekopond --config /path/to/dekopond.yaml
```

Part of the [Dekopon](https://github.com/dekopon-agents/dekopon) workspace; see
`docs/design.md` for the authority model and `docs/security-model.md` for the trust
boundaries this daemon sits outside of.

## Durable memory receipts

When the broker returns an effective all-three memory surface, the prompt notes only the on-demand
`memory recent` and `memory search` forms. After model success, the gateway bounds the answer once,
requires complete service/kernel transport acceptance, and sends exactly one fresh hidden record
request containing the original bounded sender text and exact accepted answer. It never retries;
record failure cannot change an already delivered answer. Receipts do not prove human receipt.

## Isolated model authentication

`dekopond auth chatgpt {login,status,logout,export}` runs before gateway configuration,
telemetry, transports, or runtime creation. It uses only Dekopon's isolated model credential;
ordinary serving requires `--config PATH`. See [`docs/cli.md`](../../docs/cli.md) for auth-only flags, output, exit codes and both export guards.

Asset retention is process-wide: `sessions.assetRetentionBytes` defaults to 268435456 bytes on
private disk scratch. Zero disables asset retention and attachment delivery, not the bound;
text-only sessions remain usable. Weak model references cannot prevent LRU reclamation. Generated
PNG results publish gateway-owned `chat-asset:<N>` markers for successive edits; availability is
not delivery confirmation. See [chat assets](../../docs/dekopond.md#chat-assets) for lifecycle,
limits, explicit release notices and no-refetch/no-blind-retry behavior.
