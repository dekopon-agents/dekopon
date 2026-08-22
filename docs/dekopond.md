# `dekopond` — the chat gateway and agent daemon

`dekopond` is the unprivileged half of the deployment boundary in [`design.md`](design.md): it connects to chat services, waits for a wakeup, routes each authenticated message to a named agent from the catalog, runs one bounded model session with the sandboxed shell and safe on-demand meta tools, and replies with the answer.

It holds chat bot credentials and model credentials — the things it needs to hear a question and to ask a model. It never holds a provider credential, a policy, or an authorization. Every effect a session drives is submitted to `dekopon-brokerd` as an on-behalf-of proposal naming the sender's canonical subject, and the broker alone maps that subject to a principal, decides what it may do, resolves credentials, and executes it.

**Status: Current.** Chat-transport wakeups, including a first text-only Meta WhatsApp Cloud API
webhook, chat-scoped attested routing, bounded sessions, persistent conversations, truthful
transport-acceptance receipts, and optional broker-owned durable chat memory are implemented and
tested. A route is `oneShot` unless configured otherwise; durable
memory is a separate broker/agent opt-in and never changes that default into automatic replay. A
dedicated gateway UID remains **committed direction**.

Its dependency set excludes `dekopon-broker`, `dekopon-broker-host`, `dekopon-http-host`, and `dekopon-brokerd`, and CI rejects any of them appearing in the gateway's normal dependency tree — the same discipline already applied to `dekopon-run`.

[`../examples/pr-summarizer-linter/`](../examples/pr-summarizer-linter/README.md) is the complete
worked deployment: a Slack DM from an owner-mapped sender, six narrow `gh` capabilities, a
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
  socketPath: /path/to/broker.sock            # default: dekopon-run's documented discovery order
  serverUid: 501                              # default: the daemon's own effective UID
  maxFrameBytes: 2097152                      # default: the protocol's own bound
  ioTimeoutMs: 30000

transports:
  - name: scientist-slack
    kind: slackSocketMode
    appTokenEnv: DEKOPOND_SLACK_APP_TOKEN     # environment variable NAMES only
    botTokenEnv: DEKOPOND_SLACK_BOT_TOKEN
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
    apiKeyEnv: OPENAI_API_KEY                 # optional
    timeoutMs: 120000
    classes: [reasoning, analysis]
  - name: subscription
    kind: chatgptSubscription
    model: gpt-5-codex
    authFile: /path/to/chatgpt-auth.json      # optional; defaults to Dekopon's own credential file
                                              # must be in a writable directory: refreshing rewrites it
    timeoutMs: 120000
    classes: [reasoning]
    modalities: [image]                       # optional; default none. This is image INPUT only.

imageGenerators:                              # optional; absent means no route can create images
  - name: openai-images
    kind: openaiImages                        # fixed public OpenAI Images endpoint
    model: gpt-image-1
    apiKeyEnv: OPENAI_IMAGE_API_KEY            # required environment variable NAME, never value
    timeoutMs: 120000

routes:                                       # first match wins, so order these deliberately
  - transport: scientist-slack
    match: { kind: channel, channel: c0123abc }
    agent: incident-responder                 # one named channel, its own agent
  - transport: scientist-slack
    match: { kind: channel }                  # any other channel the bot is invited to
    agent: pr-summarizer-linter
  - transport: community-discord
    match: { kind: channel }                  # Discord channels and native thread channels
    agent: pr-summarizer-linter
  - transport: scientist-slack
    match: { kind: directMessage }
    agent: pr-summarizer-linter
    model: local-qwen                         # optional; else the first model offering the agent's modelClass
    imageGenerator: openai-images              # optional; adds one bounded generate_image attempt
    limits: { maxSteps: 8, maxCapabilityCalls: 16 }
    conversation:                             # optional; default { mode: oneShot }
      mode: persistent                        # oneShot | persistent
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
  telemetryPayloads: false
```

The `conversation:` block is tagged on `mode`, and both halves are strict: an unknown mode and a window bound written next to `mode: oneShot` are equally decode failures. That is deliberate rather than incidental. A bound that can never take effect is far more likely a mode typo than an intention, and a decoder that ignored it would leave a configuration file claiming a memory the daemon does not have.

### No secrets in this file

Transports, chat models, and image generators name **environment variables**, never values, following the precedent `dekopon-telemetry` set for OTLP ingest credentials. A variable name is validated as a name (`[A-Za-z_][A-Za-z0-9_]*`), so pasting a token where a variable name belongs is a startup failure rather than a token sitting in plain text while the daemon reports a missing credential. Missing required variables are reported at startup **by variable name and never by value**. A variable exported with a blank value is refused the same way: an empty app secret is an HMAC key anyone can compute, and an empty bearer token is still sent as a header, so presence has to mean a credential rather than an export.

### Startup fails closed

A gateway that starts and then refuses everything is worse than one that does not start. These are all startup failures:

- a route naming an agent the catalog does not contain, or one the catalog disables;
- an agent with no resolvable model — no `model` override and no configured model offering its `modelClass`, or no `modelClass` at all;
- duplicate transport names, duplicate model names, a route naming an unknown transport or an unknown model;
- a zero step budget, a zero capability budget, or zero concurrency;
- a transport endpoint override that is neither its pinned production origin (Slack, Discord, Telegram, or the Meta Graph API) nor a literal loopback `http://` URL. Literal means `127.0.0.1` or `::1`: the name `localhost` is resolved by whatever the host's resolver says today, which is not the same promise;
- a `channel` written beside `kind: directMessage`. The field belongs to the other kind, and a decoder that shrugged at it would leave an operator convinced they had scoped a route to one channel while it claimed every direct message on the transport;
- a missing or blank chat or named image-generator credential environment variable;
- a route naming an image generator on a text-only transport, which today means `whatsappCloudApi`;
- an unknown Slack experience, activity mode/fallback, or field inside those strict blocks; an off
  Slack activity with a reaction fallback, or classic native activity with no reaction fallback,
  is also refused because the configured fallback could never take effect;
