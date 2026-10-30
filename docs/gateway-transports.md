# Gateway transports

How `dekopon-gatewayd` connects to each chat service and the GitHub webhook. Configure the keys in [`gatewayd.md`](gatewayd.md#configuration).

## Transports

### fauxlack (tests only)

The `fauxlack` Cargo feature enables an in-process chat service for Rust gateway tests. It has
no credentials, socket or listener. Build a `dekopon_gatewayd::fauxlack::Fauxlack` from one YAML
manifest, then pass it to `run_fauxlack(config_path, shutdown, service, shell_threads)`. Ordinary `run`, `check`
and the executable have no service and refuse every fauxlack transport, even in a feature build.

```yaml
transports:
  - name: chat
    kind: fauxlack
    server: acme
    bot: udekopon
    indicators: {mode: native, progress: message}
```

The manifest's `servers` list declares each server's `id`, `users` (`id`, `name`, optional `bot`),
`channels` (`id`, `name`, `members`), `directMessages` (`id`, `members`) and `files`. File entries
have a `name` and exactly one of `canvas` (immutable Markdown), `snippet` (plain text), or
`image: {mime, base64}`. All authored levels reject unknown fields. IDs use the shared subject
and conversation validators; semantic conflicts are reported together. Authority belongs to
broker mappings and grants, not the manifest.

Member bots receive posts in order over bounded queues; posting waits for room. The adapter drops
bot-authored posts. Channel roots and replies share one thread; DMs have no thread. Channel posts
must explicitly mention the bot with `<@userId>`. The driver offers replies and message progress;
other optional surfaces are absent. Manifest files are fetched from memory and authorized outbound
attachments appear as files on the bot's answer. Waiting handles observe completed answers only,
including progress finalized in place; unfinished or deleted progress is absent from transcripts.
Each posted handle captures its thread observer before delivery, so an immediate answer remains
observable. Clones share that observer's cursor.

Without the feature neither the world nor adapter compiles. The config and private target/source
vocabulary still decode in every build; existing wake and journal persistence rules apply. Tests
use disposable stores. This service tests gateway processing after its own projection; real chat
platform projection remains covered by those adapters' tests.

### Slack Socket Mode

An app-level token opens `apps.connections.open`, which returns a `wss://` URL; a bot token answers through `chat.postMessage` or Slack's external file-upload flow for an attachment. No public HTTP endpoint is needed, which is why Socket Mode rather than a public Events API request URL. [`../examples/slack/`](../examples/slack/README.md) has separate classic/free and paid/admin-enabled Agent manifests, plus the token and identity-mapping walkthrough. The classic manifest enables interactivity, which is what delivers a `block_actions` Stop press over that same connection; without it a cancel button renders and the press reaches nothing.

`experience` controls Slack's conversation model and is never inferred from a cosmetic API result:

- `classic` (default) retains top-level DM replies and one whole-DM conversation. With native
  indicators and `reaction.working: auto`, the gateway adds its fixed `:tangerine:` reaction to
  the inbound message and removes only this turn's attempted working reaction.
- `agent` makes DMs thread-scoped like Slack Agent sessions and enables authorization-fed channel
  thread continuation. After fresh broker authorization the gateway calls
  `agents.sessions.setStatus(processing)` once; Slack owns the standard Working UI and one-hour
  processing timeout, so the gateway does not waste rate limit on a heartbeat. After a reply or
  no-reply completion it asynchronously returns the session to `active`; no-reply means
  no chat message, not omission of that cosmetic cleanup.
  `feature_disabled`, `missing_scope`, and equivalent permanent installation errors disable Agent
  status for that transport; `reaction.working: auto` can fall back, then no-op if reactions
  are also unavailable. It never guesses the workspace plan.

An Agent transport may opt into `indicators.statusText: true` (default false, also overridable per
conversation kind), with `progress: off` or `auto`. Where no progress message is
being written, the first opted-in note returns the native session to `active` and shows the existing
rendered line through `assistant.threads.setStatus`. The handover is one-way for the run: subsequent
note, tool, working and keep-alive lines use that status surface and the same coalescing/60-edit budget.
Slack hides its native Stop while custom status text shows; configured stop words still work.
Only the existing keep-alive schedule refreshes it; Slack drops the text after two minutes without
a refresh. Exhausting keep-alives, spending the 60-edit budget or tripping the status-text breaker
attempts to restore native Working once. Terminal handling clears text before delivering the answer or stopped reply, then
returns native status to `active`. Permanent installation errors disable the custom surface for the
transport. This legacy assistant method sunsets with Slack's assistant bridge in February 2027.

Slack's native `processing` state includes a Stop button. The transport acknowledges
`agent_session_stopped` before handling it, derives its user and thread only from Slack's envelope,
and lets the initiating subject win one atomic race against the normal answer. A Stop prevents
subsequent model turns and capability invocations, suppresses the stale answer/history commit,
queues `active`, and sends the `stopped` template (`Stopped.` by default). Wall-clock stops instead use the fixed time-limit line below. In-flight model HTTP sends and reads, including silent
reasoning, select on the session watch and are cancelled locally. Remote inference or provider
effects already accepted cannot be rolled back. The broker leg drives a provider command word's round trip in place using the
`dekopon-process` cancel latch and shared `TreeContext` accounting. Stop or the script deadline drops a command word in
flight, after which the script reads `session-cancelled`. Unknown,
duplicate, and other-user Stop events are ignored.

Both manifests subscribe to `message.channels` and `message.groups`, requiring
`channels:history` and `groups:history`, so the transport can hear follow-ups that contain no new
mention. Without route follow, the Agent experience uses **owned-thread continuation**:

- an explicitly addressed channel message proposes an exact authenticated
  `(workspace, channel, root thread, sender)` claim;
- only a fresh non-empty broker capability surface installs or refreshes that claim;
- a later unmentioned event must match every coordinate; a new turn is freshly authorized, while
  a same-sender steer joins the running leg; another sender and another thread remain ambient;
- a definitive authorization refusal removes the claim; the 1,024-entry LRU and every claim vanish
  on process restart; and
- all unmatched channel-history events are discarded inside the Slack transport before routing,
  authorization, payload telemetry, or model spend.

An inherited Slack Agent continuation or an authorized Slack/Discord follow turn may decline its reply. The prompt says the message
was not addressed to the agent and offers `decline_chat_reply`; selecting it before any
capability work posts nothing, produces no transport receipt or durable recording, and remembers
the user's message as a user-only in-process turn. A decline selected in the same turn as work runs
none of that work. If an earlier capability invocation already happened, silence is refused and the
model must send a concise report. With no model turn left, either optional turn posts the fixed
unreported-work warning directing the sender to check the audit before retrying. Explicit mentions and
DMs never receive the decline tool.

The protocol's one sharp edge is redelivery: Slack expects an acknowledgment within roughly three seconds and resends the envelope otherwise. A Dekopon session takes far longer than that, so **the acknowledgment is sent before any processing begins** — before parsing, before routing, before any model call. A bounded ring of 1024 seen `(channel, ts)` pairs absorbs the redeliveries that happen anyway across a reconnect.

- `disconnect` envelopes are routine (Slack rotates sockets on its own schedule) and trigger a reconnect with jittered exponential backoff capped at 60 seconds.
- Every read has a 90-second liveness deadline. The shared recovery layer bounds the complete connection attempt (including handshake and `hello`) to 30 seconds. Slack pings a healthy connection about every 30 seconds and sends no client heartbeat of its own, so silence past the deadline means the path is gone without TCP saying so: a NAT table dropping the flow, or a partition with no RST. An expired deadline logs `gateway_transport_silent` and reports the socket closed, which is the reconnect path the backoff already owns. Without it the reader waits on a half-open socket forever and every route on the workspace goes quiet with nothing logged.
- Messages carrying `bot_id` and messages from the bot's own user identifier are dropped. Both checks matter: another app's post carries `bot_id`, and this app's own post arrives with the bot's user identifier and no `bot_id` at all.
- A subtyped message is dropped unless its subtype is `file_share`, `me_message`, or `thread_broadcast`. Most subtypes are events *about* a message — an edit, a deletion, a channel join — and answering one would answer a question twice or answer nobody. Those three are a person making a new request. `file_share` is the one worth naming: Slack stamps it on any message carrying an upload, so a question asked with a screenshot attached arrives under it. The list is an allowlist, so a subtype Slack introduces later is dropped until someone decides it is a request.
- A message's attachments become **chat assets**, described in the prompt and fetched only on demand. See [Chat assets](gateway-assets.md#chat-assets) below. The transport reports what arrived and nothing more: names and media types come from the event, so they are sender-controlled and untrusted exactly like the message text. An upload posted with no comment is a request in itself — the reference note is the whole message. A message with neither top-level text nor a top-level file is not a request and is dropped, even when it contains a forwarded message.
- An admitted Slack forward with a share/unfurl attachment contributes visibly quoted original text, available author/channel/time labels and inline files to the forwarding speaker's request and to platform recall. Original attribution never changes the speaker or route; a mention inside the quote cannot start a conversation. Forwarded files share the message's ten-attachment allowance. Files with no ID or private download URL remain in the asset inventory, whose reference note says the gateway cannot see them; fetchable files that later fail are reported by the ordinary asset fetch failure.
- `channel_type` decides the conversation kind: `im` is a direct message, `mpim` a group direct message, anything else a channel, or a thread under it when the message sits inside one. An `app_mention` event carries no `channel_type` at all, so the gateway asks `conversations.info` rather than assuming `channel` — a mention in a DM or a multi-person DM minted as a channel matches no `directMessage` or `groupDirectMessage` route or grant. That call is deliberately the last thing the reader does, after every drop that costs nothing, so ambient traffic never pays for it; it takes the two-second liveness deadline, is never retried, honours the app-wide 429 cooldown every other Web API call shares, and its answer is remembered for 512 conversations, oldest evicted — a conversation never turns into a different one, so a hit is never stale and a restart costs one lookup per conversation the bot is addressed in. This is why an installation needs `channels:read`, `groups:read`, `im:read`, and `mpim:read` beside its reply and history scopes. A lookup that does not answer drops that one message with `drop.reason = conversation-unresolved`, because a guessed kind is what a route and a grant would then be decided against.
- A `thread_ts` equal to the message's own `ts` is the post that *opened* a thread rather than a reply inside one — Slack stamps it on both — so that message keeps the kind of the conversation it was posted in instead of becoming a thread under itself.
- Subject: `slack.<team>.<user>`, lowercased.
- A channel answer joins the thread it was asked in, starting one on the inbound message when there is none. A classic direct message has no thread to join; an Agent direct message is intentionally rooted at `thread_ts = event.thread_ts || event.ts`, and that root also scopes admission, history, status, Stop, and owned continuation.
- An answer is posted in a Block Kit [`markdown` block](https://docs.slack.dev/reference/block-kit/blocks/markdown-block/), which carries the model's CommonMark unchanged and lets Slack render it. The `text` field is mrkdwn — a proprietary syntax where bold is `*one asterisk*` and a link is `<url|label>` — so an answer posted through it alone arrives with `**bold**` as four literal asterisks, and tables and task lists cannot be expressed in it at all. Translating in this process would be a second translation of what Slack is about to translate, so the gateway does none: the block gets the answer verbatim. `text` carries the notification fallback, the one place blocks do not render. The block caps a payload at 12,000 characters, which the 8 KiB outbound bound already sits under.

### Slack pull-request links

A Slack channel route may opt in with `ambient: { pullRequestLinks: true }` alongside
`follow: true` and `sessions.journal`. The boolean defaults to false; enabled values refuse
non-Slack transports and route kinds other than channel/thread, including DMs and group DMs.
The first matching route controls the opt-in. Use core support before adding the key: older
gateways reject it. Install the follow provider and reviewed broker grants before activation.

An unaddressed human top-level message containing an HTTPS `github.com/<owner>/<repo>/pull/<n>`
link starts one turn under that message. Host case is ignored; `/files`, `/commits`, `/checks`
and one trailing slash are accepted, and query/fragment do not change the target. Slack
`<URL|label>` links work. Distinct targets are deduplicated case-insensitively in arrival order
and reviewed in one turn; structured owner, repository and positive number reach the prompt.
Repository names must fit the canonical GitHub subject grammar; a dotted repository name is
refused during link parsing before a review or subscription can be created.
Bot posts, edits, other GitHub URLs and thread replies gain no trigger. A mention retains
ordinary mention behavior. Both classic and Agent Slack experiences support the opt-in.
An ambient link from an unmapped or agent-denied poster gets no reply or reaction; the refusal
is still traced with `admission.trigger = "pull-request-link"`.

A PR-link turn keeps only the poster's broker leg throughout execution, downscoped by the channel
when mapped. Other speakers wait for separate, freshly authorized turns; their additional grants
cannot authorize work within the review turn. Ordinary mention turns keep participant handles.
The gateway provides review instructions, including a head-pinned COMMENT review, the short Slack
summary with the returned review link, and the granted `follow stay` command. GitHub tools and
authority remain broker/provider-owned; no repository allowlist is added here. The successful
follow command opens the existing ticket, and delivered follow replies refresh it normally.
With `wakes: true` and `sessions.wakes`, the first successful COMMENT review of a structured
target in a mapped channel also subscribes that thread to new commits; see [PR subscriptions](gateway-wakes.md#pr-subscriptions).

### Slack thread follow

On a `follow: true` channel/thread route, a successful broker-authorized no-argument
`dekopon-provider-follow.stay` opens one ticket shared by every speaker in the exact thread.
With a journal, the gateway initializes resident follow state even when every route starts with
follow disabled, so enabling the first follow route through reload needs no restart.
Slack must deliver `message.channels`/`message.groups` events with `channels:history`/`groups:history`;
configuration cannot verify installed app scopes. Both example manifests subscribe to these events.
Unmentioned replies enter the same bounded follow batches described below, ahead of Agent
owned-thread continuation or participant steering. Other threads and untagged top-level messages
remain inactive. The turn uses the newest speaker's fresh broker leg, downscoped by the channel
space when mapped. Speaker labels in a batch describe context and grant no executable handles.
Declining is the default; only a useful contribution warrants a reply, and only a delivered reply
refreshes the 24-hour ticket.

`follow leave` takes effect after its visible report. `mute`, configured stop words, a human
`-1`/`thumbsdown` reaction on any message this bot posted in the thread, or lapse closes following.
These signals remove pending follow work; session cancellation still belongs only to its asker.
An @mention is admitted again and an authorized `follow stay` restores the ticket. Follow-enabled
routes do not fall back to owned-thread continuation after leaving.

The reaction handler requires `reaction_added` and `reactions:read`, plus `users:read` to distinguish
bot reactors. It resolves the referenced message's root with `reactions.get`, verifies the bot
as author, and ignores bot reactors, non-agent messages and unresolved lookups. Install or reinstall
the app with these scopes before using reaction-based leave. No ticket marker format changes.

### Discord Gateway

A route may declare `follow: true` for Slack socket-mode channel/thread kinds, or Discord channel/thread kinds when its transport sets `messageContent: true`, with `sessions.journal` configured in either case. Absent, `null`, and `false` leave it disabled; other route kinds and transports refuse at configuration load. The gateway loads a bounded resident ticket table from private `.followed` journal-root markers at boot; marker expiry uses the stored `untilMs`, not file mtime. A successful authorized no-argument `dekopon-provider-follow.stay` invocation on its speaker's leg records an invitation. Batches retain each speaker's text with batch-local speaker labels, under the existing text bound. Unmentioned messages with a valid ticket coalesce for 30 seconds of quiet or at most two minutes from the first message, then queue a fresh turn as the newest speaker, without steering a running turn. An addressed message from that speaker cancels the pending batch. Each turn opens a fresh broker leg as the newest authenticated speaker: the broker maps that subject, checks `agent.prompt` and returns only that speaker's currently named capabilities. Unmapped, forbidden, empty-grant and subsequently revoked speakers receive no model call, capability call or reply; the inviter's grants are never reused. A memory-forbidden speaker cannot use a memory capability, and a new provider capability is unavailable until explicitly listed and granted by the broker. Follow turns show no streaming or progress before completion: a successful final reply is posted once, while seal, refusal, failure, cancellation or pre-work quiet decline posts nothing, except that a worked turn declining on its final step posts the fixed unreported-work warning without leaving or refreshing the ticket. Only a delivered reply refreshes the 24-hour ticket. A successful no-argument `dekopon-provider-follow.leave` requests self-leave, applied only after the turn delivers its visible report; a seal or any participant's configured stop word ends follow; stop words still cancel running work only for their own sender. The out-of-tree provider and reviewed grants must ship before route opt-in.

Discord Gateway v10 is another outbound WebSocket transport. The daemon discovers the service URL through authenticated `GET /api/v10/gateway/bot`, requests the non-privileged `GUILD_MESSAGES` and `DIRECT_MESSAGES` intents, adds the privileged `MESSAGE_CONTENT` intent when the transport sets `messageContent: true`, and identifies after Hello. It jitters the first heartbeat, requires each heartbeat ACK, tracks dispatch sequence, resumes a live session after reconnect, honors Invalid Session and identify/session-start limits, and treats Discord's fatal close codes as terminal transport failures; close 4014 (disallowed intents) stops the transport with an error naming the Developer Portal toggle. No public endpoint is required. Without `messageContent`, Discord exposes content and attachments only in DMs and in guild messages whose structured `mentions` array names the bot; with it, every message the bot can see arrives with its text, and addressing still decides which ones wake a session.

- Bots, webhooks, the bot's own posts, and message types other than ordinary messages and replies are dropped.
- Absence of `guild_id` is a direct message. A guild message is a channel message and must address the bot: its user in `mentions`, or its managed role in `mention_roles` (see [How @mentions reach the bot on Discord](#how-mentions-reach-the-bot-on-discord)). Subject: `discord.<user id>`; Discord user snowflakes are global, so a guild is not part of the canonical subject.
- A Discord thread is itself a channel. Its channel ID is the route key, conversation identity, and reply destination. A catch-all channel route covers transient threads; a route naming only a parent channel does not automatically claim its thread IDs.
- Replies use `POST /api/v10/channels/{channel}/messages`. Provider attachments ride the first post as multipart attachments; the first guild post references the incoming message with `fail_if_not_exists: false`; every post disables parsed/reply mentions, so model-authored text cannot ping a user, role, or `@everyone`. Discord's 2,000-character ceiling is handled by lossless multi-message splitting at line boundaries. Splitting knows Markdown code fences (backtick or tilde, any length, up to three spaces of indent, an unterminated fence running to the end): a block that would straddle a boundary but fits in one message moves whole to the next, and a block longer than a message is cut at a line inside it, closed at the end of the message and reopened with the same info string at the start of the next, both fences counted against the ceiling. Text outside fences is otherwise left unchanged. Failure after an accepted attachment or first chunk is partial delivery rather than a complete receipt.
- With `indicators.mode: native`, an authorized session immediately triggers `POST /channels/{id}/typing` and renews around every eight seconds, inside Discord's ten-second native lease. Typing has no explicit clear; sealing stops renewal and the final message clears it sooner. Calls use a short deadline, honor a `429` cooldown, never take the final-message REST lock, and cannot fail the answer.

[`../examples/discord/`](../examples/discord/README.md) is the bot installation, permission, token, route, and identity-mapping walkthrough.

### How @mentions reach the bot on Discord

A server the bot is installed in has two things that render as the same "@Dekopon" pill (the application's name), and the member autocomplete, on mobile especially, offers both:

- **The bot user**, listed in the member list with an **APP** badge. Mentioning it puts the bot's user id in the message's `mentions` array.
- **The managed integration role** Discord creates when the application is installed with permissions. It carries `tags.bot_id` set to the bot's user id. Mentioning it puts the role id in `mention_roles` and leaves `mentions` empty.

Discord includes a guild message's text only for a user mention of the bot, unless the application holds the Message Content intent. What reaches the gateway, by mention and `messageContent` (an unaddressed message is ignored with `gateway_message_ignored`, `reason = not-addressed`):

| Guild message | `messageContent: false` | `messageContent: true` |
|---|---|---|
| Mentions the bot user | text delivered, addressed | text delivered, addressed |
| Mentions the bot's managed role | no text: dropped as `content-withheld`, `mention.roles = true` | text delivered, addressed |
| Mentions another role, such as a user-created "@bots" the bot holds | no text: dropped as `content-withheld`, `mention.roles = true` | text delivered, not addressed |
| Mentions nothing | no text: dropped as `content-withheld` | text delivered, not addressed |

On a guild message with any `mention_roles`, the gateway reads `GET /guilds/{guild}/roles` once per guild and keeps the role whose `tags.bot_id` is this transport's own bot user, or the fact that there is none, for 128 guilds, oldest evicted. The message is addressed when `mention_roles` contains that role. Only the bot's own managed role counts, so several Dekopon applications in one server each answer only their own. In the text the session sees, the role mention is rewritten to the bot's user mention, so stop words and the prompt read both pills alike. The lookup has a two-second deadline and honours the REST cooldown; one that does not answer leaves the message addressed by `mentions` alone and logs `gateway_role_unresolved` at debug. A bot removed and reinstalled gets a new role, which the gateway learns at its next restart.

To turn on `messageContent`:

1. Developer Portal → your application → **Bot** → **Privileged Gateway Intents** → turn on **Message Content Intent** and save.
2. Set `messageContent: true` on the `discordGateway` transport and restart.

If the Portal toggle is off, Discord closes the connection with 4014 and the transport stops at startup with "Discord refused the requested gateway intents (close 4014)", naming the toggle. It does not retry. The toggle also governs `platform` recall, which reads other people's text over REST whatever `messageContent` says.

Whether a member is offered the role in autocomplete is server configuration:

1. Right-click the server icon in the left server list → **Server Settings** → **Roles**. The settings sidebar lists Server Profile, Server Tag, Engagement, Boost Perks, Emoji, Stickers, Soundboard, Members, Roles, and Invites.
2. The Roles page shows a **Default Permissions** card ("@everyone • applies to all server members") above the role list. The bot's role is in the list under the application's name, with a shield icon and 1 member.
3. Opening the role shows a yellow banner: "This role is managed by an integration: Dekopon. It cannot be manually assigned to members. You can remove the integration to remove this role." Its tabs are **Display**, **Permissions**, **Links**, and **Manage Members (1)**. Lower on **Display** is **Allow anyone to @mention this role**, with the note "Members with the 'Mention @everyone, @here, and All Roles' permission will always be able to ping this role."
4. **Default Permissions** opens **Edit Role — @everyone**. On its **Permissions** tab, search "mention": under **Text Channel Permissions** is **Mention @everyone, @here, and All Roles**, described as "Allows members to use @everyone (everyone in the server) or @here (only online members in that channel). They can also @mention all roles, even if the role's 'Allow anyone to mention this role' permission is disabled."

A member is offered the role when its own **Allow anyone to @mention this role** is on, or when the member holds **Mention @everyone, @here, and All Roles**: through @everyone, through another role, or through a channel's permission override (Edit Channel → **Permissions**). The server owner and any **Administrator** role hold every permission, so they are always offered it. Without `messageContent`, turn the role toggle and the @everyone permission off, and check channel overrides, so members see only the bot user. With `messageContent: true`, either pill wakes the bot and the toggles do not matter.

When someone mentions the bot and nothing happens:

- **Which @Dekopon was picked?** The bot user carries the **APP** badge; the role does not.
- **A role mention without content?** It needs `messageContent: true` on this transport and **Message Content Intent** on in the Portal.
- **Keep the role out of autocomplete** with the two toggles and the channel overrides above.
- **Read the evidence.** Each Discord drop is a `gateway.message.dropped` log event with `drop.reason`, and the message's `transport.receive` span carries the same word. `content-withheld` with `mention.roles = true` is a role mention Discord sent without its text. A message that arrived unaddressed logs `gateway_message_ignored` with `reason = not-addressed` at debug. See [Observability](observability.md).

### Telegram long polling

`getUpdates?timeout=50&offset=N` blocks server-side and returns as soon as anything arrives, so waiting costs one idle connection. **The poll is the wakeup and advancing `offset` is the acknowledgment** — there is no separate ack call and therefore no ack-before-work problem. The offset advances past every update, including ones the daemon chose not to route, or a filtered bot message would return forever.

Messages from bots are dropped. A private chat is a direct message; a group is a channel. Subject: `telegram.<user id>`. A `message_thread_id` is a thread coordinate only when the update also carries `is_topic_message`: a forum topic becomes the distinct canonical conversation `<chat>:topic:<id>` and the reply carries that same thread ID, while a plain reply — which Telegram stamps with `message_thread_id` too — stays in the chat's own conversation. A topic id that is not a positive integer is an envelope the Bot API would never mint: it drops that one update with `drop.reason = malformed-envelope` rather than failing the poll, because the offset has already advanced past the whole batch and every update behind it would be abandoned with it.

`sendMessage` refuses text over 4,096 UTF-16 code units, which is half the gateway's own outbound bound, so an answer is split losslessly across sequential messages the same way Discord's is. Only the first quotes the incoming message; the topic identifier goes on every one, because it is what keeps a continuation in the same forum topic.

Telegram's optional `message_thread_id` is preserved consistently in admission, conversation
identity, replies, generated-photo uploads, and liveness, so a forum-topic pulse cannot appear in
another topic. Generated PNGs use `sendPhoto`; text up to Telegram's 1,024-unit caption ceiling is
accepted with the first photo, while longer text follows as losslessly split `sendMessage` calls; a
failure after any accepted part is partial delivery. With
`indicators.mode: native`, an authorized session sends `sendChatAction(action=typing)` and renews
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
message type, wrong-destination, and self/echo messages are acknowledged and ignored. Text and image messages
use signed `messages[].from` both as reply target and as the sole identity source; profile names,
display phone numbers, message text, WABA IDs, and phone-number IDs cannot assert the sender. A
message is answered only when one of the delivery's own `contacts[]` names that sender in `wa_id` —
an individual message is the shape where exactly one does, while a group payload names the group in
`from` and matches none of them, with `group_id` and `participant` kept beside it as the two checks
that say so outright. A shape Meta adds later that this gateway cannot answer is therefore dropped
rather than answered as though the group were a person, each dropped message recording its own
`gateway_message_ignored` reason because one receive span covers the whole delivery.
Canonical subject is `whatsapp.<wa_id>`. The WABA, receiving phone number, and sender remain in the
transport-derived chat scope as `<waba>:<phone-number-id>:<wa_id>`.

The handler enqueues one bounded delivery before returning HTTP 200. One delivery carries at most
128 text/image messages, and the queue admits at most 512 messages across 64 delivery slots. The
200 returns before the turn starts, so Meta only retries a delivery whose 200 was lost in the
network, not one that is merely slow to answer; with no other application subscribed to the WABA,
that is the only remaining duplicate source. A redelivered message that arrives while the original
turn is still running is admitted as a steer on that running session rather than a second one; one
that arrives after the original turn has completed starts a fresh session and gets a second reply,
the accepted cost of deleting the WhatsApp dedup ring. A crash after the 200 but before queue drain
loses the accepted work. Queue saturation returns 503 so Meta can redeliver.

PNG/JPEG photos carry one lazy media-ID reference; webhook URLs are ignored. Captions become the
bounded user text, including an absent/empty caption (the session then receives the reference note).
No download occurs before routing and fresh session authorization. An image-capable route model
(`modalities: [image]`) is required for model image inspection. To edit, install a handle-aware
external image provider and the asset command provider in the broker, with narrow capabilities and
credentials there. References such as `{"images":["chat-asset:1"]}` stay unchanged on proposals;
the gateway passes the referenced descriptor. Sending the attached result requires a separate
`asset.send` grant, preferably restricted to the asset provider. The image credential never enters
the gateway.

At fetch time, Graph `GET /{version}/{media-id}?phone_number_id={phone-number-id}` resolves the
opaque ID. Metadata JSON is capped at 16 KiB; media ID, MIME, and numeric/string `file_size` must
match the source and download. Only exact HTTPS `lookaside.fbsbx.com:443` with path
`/whatsapp_business/attachments/` may receive the bearer token for download. Userinfo, fragments,
other hosts/ports/paths, redirects, and ambient proxies are refused. This is a conservative support
policy, not a claim that Meta guarantees an exhaustive CDN list; unknown CDN URLs fail closed.
The process-owned client resolves only Graph and that host, with a 5-second DNS deadline and at
most 32 public addresses bound to the actual connection (no second unchecked lookup). Each Graph
or download request has a 15-second deadline; session admission, the four model fetches per turn,
the route's capability-call budget, and each invocation's five input descriptors / 40 MiB decoded-byte
ceiling bound concurrency and total work. Downloads enforce the smaller of the caller's bound and
5,000,000 bytes while streaming, then verify the declared length and PNG/JPEG signature. There is
no image decoding, dimension/color-space guarantee, transcoding, URL caching, or automatic retry.

Queued PNG/JPEG replies use multipart `POST /{version}/{phone-number-id}/media` with
`messaging_product=whatsapp` and a gateway-named `file` part carrying the declared PNG/JPEG content type, then an ordinary
image message with `image.id` (never a model-selected URL). The same 5,000,000-byte ceiling applies
to decoded upload bytes, independent of identity/base64 storage. All outputs are preflighted before
uploading or sending any reply part; retention counts stored bytes while invocation limits count decoded bytes. Text of at most 1,024 Unicode scalars captions the first image; longer text is sent in full as
split text after all images, and empty captions are omitted. Upload acceptance alone is not message
acceptance, and a message ID is not proof of human delivery. There is no media deletion subsystem;
Meta's uploaded-media retention applies.

Replies are bounded JSON POSTs to the pinned
`https://graph.facebook.com/{version}/{phone-number-id}/messages` endpoint with the gateway-held
bearer token. Redirects are disabled, responses and time are bounded, and Meta error bodies never
reach chat or logs. WhatsApp accepts 4,096 Unicode scalar values per text message and the session's
own outbound bound is 8 KiB, so a long answer is split at a line boundary where one exists and sent
as consecutive messages rather than truncated — the same rule the Discord transport follows. A
failure after the first chunk is `partial-delivery`: the answer arrived in part, the underlying
service category is logged once as `gateway_whatsapp_reply_partial`, and no delivered turn is
recorded. No send is retried: a timeout after request transmission is outcome-unknown and blindly
resending could duplicate a visible answer. After Graph accepts every image and text chunk, the signed inbound
message ID becomes the service-typed delivery identity for optional durable chat memory, bound to
the WABA and receiving phone number in the attested scope. Failed or outcome-unknown replies record
no delivered turn. Free-form replies remain subject to Meta's customer-service window; there is no
template fallback.

Refusals are visible without being a megaphone. Every refused request emits
`gateway_whatsapp_webhook_refused` with a stable `reason` — `unsigned`, `signature`, `oversize`,
`malformed`, `saturated`, `timeout`, `verification`, `unavailable` — its HTTP status, and nothing
about its content. A stranger decides how often those happen, so each reason is emitted at most once
a minute carrying the number of refusals it stands for: a wrong app secret is one obvious line, and
a flood is one line a minute too. A failed `accept()` is classified rather than treated as the end
of the listener: a dead connection is debug-level and ignored. Descriptor or buffer exhaustion
is warned and ends the listener, as does an unusable listening socket (with
`gateway_whatsapp_listener_stopped`); the shared recovery layer rebinds it with bounded backoff.
The old listener and its connection tasks are dropped before rebinding.

Video, documents, stickers, templates, interactive messages, reactions, progress messages, status processing, business-management
APIs, embedded signup, webhook multiplexing, and daemon TLS termination are outside this transport;
the project-wide list is [non-goals](design.md#non-goals). See
[`../examples/whatsapp/`](../examples/whatsapp/README.md) for placeholder-only setup.

### Local development transport

An owner-only (`0600`) Unix socket under a private parent directory, with `dekopon-brokerd`'s socket hygiene: the parent must be an owner-owned directory with no group or world access, an existing socket is replaced only if it is already private and single-link, and the guard removes only the exact inode it created. Line-delimited JSON in, line-delimited JSON out on the same connection:

```console
$ nc -U /path/to/gatewayd-dev.sock
{"subject": "tel.16034700182", "conversation": {"kind": "channel", "id": "ops"}, "text": "what changed today?"}
{"reply": "Nothing external. Two read-only capability calls."}
```

Text-only output keeps that exact shape. Provider attachments add an `images` array containing the
gateway-owned `filename`, `mediaType`, and base64 `data`; the field is absent otherwise. The local
line can therefore approach the base64 expansion of the 8 MiB decoded bound and remains a
development protocol rather than a compact production transport.

**This transport trusts its local caller to declare a subject.** That is the whole point of it — it exists so a developer can drive a routed session without a Slack workspace — and it is why it is a development tool rather than a production transport. It grants nothing by doing so: the declared subject is only a claim carried into the broker's chat-attested `invoke`, and the broker needs an attestor grant covering that namespace plus an owner-controlled mapping before it resolves to a principal. Its `0600` mode keeps it reachable only by the owner's UID, the gateway's local trust domain.

On the default private scope, a declared subject also selects history **inside that configured local transport**. A caller can replay compacted exchanges previously created under the same local transport, agent, direct-message identity, and declared subject, but cannot alias Slack, Discord, or another configured transport because the transport component differs. On explicit shared scope the subject is intentionally absent from the state key, so every authorized caller of that local route shares its one local direct-message conversation. No authority moves — the broker decides every invocation for itself — but text does, which is a second reason this socket is `0600` and a development tool.

The gateway-authored participant label says `authenticated participant` in every scope across transports. Here it means the participant claim accepted on a fresh broker leg from the owner-UID-authenticated local caller; it does **not** mean the development socket independently authenticated the declared person. The line names its own `conversation` — `{ kind, container?, id, thread? }`, defaulting to `{ kind: directMessage, id: dev }` — which makes this the one transport that can produce every kind, and therefore the one a route table, a liveness override, or a memory key can be exercised against without a chat service. Every line on this socket is addressed, because the owner-only socket mode is the authentication and a line-delimited JSON request carries no mention syntax for the routing loop's grammar to find — an unaddressed line would be dropped before it reached a route, which would leave the one transport that can produce a `channel`, `thread`, or `groupDirectMessage` unable to reach one.
