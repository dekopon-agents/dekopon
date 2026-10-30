# Gateway assets and media

How the gateway accepts files from chat, hands them to providers as assets and delivers outputs back.

## Multi-message media inputs

The shared gateway collects **media-first** inputs after routing and address checks and before
session admission. Text that arrives first is admitted immediately.

- **WhatsApp:** `debounceMs` is the quiet interval (default **5000ms**) and `debounceMaxWaitMs` the
  hard maximum from the first media receipt (default **15000ms**), both unsigned 32-bit
  milliseconds (`0..=4294967295`). `debounceMs: 0` disables collection, and the maximum is then
  unused. When enabled, the maximum must be at least the quiet interval. Invalid types,
  negative or out-of-range integers, incompatible enabled combinations and unrepresentable timer
  deadlines are refused at configuration load. Accepted later photos, captions and standalone text
  extend that actor's quiet deadline, never beyond the first receipt plus the maximum; refused
  inputs do not extend it. These are heuristic bursts: webhook payload boundaries are not album
  identities.
- **Telegram:** a separate fixed **3-second** window applies only to authenticated
  `media_group_id` members, because Telegram supplies no group-end marker. Member captions join;
  ordinary text and ungrouped media stay immediate and cannot claim membership. Different native
  groups never merge: a competing group for the same pending actor and audience is visibly
  refused. WhatsApp's setting never changes Telegram timing. In groups and topics, an unaddressed
  native member inherits addressing only from an already-open addressed lead with the exact same
  actor, route, full scope, reply audience and native group ID. Members arriving before that lead,
  or after expiry, stay ignored unless individually addressed; there is no pre-address buffer or
  grant inheritance.
- **Slack, Discord and local:** native attachment arrays are indivisible and immediate, and local
  text is immediate. Slack and Discord refuse a native envelope over 10 attachments rather than
  accept a truncated subset.

**Isolation and bounds.** Collection is isolated by canonical authenticated subject, exact bound
route, configured transport, complete conversation/container/thread and reply audience, even on
shared-history routes. Each batch holds at most **8 envelopes, 32 assets and 16 KiB of rendered
text**, including the source-boundary and caption-presence labels added for multiple inputs. A limit
rejects only the incoming envelope, visibly and with a traced cause; members already collected stay.
Native arrays are never split. Each compatible actor key has at most one collecting batch, and at
most `sessions.maxConcurrent` batches collect at once, independently of execution permits.

**Admission.** At quiet expiry or the hard deadline, whichever comes first, the batch attempts
normal admission. For a running conversation with mailbox room, batches follow the session's
steering rule: on mapped shared or one-shot channels another authorized native-chat participant
joins the turn; elsewhere the same sender steers and another sender queues. After cancellation or
the turn's seal, both queue. Steers and follow-ups share a 100-item mailbox. A full mailbox or
exhausted execution capacity makes a batch eligible for a busy reply even when `replyOnBusy` is
false, subject to the bounded refusal-reply capacity and transport delivery. Later media starts
another bounded collection; standalone text follows ordinary steering and admission. WhatsApp
collection waits at most its configured maximum and Telegram's at most 3 seconds; queueing,
inference and transport time come on top. Collection does not guarantee that every group member
joins one turn, and nothing replays or rolls back paid effects.

**Authority and traces.** New turns receive fresh broker authorization and generation selection
after admission; a same-sender steer joins the running turn's existing leg and asset access. Other
participants on a mapped shared or one-shot channel receive fresh authorization before joining its
asset access. Collection itself fetches no assets, contacts no model or provider and publishes no
progress. One lead message owns progress, reply target and native delivery identity; other message
IDs are not fabricated into an album ID. An authenticated stop removes only that actor's pending
work and keeps the ownership rules for active sessions. Shutdown discards pending batches without
starting them. Original receipt traces keep their input and terminal disposition. Ordinary collected
executions export causal links to every constituent; consumed steers link their constituents only
to the admission span, not the running execution (see [observability](observability.md)).
Collection does not change asset leases, history scope or generation fences.

## Asset handles and delivery

The gateway's conversation-generation table is the only asset inventory. The model learns it from
the note on its newest turn, which lists every attachment that fits, newest first, and says how to
read it: `fetch_chat_asset` for an image or document, `asset cat chat-asset:N` for other text when
`asset.cat` is granted. Every reachable file has a number, including files the model cannot show,
and older files omitted for space stay addressable by number.

