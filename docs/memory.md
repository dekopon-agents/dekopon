# Memory

An agent remembers a conversation by replaying its transcript into the next prompt. Summaries are
written before they are needed, and the replayed history changes as rarely as possible, so the
provider's prompt cache keeps matching.

## The journal and its summaries

On a route that recalls from `sessions.journal`, the gateway appends one record per turn after the
reply: the person's text, the answer, reasoning, and every tool call with its result. Records are
plaintext files under the gateway's UID, with no encryption at rest, evicted whole by age and
per-agent size.

A background engine then writes each record's summary parts through the route's model or
`summaries.model`. Each part adds detail to those before it, so `summary-1` is a gist and the last
part the fullest. No reply or turn waits on them, and a failed part stays failed, never faked.
Summary calls are metered but never admitted against the agent's token budget, which does not cap
them.

## The ladder

The newest records, the hot window, replay as originals, tool results cut to fit. An older record
replays at one rung: the original, then each summary from fullest to shortest, then a metadata line
(number, time, size, and each tool call's outcome). Summary and metadata rungs are text in one
leading `[gateway: earlier conversation]` message, never executable tool calls.

An older record starts at its pin, else the decider's choice, else its fullest summary, else the
original. It changes rung only when:

- a repack runs;
- it leaves the hot window;
- the model changes its pin.

A repack runs when the history outgrows the route's byte or turn bounds. It keeps the newest
`maxTurns` records and steps down attachments, then older records one rung at a time, then hot
records whole, until the history fits three quarters of `maxBytes`. A record with no rung left is
omitted and the model is told which, never silently lost.

**Why the cache stays warm.** Providers cache exact prefixes. A history trimmed from the front, or
summarized afresh each turn, changes the prompt's first bytes and makes every request a full-price
miss. Between repacks, everything older than the hot edge is byte-identical from one request to the
next, and a summary that finishes late waits for the next repack rather than rewriting a record in
place. A repack costs one miss, and with a hot window of four or more turns the quarter of headroom means
one never follows another. The per-conversation cache key is a routing hint, never isolation, and it
lets the model provider link a conversation's turns.

## Reading back and pinning

The model can read any rung of a retained record and pin the rung a record or attachment replays
at. A pin survives restarts but forces nothing: a repack can still step a pinned record down. Reads
are refused once a record has expired or its grant changed.

## The decider

`selection: {decider: …}` adds a second opinion to the ladder. At most once per turn, the gateway
asks a broker capability where the oldest undecided older record should start. It sees metadata only,
never text, and may choose only that
record's summaries or metadata. The broker authorizes the call like any other. The choice applies at
the next repack; a refusal or invalid answer leaves the ladder in charge.

## Attachment summaries

With attachment summaries on, the gateway describes each attached file it still holds, after the reply, through the summary model, and downloads nothing
for it. The attachment then replays as its description. Pinned to `native`, an image or binary
document replays as the file itself when the replying model accepts it; text documents have no
native rung.

The file reaches the summary model without the replying model asking. The description is untrusted
text that replays to everyone the history replays to, never carries the file's name or platform
locator, and is evicted with its record.

## Scope: who the history replays to

`scope: privateConversation` gives each authenticated sender their own history in a
conversation. `sharedConversation` drops the sender: everyone in that one group DM, channel or
thread shares it, and one participant's earlier prompt and answer replay to all of them. It is never
cross-agent or cross-conversation memory.

## Durable memory

The broker can host a storage provider on the chat-memory routes. When Cedar grants all three to the sender, the gateway records each turn after
the chat service accepted the answer, and the model may retrieve but never record turns. Stored
turns never enter a prompt on their own. Recording does not deduplicate, and there is no export or
encryption at rest.

`continuityPolicy: paranoid` starts a new durable-memory generation when the sender's effective
authority changes, even between turns whose GitHub facts or trigger a policy reads, though message
and probe turns share one; `relaxed` never does.

| `paranoid` [rotates](../crates/dekopon-broker/src/continuity.rs) on | Neither rotates on |
|---|---|
| capabilities, providers, storage access, HTTP authority, CA bytes | limits, timeouts, artifact digests, binding names, map revision |
| applicable request policy, secret references, sinks | inapplicable policy, labels |

## Retention

One byte budget spans journals and durable memory. A chat idle for the window, counting
wakes but not reads, expires whole. Over budget, the least-recently-active chat leaves both stores,
skipping chats in use, or the write is refused. Deletion is permanent.

## Memory never grants authority

History, summaries, descriptions and pins never reach the broker as policy input; each later
capability call is a fresh proposal authorized for the person asking now.
