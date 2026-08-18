# `dekopond` — the chat gateway and agent daemon

`dekopond` is the unprivileged half of the deployment boundary in [`design.md`](design.md): it connects to chat services, waits for a wakeup, routes each authenticated message to a named agent from the catalog, runs one bounded model session whose only tool is the sandboxed shell, and replies with the answer.

It holds chat bot credentials and model credentials — the things it needs to hear a question and to ask a model. It never holds a provider credential, a policy, or an authorization. Every effect a session drives is submitted to `dekopon-brokerd` as an on-behalf-of proposal naming the sender's canonical subject, and the broker alone maps that subject to a principal, decides what it may do, resolves credentials, and executes it.

**Status: Current.** Chat-transport wakeups, attested routing, bounded sessions, and persistent conversations are implemented and tested. A route is `oneShot` unless its configuration says otherwise, so a deployment that never writes a `conversation:` block behaves exactly as it did before this existed. A dedicated gateway UID remains **committed direction**, and agent memory that outlives a conversation is not designed at all.

Its dependency set excludes `dekopon-broker`, `dekopon-broker-host`, `dekopon-http-host`, and `dekopon-brokerd`, and CI rejects any of them appearing in the gateway's normal dependency tree — the same discipline already applied to `dekopon-run`.

[`../examples/rubber-stamper/`](../examples/rubber-stamper/README.md) is the complete worked
deployment: a Slack DM from an owner-mapped sender, five `gh` capabilities, a broker-injected
GitHub token, and the audit record naming the person who asked. Read it alongside this document —
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
  - name: tg
    kind: telegramLongPoll
    botTokenEnv: DEKOPOND_TELEGRAM_TOKEN
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

routes:                                       # first match wins, so order these deliberately
  - transport: scientist-slack
    match: { kind: channel, channel: c0123abc }
    agent: incident-responder                 # one named channel, its own agent
  - transport: scientist-slack
    match: { kind: channel }                  # any other channel the bot is invited to
    agent: xaviers-rubber-stamper
  - transport: scientist-slack
    match: { kind: directMessage }
    agent: xaviers-rubber-stamper
    model: local-qwen                         # optional; else the first model offering the agent's modelClass
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

Transports and models name **environment variables**, never values, following the precedent `dekopon-telemetry` set for OTLP ingest credentials. A variable name is validated as a name (`[A-Za-z_][A-Za-z0-9_]*`), so pasting a token where a variable name belongs is a startup failure rather than a token sitting in plain text while the daemon reports a missing credential. Missing variables are reported at startup **by variable name and never by value**.

### Startup fails closed

A gateway that starts and then refuses everything is worse than one that does not start. These are all startup failures:

- a route naming an agent the catalog does not contain, or one the catalog disables;
- an agent with no resolvable model — no `model` override and no configured model offering its `modelClass`, or no `modelClass` at all;
- duplicate transport names, duplicate model names, a route naming an unknown transport or an unknown model;
- a zero step budget, a zero capability budget, or zero concurrency;
- a transport endpoint override that is neither the one production origin nor a literal loopback `http://` URL;
- a `channel` written beside `kind: directMessage`. The field belongs to the other kind, and a decoder that shrugged at it would leave an operator convinced they had scoped a route to one channel while it claimed every direct message on the transport;
- a missing credential environment variable;
- an unreachable broker. `dekopond` makes one `capabilities()` call on the configured socket before connecting any transport and logs the capability count as `gateway_broker_ready`.

The `conversation:` block adds three more:

- a `persistent` route with a zero idle timeout, a zero turn window, or a zero byte window — the same rule a zero step budget already follows, because a bound of zero is a bound nobody meant to write;
- an idle timeout or a window bound on a `oneShot` route. The setting can never take effect there, and a setting that can never take effect is far more likely a mode typo than an intention;
- a zero `sessions.maxConversations`, which would make every history immediately evictable and turn a persistent route into an expensive one-shot one.

## Transports

### Slack Socket Mode