- an unreachable broker. `dekopond` makes one `capabilities()` call on the configured socket before connecting any transport and logs the capability count as `gateway_broker_ready`.

The `conversation:` block adds three more:

- a `persistent` route with a zero idle timeout, a zero turn window, or a zero byte window — the same rule a zero step budget already follows, because a bound of zero is a bound nobody meant to write;
- an idle timeout or a window bound on a `oneShot` route. The setting can never take effect there, and a setting that can never take effect is far more likely a mode typo than an intention;
- a zero `sessions.maxConversations`, which would make every history immediately evictable and turn a persistent route into an expensive one-shot one.

## Agent configuration self-inspection

Every authorized session is offered `inspect_agent_config`. When someone asks “what is this
agent's configuration?”, the model can call it and receive one bounded JSON snapshot designed to
render as concise Markdown tables:

- agent identifier, description, and catalog `modelClass`;
- the exact catalog `instructions` supplied as this session's system prompt;
- route step/capability limits and one-shot or persistent conversation bounds; and
- the capability metadata in this sender's fresh `capabilitiesForChat(subject, agent, scope)` result:
  identifier, selected provider, description, effect, risk, and idempotency.

That last section is an **effective Cedar view**, not Cedar source. Raw policy, policy IDs and
digests, denied or merely declared capabilities, execution constraints, credential bindings,
principal/subject/channel/transport identifiers, model endpoints and auth paths, broker paths, and
all credential names and values are absent. The gateway never receives provider credentials or raw
policy, and the typed view has no field for the chat/model credentials it does hold. Each serialized
result has a 128 KiB hard ceiling. Calls are repeatable under the prompt loop's shared per-turn tool
call and model-step bounds; there is no inspection-specific call limit. An oversized view produces
one fixed content-free diagnostic instead of a partial configuration.

Inspection happens only after the ordinary authorization gate, makes no broker invocation, spends
no capability-call budget, grants nothing, and creates no durable broker audit record. It does make
standing instructions visible to any sender authorized to use that agent. Those instructions were
already model input and must never contain credentials; operators should not treat a system prompt
as a secret from its users.

## Generated images

Image input and image output are deliberately separate. `modalities: [image]` says a chat model may
be shown an inbound screenshot; it does not imply that Chat Completions or the private
ChatGPT/Codex subscription endpoint can draw. Outbound generation exists only when a route names one
entry from `imageGenerators`, and startup fails if the name or its credential cannot be resolved.

That route's authorized sessions receive `generate_image({prompt})`. The prompt is model-authored
and bounded to 4 KiB. The generator endpoint, model credential, filename, PNG media type, and chat
destination are all owner/gateway-controlled. One session may make one attempt and retain one
signature-validated PNG up to 8 MiB. A failed attempt is still spent because the model provider may
have billed it. The bytes never become prompt text, a tool result, telemetry, conversation history,
durable memory, provider output, broker protocol, evidence, or audit.

Delivery uses each service's native upload path: Slack's three-step external file flow, Discord
multipart Create Message, Telegram multipart `sendPhoto`, and an omitted-when-empty base64 `images`
array on the local socket. WhatsApp has no path here — the Cloud API transport is text-only, and
sending an image through it would need Meta's separate media upload — so a route that names an image
generator on a `whatsappCloudApi` transport is a startup failure. Discovering that at reply time
would mean paying a model for a PNG and then dropping it. `DeliveryReceipt` covers the complete
text/image reply. If Telegram or a
split Discord reply accepts only part, the session is `reply-failed` and performs no durable record.
Persistent history remembers only final text; editing or referring to prior pixels requires a fresh
generation.

Slack installations need `files:write` in addition to the existing reply/read scopes. Discord bots
need **Attach Files** in addition to View/Send/Read History/Send in Threads. Telegram needs no
additional bot permission.

## Transports

### Slack Socket Mode

An app-level token opens `apps.connections.open`, which returns a `wss://` URL; a bot token answers through `chat.postMessage` or Slack's external file-upload flow for a generated PNG. No public HTTP endpoint is needed, which is why Socket Mode rather than a public Events API request URL. [`../examples/slack/`](../examples/slack/README.md) has separate classic/free and paid/admin-enabled Agent manifests, plus the token and identity-mapping walkthrough.

`experience` controls Slack's conversation model and is never inferred from a cosmetic API result:

- `classic` (default) retains top-level DM replies and one whole-DM conversation. With native
  activity and `classicFallback: reaction`, the gateway adds its fixed `:tangerine:` reaction to
  the inbound message and removes only a reaction that generation successfully added.
- `agent` makes DMs thread-scoped like Slack Agent sessions and enables authorization-fed channel
  thread continuation. After fresh broker authorization the gateway calls
  `agents.sessions.setStatus(processing)` once; Slack owns the standard Working UI and one-hour
  processing timeout, so the gateway does not waste rate limit on a heartbeat. After a reply or
  deliberate no-reply completion it asynchronously returns the session to `active`; no-reply means
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
boundary. Unknown, duplicate, and other-user Stop events are ignored.

The Agent manifest also subscribes to `message.channels` and `message.groups`, requiring
`channels:history` and `groups:history`, so the transport can hear follow-ups that contain no new
mention. This is deliberately **owned-thread continuation**, not ambient activation:

- an explicitly addressed channel message proposes an exact authenticated
  `(workspace, channel, root thread, sender)` claim;
