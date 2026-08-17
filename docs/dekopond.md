# `dekopond` — the chat gateway and agent daemon

`dekopond` is the unprivileged half of the deployment boundary in [`design.md`](design.md): it connects to chat services, waits for a wakeup, routes each authenticated message to a named agent from the catalog, runs one bounded model session whose only tool is the sandboxed shell, and replies with the answer.

It holds chat bot credentials and model credentials — the things it needs to hear a question and to ask a model. It never holds a provider credential, a policy, or an authorization. Every effect a session drives is submitted to `dekopon-brokerd` as an on-behalf-of proposal naming the sender's canonical subject, and the broker alone maps that subject to a principal, decides what it may do, resolves credentials, and executes it.

**Status: Current.** Chat-transport wakeups, attested routing, and bounded sessions are implemented and tested. Conversation context and agent memory remain committed direction: each message is one independent session with no history.

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
    timeoutMs: 120000
    classes: [reasoning]

routes:
  - transport: scientist-slack
    match: { kind: directMessage }            # or { kind: channel, channel: c0123abc }
    agent: xaviers-rubber-stamper
    model: local-qwen                         # optional; else the first model offering the agent's modelClass
    limits: { maxSteps: 8, maxCapabilityCalls: 16 }

sessions:
  maxConcurrent: 4                            # optional, default 4
  replyOnBusy: true                           # optional, default true

shutdownGraceMs: 120000                       # optional, default 120000

telemetry:                                    # optional, identical in shape to broker.yaml's
  endpoint: http://127.0.0.1:5080/api/default
  transport: http
  serviceName: dekopond
  exportTimeoutMs: 10000
  telemetryPayloads: false
```

### No secrets in this file

Transports and models name **environment variables**, never values, following the precedent `dekopon-telemetry` set for OTLP ingest credentials. A variable name is validated as a name (`[A-Za-z_][A-Za-z0-9_]*`), so pasting a token where a variable name belongs is a startup failure rather than a token sitting in plain text while the daemon reports a missing credential. Missing variables are reported at startup **by variable name and never by value**.

### Startup fails closed

A gateway that starts and then refuses everything is worse than one that does not start. These are all startup failures:

- a route naming an agent the catalog does not contain, or one the catalog disables;
- an agent with no resolvable model — no `model` override and no configured model offering its `modelClass`, or no `modelClass` at all;
- duplicate transport names, duplicate model names, a route naming an unknown transport or an unknown model;
- a zero step budget, a zero capability budget, or zero concurrency;
- a transport endpoint override that is neither the one production origin nor a literal loopback `http://` URL;
- a missing credential environment variable;
- an unreachable broker. `dekopond` makes one `capabilities()` call on the configured socket before connecting any transport and logs the capability count as `gateway_broker_ready`.

## Transports

### Slack Socket Mode

An app-level token opens `apps.connections.open`, which returns a `wss://` URL; a bot token answers through `chat.postMessage`. No public HTTP endpoint is needed, which is why Socket Mode rather than the Events API. [`../examples/slack/`](../examples/slack/README.md) has an app manifest requesting exactly the scopes and events this transport consumes, plus the token and identity-mapping walkthrough.

The protocol's one sharp edge is redelivery: Slack expects an acknowledgment within roughly three seconds and resends the envelope otherwise. A Dekopon session takes far longer than that, so **the acknowledgment is sent before any processing begins** — before parsing, before routing, before any model call. A bounded ring of 1024 seen `(channel, ts)` pairs absorbs the redeliveries that happen anyway across a reconnect.

- `disconnect` envelopes are routine (Slack rotates sockets on its own schedule) and trigger a reconnect with jittered exponential backoff capped at 60 seconds.
- Messages carrying `bot_id`, messages from the bot's own user identifier, and subtyped messages (edits, joins) are dropped. Both bot checks matter: another app's post carries `bot_id`, and this app's own post arrives with the bot's user identifier and no `bot_id` at all.
- `channel_type: im` is a direct message; anything else is a channel.
- Subject: `slack.<team>.<user>`, lowercased.
- A channel answer joins the thread it was asked in, starting one on the inbound message when there is none. A direct message has no thread to join and answering in one would hide the reply.

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