An app-level token opens `apps.connections.open`, which returns a `wss://` URL; a bot token answers through `chat.postMessage`. No public HTTP endpoint is needed, which is why Socket Mode rather than the Events API. [`../examples/slack/`](../examples/slack/README.md) has an app manifest requesting exactly the scopes and events this transport consumes, plus the token and identity-mapping walkthrough.

The protocol's one sharp edge is redelivery: Slack expects an acknowledgment within roughly three seconds and resends the envelope otherwise. A Dekopon session takes far longer than that, so **the acknowledgment is sent before any processing begins** — before parsing, before routing, before any model call. A bounded ring of 1024 seen `(channel, ts)` pairs absorbs the redeliveries that happen anyway across a reconnect.

- `disconnect` envelopes are routine (Slack rotates sockets on its own schedule) and trigger a reconnect with jittered exponential backoff capped at 60 seconds.
- Messages carrying `bot_id`, messages from the bot's own user identifier, and subtyped messages (edits, joins) are dropped. Both bot checks matter: another app's post carries `bot_id`, and this app's own post arrives with the bot's user identifier and no `bot_id` at all.
- `channel_type: im` is a direct message; anything else is a channel.
- Subject: `slack.<team>.<user>`, lowercased.
- A channel answer joins the thread it was asked in, starting one on the inbound message when there is none. A direct message has no thread to join and answering in one would hide the reply.
- An answer is posted in a Block Kit [`markdown` block](https://docs.slack.dev/reference/block-kit/blocks/markdown-block/), which carries the model's CommonMark unchanged and lets Slack render it. The `text` field is mrkdwn — a proprietary syntax where bold is `*one asterisk*` and a link is `<url|label>` — so an answer posted through it alone arrives with `**bold**` as four literal asterisks, and tables and task lists cannot be expressed in it at all. Translating in this process would be a second translation of what Slack is about to translate, so the gateway does none: the block gets the answer verbatim. `text` is still sent as the notification fallback, the one place blocks do not render. The block caps a payload at 12,000 characters, which the 8 KiB outbound bound already sits under.

### Telegram long polling

`getUpdates?timeout=50&offset=N` blocks server-side and returns as soon as anything arrives, so waiting costs one idle connection. **The poll is the wakeup and advancing `offset` is the acknowledgment** — there is no separate ack call and therefore no ack-before-work problem. The offset advances past every update, including ones the daemon chose not to route, or a filtered bot message would return forever.

Messages from bots are dropped. A private chat is a direct message; a group is a channel. Subject: `telegram.<user id>`.

### Local development transport

An owner-only (`0600`) Unix socket under a private parent directory, with `dekopon-brokerd`'s socket hygiene: the parent must be an owner-owned directory with no group or world access, an existing socket is replaced only if it is already private and single-link, and the guard removes only the exact inode it created. Line-delimited JSON in, line-delimited JSON out on the same connection:

```console
$ nc -U /path/to/dekopond-dev.sock
{"subject": "tel.16034700182", "channel": "dev", "text": "what changed today?"}
{"reply": "Nothing external. Two read-only capability calls."}
```

**This transport trusts its local caller to declare a subject.** That is the whole point of it — it exists so a developer can drive a routed session without a Slack workspace — and it is why it is a development tool rather than a production transport. It grants nothing by doing so: the declared subject is still only a claim carried into the broker's `invokeFor`, and the broker still needs an attestor grant covering that namespace plus an owner-controlled mapping before it resolves to a principal. Its `0600` mode keeps it reachable only by the owner's UID, which is the trust domain the broker socket already lives in.

A declared subject also selects a *history*. A local caller can therefore name a subject some Slack sender created and have that person's compacted exchange replayed into its own prompt. No authority moves — the broker still decides every invocation for itself — but text does, which is a second reason this socket is `0600` and a development tool.

Every local message is a direct message; channel routes are a chat-service concept.

## Routing

First match wins on (transport name, direct message or channel). The channel is optional: `{ kind: channel, channel: c0123abc }` claims that one channel, and `{ kind: channel }` claims **any** channel the bot is in. Unmatched messages are ignored with a debug-level event — bots see ambient traffic, and silence is the correct answer.

Leaving `channel` out exists because naming them does not scale. One route per channel means enumerating service-native identifiers and editing this file again every time somebody creates a channel, and until an operator notices and redeploys, the bot is silent in the new one while appearing to be deployed workspace-wide. An absent `channel` says "wherever I am invited", which is membership the chat service already controls.

**Declaration order is the whole precedence rule.** Routes are consulted top to bottom, so a named-channel route written above a catch-all keeps that channel for itself while the catch-all takes everything else — special handling in `#incidents`, the default everywhere else. Nothing sorts by specificity, deliberately: a hidden ranking is how an operator ends up unable to say which route answered.

In a channel the bot must additionally be addressed: `<@BOT_USER_ID>` on Slack, `@botname` on Telegram. **A route decides which agent answers; the mention decides whether anything answers at all**, and widening the first leaves the second exactly where it stood. A channel route that fired on every message would be noise and cost. On Slack the app is not even offered the traffic — the manifest in [`../examples/slack/`](../examples/slack/README.md) subscribes to `app_mention` and not to channel history.

### Being available in a channel is not authority

A route matching every channel widens no authority whatsoever, and an operator reading "available in all channels" must not read it as "available to everyone". Every session still opens an attested broker leg naming the sender's canonical subject; the broker still maps that subject to a principal, still requires policy permitting `agent.prompt` for it, and still refuses an unmapped sender before any model call is made. A catch-all route changes *where* the mapped people can reach the bot. It does not change who they are, and somebody the owner never mapped gets the same refusal in a catch-all channel that they would have got in a named one.

Nor do two people in one channel share a conversation. History is keyed on `(transport, the conversation identity, the sender's canonical subject)`, so the bot remembers each of them separately and remembers nothing of what it told the other — see [the key includes the sender](#the-key-includes-the-sender), which matters most in exactly the shared channel a catch-all makes easy to reach.

## Sessions

Each routed message runs one session. On a `oneShot` route — the default, and every route in a configuration that never writes a `conversation:` block — that session is entirely independent, and the `persistent` clauses in steps 3 and 4 are the whole difference the other mode makes:

1. **Admission.** A process-wide semaphore bounds what the daemon costs at once, and a per-`(transport, channel, thread)` in-flight set stops one conversation from queueing work on itself — what a person does when a bot seems slow and they send the same thing again. A rejected message gets `I'm busy — try again shortly.` when `replyOnBusy` is set, and silence otherwise.
2. **Authorization.** The session opens an attested broker leg with `capabilitiesFor(subject, agent)`. If the answer is empty — or the broker refuses, because the attestation was not honored or because policy does not permit this principal to drive this agent — the sender gets `You're not authorized to use this agent.` and **no model call is made**. That is the cheapest possible refusal, and one the message text cannot argue with.
3. **Execution.** On a `persistent` route the session first looks up its conversation, keyed on `(transport, the conversation identity, the sender's canonical subject)`. An entry idle past the route's timeout, or built under a granted capability set that differs from the one this message's leg just reported, is dropped rather than used; whatever survives is seeded into the prompt ahead of the new message as compacted `(question, answer)` pairs, oldest dropped first until the window's turn and byte bounds both hold. The lookup happens *after* step 2 because the grant comparison needs a fresh grant to compare against. Then, as before: the model client is built from the route's model, the shell runtime is given the attested leg as its only capability dispatch, and the prompt loop runs on a blocking task with the agent's `instructions` as the system prompt. Instructions are supplied fresh on every message and never stored, so editing an agent's standing orders takes effect on the next message without rewriting a single remembered conversation. Shell bounds are `dekopon-shell`'s defaults except `maxCapabilityCalls`, which comes from the route. Every model request the session then makes declares a [prompt cache key](#the-prompt-cache-key) — the conversation's on a `persistent` route, the route's on a `oneShot` one.
4. **Answer.** The session's final text goes back to chat. On failure the sender gets one fixed line, `The agent could not complete this request.` — a `PromptError` can carry model-chosen text, a provider message, or a transport diagnostic, and chat is the last place any of those belong. The operator reads the category from telemetry. A `persistent` route then writes the exchange back as one more remembered turn, trims the window, and restarts the idle clock. **The fixed failure line is never stored.** A session that reached the model and failed records its question with nothing in the answer's place, which is truthful and is what makes the retry after it answerable; a session refused at step 2 records nothing at all, because it never asked. What must never be remembered is this daemon's own failure sentence, since replaying it would teach the model to keep producing it.

Text is bounded in both directions: inbound to 16 KiB keeping the head (a chat message states its request first), outbound to 8 KiB keeping head and tail (an answer's conclusion is usually its last line). Both truncations say so in the text.

At shutdown, transport readers are aborted and in-flight sessions get `shutdownGraceMs` to finish — a model call is already paid for, and abandoning it means a person watching a chat window never hears back.

## Conversations

**Status: current.** History is a trust surface rather than a feature flag, and [`security-model.md`](security-model.md#conversation-memory-as-a-trust-surface) states the surface it accepts.

A `persistent` route keeps a bounded history per sender and replays it into the next prompt, so a follow-up question can say "and the second one?" and be answered. `oneShot` is the default and is exactly the behavior every route had before this existed.

### The history lives in the gateway

In the daemon's memory, and nowhere else. It is never written to disk, never sent to the broker, and lost on restart: `dekopond` comes back with every conversation forgotten, and a person who asks a follow-up across a restart gets a first-message answer.

That placement is the whole point rather than an implementation shortcut. The broker holds provider credentials and a deliberately metadata-only audit chain in which a provider's output survives only as a digest. Conversation text there would put the most sensitive content in the system inside the most privileged process, sitting beside a record built specifically not to contain it. The gateway already handles this text — it read the message and it wrote the answer — so keeping the history there adds no new reader.

### The key includes the sender

`(transport, the conversation identity, the sender's canonical subject)`. The subject is in the key because the alternative replays one person's exchange into another person's prompt, and in a shared channel that is not a hypothetical.

The conversation identity is the transport-derived one, not `(channel, thread)`. Slack omits `thread_ts` on the message that *starts* a thread and sends it on every reply inside one, while the bot answers that first message in a thread rooted at it — so anything keyed on the raw thread identifier files the opening question apart from every reply to it and orphans the first turn of every threaded conversation. Each transport derives the identity because only a transport holds the service-native pieces it takes.

This is deliberately *not* the admission key from step 1, which is `(transport, channel, thread)` and has no subject in it. The two keys answer different questions. Serialization asks "is this bot already busy on this thread", and two people talking at once in one thread are one thing to serialize. History asks "whose exchange was this", and the same two people are two histories.

Because the two keys are different, admission does not serialize history access: a sender who replies in-thread to their own message before the bot answers admits twice under two keys and runs two sessions against one history. The store handles that itself. Both sessions read the same seed, and each *appends* its own exchange rather than writing back a whole window, so neither can erase the other's answer.

The visible consequence of the subject in the key is that in a channel the bot remembers each person separately and remembers nothing about what it told somebody else in the same thread, which will occasionally read as forgetfulness. That is the correct trade.

### Prior turns are compacted

A stored turn is `(the user's message, the final answer)`, or the message alone when the session failed before it produced one. Every intermediate step — the model's tool calls, the scripts it authored, and their output — is dropped at write-back and never replayed.

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

Every message opens a fresh attested broker leg and gets a fresh `capabilitiesFor` answer, exactly as step 2 already describes. Persistence changes nothing here: no grant is remembered, no decision is carried forward, and history is prompt text rather than authorization input.

The granted capability set is additionally **stored with the conversation** and compared on every message. Any difference drops the history and starts a fresh conversation; an empty grant removes the entry outright. The reason is narrow and specific: output a session fetched under a broad grant is sitting in the history, and if the owner then narrows what that subject may reach, an unchecked entry would keep replaying it after the capability that produced it was taken away. Invalidation costs a cache miss on the first message after any policy change, which is the right price — a narrowed grant is precisely when replaying old output is wrong.

Its reach is exactly the granted capability *identifiers*, which is less than it sounds like. A policy edit that keeps the same capability list but tightens its owner-authored constraint set — a narrower allowed host, a smaller output ceiling, a different credential — produces an identical grant set and does not drop the history. Text fetched under the older constraints stays in the prompt until the window or the idle timeout removes it.

### Why fifteen minutes

The idle-timeout default is pulled in two directions and deliberately loses one of them.

A provider's prompt cache clears within minutes of the last request. An entry that outlives the cache pays full price to re-read its own history on the next message, so a timeout chosen for cache hits would be a few minutes at most. Human conversational memory runs on a much longer clock: someone who asks a follow-up after a meeting expects the bot to still know what they were talking about, and a bot that forgot four minutes ago is the failure people actually report.

The default is 15 minutes, which resolves toward the person, because the user-visible point of this feature is memory and not cache hits. An operator who wants the cache-aligned behavior sets `idleTimeoutMs` down to it and gets a bot with a shorter attention span in exchange. **The cost control is the window, not the cache:** `maxTurns` and `maxBytes` bound what any one message pays no matter how long its conversation has been alive, and they are the settings to reach for when the bill is the problem.

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

- **Provider prompt caches clear after minutes of inactivity.** A gateway that receives a message every few hours misses on the first turn of every conversation no matter what key it sends. The win is a cheaper *burst* — the tool-calling turns inside one live session, and a follow-up sent while the conversation is still warm — not a standing discount on the bill.
- **A window trim costs a miss by construction.** When `maxTurns` or `maxBytes` drops the oldest exchange, the front of the request is rewritten and the cached prefix ends at the first changed token. That is an argument for a *generous* window that trims rarely rather than a tight one that trims constantly, which is the opposite of how a size bound is usually tuned. The bound is still the cost control; it is just not free to hit.
- **Changing an agent's `instructions` invalidates everything.** They sit ahead of every message, and on the ChatGPT path they are hoisted into a separate top-level field, so an edit — including switching between having them and not — rewrites the front of every request on that route.

**Read `usage.cached_input_tokens` to find out whether any of this is working.** It is already plumbed end to end: whatever the provider reports lands on the `prompt.model_turn` span and on the `accounting.model.turn` audit event, alongside `usage.input_tokens`. The ratio between the two on a conversation's second and later turns is the whole answer, and a count the provider did not report is absent rather than zero, so a missing field means "unreported" and never "no cache hits". No new instrumentation is involved in checking.

### What this means for retention

On a `persistent` route, chat text sits in `dekopond`'s memory for at least the idle timeout after somebody stops talking. On the default that is fifteen minutes of a person's question, and the agent's answer, resident in a process that previously kept neither past the reply. **At least**, because eviction is lazy: an abandoned conversation is dropped by the next lookup on its key or by the ceiling displacing it, so with neither happening the bytes stay in the process until it exits. What a timed-out entry can never do is reach a prompt. The daemon writes none of it to disk; the operating system's own paging and core-dump behavior are outside what the daemon controls. Under the single-UID deployment described below, any process under the owner's UID can read that memory — "in memory only" is a durability property, not an isolation one.

## Authorization flow

```text
chat service            authenticates the sender
      |
      v
dekopond                subject = ExternalSubject::slack(team, user)   (routing metadata, not authority)
      |                 agent   = the route's catalog agent
      |
      | capabilitiesFor(subject, agent)  ── empty ⇒ refuse, no model call
      | invokeFor(proposal, subject, agent)
      v
dekopon-brokerd         attestor grant bounds the namespace
                        identityMappings turn the subject into a principal
                        policy must permit agent.prompt for that principal and agent
                        policy conditioned on context.via decides what it may then reach
                        credentials resolve, the provider executes, audit records it
```

The broker is the sole authority. `dekopond` supplies the subject and never the principal; a refused attestation is an audited denial recorded against the gateway's own peer identity. Driving an agent at all is its own policy statement — `Dekopon::Action::"agent.prompt"` over `Dekopon::Agent::"<name>"` — so a mapped subject the owner never permitted to use this agent is refused before the capability listing is even assembled, and an `invokeFor` under such a session is the audited denial `agent-denied`. See [`security-model.md`](security-model.md) for the complete attestation contract, and note in particular that **a policy written for direct peers can never authorize an attested context and vice versa** — adding a gateway cannot widen a grant that already existed.

## Telemetry

Spans follow [`observability.md`](observability.md):

| Span | Fields |
|---|---|
| `gateway.message` | `transport`, `agent`, `outcome` (`answered`, `unauthorized`, `busy`, `failed`, `reply-failed`) |
| `gateway.session` | `agent`, `conversation.turns`, `conversation.bytes`; wraps the broker leg and the model session |

The prompt loop's own spans (`prompt.session`, `prompt.model_turn`, `prompt.script`, `shell.command`) nest under `gateway.session`, and the broker's spans join the same trace through the proposal's `traceparent`.

The metadata-only default carries transport, agent, and outcome and nothing else. Chat text and canonical subject identifiers appear **only** under `telemetryPayloads: true`, as the `gateway.message.received` log event — the same gate `dekopon-run` uses for prompts and script text. Enabling it declares the telemetry sink in scope for the messages this daemon handles.

The prompt cache key is behind that same gate, as `gateway.session.cache_key`, and not on the metadata-only default. It names nobody — that is the whole design — but within one process it still joins one person's turns to each other, and a join key over somebody's conversation is the linkage the default exists to withhold. It rides its own log event rather than joining `gateway.message.received`, so a key and a canonical subject never appear on one line.

Conversations add to both lists, and change the meaning of one field that already exists. `gateway.session` carries `conversation.turns` and `conversation.bytes` — how much history this message replayed, as a count and a byte total and never as text; both are zero on a `oneShot` route and on the first message of any conversation. `gateway_conversation_evicted` is in the lifecycle events below with a reason of `idle`, `capacity`, or `grant-changed`. The second-order effect is the one that catches people: on a seeded session `message.count` counts the replayed window plus this exchange rather than this exchange alone. [`observability.md`](observability.md#what-conversation-history-changes) has the dashboard consequences.

Lifecycle events on stdout as structured JSON: `gateway_broker_ready`, `gateway_transport_connected`, `gateway_started` (transport and route counts), `gateway_session_rejected`, `gateway_session_failed`, `gateway_conversation_evicted`, `gateway_transport_disconnected`, `gateway_stopped`. Failure events carry a stable category, never raw error text, and an eviction carries a reason and nothing about the conversation it forgot.

## The single-UID caveat

In the current deployment `dekopond` and `dekopon-brokerd` run under one UID, and the broker's socket mode `0600` makes that UID one trust domain. An attestor grant therefore adds no authority beyond what any process under the owner's UID already has: every such process can already act as the configured gateway peer.

What the mechanism buys today is attribution and blast-radius shape — subject-level audit, deny-by-default policy conditioned on `via`, and configuration that fails closed on undeclared principals. Namespace scoping and `via` become real isolation only when the gateway runs under its own UID with its own peer identity, and that deployment (along with the socket permissions it needs) remains committed direction. [`security-model.md`](security-model.md) states this in full.

The caveat covers conversation history too. Holding it in gateway memory keeps it off disk and out of the privileged process; it does not keep it away from another process running as the same user.

## Related documents

- [`design.md`](design.md) — the authority model this daemon deliberately sits outside of.
- [`security-model.md`](security-model.md) — attestation, trust boundaries, the single-UID limitation, and the trust surface conversation memory accepts.
- [`broker-http.md`](broker-http.md) — the broker contract the gateway proposes into.
- [`run.md`](run.md) — the one-shot runner that shares the same session layer.
- [`observability.md`](observability.md) — span semantics, payload gating, and data minimization.