- only a fresh non-empty broker capability surface installs or refreshes that claim;
- a later unmentioned event must match every coordinate and is still freshly authorized before
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
- Messages carrying `bot_id` and messages from the bot's own user identifier are dropped. Both checks matter: another app's post carries `bot_id`, and this app's own post arrives with the bot's user identifier and no `bot_id` at all.
- A subtyped message is dropped unless its subtype is `file_share`, `me_message`, or `thread_broadcast`. Most subtypes are events *about* a message — an edit, a deletion, a channel join — and answering one would answer a question twice or answer nobody. Those three are a person making a new request. `file_share` is the one worth naming: Slack stamps it on any message carrying an upload, so while every subtype was dropped, asking a question with a screenshot attached produced no answer at all. The list is an allowlist, so a subtype Slack introduces later is dropped until someone decides it is a request.
- A message's attachments become **chat assets**, described in the prompt and fetched only on demand. See [Chat assets](#chat-assets) below. The transport reports what arrived and nothing more: names and media types come from the event, so they are sender-controlled and untrusted exactly like the message text. An upload posted with no comment is still a request — the reference note is then the whole message. A message with neither text nor a file is not a request and is dropped.
- `channel_type: im` is a direct message; anything else is a channel.
- Subject: `slack.<team>.<user>`, lowercased.
- A channel answer joins the thread it was asked in, starting one on the inbound message when there is none. A classic direct message has no thread to join; an Agent direct message is intentionally rooted at `thread_ts = event.thread_ts || event.ts`, and that root also scopes admission, history, status, Stop, and owned continuation.
- An answer is posted in a Block Kit [`markdown` block](https://docs.slack.dev/reference/block-kit/blocks/markdown-block/), which carries the model's CommonMark unchanged and lets Slack render it. The `text` field is mrkdwn — a proprietary syntax where bold is `*one asterisk*` and a link is `<url|label>` — so an answer posted through it alone arrives with `**bold**` as four literal asterisks, and tables and task lists cannot be expressed in it at all. Translating in this process would be a second translation of what Slack is about to translate, so the gateway does none: the block gets the answer verbatim. `text` is still sent as the notification fallback, the one place blocks do not render. The block caps a payload at 12,000 characters, which the 8 KiB outbound bound already sits under.

### Discord Gateway

Discord Gateway v10 is another outbound WebSocket transport. The daemon discovers the service URL through authenticated `GET /api/v10/gateway/bot`, requests only the non-privileged `GUILD_MESSAGES` and `DIRECT_MESSAGES` intents, and identifies after Hello. It jitters the first heartbeat, requires each heartbeat ACK, tracks dispatch sequence, resumes a live session after reconnect, honors Invalid Session and identify/session-start limits, and treats Discord's fatal close codes as terminal transport failures. No public endpoint or privileged Message Content intent is required: Discord exposes content and attachments in DMs and in guild messages whose structured `mentions` array names the bot, and those are the only messages that may wake a session.

- Bots, webhooks, the bot's own posts, and message types other than ordinary messages and replies are dropped.
- Absence of `guild_id` is a direct message. A guild message is a channel message and must name the bot in its structured mentions. Subject: `discord.<user id>`; Discord user snowflakes are global, so a guild is not part of the canonical subject.
- A Discord thread is itself a channel. Its channel ID is the route key, conversation identity, and reply destination. A catch-all channel route covers transient threads; a route naming only a parent channel does not automatically claim its thread IDs.
- Replies use `POST /api/v10/channels/{channel}/messages`. A generated PNG is one multipart attachment on the first post; the first guild post references the incoming message with `fail_if_not_exists: false`; every post disables parsed/reply mentions, so model-authored text cannot ping a user, role, or `@everyone`. Discord's 2,000-character ceiling is handled by lossless multi-message splitting, with Markdown left unchanged. Failure after an accepted image/first chunk is partial delivery rather than a complete receipt.
- With `activity.mode: native`, an authorized session immediately triggers `POST /channels/{id}/typing` and renews around every eight seconds, inside Discord's ten-second native lease. Typing has no explicit clear; sealing stops renewal and the final message clears it sooner. Calls use a short deadline, honor a `429` cooldown, never take the final-message REST lock, and cannot fail the answer.

[`../examples/discord/`](../examples/discord/README.md) is the bot installation, permission, token, route, and identity-mapping walkthrough.

## Chat assets

A screenshot is part of the message that carried it. Slack, Discord, and Telegram deliver it by reference rather than by value, so the gateway resolves that reference in order to hear the whole request. Slack and Telegram require the bot token they already terminate here; Discord CDN downloads do not receive it. This grants no policy, no provider credential, and no way to write anything.

What it does not do is read every file that arrives. Bytes cost tokens on every turn they appear in, and most turns do not need them. So each attachment is *named* in the prompt and fetched only if the model decides the answer depends on it:

```text
what does this say?

[gateway: the sender attached
  Chat Asset #1 — screenshot.png (image/png, 214 KB)
  recording.mov (video/quicktime, 41.3 MB) — not a type the gateway can show you
  Call fetch_chat_asset with the number to look at one.]
```

The model then calls `fetch_chat_asset(1)`. Because a tool result cannot carry an image — Chat Completions types a `tool` message's content as a string, and the Responses API types `function_call_output.output` the same way — the answer arrives as two messages: the tool result says the asset follows, and a `user` message carries the bytes. That shape is the only one both wire formats accept.

- **Numbering is per conversation and monotonic.** `Chat Asset #5` means one file for as long as the reference line naming it is being replayed, which is what lets a follow-up three turns later resolve. Numbers are assigned by the gateway rather than by a transport, so two transports cannot collide inside one conversation.
- **Every prompt names the whole inventory**, not only what the newest message brought, with the new ones marked. A reference line is the only way a model learns a number exists, and one that lived solely in the turn that introduced it became unreachable as soon as ordinary chatter pushed that turn out of the replayed history window — while the store still held the file for another hour. The model would then answer that it had never been sent the file, which was true of the prompt it could see and false of the conversation.
- **The reference line is what history remembers, not the bytes.** It is a few dozen bytes, so it replays inside the conversation byte budget instead of evicting real conversation the way a base64 screenshot would.
- **A file that cannot be shown is still named.** A media type outside the allowlist, a model with no image modality, or a file Slack withholds entirely all produce a line saying so. Ignoring it is what made the gateway deny a screenshot that plainly existed.
- **Only the media types a model can actually accept are offered.** Images: `image/png`, `image/jpeg`, `image/webp`, `image/gif`. Documents: PDF, plain text, Markdown, CSV, HTML, XML, JSON, RTF, and the Word, PowerPoint, and Excel formats. A chat service imposes no allowlist on uploads at all — a 700 MB screen recording is a legal attachment — so the narrow end of that intersection is the one worth enforcing. A spreadsheet is parsed to its first thousand rows per sheet, which is worth knowing before concluding a model ignored the bottom of one.
- **A route's model has to opt in to images.** `modalities: [image]` on a model entry; the default is text only, because an OpenAI-compatible endpoint is very often a small local model that will either error or invent an answer when handed an image. Documents need no modality: a PDF is a parsed attachment to every endpoint that accepts one at all, so gating it on vision would refuse it to a model perfectly able to read it.
- **Bounds.** 8 MiB per attachment, enforced while the response streams rather than after it, because a reported size is sender-influenced and a chunked response need not declare a length. Four fetches per session. Thirty-two attachments addressable per conversation, evicted oldest-first. Every one of these refuses in a sentence the model reads and can answer around, never by failing the session.
- **Redirects.** The HTTP client refuses redirects globally so a bearer token is never forwarded by policy. Slack's `url_private_download` genuinely redirects to its own file host, so that transport follows exactly one hop, only to a host it recognises by comparing the host itself rather than a URL prefix, and re-attaches the token by hand.
- **Resolving a reference differs by transport.** Slack carries a private download URL on the event itself. Discord carries a signed CDN URL plus the source channel/message/attachment IDs; the CDN request carries no token, and an expired 401/403/404 URL is refreshed by re-reading that exact message through pinned Discord REST before retrying the same attachment ID. Telegram carries only a `file_id`, so a fetch is two calls: `getFile` turns the handle into a path valid for about an hour, and the bytes live under `/file/bot<token>/<path>` rather than the method prefix. The round trip happens at fetch time, which is also when that path is freshest.
- **Discord specifics.** Photos and arbitrary files share the attachment object, retaining their sender-controlled filename, optional media type, and reported size. Production downloads accept only HTTPS `cdn.discordapp.com` or `media.discordapp.net` URLs, reject credentials and redirects, and enforce the byte ceiling while streaming.
- **Telegram specifics.** A photo arrives as the same image at several sizes and the largest is the one used — a model asked to read text in a screenshot cannot read a 90-pixel-wide copy. Telegram reports no media type for a photo, so `image/jpeg` is inferred, which is what the Bot API re-encodes every photo to; a file sent as a *document* keeps its own bytes, name, and declared type. Words on an upload arrive in `caption` rather than `text`.

### Telegram long polling

`getUpdates?timeout=50&offset=N` blocks server-side and returns as soon as anything arrives, so waiting costs one idle connection. **The poll is the wakeup and advancing `offset` is the acknowledgment** — there is no separate ack call and therefore no ack-before-work problem. The offset advances past every update, including ones the daemon chose not to route, or a filtered bot message would return forever.

Messages from bots are dropped. A private chat is a direct message; a group is a channel. Subject: `telegram.<user id>`. A forum `message_thread_id` creates the distinct canonical conversation `<chat>:topic:<id>`, and the reply carries that same thread ID; plain messages retain the chat itself as their conversation.

Telegram's optional `message_thread_id` is preserved consistently in admission, conversation
identity, replies, generated-photo uploads, and activity, so a forum-topic pulse cannot appear in
another topic. Generated PNGs use `sendPhoto`; text up to Telegram's 1,024-unit caption ceiling is
accepted with the image, while longer text follows as losslessly split `sendMessage` calls; a
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
that running process are acknowledged without another session. This is deliberately not durable
exactly-once: restart forgets claims, and a crash after 200 but before queue drain may lose the
accepted work. Queue saturation returns 503 and rolls back new claims so Meta can redeliver.

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
a flood is still one line a minute. A failed `accept()` is classified rather than treated as the end
of the listener, because nothing restarts a transport reader: a dead connection is debug-level and
ignored, descriptor or buffer exhaustion is warned and retried after a short pause, and only a
listening socket that can never serve again stops the loop with
`gateway_whatsapp_listener_stopped`.

Media, templates, interactive messages, reactions, activity, status processing, business-management
APIs, embedded signup, webhook multiplexing, and daemon TLS termination are non-goals. See
[`../examples/whatsapp/`](../examples/whatsapp/README.md) for placeholder-only setup.

### Local development transport

An owner-only (`0600`) Unix socket under a private parent directory, with `dekopon-brokerd`'s socket hygiene: the parent must be an owner-owned directory with no group or world access, an existing socket is replaced only if it is already private and single-link, and the guard removes only the exact inode it created. Line-delimited JSON in, line-delimited JSON out on the same connection:

```console
$ nc -U /path/to/dekopond-dev.sock
{"subject": "tel.16034700182", "channel": "dev", "text": "what changed today?"}
{"reply": "Nothing external. Two read-only capability calls."}
```

Text-only output keeps that exact shape. A generated image adds an `images` array containing the
gateway-owned `filename`, `mediaType`, and base64 `data`; the field is absent otherwise. The local
line can therefore approach the base64 expansion of the 8 MiB decoded bound and remains a
development protocol rather than a compact production transport.

**This transport trusts its local caller to declare a subject.** That is the whole point of it — it exists so a developer can drive a routed session without a Slack workspace — and it is why it is a development tool rather than a production transport. It grants nothing by doing so: the declared subject is still only a claim carried into the broker's `invokeForChat`, and the broker still needs an attestor grant covering that namespace plus an owner-controlled mapping before it resolves to a principal. Its `0600` mode keeps it reachable only by the owner's UID, which is the trust domain the broker socket already lives in.

A declared subject also selects a *history*. A local caller can therefore name a subject some Slack sender created and have that person's compacted exchange replayed into its own prompt. No authority moves — the broker still decides every invocation for itself — but text does, which is a second reason this socket is `0600` and a development tool.

Every local message is a direct message; channel routes are a chat-service concept.

## Routing

First match wins on (transport name, direct message or channel). The channel is optional: `{ kind: channel, channel: c0123abc }` claims that one channel, and `{ kind: channel }` claims **any** channel the bot is in. Unmatched messages are ignored with a debug-level event — bots see ambient traffic, and silence is the correct answer.

Leaving `channel` out exists because naming them does not scale. One route per channel means enumerating service-native identifiers and editing this file again every time somebody creates a channel, and until an operator notices and redeploys, the bot is silent in the new one while appearing to be deployed workspace-wide. An absent `channel` says "wherever I am invited", which is membership the chat service already controls.

**Declaration order is the whole precedence rule.** Routes are consulted top to bottom, so a named-channel route written above a catch-all keeps that channel for itself while the catch-all takes everything else — special handling in `#incidents`, the default everywhere else. Nothing sorts by specificity, deliberately: a hidden ranking is how an operator ends up unable to say which route answered.

A channel initially requires the bot to be addressed: `<@BOT_USER_ID>` on Slack, a structured `mentions[].id` match on Discord, or `@botname` on Telegram. **A route decides which agent answers; an explicit address decides whether a new channel conversation starts**, and widening the first leaves the second exactly where it stood. Discord and Telegram retain that rule on every message. Slack classic does too. Slack Agent has one bounded exception: after an explicitly addressed message is freshly authorized, the same authenticated sender may continue without another mention inside that exact owned root thread. Every continuation is authorized again and may decline to post; all non-owned channel history is dropped before routing. The Agent manifest therefore receives ambient public/private channel events, while the classic manifest remains mention-only.

### Being available in a channel is not authority

A route matching every channel widens no authority whatsoever, and an operator reading "available in all channels" must not read it as "available to everyone". Every session still opens an attested broker leg naming the sender's canonical subject; the broker still maps that subject to a principal, still requires policy permitting `agent.prompt` for it, and still refuses an unmapped sender before any model call is made. A catch-all route changes *where* the mapped people can reach the bot. It does not change who they are, and somebody the owner never mapped gets the same refusal in a catch-all channel that they would have got in a named one.

Nor do two people in one channel share a conversation. History is keyed on `(transport, the conversation identity, the sender's canonical subject)`, so the bot remembers each of them separately and remembers nothing of what it told the other — see [the key includes the sender](#the-key-includes-the-sender), which matters most in exactly the shared channel a catch-all makes easy to reach.

## Sessions

Each routed message runs one session. On a `oneShot` route — the default, and every route in a configuration that never writes a `conversation:` block — that session is entirely independent, and the `persistent` clauses in steps 4 and 5 are the whole difference the other mode makes:

1. **Admission.** A process-wide semaphore bounds what the daemon costs at once, and a per-`(transport, channel, thread)` in-flight set stops one conversation from queueing work on itself — what a person does when a bot seems slow and they send the same thing again. A rejected message gets `I'm busy — try again shortly.` when `replyOnBusy` is set, and silence otherwise.
2. **Authorization.** The session opens an attested broker leg with `capabilitiesForChat(subject, agent, scope)`. If the answer is empty — or the broker refuses, because the attestation was not honored or because policy does not permit this principal to drive this agent — the sender gets `You're not authorized to use this agent.` and **no model call or activity write is made**. That is the cheapest possible refusal, and one the message text cannot argue with.
3. **Activity.** When the transport opted in, one session-owned generation starts immediately after the fresh grant. The service renders it; the model supplies no target, status text, frame, emoji, or timing. The coordinator permits one request at a time, refreshes expiring signals, seals synchronously before terminal delivery, and queues cleanup afterwards so cosmetic I/O never delays the reply or holds admission. Two consecutive failures stop renewal for that generation; permanent Slack installation failures additionally trip a transport-wide fallback breaker.
4. **Execution.** On a `persistent` route the session first looks up its conversation, keyed on `(transport, the conversation identity, the sender's canonical subject)`. An entry idle past the route's timeout, or built under a granted capability set that differs from the one this message's leg just reported, is dropped rather than used; whatever survives is seeded into the prompt ahead of the new message as compacted `(question, answer)` pairs, oldest dropped first until the window's turn and byte bounds both hold. The lookup happens *after* step 2 because the grant comparison needs a fresh grant to compare against. Then, as before: the model client is built from the route's model, the shell runtime is given the attested leg as its only capability dispatch, the credential-free `inspect_agent_config` view is built from the same fresh leg, an explicitly named image generator adds its one-attempt meta tool, and the prompt loop runs on a blocking task with the agent's `instructions` as the system prompt. Instructions are supplied fresh on every message and never stored, so editing an agent's standing orders takes effect on the next message without rewriting a single remembered conversation. Shell bounds are `dekopon-shell`'s defaults except `maxCapabilityCalls`, which comes from the route. Every model request the session then makes declares a [prompt cache key](#the-prompt-cache-key) — the conversation's on a `persistent` route, the route's on a `oneShot` one.
5. **Answer, deliberate silence, and optional durable recording.** A required session's final bounded text and optional generated PNG go back to chat. An inherited Slack Agent continuation may instead call `decline_chat_reply` before capability work, which commits its user-only in-process turn, cleans up activity, and sends no reply request. On failure the sender gets one fixed line, `The agent could not complete this request.` — a `PromptError` can carry model-chosen text, a provider message, or a transport diagnostic, and chat is the last place any of those belong. The operator reads the category from telemetry. A `persistent` route writes only the textual exchange back as one more in-process remembered turn, trims the window, and restarts the idle clock. **The fixed failure line and generated bytes are never stored.** A declined or failed model session records its question with nothing in the in-process answer's place, which is truthful and is what makes a later follow-up answerable; a session refused at step 2 records nothing at all. When optional durable memory is authorized, only textual turns whose complete text/image reply received transport acceptance are recorded, exactly once and without automatic retry. Declines, failure replies, partial delivery, reply errors, and sessions won by an authenticated Stop event are never durably recorded.

Text is bounded in both directions: inbound to 16 KiB keeping the head (a chat message states its request first), outbound to 8 KiB keeping head and tail (an answer's conclusion is usually its last line). Both truncations say so in the text.

At shutdown, transport readers are aborted and in-flight sessions get `shutdownGraceMs` to finish — a model call is already paid for, and abandoning it means a person watching a chat window never hears back. If the grace expires, dropping each async owner marks its synchronous prompt loop cancelled before aborting the wrapper, so no later model turn or capability call starts. A model request or provider effect already in progress remains non-rollbackable and may finish after the async owner is gone.

## Conversations

**Status: current.** History is a trust surface rather than a feature flag, and [`security-model.md`](security-model.md#conversation-memory-as-a-trust-surface) states the surface it accepts.

A `persistent` route keeps a bounded history per sender and replays it into the next prompt, so a follow-up question can say "and the second one?" and be answered. `oneShot` is the default and is exactly the behavior every route had before this existed.

### The history lives in the gateway

This subsection describes the automatic replay window, not the separate on-demand durable provider. The replay window lives in the daemon's memory and nowhere else. It is never written to disk, never sent to the broker, and is lost on restart: `dekopond` comes back with every conversation forgotten, and a person who asks a follow-up across a restart gets a first-message answer.

That placement is the whole point rather than an implementation shortcut. The broker holds provider credentials and a deliberately metadata-only audit chain in which a provider's output survives only as a digest. Conversation text there would put the most sensitive content in the system inside the most privileged process, sitting beside a record built specifically not to contain it. The gateway already handles this text — it read the message and it wrote the answer — so keeping the history there adds no new reader.

### The key includes the sender

`(transport, the conversation identity, the sender's canonical subject)`. The subject is in the key because the alternative replays one person's exchange into another person's prompt, and in a shared channel that is not a hypothetical.

The conversation identity is the transport-derived one, not `(channel, thread)`. Slack omits `thread_ts` on the message that *starts* a thread and sends it on every reply inside one, while the bot answers that first message in a thread rooted at it — so anything keyed on the raw thread identifier files the opening question apart from every reply to it and orphans the first turn of every threaded conversation. Each transport derives the identity because only a transport holds the service-native pieces it takes.

This is deliberately *not* the admission key from step 1, which is `(transport, channel, thread)` and has no subject in it. The two keys answer different questions. Serialization asks "is this bot already busy on this thread", and two people talking at once in one thread are one thing to serialize. History asks "whose exchange was this", and the same two people are two histories.

Because the two keys are different, admission does not serialize history access: a sender who replies in-thread to their own message before the bot answers admits twice under two keys and runs two sessions against one history. The store handles that itself. Both sessions read the same seed, and each *appends* its own exchange rather than writing back a whole window, so neither can erase the other's answer.

The visible consequence of the subject in the key is that in a channel the bot remembers each person separately and remembers nothing about what it told somebody else in the same thread, which will occasionally read as forgetfulness. That is the correct trade.

### Prior turns are compacted

A stored turn is `(the user's message, the final answer)`, or the message alone when the session failed or deliberately declined an optional reply. Every intermediate step — the model's tool calls, the scripts it authored, and their output — is dropped at write-back and never replayed.

The number that forces this: one script's combined output can reach 256 KiB, which is `dekopon-shell`'s default `max_output_bytes` in [`../crates/dekopon-shell/src/limits.rs`](../crates/dekopon-shell/src/limits.rs). Replaying full transcripts would let a single earlier turn cost more than the entire window budget, and it would do so most on exactly the sessions that did the most work.

The loss is real and worth naming: the model cannot re-read a command it ran three messages ago, only what it said about it. If it summarized badly, the bad summary is what persists. A `persistent` route buys continuity of conversation, not continuity of evidence — the broker's audit chain is where what actually happened is recorded.

### The bounds

| Setting | Where | Bounds |
|---|---|---|
| `mode` | route | `oneShot` (default) or `persistent` |
| `idleTimeoutMs` | route | How long an untouched conversation survives; default 900000 |
| `maxTurns` | route | Exchanges the window replays; default 12 |
| `maxBytes` | route | Bytes the window replays; default 65536 |
| `maxConversations` | `sessions:` | Conversations the process tracks at once; default 1024 |

`maxTurns` and `maxBytes` both apply, oldest turns dropping first until both hold. Two bounds because they fail differently: twelve one-line exchanges and twelve paragraph-length ones are the same number of turns and very different prompts.

`maxConversations` lives under `sessions:` rather than in the route block because it is a property of the process, not of a route, and `sessions:` is already where "what this daemon costs at once" is configured. It is a memory bound and not an admission bound: reaching it evicts the least recently used conversation rather than refusing a message, because a person talking now matters more than one who stopped an hour ago. An eviction is logged as `gateway_conversation_evicted` with a reason, so a ceiling set too low is visible as churn instead of as a bot that intermittently forgets.

Neither eviction runs on a timer. There is no sweeper task and no shutdown hook: the idle timeout is checked by the lookup that would otherwise have used the entry, and the ceiling is enforced by the write that would otherwise have exceeded it. History is process memory and dies with the process, which is a documented property of where it lives rather than a gap in how it is cleaned up.

### Authorization is never cached

Every message opens a fresh attested broker leg and gets a fresh `capabilitiesForChat` answer, exactly as step 2 already describes. Persistence changes nothing here: no grant is remembered, no decision is carried forward, and history is prompt text rather than authorization input.

The granted capability set is additionally **stored with the conversation** and compared on every message. Any difference drops the history and starts a fresh conversation; an empty grant removes the entry outright. The reason is narrow and specific: output a session fetched under a broad grant is sitting in the history, and if the owner then narrows what that subject may reach, an unchecked entry would keep replaying it after the capability that produced it was taken away. Invalidation costs a cache miss on the first message after any policy change, which is the right price — a narrowed grant is precisely when replaying old output is wrong.

Its reach is exactly the granted capability *identifiers*, which is less than it sounds like. A policy edit that keeps the same capability list but tightens its owner-authored constraint set — a narrower allowed host, a smaller output ceiling, a different credential — produces an identical grant set and does not drop the history. Text fetched under the older constraints stays in the prompt until the window or the idle timeout removes it.

### Why fifteen minutes

The idle-timeout default is pulled in two directions and deliberately loses one of them.

The ChatGPT subscription endpoint publishes no prompt-cache lifetime. Public OpenAI API policies vary by model and retention mode, so tuning a user-visible memory timeout to one guessed provider TTL would couple two mechanisms that do not share a contract. Human conversational memory runs on a longer clock: someone who asks a follow-up after a meeting expects the bot to know what they were discussing, and a bot that forgot after a brief lull is the failure people report.

The default is 15 minutes, which resolves toward the person because the user-visible point of this feature is memory, not a cache hit. **The cost control is the window, not the cache:** `maxTurns` and `maxBytes` bound what any one message pays no matter how long its conversation has been alive. [`inference.md`](inference.md#provider-retention-what-can-be-said) records the public API comparison, the undocumented subscription boundary, and why keeping a process alive does not pin a provider cache.

### The prompt cache key

Every model request carries a `prompt_cache_key`, on both model backends. **It is a routing hint and never an access-control boundary.** It tells the provider which requests are likely to share a leading prefix so they can land on one cache; it authorizes nothing, isolates nothing, and hides nothing. The request still carries the whole conversation either way, and a backend that ignores the field returns a byte-identical answer at full price. An operator must not read two requests sharing a key as two requests sharing anything else: authorization is still asked per message, on a fresh attested leg, as it always was.

**It carries nothing about the sender.** The key is an opaque identifier *minted* when the thing it names is created — not the canonical subject, not a hash of one, not a salted one. A canonical subject can be a phone number, so sending it would hand a model provider the sender's identity in exchange for routing that happens anyway; hashing it does not fix that, because a hash of a stable subject is a stable pseudonym, which is exactly what this daemon declines to put in its *own* telemetry when `telemetryPayloads` is off. A configured salt is worse again: a new secret to manage whose only purchase is a pseudonym that survives restarts.

Where it comes from, and how long it lives:

| Route mode | Key names | Minted | Rotates when |
|---|---|---|---|
| `persistent` | one conversation, `(transport, conversation identity, subject)` | with the conversation entry | the entry is evicted — idle, capacity, or a changed grant — or the process restarts |
| `oneShot` | one bound route | once, at startup, when routes bind | the process restarts |

Rotation is the property that keeps it from becoming a durable identifier for a person, and it is also just correct: an evicted conversation rebuilds a prompt that shares no prefix with the one it replaced, so continuing to name the old lane would be a guaranteed miss.

A `oneShot` route's key is shared by **every sender that route answers**, which reads alarming and is not. That route's shared prefix is the agent's `instructions` and the tool definitions, and then this one message: the shared part is identical for everyone the route serves and contains nothing about any of them, so pointing the route's traffic at one lane shares what was already common property. Nothing sender-specific can hit — a different sender's message diverges from the first token that differs, and a cache key is a hint about a shared *prefix*, not a handle on somebody's answer. The alternative, a fresh key per message, would name a lane holding exactly one request and give up the only caching a stateless route can have.

### What the cache key is actually worth

Less than it sounds like, and worth having anyway. Set expectations from the provider's own behavior rather than from the key:

- **Retention belongs to the provider, and the subscription lifetime is undocumented.** The key can improve a burst — especially append-only tool-loop turns and a follow-up whose prefix still matches — but it promises no standing discount. Read the reported token counts instead of inferring a hit from elapsed time or a live process.
- **A window trim costs a miss by construction.** When `maxTurns` or `maxBytes` drops the oldest exchange, the front of the request is rewritten and the cached prefix ends at the first changed token. That is an argument for a *generous* window that trims rarely rather than a tight one that trims constantly, which is the opposite of how a size bound is usually tuned. The bound is still the cost control; it is just not free to hit.
- **Changing an agent's `instructions` invalidates everything.** They sit ahead of every message, and on the ChatGPT path they are hoisted into a separate top-level field, so an edit — including switching between having them and not — rewrites the front of every request on that route.

**Read `usage.cached_input_tokens` to find out whether any of this is working.** It is already plumbed end to end: whatever the provider reports lands on the `prompt.model_turn` span and on the `accounting.model.turn` audit event, alongside `usage.input_tokens`. The ratio between the two on a conversation's second and later turns is the whole answer, and a count the provider did not report is absent rather than zero, so a missing field means "unreported" and never "no cache hits". No new instrumentation is involved in checking.

### What this means for retention

On a `persistent` route, chat text sits in `dekopond`'s memory for at least the idle timeout after somebody stops talking. On the default that is fifteen minutes of a person's question, and the agent's answer, resident in a process that previously kept neither past the reply. **At least**, because eviction is lazy: an abandoned conversation is dropped by the next lookup on its key or by the ceiling displacing it, so with neither happening the bytes stay in the process until it exits. What a timed-out entry can never do is reach a prompt. The daemon writes none of it to disk; the operating system's own paging and core-dump behavior are outside what the daemon controls. Under the single-UID deployment described below, any process under the owner's UID can read that memory — "in memory only" is a durability property, not an isolation one.

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
only then opens one fresh broker client for one `RecordDeliveredTurnForChat`. The recorded user text
is the original bounded sender text, excluding generated attachment reference notes; assistant text
is exactly what the transport accepted. No response, denial, timeout, EOF, partial Discord delivery,
or outcome-unaudited is retried, and none changes the already delivered `answered` outcome.

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
dekopond                subject = ExternalSubject::{slack,discord,telegram}(...) (routing metadata, not authority)
      |                 agent   = the route's catalog agent
      |
      | capabilitiesForChat(subject, agent, scope)  ── empty ⇒ refuse, no model call
      | invokeForChat(proposal, subject, agent, scope)
      v
dekopon-brokerd         attestor grant bounds the namespace
                        identityMappings turn the subject into a principal
                        policy must permit agent.prompt for that principal and agent
                        policy conditioned on context.via decides what it may then reach
                        credentials resolve, the provider executes, audit records it
```

The broker is the sole authority. `dekopond` supplies the subject and never the principal; a refused attestation is an audited denial recorded against the gateway's own peer identity. Driving an agent at all is its own policy statement — `Dekopon::Action::"agent.prompt"` over `Dekopon::Agent::"<name>"` — so a mapped subject the owner never permitted to use this agent is refused before the capability listing is even assembled, and an `invokeForChat` under such a session is the audited denial `agent-denied`. See [`security-model.md`](security-model.md) for the complete attestation contract, and note in particular that **a policy written for direct peers can never authorize an attested context and vice versa** — adding a gateway cannot widen a grant that already existed.

## Informational status reporting

After its ordinary broker capability probe succeeds, the gateway best-effort publishes a bounded normalized inventory for `dekopon-webui`: agent identifier, description, enabled/model-class flags, capability/provider identifiers, and provider permission declarations. It refreshes that static snapshot once a minute so a restarted broker recovers its in-memory view; a truncated report says so explicitly. It deliberately omits standing instructions, labels, policy profile, chat content, subjects, principals, model endpoints, and every credential. The broker accepts it only from a mapped peer carrying an attestor grant.

The shared prompt loop also calls an optional usage observer after every successfully decoded model response, including responses followed by a later tool/session failure and responses whose provider omitted usage. One session accumulates input, cached-input, output, reasoning-output, and total token counts plus an explicit missing count for each field. A bounded background reporter coalesces deltas and gives each send a short deadline. A full queue, broker restart, old protocol, or timeout logs a stable failure category and never delays or changes the answer; normal `accounting.model.turn` telemetry remains the retained accounting path.

Both reports are self-reported informational state held only in broker memory. They reset on broker restart and never participate in identity, `capabilitiesForChat`, Cedar, constraints, credential selection, provider execution, evidence, replay, or durable audit. A compromised gateway can lie to the dashboard and gains no effect authority by doing so.

## Telemetry

Spans follow [`observability.md`](observability.md):

| Span | Fields |
|---|---|
| `gateway.message` | `transport`, `agent`, `outcome` (`answered`, `declined`, `unauthorized`, `busy`, `failed`, `cancelled`, `reply-failed`) |
| `gateway.session` | `agent`, `conversation.turns`, `conversation.bytes`; wraps the broker leg and the model session |

The prompt loop's own spans (`prompt.session`, `prompt.model_turn`, `prompt.script`, `shell.command`) nest under `gateway.session`, and the broker's spans join the same trace through the proposal's `traceparent`.

The metadata-only default carries transport, agent, and outcome and nothing else. Chat text and canonical subject identifiers appear **only** under `telemetryPayloads: true`, as the `gateway.message.received` log event — the same gate `dekopon-run` uses for prompts and script text. Enabling it declares the telemetry sink in scope for the messages this daemon handles.

The prompt cache key is behind that same gate, as `gateway.session.cache_key`, and not on the metadata-only default. It names nobody — that is the whole design — but within one process it still joins one person's turns to each other, and a join key over somebody's conversation is the linkage the default exists to withhold. It rides its own log event rather than joining `gateway.message.received`, so a key and a canonical subject never appear on one line.

Conversations add to both lists, and change the meaning of one field that already exists. `gateway.session` carries `conversation.turns` and `conversation.bytes` — how much history this message replayed, as a count and a byte total and never as text; both are zero on a `oneShot` route and on the first message of any conversation. `gateway_conversation_evicted` is in the lifecycle events below with a reason of `idle`, `capacity`, or `grant-changed`. The second-order effect is the one that catches people: on a seeded session `message.count` counts the replayed window plus this exchange rather than this exchange alone. [`observability.md`](observability.md#what-conversation-history-changes) has the dashboard consequences.

Lifecycle events on stdout as structured JSON: `gateway_broker_ready`, `gateway_transport_connected`, `gateway_started` (transport and route counts), `gateway_session_rejected`, `gateway_session_failed`, `gateway_session_cancelled`, `gateway_session_stop_requested`, `gateway_activity_degraded`, `gateway_conversation_evicted`, `gateway_transport_disconnected`, `gateway_stopped`. Activity-call failures are debug-level `gateway_activity_failed` records. They carry only operation and stable category; degradation carries transport and surface. Neither includes a subject, target identifier, status text, raw service response, or credential. Other failure events likewise carry stable categories, and an eviction carries a reason and nothing about the conversation it forgot. An optional no-reply decision closes `gateway.message` with `outcome=declined`; its `agent.reply.declined` record carries only the model-turn number and no text or thread coordinate.

## The single-UID caveat

In the current deployment `dekopond` and `dekopon-brokerd` run under one UID, and the broker's socket mode `0600` makes that UID one trust domain. An attestor grant therefore adds no authority beyond what any process under the owner's UID already has: every such process can already act as the configured gateway peer.

What the mechanism buys today is attribution and blast-radius shape — subject-level audit, deny-by-default policy conditioned on `via`, and configuration that fails closed on undeclared principals. Namespace scoping and `via` become real isolation only when the gateway runs under its own UID with its own peer identity, and that deployment (along with the socket permissions it needs) remains committed direction. [`security-model.md`](security-model.md) states this in full.

The caveat covers conversation history too. Holding it in gateway memory keeps it off disk and out of the privileged process; it does not keep it away from another process running as the same user.

## Related documents

- [`design.md`](design.md) — the authority model this daemon deliberately sits outside of.
- [`security-model.md`](security-model.md) — attestation, trust boundaries, the single-UID limitation, and the trust surface conversation memory accepts.
- [`broker-http.md`](broker-http.md) — the broker contract the gateway proposes into.
- [`run.md`](run.md) — the one-shot runner that shares the same session layer.
- [`inference.md`](inference.md) — request types and wire JSON, cache retention caveats, current chat memory, and the unexplored long-term-memory boundary.
- [`observability.md`](observability.md) — span semantics, payload gating, and data minimization.
