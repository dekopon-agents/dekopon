# Gateway transports

How `dekopon-gatewayd` talks to each chat service: what a transport admits and drops, how it
replies, its refusals and limits, and the log events that show them. Read
[Shared behaviour](#shared-behaviour), then your transport's section.

- [`gatewayd.md`](gatewayd.md#configuration) owns every key named here and
  [the refusals at load](gatewayd.md#startup-fails-closed); the GitHub webhook is in
  [GitHub webhook transport](gatewayd.md#github-webhook-transport).
- [Routing](gateway-conversations.md#routing) decides which messages need a mention;
  [liveness](gateway-liveness.md#liveness-progress-and-stopping-a-run) covers indicators, progress
  and Stop; [chat assets](gateway-assets.md#chat-assets) covers attachments.
- [Gateway spans](observability.md#gateway-spans) defines every `drop.reason` word and the fields
  of the log events it lists.

## Shared behaviour

- **Recovery.** Every transport reconnects under [connection recovery](gatewayd.md#connection-recovery).
- **Long answers.** An answer is bounded at 8 KiB. Discord, Telegram and WhatsApp split one longer
  than the service's message ceiling into consecutive messages without losing text: at a line
  boundary where one fits, mid-line otherwise. A fenced code block (backtick or tilde, any length,
  up to three spaces of indent; an unterminated fence runs to the end) that would straddle a
  boundary moves whole to the next message when it fits in one. A longer block is closed at the end
  of a message and reopened with the same info string in the next, both fences counted against the
  ceiling.
- **Partial delivery.** A failure after the service accepted an earlier chunk or file is
  `partial-delivery`: no complete receipt and no delivered turn.

## fauxlack (tests only)

The `fauxlack` Cargo feature adds an in-process chat service for Rust gateway tests, with no
credentials, socket or listener. Build a `dekopon_gatewayd::fauxlack::Fauxlack` from one YAML
manifest, then pass it to `run_fauxlack(config_path, shutdown, service, shell_threads)`. `run`,
`check` and the executable have no service and refuse every fauxlack transport, even in a feature
build. Without the feature neither the world nor the adapter compiles; the configuration still
decodes.

```yaml
transports:
  - name: chat
    kind: fauxlack
    server: acme
    bot: udekopon
    indicators: {mode: native, progress: message}
```

- **Manifest.** `servers` declares each server's `id`, `users` (`id`, `name`, optional `bot`),
  `channels` (`id`, `name`, `members`), `directMessages` (`id`, `members`) and `files`. A file has a
  `name` and exactly one of `canvas` (immutable Markdown), `snippet` (plain text) or
  `image: {mime, base64}`. Every level refuses unknown fields; IDs use the shared subject and
  conversation validators, and all conflicts are reported together. The manifest grants nothing:
  broker mappings and grants decide.
- **Delivery.** Member bots receive posts in order over bounded queues; posting waits for room. The
  adapter drops bot-authored posts. Channel roots and their replies share one thread; DMs have no
  thread. A channel post must mention the bot as `<@userId>`.
- **Driver.** Replies and message progress only. Manifest files are fetched from memory; authorized
  outbound attachments appear as files on the bot's answer.
- **Observing.** A posted handle captures its thread observer before delivery, so an immediate
  answer is still seen; clones share the observer's cursor. Waiting returns completed answers only,
  including progress finalized in place; unfinished or deleted progress is absent from transcripts.

The adapter tests gateway processing after projection; each real adapter's own tests cover its
projection.

## Slack Socket Mode

The app-level token opens `apps.connections.open`, which returns a `wss://` URL; the bot token
posts answers with `chat.postMessage` and attachments through Slack's external file upload. No
public endpoint is needed. [`examples/slack/`](../examples/slack/README.md) owns app setup: the
classic and Agent manifests, scopes, tokens and identity mapping. The classic manifest enables
interactivity, which carries a Stop button's `block_actions` press over the same socket; without it
the button renders and the press reaches nothing.

`experience` sets Slack's conversation model. The gateway never infers it or the workspace plan,
and a status refusal never changes it.

| | `classic` (default) | `agent` |
|---|---|---|
| Direct messages | Top-level replies; one conversation per DM | Each root message starts its own threaded conversation, rooted at `event.thread_ts` or else `event.ts`; that root also scopes admission, history, status, Stop and continuation |
| Working indicator | With native indicators and `reaction.working: auto`, the fixed `:tangerine:` reaction on the inbound message; cleanup removes only the reaction this run added | `agents.sessions.setStatus(processing)` once after fresh authorization; back to `active` when the run ends, with or without a reply |
| Stop | A `cancelButton: true` button | Slack's native Stop, sent as `agent_session_stopped` |
| Unmentioned channel messages | Dropped unless a follow ticket or PR link admits them | Owned-thread continuation, below |

**Agent status.** Slack times Working out after an hour, so the gateway sends no heartbeat. A
permanent installation error (`feature_disabled`, `missing_scope`, `not_allowed_token_type`,
`method_deprecated`, `deprecated_endpoint`) disables Agent status for the transport and logs
`gateway_progress_degraded` (`surface = agent-status`); `reaction.working: auto` then falls back to
the reaction, or to nothing without reactions.

**Status text.** An Agent transport may set `indicators.statusText: true` with `progress: off` or
`auto`. When no progress message is being written, the first note from a `progressNotes: true`
route returns the session to `active` and shows the rendered line through
`assistant.threads.setStatus`. For the rest of the run, note, tool, working and keep-alive lines go
there, under the same coalescing and 60-edit budget.

- Slack hides its native Stop while status text shows; configured stop words still work.
- Only keep-alive ticks refresh the text, and Slack drops it after two minutes without one.
- Running out of keep-alives or edits, tripping the status-text breaker, or a permanent error
  (`surface = assistant-status`) restores native Working once.
- At the end the gateway clears the text before delivering the answer or stopped reply, then
  returns the session to `active`.
- `assistant.threads.setStatus` belongs to Slack's assistant bridge, which Slack retires in
  February 2027.

**Stop.** A classic press arrives as `block_actions`, an Agent Stop as `agent_session_stopped`,
whose user and thread come only from Slack's envelope. Unknown and duplicate stops are ignored.
[Stopping a run](gateway-liveness.md#liveness-progress-and-stopping-a-run) covers who may stop, the
`stopped` reply and the fixed time-limit line a wall-clock stop sends instead.

**Receiving.**

- Slack resends an envelope not acknowledged within about three seconds, and a session takes far
  longer, so the transport acknowledges each envelope before parsing it. A ring of 1,024 seen
  `(channel, ts)` pairs drops redeliveries and the second copy of a mention that arrives as both
  `message` and `app_mention`.
- Every read has a 90-second deadline, three of Slack's 30-second pings. An expired deadline logs
  `gateway_transport_silent` at warn and reconnects, as does a `disconnect` envelope.
- Only `message` and `app_mention` events can start a turn. Dropped: an event with `bot_id`, with no
  `user`, or from the bot's own user (this app's posts carry its user and no `bot_id`); a subtype
  other than `file_share`, `me_message` or `thread_broadcast`, so edits, deletions, joins and any
  new subtype; a message with neither top-level text nor a top-level file, even one that forwards a
  message.
- `channel_type` `im` is a direct message, `mpim` a group DM, anything else a channel; a reply
  inside a thread is a `thread` except in a DM. A `thread_ts` equal to the message's own `ts` marks
  the post that opened the thread, which keeps its parent's kind.
- `app_mention` carries no `channel_type`, so the transport asks `conversations.info` once per
  conversation: a 2-second deadline, no retry, skipped during the 429 cooldown that liveness and
  history calls share, and the answer cached for 512 conversations, oldest evicted. That is why the
  app needs `channels:read`, `groups:read`, `im:read` and `mpim:read`. A lookup that does not answer
  drops that message with `drop.reason = conversation-unresolved` and a debug
  `gateway_conversation_unresolved` naming the cause; the kind is never guessed.
- Subject: `slack.<team>.<user>`, lowercased.

**Owned-thread continuation (Agent).** Both manifests subscribe to `message.channels` and
`message.groups` (`channels:history`, `groups:history`) so the transport hears unmentioned
replies. After an explicitly addressed message whose sender's fresh broker grant is non-empty, that
sender may continue in the same root thread without a mention: a new turn is freshly authorized, a
message during a running turn steers it. A refusal, an empty grant, a seal or a restart ends the
claim; claims sit in a 1,024-entry LRU. Any other unmentioned channel event that no follow ticket or
PR link admits is dropped inside the transport, before routing, authorization or model spend.
[Slack Agent continuation boundary](architecture.md#slack-agent-continuation-boundary) owns the
claim and the optional `decline_chat_reply`.

**Attachments and forwards.**

- Up to ten files per message become [chat assets](gateway-assets.md#chat-assets): named in the
  prompt, fetched on demand. Names (cut to 128 bytes) and media types come from the event and are
  as untrusted as the text. An upload with no comment is a request.
- An admitted message that forwards another adds a quoted block: the original text (or its rich
  text), author, channel and time labels, and its files within the same ten-file allowance. The
  block reaches the request and platform recall. It never changes the speaker or route, and a
  mention inside it addresses nothing. A file with no ID or private URL is listed but cannot be
  fetched.

**Replying.**

- A channel answer joins the thread it was asked in, starting one on the inbound message. A classic
  DM answers top-level; an Agent DM answers in its root thread.
- The answer is a Block Kit [`markdown` block](https://docs.slack.dev/reference/block-kit/blocks/markdown-block/),
  which Slack renders as CommonMark; `text` is mrkdwn, so it repeats the answer only as the
  notification fallback. The block holds the answer as written, except that a line holding only an
  ordered-list marker (`1.` or `1)`) outside a code fence is escaped as `1\.`.
- A 429 on `chat.postMessage` logs `gateway_reply_rate_limited` and delays the same post once; a
  second 429 fails the reply. A post counts only when Slack returns the same channel and a
  canonical timestamp.
- Files upload through `files.getUploadURLExternal` and `files.completeUploadExternal`, with the
  text as the first file's comment.

**Log events:** `gateway_transport_silent`, `gateway_conversation_unresolved`,
`gateway_reply_rate_limited`, and `gateway_progress_degraded` with `surface` `agent-status`,
`assistant-status`, `reaction` or `cancel-ack`.

### Slack pull-request links

`ambient: { pullRequestLinks: true }` (default false) makes a Slack route review pull requests
posted in its channels, in either experience. Load refuses it unless the route also sets
`follow: true` and lists only the `channel` and `thread` kinds on a `slackSocketMode` transport.
The first matching route decides. Install the follow provider and its reviewed broker grants first.

An unaddressed top-level human message containing an HTTPS `github.com/<owner>/<repo>/pull/<n>`
link starts one turn under that message.

- Host case is ignored; `/files`, `/commits`, `/checks` and one trailing slash are accepted; query
  and fragment are ignored; Slack `<URL|label>` links work.
- Bot posts, edits, thread replies and other GitHub URLs trigger nothing. A mention keeps ordinary
  mention behaviour.
- Targets are deduplicated case-insensitively in arrival order and reviewed in one turn; the prompt
  gets each structured owner, repository and positive number.
- A repository name outside the GitHub subject grammar, such as a dotted one, cancels every link in
  that message before any review or subscription exists, and logs `gateway_message_ignored` with
  `reason = invalid-repository`.
- An unmapped or agent-denied poster gets no reply or reaction; the refusal is still traced with
  `admission.trigger = "pull-request-link"`.

The turn runs on the poster's broker leg alone, downscoped by the channel when mapped. Other
speakers wait for separate, freshly authorized turns, and their grants authorize nothing in this
one; ordinary mention turns keep [participant handles](gateway-conversations.md#participant-legs).
The gateway tells the model to submit a head-pinned COMMENT review, run the granted `follow stay`,
and reply with a short summary and the review link. GitHub tools and authority stay with the broker
and provider; this key adds no repository allowlist. A successful `follow stay` opens the thread's
follow ticket. With `wakes: true` and `sessions.wakes`, the review also subscribes the thread to
new commits: [PR subscriptions](gateway-wakes.md#pr-subscriptions).

### Slack thread follow

Once invited, the agent hears every speaker's unmentioned replies in one thread. Follow runs on
Slack and on Discord with `messageContent: true`.

**Configure.**

- Set `follow: true` on a route whose kinds are only `channel` and `thread`, on a `slackSocketMode`
  transport or a `discordGateway` transport with `messageContent: true`, and set
  `sessions.journal`. Absent, `null` and `false` disable it; `follow: true` on any other route is
  refused at load.
- Slack must deliver `message.channels` and `message.groups`, as both example manifests do;
  configuration cannot check installed scopes.
- Ship the out-of-tree `dekopon-provider-follow` and its reviewed grants before enabling a route.
- With a journal, follow state loads at boot even when no route follows, so a reload can enable the
  first follow route.

**Invite.** A successful, broker-authorized, no-argument `dekopon-provider-follow.stay` on the
speaker's own leg opens one ticket for the exact thread, shared by every speaker. The ticket
persists as a private `.followed` marker in the journal root; boot loads at most 1,024, and expiry
uses the stored `untilMs`, not file mtime. If the marker cannot be written the command fails with
`follow marker was not persisted`; on a route without follow it fails with
`follow is not enabled on this route`.

**Batch.**

- With a live ticket, unmentioned messages collect into one batch per thread. It closes after 30
  seconds of quiet or two minutes from its first message, then queues one fresh turn as its newest
  speaker; it never steers a running turn. A mention from that speaker cancels it.
- Each speaker's text sits under a batch-local `[follow speaker N]` label, within the inbound text
  bound. Labels grant no executable handles.
- At most 1,024 batches wait at once; a message past that is dropped with a `gateway.follow` record
  (`operation = refused`, `reason = capacity`).
- On Slack, follow takes precedence over owned-thread continuation and participant steering, and a
  follow route never falls back to continuation: without a ticket, an unmentioned message is
  dropped. Other threads and untagged top-level messages stay inactive.

**Authorize and answer.**

- Each turn opens a fresh broker leg as the newest speaker: the broker maps that subject, checks
  `agent.prompt` and returns only that speaker's current capabilities. An unmapped, forbidden,
  empty-grant or revoked speaker gets no model call, capability call or reply; the inviter's grants
  are never reused.
- Follow turns stream nothing and show no progress. A successful final reply posts once; a seal,
  refusal, failure, cancellation or decline posts nothing. Declining with `decline_chat_reply` is
  the default; a turn that did capability work and declines at its last step posts the fixed
  unreported-work warning, without leaving or refreshing the ticket.
- Only a delivered reply refreshes the 24-hour ticket.

**Leave.**

- A successful no-argument `dekopon-provider-follow.leave` leaves after the turn delivers its
  visible report.
- A seal, lapse, or any participant's configured stop word ends following, and on Slack so do
  `mute` and a human `-1`/`thumbsdown` reaction on any message the bot posted in the thread. Each
  drops pending follow work. A stop word still cancels running work only for its own sender.
- After leaving, a mention is admitted as usual and an authorized `follow stay` restores the ticket.
- The reaction needs `reaction_added`, `reactions:read` and `users:read`, so install or reinstall
  the app with them first. The transport resolves the message with `reactions.get`, checks that the
  bot wrote it in a thread with a live ticket, and ignores bot and app reactors (`users.info`).
  A lookup that fails is ignored and logs `gateway_follow_reaction_ignored` at debug.

**Log events:** `gateway.follow` records each invite, refresh, leave, seal and refusal, with a
`cause` on leave (`leave`, `mute`, `stop-word`, `reaction`, `lapse`).

## Discord Gateway

The transport connects outbound to Discord Gateway v10; no public endpoint is needed.
[`examples/discord/`](../examples/discord/README.md) owns bot installation, permissions, token,
routes and identity mapping.

**Connecting.**

- Discovers the socket URL with authenticated `GET /api/v10/gateway/bot` and identifies with the
  `GUILD_MESSAGES` and `DIRECT_MESSAGES` intents, adding the privileged `MESSAGE_CONTENT` intent
  when the transport sets `messageContent: true`.
- Jitters the first heartbeat and reconnects when a heartbeat goes unacknowledged; tracks the
  dispatch sequence and resumes the session after a reconnect. Invalid Session waits 1–5 seconds,
  then resumes or identifies again. Identifies are 5 seconds apart, within the session-start limit.
- Terminal, with no retry: close codes 4004 and 4010–4013, an exhausted session-start limit, HTTP
  401 or 403, and close 4014 (disallowed intents). 4014 fails with
  `Discord refused the requested gateway intents (close 4014)` and names the Developer Portal
  toggle.

**Receiving.**

- Dropped: bot and webhook posts, the bot's own posts, and message types other than default and
  reply. A message with neither content nor attachments is dropped as `content-withheld`: Discord
  sent it without its text ([How @mentions reach the bot on Discord](#how-mentions-reach-the-bot-on-discord)).
- A message without `guild_id` is a direct message. A guild message is addressed when its
  `mentions` names the bot user or its `mention_roles` holds the bot's managed role.
- Subject: `discord.<user id>`. User snowflakes are global, so the guild is not part of it.
- A guild message's kind comes from one `GET /channels/{id}` per channel: 2-second deadline,
  cached for 512 channels. Text, voice and announcement channels are `channel`. A thread or forum post is
  `thread`, with its parent channel as the conversation `id` and its own channel ID as `thread`, so
  `kind: [channel, thread]` with the parent's ID claims its threads. Any other channel type, or a
  lookup that does not answer, drops the message as `conversation-unresolved`.
- Each drop logs `gateway.message.dropped` with its `drop.reason`.
- [Thread follow](#slack-thread-follow) needs `messageContent: true` and has no `mute` or reaction
  leave.

**Replying.**

- Replies are `POST /api/v10/channels/{channel}/messages` to the message's channel, which for a
  thread is the thread itself. Provider attachments ride the first post as multipart; the first
  guild post references the request with `fail_if_not_exists: false`. Every post disables parsed
  and reply mentions, so model text cannot ping a user, role or `@everyone`.
- An answer longer than 2,000 UTF-16 units is split ([Shared behaviour](#shared-behaviour)). A 429
  delays a post once when Discord asks for 30 seconds or less, and fails it otherwise.
- With `indicators.mode: native`, an authorized session sends `POST /channels/{id}/typing` and
  renews it about every eight seconds, inside Discord's ten-second lease. Typing has no clear; the
  final message ends it sooner. Typing calls have a 2-second deadline and their own 429 cooldown,
  never take the reply lock, and cannot fail the answer.

**Log events:** `gateway.message.dropped`, `gateway_conversation_unresolved`,
`gateway_role_unresolved`, `gateway_cancel_ack_failed`.

### How @mentions reach the bot on Discord

A server the bot is installed in has two things that render as the same "@Dekopon" pill (the
application's name), and member autocomplete, on mobile especially, offers both:

- **The bot user**, listed with an **APP** badge. Mentioning it puts the bot's user ID in the
  message's `mentions`.
- **The managed integration role** Discord creates when the application is installed with
  permissions. It carries `tags.bot_id` set to the bot's user ID. Mentioning it puts the role ID in
  `mention_roles` and leaves `mentions` empty.

Discord sends a guild message's text only when it mentions the bot user, unless the application
holds the Message Content intent. What reaches the gateway (an unaddressed message is ignored with
`gateway_message_ignored`, `reason = not-addressed`):

| Guild message | `messageContent: false` | `messageContent: true` |
|---|---|---|
| Mentions the bot user | text delivered, addressed | text delivered, addressed |
| Mentions the bot's managed role | no text: dropped as `content-withheld`, `mention.roles = true` | text delivered, addressed |
| Mentions another role, such as a user-created "@bots" the bot holds | no text: dropped as `content-withheld`, `mention.roles = true` | text delivered, not addressed |
| Mentions nothing | no text: dropped as `content-withheld` | text delivered, not addressed |

On a guild message with any `mention_roles`, the gateway reads `GET /guilds/{guild}/roles` once per
guild and caches the role whose `tags.bot_id` is this transport's bot user, or that there is none,
for 128 guilds, oldest evicted. Only the bot's own managed role addresses it, so several Dekopon
applications in one server each answer only their own. The session sees the role mention rewritten
to the bot's user mention, so stop words and the prompt read both pills alike. The lookup has a
2-second deadline and honours the REST cooldown; one that does not answer leaves the message
addressed by `mentions` alone and logs `gateway_role_unresolved` at debug. A reinstalled bot gets a
new role, which the gateway learns at its next restart.

When someone mentions the bot and nothing happens:

- **Which @Dekopon was picked?** The bot user carries the **APP** badge; the role does not.
- **A role mention without text?** It needs `messageContent: true` on the transport and **Message
  Content Intent** on in the Portal ([Set up role mentions](#set-up-role-mentions)).
- **Read the evidence.** `gateway.message.dropped` and the `transport.receive` span carry the
  table's `drop.reason` and `mention.roles`; an unaddressed message logs `gateway_message_ignored`
  (`reason = not-addressed`) at debug.

### Set up role mentions

Either let role mentions through or keep the role out of autocomplete.

To let them through:

1. Developer Portal → your application → **Bot** → **Privileged Gateway Intents** → turn on
   **Message Content Intent** and save.
2. Set `messageContent: true` on the `discordGateway` transport and restart.

With the Portal toggle off, the transport stops at connect with close 4014
([Discord Gateway](#discord-gateway)).

To hide the role instead, keep `messageContent` off and turn off both ways a member is offered it,
then check channel overrides:

- the role's own **Allow anyone to @mention this role**: Server Settings → **Roles** → the role
  under the application's name (marked as managed by the integration) → **Display**;
- **Mention @everyone, @here, and All Roles**, which a member holds through @everyone (**Roles** →
  **Default Permissions** → **Permissions**), through another role, or through a channel's
  permission override (Edit Channel → **Permissions**).

The server owner and any **Administrator** role hold every permission, so they are always offered
the role.

## Telegram long polling

`getUpdates?timeout=50&offset=N` holds one connection until an update arrives. Advancing `offset`
is the acknowledgment, so there is no separate ack call. The offset moves past every update, routed
or not, or a filtered update would return forever.

**Receiving.**

- Dropped: bot messages, messages with no text, caption or attachment, and broadcast-channel posts
  (`drop.reason = broadcast-channel`; nobody can reply there).
- A private chat is a direct message; a group or supergroup is a channel and needs `@botname`
  ([Routing](gateway-conversations.md#routing)).
- Subject: `telegram.<user id>`.
- A forum topic is a `thread` keyed `<chat>:<topic>`, but only when the update carries
  `is_topic_message`: a plain reply also carries `message_thread_id` and stays in the chat's
  conversation. A topic ID that is not a positive integer drops that one update as
  `malformed-envelope`; the rest of the batch still routes, since the offset already passed it.
- The topic ID follows the message through admission, conversation identity, replies, photo
  uploads and typing, so a topic's indicator never shows in another topic.

**Replying.**

- `sendMessage` takes at most 4,096 UTF-16 units, so a longer answer is split
  ([Shared behaviour](#shared-behaviour)). Outside DMs only the first message quotes the request;
  every message carries the topic ID. A send counts only when Telegram returns the same chat and
  topic.
- Provider attachments (PNG or JPEG) go through `sendPhoto`. Text within the 1,024-unit caption
  ceiling rides the first photo; longer text follows as split `sendMessage` calls.
- With `indicators.mode: native`, an authorized session sends `sendChatAction(action=typing)` and
  renews it about every four seconds, inside Telegram's five-second lease. It has no clear; the
  final message ends it. These calls use a 2-second deadline instead of the poll's 70 seconds,
  honour `retry_after`, and cannot fail the answer.

**Log events:** `gateway_transport_ack_failed` (a Stop button acknowledgment that failed).

## Meta WhatsApp Cloud API

`whatsappCloudApi` is an inbound plain-HTTP listener behind operator-owned HTTPS termination, such
as Cloudflare Tunnel and Traefik; the gateway terminates no TLS.
[`examples/whatsapp/`](../examples/whatsapp/README.md) owns network, secrets and Meta setup.
Video, documents, stickers, templates, interactive messages, inbound reactions, progress messages,
status processing, business-management APIs, embedded signup and webhook multiplexing are not
supported; see [non-goals](design.md#non-goals).

**Webhook.**

- The callback path serves GET verification and POST delivery only; other paths get 404 and other
  methods 405. Every response is content-free apart from the challenge, with
  `Cache-Control: no-store`.
- GET needs exactly one each of `hub.mode=subscribe`, `hub.verify_token` and `hub.challenge`,
  compares the token in constant time, and returns the decoded challenge as plain text.
- POST needs exactly one `X-Hub-Signature-256: sha256=<lowercase hex>` and verifies HMAC-SHA256 over
  the exact raw body before parsing JSON.
- Bounds: 16 concurrent requests, a 5-second request deadline, 32 headers within 8 KiB, a 256 KiB
  body, 128 messages per delivery, and 512 queued messages across 64 deliveries.
- Refusals: 401 `unsigned` or `signature`; 400 `malformed` (including more than 128 messages) or
  `oversize` headers; 413 `oversize` body; 403 `verification`; 408 `timeout`; 503 `saturated`. Each
  reason logs `gateway_whatsapp_webhook_refused` at most once a minute, with `suppressed` counting
  the refusals folded into the gap, and nothing about content
  ([fields](observability.md#the-whatsapp-webhook-is-the-one-signal-a-stranger-can-drive)).

**Admission.**

- Only `object=whatsapp_business_account`, `field=messages`, `messaging_product=whatsapp` changes
  for the configured WABA and phone-number ID produce sessions; every entry, change and message in
  a signed batch is checked. Status-only, unknown, unsupported-type, wrong-destination and
  self-sent messages are acknowledged and ignored.
- Text and PNG/JPEG image messages are admitted. The signed `messages[].from` is both the reply
  target and the only identity source; profile names, display phone numbers, text, WABA and
  phone-number IDs never name the sender.
- Subject: `whatsapp.<wa_id>`. The conversation is a direct message in container
  `<waba>:<phone-number-id>`.
- A message is answered only when the delivery's own `contacts[]` names its sender in `wa_id` and
  it carries no `group_id` or `participant`. A group payload names the group in `from` and matches
  no contact, so it, and any shape that fails these checks, is dropped rather than answered as if
  the group were a person. Each such message logs `gateway_message_ignored` with
  `reason = group-unsupported` and its `message.index`, because one receive span covers the whole
  delivery.
- An image message whose metadata is not PNG or JPEG with a numeric media ID logs
  `reason = image-metadata` and is dropped.

**Acknowledgment.**

- The handler queues the delivery, then returns 200 before any turn starts, so Meta retries only a
  delivery whose 200 was lost in the network. There is no dedup: a retried message arriving while
  its turn runs joins it as a steer, and one arriving later starts a fresh turn and gets a second
  reply.
- A full queue returns 503 so Meta redelivers. A crash after the 200 and before the queue drains
  loses the message.
- An `accept()` failure on a dead connection is ignored (`gateway_whatsapp_accept_failed`,
  `kind = connection`, debug). Descriptor or buffer exhaustion (`kind = exhausted`, warn) or an
  unusable socket (`gateway_whatsapp_listener_stopped`, error) ends the listener; connection
  recovery drops its connections and rebinds.

**Inbound images.**

- A photo becomes one lazy media-ID reference; webhook URLs are ignored. The caption, possibly
  empty, is the user text. Nothing downloads before routing and fresh authorization. The route's
  model needs `modalities: [image]` to see it; editing it needs an image provider in the broker
  ([image editing](../examples/whatsapp/README.md#image-editing-and-generation)).
- `GET /{version}/{media-id}?phone_number_id={phone-number-id}` resolves the ID. The metadata JSON
  is capped at 16 KiB; its ID and MIME must match the source, and its `file_size` (number or
  string) must be nonzero, within the limit and equal to the downloaded length.
- The bearer token goes only to `https://lookaside.fbsbx.com:443` with path
  `/whatsapp_business/attachments/`. Userinfo, a fragment, and any other host, port or path are
  refused, so a Meta CDN host outside that one fails closed.
- The client resolves only `graph.facebook.com` and `lookaside.fbsbx.com`, with a 5-second DNS
  deadline and at most 32 public addresses, and connects to exactly those. Each Graph or download
  request has a 15-second deadline.
- A download stops at the smaller of the caller's bound and 5,000,000 bytes, then checks the length
  and the PNG/JPEG signature. There is no decoding, transcoding, URL caching or retry.
- A refused fetch or upload logs `gateway_whatsapp_media_refused` with a fixed `reason`.

**Replying.**

- Text goes to `https://graph.facebook.com/{version}/{phone-number-id}/messages` with the gateway's
  bearer token, at most 4,096 Unicode scalars per message ([Shared behaviour](#shared-behaviour)).
  Redirects are off, responses are capped at 16 KiB, and Meta error bodies never reach chat or logs.
- Provider images (PNG or JPEG, at most 5,000,000 decoded bytes, all checked before any part is
  sent) upload by multipart `POST /{version}/{phone-number-id}/media` with
  `messaging_product=whatsapp` and a gateway-named `file` part, then go out as an image message by
  `image.id`, never by URL. Text of at most 1,024 scalars captions the first image; longer text
  follows in full after all images.
- Nothing is retried: a timeout after the request is sent leaves the outcome unknown, and resending
  could duplicate a visible answer. A failure after any accepted part is `partial-delivery` and
  logs `gateway_whatsapp_reply_partial` once with the failure's category. Upload acceptance is not
  message acceptance, and a message ID is not proof a person read it
  ([durable memory](gateway-conversations.md#durable-memory-after-transport-acceptance)).
- Free-form replies are subject to Meta's customer-service window; there is no template fallback.
  Uploaded media is never deleted; Meta's retention applies.
- With `indicators.mode: native`, typing is sent with the inbound message's read receipt and renewed
  every 20 seconds.

## Local development transport

An owner-only Unix socket that drives routed sessions without a chat service. It trusts its caller
to declare a subject, so it is a development tool, not a production transport.

The parent directory must belong to the gateway's UID with no group or world access. An existing
socket is replaced only when it is a private, single-link socket of that UID. The new socket is
`0600`, and shutdown removes only the inode it created. An insecure path is terminal. Requests and
answers are line-delimited JSON on the same connection:

```console
$ nc -U /path/to/gatewayd-dev.sock
{"subject": "tel.16034700182", "conversation": {"kind": "channel", "id": "ops"}, "text": "what changed today?"}
{"reply": "Nothing external. Two read-only capability calls."}
```

**Requests.**

- Fields: `subject` (canonical), `text`, and an optional `conversation`
  `{ kind, container?, id, thread? }` defaulting to `{ kind: directMessage, id: dev }`. Unknown
  fields are refused. `{"subject": …, "stop": true}` without `text` stops that subject's run in the
  named conversation and is answered `{"cancel": {"acked": true}}`.
- A line over 64 KiB closes the connection. Refused lines log `gateway_local_request_rejected` at
  debug with `reason` `line-too-long`, `malformed-request` or `ambiguous-request` (`stop` with
  `text`).
- Every line is addressed: the socket mode is the authentication, and the JSON carries no mention
  syntax. The caller chooses the kind, so a route table, a liveness override or a memory key can be
  exercised against `channel`, `thread` or `groupDirectMessage` without a chat service.

**Output.**

- A text answer is `{"reply": …}`; a progress message finalized in place adds its `id`. Provider
  attachments add an `images` array of gateway-owned `filename`, `mediaType` and base64 `data`, each
  file inlined whole; the field is absent otherwise.
- With native indicators the connection also receives `{"typing": true}`, `{"status": …}`,
  `{"reaction": …}`, `{"progress": {id, text, cancel}}` or `{"progress": {id, deleted: true}}`, and
  `{"delta": {id, text, cancel}}` lines.

**Trust.**

- The declared subject grants nothing. It is a claim in the broker's chat-attested `invoke`; the
  broker needs an attestor grant covering its namespace and an owner-controlled mapping before it
  resolves a principal. `0600` limits callers to the owner's UID, the gateway's local trust domain.
- The declared subject also selects history. On private scope a caller replays what was said under
  that subject on this local transport and agent, never another transport's. On
  `sharedConversation` the subject is not in the key, so every caller of the route shares one
  conversation. No authority moves, but text does.
- The `authenticated participant` label means the broker accepted the declared claim on a fresh leg
  from the owner-UID caller, not that the socket authenticated the person.
