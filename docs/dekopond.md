# `dekopond` — the chat gateway and agent daemon

`dekopond` is the unprivileged half of the deployment boundary in [`design.md`](design.md): it connects to chat services, waits for a wakeup, routes each authenticated message to a named agent from the catalog, runs one bounded model session with the sandboxed shell and safe on-demand meta tools, and replies with the answer.

It holds chat bot credentials and model credentials — the things it needs to hear a question and to ask a model. It never holds a provider credential, a policy, or an authorization. Every effect a session drives is submitted to `dekopon-brokerd` as an on-behalf-of proposal naming the sender's canonical subject, and the broker alone maps that subject to a principal, decides what it may do, resolves credentials, and executes it.

**Status: Current.** A route is `oneShot` unless configured otherwise; durable memory is a separate
broker/agent opt-in and does not turn that default into automatic replay. The chart enforces the
[current local process boundary](security-model.md#current-local-process-boundary).

Its dependency set excludes `dekopon-broker`, `dekopon-broker-host`, `dekopon-http-host`, `dekopon-storage-host`, `dekopon-policy`, and `dekopon-brokerd`, and CI rejects any of them appearing in the gateway's normal dependency tree.

[`../examples/conditional-write/`](../examples/conditional-write/README.md) is the complete
worked deployment: a Slack DM from an owner-mapped sender, two narrow `http-probe` capabilities, a
broker-injected GitHub token, and an audited PR review comment. Read it alongside this document —
it is the configuration this one describes in the abstract.

## Run

```console
dekopond --config /path/to/dekopond.yaml
```

The configuration file must be a regular non-symlink file owned by the daemon's UID, with a single link, not group- or world-writable, and no larger than 1 MiB. It is strictly decoded: an unknown field, an unknown transport kind, or an unknown route match is a startup failure, not a silently ignored setting.

## Configuration

```yaml
apiVersion: dekopon.dev/dekopond/v1alpha1
catalogPath: /path/to/dekopon.yaml            # dekopon-config catalog with the agents routes name

broker:                                       # optional; every field defaults
  socketPath: /path/to/broker.sock            # default: DEKOPON_BROKER_SOCKET, then XDG_RUNTIME_DIR/dekopon/broker.sock, then HOME/.local/run/dekopon/broker.sock; unresolvable is a startup failure
  serverUid: 501                              # default: the daemon's own effective UID
  maxFrameBytes: 2097152                      # default: the protocol's own bound
  ioTimeoutMs: 30000

transports:
  - name: scientist-slack
    kind: slackSocketMode
    appTokenEnv: DEKOPOND_SLACK_APP_TOKEN     # environment variable NAMES only
    botTokenEnv: DEKOPOND_SLACK_BOT_TOKEN
    endpoint: https://slack.com               # optional, tests only: the pinned origin or a literal loopback http:// URL
    experience: agent                         # optional: classic (default) | agent
    activity:                                 # optional; absent means off
      mode: native                            # off | native
      classicFallback: reaction               # none (default) | reaction
  - name: community-discord
    kind: discordGateway
    botTokenEnv: DEKOPOND_DISCORD_BOT_TOKEN
    activity: { mode: native }                # renewable native typing; optional/off by default
  - name: tg
    kind: telegramLongPoll
    botTokenEnv: DEKOPOND_TELEGRAM_TOKEN
    activity: { mode: native }                # renewable native typing; optional/off by default
  - name: whatsapp
    kind: whatsappCloudApi
    appSecretEnv: DEKOPOND_WHATSAPP_APP_SECRET
    verifyTokenEnv: DEKOPOND_WHATSAPP_VERIFY_TOKEN
    accessTokenEnv: DEKOPOND_WHATSAPP_ACCESS_TOKEN
    bind: 0.0.0.0:9080                     # pod bind; expose only through exact-path TLS ingress
    callbackPath: /webhooks/whatsapp
    wabaId: "123456789"
    phoneNumberId: "987654321"
    graphApiVersion: v23.0                 # explicit; no implicit/latest version
  - name: dev
    kind: local
    socketPath: /path/to/dekopond-dev.sock

models:
  - name: local-qwen
    kind: openaiCompatible
    endpoint: http://127.0.0.1:11434/v1
    model: qwen3
    apiKeyEnv: OPENAI_API_KEY                 # optional; absent means the endpoint needs no key.
                                              # Named but unset or blank is a startup failure.
    timeoutMs: 120000
    classes: [reasoning, analysis]
  - name: subscription
    kind: chatgptSubscription
    model: gpt-5-codex
    authFile: /path/to/chatgpt-auth.json      # optional; else DEKOPON_CHATGPT_AUTH_FILE, else Dekopon's own credential file
                                              # must be in a writable directory: refreshing rewrites it
    timeoutMs: 120000
    classes: [reasoning]
    modalities: [image]                       # optional; default none. This is image INPUT only.

routes:                                       # first match wins; order matters
  - transport: scientist-slack
    match: { kind: channel, channel: c0123abc }
    agent: incident-responder                 # one named channel, its own agent
  - transport: scientist-slack
    match: { kind: channel }                  # any other channel the bot is invited to
    agent: xaviers-conditional-writer
  - transport: community-discord
    match: { kind: channel }                  # Discord channels and native thread channels
    agent: xaviers-conditional-writer
  - transport: scientist-slack
    match: { kind: directMessage }
    agent: xaviers-conditional-writer
    model: local-qwen                         # optional; else the first model offering the agent's modelClass
    providerAttachments:                      # optional; absent means this route delivers none
      maxPerReply: 1                          # required, 1-255 attachments per reply
    chatAssetInputs: [gpt-image.edit]         # optional; capabilities whose input may name a chat asset
    improvementSuggestions: true              # optional; offers suggest_improvement, recorded to telemetry
    limits: { maxSteps: 8, maxCapabilityCalls: 16 }
    conversation:                             # optional; default { mode: oneShot }
      mode: persistent                        # oneShot | persistent
      scope: privateConversation              # privateConversation (default) | sharedConversation
      idleTimeoutMs: 900000                   # optional, default 900000 (15 minutes)
      maxTurns: 12                            # optional, default 12 exchanges in the window
      maxBytes: 65536                         # optional, default 65536 replayed history bytes

sessions:
  maxConcurrent: 4                            # optional, default 4
  replyOnBusy: true                           # optional, default true
  maxConversations: 1024                      # optional, default 1024 tracked

shutdownGraceMs: 120000                       # optional, default 120000

telemetry:                                    # optional, identical in shape to broker.yaml's
  endpoint: http://127.0.0.1:5080/api/default
  transport: http
  serviceName: dekopond
  exportTimeoutMs: 10000
```

The `conversation:` block is tagged on `mode`, and both halves are strict: an unknown mode, an unknown or wrong-case `scope`, and any persistent-only field written next to `mode: oneShot` are decode failures. `scope` is strict camelCase, accepts only `privateConversation` and `sharedConversation`, and is valid only beside `mode: persistent`; omission defaults to `privateConversation`. A setting that can never take effect is far more likely a mode typo than an intention, and a decoder that ignored it would leave a configuration file claiming a memory or audience the daemon does not have.

`improvementSuggestions: true` offers that route's sessions the `suggest_improvement` tool: a bounded channel for the model to tell the operator how the agent could be improved — an instruction that was wrong, a skill or capability it lacked, a limit it hit — at most three times per session. It is off by default because each recorded suggestion is model-authored text written to the telemetry sink as `agent.improvement.suggested`; setting the flag is the consent that puts it there. A suggestion is advisory by construction: no instruction, skill, limit, or grant moves because a model asked, and nothing it records is relayed to chat.

### No secrets in this file

Transports and chat models name **environment variables**, never values, following the precedent `dekopon-telemetry` set for OTLP ingest credentials. A variable name is validated as a name (`[A-Za-z_][A-Za-z0-9_]*`), so pasting a token where a variable name belongs is a startup failure rather than a token sitting in plain text while the daemon reports a missing credential. Missing required variables are reported at startup **by variable name and never by value**, and all three read them through one definition, so a model credential fails exactly the way a chat credential does. A variable exported with a blank value is refused the same way: an empty app secret is an HMAC key anyone can compute, and an empty bearer token is still sent as a header, so presence has to mean a credential rather than an export.

### Startup fails closed

A gateway that starts and then refuses everything is worse than one that does not start. These are all startup failures:

- a route naming an agent the catalog does not contain, or one the catalog disables;
- an agent with no resolvable model — no `model` override and no configured model offering its `modelClass`, or no `modelClass` at all;
- duplicate transport names, duplicate model names, a route naming an unknown transport or an unknown model;
- a zero step budget, a zero capability budget, or zero concurrency;
- a transport `endpoint` override (`graphEndpoint` on `whatsappCloudApi`) that is neither its pinned production origin (Slack, Discord, Telegram, or the Meta Graph API) nor a literal loopback `http://` URL. Literal means `127.0.0.1` or `::1`: the name `localhost` is resolved by whatever the host's resolver says today, which is not the same promise;
- a `channel` written beside `kind: directMessage`. The field belongs to the other kind, and a decoder that shrugged at it would leave an operator convinced they had scoped a route to one channel while it claimed every direct message on the transport;
- a missing or blank chat or bound-route model credential environment variable. A model's `apiKeyEnv` is optional and absent means "this endpoint needs no key", which a loopback llama.cpp genuinely does not; naming a variable that is unset or exported blank is the opposite claim, and this process cannot see one exported after it started;
- a route naming `providerAttachments` on a text-only transport, which today means `whatsappCloudApi`, or one whose `providerAttachments.maxPerReply` is `0` (omit the block instead), or one whose `chatAssetInputs` names a capability the catalog does not define;
- an unknown Slack experience, activity mode/fallback, or field inside those strict blocks; an off
  Slack activity with a reaction fallback, or classic native activity with no reaction fallback,
  is also refused because the configured fallback could never take effect;
- an unreachable broker. `dekopond` makes one `capabilities()` call on the configured socket before connecting any transport and logs the capability count as `gateway_broker_ready`;
- an empty `transports:`, `models:`, or `routes:` list;
- a model with `timeoutMs: 0`, or `shutdownGraceMs: 0`;
- a `whatsappCloudApi` transport whose `bind` port is 0, whose `wabaId` or `phoneNumberId` is not a canonical positive decimal, whose `callbackPath` is not lowercase literal segments, or whose `graphApiVersion` is not `v<major>.0`;
- broker frame bounds the protocol rejects (`maxFrameBytes` zero or above its hard ceiling, `ioTimeoutMs` zero), a broker socket that neither `broker.socketPath` nor the discovery order starting at `DEKOPON_BROKER_SOCKET` resolves, or a `telemetry:` block `dekopon-telemetry` refuses.

The `conversation:` block adds three more:

- a `persistent` route with a zero idle timeout, a zero turn window, or a zero byte window — the same rule a zero step budget already follows, because a bound of zero is a bound nobody meant to write;
- an idle timeout, window bound, or `scope` on a `oneShot` route; an unknown, null, or wrong-case persistent `scope`. Those settings cannot take effect as written, and silently accepting them could turn an intended private route into some other behavior;
- a zero `sessions.maxConversations`, which would make every history immediately evictable and turn a persistent route into an expensive one-shot one.

**Every problem at once.** A file that decoded is scanned to the end before it is refused, and the refusal lists everything wrong with it — `3 validation problems found:` and then one line per problem, the shape `dekopon-config` already refuses a catalog with. Only a file that cannot be understood at all — wrong ownership or permissions, oversize, or invalid YAML — stops at the first error. Route binding scans the whole table the same way, so a catalog that disabled two of the agents routes name is one refusal naming both. And a list that failed itself is not blamed on the routes that name it: no transports at all, or a transport with no name, is reported once rather than again for every route pointing at it.

**Every credential before any connection.** The chat credentials and the bound-route model credentials all resolve, and every transport client is built, before the first transport authenticates to anything. A transport that cannot be prepared is reported as `chat transport <name> could not be prepared` with the variable named in its cause; only a failure on the wire is `chat transport <name> could not connect`. That ordering is the difference between one refusal naming both missing tokens and two crash loops, the first of which had already opened a Slack socket with the token that was present. Once preparation and the broker probe succeed, every transport connection is attempted; any failures are reported together with their configured names and causes, before sessions start.

## Agent configuration self-inspection

Every authorized session is offered `inspect_agent_config`. When someone asks “what is this
agent's configuration?”, the model can call it and receive one bounded JSON snapshot designed to
render as concise Markdown tables:

- agent identifier, description, and catalog `modelClass`;
- the exact catalog `instructions` supplied as this session's system prompt;
- the skills mounted for the agent, each as its name, description, and resource file paths —
  never the skill text, which `read_skill` discloses on demand — and absent when nothing is
  mounted;
- route step/capability limits and one-shot or persistent conversation bounds, including the effective persistent scope; and
- the capability metadata in this sender's fresh `capabilities(subject, agent, scope)` result:
  identifier, selected provider, description, effect, risk, and idempotency, as
  [`catalog.md`](catalog.md#capability) defines them. *Committed direction:* removed
  ([non-goals](design.md#non-goals)).

That last section is an **effective Cedar view**, not Cedar source. Raw policy, policy IDs and
digests, denied or merely declared capabilities, execution constraints, credential bindings,
private secret-map source/selector/use inventory, principal/subject/channel/transport identifiers,
model endpoints and auth paths, broker paths, and all credential values are absent. Exact standing
instructions remain visible and may intentionally contain a public inert DRN. The gateway never receives provider credentials or raw
policy, and the typed view has no field for the chat/model credentials it does hold. Each serialized
result has a 128 KiB hard ceiling. Calls are repeatable under the prompt loop's shared per-turn tool
call and model-step bounds; there is no inspection-specific call limit. What a repeat does not do is
append a second copy: a tool result stays in the session's message vector and is re-sent to the
provider on every remaining turn, so the configuration is serialized once and every later call is
answered with a short pointer at it. The view cannot change while a session runs — it is built once,
from one fresh broker answer. An oversized view produces one fixed content-free diagnostic instead of
a partial configuration.

Inspection happens only after the ordinary authorization gate, makes no broker invocation, spends
no capability-call budget, grants nothing, and creates no durable broker audit record. It does make
standing instructions visible to any sender authorized to use that agent. Those instructions were
already model input and must never contain credentials; operators should not treat a system prompt
as a secret from its users.

## Provider attachments and chat-asset inputs

Bytes can cross between a capability and a chat conversation in both directions, and the sandboxed
shell is on neither path. It has no byte type, so printing a base64 blob would clamp it into the
model transcript as a screenful of garbage and charge the session for the rest. Both directions are
therefore courier behaviour in this daemon — it already fetches inbound attachment bytes and already
uploads files to a chat service — and neither changes who authorizes an invocation. The gateway holds
no image credential of its own: **producing** an image is a provider effect, authorized by the broker
and audited there like any other external write.

Each direction is an owner-authored route opt-in, because each is new reach.

- **Out: `providerAttachments: { maxPerReply: N }`.** A successful capability result may carry a
  reserved top-level `attachments: [{mediaType, base64}]`. The session's broker leg removes that key
  before the result reaches the shell, validates each entry (`image/png`, valid base64, PNG
  signature, at most 8 MiB decoded, at most `maxPerReply` across the whole session), puts the
  accepted bytes in a request-local slot, and writes back `attached: [{mediaType, bytes}]` so the
  shell and the model see metadata only. The ordinary result fields beside the key are untouched.
- **In: `chatAssetInputs: [<capability id>, …]`.** A capability input may name one of this
  conversation's own attachments as the exact string `chat-asset:<N>`, the number from its
  `Chat Asset #N` reference line. For a listed capability the leg replaces each marker with
  `data:<mime>;base64,<bytes>` before the proposal is submitted; image media types only. Three bounds
  apply together: at most **three expansions per invocation**, at most **8.5 MiB decoded per
  invocation**, and at most **twelve expansions per session** across every invocation it proposes.
  The session bound exists because expansion happens *before* the broker authorizes anything — without
  it a script could spend its whole capability budget proposing a listed capability and pull three
  attachments off the chat service on each one, even if policy denied every call. All three are
  separate from the model's own four `fetch_chat_asset` calls, so a remix cannot exhaust the agent's
  ability to read its own conversation. A capability *not* listed keeps the string verbatim and
  decides for itself. Unknown identifiers in the list are a startup failure naming each one.

Neither direction can fail a script. A refused attachment leaves `attached: []` (or the entries that
were accepted) plus one fixed gateway sentence the model reads, and the cause is audited once as
`agent.provider_attachment.refused` or `agent.chat_asset_input.refused` with a stable reason. A
refused *input* marker submits no proposal at all and comes back as the interpreter's non-retryable
refusal, because the marker will not become valid on a retry. Attachment bytes cross broker IPC
in expanded invocation inputs and provider results. The gateway strips result attachments before
returning them to the shell or model, and retains no attachment bytes in conversation history or
durable memory. Broker input spans include expanded attachment data; the gateway's byte-free result
convention is not a broker-telemetry filter. The reply
slot is dropped unread when a session fails or is cancelled.

Delivery uses each service's native upload path: one Slack three-step external file upload per
attachment with the answer text as the first upload's `initial_comment`, every attachment on Discord's
first multipart Create Message, one Telegram multipart `sendPhoto` per attachment with the caption on
the first, and a base64 `images` array on the local socket that is omitted entirely for a text-only
reply. Filenames are gateway-owned and carry the attachment's position, so two files in one reply do
not arrive under one name. WhatsApp has no path here — the Cloud API transport is text-only, and
sending an image through it would need Meta's separate media upload — so a route that names
`providerAttachments` on a `whatsappCloudApi` transport is a startup failure. Discovering that at
reply time would mean authorizing and paying for a PNG and then dropping it. `DeliveryReceipt` covers
the complete text/attachment reply. If Slack, Telegram, or a split Discord reply accepts only part,
the session is `reply-failed` and performs no durable record. Persistent history remembers only final
text; referring to prior pixels requires a fresh invocation.

Slack installations need `files:write` in addition to the existing reply/read scopes. Discord bots
need **Attach Files** in addition to View/Send/Read History/Send in Threads. Telegram needs no
additional bot permission.

## Transports

### Slack Socket Mode

An app-level token opens `apps.connections.open`, which returns a `wss://` URL; a bot token answers through `chat.postMessage` or Slack's external file-upload flow for an attachment. No public HTTP endpoint is needed, which is why Socket Mode rather than a public Events API request URL. [`../examples/slack/`](../examples/slack/README.md) has separate classic/free and paid/admin-enabled Agent manifests, plus the token and identity-mapping walkthrough.

`experience` controls Slack's conversation model and is never inferred from a cosmetic API result:

- `classic` (default) retains top-level DM replies and one whole-DM conversation. With native
  activity and `classicFallback: reaction`, the gateway adds its fixed `:tangerine:` reaction to
  the inbound message and removes only a reaction that generation successfully added.
- `agent` makes DMs thread-scoped like Slack Agent sessions and enables authorization-fed channel
  thread continuation. After fresh broker authorization the gateway calls
  `agents.sessions.setStatus(processing)` once; Slack owns the standard Working UI and one-hour
  processing timeout, so the gateway does not waste rate limit on a heartbeat. After a reply or
  no-reply completion it asynchronously returns the session to `active`; no-reply means
  no chat message, not omission of that cosmetic cleanup.
  `feature_disabled`, `missing_scope`, and equivalent permanent installation errors disable Agent
  status for that transport and select the configured reaction fallback, then no-op if reactions
  are also unavailable. It never guesses the workspace plan.

Slack's native `processing` state includes a Stop button. The transport acknowledges
`agent_session_stopped` before handling it, derives its user and thread only from Slack's envelope,
and lets the initiating subject win one atomic race against the normal answer. A Stop prevents
subsequent model turns and capability invocations, suppresses the stale answer/history commit,
queues `active`, and sends `Stopped.` An already-running synchronous model request or provider
effect cannot be rolled back and may finish before the prompt loop reaches its next cooperative
boundary. A provider command word the script is waiting on is the exception: its broker round
trip runs as one cancellable process node tied to the session's Stop, so the run is aborted and
joined and the script reads `session-cancelled` instead of waiting the broker out. Unknown,
duplicate, and other-user Stop events are ignored.

The Agent manifest also subscribes to `message.channels` and `message.groups`, requiring
`channels:history` and `groups:history`, so the transport can hear follow-ups that contain no new
mention. This is **owned-thread continuation**, not ambient activation:

- an explicitly addressed channel message proposes an exact authenticated
  `(workspace, channel, root thread, sender)` claim;
- only a fresh non-empty broker capability surface installs or refreshes that claim;
- a later unmentioned event must match every coordinate and is freshly authorized again before
  activity or inference; another sender and another thread remain ambient;
- a definitive authorization refusal removes the claim; the 1,024-entry LRU and every claim vanish
  on process restart; and
- all unmatched channel-history events are discarded inside the Slack transport before routing,
  authorization, payload telemetry, or model spend.

An inherited continuation is the only request whose reply is optional. The prompt explicitly says
that the agent need not take the last word and offers `decline_chat_reply`; selecting it before any
capability work posts nothing, produces no transport receipt or durable recording, and remembers
the user's message as a user-only in-process turn. A decline selected in the same turn as work runs
none of that work. If an earlier capability invocation already happened, silence is refused and the
model must send a concise report. With no model turn left, the gateway posts a fixed warning that
capability work was attempted and the audit must be checked before retrying. Explicit mentions and
DMs never receive the decline tool.

The protocol's one sharp edge is redelivery: Slack expects an acknowledgment within roughly three seconds and resends the envelope otherwise. A Dekopon session takes far longer than that, so **the acknowledgment is sent before any processing begins** — before parsing, before routing, before any model call. A bounded ring of 1024 seen `(channel, ts)` pairs absorbs the redeliveries that happen anyway across a reconnect.

- `disconnect` envelopes are routine (Slack rotates sockets on its own schedule) and trigger a reconnect with jittered exponential backoff capped at 60 seconds.
- Every read has a 90-second liveness deadline, and so does opening a socket — handshake and `hello` together. Slack pings a healthy connection about every 30 seconds and sends no client heartbeat of its own, so silence past the deadline means the path is gone without TCP saying so: a NAT table dropping the flow, or a partition with no RST. An expired deadline logs `gateway_transport_silent` and reports the socket closed, which is the reconnect path the backoff already owns. Without it the reader waits on a half-open socket forever and every route on the workspace goes quiet with nothing logged.
- Messages carrying `bot_id` and messages from the bot's own user identifier are dropped. Both checks matter: another app's post carries `bot_id`, and this app's own post arrives with the bot's user identifier and no `bot_id` at all.
- A subtyped message is dropped unless its subtype is `file_share`, `me_message`, or `thread_broadcast`. Most subtypes are events *about* a message — an edit, a deletion, a channel join — and answering one would answer a question twice or answer nobody. Those three are a person making a new request. `file_share` is the one worth naming: Slack stamps it on any message carrying an upload, so a question asked with a screenshot attached arrives under it. The list is an allowlist, so a subtype Slack introduces later is dropped until someone decides it is a request.
- A message's attachments become **chat assets**, described in the prompt and fetched only on demand. See [Chat assets](#chat-assets) below. The transport reports what arrived and nothing more: names and media types come from the event, so they are sender-controlled and untrusted exactly like the message text. An upload posted with no comment is a request in itself — the reference note is the whole message. A message with neither text nor a file is not a request and is dropped.
- `channel_type: im` is a direct message; anything else is a channel.
- Subject: `slack.<team>.<user>`, lowercased.
- A channel answer joins the thread it was asked in, starting one on the inbound message when there is none. A classic direct message has no thread to join; an Agent direct message is intentionally rooted at `thread_ts = event.thread_ts || event.ts`, and that root also scopes admission, history, status, Stop, and owned continuation.
- An answer is posted in a Block Kit [`markdown` block](https://docs.slack.dev/reference/block-kit/blocks/markdown-block/), which carries the model's CommonMark unchanged and lets Slack render it. The `text` field is mrkdwn — a proprietary syntax where bold is `*one asterisk*` and a link is `<url|label>` — so an answer posted through it alone arrives with `**bold**` as four literal asterisks, and tables and task lists cannot be expressed in it at all. Translating in this process would be a second translation of what Slack is about to translate, so the gateway does none: the block gets the answer verbatim. `text` carries the notification fallback, the one place blocks do not render. The block caps a payload at 12,000 characters, which the 8 KiB outbound bound already sits under.

### Discord Gateway

Discord Gateway v10 is another outbound WebSocket transport. The daemon discovers the service URL through authenticated `GET /api/v10/gateway/bot`, requests only the non-privileged `GUILD_MESSAGES` and `DIRECT_MESSAGES` intents, and identifies after Hello. It jitters the first heartbeat, requires each heartbeat ACK, tracks dispatch sequence, resumes a live session after reconnect, honors Invalid Session and identify/session-start limits, and treats Discord's fatal close codes as terminal transport failures. No public endpoint or privileged Message Content intent is required: Discord exposes content and attachments in DMs and in guild messages whose structured `mentions` array names the bot, and those are the only messages that may wake a session.

- Bots, webhooks, the bot's own posts, and message types other than ordinary messages and replies are dropped.
- Absence of `guild_id` is a direct message. A guild message is a channel message and must name the bot in its structured mentions. Subject: `discord.<user id>`; Discord user snowflakes are global, so a guild is not part of the canonical subject.
- A Discord thread is itself a channel. Its channel ID is the route key, conversation identity, and reply destination. A catch-all channel route covers transient threads; a route naming only a parent channel does not automatically claim its thread IDs.
- Replies use `POST /api/v10/channels/{channel}/messages`. Provider attachments ride the first post as multipart attachments; the first guild post references the incoming message with `fail_if_not_exists: false`; every post disables parsed/reply mentions, so model-authored text cannot ping a user, role, or `@everyone`. Discord's 2,000-character ceiling is handled by lossless multi-message splitting, with Markdown left unchanged. Failure after an accepted attachment or first chunk is partial delivery rather than a complete receipt.
- With `activity.mode: native`, an authorized session immediately triggers `POST /channels/{id}/typing` and renews around every eight seconds, inside Discord's ten-second native lease. Typing has no explicit clear; sealing stops renewal and the final message clears it sooner. Calls use a short deadline, honor a `429` cooldown, never take the final-message REST lock, and cannot fail the answer.

[`../examples/discord/`](../examples/discord/README.md) is the bot installation, permission, token, route, and identity-mapping walkthrough.

## Chat assets

A screenshot is part of the message that carried it. Slack, Discord, and Telegram deliver it by reference rather than by value, so the gateway resolves that reference in order to hear the whole request. Slack and Telegram require the bot token they already terminate here — and on Slack the `files:read` scope, without which Slack withholds the file's id and URL and the upload is reported as one the gateway cannot open; Discord CDN downloads do not receive it. This grants no policy, no provider credential, and no way to write anything.

What it does not do is read every file that arrives. Bytes cost tokens on every turn they appear in, and most turns do not need them. So each attachment is *named* in the prompt and fetched only if the model decides the answer depends on it:

```text
what does this say?

[gateway: the sender attached
  Chat Asset #1 — screenshot.png (image/png, 214 KB)
  recording.mov (video/quicktime, 41.3 MB) — not a type the gateway can show you
  Call fetch_chat_asset with the number to look at one.]
```

The model then calls `fetch_chat_asset(1)`. Because a tool result cannot carry an image — Chat Completions types a `tool` message's content as a string, and the Responses API types `function_call_output.output` the same way — the answer arrives as two messages: the tool result says the asset follows, and a `user` message carries the bytes. That shape is the only one both wire formats accept.

- **Numbering follows the exact history audience and live generation.** `Chat Asset #5` means at most one file in that generation, which is what lets a follow-up three turns later resolve. Its monotonic sequence survives independent asset TTL/LRU removal while the transcript generation remains live, so a removed number cannot alias a newer file. Grant/empty-grant invalidation, idle replacement, and conversation-capacity eviction close the generation's asset fence; stale sessions cannot enumerate, publish, or fetch through it, and a replacement generation may safely number from one again. Private participants and different agents/transports/conversations cannot enumerate or fetch one another's attachments. Participants on an explicitly shared route share the live attachment inventory as well as the replayed reference notes; that disclosure is part of choosing shared scope. Numbers are assigned by the gateway rather than by a transport.
- **Every prompt names the whole inventory**, not only what the newest message brought, with the new ones marked. A reference line is the only way a model learns a number exists, and one confined to the turn that introduced it goes unreachable as soon as ordinary chatter pushes that turn out of the replayed history window — while the store holds the file for another hour.
- **The reference line is what history remembers, not the bytes.** It is a few dozen bytes, so it replays inside the conversation byte budget instead of evicting real conversation the way a base64 screenshot would.
- **A file that cannot be shown is named anyway.** A media type outside the allowlist, a model with no image modality, or a file Slack withholds entirely all produce a line saying so, which the model can answer around instead of denying a screenshot that plainly exists.
- **Only the media types a model can actually accept are offered.** Images: `image/png`, `image/jpeg`, `image/webp`, `image/gif`. Documents: PDF, plain text, Markdown, CSV, HTML, XML, JSON, RTF, and the Word, PowerPoint, and Excel formats. A chat service imposes no allowlist on uploads at all — a 700 MB screen recording is a legal attachment — so the narrow end of that intersection is the one worth enforcing. A spreadsheet is parsed to its first thousand rows per sheet, which is worth knowing before concluding a model ignored the bottom of one.
- **A route's model has to opt in to images.** `modalities: [image]` on a model entry; the default is text only, because an OpenAI-compatible endpoint is very often a small local model that will either error or invent an answer when handed an image. Documents need no modality: a PDF is a parsed attachment to every endpoint that accepts one at all, so gating it on vision would refuse it to a model perfectly able to read it.
- **Bounds.** 8 MiB per attachment, enforced while the response streams rather than after it, because a reported size is sender-influenced and a chunked response need not declare a length. Four fetches per session. Thirty-two attachments addressable per conversation, evicted oldest-first. A textual file is clamped again on the way into the prompt, at the same 256 KiB a script's output is capped at, with a trailer saying where it was cut: the 8 MiB ceiling is sized for images on the wire, and that much `text/plain` is roughly two million tokens — enough to come back from the provider as a context-length rejection. Every one of these refuses in a sentence the model reads and can answer around, never by failing the session.
- **Redirects.** The HTTP client refuses redirects globally so a bearer token is never forwarded by policy. Slack's `url_private_download` genuinely redirects to its own file host, so that transport follows exactly one hop, only to a host it recognises by comparing the host itself rather than a URL prefix, and re-attaches the token by hand.
- **Resolving a reference differs by transport.** Slack carries a private download URL on the event itself. Discord carries a signed CDN URL plus the source channel/message/attachment IDs; the CDN request carries no token, and an expired 401/403/404 URL is refreshed by re-reading that exact message through pinned Discord REST before retrying the same attachment ID. Telegram carries only a `file_id`, so a fetch is two calls: `getFile` turns the handle into a path valid for about an hour, and the bytes live under `/file/bot<token>/<path>` rather than the method prefix. The round trip happens at fetch time, which is also when that path is freshest.
- **Discord specifics.** Photos and arbitrary files share the attachment object, retaining their sender-controlled filename, optional media type, and reported size. Production downloads accept only HTTPS `cdn.discordapp.com` or `media.discordapp.net` URLs, reject credentials and redirects, and enforce the byte ceiling while streaming.
- **Telegram specifics.** A photo arrives as the same image at several sizes and the largest is the one used — a model asked to read text in a screenshot cannot read a 90-pixel-wide copy. Telegram reports no media type for a photo, so `image/jpeg` is inferred, which is what the Bot API re-encodes every photo to; a file sent as a *document* keeps its own bytes, name, and declared type. Words on an upload arrive in `caption` rather than `text`.

### Telegram long polling

`getUpdates?timeout=50&offset=N` blocks server-side and returns as soon as anything arrives, so waiting costs one idle connection. **The poll is the wakeup and advancing `offset` is the acknowledgment** — there is no separate ack call and therefore no ack-before-work problem. The offset advances past every update, including ones the daemon chose not to route, or a filtered bot message would return forever.

Messages from bots are dropped. A private chat is a direct message; a group is a channel. Subject: `telegram.<user id>`. A forum `message_thread_id` creates the distinct canonical conversation `<chat>:topic:<id>`, and the reply carries that same thread ID; plain messages retain the chat itself as their conversation.

`sendMessage` refuses text over 4,096 UTF-16 code units, which is half the gateway's own outbound bound, so an answer is split losslessly across sequential messages the same way Discord's is. Only the first quotes the incoming message; the topic identifier goes on every one, because it is what keeps a continuation in the same forum topic.

Telegram's optional `message_thread_id` is preserved consistently in admission, conversation
identity, replies, generated-photo uploads, and activity, so a forum-topic pulse cannot appear in
another topic. Generated PNGs use `sendPhoto`; text up to Telegram's 1,024-unit caption ceiling is
accepted with the first photo, while longer text follows as losslessly split `sendMessage` calls; a
failure after any accepted part is partial delivery. With
`activity.mode: native`, an authorized session sends `sendChatAction(action=typing)` and renews
around every four seconds inside Telegram's five-second lease. There is no explicit clear; renewal
stops before the final message, which clears the action. Calls override the long-poll client's
70-second timeout with a short deadline, honor `retry_after`, and remain cosmetic.

### Meta WhatsApp Cloud API

The `whatsappCloudApi` transport is an inbound plain-HTTP listener intended to sit behind
Cloudflare Tunnel and Traefik (or equivalent operator-owned HTTPS termination). Its configured
callback path exposes only GET subscription verification and POST webhook delivery. GET requires
exactly one `hub.mode=subscribe`, verify token, and challenge, compares the token in constant time,
and returns the decoded challenge without JSON quoting. POST bounds connection time, headers, body,
concurrency, message count, and queue depth; requires exactly one
`X-Hub-Signature-256` whose value is `sha256=<lowercase hex>`; and verifies HMAC-SHA256 over the exact raw body before JSON
parsing. The callback path is a literal lowercase-segment path—wildcards, captures, empty segments,
and trailing slashes are rejected at startup. Responses carry `Cache-Control: no-store`; errors and
logs are content-free.

Only `object=whatsapp_business_account`, `field=messages`, `messaging_product=whatsapp` events for
the configured exact WABA/receiving-phone tuple may produce sessions. Every entry, change, and
message in a signed batch is inspected. Status-only, unknown, malformed non-message, unsupported
message type, wrong-destination, and self/echo messages are acknowledged and ignored. Ordinary text
uses signed `messages[].from` both as reply target and as the sole identity source; profile names,
display phone numbers, message text, WABA IDs, and phone-number IDs cannot assert the sender.
Canonical subject is `whatsapp.<wa_id>`. The WABA, receiving phone number, and sender remain in the
transport-derived chat scope as `<waba>:<phone-number-id>:<wa_id>`.

The handler claims signed `messages[].id` values in a 4,096-entry process-local set and atomically
enqueues one bounded delivery before returning HTTP 200. One delivery carries at most 128 text
messages, and the queue admits at most 512 messages across 64 delivery slots. Duplicates seen by
that running process are acknowledged without another session. Restart forgets the claims, and a
crash after the 200 but before queue drain loses the accepted work. Queue saturation returns 503 and
rolls back new claims so Meta can redeliver.

Replies are bounded JSON POSTs to the pinned
`https://graph.facebook.com/{version}/{phone-number-id}/messages` endpoint with the gateway-held
bearer token. Redirects are disabled, responses and time are bounded, and Meta error bodies never
reach chat or logs. WhatsApp accepts 4,096 Unicode scalar values per text message and the session's
own outbound bound is 8 KiB, so a long answer is split at a line boundary where one exists and sent
as consecutive messages rather than truncated — the same rule the Discord transport follows. A
failure after the first chunk is `partial-delivery`: the answer arrived in part, the underlying
service category is logged once as `gateway_whatsapp_reply_partial`, and no delivered turn is
recorded. No send is retried: a timeout after request transmission is outcome-unknown and blindly
resending could duplicate a visible answer. After Graph accepts every chunk, the signed inbound
message ID becomes the service-typed delivery identity for optional durable chat memory, bound to
the WABA and receiving phone number in the attested scope. Failed or outcome-unknown replies record
no delivered turn. Free-form text remains subject to Meta's customer-service window; there is no
template fallback.

Refusals are visible without being a megaphone. Every refused request emits
`gateway_whatsapp_webhook_refused` with a stable `reason` — `unsigned`, `signature`, `oversize`,
`malformed`, `saturated`, `timeout`, `verification`, `unavailable` — its HTTP status, and nothing
about its content. A stranger decides how often those happen, so each reason is emitted at most once
a minute carrying the number of refusals it stands for: a wrong app secret is one obvious line, and
a flood is one line a minute too. A failed `accept()` is classified rather than treated as the end
of the listener, because nothing restarts a transport reader: a dead connection is debug-level and
ignored, descriptor or buffer exhaustion is warned and retried after a short pause, and only a
listening socket that can never serve again stops the loop with
`gateway_whatsapp_listener_stopped`.

Media, templates, interactive messages, reactions, activity, status processing, business-management
APIs, embedded signup, webhook multiplexing, and daemon TLS termination are outside this transport;
the project-wide list is [non-goals](design.md#non-goals). See
[`../examples/whatsapp/`](../examples/whatsapp/README.md) for placeholder-only setup.

### Local development transport

An owner-only (`0600`) Unix socket under a private parent directory, with `dekopon-brokerd`'s socket hygiene: the parent must be an owner-owned directory with no group or world access, an existing socket is replaced only if it is already private and single-link, and the guard removes only the exact inode it created. Line-delimited JSON in, line-delimited JSON out on the same connection:

```console
$ nc -U /path/to/dekopond-dev.sock
{"subject": "tel.16034700182", "channel": "dev", "text": "what changed today?"}
{"reply": "Nothing external. Two read-only capability calls."}
```

Text-only output keeps that exact shape. Provider attachments add an `images` array containing the
gateway-owned `filename`, `mediaType`, and base64 `data`; the field is absent otherwise. The local
line can therefore approach the base64 expansion of the 8 MiB decoded bound and remains a
development protocol rather than a compact production transport.

**This transport trusts its local caller to declare a subject.** That is the whole point of it — it exists so a developer can drive a routed session without a Slack workspace — and it is why it is a development tool rather than a production transport. It grants nothing by doing so: the declared subject is only a claim carried into the broker's chat-attested `invoke`, and the broker needs an attestor grant covering that namespace plus an owner-controlled mapping before it resolves to a principal. Its `0600` mode keeps it reachable only by the owner's UID, the gateway's local trust domain.

On the default private scope, a declared subject also selects history **inside that configured local transport**. A caller can replay compacted exchanges previously created under the same local transport, agent, direct-message identity, and declared subject, but cannot alias Slack, Discord, or another configured transport because the transport component differs. On explicit shared scope the subject is intentionally absent from the state key, so every authorized caller of that local route shares its one local direct-message conversation. No authority moves — the broker decides every invocation for itself — but text does, which is a second reason this socket is `0600` and a development tool.

The shared-prompt label says `authenticated participant` uniformly across transports. Here it means the canonical participant claim accepted on a fresh broker leg from the owner-UID-authenticated local caller; it does **not** mean the development socket independently authenticated the declared person. Every local message is a direct message; channel routes are a chat-service concept.

## Routing

First match wins on (transport name, direct message or channel). The channel is optional: `{ kind: channel, channel: c0123abc }` claims that one channel, and `{ kind: channel }` claims **any** channel the bot is in. Unmatched messages are ignored with a debug-level event — bots see ambient traffic, and silence is the correct answer.

Leaving `channel` out exists because naming them does not scale. One route per channel means enumerating service-native identifiers and editing this file again every time somebody creates a channel, and until an operator notices and redeploys, the bot is silent in the new one while appearing to be deployed workspace-wide. An absent `channel` says "wherever I am invited", which is membership the chat service already controls.

**Declaration order is the whole precedence rule.** Routes are consulted top to bottom, so a named-channel route written above a catch-all keeps that channel for itself while the catch-all takes everything else — special handling in `#incidents`, the default everywhere else. Nothing sorts by specificity: a hidden ranking is how an operator ends up unable to say which route answered.

A channel initially requires the bot to be addressed: `<@BOT_USER_ID>` on Slack, a structured `mentions[].id` match on Discord, or `@botname` on Telegram. **A route decides which agent answers; an explicit address decides whether a new channel conversation starts**, and widening the first leaves the second exactly where it stood. Discord and Telegram retain that rule on every message. Slack classic does too. Slack Agent has one bounded exception: after an explicitly addressed message is freshly authorized, the same authenticated sender may continue without another mention inside that exact owned root thread. Every continuation is authorized again and may decline to post; all non-owned channel history is dropped before routing. The Agent manifest therefore receives ambient public/private channel events, while the classic manifest remains mention-only.

### Being available in a channel is not authority

A route matching every channel widens no authority whatsoever, and an operator reading "available in all channels" must not read it as "available to everyone". Every session opens an attested broker leg naming the sender's canonical subject; the broker maps that subject to a principal, requires policy permitting `agent.prompt` for it, and refuses an unmapped sender before any model call is made. A catch-all route changes *where* the mapped people can reach the bot. It does not change who they are, and somebody the owner never mapped gets the same refusal in a catch-all channel that they would have got in a named one.

Nor do two people in one channel share a conversation **by default**. A persistent route whose scope is omitted or `privateConversation` keys history per authenticated subject, so the bot remembers each person separately. An operator can explicitly choose `sharedConversation`; that changes prompt audience, not authority, and carries the warnings in [Scope selects the replay audience](#scope-selects-the-replay-audience). A catch-all channel does not imply that choice.

## Sessions

Each routed message runs one session. On a `oneShot` route — the default, and every route in a configuration that never writes a `conversation:` block — that session is entirely independent, and the `persistent` clauses in steps 4 and 5 are the whole difference the other mode makes:

1. **Admission.** A process-wide semaphore bounds what the daemon costs at once, and a per-`(transport, channel, thread)` in-flight set stops one conversation from queueing work on itself — what a person does when a bot seems slow and they send the same thing again. A rejected message gets `I'm busy — try again shortly.` when `replyOnBusy` is set, and silence otherwise.
2. **Authorization.** The session opens an attested broker leg with `capabilities(subject, agent, scope)`. If the answer is empty — or the broker refuses, because the attestation was not honored or because policy does not permit this principal to drive this agent — the sender gets `You're not authorized to use this agent.` and **no model call or activity write is made**. That is the cheapest possible refusal, and one the message text cannot argue with.
3. **Activity.** When the transport opted in, one session-owned generation starts immediately after the fresh grant. The service renders it; the model supplies no target, status text, frame, emoji, or timing. The coordinator permits one request at a time, refreshes expiring signals, seals synchronously before terminal delivery, and queues cleanup afterwards so cosmetic I/O never delays the reply or holds admission. Two consecutive failures stop renewal for that generation; permanent Slack installation failures additionally trip a transport-wide fallback breaker.
4. **Execution.** On a `persistent` route the session first looks up its conversation under the key in [Scope selects the replay audience](#scope-selects-the-replay-audience). An entry idle past the route's timeout, or built under a granted capability set that differs from the one this message's leg just reported, is dropped rather than used; whatever survives is seeded into the prompt ahead of the new message as compacted `(question, answer)` pairs, oldest dropped first until the window's turn and byte bounds both hold. A shared turn's user text starts with a gateway-authored canonical-participant label, for both the current message and later replay. The lookup happens *after* step 2 because the grant comparison needs a fresh grant to compare against. Then the model client is built from the route's model, the shell runtime is given the attested leg as its only capability dispatch, the credential-free `inspect_agent_config` view is built from the same fresh leg, the route's `providerAttachments` slot and `chatAssetInputs` expansion are attached to that leg, and the prompt loop runs on a blocking task with the agent's `instructions` as the system prompt. The agent's catalog skills ride the bound route — read whole into memory when the catalog loaded and shared by every session rather than re-read, so a session never touches the filesystem — and are mounted on every session on that route: a second system message after the instructions lists each by name and description, and the `read_skill` tool loads one skill's instructions, or one of its resource files, on demand. A route with `improvementSuggestions: true` additionally offers `suggest_improvement`; what it records is written to telemetry as `agent.improvement.suggested` and is never relayed to chat, so the sender sees only the answer. Instructions are supplied fresh on every message and never stored, so editing an agent's standing orders takes effect on the next message without rewriting a single remembered conversation. Shell bounds are `dekopon-shell`'s defaults except `maxCapabilityCalls`, which comes from the route. Every model request the session then makes declares a [prompt cache key](#the-prompt-cache-key) — the conversation's on a `persistent` route, the route's on a `oneShot` one.
5. **Answer, silence, and optional durable recording.** A required session's final bounded text and accepted provider attachments go back to chat. An inherited Slack Agent continuation may instead call `decline_chat_reply` before capability work, which commits its user-only in-process turn, cleans up activity, and sends no reply request. On failure the sender gets one fixed line, `The agent could not complete this request.` — a `PromptError` can carry model-chosen text, a provider message, or a transport diagnostic, and chat is the last place any of those belong. The operator reads the category from telemetry. A `persistent` route writes only the textual exchange back as one more in-process remembered turn, trims the window, and restarts the idle clock. A generation lease makes a commit from older in-flight work inert after grant invalidation, empty-grant removal, idle replacement, or capacity eviction, while concurrent work in the same generation appends in completion order. **The fixed failure line and attachment bytes are never stored.** A declined or failed model session records its question with nothing in the in-process answer's place, which is truthful and is what makes a later follow-up answerable; a session refused at step 2 records nothing at all. Optional durable recording happens under the conditions in [Durable memory after transport acceptance](#durable-memory-after-transport-acceptance).

Text is bounded in both directions: inbound to 16 KiB keeping the head (a chat message states its request first), outbound to 8 KiB keeping head and tail (an answer's conclusion is usually its last line). Both truncations say so in the text.

At shutdown, transport readers are aborted and in-flight sessions get `shutdownGraceMs` to finish — a model call is already paid for, and abandoning it means a person watching a chat window never hears back. If the grace expires, dropping each async owner marks its synchronous prompt loop cancelled before aborting the wrapper, so no later model turn or capability call starts. A model request or provider effect already in progress remains non-rollbackable and may finish after the async owner is gone.

**Abandonment is bounded, so exit is too.** A cancelled prompt loop observes its flag at its next cooperative boundary, which can be on the far side of a whole synchronous model round trip, and dropping an async runtime waits for every such thread. The daemon therefore owns its runtime and gives that final wait five seconds before exiting anyway. Without it, worst-case exit is `shutdownGraceMs` plus a model timeout — on the reference deployment 120 s + 120 s — inside a pod termination grace that `dekopon-brokerd`'s own drain has to fit into as well, because the kubelet only starts stopping the broker sidecar once this container is gone. The chart defaults that grace to 270 s and asserts at template time that it covers both drains plus `drainBudget.bufferSeconds`, refusing to render anything shorter ([`charts/dekopon/README.md`](../charts/dekopon/README.md#draining-takes-both-graces-in-sequence)); 240 s of gateway abandonment does not leave the broker its 120 s inside it, and the kubelet SIGKILLs the broker mid-drain.

**Losing every transport is a failure, not a stop.** A transport that cannot recover on its own ends its own reader — a revoked Discord token, a fatal gateway close code — and the daemon keeps serving whatever is left. That degraded state is re-announced as `gateway_transports_degraded` every 60 seconds rather than logged once, because the deployment has no gateway probe and the condition long outlives the line that reported it. When the *last* reader ends with no shutdown requested, nothing can wake the daemon again: it exits non-zero instead of reporting the success that let a gateway with no workspaces left look like a clean run.

## Conversations

**Status: Current.** History is a trust surface rather than a feature flag; [`security-model.md`](security-model.md#conversation-memory-as-a-trust-surface) states the surface it accepts.

A `persistent` route keeps a bounded history and replays it into the next prompt, so a follow-up question can say "and the second one?" and be answered. The history is private per authenticated subject by default; `scope: sharedConversation` shares it among authenticated participants in one exact routed conversation. `oneShot` is the route default.

### The history lives in the gateway

This subsection describes the automatic replay window, not the separate on-demand durable provider. The replay window lives in the daemon's memory and nowhere else. It is never written to disk, never sent to the broker, and is lost on restart: `dekopond` comes back with every conversation forgotten, and a person who asks a follow-up across a restart gets a first-message answer.

That placement is the whole point rather than an implementation shortcut. The broker holds provider credentials and decides every invocation; conversation text there would put the most sensitive content in the system inside the most privileged process. The gateway already handles this text — it read the message and it wrote the answer — so keeping the history there adds no new reader.

### Scope selects the replay audience

The effective keys are intentionally exact:

- `privateConversation` (the default): `(agent, configured transport, transport-derived conversation identity, canonical subject)`;
- `sharedConversation`: `(agent, configured transport, transport-derived conversation identity)`.

The agent boundary means two agents never share transcript or attachment state even when they are routed on the same transport conversation. The configured transport boundary prevents lookalike identities from different installations or services from aliasing. Private scope adds the canonical subject from the transport envelope accepted for the fresh broker leg, so one participant claim never receives another's history. Shared scope removes **only** that subject component; it is not agent memory, team memory, a namespace shared by routes, or any replay beyond this exact conversation.

The conversation identity is transport-derived, not mechanically `(channel, thread)`. Slack omits `thread_ts` on the message that *starts* a thread and sends it on every reply inside one, while the bot answers that first message in a thread rooted at it. Slack shared history is therefore normally root-thread scoped (and a direct message has its direct-message identity). A Discord guild message has the channel as its conversation identity: on a shared route, **the whole guild channel identity shares one replay window** until Discord creates a distinct native thread channel. Choosing shared scope on a broad Discord channel can disclose one participant's prior prompt and the agent's answer to every other mapped participant who can invoke that route there. Treat that as an explicit audience expansion, not as a convenience toggle.

Shared user turns are sent to the model as exactly:

```text
[gateway: authenticated participant: <canonical-subject>]
<existing user text>
```

The gateway writes the first line from the transport envelope accepted for the fresh broker leg after composing any attachment reference note. User text remains untrusted and may contain a lookalike label; it cannot replace the gateway-authored first line. The labelled bytes are retained in the bounded history, so replay preserves who said each turn and the label counts against `maxBytes`. Private persistent and one-shot prompt text is unchanged byte for byte and receives no label.

**The canonical participant identifier reaches the model provider on every shared turn.** The telemetry gate controls exports to the configured telemetry sink, not the prompt sent to the selected model endpoint. A canonical subject may be a phone number or service user ID. Enable shared scope only when sending those identifiers, earlier participant text, and agent answers to that model provider is acceptable for the whole conversation audience.

The state key is *not* the admission key from step 1, which is `(transport, channel, thread)` and has no agent, scope, or subject. The two keys answer different questions. Serialization asks "is this bot already busy on this thread"; state asks "which exact route audience owns this transcript and attachment inventory". Admission therefore does not serialize every possible shared-state race. The store gives each session a generation lease and attachment-access fence: sessions in one live generation append in completion order and reuse its inventory, while removal, replacement, or eviction makes every older lease inert and closes its asset fence. Stale in-flight work can therefore neither recreate forgotten history, rename its cache lane, publish into a replacement inventory, nor start a metadata/byte fetch through a retired one. A transport read already started while the generation was live may finish concurrently, but a final fence check discards those bytes instead of sending them to the model after retirement.

### Prior turns are compacted

A stored turn is `(the user's message, the final answer)`, or the message alone when the session failed or declined an optional reply. Every intermediate step — the model's tool calls, the scripts it authored, and their output — is dropped at write-back and never replayed.

The number that forces this: one script's combined output can reach 256 KiB, which is `dekopon-shell`'s default `max_output_bytes` in [`../crates/dekopon-shell/src/limits.rs`](../crates/dekopon-shell/src/limits.rs). Replaying full transcripts would let a single earlier turn cost more than the entire window budget, and it would do so most on exactly the sessions that did the most work.

The loss is real and worth naming: the model cannot re-read a command it ran three messages ago, only what it said about it. If it summarized badly, the bad summary is what persists. A `persistent` route buys continuity of conversation, not continuity of evidence — the broker's audit log is where what actually happened is recorded.

### The bounds

| Setting | Where | Bounds |
|---|---|---|
| `mode` | route | `oneShot` (default) or `persistent` |
| `scope` | persistent route | `privateConversation` (default) or explicit `sharedConversation` |
| `idleTimeoutMs` | persistent route | How long an untouched conversation survives; default 900000 |
| `maxTurns` | persistent route | Exchanges the window replays; default 12 |
| `maxBytes` | persistent route | Bytes the window replays; default 65536 |
| `maxConversations` | `sessions:` | Conversations the process tracks at once; default 1024 |

`maxTurns` and `maxBytes` both apply, oldest turns dropping first until both hold. Two bounds because they fail differently: twelve one-line exchanges and twelve paragraph-length ones are the same number of turns and very different prompts.

`maxConversations` lives under `sessions:` rather than in the route block because it is a property of the process, not of a route, and `sessions:` is already where "what this daemon costs at once" is configured. It is a memory bound and not an admission bound: reaching it evicts the least recently used conversation rather than refusing a message, because a person in the middle of a conversation matters more than one who stopped an hour ago. An eviction is logged as `gateway_conversation_evicted` with a reason, so a ceiling set too low is visible as churn instead of as a bot that intermittently forgets.

Neither eviction runs on a timer. There is no sweeper task and no shutdown hook: the idle timeout is checked by the lookup that would otherwise have used the entry, and the ceiling is enforced by the write that would otherwise have exceeded it. Closing a conversation generation makes its asset metadata and byte source immediately inaccessible through every stale session; the independently bounded asset map may retain that inert metadata until its next operation prunes it. All state is process memory and dies with the process.

### Authorization is never cached

Every message opens a fresh attested broker leg and gets a fresh chat-scoped `capabilities` answer, exactly as step 2 already describes. Persistence changes nothing here: no grant is remembered, no decision is carried forward, and history is prompt text rather than authorization input.

The granted capability set is additionally **stored with the conversation** and compared on every message. Any difference drops the history and attachment generation and starts a fresh conversation; an empty grant removes the entry outright and closes the same asset fence. The reason is narrow and specific: output and attachment references from a session with a broad grant are sitting in the retained state, and if the owner then narrows what that subject may reach, an unchecked entry would keep replaying or fetching them after the capability that produced them was taken away. Invalidation costs a cache miss on the first message after any policy change, which is the right price — a narrowed grant is precisely when replaying old output is wrong. This comparison remains conservative on a shared route: if two participants receive different capability identifier sets, moving between them resets the shared transcript and inventory rather than carrying state produced under the other set. Sharing never promotes either participant to the other's grant.

Its reach is exactly the granted capability *identifiers*, which is less than it sounds like. A policy edit that keeps the same capability list but tightens its owner-authored constraint set — a narrower allowed host, a smaller output ceiling, a different credential — produces an identical grant set and does not drop the history. Text fetched under the older constraints stays in the prompt until the window or the idle timeout removes it.

### Why fifteen minutes

The idle-timeout default is pulled in two directions and loses one of them.

The ChatGPT subscription endpoint publishes no prompt-cache lifetime. Public OpenAI API policies vary by model and retention mode, so tuning a user-visible memory timeout to one guessed provider TTL would couple two mechanisms that do not share a contract. Human conversational memory runs on a longer clock: someone who asks a follow-up after a meeting expects the bot to know what they were discussing, and a bot that forgot after a brief lull is the failure people report.

The default is 15 minutes, which resolves toward the person because the user-visible point of this feature is memory, not a cache hit. **The cost control is the window, not the cache:** `maxTurns` and `maxBytes` bound what any one message pays no matter how long its conversation has been alive. [`inference.md`](inference.md#provider-retention-what-can-be-said) records the public API comparison, the undocumented subscription boundary, and why keeping a process alive does not pin a provider cache.

### The prompt cache key

Every model request carries a `prompt_cache_key`, on both model backends. **It is a routing hint and never an access-control boundary.** It tells the provider which requests are likely to share a leading prefix so they can land on one cache; it authorizes nothing, isolates nothing, and hides nothing. The request carries the whole conversation either way, and a backend that ignores the field returns a byte-identical answer at full price. Two requests sharing a key share nothing else: authorization is asked per message, on a fresh attested leg.

**It carries nothing about the private subject or shared conversation identifier.** The key is an opaque identifier *minted* when the thing it names is created — not either audience coordinate, not a hash of one, not a salted one. A canonical subject can be a phone number, so sending it would hand a model provider the sender's identity in exchange for routing that happens anyway; hashing it does not fix that, because a hash of a stable subject is a stable pseudonym. A configured salt is worse again: a new secret to manage whose only purchase is a pseudonym that survives restarts.

Where it comes from, and how long it lives:

| Route mode | Key names | Minted | Rotates when |
|---|---|---|---|
| `persistent` | one scoped conversation: private `(agent, transport, conversation, subject)` or shared `(agent, transport, conversation)` | with the conversation entry | the entry is evicted — idle, capacity, changed grant, or empty-grant removal — or the process restarts |
| `oneShot` | one bound route | once, at startup, when routes bind | the process restarts |

Rotation keeps it from becoming a durable identifier for a person or service-native shared conversation, and it is also just correct: an evicted conversation rebuilds a prompt that shares no prefix with the one it replaced, so continuing to name the old lane would be a guaranteed miss.

A `oneShot` route's key is shared by **every sender that route answers**. That route's shared prefix is the agent's `instructions`, the skills listing when the agent mounts any, and the tool definitions, then this one message: the shared part is identical for everyone the route serves and contains nothing about any of them. Nothing sender-specific can hit — a different sender's message diverges from the first token that differs, and a cache key is a hint about a shared *prefix*, not a handle on somebody's answer. A fresh key per message would name a lane holding exactly one request and give up the only caching a stateless route can have.

What the key is worth is measured, not assumed — [`inference.md`](inference.md#how-to-evaluate-caching-in-a-deployment) has the counts and how to read them.

### What this means for retention

On a `persistent` route, chat text sits in `dekopond`'s memory for at least the idle timeout after somebody stops talking — on the default, fifteen minutes of a person's question and the agent's answer. With shared scope, that retained content and its attachment inventory belong to the exact conversation audience rather than one sender. **At least**, because eviction is lazy: an abandoned conversation is dropped by the next lookup on its key or by the ceiling displacing it, so with neither happening the bytes stay in the process until it exits. What a timed-out entry can never do is reach a prompt. The daemon writes none of it to disk; the operating system's own paging and core-dump behavior are outside what the daemon controls. Another process under the gateway UID is inside its trust domain; see the [current process boundary](#current-process-boundary).

## Durable memory after transport acceptance

The gateway receives an optional `ChatMemorySurface` only when the agent is enabled and the broker
freshly permits all three exact memory capabilities under a matching subject namespace,
owner-authored `chatScopes` grant, canonical transport/channel/conversation claim, storage
constraint, and Cedar context. Otherwise recent/search, the `memory` word, the prompt note, durable
recording, and namespace creation are all absent.

When present, the model may retrieve on demand:

```text
memory recent --last N
memory search --query TEXT
```

It cannot resolve or invoke record. After model success, the gateway bounds the final answer once
(empty output uses the fixed normal answer), asks the transport to accept those exact bytes, and
only then opens one fresh broker client for one `recordDeliveredTurn` carrying the session's chat
attestation. The recorded user text is the original bounded sender text, excluding generated
attachment reference notes; assistant text is exactly what the transport accepted. No response,
denial, timeout, EOF, partial Discord delivery, or outcome-unaudited is retried, and none changes
the already delivered `answered` outcome.

Receipts mean complete **transport acceptance**, never human receipt: Slack and Telegram require an
HTTP success status before accepting `ok: true`; Slack also validates channel and strict canonical
timestamp, Telegram validates message/chat/topic and replies inside the topic, Discord validates
every split message and treats a later failure as partial, and local acknowledges only after
`write_all` and `flush`. The hidden request carries a tagged service-specific inbound delivery
identity, and the broker checks its channel/topic/transport fields against the attested scope before
namespace creation, preventing cross-transport aliases. Local identities include a 128-bit
OS-random boot nonce, connection, and sequence, so restarts do not collide.

Durable retrieval is not conversation replay. It is never automatically inserted into a later
prompt. JSONL deduplication is permanent but finite; at capacity recording stops while reads
continue. There is no deletion/export UX or encryption-at-rest claim.

## Authorization flow

```text
chat service            authenticates the sender
      |
      v
dekopond                subject = ExternalSubject::{slack,discord,telegram,whatsapp}(...), or the local caller's declared subject (routing metadata, not authority)
      |                 agent   = the route's catalog agent
      |
      | capabilities(subject, agent, scope)  ── empty ⇒ refuse, no model call
      | invoke(proposal, subject, agent, scope)
      v
dekopon-brokerd         attestor grant bounds the namespace
                        identityMappings turn the subject into a principal
                        policy must permit agent.prompt for that principal and agent
                        policy conditioned on context.via decides what it may then reach
                        credentials resolve, the provider executes, audit records it
```

The broker is the sole authority. `dekopond` supplies the subject and never the principal; a refused attestation is an audited denial recorded against the gateway's own peer identity. Driving an agent at all is its own policy statement — `Dekopon::Action::"agent.prompt"` over `Dekopon::Agent::"<name>"` — so a mapped subject the owner never permitted to use this agent is refused before the capability listing is even assembled, and a chat-attested `invoke` under such a session is the audited denial `agent-denied`. See [`security-model.md`](security-model.md) for the complete attestation contract, and note in particular that **a policy written for direct peers can never authorize an attested context and vice versa** — adding a gateway cannot widen a grant that already existed.


## Telemetry

Spans follow [`observability.md`](observability.md):

| Span | Fields |
|---|---|
| `gateway.message` | `transport`, `agent`, `outcome` (`answered`, `declined`, `unauthorized`, `busy`, `failed`, `cancelled`, `reply-failed`) |
| `gateway.session` | `agent`, `conversation.turns`, `conversation.bytes`; wraps the broker leg and the model session |

The prompt loop's own spans (`prompt.session`, `prompt.model_turn`, `prompt.script`, `shell.script`, `shell.command`) nest under `gateway.session`, and the broker's `broker.invocation` joins the same trace through the proposal's `traceParent` field (a W3C `traceparent` value); [`observability.md`](observability.md#gateway-spans) is the authoritative list.

Chat text and canonical subject identifiers reach telemetry as the `gateway.message.received` log event, on every message. The prompt cache key rides its own log event, `gateway.session.cache_key`, so a key and a canonical subject never appear on one line. A route with `improvementSuggestions: true` writes its `agent.improvement.suggested` records beside them. None of this decides model input: the gateway-authored canonical participant label is sent to the selected model on every shared turn regardless.

`gateway.session` carries `conversation.turns` and `conversation.bytes` — how much history this message replayed, as a count and a byte total and never as text; both are zero on a `oneShot` route and on the first message of any conversation. `gateway_conversation_evicted` is in the lifecycle events below with a reason of `idle`, `capacity`, or `grant-changed`. On a seeded session `message.count` counts the replayed window plus this exchange rather than this exchange alone. [`observability.md`](observability.md#what-conversation-history-changes) has the dashboard consequences.

Lifecycle events on stdout as structured JSON (this is the lifecycle subset, not every `gateway_*` record the daemon emits): `gateway_broker_ready`, `gateway_transport_connected`, `gateway_started` (transport and route counts), `gateway_session_rejected`, `gateway_session_failed`, `gateway_session_cancelled`, `gateway_session_stop_requested`, `gateway_activity_degraded`, `gateway_conversation_evicted`, `gateway_transport_disconnected`, `gateway_transport_silent` (transport and phase), `gateway_transport_stopped`, `gateway_transport_jitter_unavailable` (an operating system that refused the entropy every reconnect delay is jittered with), `gateway_transports_degraded` (dead and configured counts plus the configured names, repeated every 60 seconds for as long as any transport stays dead), `gateway_stopped` (`shutdown` or `transports-lost`). Beyond lifecycle: `gateway_message_ignored` (debug for an unrouted or unaddressed message) and `gateway_local_request_rejected` (debug); `gateway_reply_failed`, `gateway_memory_record_failed`, `gateway_session_stop_ignored` (debug), `gateway_session_registry_conflict`; `gateway_transport_poll_failed` and `gateway_transport_reconnect_failed`; `gateway_sessions_abandoned` and `gateway_session_task_failed` (shutdown grace expired, or a session task panicked); `gateway_whatsapp_accept_failed` and `gateway_whatsapp_image_unsupported`, plus the `gateway_whatsapp_webhook_refused`, `gateway_whatsapp_reply_partial`, and `gateway_whatsapp_listener_stopped` records named in that transport's section; `gateway_signal_failed`; and at exit `gateway_exit` and `gateway_telemetry_shutdown_failed` — see [`observability.md`](observability.md#daemon-exit-and-shutdown-records). Activity-call failures are debug-level `gateway_activity_failed` records. They carry only operation and stable category; degradation carries transport and surface. Neither includes a subject, target identifier, status text, raw service response, or credential. Other failure events likewise carry stable categories, and an eviction carries a reason and nothing about the conversation it forgot. An optional no-reply decision closes `gateway.message` with `outcome=declined`; its `agent.reply.declined` record carries only the model-turn number and no text or thread coordinate.

## Current process boundary

The chart enforces the [current local process boundary](security-model.md#current-local-process-boundary).
The gateway's distinct UID owns its configuration, model credential and process memory,
not provider credentials or broker state. Another process under the gateway UID can
act as that peer; the local development transport does not independently authenticate its
declared subject.

## Related documents

- [`design.md`](design.md) — the [constitution](design.md#constitution) and the authority model this daemon sits outside of.
- [`security-model.md`](security-model.md) — attestation, trust boundaries, the distinct-UID boundary, and the trust surface conversation memory accepts.
- [`dekopon-brokerd` contract](../crates/dekopon-brokerd/README.md#boundaries) — the broker contract the gateway proposes into.
- [`dekopon-agent`](../crates/dekopon-agent/README.md) — the shared session layer.
- [`inference.md`](inference.md) — request types and wire JSON, prompt-cache retention caveats, and the chat memory contract.
- [`observability.md`](observability.md) — span semantics, payload gating, and what telemetry excludes.

## Isolated model authentication

`dekopond auth chatgpt {login,status,logout,export}` runs before gateway configuration,
telemetry, transports, or runtime creation. It uses only Dekopon's isolated model credential;
ordinary serving requires `--config PATH`. See [`cli.md`](cli.md) for auth-only flags, output, exit codes and both export guards.