- **References:** to hand a file to a provider command, write `chat-asset:N` as its own unquoted
  word. Quoted references, escaped text, variable values and references inside larger words are
  plain text. Builtins never read attachments; use `asset cat chat-asset:3 | head` to pipe file text
  into a builtin.
- **Invocation:** each provider command sends one `Run` frame and receives one `Ran` response. The
  gateway resolves and pins at most five distinct references, numbers indices by first occurrence,
  and sends only those metadata rows and read-only, close-on-exec descriptors. Repeated references
  share one pin and descriptor. Pins last until the response or cancellation. Data URLs in argv,
  including the value after the first `=`, are refused before submission. The broker parses the
  command word and authorizes its proposal through the usual subject, Cedar, grant, secret-use and
  HTTP checks. An unquoted literal `<word> --help` uses the cached page without a frame or
  capability-call charge. Gateway-built proposals use `Invoke` with no inventory rows or asset
  input descriptors; an `Invoke` carries at most five metadata rows.
- **Attach:** a successful provider `attach` returns a descriptor and typed metadata. The gateway
  numbers it at intake, charges its fstat size to the disk LRU, and appends a bounded stderr note
  naming the reference, declared content type and stored-byte count. The declared label is
  authoritative; a bounded decoded-prefix disagreement produces one metadata-only event. With
  `sessions.journal`, admission copies provider outputs into the content-addressed store under
  `sessions.assets.path`; generated files stay session-only and are discarded at restart. Without a
  journal, outputs stay on temporary disk without copying and are reclaimed by closing their last
  descriptor. Both modes share `sessions.assetRetentionBytes` (default 1 GiB). All descriptor reads
  are positional. A trap, denial or timeout admits no asset effects.
- **MP4:** outbound `video/mp4` files are named `asset-N.mp4`, and their bytes bypass the model: an
  MP4 is not a model-readable image. Slack, Discord, the local transport and test-only fauxlack
  deliver the named file; WhatsApp and Telegram refuse it before upload.

**Attach is not send.** An explicit broker-authorized `asset.send` queues a file for this turn's
reply. Four newly sent assets are permitted per turn. Sent state lasts as long as the table entry:
a duplicate send, even in a later turn, is a no-op, and a sent file cannot be removed. Removing an
unsent asset closes its unpinned retained file and releases its accounting. No failure implicitly
retries or clears sent state. Empty answer text still delivers queued files; failed or cancelled
turns deliver none. Delivery disposition is logged as `agent.asset.send`; a failure adds one bounded
gateway notice to the next turn. An output retention refusal names the failure without suggesting a
repeat of an already-executed paid capability.

**Limits.** **8 MiB decoded per asset, five distinct references or attached outputs and 40 MiB
decoded per invocation**, and the configurable 1 GiB default disk budget. These are separate from
model-facing fetch limits.

- An 8 MiB asset stored as base64 may take up to 11,184,812 bytes; identity stays at 8,388,608.
  Disk retention and broker in-flight spool budgets account for actual stored bytes.
- Live generations have no per-conversation count cap or idle expiry; one-shot inventories expire
  after one idle hour. Resolving bytes updates LRU recency.
- An unknown inbound length shows as `size unknown` in the inventory note and as a null stored-byte
  count in metadata rows, never as an empty file; reported or fetched lengths, including a genuine
  zero, stay numeric. Building the inventory never fetches a file just to learn its size.
- Evicted files keep their last known length; a session-bound use refetches inbound bytes with
  fresh authorization. Generated files are never regenerated.

**Delivery.** Each adapter uses its native path: Slack external upload, Discord multipart Create
Message, Telegram `sendPhoto`, WhatsApp Graph media/image messages, and local base64 JSON. Each
upload reads a bounded file; the shared native codec converts base64 storage to raw upload bytes,
and local encodes into its output line. There is no image-format conversion.

| Adapter | Supported declared content types |
|---|---|
| Slack, Discord, local | Any concrete syntactically valid media type; no wildcards |
| Telegram, WhatsApp | `image/png`, `image/jpeg` |

Unsupported formats refuse before uploading, and WhatsApp also keeps its 5,000,000-byte ceiling.
These are adapter-supported sets, not claims about every upstream feature, so route instructions
should name the adapter's accepted types and tell the model to plan a converter when necessary.
Filenames are gateway-generated. Only authenticated reply coordinates select the target; a partial
delivery fails the reply and suppresses durable recording.

