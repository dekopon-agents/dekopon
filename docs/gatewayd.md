# `dekopon-gatewayd` — the chat gateway and agent daemon

`dekopon-gatewayd` is the unprivileged half of the deployment boundary in [`design.md`](design.md): it connects to chat services, waits for a wakeup, routes each authenticated message to a named agent from the catalog, runs one bounded model session with the sandboxed shell and safe on-demand meta tools, and replies with the answer.

It holds chat bot credentials and model credentials — the things it needs to hear a question and to ask a model. It never holds a provider credential, a policy, or an authorization. Every effect a session drives is submitted to `dekopon-brokerd` as an on-behalf-of proposal naming the sender's canonical subject, and the broker alone maps that subject to a principal, decides what it may do, resolves credentials, and executes it.

**Status: Current.** A route is `persistent` unless configured otherwise; durable memory is a separate
broker/agent opt-in and does not turn that default into automatic replay. The chart enforces the
[current local process boundary](security-model.md#current-local-process-boundary).

Its dependency set excludes `dekopon-broker`, `dekopon-broker-host`, `dekopon-http-host`, `dekopon-storage-host`, `dekopon-policy`, and `dekopon-brokerd`, and CI rejects any of them appearing in the gateway's normal dependency tree.

[`../examples/conditional-write/`](../examples/conditional-write/README.md) is the complete
worked deployment: a Slack DM from an owner-mapped sender, two narrow `http-probe` capabilities, a
broker-injected GitHub token, and an audited PR review comment. Read it alongside this document —
it is the configuration this one describes in the abstract.

## Run

```console
dekopon-gatewayd --config /path/to/gatewayd.yaml
```

The configuration file must be a regular non-symlink file owned by the daemon's UID, with a single link, not group- or world-writable, and no larger than 1 MiB. It is strictly decoded: an unknown field, an unknown transport kind, or an unknown route match is a startup failure, not a silently ignored setting.

## Check a configuration

```console
dekopon-gatewayd check gatewayd.d --catalog agents.d
dekopon-gatewayd check gatewayd.yaml --output json
dekopon-gatewayd check candidate.d --catalog agents.d --probe --against live.d
```

`check` runs the validation startup runs, through the same function, and stops before the broker
is contacted. Every stage whose inputs loaded runs, so it prints every configuration, catalog and
route problem at once, and a stage that could not run (routes when the catalog failed, say) is a
warning naming it. Output is one line per problem or warning, or
`{ "ok", "problems", "warnings" }`; it exits 0 with no problems, 1 with problems and 2 on a usage
error. `--catalog` replaces `catalogPath`, which usually names a deployment path. Files may be the
invoking user's own 0644 files; symlinks and group/world-writable files are still refused. Plain
`check` touches nothing runtime: the broker socket is never contacted, no transport or model
credential variable is read (each one boot will read is listed as a warning), no ChatGPT login is
opened and no journal directory is created. Transport clients are not built, so checks that live
in their constructors run only at boot.

`--probe` adds the one online stage, and with it the one credential read: for each changed
`openrouter` model it reads the model's `apiKeyEnv` and sends a single completion capped at 16
output tokens, through the same client and routing settings the gateway uses, with a 30-second
bound (or the model's `timeoutMs`, if shorter). A refusal is a problem naming the model, its
vendor model id, the file that defines it, the routes it serves and the vendor's status and body;
the key never appears, because the client redacts it from the body. A model is changed when its
name is absent from the `--against` configuration or its parsed settings differ there (defaults
filled in, so formatting alone is no change); without `--against` every model is probed, and an
`--against` that does not load is a warning and probes every model. `chatgptSubscription` models
are never probed, since reading the login can refresh and rotate it under the running gateway;
each changed one is a `not probed: subscription auth` warning, as is each changed `anthropic`
(`proxy-only model`) and `openaiCompatible` model.

## Configuration

```yaml
apiVersion: dekopon.dev/gatewayd/v1alpha1
catalogPath: /path/to/dekopon.yaml            # dekopon-config catalog with the agents routes name

broker:                                       # optional; every field defaults
  socketPath: /path/to/broker.sock            # default: DEKOPON_BROKER_SOCKET, then XDG_RUNTIME_DIR/dekopon/broker.sock, then HOME/.local/run/dekopon/broker.sock; unresolvable is a startup failure
  serverUid: 501                              # default: the daemon's own effective UID
  maxFrameBytes: 2097152                      # default: the protocol's own bound
  ioTimeoutMs: 30000                          # also how long a turn, job or probe retries a missing or refusing broker socket before any request is sent

transports:
  - name: scientist-slack
    kind: slackSocketMode
    appTokenEnv: DEKOPON_GATEWAYD_SLACK_APP_TOKEN     # environment variable NAMES only
    botTokenEnv: DEKOPON_GATEWAYD_SLACK_BOT_TOKEN
    endpoint: https://slack.com               # optional, tests only: the pinned origin or a literal loopback http:// URL
    experience: agent                         # optional: classic (default) | agent
    liveness:                                 # optional; absent means off
      mode: native                            # off | native
      classicFallback: reaction               # none (default) | reaction; slackSocketMode only
      progress: auto                          # auto (default) | off | message
      statusText: false                       # Agent only; true requires progress: off or auto
      cancelButton: false                     # default false; refused on whatsappCloudApi and experience: agent
      keepAlive: { atSeconds: [15, 45], everySeconds: 60, max: 10 }   # optional; defaults shown
      templates:                              # optional; defaults ship in the binary
        working: "Working on it…"
        tool: "Running {word}…"
        keepAlive: "Still working ({elapsed_s} s)…"
        note: "{note}…"
        noteEta: "{note} (~{eta_s} s)…"
        stopped: "Stopped." # a person's or operator's stop
        failed: "The agent could not complete this request." # a failure with no named cause
  - name: community-discord
    kind: discordGateway
    botTokenEnv: DEKOPON_GATEWAYD_DISCORD_BOT_TOKEN
    messageContent: false                     # optional, default false; true requests the privileged Message Content
                                              # intent, which the Developer Portal must allow (see Discord Gateway)
    liveness: { mode: native, progress: message, cancelButton: true }
  - name: tg
    kind: telegramLongPoll
    botTokenEnv: DEKOPON_GATEWAYD_TELEGRAM_TOKEN
    liveness: { mode: native, progress: message, cancelButton: true }
  - name: whatsapp
    kind: whatsappCloudApi
    debounceMs: 5000                      # quiet interval; 0 bypasses collection
    debounceMaxWaitMs: 15000               # maximum from first media receipt
    appSecretEnv: DEKOPON_GATEWAYD_WHATSAPP_APP_SECRET
    verifyTokenEnv: DEKOPON_GATEWAYD_WHATSAPP_VERIFY_TOKEN
    accessTokenEnv: DEKOPON_GATEWAYD_WHATSAPP_ACCESS_TOKEN
    bind: 0.0.0.0:9080                     # pod bind; expose only through exact-path TLS ingress
    callbackPath: /webhooks/whatsapp
    wabaId: "123456789"
    phoneNumberId: "987654321"
    graphApiVersion: v23.0                 # explicit; no implicit/latest version
    liveness: { mode: native }             # typing only: WhatsApp cannot edit a message
  - name: dev
    kind: local
    socketPath: /path/to/gatewayd-dev.sock
    liveness: { mode: native, progress: message, stream: true, cancelButton: true }

stopWords: [stop, cancel]                     # optional, default shown; see Stopping a run

models:
  - name: local-qwen
    kind: openaiCompatible
    endpoint: http://127.0.0.1:11434/v1
    model: qwen3
    apiKeyEnv: OPENAI_API_KEY                 # optional; absent means the endpoint needs no key.
                                              # Named but unset or blank is a startup failure.
    timeoutMs: 120000
    stream: true                              # optional, default true; write false only for an endpoint
                                              # whose SSE is broken. chatgptSubscription has no such
                                              # field: it always streams.
    classes: [reasoning, analysis]
  - name: subscription
    kind: chatgptSubscription
    model: gpt-5-codex
    authFile: /path/to/chatgpt-auth.json      # optional; else DEKOPON_CHATGPT_AUTH_FILE, else Dekopon's own credential file
                                              # must be in a writable directory: refreshing rewrites it
    timeoutMs: 120000
    classes: [reasoning]
    modalities: [image]                       # optional; default none. This is image INPUT only.

  - name: explore
    kind: openrouter
    model: anthropic/claude-sonnet-4.5
    apiKeyEnv: OPENROUTER_API_KEY              # required environment-variable reference, never a value
    timeoutMs: 120000
    classes: [general]
    generation: { maxOutputTokens: 4096 }
    reasoning: { effort: medium }
    routing: { allowFallbacks: false, requireParameters: true, only: [anthropic] }
    cache: { style: explicitPrefix, ttl: 5m }

routes:                                       # first match wins; order matters
  - transport: scientist-slack
    conversation: { kind: [channel, thread], ids: [c0123abc] }
    agent: incident-responder                 # one named channel and the threads under it
  - transport: scientist-slack
    conversation: { kind: [channel, thread] } # any other channel the bot is invited to
    agent: xaviers-conditional-writer
  - transport: community-discord
    conversation: { kind: any }               # every kind, including group DMs and forum posts
    agent: xaviers-conditional-writer
  - transport: scientist-slack
    conversation: { kind: [directMessage] }
    subjects: [slack.t0123abc.u9xyz]          # optional, and only beside kind: [directMessage]
    agent: xaviers-conditional-writer
    model: local-qwen                         # optional; else the first model offering the agent's modelClass
    inspectAgentConfig: true                  # optional, default false; offers inspect_agent_config
    progressDetail: plain                     # optional: off | plain (default) | detailed
    progressNotes: false                      # optional; true lets the model write one bounded note on the progress line
    steering: abort                           # optional: abort (default) | boundary
    limits:                                   # maxDurationMs, scriptTimeoutMs and jobTimeoutMs are optional
      maxSteps: 8
      maxCapabilityCalls: 16
      maxDurationMs: 300000                   # whole-session wall clock; omitted means none, 0 refused
      scriptTimeoutMs: 240000                 # one script's deadline; default 30000, 0 refused
      jobTimeoutMs: 3600000                   # optional; enables detached jobs on this route, 0 refused
    memory:                                   # optional; default { mode: persistent }
      mode: persistent                        # oneShot | persistent
      scope: privateConversation              # privateConversation (default) | sharedConversation
      idleTimeoutMs: 900000                   # optional, default 900000 (15 minutes)
      maxTurns: 12                            # optional, default 12 exchanges in the window
      maxBytes: 65536                         # optional, default 65536 replayed history bytes
      recall: journal                         # optional: none | journal | platform; default journal when sessions.journal is set, else none
      forgetAfterMs: 604800000                # optional, default 604800000 (7 days); needs recall
    wakes: true                               # optional, default false; offers the wake tool, needs sessions.wakes

sessions:
  maxConcurrent: 4                            # optional, default 4
  maxJobs: 2                                  # optional, default 2; 0 refused
  replyOnBusy: true                           # default true; saturation or a full mailbox only
  maxConversations: 1024                      # optional, default 1024 tracked
  assetRetentionBytes: 268435456              # optional, process-wide disk budget; 0 disables assets
  journal:                                    # optional; absent, no conversation text is written to disk
    path: /var/lib/dekopon-gatewayd/journal           # relative paths resolve against this file; files idle past the longest journal-route forgetAfterMs are deleted
  wakes:                                      # optional; absent, no route may schedule a wake
    path: /var/lib/dekopon-gatewayd/wakes.jsonl       # relative paths resolve against this file
    maxPerSubject: 20                         # optional, default 20 pending wakes per person
    minIntervalMs: 300000                     # optional, default 5 minutes between watch checks; must exceed scriptTimeoutMs
    maxHorizonMs: 2592000000                  # optional, default 30 days

shutdownGraceMs: 120000                       # optional, default 120000

telemetry:                                    # optional, identical in shape to broker.yaml's
  endpoint: http://127.0.0.1:5080/api/default
  transport: http
  serviceName: dekopon-gatewayd
  exportTimeoutMs: 10000
```

The route's `conversation:` block is the **match**: `kind` is the word `any` or a list of `directMessage`, `groupDirectMessage`, `channel`, and `thread`, with an optional `container` (Slack team, Discord guild, WhatsApp `waba:phoneNumberId`) and an optional `ids` list. A bare `kind: channel` is a decode failure naming the list form, because it reads as though it claimed the threads under the channel too. `subjects:` restricts a route to named canonical subjects and is accepted only beside `kind: [directMessage]`; it is routing, never authority, and a per-person channel route would read as an access-control list and be trusted as one.

`inspectAgentConfig: true` offers the `inspect_agent_config` tool on that route's sessions; without it the tool is not offered. Leaving it off removes the structured dump — the description, model class, limits, and the agent's `instructions` verbatim — and nothing else: the instructions are still the system prompt, so this is not secrecy from a determined user, only the removal of a one-call transcript of them.

`limits.scriptTimeoutMs` bounds one script rather than the whole session: the shell ends the run at that deadline with `dekopon-shell: script exceeded its <N>ms deadline`, and a provider call still in flight is abandoned with it — the broker's call is dropped along with the script, so an effect that had already reached the provider leaves a decision record and no execution record. It defaults to 30000, which is the deadline every route ran under before the field existed, and 30 seconds is not enough for a genuinely slow capability: `gpt-image.edit` routinely takes longer, while a generate finishes in about 24 seconds. `maxDurationMs` is still the session bound, and a `scriptTimeoutMs` above it is refused at startup because the session would be cancelled first.

The `memory:` block — which was called `conversation:` before 0.14.0 — is tagged on `mode`, and both halves are strict: an unknown mode, an unknown or wrong-case `scope`, and any persistent-only field written next to `mode: oneShot` are decode failures. `scope` is strict camelCase, accepts only `privateConversation` and `sharedConversation`, and is valid only beside `mode: persistent`; omission defaults to `privateConversation`. A setting that can never take effect is far more likely a mode typo than an intention, and a decoder that ignored it would leave a configuration file claiming a memory or audience the daemon does not have.

`progressNotes: true` offers the shell's `progress` builtin in the bash tool description and
admits its bounded, model-authored notes to `gateway.progress` telemetry. It defaults to false:
turning it on is consent for this text to reach telemetry and chat under the
[security model's liveness rule](security-model.md#current-gateway-posture). A live note owns an
active progress message until another note replaces it, the next model turn or steering clears it,
or the session ends; individual tool completions do not clear it. At a keep-alive tick, a note older
than twice its ETA (or 120 seconds without an ETA) is cleared before rendering; the ETA never counts down.
A transport with liveness off or no active progress message still records an opted-in note.

### OpenRouter model settings

The tag is exactly `kind: openrouter`, not `openRouter`. `name`, nonblank `model`, `apiKeyEnv`
and positive `timeoutMs` are required. Any nonblank model ID is accepted locally; there is no
catalog. `classes` and `modalities` default to `[]`; `modalities: [image]` means image input only.
The production URL is fixed at `https://openrouter.ai/api/v1/chat/completions`, with streaming
always on. `endpoint`, `stream` and `authFile` are not accepted for this kind.

Each optional block is strict, as is the model entry:

| Block | Members and bounds | Wire mapping |
|---|---|---|
| `generation` | optional `maxOutputTokens` positive integer, finite `temperature` 0–2 inclusive, finite `topP` >0 and ≤1 | `max_tokens`, `temperature`, `top_p` |
| `reasoning` | required `effort`: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` | `reasoning: {effort}` |
| `routing` | optional `allowFallbacks`, `requireParameters` booleans; optional nonempty `only` list of nonblank strings | `provider: {allow_fallbacks, require_parameters, only}` |
| `cache` | required `style`: `automatic` or `explicitPrefix`; optional `ttl`: `5m` or `1h`, only with `explicitPrefix` | explicit system-part `cache_control: {type: ephemeral, ttl?}` |

Omitted members are absent on the wire, not sent as default values. No cache block means automatic
mode (no marker); an omitted TTL is not invented. Explicit caching marks the last content part of
the last leading system message, including optional-reply guidance; no leading system message is
an `InvalidRequest` before sending. Reasoning fragments remain private continuation state, never
chat progress. The response-cache header `X-OpenRouter-Cache: false` is always sent; there is no
`usage.include`, `stream_options` or `reasoning.exclude`. The prompt cache key is
sent as `session_id` for sticky provider routing. Forwarded controls are requests, not guarantees
that an upstream provider honored them. A provider change mid-conversation is accepted; pin
`routing.only` when cost or cache locality matters. `allowFallbacks: false` disables failover but
does not pin routing.

Unknown fields and bad enum spellings are decode refusals. Semantic numeric bounds (including
non-finite values), empty routing lists/entries and automatic-cache TTLs join the existing
`ConfigProblem` collection rather than being clamped. Missing/blank credential variables are
separate `StartupProblem`s at startup. These blocks are refused on
`chatgptSubscription` and `openaiCompatible`.

Each call has one total `timeoutMs` deadline; see [`inference.md`](inference.md).

### No secrets in this file

Transports and chat models name **environment variables**, never values, following the precedent `dekopon-telemetry` set for OTLP ingest credentials. A variable name is validated as a name (`[A-Za-z_][A-Za-z0-9_]*`), so pasting a token where a variable name belongs is a startup failure rather than a token sitting in plain text while the daemon reports a missing credential. Missing required variables are reported at startup **by variable name and never by value**, and all three read them through one definition, so a model credential fails exactly the way a chat credential does. A variable exported with a blank value is refused the same way: an empty app secret is an HMAC key anyone can compute, and an empty bearer token is still sent as a header, so presence has to mean a credential rather than an export.

### Startup fails closed

A gateway that starts and then refuses everything is worse than one that does not start. These are all startup failures:

- a route naming an agent the catalog does not contain, or one the catalog disables;
- an agent with no resolvable model — no `model` override and no configured model offering its `modelClass`, or no `modelClass` at all;
- duplicate transport names, duplicate model names, a route naming an unknown transport or an unknown model;
- a zero step budget, a zero capability budget, or zero concurrency;
- a transport `endpoint` override (`graphEndpoint` on `whatsappCloudApi`) that is neither its pinned production origin (Slack, Discord, Telegram, or the Meta Graph API) nor a literal loopback `http://` URL. Literal means `127.0.0.1` or `::1`: the name `localhost` is resolved by whatever the host's resolver says today, which is not the same promise;
- a route still written with the retired `match` key, or a `mode`/`scope`/`idleTimeoutMs`/`maxTurns`/`maxBytes` key under `conversation:`. Both name their replacement: the route match is `conversation:`, and the memory window is `memory:`;
- a selector that can never name a conversation: an empty or duplicated `kind` list, an empty `ids` list, an id or `container` the transport would never mint, a `container` on Telegram (which has nothing above a chat), a kind the transport never produces (`groupDirectMessage` on Discord, `channel` on WhatsApp), or an `ids` entry in `id:thread` form — a selector names the parent, and the kind list decides whether its threads come with it;
- `subjects:` beside anything but `kind: [directMessage]`, an empty `subjects:` list, or `memory.scope: sharedConversation` on a `[directMessage]`-only route, where the direct message already is the subject;
- a `liveness.conversations.<kind>` key for a kind the transport never produces;
- a missing or blank chat or bound-route model credential environment variable. An `openaiCompatible` model's `apiKeyEnv` is optional (OpenRouter requires it), and absent means "this endpoint needs no key", which a loopback llama.cpp genuinely does not; naming a variable that is unset or exported blank is the opposite claim, and this process cannot see one exported after it started;
- an unknown Slack experience, liveness mode/fallback, or field inside those strict blocks; an off
  Slack liveness with a reaction fallback, or a classic app with native liveness and no reaction
  fallback, is also refused because the configured fallback could never take effect;
- `liveness.progress: message`, `liveness.stream: true`, `liveness.statusText: true`, or `liveness.cancelButton: true` while `liveness.mode` is
  `off`, where none of them could ever take effect; each one is named;
- `liveness.stream` or `liveness.cancelButton` on `whatsappCloudApi`, which can neither edit a
  message nor carry an interactive component; `liveness.stream` on any Slack transport, since Slack
  progress now streams through Agent status text or the progress message instead of
  `chat.appendStream`; and `liveness.cancelButton` on a Slack transport with `experience: agent`,
  which renders its own Stop control;
- `liveness.statusText: true` on anything except Slack's Agent experience, or effective
  `statusText` alongside `progress: message`, including conversation overrides;
- `liveness.classicFallback` on a transport that is not `slackSocketMode`;
- a `liveness.keepAlive` with `everySeconds: 0` or an offset of `0` — a period of zero is a render
  loop rather than a keep-alive;
- a `liveness.templates` line using a placeholder that field cannot render. The known placeholders
  are `{word}` (the `tool` line only), `{turn}`, `{of}`, `{calls}`, `{calls_max}`, and
  `{elapsed_s}`, plus `{note}` only in `note`/`noteEta` and `{eta_s}` only in `noteEta`;
  neither note field permits `{word}`. `stopped` and `failed` render none; `stopped` is a person's
  or operator's stop, `failed` a failure without a [named cause](#stop-causes). Every
  offending placeholder in the block is named, not the first;
- an empty `stopWords:` list, or one with a blank word. Omit the key to keep `[stop, cancel]`;
- a route with `progressNotes: true` and `progressDetail: off`: notes need a progress line;
- a route with `limits.maxDurationMs: 0`, which would cancel every session the instant it started;
- a route with `limits.scriptTimeoutMs: 0`, which would end every script the instant it started, or one whose `scriptTimeoutMs` is greater than its `maxDurationMs`, where the session bound is reached first and the script deadline could never take effect; that refusal names both numbers;
- an unreachable broker. `dekopon-gatewayd` makes one `capabilities()` call on the configured socket before connecting any transport and logs the capability count as `gateway_broker_ready`;
- an empty `transports:`, `models:`, or `routes:` list;
- a model with `timeoutMs: 0`, or `shutdownGraceMs: 0`;
- a `whatsappCloudApi` transport whose `bind` port is 0, whose `wabaId` or `phoneNumberId` is not a canonical positive decimal, whose `callbackPath` is not lowercase literal segments, or whose `graphApiVersion` is not `v<major>.0`;
- broker frame bounds the protocol rejects (`maxFrameBytes` zero or above its hard ceiling, `ioTimeoutMs` zero), a broker socket that neither `broker.socketPath` nor the discovery order starting at `DEKOPON_BROKER_SOCKET` resolves, or a `telemetry:` block `dekopon-telemetry` refuses.

The `memory:` block adds three more:

- a `persistent` route with a zero idle timeout, a zero turn window, or a zero byte window — the same rule a zero step budget already follows, because a bound of zero is a bound nobody meant to write;
- an idle timeout, window bound, or `scope` on a `oneShot` route; an unknown, null, or wrong-case persistent `scope`. Those settings cannot take effect as written, and silently accepting them could turn an intended private route into some other behavior;
- a zero `sessions.maxConversations`, which would make every history immediately evictable and turn a persistent route into an expensive one-shot one.

**Every problem at once.** A file that decoded is scanned to the end before it is refused, and the refusal lists everything wrong with it — `3 validation problems found:` and then one line per problem, the shape `dekopon-config` already refuses a catalog with. Only a file that cannot be understood at all — wrong ownership or permissions, oversize, or invalid YAML — stops at the first error. Route binding scans the whole table the same way, so a catalog that disabled two of the agents routes name is one refusal naming both. And a list that failed itself is not blamed on the routes that name it: no transports at all, or a transport with no name, is reported once rather than again for every route pointing at it.

**Every credential before any connection.** The chat credentials and the bound-route model credentials all resolve, and every transport client is built, before the first transport authenticates to anything. Preparation still reports every missing credential together, by variable name rather than value. After preparation and the broker probe succeed, all transports connect concurrently. A healthy transport serves immediately while another recovers; its authenticated bot identity is installed before its first message can route. `gateway_started` means supervision is running, not that every transport is connected.

### Connection recovery

Every adapter — Slack, Discord, Telegram, WhatsApp and local — uses the same private composable
recovery extension. It wraps only connection establishment and receiving, never reply drivers,
provider invocations, or outbound message effects. Existing clients, Slack's dedup ring, Telegram
offsets, Slack thread ownership and Discord resume/identify state survive recovery. WhatsApp keeps
no dedup ring, so a Meta webhook retry — which happens only when its HTTP 200 is lost in the
network — either joins an in-flight turn as a steer or starts a fresh one and gets a second reply.
Discord's own resume sequence-number tracking already prevented a resumed connection from replaying
an event the client had already received, so deleting its now-redundant `SeenIds` ring changes
nothing about resume.

The fixed policy requires no configuration:

- Each initial or reconnect attempt has a **30-second deadline**, including a hung handshake.
- A failed attempt or established-connection failure spends one of **10 failures per episode**.
  The initial failure counts; the tenth is terminal, with no eleventh attempt. A permanent refusal
  (including an insecure local socket path or Discord fatal close/session-start exhaustion) is
  terminal immediately.
- After failures 1–9, backoff starts at **500 ms**, doubles to a **60-second base cap**, and adds
  **0–249 ms** of OS-random jitter. Shutdown cancels both sleep and connection work promptly.
- The episode resets only after **five continuously connected minutes**, including idle time.
  A successful handshake or message alone does not reset it, so rapid flapping eventually exits.
  Adapter liveness detects established-path loss; a quiet but healthy listener is not timed out.
- Terminal failure of **any** transport stops all readers, drains sessions for `shutdownGraceMs`,
  then exits nonzero through `gateway_exit`, preserving the failure cause. The executable also
  bounds teardown of abandoned blocking work to five seconds. A supervisor such as Kubernetes
  can then restart the **gateway container**, not necessarily the pod. This is recovery behavior,
  not a claim about the root cause of a connection reset.

The extension delegates reconnect protocol details to the adapter, without nesting retry loops.
A changed bot identity on recovery is terminal rather than routing under stale identity metadata.


## Agent configuration self-inspection

Every authorized session on a route that has written `inspectAgentConfig: true` is offered
`inspect_agent_config`. When someone asks “what is this
agent's configuration?”, the model can call it and receive one bounded JSON snapshot designed to
render as concise Markdown tables:

- agent identifier, description, and catalog `modelClass`;
- the exact catalog `instructions` supplied as this session's system prompt;
- the skills mounted for the agent, each as its name, description, and resource file paths —
  never the skill text, which `read_skill` discloses on demand — and absent when nothing is
  mounted;
- route step/capability limits and the one-shot or persistent `memory:` window, including the effective persistent scope; and
- the capability metadata in this sender's fresh `capabilities(subject, agent, scope)` result:
  identifier, selected provider, description, effect, and risk, as the provider manifest and the
  broker's `capabilities` define them.

That last section is an **effective Cedar view**, not Cedar source. Raw policy, policy IDs and
digests, denied or merely declared capabilities, execution constraints, credential bindings,
private secret-map source/selector/use inventory, principal/subject/channel/transport identifiers,
model endpoints and auth paths, broker paths, and all credential values are absent. Exact standing
instructions remain visible and may intentionally contain a public inert DRN. The gateway never receives provider credentials or raw
policy, and the typed view has no field for the chat/model credentials it does hold. Each serialized
result has a 128 KiB hard ceiling. A route without `inspectAgentConfig: true` does not offer the tool at all: the model's tool list does not carry it and a scripted call is an unknown tool. What that buys is flat: the structured dump is gone, and the `instructions` are still this session's system prompt, so secrecy from a determined user is the model's obedience rather than a gate. Calls are repeatable under the prompt loop's shared per-turn tool
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

## Multi-message media inputs

The shared gateway collects **media-first** inputs after routing/address checks and before
session admission. WhatsApp `debounceMs` is the quiet interval (default **5000ms**), and
`debounceMaxWaitMs` is the hard maximum from the first media receipt (default **15000ms**).
Both are unsigned 32-bit millisecond durations (`0..=4294967295`). `debounceMs: 0` disables
collection entirely; the maximum is then unused. Idle inputs use immediate admission.
When enabled, the maximum must be at least the quiet interval. Invalid types, negative/out-of-range
integers, incompatible enabled combinations and unrepresentable timer deadlines are refused at
configuration load. Accepted later photos, captions and standalone text extend that actor's quiet
deadline, but never beyond the first receipt plus the maximum. Refused inputs do not extend it.
Text arriving first remains immediate. These are heuristic bursts, not WhatsApp albums; webhook
payload boundaries are not album identities.

Telegram uses a separate fixed **3-second** window only for authenticated `media_group_id`
members, because Telegram supplies no group-end marker. Member captions join; ordinary text and
ungrouped Telegram media remain immediate and cannot claim membership. Different native groups
never merge: a competing group for the same pending actor/audience is visibly refused. WhatsApp's
setting never changes Telegram timing. In groups/topics, unaddressed native members may inherit
addressing only from an already-open addressed lead with the exact same actor, route, full scope,
reply audience and native group ID. Members arriving before that addressed lead (or after expiry)
remain ignored unless individually addressed; there is no pre-address buffer or grant inheritance.
Slack/Discord native attachment arrays remain indivisible
and immediate; local text remains immediate. Their existing 10-attachment parser ceiling now
refuses an oversized native envelope instead of accepting a truncated subset.

Collection is isolated by canonical authenticated subject, exact bound route, configured transport,
complete conversation/container/thread and reply audience, even on shared-history routes. Each
batch retains at most **8 envelopes, 32 assets and 16 KiB of rendered text**, including the source
boundary/caption-presence labels added for multiple inputs. Limits reject only the incoming
envelope, visibly and with a traced cause; already collected members remain intact. Native arrays
are never split. There is at most one collecting batch per compatible actor key and globally at
most `sessions.maxConcurrent` batches, independently of execution permits.

At quiet expiry or the hard deadline, whichever comes first, the batch attempts normal admission.
For a running conversation with mailbox room, the same sender's batch becomes a steer; another
sender's batch queues as a follow-up. After cancellation or completion is claimed, both queue.
Steers and follow-ups share an eight-item mailbox. A full mailbox or exhausted execution capacity
makes a batch eligible for a busy reply even when `replyOnBusy` is false, subject to the bounded
refusal-reply capacity and transport delivery.
Later media starts another bounded collection; standalone text follows ordinary steering/admission.
There is no replay or rollback of paid effects. WhatsApp collection waits at most its configured
maximum (15 seconds by default); Telegram waits its fixed 3 seconds. Queueing, inference and
transport time remain additional; collection does not guarantee that every group member joins one turn.

New turns receive fresh broker authorization and generation selection after admission; a same-sender
steer joins the running turn's existing leg and asset access. Collection itself does
not fetch assets, contact the model/provider or publish progress. One lead message owns progress,
reply target and native delivery identity; other message IDs are not fabricated into an album ID.
An authenticated stop removes only that actor's pending work and preserves the existing ownership
rules for active sessions. Shutdown discards pending batches without starting them. Original
receipt traces retain their input and terminal disposition. Ordinary collected executions export
causal links to every constituent; consumed steers link their constituents only to the admission
span, not the running execution (see [observability](observability.md)). Asset leases, history
scope, generation fences and on-demand fetch budgets are unchanged.

## Asset handles and delivery

The gateway's conversation-generation table is the only asset inventory: uploads and provider
outputs have scoped `chat-asset:<N>` references, never model-supplied paths. Every exact matching
proposal string leaf is resolved automatically, in first-occurrence order; repeated references
share one pin and descriptor. Data URLs in proposals are refused before broker submission.
The proposal JSON remains unchanged. Only referenced files cross the broker boundary as read-only,
close-on-exec descriptors; the complete bounded metadata table and remaining-send allowance ride
beside the invocation. Broker subject, Cedar and HTTP authorization are unchanged.

A successful provider `attach` returns a descriptor and typed metadata, not a JSON byte envelope.
The gateway numbers it at intake, charges its fstat size to the disk LRU, and appends a bounded
stderr note naming the reference, declared content type and stored-byte count. The declared label
is authoritative; a bounded decoded-prefix disagreement produces one metadata-only event.
Pathless outputs are retained without copying and reclaimed by closing their last descriptor.
All descriptor reads are positional. A trap, denial or timeout admits no asset effects.

**Attach is not send.** An explicit broker-authorized `asset.send` queues a file for this turn's
reply. Four newly sent assets are permitted per turn. Sent state persists for the lifetime of the
table entry: duplicate sends, even in later turns, are no-ops; a sent file cannot be removed.
Removal of an unsent asset closes its unpinned retained file and releases accounting. No failure
implicitly retries or clears sent state. Empty answer text still delivers queued files; failed
or cancelled turns deliver none. Delivery disposition is logged as `agent.asset.send`; a failure
adds one bounded gateway notice to the next turn. An output retention refusal names the failure
without suggesting a repeat of an already-executed paid capability.

Limits are **8 MiB decoded per asset, five distinct references / attached outputs and 40 MiB decoded
per invocation**, 32 table entries per conversation and the configurable 256 MiB default disk LRU.
An eight-MiB base64 asset may store up to 11,184,812 bytes; identity stays at 8,388,608.
Disk retention and broker in-flight spool budgets account for actual stored bytes.
These limits are separate from model-facing fetch limits. Listing metadata does not update LRU
recency; resolving bytes does. Unknown inbound lengths are shown as `size unknown` in reference
notes and as null stored-byte counts in provider listings, not as empty files. Reported or fetched
lengths, including a genuine zero, remain numeric. Listing never fetches files just to learn their
size. Reclaimed files retain their last known length and never refetch or silently substitute older pixels.

Delivery keeps existing native paths: Slack external upload, Discord multipart Create Message,
Telegram `sendPhoto`, WhatsApp Graph media/image messages, and local base64 JSON. Each upload reads
a bounded file; the shared native codec converts base64 storage to raw upload bytes, and local
encodes into its output line. There is no image-format conversion.

| Adapter | Supported declared content types |
|---|---|
| Slack, Discord, local | Any concrete syntactically valid media type; no wildcards |
| Telegram, WhatsApp | `image/png`, `image/jpeg` |

Route instructions should name the adapter's accepted types and tell the model to plan a converter
when necessary. These are adapter-supported sets, not claims about every upstream feature.
Unsupported formats refuse before uploading. WhatsApp additionally retains its 5,000,000-byte
ceiling. Filenames are gateway-generated. Only authenticated reply coordinates select the target;
a partial delivery fails the reply and suppresses durable recording. Provider result byte envelopes
are refused by capability name with a message naming `dekopon:asset`.

Slack installations need `files:write` in addition to the existing reply/read scopes. Discord bots
need **Attach Files** in addition to View/Send/Read History/Send in Threads. Telegram needs no
additional bot permission.

## Transports

### Slack Socket Mode

An app-level token opens `apps.connections.open`, which returns a `wss://` URL; a bot token answers through `chat.postMessage` or Slack's external file-upload flow for an attachment. No public HTTP endpoint is needed, which is why Socket Mode rather than a public Events API request URL. [`../examples/slack/`](../examples/slack/README.md) has separate classic/free and paid/admin-enabled Agent manifests, plus the token and identity-mapping walkthrough. The classic manifest enables interactivity, which is what delivers a `block_actions` Stop press over that same connection; without it a cancel button renders and the press reaches nothing.

`experience` controls Slack's conversation model and is never inferred from a cosmetic API result:

- `classic` (default) retains top-level DM replies and one whole-DM conversation. With native
  liveness and `classicFallback: reaction`, the gateway adds its fixed `:tangerine:` reaction to
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

An Agent transport may opt into `liveness.statusText: true` (default false, also overridable per
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
queues `active`, and sends `Stopped.` (a wall-clock stop sends its own [fixed line](#stop-causes) instead). In-flight model HTTP sends and reads, including silent
reasoning, select on the session watch and are cancelled locally. Remote inference or provider
effects already accepted cannot be rolled back. A provider command word the script is waiting on is the exception: its broker round
trip runs as one cancellable process node tied to the session's Stop, so the run is aborted and
joined and the script reads `session-cancelled` instead of waiting the broker out. Unknown,
duplicate, and other-user Stop events are ignored.

The Agent manifest also subscribes to `message.channels` and `message.groups`, requiring
`channels:history` and `groups:history`, so the transport can hear follow-ups that contain no new
mention. This is **owned-thread continuation**, not ambient activation:

- an explicitly addressed channel message proposes an exact authenticated
  `(workspace, channel, root thread, sender)` claim;
- only a fresh non-empty broker capability surface installs or refreshes that claim;
- a later unmentioned event must match every coordinate; a new turn is freshly authorized, while
  a same-sender steer joins the running leg; another sender and another thread remain ambient;
- a definitive authorization refusal removes the claim; the 1,024-entry LRU and every claim vanish
  on process restart; and
- all unmatched channel-history events are discarded inside the Slack transport before routing,
  authorization, payload telemetry, or model spend.

An inherited continuation is the only request whose reply is optional. The prompt says the message
was not addressed to the agent and offers `decline_chat_reply`; selecting it before any
capability work posts nothing, produces no transport receipt or durable recording, and remembers
the user's message as a user-only in-process turn. A decline selected in the same turn as work runs
none of that work. If an earlier capability invocation already happened, silence is refused and the
model must send a concise report. With no model turn left, the gateway posts a fixed warning that
capability work was attempted and the audit must be checked before retrying. Explicit mentions and
DMs never receive the decline tool.

The protocol's one sharp edge is redelivery: Slack expects an acknowledgment within roughly three seconds and resends the envelope otherwise. A Dekopon session takes far longer than that, so **the acknowledgment is sent before any processing begins** — before parsing, before routing, before any model call. A bounded ring of 1024 seen `(channel, ts)` pairs absorbs the redeliveries that happen anyway across a reconnect.

- `disconnect` envelopes are routine (Slack rotates sockets on its own schedule) and trigger a reconnect with jittered exponential backoff capped at 60 seconds.
- Every read has a 90-second liveness deadline. The shared recovery layer bounds the complete connection attempt (including handshake and `hello`) to 30 seconds. Slack pings a healthy connection about every 30 seconds and sends no client heartbeat of its own, so silence past the deadline means the path is gone without TCP saying so: a NAT table dropping the flow, or a partition with no RST. An expired deadline logs `gateway_transport_silent` and reports the socket closed, which is the reconnect path the backoff already owns. Without it the reader waits on a half-open socket forever and every route on the workspace goes quiet with nothing logged.
- Messages carrying `bot_id` and messages from the bot's own user identifier are dropped. Both checks matter: another app's post carries `bot_id`, and this app's own post arrives with the bot's user identifier and no `bot_id` at all.
- A subtyped message is dropped unless its subtype is `file_share`, `me_message`, or `thread_broadcast`. Most subtypes are events *about* a message — an edit, a deletion, a channel join — and answering one would answer a question twice or answer nobody. Those three are a person making a new request. `file_share` is the one worth naming: Slack stamps it on any message carrying an upload, so a question asked with a screenshot attached arrives under it. The list is an allowlist, so a subtype Slack introduces later is dropped until someone decides it is a request.
- A message's attachments become **chat assets**, described in the prompt and fetched only on demand. See [Chat assets](#chat-assets) below. The transport reports what arrived and nothing more: names and media types come from the event, so they are sender-controlled and untrusted exactly like the message text. An upload posted with no comment is a request in itself — the reference note is the whole message. A message with neither text nor a file is not a request and is dropped.
- `channel_type` decides the conversation kind: `im` is a direct message, `mpim` a group direct message, anything else a channel, or a thread under it when the message sits inside one. An `app_mention` event carries no `channel_type` at all, so the gateway asks `conversations.info` rather than assuming `channel` — a mention in a DM or a multi-person DM minted as a channel matches no `directMessage` or `groupDirectMessage` route or grant. That call is deliberately the last thing the reader does, after every drop that costs nothing, so ambient traffic never pays for it; it takes the two-second liveness deadline, is never retried, honours the app-wide 429 cooldown every other Web API call shares, and its answer is remembered for 512 conversations, oldest evicted — a conversation never turns into a different one, so a hit is never stale and a restart costs one lookup per conversation the bot is addressed in. This is why an installation needs `channels:read`, `groups:read`, `im:read`, and `mpim:read` beside its reply and history scopes. A lookup that does not answer drops that one message with `drop.reason = conversation-unresolved`, because a guessed kind is what a route and a grant would then be decided against.
- A `thread_ts` equal to the message's own `ts` is the post that *opened* a thread rather than a reply inside one — Slack stamps it on both — so that message keeps the kind of the conversation it was posted in instead of becoming a thread under itself.
- Subject: `slack.<team>.<user>`, lowercased.
- A channel answer joins the thread it was asked in, starting one on the inbound message when there is none. A classic direct message has no thread to join; an Agent direct message is intentionally rooted at `thread_ts = event.thread_ts || event.ts`, and that root also scopes admission, history, status, Stop, and owned continuation.
- An answer is posted in a Block Kit [`markdown` block](https://docs.slack.dev/reference/block-kit/blocks/markdown-block/), which carries the model's CommonMark unchanged and lets Slack render it. The `text` field is mrkdwn — a proprietary syntax where bold is `*one asterisk*` and a link is `<url|label>` — so an answer posted through it alone arrives with `**bold**` as four literal asterisks, and tables and task lists cannot be expressed in it at all. Translating in this process would be a second translation of what Slack is about to translate, so the gateway does none: the block gets the answer verbatim. `text` carries the notification fallback, the one place blocks do not render. The block caps a payload at 12,000 characters, which the 8 KiB outbound bound already sits under.

### Discord Gateway

Discord Gateway v10 is another outbound WebSocket transport. The daemon discovers the service URL through authenticated `GET /api/v10/gateway/bot`, requests the non-privileged `GUILD_MESSAGES` and `DIRECT_MESSAGES` intents, adds the privileged `MESSAGE_CONTENT` intent when the transport sets `messageContent: true`, and identifies after Hello. It jitters the first heartbeat, requires each heartbeat ACK, tracks dispatch sequence, resumes a live session after reconnect, honors Invalid Session and identify/session-start limits, and treats Discord's fatal close codes as terminal transport failures; close 4014 (disallowed intents) stops the transport with an error naming the Developer Portal toggle. No public endpoint is required. Without `messageContent`, Discord exposes content and attachments only in DMs and in guild messages whose structured `mentions` array names the bot; with it, every message the bot can see arrives with its text, and addressing still decides which ones wake a session.

- Bots, webhooks, the bot's own posts, and message types other than ordinary messages and replies are dropped.
- Absence of `guild_id` is a direct message. A guild message is a channel message and must address the bot: its user in `mentions`, or its managed role in `mention_roles` (see [How @mentions reach the bot on Discord](#how-mentions-reach-the-bot-on-discord)). Subject: `discord.<user id>`; Discord user snowflakes are global, so a guild is not part of the canonical subject.
- A Discord thread is itself a channel. Its channel ID is the route key, conversation identity, and reply destination. A catch-all channel route covers transient threads; a route naming only a parent channel does not automatically claim its thread IDs.
- Replies use `POST /api/v10/channels/{channel}/messages`. Provider attachments ride the first post as multipart attachments; the first guild post references the incoming message with `fail_if_not_exists: false`; every post disables parsed/reply mentions, so model-authored text cannot ping a user, role, or `@everyone`. Discord's 2,000-character ceiling is handled by lossless multi-message splitting, with Markdown left unchanged. Failure after an accepted attachment or first chunk is partial delivery rather than a complete receipt.
- With `liveness.mode: native`, an authorized session immediately triggers `POST /channels/{id}/typing` and renews around every eight seconds, inside Discord's ten-second native lease. Typing has no explicit clear; sealing stops renewal and the final message clears it sooner. Calls use a short deadline, honor a `429` cooldown, never take the final-message REST lock, and cannot fail the answer.

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

## Chat assets

A screenshot is part of the message that carried it. Slack, Discord, Telegram, and WhatsApp deliver it by reference rather than by value, so the gateway resolves that reference in order to hear the whole request. Slack, Telegram, and WhatsApp require the bot token they already terminate here — and on Slack the `files:read` scope, without which Slack withholds the file's id and URL and the upload is reported as one the gateway cannot open; Discord CDN downloads do not receive it. This grants no policy, no provider credential, and no way to write anything.

What it does not do is read every file that arrives. Bytes cost tokens on every turn they appear in, and most turns do not need them. So each attachment is *named* in the prompt and fetched only if the model decides the answer depends on it:

```text
what does this say?

[gateway: the sender attached
  chat-asset:1 — screenshot.png (image/png, 214 KB)
  recording.mov (video/quicktime, 41.3 MB) — not a type the gateway can show you
  Call fetch_chat_asset with the number to look at one.]
```

The model then calls `fetch_chat_asset(1)`. Because a tool result cannot carry an image — Chat Completions types a `tool` message's content as a string, and the Responses API types `function_call_output.output` the same way — the answer arrives as two messages: the tool result says the asset follows, and a `user` message carries the bytes. That shape is the only one both wire formats accept.

- **Numbering follows the exact history audience and live generation.** `chat-asset:5` means at most one file in that generation, which is what lets a follow-up three turns later resolve. Its monotonic sequence survives independent asset TTL/LRU removal while the transcript generation remains live, so a removed number cannot alias a newer file. Grant/empty-grant invalidation, idle replacement, and conversation-capacity eviction close the generation's asset fence; stale sessions cannot enumerate, publish, or fetch through it, and a replacement generation may safely number from one again. Private participants and different agents/transports/conversations cannot enumerate or fetch one another's attachments. Participants on an explicitly shared route share the live attachment inventory as well as the replayed arrival lines; that disclosure is part of choosing shared scope. Numbers are assigned by the gateway rather than by a transport; one-shot IDs are process-monotonic so expiry cannot alias an old reference.
- **Every prompt names the whole inventory**, not only what the newest message brought, with the new ones marked. A reference line is the only way a model learns a number exists, and one confined to the turn that introduced it goes unreachable as soon as ordinary chatter pushes that turn out of the replayed history window — while the store holds the file for another hour. The inventory note is not recorded: history keeps each turn's text plus one `[gateway: attached chat-asset:N — name]` line per file that arrived with it, the same line platform recall writes, so replayed turns do not each carry a stale copy of the inventory.
- **The reference line is what history remembers, not the bytes.** It is a few dozen bytes, so it replays inside the conversation byte budget instead of evicting real conversation the way a base64 screenshot would.
- **A file that cannot be shown is named anyway.** A media type outside the allowlist, a model with no image modality, or a file Slack withholds entirely all produce a line saying so, which the model can answer around instead of denying a screenshot that plainly exists.
- **Only the media types a model can actually accept are offered.** Images: `image/png`, `image/jpeg`, `image/webp`, `image/gif`. Documents: PDF, plain text, Markdown, CSV, HTML, XML, JSON, RTF, and the Word, PowerPoint, and Excel formats. A chat service imposes no allowlist on uploads at all — a 700 MB screen recording is a legal attachment — so the narrow end of that intersection is the one worth enforcing. A spreadsheet is parsed to its first thousand rows per sheet, which is worth knowing before concluding a model ignored the bottom of one.
- **A route's model has to opt in to images.** `modalities: [image]` on a model entry; the default is text only, because an OpenAI-compatible endpoint is very often a small local model that will either error or invent an answer when handed an image. Documents need no modality: a PDF is a parsed attachment to every endpoint that accepts one at all, so gating it on vision would refuse it to a model perfectly able to read it.
- **Bounds.** 8 MiB per attachment, enforced while the response streams rather than after it, because a reported size is sender-influenced and a chunked response need not declare a length. Four fetches per session. Thirty-two attachments addressable per conversation, evicted oldest-first. A textual file is clamped again on the way into the prompt, at the same 256 KiB a script's output is capped at, with a trailer saying where it was cut: the 8 MiB ceiling is sized for images on the wire, and that much `text/plain` is roughly two million tokens — enough to come back from the provider as a context-length rejection. Every one of these refuses in a sentence the model reads and can answer around, never by failing the session.
- **Redirects.** The HTTP client refuses redirects globally so a bearer token is never forwarded by policy. Slack's `url_private_download` genuinely redirects to its own file host, so that transport follows exactly one hop and re-attaches the token by hand. Both URLs are checked on one rule, at the single place the token is attached: the URL the event supplied and the `location` after it are parsed rather than prefix-matched, and each must name an allowlisted Slack host over HTTPS on the default port with no credentials in the authority. A refused URL fails the fetch before any request is made.
- **Ambient proxies.** Every transport's client is built from one `credential_client` shape that sets `no_proxy()`, so an exported `HTTPS_PROXY`, `HTTP_PROXY` or `ALL_PROXY` carries no Slack, Discord, Telegram or WhatsApp token — nor the messages and files it authenticates — through a host nobody named to Dekopon. There is no flag to opt back in; a chat service reachable only through a proxy is unreachable.
- **Gateway-owned disk LRU, weak references.** Uploads fetch once; descriptor-backed outputs join
  the same inventory with capability/invocation provenance. A bounded typed-output note names each
  retained reference; same-session and later-turn edits can address it without carrying bytes in
  the shell, prompt transcript or history. Retention and delivery are separate acts.
- **One configurable residency budget.** `sessions.assetRetentionBytes` defaults to **268435456**
  (256 MiB) across all conversations and generated/inbound assets in this gateway process. **Zero
  disables retention**: newly fetched/generated assets cannot be retained or delivered; text remains
  usable. It is never an unlimited setting. The shared 8 MiB per-file and transport/proposal limits
  remain independent safety limits, not competing retention caches. Empty files charge one byte.
  Configure disk-backed temporary storage with sufficient capacity separately from this logical budget.
- **Use means consumption.** Successful scoped resolution for model inclusion or provider submission
  refreshes recency; inventory listing and history reference replay do not. Least-recently-used
  unpinned files are reclaimed before admission. Individually oversized assets are refused without
  evicting useful entries; insufficient unpinned space refuses clearly. Active provider requests and
  outbound delivery hold temporary pins charged to the same budget. Model messages hold weak
  references, never residency ownership. Retired/expired inventories are pruned lazily; their files
  are reclaimed on the next blocking admission, once active pins finish. Unlink failures retain
  accounting rather than pretending disk space was freed.
- **Released is not unfetched.** A reclaimed asset is never silently downloaded again or substituted
  with an original input. Requested provider inputs are all resolved and pinned before submission;
  one missing input refuses the entire call. Historical model attachment parts become an explicit
  gateway-authored release notice at request encoding, letting the model choose its next action.
  Actual descriptor IO/corruption errors remain errors, not fabricated release notices. Scoped
  inventory state and at most 1024 release tombstones distinguish released, unknown and unauthorized
  IDs. Persistent IDs remain monotonic within the generation; one-shot IDs are process-monotonic
  to avoid reuse after independent inventory expiry.
- **Private scratch, not persistence.** The gateway creates 0700 directories and 0600 files beneath
  the process temporary directory. No model/provider path is accepted or serialized; reads use the
  original descriptor. No startup recovery, external cache or restart persistence is provided.
  Cleanup failures emit bounded diagnostics; abrupt process death can leave scratch until the
  temporary volume is removed.
- **Storage failures are not successful empty images.** Capacity exhaustion, OS IO categories
  (including disk-full), and changed/truncated length are sanitized failures, never paths or bytes.
  An unlinked file still reads through its original descriptor while a lease owns it. No storage
  failure triggers an automatic network fallback or paid-call retry. A provider-result spool refusal
  explicitly says the capability already executed and its attachment was not delivered; existing
  complete/partial transport acceptance rules remain in force. Outbound transports transfer all
  reply leases to a trace-contextual blocking task before delivery; reads and normal disposal do
  not occupy async workers. Cancelling the wait does not stop that task: it releases ownership
  when IO returns. Blocking model consumers retain the synchronous API. Local image answers use
  a separate reply instead of in-place progress/stream finalization; text-only answers still finalize
  in place. An unpolled transport future or runtime shutdown before queued work starts can still
  drop leases on the calling thread; this is not a general asynchronous cleanup service.
- **Disk backing is an operator requirement.** The chart uses disk-backed gateway `/tmp`, separately
  sized by `volumeSizes.gatewayTmp`; broker scratch stays tmpfs. Other installations must provide
  a disk-backed temporary directory, not a memory-backed `emptyDir` or `TMPDIR`. Existing deployed
  manifests and overrides need separate rollout verification. This removes retained bulk bytes,
  not all peak allocations: bounded downloads/decodes, textual conversion, base64/JSON request
  bodies, broker frames and multipart upload buffers still allocate temporarily; filesystem page
  cache is also outside this claim. No RSS measurement is implied.

- **Resolving a reference differs by transport.** Slack carries a private download URL on the event itself. Discord carries a signed CDN URL plus the source channel/message/attachment IDs; the CDN request carries no token, and an expired 401/403/404 URL is refreshed by re-reading that exact message through pinned Discord REST before retrying the same attachment ID. Telegram carries only a `file_id`, so a fetch is two calls: `getFile` turns the handle into a path valid for about an hour, and the bytes live under `/file/bot<token>/<path>` rather than the method prefix. The round trip happens at fetch time, which is also when that path is freshest.
- **Discord specifics.** Photos and arbitrary files share the attachment object, retaining their sender-controlled filename, optional media type, and reported size. Production downloads accept only HTTPS `cdn.discordapp.com` or `media.discordapp.net` URLs, reject credentials and redirects, and enforce the byte ceiling while streaming.
- **Telegram specifics.** A photo arrives as the same image at several sizes and the largest is the one used — a model asked to read text in a screenshot cannot read a 90-pixel-wide copy. Telegram reports no media type for a photo, so `image/jpeg` is inferred, which is what the Bot API re-encodes every photo to; a file sent as a *document* keeps its own bytes, name, and declared type. Words on an upload arrive in `caption` rather than `text`.

### Telegram long polling

`getUpdates?timeout=50&offset=N` blocks server-side and returns as soon as anything arrives, so waiting costs one idle connection. **The poll is the wakeup and advancing `offset` is the acknowledgment** — there is no separate ack call and therefore no ack-before-work problem. The offset advances past every update, including ones the daemon chose not to route, or a filtered bot message would return forever.

Messages from bots are dropped. A private chat is a direct message; a group is a channel. Subject: `telegram.<user id>`. A `message_thread_id` is a thread coordinate only when the update also carries `is_topic_message`: a forum topic becomes the distinct canonical conversation `<chat>:topic:<id>` and the reply carries that same thread ID, while a plain reply — which Telegram stamps with `message_thread_id` too — stays in the chat's own conversation. A topic id that is not a positive integer is an envelope the Bot API would never mint: it drops that one update with `drop.reason = malformed-envelope` rather than failing the poll, because the offset has already advanced past the whole batch and every update behind it would be abandoned with it.

`sendMessage` refuses text over 4,096 UTF-16 code units, which is half the gateway's own outbound bound, so an answer is split losslessly across sequential messages the same way Discord's is. Only the first quotes the incoming message; the topic identifier goes on every one, because it is what keeps a continuation in the same forum topic.

Telegram's optional `message_thread_id` is preserved consistently in admission, conversation
identity, replies, generated-photo uploads, and liveness, so a forum-topic pulse cannot appear in
another topic. Generated PNGs use `sendPhoto`; text up to Telegram's 1,024-unit caption ceiling is
accepted with the first photo, while longer text follows as losslessly split `sendMessage` calls; a
failure after any accepted part is partial delivery. With
`liveness.mode: native`, an authorized session sends `sendChatAction(action=typing)` and renews
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

The shared-prompt label says `authenticated participant` uniformly across transports. Here it means the participant claim accepted on a fresh broker leg from the owner-UID-authenticated local caller; it does **not** mean the development socket independently authenticated the declared person. The line names its own `conversation` — `{ kind, container?, id, thread? }`, defaulting to `{ kind: directMessage, id: dev }` — which makes this the one transport that can produce every kind, and therefore the one a route table, a liveness override, or a memory key can be exercised against without a chat service. Every line on this socket is addressed, because the owner-only socket mode is the authentication and a line-delimited JSON request carries no mention syntax for the routing loop's grammar to find — an unaddressed line would be dropped before it reached a route, which would leave the one transport that can produce a `channel`, `thread`, or `groupDirectMessage` unable to reach one.

## Routing

First match wins on (transport name, conversation, subjects). A conversation is where the message was posted: its `kind` — `directMessage`, `groupDirectMessage`, `channel`, or `thread` — plus the `container` above it (Slack team, Discord guild, WhatsApp `waba:phoneNumberId`; Telegram has none) and the `id` the service names. For a `thread`, the `id` is its **parent**, so `kind: [channel]` claims a channel and excludes the threads under it while `kind: [channel, thread]` claims both, and `kind: any` claims every kind the transport produces. A private Discord thread inherits its parent's grants: the grant direction is safe, because a thread's audience is a subset of its parent's.

`ids` is optional. Naming channels does not scale: one route per channel means enumerating service-native identifiers and editing this file again every time somebody creates a channel, and until an operator notices and redeploys, the bot is silent in the new one while appearing to be deployed workspace-wide. An absent `ids` says "wherever I am invited", which is membership the chat service already controls.

`subjects:` restricts a direct-message route to named canonical subjects and never widens one. It is accepted only beside `kind: [directMessage]`, because a per-person *channel* route — "in #general, Simon gets agent X" — is an access-control list by another name and the first thing somebody will trust as one; authority stays the broker's subject mapping and Cedar. A DM route **without** `subjects:` answers strangers: an unmapped sender gets a broker round trip and the fixed unauthorized refusal line rather than silence, which is a deliberate choice to make rather than one to inherit.

Unmatched messages are ignored with a debug-level `gateway_message_ignored { reason: unrouted, conversation.kind, conversation.container }` — bots see ambient traffic, and silence is the correct answer, but which kind and which container went unclaimed is what answers "why did the bot not reply in here".

**Declaration order is the whole precedence rule.** Routes are consulted top to bottom, so a named-channel route written above a catch-all keeps that channel for itself while the catch-all takes everything else — special handling in `#incidents`, the default everywhere else. Nothing sorts by specificity: a hidden ranking is how an operator ends up unable to say which route answered.

Every kind but `directMessage` — a group DM and a thread included — initially requires the bot to be addressed: `<@BOT_USER_ID>` on Slack, a structured `mentions[].id` match or the bot's managed role in `mention_roles` on Discord, or `@botname` on Telegram. **A route decides which agent answers; an explicit address decides whether a new channel conversation starts**, and widening the first leaves the second exactly where it stood. Discord and Telegram retain that rule on every message. Slack classic does too. Slack Agent has one bounded exception: after an explicitly addressed message is freshly authorized, the same authenticated sender may continue without another mention inside that exact owned root thread. A continuation starting a new turn is authorized again and may decline to post; a same-sender steer joins the running leg. All non-owned channel history is dropped before routing. The Agent manifest therefore receives ambient public/private channel events, while the classic manifest remains mention-only.

### Being available in a channel is not authority

A route matching every channel widens no authority whatsoever, and an operator reading "available in all channels" must not read it as "available to everyone". Every session opens an attested broker leg naming the sender's canonical subject; the broker maps that subject to a principal, requires policy permitting `agent.prompt` for it, and refuses an unmapped sender before any model call is made. A catch-all route changes *where* the mapped people can reach the bot. It does not change who they are, and somebody the owner never mapped gets the same refusal in a catch-all channel that they would have got in a named one.

Nor do two people in one channel share a conversation **by default**. A persistent route whose scope is omitted or `privateConversation` keys history per authenticated subject, so the bot remembers each person separately. An operator can explicitly choose `sharedConversation`; that changes prompt audience, not authority, and carries the warnings in [Scope selects the replay audience](#scope-selects-the-replay-audience). A catch-all channel does not imply that choice.

## Liveness, progress, and stopping a run

A long turn used to look like a hang. `liveness:` is what one transport shows while a session runs,
and `stopWords:` plus `limits.maxDurationMs` are how a run ends before it finishes on its own.

**One message per session.** Whatever the policy posts, it posts once and then edits: a keep-alive
is an edit, a streamed answer grows in place, and the answer finalizes the same message. There is
never a second gateway message beside it, which is why `stream: true` means the stream *is* the
surface and no separate progress line is posted.

**Native-first by default.** An absent liveness block or `mode: off` still disables all intermediate
presentation. With `mode: native`, omitted `progress` means `auto`: prefer native status, then typing,
then the configured/implemented reaction. Healthy Slack Agent status or Telegram/Discord typing
therefore needs no placeholder message; the complete answer is a fresh reply. WhatsApp remains
typing-only. Slack's definitive native-status refusal exposes its configured reaction fallback in
the same session without changing the configured conversation experience. Transient errors do not
disable native status for future sessions.

Auto permits delayed progress only without a working indicator and where an editable surface exists.
Explicit `progress: message` keeps message progress alongside the ambient indicators; `progress: off`
forbids progress prose, not indicators or answer streaming. An explicit `cancelButton: true` with Auto
also permits a message-backed Stop control alongside the indicator. Startup refuses that control
with progress Off and no stream, or on a detail-Off route without streaming, including effective
conversation-kind overrides. Existing transport-specific button/stream refusals remain.

**When prose appears.** An eligible progress message is posted on the first of a text delta, a capability call starting, a
turn that drove one, or the 15-second keep-alive tick — never on the first model turn alone, so a
fast one-turn answer never leaves a "Working on it…" message behind the reply that obsoleted it a
second later. With the stream off the text itself is never shown, so the first delta posts the
working line and later ones change nothing: a model that has started writing is the news, not what
it wrote. Every later event edits that message, coalesced to the newest state inside the
transport's own minimum edit interval, with 60 edits and 10 keep-alives per session as the bounds.

**Detail levels.** `progressDetail` is per route, because the same event stream serves a family
Discord and an operations channel:

| Level | What the message says |
|---|---|
| `off` | No progress prose. Native indicators and explicitly requested answer streaming remain enabled. |
| `plain` | The verbs only — `Working on it…`, `Running gpt-image…` — with elapsed seconds on keep-alive ticks, where the number is fresh by construction rather than frozen since the last event. |
| `detailed` | The same verbs plus turn, capability-call, and elapsed counters on every edit. |

Detail controls only eligible progress prose, not the answer stream. A transport without an editable
surface shows no prose at any level.

**Limits.** Auto avoids submitting placeholder messages, not a guarantee of notification delivery.
Message progress and streaming may notify on creation and may not notify on final edits. Existing
Discord typing cooldowns can report success without a wire send, temporarily suppressing Auto's
message fallback even when no typing is visible. Lease visibility and actual notifications are not
verified by loopback tests; no cooldown or reaction-ownership redesign is included.

**Steering acknowledgment.** Accepted steers and queued non-wake follow-ups get a best-effort 👀 on their own
inbound message when it has a liveness target. Slack uses `eyes` independently of `classicFallback`
but honors reaction-scope availability; Discord, Telegram and WhatsApp send 👀. Local keeps its
`{"reaction": true}` frame. Slack does not track eyes for progress cleanup. Acknowledgment calls run
outside the admission lock with a two-second bound; failure is debug-only
(`gateway_steer_ack_failed`, with `error = deadline` when the bound expires), never a reply.
Wakes and messages without a liveness target receive none.

**Keep-alive.** `keepAlive: { atSeconds: [15, 45], everySeconds: 60, max: 10 }` is the default: two
early ticks that answer "did it hear me", then one a minute, ten times, then silence. Each tick is
an edit of the one message. Each tick is also scheduled from the one before it, so a task that woke
late ticks late rather than firing every offset it slept through at once.

**Nothing cosmetic can hold the answer.** Every call the policy makes — reaction, typing, status,
post, edit, delete, stream, finalize — has a two-second deadline of its own. A service that accepts
the connection and then answers nothing costs the waiting person those two seconds and no more: the
call counts as that rung's failure, two consecutive failures stop that rung for the session, and the
answer is delivered either way. `gateway_progress_rendered` reports it with `category=deadline`. The
call that creates the surface — the first `post`, or the first stream render — stops its rung on the
first deadline miss instead: the transport may still land it, and a second attempt would post a
second message beside one this session holds no reference to.

**Templates.** Seven operator strings, each overridable per transport, default to the sentences
this daemon ships. `tool` permits `{word}`; `working`, `tool`, `keepAlive`, `note`, and `noteEta`
permit `{turn}`, `{of}`, `{calls}`, `{calls_max}`, and `{elapsed_s}`. Only `note`/`noteEta` permit
`{note}`, and only `noteEta` permits `{eta_s}`; neither permits `{word}`. `stopped` and `failed`
render no placeholders. `stopped` renders a person's or operator's stop only, and `failed` a failure
without one of the [stop causes](#stop-causes) below. A placeholder a field cannot render is a startup refusal naming it.
The command word comes from a provider manifest and is bounded to 32 characters. A bounded note
on a route with `progressNotes: true` is the one model-authored text admitted to a progress line;
no prompt, other capability argument, or provider result is admitted. Note text is inserted as a
literal value, never expanded as a template: a note containing `{elapsed_s}` shows those characters.
Slack, Discord and Telegram progress posts and edits disable link previews/unfurls. Discord
explicitly restores previews when the progress message becomes the answer; Telegram's edit defaults
already restore them. Ordinary answer and stream paths keep their existing behavior.

**Streaming.** With enabled liveness, `stream: true` and a driver that implements it, the model's answer appears as it
is written, cut to the transport's own character ceiling with a trailing `…` where it was cut, and
finalized in place when the turn ends. That is the transport half; the model half is `stream:` on
an `openaiCompatible` model, which is on by default and is also what lets a stop interrupt a turn
instead of waiting out `timeoutMs`. Turning it off is for the endpoint that claims
chat-completions compatibility and gets streaming wrong — a proxy that buffers the whole body, a
server that drops `usage` — and costs that model both properties; `kind: chatgptSubscription`
always streams and refuses the field.
During a reasoning phase the model may send no visible text. Model HTTP sends and reads observe
cancellation even then; remote work already accepted is not rolled back. A model-only steering
interrupt resets the local streamed partial, but text already visible in chat remains until the
next delta replaces it.

**Stopping a run.** Three ways, all of them authenticated, and all of them reaching the same
single decision:

- a **stop word** in the conversation. `stopWords:` defaults to `[stop, cancel]` and is an operator
  list because the people talking to a deployment do not all speak English. A message matches only
  if it is *exactly* one of those words after the bot mention and any trailing punctuation are
  stripped, case-insensitively — so `<@U0123> stop.` in a channel matches and "we should stop doing
  that" does not. It stops that sender's running turn or removes their pending collected/queued
  input. With no matching session or pending input, "stop" follows ordinary routing; an already
  cancelled session ignores a repeat stop rather than starting another turn;
- a **cancel button** on the progress message, where `cancelButton: true` and the service has
  interactive components. Slack's Agent experience renders its own Stop instead, which is why the
  flag is refused there. The value on a Slack button and the `custom_id` on a Discord one are the
  conversation key the transport minted when the message arrived, carried on the liveness target
  rather than re-derived at press time — a second spelling of that key is a press the admission
  map does not recognise. A Discord thread's key is `parent:thread`, and because a thread is
  itself a channel the press arrives in the thread: it is matched against the key's last segment;
- `limits.maxDurationMs`, counted from the moment the agent starts working rather than from
  receipt, because waiting behind another turn is not the agent taking too long.

Only the subject who started a run may stop it. User stops also discard that subject's queued
steers and follow-ups, leaving other senders' work intact; budget or operator cancellation does not
clear the mailbox. Another person's press cannot stop the running turn and is recorded as
`gateway_session_stop_ignored`, though it can remove their own pending input. A stop is cooperative,
not rollback: local model I/O is cancelled, while remote inference or provider effects already
accepted may still finish and their result is suppressed. Cancellation suppresses that turn's model
answer. Once the loop claims completion, a stop cannot cancel that answer, even during delivery. It still removes that sender's
pending input. A stop word that removes pending input can receive `Stopped.` through the bounded
refusal-reply path; buttons/native Stop acknowledge through their transport. With nothing pending,
a stop during completion is ignored with reason `already-ended`.

## Detached jobs

A route opts into jobs with `limits.jobTimeoutMs`; without it, `&`, `jobs`, `wait` and
`kill` report that jobs are off. `cmd &` starts a job that outlives its script and turn,
prints `[N]`, and sets `$!`. `jobs` lists the person's jobs in that conversation;
`wait %N` waits without a completion notice, and `kill %N` stops one. A finished job
not waited for sends a bounded notice as a separate turn or a steer; a busy gateway
may drop it, so `jobs` is the fallback. `sessions.maxJobs` (default 2) bounds running
jobs and retained rows across the process. Jobs and their rows are lost at restart.

A job inherits its starting scope, but its variables and buffers die with it; only its
printed output reaches the notice (at most 16 KiB of inbound text). Jobs have no chat
asset inputs or reply attachment slot and cannot send files or images. A job started
by a probe remains read-only; the broker refuses write capabilities under trigger
`probe`. A notice turn carries trigger `job` and may itself start jobs: on a route
with `jobTimeoutMs`, an agent can keep itself alive through successive notices.
The route key is the explicit opt-in; the person's stop word cancels their running
jobs in that conversation, including when no turn is active.

## Sessions

Each admitted turn runs one session; same-sender messages can steer that turn, while other senders
and wakes get their own follow-up sessions. On a `oneShot` route (`memory: { mode: oneShot }`) each
session starts without prior history; the `persistent` clauses in steps 4 and 5 describe replay:

1. **Admission.** A process-wide semaphore bounds concurrent work. The per-`(transport, conversation)` map is checked first: the same sender steers a running turn; other senders, wakes, and input arriving after cancellation or completion is claimed queue as follow-ups. Together steers and follow-ups hold at most eight items. Only a full mailbox (`busy.cause = same-conversation`) or lack of a permit for a new conversation (`saturated`) refuses admission. A refusal is eligible for `I'm busy — try again shortly.` when `replyOnBusy` is set; a collected batch is eligible regardless. Sending still requires a free refusal-reply permit and successful transport delivery. Follow-ups run inline under the existing permit, with their own route, subject, fresh cancellation, broker leg and receipt trace. A conversation with a steady stream of messages keeps its permit until its mailbox is empty.
2. **Authorization.** The session opens an attested broker leg with `capabilities(subject, agent, scope)`. If the answer is empty — or the broker refuses, because the attestation was not honored or because policy does not permit this principal to drive this agent — the sender gets `You're not authorized to use this agent.` and **no model call or session progress starts**. Queued or steered input may already have received its admission acknowledgment, which does not signify authorization.
3. **Liveness.** When the transport opted in, one session-owned policy task starts immediately after the fresh grant. The service renders everything; the model supplies no target, wording, emoji, cadence, or timing, apart from an opted-in progress note. The policy owns one message, spends the session's edit budget, seals synchronously before terminal delivery, and returns the service's own indicators to rest afterwards, so cosmetic I/O never delays the reply or holds admission. Two consecutive failures stop that surface for the session; permanent Slack installation failures additionally trip a transport-wide fallback breaker. What it shows is [Liveness, progress, and stopping a run](#liveness-progress-and-stopping-a-run).
4. **Execution.** On a `persistent` route the session first looks up its conversation under the key in [Scope selects the replay audience](#scope-selects-the-replay-audience). An entry idle past the route's timeout, or built under a granted capability set that differs from the one this message's leg just reported, is dropped rather than used; whatever survives is seeded into the prompt ahead of the new message as compacted `(question, answer)` pairs, oldest dropped first until the window's turn and byte bounds both hold. A shared turn's user text starts with a gateway-authored participant label naming the sender's broker principal, for both the current message and later replay. The lookup happens *after* step 2 because the grant comparison needs a fresh grant to compare against. Then the model client is built from the route's model, the shell runtime is given the attested leg as its only capability dispatch, the credential-free `inspect_agent_config` view is built from the same fresh leg and offered only where the route wrote `inspectAgentConfig: true`, the scoped asset table and request-local explicit-send slot are attached to that leg, and the prompt loop runs on a blocking task with the agent's `instructions` as the system prompt, followed by the chat-memory note when memory is granted, a note that the chat shows only PNG and JPEG on WhatsApp and Telegram, and one line the gateway renders from the route's limits: `[gateway: N steps, M capability calls and W minutes per message.]`, the minutes only when `maxDurationMs` is set. The agent's catalog skills ride the bound route — read whole into memory when the catalog loaded and shared by every session rather than re-read, so a session never touches the filesystem — and are mounted on every session on that route: a second system message after the instructions lists each by name and description, and the `read_skill` tool loads one skill's instructions, or one of its resource files, on demand. Every session also offers `suggest_improvement`; what it records is written to telemetry as `agent.improvement.suggested` and is never relayed to chat, so the sender sees only the answer. Instructions are supplied fresh for each session and never stored in history; steers use that session's instructions. Shell bounds are `dekopon-shell`'s defaults except `maxCapabilityCalls` and the script deadline, which both come from the route. Every model request declares a [prompt cache key](#the-prompt-cache-key) (`session_id` on OpenRouter) — the conversation's on a `persistent` route, the route's on a `oneShot` one.
5. **Answer, silence, and optional durable recording.** A required session's final bounded text and accepted provider attachments go back to chat. An inherited Slack Agent continuation may instead call `decline_chat_reply` before capability work, which commits its user-only in-process turn, removes the progress message, and sends no reply request. On failure the sender gets one fixed line: a [stop cause's sentence](#stop-causes) when it has one, otherwise `The agent could not complete this request.` — a `PromptError` can carry model-chosen text, a provider message, or a transport diagnostic, and chat is the last place any of those belong. The operator reads the category from telemetry. A `persistent` route writes only the textual exchange back as one more in-process remembered turn, trims the window, and restarts the idle clock. A generation lease makes a commit from older in-flight work inert after grant invalidation, empty-grant removal, idle replacement, or capacity eviction, while concurrent work in the same generation appends in completion order. **The fixed failure line and attachment bytes are never stored.** A declined or failed model session records its question with nothing in the in-process answer's place, which is truthful and is what makes a later follow-up answerable; a session refused at step 2 records nothing at all. Optional durable recording happens under the conditions in [Durable memory after transport acceptance](#durable-memory-after-transport-acceptance).

`steering: abort` is the route default: a same-sender message interrupts only the in-flight model
call, never a tool, script or provider call. `boundary` waits for the current step to finish. Both
read steers before the next model call, and replace a draft answer when another step remains.
An interrupted call does not spend `maxSteps`. Each run requests at most eight model interrupts;
after that, steers wait for the next boundary. A follow-up starts with a fresh interrupt budget.
Session duration and per-call timeouts still apply.
Steers left at the last step, decline or failure fold into a follow-up under their original sender.
Consumed steer text joins the turn's user text in history and the journal.
When the newest turn alone exceeds the in-process byte window, its user text is shortened to a
UTF-8-safe prefix ending in `[…]` if the intact answer and marker fit; otherwise whole-turn eviction
still applies.

Text is bounded in both directions: inbound to 16 KiB keeping the head (a chat message states its request first), outbound to 8 KiB keeping head and tail (an answer's conclusion is usually its last line). Both truncations say so in the text.

Follow-ups are memory-only and continue inline during shutdown grace. When `abort_all` ends that
grace, queued receipts become `abandoned`; queued wakes are lost and are not replayed.

At shutdown, transport readers are aborted and in-flight sessions get `shutdownGraceMs` to finish — a model call is already paid for, and abandoning it means a person watching a chat window never hears back. If the grace expires, dropping each async owner marks its synchronous prompt loop cancelled before aborting the wrapper, so no later model turn or capability call starts. A model request or provider effect already in progress remains non-rollbackable and may finish after the async owner is gone.

**Abandonment is bounded, so exit is too.** The async model transport observes the session watch even during a silent read, while other blocking work (including credential/file IO) can still drain after cancellation; dropping an async runtime waits for its blocking threads. The daemon therefore owns its runtime and gives that final wait five seconds before exiting anyway. Without it, worst-case exit is `shutdownGraceMs` plus a model timeout — on the reference deployment 120 s + 120 s — inside a pod termination grace that `dekopon-brokerd`'s own drain has to fit into as well, because the kubelet only starts stopping the broker sidecar once this container is gone. The chart defaults that grace to 270 s and asserts at template time that it covers both drains plus `drainBudget.bufferSeconds`, refusing to render anything shorter ([Kubernetes guide](kubernetes.md#draining-takes-both-graces-in-sequence)); 240 s of gateway abandonment does not leave the broker its 120 s inside it, and the kubelet SIGKILLs the broker mid-drain.

**Exhausting one transport is a gateway failure.** Healthy transports keep serving during a peer's
bounded recovery episode. A permanent refusal, exhausted episode, or reader-task panic stops the
whole gateway through the same bounded drain as shutdown, but with a nonzero exit. See
[Connection recovery](#connection-recovery).

### Stop causes

A session that ends without answering carries its cause to chat and to the next turn. The chat line
is fixed per cause and never carries error text:

| Cause | Chat line |
| --- | --- |
| A person's stop or an operator's | `liveness.templates.stopped` (`Stopped.`) |
| The route's `maxDurationMs` | `Stopped: this session reached its time limit. Capability calls already made were not undone.` |
| A model call past its deadline | `Stopped: the model did not answer in time. Capability calls already made were not undone.` |
| Any other model failure | `liveness.templates.failed` |
| An empty answer | `Stopped: the model returned an empty answer. Capability calls already made were not undone.` |
| The route's `maxSteps` | `Stopped: this session reached its step limit. Capability calls already made were not undone.` |
| A lost session task | `Stopped: the gateway lost this session's task. Capability calls already made were not undone.` |

Every other failure still sends `liveness.templates.failed`, and an over-budget or unreported-work
failure its own sentence. The next message in the same conversation reaches the model once with one
line before its text, `[gateway: the previous turn stopped before answering: <reason>. Capability
calls already made were not undone.]`. The notice lives in memory only: it is never journaled,
never in history, and a restart loses it.

A wall-clock stop also seals the conversation. The resident window is evicted (reason `sealed`), and
a marker records when: beside the journal as `<stem>.sealed` when `sessions.journal` is set, in
memory otherwise. A sealed thread answers every later message, a queued follow-up included,
with `Sorry, this agent took too long and we've canceled the chat. Please feel free to start a new
one with a smaller scope.` and outcome `sealed`, without reaching a model. A direct message, group
direct message or channel is never refused: its next message starts fresh, with platform and
journal recall skipping everything at or before the seal, and the notice above is the only trace
of the stop.

## Conversations

**Status: Current.** History is a trust surface rather than a feature flag; [`security-model.md`](security-model.md#conversation-memory-as-a-trust-surface) states the surface it accepts.

A `persistent` route keeps a bounded history and replays it into the next prompt, so a follow-up question can say "and the second one?" and be answered. The history is private per authenticated subject by default; `scope: sharedConversation` shares it among authenticated participants in one exact routed conversation. `persistent` with the default window is the route default; `memory: { mode: oneShot }` opts out.

### The history lives in the gateway

This subsection describes the automatic replay window, not the separate on-demand durable provider. The live window sits in the daemon's memory and is never sent to the broker. The broker holds provider credentials and decides every invocation; conversation text there would put the most sensitive content in the system inside the most privileged process. The gateway already read the message and wrote the answer, so keeping the history there adds no new reader.

`idleTimeoutMs` is how long a window stays in memory, not how long it is remembered. When a message finds no window in memory (first contact, idle expiry, capacity eviction, or a restart), the route's `recall` rebuilds one before the model runs:

- `none` starts empty. It is the default without `sessions.journal`, and it is the only behaviour older releases had.
- `journal` reads the gateway's own on-disk transcript. With `sessions.journal` set, every committed exchange on a journal route is appended to one JSONL file per state key, named by a SHA-256 of that key: directory `0700`, files `0600`, no fsync. Each line carries the conversation's attachment inventory at that moment and its next `chat-asset:` number; recall uses the newest line's. Only the trailing run of exchanges recorded under the message's current grant is recalled, so a narrowed grant never replays output from a wider one. Exchanges older than `forgetAfterMs` are skipped. A file past twice the route's `maxBytes` plus 64 KiB is rewritten to the window. At startup and on every recall, files not written for longer than the longest `forgetAfterMs` of any journal route are deleted. A file that does not parse, including one from an older release's format, is deleted, and that message starts empty. A wall-clock stop writes a sibling `<stem>.sealed` file, `0600`, holding `{"atMs":<u64>}`; recall skips every line at or before it, the same retention deletes it, and one that does not parse is deleted. See [Stop causes](#stop-causes).
- `platform` asks the chat service for the conversation's recent messages (Slack `conversations.replies` or `conversations.history`, Discord `GET /channels/{id}/messages`), so the model sees what the person sees, other participants included. It reads at most `min(2 × maxTurns, 100)` messages, waits at most 5 s, and a read that fails answers from an empty window. Only Slack and Discord have a history API; startup refuses `platform` on WhatsApp, Telegram, and local routes. Discord returns other people's text only when the application has the Message Content intent enabled in the Developer Portal, whether or not the transport sets `messageContent`; without it, those messages are skipped. Each author is labelled with the broker principal it maps to, `[gateway: chat history, from simon]`: for every distinct author other than the sender, the gateway asks the broker for that author's `capabilities` in this chat scope and reads only `principal`, inside the same 5 s. An author no principal names keeps `[gateway: chat history, from <service user id>]` on a private route and becomes `[gateway: chat history, from unmapped participant]` on a `sharedConversation` route, so no platform id reaches the model there. Either label is service-reported authorship, not broker authentication, and each unmapped lookup logs `broker_capabilities_refused` with `reason: unmapped-subject` in the broker.

On a `platform` route a window already in memory also catches up. The gateway remembers the newest chat message each window took in (the trigger, or the newest recalled message for a wake) and the messages steered or folded into its turns; a later commit never moves it back or forgets those. When a message from the chat service finds the window resident, a mention or a thread continuation alike, the gateway reads the messages strictly after that watermark and before the trigger, under the same `min(2 × maxTurns, 100)`, 5 s and `forgetAfterMs` bounds, drops the bot's own messages and any it already took in, and keeps the newest that fit in `maxBytes`. They reach the model as one turn of their own ahead of the message, the same `[gateway: chat history, from …]` block the cold read produces, attachments numbered as `chat-asset:N` like any other; on a `sharedConversation` route that turn carries no `authenticated participant` line, so other people's words never sit under the sender's. The window records that turn with the exchange, so the next mention reads only what came after. Slack bounds the read with `oldest` and `latest`; Discord pages with `after`, cuts at the trigger, and when one page of 100 does not reach the trigger, reads the newest messages before it instead. A failed read logs `gateway_recall_failed` and the turn runs without the block.

A recalled window brings its attachments with it, under the same `chat-asset:N` numbers the replayed turns use, and bytes are still fetched only on demand. An attachment that arrived before `forgetAfterMs` is not recalled. WhatsApp media ids expire after 7 days, so with a longer `forgetAfterMs` an older WhatsApp photo is named for that one recall but can no longer be opened, and the next journal line drops it.

### Scope selects the replay audience

The effective keys are intentionally exact:

- `privateConversation` (the default): `(agent, configured transport, transport-derived conversation identity, canonical subject)`;
- `sharedConversation`: `(agent, configured transport, transport-derived conversation identity)`.

The agent boundary means two agents never share transcript or attachment state even when they are routed on the same transport conversation. The configured transport boundary prevents lookalike identities from different installations or services from aliasing. Private scope adds the canonical subject from the transport envelope accepted for the fresh broker leg, so one participant claim never receives another's history. Shared scope removes **only** that subject component; it is not agent memory, team memory, a namespace shared by routes, or any replay beyond this exact conversation.

The conversation identity is the transport-minted conversation's own key — `id`, or `id:thread` when the answer lands in a thread — not mechanically `(channel, thread)`. Slack omits `thread_ts` on the message that *starts* a thread and sends it on every reply inside one, while the bot answers that first message in a thread rooted at it, so the key is the thread the answer joins rather than the raw field. What each kind keys on, and who a shared key lets in:

| kind | `privateConversation` | `sharedConversation` | audience the shared key implies |
|---|---|---|---|
| `directMessage` | (agent, transport, key, subject) | refused at startup on a `[directMessage]`-only route | none: the direct message already is the subject |
| `groupDirectMessage` | (agent, transport, key, subject) | (agent, transport, key) | every mapped member of the group |
| `channel` | (agent, transport, key, subject) | (agent, transport, key) | every mapped member who can invoke the route there; a Discord guild channel whole |
| `thread` | (agent, transport, key, subject) | (agent, transport, key) | the thread's participants; a private Discord thread inherits its parent's grants |

Choosing shared scope on a broad Discord channel can disclose one participant's prior prompt and the agent's answer to every other mapped participant who can invoke that route there. Treat that as an explicit audience expansion, not as a convenience toggle.

Shared user turns are sent to the model as exactly:

```text
[gateway: authenticated participant: <principal>]
<existing user text>
```

`<principal>` is the broker principal the attested sender maps to, which the broker names in its `capabilities` answer on that fresh leg; the gateway prints it and decides nothing from it. When the broker names none the line is `[gateway: unmapped participant]`, with no platform identifier. The gateway writes the first line from the transport envelope accepted for the fresh broker leg after composing any attachment reference note, and writes the same line on the recorded turn, which carries arrival lines instead of the note. User text remains untrusted and may contain a lookalike label; it cannot replace the gateway-authored first line. The labelled bytes are retained in the bounded history, so replay preserves who said each turn and the label counts against `maxBytes`. Private persistent and one-shot prompt text is unchanged byte for byte and receives no label.

**The participant's principal name reaches the model provider on every shared turn.** The telemetry gate controls exports to the configured telemetry sink, not the prompt sent to the selected model endpoint. Platform identifiers such as phone numbers stay out of the label. Enable shared scope only when sending those names, earlier participant text, and agent answers to that model provider is acceptable for the whole conversation audience.

The state key is *not* the admission key from step 1, which is `(transport, conversation key)` and has no agent, scope, or subject. The two keys answer different questions. Serialization asks "is this bot already busy on this thread"; state asks "which exact route audience owns this transcript and attachment inventory". Admission therefore does not serialize every possible shared-state race. The store gives each session a generation lease and attachment-access fence: sessions in one live generation append in completion order and reuse its inventory, while removal, replacement, or eviction makes every older lease inert and closes its asset fence. Stale in-flight work can therefore neither recreate forgotten history, rename its cache lane, publish into a replacement inventory, nor start a metadata/byte fetch through a retired one. A transport read already started while the generation was live may finish concurrently, but a final fence check discards those bytes instead of sending them to the model after retirement.

### Prior turns are compacted

A stored turn is `(the user's message, the final answer)`, or the message alone when the session failed or declined an optional reply. Every intermediate step — the model's tool calls, the scripts it authored, and their output — is dropped at write-back and never replayed.

The number that forces this: one script's combined output can reach 256 KiB, which is `dekopon-shell`'s default `max_output_bytes` in [`../crates/dekopon-shell/src/limits.rs`](../crates/dekopon-shell/src/limits.rs). Replaying full transcripts would let a single earlier turn cost more than the entire window budget, and it would do so most on exactly the sessions that did the most work.

The loss is real and worth naming: the model cannot re-read a command it ran three messages ago, only what it said about it. If it summarized badly, the bad summary is what persists. A `persistent` route buys continuity of conversation, not continuity of evidence — the broker's audit log is where what actually happened is recorded.

### The bounds

| Setting | Where | Bounds |
|---|---|---|
| `mode` | route `memory:` | `persistent` (default) or `oneShot` |
| `scope` | persistent route | `privateConversation` (default) or explicit `sharedConversation` |
| `idleTimeoutMs` | persistent route | How long an untouched conversation survives; default 900000 |
| `maxTurns` | persistent route | Exchanges the window replays; default 12 |
| `maxBytes` | persistent route | Bytes the window replays; default 65536 |
| `recall` | persistent route | Where a window not in memory is rebuilt from: `none`, `journal`, or `platform` |
| `forgetAfterMs` | persistent route | Oldest exchange or chat message `recall` may bring back; default 604800000 |
| `maxConversations` | `sessions:` | Conversations the process tracks at once; default 1024 |

`maxTurns` and `maxBytes` both apply, oldest turns dropping first until both hold. Two bounds because they fail differently: twelve one-line exchanges and twelve paragraph-length ones are the same number of turns and very different prompts.

`maxConversations` lives under `sessions:` rather than in the route block because it is a property of the process, not of a route, and `sessions:` is already where "what this daemon costs at once" is configured. It is a memory bound and not an admission bound: reaching it evicts the least recently used conversation rather than refusing a message, because a person in the middle of a conversation matters more than one who stopped an hour ago. An eviction is logged as `gateway_conversation_evicted` with a reason, so a ceiling set too low is visible as churn instead of as a bot that intermittently forgets.

Neither eviction runs on a timer. There is no sweeper task and no shutdown hook: the idle timeout is checked by the lookup that would otherwise have used the entry, and the ceiling is enforced by the write that would otherwise have exceeded it. Closing a conversation generation makes its asset metadata and byte source immediately inaccessible through every stale session; the independently bounded asset map may retain that inert metadata until its next operation prunes it. All state is process memory and dies with the process.

### Authorization is never cached

Every new turn opens a fresh attested broker leg and gets a fresh chat-scoped `capabilities` answer, exactly as step 2 describes; same-sender steers use the running turn's leg. Persistence changes nothing here: no grant is remembered, no decision is carried forward, and history is prompt text rather than authorization input.

The granted capability set is additionally **stored with the conversation** and compared at each new turn. Any difference drops the history and attachment generation and starts a fresh conversation; an empty grant removes the entry outright and closes the same asset fence. The reason is narrow and specific: output and attachment references from a session with a broad grant are sitting in the retained state, and if the owner then narrows what that subject may reach, an unchecked entry would keep replaying or fetching them after the capability that produced them was taken away. Invalidation costs a cache miss on the first new turn after a policy change, which is the right price — a narrowed grant is precisely when replaying old output is wrong. This comparison remains conservative on a shared route: if two participants receive different capability identifier sets, moving between them resets the shared transcript and inventory rather than carrying state produced under the other set. Sharing never promotes either participant to the other's grant.

Its reach is exactly the granted capability *identifiers*, which is less than it sounds like. A policy edit that keeps the same capability list but tightens its owner-authored constraint set — a narrower allowed host, a smaller output ceiling, a different credential — produces an identical grant set and does not drop the history. Text fetched under the older constraints stays in the prompt until the window or the idle timeout removes it.

### Why fifteen minutes

The idle-timeout default is pulled in two directions and loses one of them.

The ChatGPT subscription endpoint publishes no prompt-cache lifetime. Public OpenAI API policies vary by model and retention mode, so tuning a user-visible memory timeout to one guessed provider TTL would couple two mechanisms that do not share a contract. Human conversational memory runs on a longer clock: someone who asks a follow-up after a meeting expects the bot to know what they were discussing, and a bot that forgot after a brief lull is the failure people report.

The default is 15 minutes, which resolves toward the person because the user-visible point of this feature is memory, not a cache hit. **The cost control is the window, not the cache:** `maxTurns` and `maxBytes` bound what any one message pays no matter how long its conversation has been alive. [`inference.md`](inference.md#provider-retention-what-can-be-said) records the public API comparison, the undocumented subscription boundary, and why keeping a process alive does not pin a provider cache.

### The prompt cache key

Every model request carries the key: as `prompt_cache_key` on Codex and OpenAI-compatible backends, as `session_id` on OpenRouter. **It is a routing hint and never an access-control boundary.** It tells the provider which requests are likely to share a leading prefix so they can land on one cache; it authorizes nothing, isolates nothing, and hides nothing. The request carries the whole conversation either way, and a backend that ignores the field still evaluates the whole request without that affinity hint. Sharing a key grants nothing: each new session opens a fresh attested leg; steers share only their running session's leg.

**It carries nothing about the private subject or shared conversation identifier.** The key is an opaque identifier *minted* when the thing it names is created — not either audience coordinate, not a hash of one, not a salted one. A canonical subject can be a phone number, so sending it would hand a model provider the sender's identity in exchange for routing that happens anyway; hashing it does not fix that, because a hash of a stable subject is a stable pseudonym. A configured salt is worse again: a new secret to manage whose only purchase is a pseudonym that survives restarts.

Where it comes from, and how long it lives:

| Route mode | Key names | Minted | Rotates when |
|---|---|---|---|
| `persistent` | one scoped conversation: private `(agent, transport, conversation key, subject)` or shared `(agent, transport, conversation key)` | with the conversation entry | the entry is evicted — idle, capacity, changed grant, or empty-grant removal — or the process restarts |
| `oneShot` | one bound route | once, at startup, when routes bind | the process restarts |

Rotation keeps it from becoming a durable identifier for a person or service-native shared conversation, and it is also just correct: an evicted conversation rebuilds a prompt that shares no prefix with the one it replaced, so continuing to name the old lane would be a guaranteed miss.

A `oneShot` route's key is shared by **every sender that route answers**. That route's shared prefix is the agent's `instructions`, the skills listing when the agent mounts any, and the tool definitions, then this one message: the shared part is identical for everyone the route serves and contains nothing about any of them. Nothing sender-specific can hit — a different sender's message diverges from the first token that differs, and a cache key is a hint about a shared *prefix*, not a handle on somebody's answer. A fresh key per message would name a lane holding exactly one request and give up the only caching a stateless route can have.

What the key is worth is measured, not assumed — [`inference.md`](inference.md#how-to-evaluate-caching-in-a-deployment) has the counts and how to read them.

### What this means for retention

On a `persistent` route, chat text sits in `dekopon-gatewayd`'s memory for at least the idle timeout after somebody stops talking — on the default, fifteen minutes of a person's question and the agent's answer. With shared scope, that retained content and its attachment inventory belong to the exact conversation audience rather than one sender. **At least**, because eviction is lazy: an abandoned conversation is dropped by the next lookup on its key or by the ceiling displacing it, so with neither happening the bytes stay in the process until it exits. What a timed-out entry can never do is reach a prompt. Without `sessions.journal` the daemon writes none of that conversation text to disk (active attachment payload leases use private temporary files); with it, journal routes keep their windows in the journal directory until the file is compacted, evicted, or deleted by the operator; the operating system's own paging and core-dump behavior are outside what the daemon controls. Another process under the gateway UID is inside its trust domain; see the [current process boundary](#current-process-boundary).

## Wakes

A route with `wakes: true` offers the model a `wake` tool, so an agent can come back to a
conversation later without anyone writing to it. A wake is a synthesized message: it carries the
subject, conversation and reply target of the authenticated message that scheduled it, and the
model chooses only when and the note it will read. It never names a person or a place.

- `schedule` wakes the agent once, `afterSeconds` from now.
- `watch` runs a probe script every `everySeconds` (at least `minIntervalMs`, at most `forSeconds`)
  for up to `forSeconds`, with no model in the
  loop. Exit 0 wakes the agent with the probe's output; exit 1 keeps waiting; any other exit, or
  output over 8 KiB, wakes it with the failure and retires the watch. `$PREV` holds the previous
  run's output. The first run happens while scheduling, with `$PREV` unset: it never wakes the
  agent, and a probe that exits 2 or more there is refused rather than stored.
- `list` and `cancel` see only the asking person's wakes. The tool is the only way to see them.

Every probe run opens its own broker leg attested with `trigger: probe`. The broker neither lists
nor authorizes a capability that is not read-only on that leg (`probe-write`), whatever policy
says. The woken session attests `trigger: wake` and is authorized afresh, like any message, so
a wake can do nothing the person could not do by typing; owner Cedar policy can narrow it further
through `context.trigger`.

Pending wakes live in one JSONL file (`0600`, rewritten through a rename, no fsync). A due wake's
row is removed before its session starts, so a restart at that moment loses it; nothing is ever
delivered twice. A due watch is leased by moving its next check forward in the same write, so one
watch never runs two probes at once. Ticks take a `sessions.maxConcurrent` permit and are skipped,
not queued, when none is free. A line that does not parse refuses startup; deleting the file
resets every pending wake. On WhatsApp a wake cannot be scheduled past 24 hours after the message
that asked for it, because Meta refuses free-form messages outside that window.

A wake bypasses stop words, the addressed filter and media collection. Behind a running
conversation it queues as a follow-up if there is mailbox room, never as a steer; an idle conversation
starts its turn directly if a permit is free. Wakes receive no acknowledgment. The same mailbox
and saturation refusals apply. Only when its route no longer answers there as
the same agent with `wakes: true` does the gateway post the note and any probe output without a session. The local development
transport cannot schedule wakes: its reply target is a connection number a restart reuses. Provider chat memory does not record wake turns;
the resident window and the journal do. Platform recall for a wake reads the history up to now.

## Durable memory after transport acceptance

The gateway receives an optional `ChatMemorySurface` only when the agent is enabled and the broker
freshly permits all three exact memory capabilities under a matching subject namespace,
a canonical transport and conversation claim, a storage constraint, and Cedar context. The storage namespace's two
conversation-derived scope values are `channel := conversation.id` and
`conversation := conversation.key()`, so a Slack channel and a non-thread Discord channel keep the
namespaces they had in 0.13, while WhatsApp, Telegram topics, and Discord threads change shape and
start empty ([upgrading.md](upgrading.md)). Otherwise recent/search, the `memory` word, the prompt note, durable
recording, and namespace creation are all absent.

When present, the model may retrieve on demand:

```text
memory recent --last N
memory search --query TEXT
```

It cannot resolve or invoke record. After model success, the gateway bounds the final answer once
(empty output uses the fixed normal answer), asks the transport to accept those exact bytes, and
only then opens one fresh broker client for one `recordDeliveredTurn` carrying the session's chat
attestation. The recorded user text is the original bounded sender text followed by each consumed
steer's raw text, joined by blank lines, excluding gateway timing and attachment reference notes.
If the combined user and assistant text exceeds the existing 64-KiB recording ceiling, only the
recorded user text is shortened to a UTF-8-safe prefix ending in `[…]`; space is reserved for that
marker and the entire accepted assistant text. This recording bound leaves prompt history
unchanged.
Assistant text is exactly what the transport accepted. No response,
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
prompt. Recording never deduplicates: a redelivered message becomes a second stored turn. There is
no deletion/export UX or encryption-at-rest claim.

## Authorization flow

```text
chat service            authenticates the sender
      |
      v
dekopon-gatewayd                subject = ExternalSubject::{slack,discord,telegram,whatsapp}(...), or the local caller's declared subject (routing metadata, not authority)
      |                 agent   = the route's catalog agent
      |
      | capabilities(subject, agent, scope)  ── empty ⇒ refuse, no model call
      | invoke(proposal, subject, agent, scope)
      v
dekopon-brokerd         attestor grant bounds the namespace
                        principals turn the subject into a principal
                        policy must permit agent.prompt for that principal and agent
                        policy conditioned on context.via decides what it may then reach
                        credentials resolve, the provider executes, audit records it
```

The broker is the sole authority. `dekopon-gatewayd` supplies the subject and never the principal; a refused attestation is an audited denial recorded against the gateway's own peer identity. Driving an agent at all is its own policy statement — `Dekopon::Action::"agent.prompt"` over `Dekopon::Agent::"<name>"` — so a mapped subject the owner never permitted to use this agent is refused before the capability listing is even assembled, and a chat-attested `invoke` under such a session is the audited denial `agent-denied`. See [`security-model.md`](security-model.md) for the complete attestation contract, and note in particular that **a policy written for direct peers can never authorize an attested context and vice versa** — adding a gateway cannot widen a grant that already existed.


## Token budgets

An optional `metering` block gives an agent a token budget. A model call that would exceed it is
refused before it is sent, and the turn ends with a sentence that says why:

> I'm at 99% of my token budget (5-hour rolling window): 10 tokens left, this message needs about
> 100. Try again in 15 minutes.

```yaml
metering:
  budgets:
    gylmar:                       # agent id; it must be served by a route
      models: [astra, terra]      # optional; default every model
      meters:
      - {kind: rolling, limit: 200000, period: 5h}
      - {kind: fixed,   limit: 1000000, period: 1d}
      - {kind: session, limit: 300000, length: 5h}
      - {kind: credit,  capacity: 2000000, refill: 500000, per: 1d, initial: full}
  restore:                        # optional; see below
    kind: openobserve             # or quickwit
    endpoint: https://openobserve.openobserve.svc.cluster.local:5080/openobserve
    org: default
    stream: dekopon               # quickwit: `index: otel-logs-v0_9`, and no org or authEnv
    authEnv: DEKOPON_METER_SEARCH_AUTH
    delay: 30s                    # default 30s
    timeout: 10s                  # default 10s
    lookbackMax: 7d               # default 7d
```

| Meter | Allows a call when | A refusal waits |
|---|---|---|
| `fixed` | the current window's spend plus the call fits `limit`; windows start at UTC multiples of `period` | until the window ends |
| `rolling` | the spend in the trailing `period` plus the call fits `limit`; spend is kept in buckets of `period / 60` and leaves the window when its bucket does | until enough of the oldest buckets leave |
| `session` | no session is open, or the open session's spend plus the call fits `limit`; a session opens on the first charge and lasts `length` | until the open session ends |
| `credit` | the balance, refilling at `refill` per `per` up to `capacity`, covers the call | until enough has refilled |

A budget allows a call only if every meter does; the refusal names the meter with the longest wait,
and a call larger than a meter's whole limit is refused outright ("It can't run as is."). Durations
are `humantime` strings (`30m`, `5h`, `1d`) of at least one minute and at most 366 days. `initial`
is `full` or a token count no greater than `capacity`. Unknown keys are denied, and every unknown
agent, unknown model, empty `meters` or `models` list, zero limit, oversized `initial`, and too-short
or too-long period is reported in one startup failure.

A call costs its input plus output tokens, raw and unweighted. Before a call the gateway reserves an
estimate — the request's JSON bytes divided by four plus 1,000 per image, raised to the session's
last reported input plus the new messages' bytes divided by four and 1,000 per new image, plus an output reserve of the
model's `generation.maxOutputTokens`, else 4,096 for a `reasoning`-class model, else 1,024 — so two
concurrent sessions cannot both pass and both overspend. When the call ends the reservation is
swapped for the provider's reported usage; a provider that reports nothing is charged the input
estimate and a quarter-token per streamed byte. A call that was never sent (an invalid request, an
authentication failure, an attachment failure, an upstream 429) is charged nothing; a cancelled,
timed-out, or failed call is charged its input estimate and the text it already streamed. A call may
settle above its estimate; that is real spend, and it shows as debt that delays the next allow.

Without `metering`, or for an agent with no budget, spend is unbounded and still recorded: every
model call writes one [`meter` record](observability.md#the-meter-charge-record), so a budget added
later restores real history. A refusal mid-turn ends the turn with the sentence; capability calls
that already ran stay run. It is logged as `gateway_session_refused` with category `over-budget`,
never as `gateway_session_failed` or a `gateway.progress kind="failed"` record; its terminal is still
recorded as `kind="terminal_failed"`. The message's outcome is `refused`.

Budgets live in gateway memory. They need neither `telemetry` nor `restore`: without them every boot
starts every window empty. `restore` without `telemetry` is a startup error, because it reads
records this gateway never exports.

### Restoring token windows at boot is best effort

With `restore` and at least one budget, the gateway serves at once and, after `delay`, sends one
search, bounded by `timeout`, for the `meter` records in `[boot − lookback, boot)`, summed per agent, model, and time
bucket. The lookback is the longest history any meter can still see (a fixed window's start, a
rolling period, twice a session length, or a credit bucket's time to fill), capped at `lookbackMax`;
the bucket is the larger of one minute and the lookback divided by 500. Fresh meters are built from
that history, the charges made since boot are replayed on top, and the result replaces the live
meters; calls in flight keep their reservations. The search trusts the same CA as the OTLP exporter
(`OTEL_EXPORTER_OTLP_CERTIFICATE`). For OpenObserve, `authEnv` names the variable holding the whole
`Authorization` header value; Quickwit's searcher takes none.

A restored window can under-count. The OTLP exporter drops records when its queue is full, a crash
loses the batch in flight, and the store may not have ingested the last seconds before boot when
the search runs. OpenObserve rejects records older than its `ZO_INGEST_ALLOWED_UPTO`. A partial or
truncated search result, an error, a timeout, or a clock earlier than this release (an unsynced
clock) applies nothing: the gateway logs one `meter.restore` warning and keeps the live meters, with
no retry. Within a complete result, a rolling or fixed window is restored within one bucket; a
session is exact when the history holds a gap of at least one session length and otherwise may open
early; a credit bucket starts at `initial` at the start of the lookback and over-credits by at most
one capacity. Restore also assumes that two gateways never run at once, which the chart's `Recreate`
deployment strategy guarantees: a rolling update would let the old process spend while the new one
restores. No serving path waits on restore, and nothing reconciles it later.

## Guest model proxy

An optional `proxy` block serves model APIs to Firecracker guests, so `claude`, `pi` or `codex`
inside a VM run with no credentials of their own and spend their agent's
[token budget](#token-budgets). It is its own listener, separate from every transport, and is
reachable only through the jail's egress gateway; it has no IngressRoute or public route.

```yaml
proxy:
  bind: 0.0.0.0:9090
  tls:                                   # required: there is no plaintext mode
    certFile: /etc/dekopon-proxy-tls/tls.crt
    keyFile: /etc/dekopon-proxy-tls/tls.key
    clientCaFile: /etc/dekopon-proxy-tls/ca.crt
  jailIdentity: spiffe://homelab/ns/vm-runner/sa/vm-runner-jail   # the client cert's exact URI SAN
  maxConnections: 16                     # optional; connections served at once
  guests:
    dekopon:gylmar-vm: {agent: gylmar, models: [astra, glm-flash, claude-opus]}
```

| Path | Dialect | Upstream |
|---|---|---|
| `POST /v1/messages`, `POST /v1/messages/count_tokens` | Anthropic Messages | a `kind: anthropic` model |
| `POST /v1/responses` | OpenAI Responses | a `chatgptSubscription` model (Codex) |
| `POST /v1/chat/completions` | OpenAI chat completions | an `openrouter` model |

- **Identity.** The client certificate must chain to `clientCaFile` and carry `jailIdentity` as a
  URI SAN, or the TLS handshake fails. The jail names the VM in `x-dekopon-vm-subject`; an
  unlisted subject gets a 403. See [the security model](security-model.md#the-guest-model-proxy-trusts-the-jail-to-name-its-vm).
- **Models.** A guest names a configured model (`astra`, `claude-opus`), never an upstream id.
  A model outside the guest's list, or one the path's dialect cannot reach, gets a 403. The proxy
  rewrites `model` to the configured upstream id and injects the upstream credential. It drops
  the guest's `authorization` and `x-api-key`, and passes `anthropic-version`, `anthropic-beta`
  and `x-request-id` unchanged. Every other body byte is forwarded as the guest sent it.
- **Codex.** The proxy forces `store: false` and refuses a call without `stream: true` with a 400,
  because the ChatGPT backend requires both. On a 401 it retries once after a forced refresh of the same
  credential file the gateway's own client uses.
- **`kind: anthropic`** (`name`, `model`, `apiKeyEnv`) is proxy-only for now. A route that selects
  one is a startup error.
- **Metering.** Every call except `count_tokens` is admitted against the agent's budget before
  it is sent. The estimate is the body's bytes divided by four, plus 1,000 per image, plus the
  model's output reserve. A refusal is the dialect's own throttling error: a 429 with
  `rate_limit_error` (Anthropic) or `rate_limit_exceeded` (OpenAI), a `retry-after` header, and
  the budget sentence in the third person. A request that can never fit is a plain 400
  `invalid_request_error`, never worded as "prompt is too long", which Claude Code would read as
  a reason to compact and retry. Usage is read from each streamed event as it passes. A client
  that disconnects is charged what had streamed, and the `meter` record says `meter.via = "proxy"`.
- **Accounting.** A proxied call that gets an error status from the upstream is charged zero
  tokens, because nothing ran, so a client retrying an overload (529, 503) spends no budget. A call
  that streams and then fails is charged what was observed. The agent's own model path still
  charges the input estimate when its provider fails; that path has no automatic retry, so the
  difference is deliberate.
- **Streaming.** Responses stream back unbuffered. While the upstream is silent, the proxy
  writes an SSE comment `: ping` every 20 seconds, under the jail's 90-second idle timer; both SDKs
  ignore comment lines. Pings exist only inside an SSE body: a `stream: false` call gets nothing
  until it completes, and the jail's 90-second idle timer cuts one that runs longer, which is then
  charged as cancelled. An upstream silent for 300 seconds ends the stream. The proxy sets no
  total timeout; the jail's `maxConnectionSeconds` (1800 seconds by default) bounds a proxied
  call. A stream cut there is charged as cancelled, and the client's retry is charged again.
- **Bounds.** A request body over 8 MiB gets a 413 in the dialect's error shape. At most
  `maxConnections` connections (default 16) are served at once; 0 is a startup error. A request's
  headers must arrive within 30 seconds; its body is bounded by the jail's `maxConnectionSeconds`. The proxy
  validates a body without parsing it into a tree and records only the top-level keys it reads.
  Each connection briefly holds two copies of its body while reading and rewriting it, so request
  bodies take up to `maxConnections × 2 × 8 MiB` together: 256 MiB at the default. Raise it only
  with the gateway's memory limit.
- **Rotation.** The listener rereads its certificate, key and client CA when their mtime changes,
  checked on each connection, so cert-manager's renewals need no restart.
- **Startup.** No `proxy` block means no listener. Every unknown agent, unknown model, model the
  proxy cannot serve (`openaiCompatible`), and unreadable TLS file is reported in one startup
  failure.

**The model jail.** A VM agent calls only the models its grant names, each on its own path. Every
refusal the proxy sends itself uses the dialect's error shape, starts with `dekopon sandbox:`, names
the rule and says what to do instead, so `claude`, `pi` and `codex` print it as a rule rather than
an outage. It never quotes the request; it names the grant's configured models instead.

| Refusal | Status |
|---|---|
| The VM's subject has no grant | 403 (`permission_error` / `permission_denied`) |
| A model outside the grant, or not served on this path | 403, listing the granted models |
| A body over 8 MiB | 413 |
| A body that is not a JSON object, or names no `model` | 400 |
| A repeated top-level `model`, `stream`, `store`, `models` or `route` | 400 |
| A Codex call without `stream: true` | 400 |
| OpenRouter fallback routing (`models` or `route`) | 400 |
| A budget wait | 429 with `retry-after` |
| A request that can never fit the budget | 400 |

An upstream's own error passes through unchanged, and an unreachable upstream is a 502; neither is a
sandbox rule.

Nothing points a guest's client at the proxy for it: the agent's `vm exec` script passes the
settings in `env`. For Claude Code that is `ANTHROPIC_BASE_URL=https://models.vm.internal`,
`ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_OPUS_MODEL`, `ANTHROPIC_DEFAULT_SONNET_MODEL` and
`ANTHROPIC_DEFAULT_HAIKU_MODEL` set to names in the guest's grant, and
`CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`. An OpenAI client gets `https://models.vm.internal/v1`
as its base URL. The token any client sends is ignored.

**Cost warning.** A Claude Code turn re-sends tens of thousands of cached input tokens on every
request, and the budget counts cached input at the full rate, because a call costs its raw input
plus output. A budget sized for chat is gone in minutes; size a proxied agent's budget for agentic
coding.

## Telemetry

Spans follow [`observability.md`](observability.md):

| Span | Fields |
|---|---|
| `transport.receive` | `transport.kind` (`slack`, `discord`, `telegram`, `whatsapp`, `local`), `message.id`, `drop.reason`; the trace root |
| `gateway.message` | `transport`, `agent`, `outcome` (`answered`, `declined`, `refused`, `unauthorized`, `sealed`, `steered`, `queued`, `busy`, `failed`, `cancelled`, `reply-failed`), `busy.cause` (`same-conversation` or `saturated`, on `busy` only) |
| `gateway.session` | `agent`, `conversation.turns`, `conversation.bytes`; wraps the broker leg and the model session |

A message's trace starts in the transport that received it, not at routing: `transport.receive` is opened before the payload is parsed, so Slack's envelope acknowledgment, WhatsApp's signature check and its 200, Telegram's `offset` advance, Discord's addressing decision, the local transport's line parse, and the routing decision itself are all inside it. `gateway.message` nests under it and closes it. `message.id` is the service's own identifier for the turn — a Slack `ts`, a Discord snowflake, a Telegram `message_id`, a WhatsApp `wamid`, the development transport's boot-scoped counter — and a receipt that routes nothing closes without one, which is the trace that answers why a message went unanswered. The sender and the text stay off it; they ride `gateway.message.received` below. A receipt the transport declines to route records `drop.reason` instead — one word for why, such as `self-authored`, `content-withheld`, or `duplicate` — so a message that produced no reply says so in its own trace.

The prompt loop's own spans (`prompt.session`, `prompt.model_turn`, `prompt.script`, `shell.script`, `shell.command`) nest under `gateway.session`, and the broker's `broker.invocation` joins the same trace through the proposal's `traceParent` field (a W3C `traceparent` value); [`observability.md`](observability.md#gateway-spans) is the authoritative list.

Chat text and canonical subject identifiers reach telemetry as the `gateway.message.received` log event, on every message. The prompt cache key rides its own log event, `gateway.session.cache_key`, so a key and a canonical subject never appear on one line. Every session writes its `agent.improvement.suggested` records beside them. None of this decides model input: the gateway-authored canonical participant label is sent to the selected model on every shared turn regardless.

`gateway.session` carries `conversation.turns` and `conversation.bytes` — how much history this message replayed, as a count and a byte total and never as text; both are zero on a `oneShot` route and on the first message of any conversation. `gateway_conversation_evicted` is in the lifecycle events below with a reason of `idle`, `capacity`, or `grant-changed`. On a seeded session `message.count` counts the replayed window plus this exchange rather than this exchange alone. [`observability.md`](observability.md#what-conversation-history-changes) has the dashboard consequences.

Lifecycle events on stdout as structured JSON (this is the lifecycle subset, not every `gateway_*` record the daemon emits): `gateway_broker_ready`, `gateway_transport_connected`, `gateway_started` (transport and route counts), `gateway_session_rejected`, `gateway_session_failed`, `gateway_session_refused` (a token budget ended the turn), `gateway_proxy_listening` (the guest model proxy's port), `gateway_session_cancelled`, `gateway_session_stop_requested`, `gateway_progress_degraded`, `gateway_conversation_evicted`, `gateway_transport_silent` (transport and phase), `gateway_transport_jitter_unavailable` (an operating system that refused the entropy every reconnect delay is jittered with), `gateway_cache_key_entropy_unavailable`, `gateway_transport_stopped` and `gateway_transport_task_failed` (additional reader failures observed during drain), `gateway_transport_recovering` (configured name, error category, episode failure count and delay in milliseconds), `gateway_stopped` (`shutdown`, `transport-failed` or `transports-lost`). Beyond lifecycle: `gateway_message_ignored` (debug for an unrouted or unaddressed message, and for a WhatsApp group payload, which carries `reason` and its `message.index` inside the delivery) and `gateway_local_request_rejected` (debug); `gateway_reply_failed`, `gateway_memory_record_failed`, `gateway_session_stop_ignored` (debug), `gateway_steer_refused` (`mailbox-full`), `gateway_steer_ack_failed` (debug); `gateway_sessions_abandoned` and `gateway_session_task_failed` (shutdown grace expired, or a session task panicked); `gateway_whatsapp_accept_failed` and `gateway_whatsapp_media_refused`, plus the `gateway_whatsapp_webhook_refused`, `gateway_whatsapp_reply_partial`, and `gateway_whatsapp_listener_stopped` records named in that transport's section; `gateway_wake_fired`, `gateway_wake_orphaned`, `gateway_wake_cancelled`,
`gateway_wake_tick_failed`, `gateway_wake_tick_skipped`, `gateway_wake_probe_unavailable`, and
`gateway_wake_task_failed`, `gateway_wake_store_failed` (a failed write refuses that one change; a
failed write while firing or leasing stops wake firing until restart), `gateway_memory_record_skipped`
(debug: a wake turn is not recorded to provider chat memory);
`gateway_signal_failed`; and at exit `gateway_exit` and `gateway_telemetry_shutdown_failed` — see [`observability.md`](observability.md#daemon-exit-and-shutdown-records). Progress-call outcomes are debug-level `gateway_progress_rendered` records carrying the transport, the primitive, the outcome, and — for a stream render — how many characters were on screen; `gateway_progress_degraded` says which primitive stopped, `gateway_progress_budget_exhausted` that a session spent its edits, and `gateway_progress_dropped` that the policy's queue overflowed. Neither includes a subject, target identifier, status text, raw service response, or credential. Other failure events likewise carry stable categories, and an eviction carries a reason and nothing about the conversation it forgot. An optional no-reply decision closes `gateway.message` with `outcome=declined`; its `agent.reply.declined` record carries only the model-turn number and no text or thread coordinate.

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

`dekopon-gatewayd auth chatgpt {login,status,logout,export}` runs before gateway configuration,
telemetry, transports, or runtime creation. It uses only Dekopon's isolated model credential;
ordinary serving requires `--config PATH`. See [`cli.md`](cli.md) for auth-only flags, output, exit codes and both export guards.