Every local message is a direct message; channel routes are a chat-service concept.

## Routing

First match wins on (transport name, direct message or named channel). Unmatched messages are ignored with a debug-level event — bots see ambient traffic, and silence is the correct answer.

In a shared channel the bot must additionally be addressed: `<@BOT_USER_ID>` on Slack, `@botname` on Telegram. A channel route that fired on every message would be noise and cost.

## Sessions

Each routed message runs one independent session:

1. **Admission.** A process-wide semaphore bounds what the daemon costs at once, and a per-`(transport, channel, thread)` in-flight set stops one conversation from queueing work on itself — what a person does when a bot seems slow and they send the same thing again. A rejected message gets `I'm busy — try again shortly.` when `replyOnBusy` is set, and silence otherwise.
2. **Authorization.** The session opens an attested broker leg with `capabilitiesFor(subject, agent)`. If the answer is empty — or the broker refuses, because the attestation was not honored or because policy does not permit this principal to drive this agent — the sender gets `You're not authorized to use this agent.` and **no model call is made**. That is the cheapest possible refusal, and one the message text cannot argue with.
3. **Execution.** The model client is built from the route's model, the shell runtime is given the attested leg as its only capability dispatch, and `run_prompt` runs on a blocking task with the agent's `instructions` as the system prompt. Shell bounds are `dekopon-shell`'s defaults except `maxCapabilityCalls`, which comes from the route.
4. **Answer.** The session's final text goes back to chat. On failure the sender gets one fixed line, `The agent could not complete this request.` — a `PromptError` can carry model-chosen text, a provider message, or a transport diagnostic, and chat is the last place any of those belong. The operator reads the category from telemetry.

Text is bounded in both directions: inbound to 16 KiB keeping the head (a chat message states its request first), outbound to 8 KiB keeping head and tail (an answer's conclusion is usually its last line). Both truncations say so in the text.

At shutdown, transport readers are aborted and in-flight sessions get `shutdownGraceMs` to finish — a model call is already paid for, and abandoning it means a person watching a chat window never hears back.

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
| `gateway.session` | `agent`; wraps the broker leg and the model session |

The prompt loop's own spans (`prompt.session`, `prompt.model_turn`, `prompt.script`, `shell.command`) nest under `gateway.session`, and the broker's spans join the same trace through the proposal's `traceparent`.

The metadata-only default carries transport, agent, and outcome and nothing else. Chat text and canonical subject identifiers appear **only** under `telemetryPayloads: true`, as the `gateway.message.received` log event — the same gate `dekopon-run` uses for prompts and script text. Enabling it declares the telemetry sink in scope for the messages this daemon handles.

Lifecycle events on stdout as structured JSON: `gateway_broker_ready`, `gateway_transport_connected`, `gateway_started` (transport and route counts), `gateway_session_rejected`, `gateway_session_failed`, `gateway_transport_disconnected`, `gateway_stopped`. Failure events carry a stable category, never raw error text.

## The single-UID caveat

In the current deployment `dekopond` and `dekopon-brokerd` run under one UID, and the broker's socket mode `0600` makes that UID one trust domain. An attestor grant therefore adds no authority beyond what any process under the owner's UID already has: every such process can already act as the configured gateway peer.

What the mechanism buys today is attribution and blast-radius shape — subject-level audit, deny-by-default policy conditioned on `via`, and configuration that fails closed on undeclared principals. Namespace scoping and `via` become real isolation only when the gateway runs under its own UID with its own peer identity, and that deployment (along with the socket permissions it needs) remains committed direction. [`security-model.md`](security-model.md) states this in full.

## Related documents

- [`design.md`](design.md) — the authority model this daemon deliberately sits outside of.
- [`security-model.md`](security-model.md) — attestation, trust boundaries, and the single-UID limitation.
- [`broker-http.md`](broker-http.md) — the broker contract the gateway proposes into.
- [`run.md`](run.md) — the one-shot runner that shares the same session layer.
- [`observability.md`](observability.md) — span semantics, payload gating, and data minimization.