Slack installations need `files:write` beside the reply and read scopes. Discord bots need
**Attach Files** beside View, Send, Read History and Send in Threads. Telegram needs no additional
bot permission.

## Chat assets

A screenshot is part of the message that carried it. Slack, Discord, Telegram, and WhatsApp deliver it by reference rather than by value, so the gateway resolves that reference in order to hear the whole request. Slack, Telegram, and WhatsApp require the bot token they already terminate here — and on Slack the `files:read` scope, without which Slack withholds the file's id and URL and the upload is reported as one the gateway cannot open; Discord CDN downloads do not receive it. This grants no policy, no provider credential, and no way to write anything.

What it does not do is read every file that arrives. Bytes cost tokens on every turn they appear in, and most turns do not need them. So each attachment is named in the fitted inventory note and fetched when the model requests it through `fetch_chat_asset` or hands its reference to a provider command:

```text
what does this say?

[gateway: files in this conversation
  chat-asset:2 — recording.mov (video/quicktime, 41.3 MB) — not readable here; a command can take it as chat-asset:2
  chat-asset:1 — screenshot.png (image/png, 214 KB)
  Call fetch_chat_asset with the number to look at an image or document.]
```

The model then calls `fetch_chat_asset(1)`. Because a tool result cannot carry an image — Chat Completions types a `tool` message's content as a string, and the Responses API types `function_call_output.output` the same way — the answer arrives as two messages: the tool result says the asset follows, and a `user` message carries the bytes. That shape is the only one both wire formats accept.

Whether a session offers `fetch_chat_asset` is decided once, when it starts: every session whose conversation generation is live and whose gateway retains assets (`assetRetentionBytes` above zero) carries the tool on every turn, including turns with nothing to fetch, so the tool list stays part of the cached request prefix. A file that arrives by steer or from a provider mid-session is fetchable through it. A call that names nothing addressable — no such file, a retired generation, an evicted generated file — gets a refusal sentence as its tool result and no bytes.

- **Numbering follows the exact history audience and live generation.** `chat-asset:5` means at most one file in that generation, which is what lets a follow-up three turns later resolve. Its monotonic sequence survives LRU removal while the transcript generation remains live, so a removed number cannot alias a newer file. Grant/empty-grant invalidation, idle replacement, and conversation-capacity eviction close the generation's asset fence; stale sessions cannot enumerate, publish, or fetch through it, and a replacement generation may safely number from one again. Private participants and different agents/transports/conversations cannot enumerate or fetch one another's attachments. Participants on an explicitly shared route share the live attachment inventory as well as the replayed arrival lines; that disclosure is part of choosing shared scope. Numbers are assigned by the gateway rather than by a transport; one-shot IDs are process-monotonic so expiry cannot alias an old reference.
- **Every prompt names every attachment that fits**, newest first, with the new ones marked and one omission line naming the oldest range left out for space. A reference line is the only way a model learns a number exists, and one confined to the turn that introduced it goes unreachable as soon as ordinary chatter pushes that turn out of the replayed history window — while its generation is still live. One-shot inventories alone retain files for up to another hour of idleness. The inventory note is not recorded: history keeps each turn's text plus one `[gateway: attached chat-asset:N — name]` line per file that arrived with it, the same line platform recall writes, so replayed turns do not each carry a stale copy of the inventory.
- **The reference line is what history remembers, not the bytes.** It is a few dozen bytes, so it replays inside the conversation byte budget instead of evicting real conversation the way a base64 screenshot would.
- **A file that cannot be shown is named anyway.** A media type outside the allowlist, a model with no image modality, or a file Slack withholds entirely all produce a line saying so, which the model can answer around instead of denying a screenshot that plainly exists.
- **Only the media types a model can actually accept are offered.** Images: `image/png`, `image/jpeg`, `image/webp`, `image/gif`. Documents: PDF, plain text, Markdown, CSV, HTML, XML, JSON, RTF, and the Word, PowerPoint, and Excel formats. A chat service imposes no allowlist on uploads at all — a 700 MB screen recording is a legal attachment — so the narrow end of that intersection is the one worth enforcing. A spreadsheet is parsed to its first thousand rows per sheet, which is worth knowing before concluding a model ignored the bottom of one.
- **A route's model has to opt in to images.** `modalities: [image]` on a model entry; the default is text only, because an OpenAI-compatible endpoint is very often a small local model that will either error or invent an answer when handed an image. Documents need no modality: a PDF is a parsed attachment to every endpoint that accepts one at all, so gating it on vision would refuse it to a model perfectly able to read it.
- **Bounds.** 8 MiB per attachment, enforced while the response streams rather than after it, because a reported size is sender-influenced and a chunked response need not declare a length. Fetched assets carried in one model request total at most 40 MiB decoded; a crossing fetch refuses and names the unseen ID without discarding earlier images. A textual file is clamped again on the way into the prompt, at the same 256 KiB a script's output is capped at, with a trailer saying where it was cut: the 8 MiB ceiling is sized for images on the wire, and that much `text/plain` is roughly two million tokens — enough to come back from the provider as a context-length rejection. Every one of these refuses in a sentence the model reads and can answer around, never by failing the session. With retention smaller than the carried request, a crossing fetch can evict its own file from the conversation before refusing; a later session-bound use can refill an evicted inbound file; generated files cannot be regenerated.
- **Redirects.** The HTTP client refuses redirects globally so a bearer token is never forwarded by policy. Slack's `url_private_download` genuinely redirects to its own file host, so that transport follows exactly one hop and re-attaches the token by hand. Both URLs are checked on one rule, at the single place the token is attached: the URL the event supplied and the `location` after it are parsed rather than prefix-matched, and each must name an allowlisted Slack host over HTTPS on the default port with no credentials in the authority. A refused URL fails the fetch before any request is made.
- **Ambient proxies.** Every transport's client is built from one `credential_client` shape that sets `no_proxy()`, so an exported `HTTPS_PROXY`, `HTTP_PROXY` or `ALL_PROXY` carries no Slack, Discord, Telegram or WhatsApp token — nor the messages and files it authenticates — through a host nobody named to Dekopon. There is no flag to opt back in; a chat service reachable only through a proxy is unreachable.
- **Gateway-owned disk LRU, weak references.** Uploads download on first use and refill on use after eviction; descriptor-backed outputs join
  the same inventory with capability/invocation provenance. A bounded typed-output note names each
  retained reference; same-session and later-turn edits can address it without carrying bytes in
  the shell, prompt transcript or history. Retention and delivery are separate acts.
- **One configurable residency budget.** `sessions.assetRetentionBytes` defaults to **1073741824**
  (1 GiB), shared across conversations and generated/inbound files. Zero disables assets and removes
  `fetch_chat_asset`; it is never unlimited. Content-identical stored representations share one file
  and one charge. Empty files charge one byte. The 8 MiB decoded per-file and 40 MiB per-request
  limits remain independent safety limits.
- **History owns durable bytes.** With `sessions.journal`, `sessions.assets.path` is required and
  stores SHA-256-named files plus a restartable index. `assets: null` means absent; `{}`, an empty
  path and an unresolvable path refuse configuration. The assets directory must neither equal,
  contain nor sit inside the journal directory, including resolved aliases. Relative paths resolve
  against the configuration file. Uploaded files already downloaded for use become resident again
  when their conversation is recalled after restart, without another download. Arrival alone never
  downloads. Generated outputs are session-only, share this budget while live, and are not restored.
- **Use means consumption.** Successful scoped consumption refreshes persisted recency; listing,
  replay and `pin(Recency::Keep)` do not. Admission evicts unpinned re-downloadable inbound files
  first, then generated files, least recently used within each class. Unloaded conversations count
  toward the budget and supply eviction candidates after restart. An oversized file or insufficient
  unpinned capacity evicts nothing. Failed storage preserves a not-stored reference; attaching an
  output does not fail because its bytes could not be stored. Failure surfaces when it is used.
- **Journal retention owns deletion.** Sweep, record compaction, forget and seal retention release
  references; the last retained record naming a digest releases its file and index bindings. Shared
  digests survive while any conversation still names them. One-shot idle expiry and generation
  retirement release in-memory rows and pins without deleting journal-owned bytes. Fence-less
  inventories still expire when idle. The conversation ceiling no longer evicts asset inventories.
- **Concurrent uses share a download.** A cold or evicted inbound attachment downloads in a store-owned task, at most two
  at a time, under the 60-second fetch deadline. Model fetches and provider inputs that ask for the
  same attachment share that download; when the last of them is cancelled, so is the download, and
  the attachment can be requested again. History refills share one 60-second deadline per request.
  A platform-gone response is cached on the live row; later uses refuse without another download.
- **Refill happens on use.** A session-bound use of an evicted inbound attachment downloads it again through
  the existing platform fetch path, under the live generation and current grant. Generated files
  are never regenerated. Requested provider inputs are all resolved and pinned before submission;
  one missing input refuses the entire call. Before request encoding, historical model attachment parts are resolved through the same
  session-bound path within the request byte budget and pinned through encoding. Parts still
  missing bytes become an explicit gateway-authored release notice.
  Actual descriptor IO/corruption errors remain errors, not fabricated release notices. Scoped
  inventory state and at most 1024 release tombstones distinguish released, unknown and revoked
  IDs. Persistent IDs remain monotonic within the generation; one-shot IDs are process-monotonic
  to avoid reuse after one-shot inventory expiry.
- **Use refusals name the cause.** Both provider-input refusals and `fetch_chat_asset` tool text
  name `evicted` for missing generated bytes, `platform-gone` for platform responses 404, 410,
  401 or 403, `revoked` for a retired access generation, and `store-failed` for a not-stored row
  or failed re-admission. Other transport failures and timeouts remain `unavailable`; the row
  stays evicted and a later use retries. Attaching an output remains silent on store failure.
- **Gateway-private storage.** Store files are 0600 inside 0700 directories owned by the gateway
  UID. The index contains digest, row identity, origin class, state and recency, never attachment
  sources or platform locators. Locators remain in the journal. Reads stay session-bound and the
  broker receives descriptors, never a store path. Without a journal, attachments continue to use
  process-lifetime private temporary files; no durable assets directory may be configured.
- **Storage failures are not successful empty images.** Capacity exhaustion, OS IO categories
  (including disk-full), and changed/truncated length are sanitized failures, never paths or bytes.
  An unlinked file still reads through its original descriptor while a lease owns it. No storage
  failure triggers a network fallback or paid-call retry. An evicted inbound attachment is
  refetched only on session-bound use; resident-only probes never fetch or touch recency. A provider-result spool refusal
  explicitly says the capability already executed and its attachment was not delivered; existing
  complete/partial transport acceptance rules remain in force. Outbound transports transfer all
  reply leases to a trace-contextual blocking task before delivery; reads and normal disposal do
  not occupy async workers. Cancelling the wait does not stop that task: it releases ownership
  when IO returns. Blocking model consumers retain the synchronous API. Local image answers use
  a separate reply instead of in-place progress/stream finalization; text-only answers still finalize
  in place. An unpolled transport future or runtime shutdown before queued work starts can still
  drop leases on the calling thread; this is not a general asynchronous cleanup service.
- **Disk backing is an operator requirement.** The chart uses disk-backed gateway `/tmp`, separately
  sized by `volumeSizes.gatewayTmp`, and mounts durable assets on the state claim when enabled; broker scratch stays tmpfs. Other installations must provide
  a disk-backed temporary directory, not a memory-backed `emptyDir` or `TMPDIR`. Existing deployed
  manifests and overrides need separate rollout verification. This removes retained bulk bytes,
  not all peak allocations: bounded downloads/decodes, textual conversion, base64/JSON request
  bodies, broker frames and multipart upload buffers still allocate temporarily; filesystem page
  cache is also outside this claim. No RSS measurement is implied.

- **Resolving a reference differs by transport.** Slack carries a private download URL on the event itself. Discord carries a signed CDN URL plus the source channel/message/attachment IDs; the CDN request carries no token, and an expired 401/403/404 URL is refreshed by re-reading that exact message through pinned Discord REST before retrying the same attachment ID. Telegram carries only a `file_id`, so a fetch is two calls: `getFile` turns the handle into a path valid for about an hour, and the bytes live under `/file/bot<token>/<path>` rather than the method prefix. The round trip happens at fetch time, which is also when that path is freshest.
- **Discord specifics.** Photos and arbitrary files share the attachment object, retaining their sender-controlled filename, optional media type, and reported size. Production downloads accept only HTTPS `cdn.discordapp.com` or `media.discordapp.net` URLs, reject credentials and redirects, and enforce the byte ceiling while streaming.
- **Telegram specifics.** A photo arrives as the same image at several sizes and the largest is the one used — a model asked to read text in a screenshot cannot read a 90-pixel-wide copy. Telegram reports no media type for a photo, so `image/jpeg` is inferred, which is what the Bot API re-encodes every photo to; a file sent as a *document* keeps its own bytes, name, and declared type. Words on an upload arrive in `caption` rather than `text`.
