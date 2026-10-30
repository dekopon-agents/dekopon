# `dekopon-gatewayd` — the chat gateway and agent daemon

`dekopon-gatewayd` is the unprivileged half of the deployment boundary in [`design.md`](design.md). It connects to chat services, waits for a message or wakeup, routes each authenticated message to a named agent from the catalog, runs one bounded model session with the sandboxed shell and on-demand meta tools, and replies.

It holds chat bot and model credentials: what it needs to hear a question and ask a model. It holds no policy, no authorization and no provider credential except the shared DekoponVille Slack bot token named in [the security model](security-model.md#current-gateway-posture). Every provider effect a session drives is an on-behalf-of proposal to `dekopon-brokerd` naming the sender's canonical subject; the broker alone maps that subject to a principal, decides what it may do, resolves credentials and executes.

**Status: Current.** A route is `persistent` unless configured otherwise; durable memory is a separate
broker/agent opt-in and never turns that default into automatic replay. The chart enforces the
[current local process boundary](security-model.md#current-local-process-boundary), and CI keeps
the crates listed in its [dependency gates](development.md#runtime-facts-that-are-easy-to-miss) out
of the gateway's dependency tree.

[`../examples/conditional-write/`](../examples/conditional-write/README.md) is the complete worked
deployment: a Slack DM from an owner-mapped sender, two narrow `http-probe` capabilities, a
broker-injected GitHub token, and an audited PR review comment. Read it beside this reference.

This reference covers running and checking the daemon, the `gatewayd.yaml` keys, agent
self-inspection and telemetry. Each subject has its own reference:

- [transports](gateway-transports.md): Slack, Discord, Telegram, WhatsApp, GitHub and local;
- [assets and media](gateway-assets.md): media inputs, asset handles and chat assets;
- [routing, sessions and conversations](gateway-conversations.md), durable memory and authorization;
- [liveness, progress and stopping a run](gateway-liveness.md);
- [wakes and detached jobs](gateway-wakes.md);
- [token budgets, the guest model proxy and model authentication](gateway-models.md).

## Run

```console
dekopon-gatewayd --config /path/to/gatewayd.yaml
```

`--config` names one YAML file or a directory of fragments. Decoding is strict: an unknown field,
transport kind or route match is a startup failure, not an ignored setting.

- **File:** owned by the daemon's UID, one hard link, not group- or world-writable, at most 1 MiB,
  and not a symlink. Root-owned files on a read-only mount and Kubernetes ConfigMap projections
  are also accepted under [gateway source trust](security-model.md#current-gateway-posture).
- **Directory:** every `*.yaml` directly inside it, in filename order, is a fragment of one
  document under the same file rules. The directory is owned by the daemon's UID and not group- or
  world-writable, unless it is a read-only Kubernetes projection. `transports`, `models` and
  `routes` concatenate; any other key set in two fragments, or fragments that disagree on
  `apiVersion`, refuse startup; a key conflict names the files that set it. A route sits in the fragment that defines its
  transport, so renaming a file cannot reorder routes. A directory with no `*.yaml` is refused.

## Environment file

Both serving daemons accept `--env-file PATH`, a private file of variables
loaded before serving; [`dekopon-brokerd`](../crates/dekopon-brokerd/README.md#environment-file)
reads it the same way.

```console
dekopon-gatewayd --config /path/to/gatewayd.yaml --env-file /path/to/gateway.env
```

- **Precedence:** a command-line option, then an inherited variable (an empty one included),
  then the file, then the default. YAML configuration is unaffected.
- **Grammar:** UTF-8, LF or CRLF. Blank lines and lines whose first non-blank character is `#` are
  ignored. Each other line is `KEY=VALUE` with an optional leading `export `; the key is
  `[A-Za-z_][A-Za-z0-9_]*` and `=` follows it directly. A value is unquoted (no whitespace or
  quote characters) or wrapped in `'…'` or `"…"`, which hold spaces. Values are literal: `$`, `\`
  and `#` are ordinary characters, nothing is expanded, and text after a value is an error, so
  there are no inline comments. `K=`, `K=''` and `K=""` set an empty value.
- **Refused lines:** a duplicate key, a NUL, a byte-order mark, a stray carriage return, an
  unmatched quote, and the keys `DEKOPON_SHELL_JQ_WORKER`, `LD_*`, `DYLD_*`, `HOME`, `TMPDIR`,
  `PATH`, `APPDATA`, `XDG_*`, `DEKOPON_CHATGPT_AUTH_FILE`, `DEKOPON_BROKER_SOCKET` and
  `DEKOPON_CONFIG`. A refusal names the path and line, never a key or value.
- **File policy:** at most 64 KiB, a regular file (not a symlink, FIFO or directory) owned by the
  daemon's effective user, mode `0600` (no group or other bits), one hard link. Its directory and
  every ancestor must be owned by root or that user and not group- or world-writable unless sticky.
  A relative path resolves once against the working directory; launchd and systemd units should
  use absolute paths. On macOS an admin-owned Homebrew prefix such as `/usr/local/etc` or
  `/opt/homebrew` fails the ancestor rule for a separate daemon user; a root-owned chain such as
  `/Library/Application Support/Dekopon/gateway/` passes.
- **Helper modes:** `auth` and `check` (including `--probe`) refuse the option with exit 2 before
  opening anything; `--help` and `--version` open nothing.
- **Startup:** the daemon re-executes itself in place with the merged environment before any
  thread exists, so its PID, supervisor and signal handling are those of a direct start. A
  refused file exits 1 and an invalid value exits 2, before anything is bound. A value error from
  a variable names the option and variable, not the value.

## Runtime threads

Flags size the thread pools before configuration loads; no configuration key controls a pool.
Counts must be positive. Effective worker and blocking counts, whether from flags,
`DEKOPON_WORKER_THREADS`, `DEKOPON_BLOCKING_THREADS`, `TOKIO_WORKER_THREADS` or available CPUs,
must each be at most 4096; a larger count is a command-line validation error (exit 2) before any
pool exists.

| Flag | Environment fallback | Default |
|---|---|---|
| `--worker-threads` | `DEKOPON_WORKER_THREADS` | `TOKIO_WORKER_THREADS`, otherwise available CPUs |
| `--blocking-threads` | `DEKOPON_BLOCKING_THREADS` | 8 |
| `--blocking-keep-alive` | `DEKOPON_BLOCKING_KEEP_ALIVE` | 10s; humantime duration |
| `--shell-threads` | `DEKOPON_SHELL_THREADS` | 32 |

With both OTel exporters enabled and idle blocking threads retired, gatewayd runs `W + 4` threads:
main, `W` workers named `gw-rt-N`, `gw-stdout`, and two SDK-owned export threads. Its ceiling is
`W + 4 + B + S`, where `B` is the blocking cap and `S` the shared `ShellThreads` cap. Each disabled
exporter removes one thread from both counts. The Pi deployment sets `W=2, B=8, S=32`: 6 idle
threads and at most 46. Blocking threads share the `gw-rt-N` names and retire after the
keep-alive. The stdout worker and OTel providers drain before runtime shutdown.

The shell cap is one process budget. Turns (`sh-turn`), jobs (`sh-job`), wake ticks (`sh-tick`),
memory summaries (`sh-memory`), pipeline stages, helpers, calls, feeders, jq pumps and upcalls all
spend it, and admission refuses at once with `LimitExceeded::ShellThreads` when it is exhausted. A
turn holds one permit and each non-final pipeline stage one more; the default retained-value budget
permits at most sixteen producer stacks, seventeen permits with the turn. The blocking pool runs
short I/O and final native joins after thread bodies finish; permits stay held through thread-local
teardown and the join.

## Check a configuration

```console
dekopon-gatewayd check gatewayd.d --catalog agents.d
dekopon-gatewayd check gatewayd.yaml --output json
dekopon-gatewayd check candidate.d --catalog agents.d --probe --against live.d
```

`check` runs the validation startup runs, through the same function, and stops before the broker
is contacted. Every stage whose inputs loaded runs, so it reports every configuration, catalog and
route problem at once; a stage that could not run (routes when the catalog failed, say) is a
warning naming it.

- **Output:** one `problem` or `warning` line each (`ok` when there is no problem), or with
  `--output json` `{ "ok", "problems", "warnings" }`. Exit 0 with no problems, 1 with problems,
  2 on a usage error.
- **Inputs:** `--catalog` replaces `catalogPath`, which usually names a deployment path. Files
  follow the [Run](#run) rules with the invoking user as owner, so the user's own `0644` files pass.
- **Side effects:** plain `check` never contacts the broker socket, reads no transport or model
  credential variable (each one boot will read is listed as a warning), opens no ChatGPT login and
  creates no journal directory. It builds no transport client, so checks that live in client
  constructors run only at boot.

**`--probe`** adds the one online stage and its one credential read. For each changed `openrouter`
model it reads the model's `apiKeyEnv` and sends one canary request through the gateway's own
client and routing settings, capped at 16 output tokens and bounded by 30 seconds or the model's
`timeoutMs`, whichever is shorter.

- A model declaring `modalities: [image]` receives a built-in one-pixel PNG; a text-only model
  receives a text turn. Both requests declare one fixed `canary` tool with empty-object arguments
  and must return exactly one well-formed call to it. No tool result or second request is sent.
- A refusal, malformed call or deadline is a problem naming the model, its vendor model ID, the
  file that defines it and the routes it serves. A vendor failure keeps its status and a redacted
  diagnostic when available; a deadline reports that the turn expired. The key never appears,
  because the client redacts it from the body.
- A model is changed when its name is absent from the `--against` configuration, its parsed
  settings differ there (defaults filled in, so formatting alone is no change), or a route newly
  binds it. A binding compares transport, conversation selector, subjects, agent and selected
  model. `--against` requires `--probe`. The baseline binds against its own `catalogPath`;
  `--catalog` applies only to the candidate.
- Without `--against`, every model is probed. A baseline configuration or catalog that does not
  load or bind is a warning, and every model is then probed.
- `chatgptSubscription` models are never probed, because reading the login can refresh and rotate
  it under the running gateway. Each changed one warns `not probed: subscription auth`; a changed
  `anthropic` model warns `not probed: proxy-only model` and a changed `openaiCompatible` model
  `not probed: openaiCompatible endpoint`.

Reload runs the same canary against the active configuration and refuses a failed candidate while
the running configuration keeps serving. Boot does not run it, so run
`check --probe --against <live config>` before deploying a candidate that restarts the gateway.

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
    indicators:                               # optional; absent mode is off
      mode: native                            # off (default) | native
      status: auto                            # auto (default) | off
      typing: auto                            # auto (default) | off
      reaction: { working: auto, seen: off }  # working off (default) | on | auto; seen off (default) | on
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
        stopped: "Stopped."                    # person and operator stops
        failed: "The agent could not complete this request." # failures without a fixed cause line
  - name: community-discord
    kind: discordGateway
    botTokenEnv: DEKOPON_GATEWAYD_DISCORD_BOT_TOKEN
    messageContent: false                     # optional, default false; true requests the privileged Message Content
                                              # intent, which the Developer Portal must allow (see Discord Gateway)
    indicators: { mode: native, progress: message, cancelButton: true }
  - name: tg
    kind: telegramLongPoll
    botTokenEnv: DEKOPON_GATEWAYD_TELEGRAM_TOKEN
    indicators: { mode: native, progress: message, cancelButton: true }
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
    indicators: { mode: native }           # typing only: WhatsApp cannot clear a working reaction
  - name: dev
    kind: local
    socketPath: /path/to/gatewayd-dev.sock
    indicators: { mode: native, progress: message, stream: true, cancelButton: true }

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
    indicators: { reaction: { seen: on } }    # optional route override; opt in to steer acknowledgements
  - transport: scientist-slack
    conversation: { kind: [channel, thread] } # any other channel the bot is invited to
    agent: xaviers-conditional-writer
  - transport: community-discord
    conversation: { kind: any }               # every kind, including group DMs and forum posts
    agent: xaviers-conditional-writer
    follow: false                             # optional; true requires a channel/thread-only selector, messageContent: true and sessions.journal
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
  indicators: { reaction: { seen: off } }    # optional defaults; absent/null leaves inherit
  maxConcurrent: 4                            # optional, default 4
  maxJobs: 2                                  # optional, default 2; 0 refused
  replyOnBusy: true                           # default true; saturation or a full mailbox only
  maxConversations: 1024                      # optional, default 1024 tracked
  assetRetentionBytes: 1073741824             # optional, shared disk budget; 0 disables assets
  journal:                                    # optional; absent, no conversation text is written to disk
    path: /var/lib/dekopon-gatewayd/journal           # relative paths resolve against this file; file sweep uses the longer of memory retention and journal-route forgetAfterMs
  assets:                                     # required with journal; refused without it
    path: /var/lib/dekopon-gatewayd/assets      # separate directory; relative paths resolve against this file
  wakes:                                      # optional; absent, no route may schedule a wake
    path: /var/lib/dekopon-gatewayd/wakes.jsonl       # relative paths resolve against this file
    maxPerSubject: 20                         # optional, default 20 pending wakes per person
    maxSubscriptionsPerAgent: 64              # optional, default 64 pending PR subscriptions per agent
    subscriptionExpiryMs: 1209600000          # optional, default 14 days from subscription creation
    minIntervalMs: 300000                     # optional, default 5 minutes between watch checks; must exceed scriptTimeoutMs
    maxHorizonMs: 2592000000                  # optional, default 30 days

shutdownGraceMs: 120000                       # optional, default 120000

telemetry:                                    # optional, identical in shape to broker.yaml's
  endpoint: http://127.0.0.1:5080/api/default
  transport: http
  serviceName: dekopon-gatewayd
  exportTimeoutMs: 10000
```

**Route match.** The route's `conversation:` block is the match: `kind` is the word `any` or a list
of `directMessage`, `groupDirectMessage`, `channel` and `thread`, with an optional `container`
(Slack team, Discord guild, WhatsApp `waba:phoneNumberId`, or a test-only fauxlack server) and an
optional `ids` list. A bare `kind: channel` is a decode failure naming the list form, because it
reads as though it claimed the threads under the channel too. `subjects:` restricts a route to
named canonical subjects and is accepted only beside `kind: [directMessage]`; it is routing, never
authority, and a per-person channel route would read as an access-control list and be trusted as
one.

**Script deadline.** `limits.scriptTimeoutMs` (default 30000) bounds one script, not the session.
At the deadline the shell ends the run with `dekopon-shell: script exceeded its <N>ms deadline` and
abandons any provider call still in flight; the broker drops that call with the script, so an
effect that had already reached the provider leaves a decision record and no execution record. A
slow capability such as `gpt-image.edit` can need more than 30 seconds. `maxDurationMs` bounds the
session; a `scriptTimeoutMs` above it is refused, naming both numbers, because the session would be
cancelled first.

**Memory mode.** The `memory:` block is tagged on `mode`, and both modes are strict: an unknown
mode, an unknown, null or wrong-case `scope`, and any persistent-only field beside `mode: oneShot`
are decode failures. `scope` accepts only `privateConversation` (the default) and
`sharedConversation`, and only beside `mode: persistent`. A setting that can never take effect is
far more likely a typo than an intention, and accepting it would leave a file claiming a memory or
audience the daemon does not have.

**Progress notes.** `progressNotes: true` offers the shell's `progress` builtin in the bash tool
description and admits its bounded, model-authored notes to `gateway.progress` telemetry. It
defaults to false: turning it on is consent for this text to reach telemetry and chat under the
[security model's liveness rule](security-model.md#current-gateway-posture). A live note owns an
active progress message until another note replaces it, the next model turn or steering clears it,
or the session ends; individual tool completions do not clear it. At a keep-alive tick, a note older
than twice its ETA (or 120 seconds without an ETA) is cleared before rendering; the ETA never counts
down. A transport with liveness off or no active progress message still records an opted-in note.

### Memory settings

`sessions.memory` holds journal retention, the one gateway summary engine, and per-agent
`agents.<agentId>` entries. An agent ID must name a routed agent, and its `retention` may only
narrow the global limits. A persistent route may set `hotTurns`, `summaries`, `capture` and `selection` under
`routes[].memory`; a
route's value replaces its agent's. Unknown keys refuse at load.

| Key | Default | Rule |
|---|---|---|
| `retention.maxAgeMs` | 604800000 (7 days) | global ceiling; agent override `retention.maxAgeMs` |
| `retention.maxBytesPerAgent` | 67108864 (64 MiB) | global ceiling; agent override `retention.maxBytes` |
| `summaries.concurrency` | 1 | summary engine workers |
| `summaries.queue` | 64 | summary engine queue |
| `hotTurns` (agent or route) | `min(10, maxTurns)` | newest records replayed verbatim; above `maxTurns` refuses |
| `summaries` (agent or route) | on: the route's model, `parts: [200, 1024, 4096]` bytes, `maxInputBytes: 131072` | `off` or `{model?, parts?, maxInputBytes?}`; `parts` holds 1 to 255 entries; an unknown or proxy-only `model` refuses |
| `capture` (agent or route) | `{maxItemBytes: 262144}` | caps each stored text item; opaque reasoning has a fixed 64 KiB cap, and tool arguments are stored as sent |
| `selection` (agent or route) | `ladder` | or `{decider: {capability, backend, model, requireProbabilities}}`, all four required and nonempty |

- Summaries resolve to `off` on a route whose `recall` is not `journal`, because those records
  never replay.
- `maxBytes` defaults to 524288 on journal-recall routes and 65536 elsewhere.
- Without a journal, persistent routes are window-only and `check` prints one warning; an explicit
  `recall: journal` still refuses without storage.
- On journal-backed persistent routes, `read_memory_record`, `set_memory_pin` and
  `clear_memory_pin` use the full scoped conversation key and current grant; one-shot and
  journal-less routes offer none of them.
- Representation IDs are `native` (the original turn), `summary-<n>` (generated summary part `n`)
  and `metadata` (the record's envelope). An ID that does not parse, or names a part the plan does
  not have, is refused with the record's available IDs; a part whose summary is queued, running or
  failed is refused the same way, without its error or any original text.
- A read returns at most 16 KiB of UTF-8, or a typed refusal naming why, and persists nothing.
- A pin is queued during the turn and written after the reply, only if the turn's own record is
  written. It chooses the rung a record replays at once it is older than the hot window, not
  whether it replays.

### GitHub webhook transport

A transport of `kind: githubWebhook` requires `name`, `bind` (a nonzero port) and `secretEnv` (an
environment variable name, never the secret). Its path is fixed at `/webhooks/github`;
`callbackPath` is not accepted. When a `whatsappCloudApi` transport exists, the GitHub `bind` must
equal a WhatsApp `bind`, sharing its listener, and callback paths must not collide. `check`
validates structure and warns about the credential without reading it; boot requires a present,
nonblank credential.

- **Requests:** a POST has bounded headers and a 256 KiB body, and its exact raw-body HMAC-SHA256
  is verified before parsing. Unknown event headers and repository-less events, including `ping`,
  are acknowledged and ignored, as is a repository name outside the subject grammar, including a
  name with a dot.
- **Routes:** a GitHub route requires a nonempty, unique `githubEvents` list of typed family names
  (for example `[issues, issue_comment, push]`), `memory: { mode: oneShot }`, and a canonical
  selector such as `conversation: { kind: [thread], container: owner, ids: [owner.repo] }`.
  `pull_request` wakes only on opened, reopened and synchronize (closed only removes
  [PR subscriptions](gateway-wakes.md#pr-subscriptions)); `workflow_run` only on completed runs that succeeded or
  failed; other listed families wake on every action.
  `githubEvents` is refused on other transports, and `null`, unknown family names and dotted repo
  IDs fail. Declaration-order selection happens before the selected route's event filter, so a
  later overlapping route never receives an event the first route omits.
- **Sessions:** a delivery enters the session admission gate as an addressed `message` with
  `github.<owner>.<repo>` as its attested subject. Conversations key by `owner.repo:<n>` for a
  local issue, PR or discussion number, or `owner.repo:repository` for repository-level events. A
  run or check keys by its one local associated PR, otherwise by the repository. A dependency keys
  by its local blocked issue; a foreign blocked issue uses the repository bucket, and a foreign
  blocking issue supplies no number or fact.
- **Batches:** deliveries never steer or interrupt. Consecutive queued deliveries from one actor
  coalesce into batches of at most 16; another actor starts a new batch. The mailbox stays 100
  slots and the session permit stays the global concurrency bound. Notes render from the batch at
  turn start, contain no titles, bodies or other prose from the payload, and direct the agent to
  read current state through a broker-granted capability.
- **Acknowledgement:** a 200 acknowledges session admission, queueing, an ignored delivery or a
  committed recent duplicate, not completion. Busy admission, a full transport queue or mailbox,
  an unanswered reserved duplicate, a closed channel and the webhook deadline return 503. A
  timed-out request may still be admitted later, and a late admission commits its GUID. Dedupe
  keeps 1,024 committed GUIDs in memory; reservations stay pending until answered or dropped, and
  busy or dropped tickets release their GUID for redelivery. GitHub does not redeliver failed
  deliveries automatically; the 503 shows in its delivery log for manual redelivery. Restart,
  eviction or a replayed signed body may open another freshly authorized turn: the delivery GUID
  is unsigned and GitHub signs no timestamp.
- **Authority:** the gateway has no GitHub API client, outbound reply, repository registration
  list or automatic post target. Each turn opens a fresh broker leg and receives fresh
  capabilities under `agent.prompt` before calling the model. Unmapped subjects, denied grants,
  busy sessions and model failures post nothing to GitHub; final answers stay in the session
  trace. A signed run claim, even one with no PR association, grants no fix authority: an agent
  needs a broker-granted current-check read and a separate narrow write grant. A route may set
  `wakes: true` for silent wakes. A delivery resumes no chat watch or earlier repository session;
  only a [PR subscription](gateway-wakes.md#pr-subscriptions) resumes its anchored Slack thread.

### Extra model CA bundles

The optional top-level `extraCABundles` list applies to the OpenRouter, OpenAI-compatible and
ChatGPT subscription inference clients together:

```yaml
extraCABundles:
  - /var/run/dekopon-trust/model-roots.pem
```

- Omitting it or setting `[]` keeps public WebPKI trust alone.
- Each entry is an absolute path to a regular file of 1–65,536 bytes holding a valid, nonempty PEM
  certificate bundle; at most eight entries are accepted.
- Boot and reload read the files. Plain `check` checks only path shape and count; `check --probe`
  also reads them for its model request. Replacing a bundle at the same path activates a new
  serving generation; sessions already running keep their original client trust.
- The roots supplement public roots and grant no network access. A listed CA lets whoever holds its
  private key impersonate model endpoints to this gateway.
- No ambient `SSL_CERT_FILE`, `SSL_CERT_DIR`, system trust store or proxy is consulted; redirects
  and timeouts are unchanged.
- The list covers inference and gateway subscription OAuth refresh. Each generation keeps its own
  HTTP client trust, including refreshes already in flight. OTLP export, the OCI provider manager
  and the broker have separate CA configuration.
- Mount the public PEM file read-only at the configured absolute path with an operator-supplied
  volume mount in the rendered Deployment; the chart has no generic gateway extra-mount value.

### OpenRouter model settings

The tag is exactly `kind: openrouter`, not `openRouter`. `name`, nonblank `model`, `apiKeyEnv`
and positive `timeoutMs` are required. Any nonblank model ID is accepted locally; there is no
catalog. `classes` and `modalities` default to `[]`; `modalities: [image]` means image input only.
`endpoint`, `stream` and `authFile` are not accepted: the URL is fixed and streaming is always on
([wire behavior](inference.md#streaming-and-interruption)).

Each optional block is strict, as is the model entry:

| Block | Members and bounds | Wire mapping |
|---|---|---|
| `generation` | optional `maxOutputTokens` positive integer, finite `temperature` 0–2 inclusive, finite `topP` >0 and ≤1 | `max_tokens`, `temperature`, `top_p` |
| `reasoning` | required `effort`: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` | `reasoning: {effort}` |
| `routing` | optional `allowFallbacks`, `requireParameters` booleans; optional nonempty `only` list of nonblank strings | `provider: {allow_fallbacks, require_parameters, only}` |
| `cache` | required `style`: `automatic` or `explicitPrefix`; optional `ttl`: `5m` or `1h`, only with `explicitPrefix` | `cache_control` markers, placed as in [cache anchors](inference.md#openrouter-controls-and-cache-anchors) |

- Omitted members are absent on the wire, not sent as default values. No `cache` block means
  automatic mode (no marker); an omitted TTL is not invented.
- Explicit caching with no leading system message is an `InvalidRequest` before sending.
- `allowFallbacks: false` disables failover but does not pin routing; pin `routing.only` when cost
  or cache locality matters.
- Unknown fields and bad enum spellings are decode refusals. Out-of-range or non-finite numbers,
  an empty `routing.only` list or blank entry, and a TTL in automatic mode are reported with every
  other configuration problem, never clamped. A missing or blank credential variable is a separate
  startup problem. These blocks are refused on `chatgptSubscription` and `openaiCompatible`.
- Each call has one total `timeoutMs` deadline; see [`inference.md`](inference.md).

### No secrets in this file

Transports and models name **environment variables**, never values. A variable name must match
`[A-Za-z_][A-Za-z0-9_]*`, so a token pasted where a name belongs is a startup failure rather than
plaintext in the file. Transports and models read variables through one definition: a missing
required variable is a startup failure reported **by variable name, never by value**. A blank
value counts as missing, because an empty app secret is an HMAC key anyone can compute and an empty
bearer token is still sent as a header.

### Startup fails closed

Startup refuses each of the following, as well as strict decoding failures ([Run](#run)) and the
refusals stated beside each key above:

- a route naming an agent the catalog does not contain, or one the catalog disables;
- an agent with no resolvable model — no `model` override and no configured model offering its
  `modelClass`, or no `modelClass` at all;
- duplicate transport or model names; a route naming an unknown transport, an unknown model, or an
  `anthropic` model, which only the [guest model proxy](gateway-models.md#guest-model-proxy) serves;
- an empty `transports:`, `models:` or `routes:` list;
- a zero step budget, capability budget or `sessions.maxConcurrent`; a model with `timeoutMs: 0`;
  `shutdownGraceMs: 0`;
- a persistent route with a zero idle timeout, turn window or byte window, or a zero
  `sessions.maxConversations`, which would make every history immediately evictable;
- a transport `endpoint` override (`graphEndpoint` on `whatsappCloudApi`) that is neither its pinned
  production origin (Slack, Discord, Telegram, or the Meta Graph API) nor a literal loopback
  `http://` URL. Literal means `127.0.0.1` or `::1`: the name `localhost` resolves to whatever the
  host's resolver says today;
- a selector that can never name a conversation: an empty or duplicated `kind` list, an empty `ids`
  list, an id or `container` the transport would never mint, a `container` on Telegram (which has
  nothing above a chat), a kind the transport never produces (`groupDirectMessage` on Discord,
  `channel` on WhatsApp), or an `ids` entry in `id:thread` form — a selector names the parent, and
  the kind list decides whether its threads come with it;
- `subjects:` beside anything but `kind: [directMessage]`, an empty `subjects:` list, or
  `memory.scope: sharedConversation` on a `[directMessage]`-only route, where the direct message
  already is the subject;
- an `indicators.conversations.<kind>` key for a kind the transport never produces;
- a missing or blank chat or bound-route model credential variable. An `openaiCompatible` model's
  `apiKeyEnv` is optional (OpenRouter requires it): absent means the endpoint needs no key, as a
  loopback llama.cpp does not, while naming a variable that is unset or blank is refused, and this
  process cannot see one exported after it started;
- an unknown Slack experience, indicator key or enum value;
- with resolved `mode: native`, `indicators.stream` or `indicators.cancelButton` on
  `whatsappCloudApi`, which can neither edit a message nor carry an interactive component;
  `indicators.stream` on Slack; `indicators.cancelButton` on Slack Agent; `cancelButton` with
  neither streaming nor a progress line; or `indicators.statusText: true` on anything except Slack
  Agent, or alongside `progress: message`, including conversation and route overrides;
- `reaction.working: on|auto` on WhatsApp, which cannot clear a working reaction;
- an effective `indicators.keepAlive` with `everySeconds: 0` or an offset of `0`, which is a render
  loop rather than a keep-alive;
- an `indicators.templates` line using a placeholder its field cannot render. `working` and
  `keepAlive` render `{turn}`, `{of}`, `{calls}`, `{calls_max}` and `{elapsed_s}`; `tool` adds
  `{word}`; `note` adds `{note}`, and `noteEta` adds `{note}` and `{eta_s}`; `stopped` and `failed`
  render none. Every offending placeholder in the block is named, not only the first;
- an empty `stopWords:` list, or one with a blank word. Omit the key to keep `[stop, cancel]`;
- a route with `progressNotes: true` and `progressDetail: off`: notes need a progress line;
- a `whatsappCloudApi` transport whose `bind` port is 0, whose `wabaId` or `phoneNumberId` is not a
  canonical positive decimal, whose `callbackPath` is not lowercase literal segments, or whose
  `graphApiVersion` is not `v<major>.0`; two `whatsappCloudApi` transports on one `bind`, even with
  distinct callback paths, because each owns its own listener;
- broker frame bounds the protocol rejects (`maxFrameBytes` zero or above its hard ceiling,
  `ioTimeoutMs` zero), a broker socket that neither `broker.socketPath` nor the discovery order
  starting at `DEKOPON_BROKER_SOCKET` resolves, or a `telemetry:` block `dekopon-telemetry`
  refuses;
- an unreachable broker, or one whose generation is not active. Before connecting any transport,
  `dekopon-gatewayd` makes one `capabilities()` call on the configured socket and logs the
  capability count as `gateway_broker_ready`.

**Every problem at once.** A file that decoded is scanned to the end before it is refused, and the
refusal lists everything wrong with it: `3 validation problems found:`, then one line per problem,
the shape `dekopon-config` uses for a catalog. Only a file that cannot be understood at all — wrong
ownership or permissions, oversize, or invalid YAML — stops at the first error. Route binding scans
the whole table the same way, so a catalog that disabled two routed agents is one refusal naming
both. A list that failed itself is not blamed on the routes that name it: no transports at all, or
a transport with no name, is reported once rather than again for every route pointing at it.

**Every credential before any connection.** Chat credentials and bound-route model credentials all
resolve, and every transport client is built, before the first transport authenticates. Preparation
reports every missing credential together, by variable name. After preparation and the broker probe
succeed, all transports connect concurrently. A healthy transport serves at once while another
recovers; its authenticated bot identity is installed before its first message can route.
`gateway_started` means supervision is running, not that every transport is connected.

### Connection recovery

Every adapter — Slack, Discord, Telegram, WhatsApp and local — shares one recovery policy. It wraps
only connection establishment and receiving, never reply drivers, provider invocations or outbound
message effects. Clients, Slack's dedup ring, Telegram offsets, Slack thread ownership and Discord
resume/identify state survive recovery. WhatsApp keeps no dedup ring, so a Meta webhook retry —
which happens only when its HTTP 200 is lost in the network — either joins an in-flight turn as a
steer or starts a fresh one and gets a second reply.

The fixed policy needs no configuration:

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
  then exits nonzero through `gateway_exit`, preserving the failure cause. The executable bounds
  teardown of abandoned blocking work to five seconds. A supervisor such as Kubernetes can then
  restart the **gateway container**, not necessarily the pod.

The adapter owns reconnect protocol details; retry loops never nest. A changed bot identity on
recovery is terminal rather than routing under stale identity metadata.

## Agent configuration self-inspection

A route with `inspectAgentConfig: true` offers every authorized session the `inspect_agent_config`
tool. When someone asks "what is this agent's configuration?", the model can call it and receive
one bounded JSON snapshot designed to render as concise Markdown tables:

- agent identifier, description, and catalog `modelClass`;
- the exact catalog `instructions` supplied as this session's system prompt;
- the skills mounted for the agent, each as its name, description, and resource file paths —
  never the skill text, which `read_skill` discloses on demand — and absent when nothing is
  mounted;
- route step/capability limits and the one-shot or persistent `memory:` window, including the
  effective persistent scope; and
- the capability metadata in this sender's fresh `capabilities(subject, agent, scope)` result:
  identifier, selected provider, description, effect, and risk, as the provider manifest and the
  broker's `capabilities` define them.

That last section is an **effective Cedar view**, not Cedar source. Absent: raw policy, policy IDs
and digests, denied or merely declared capabilities, execution constraints, credential bindings,
private secret-map source/selector/use inventory, principal/subject/channel/transport identifiers,
model endpoints and auth paths, broker paths, and all credential values. Exact standing
instructions stay visible and may intentionally contain a public inert DRN. The gateway never
receives provider credentials or raw policy, and the typed view has no field for the chat and model
credentials it does hold.

- **Bounds:** each serialized result has a 128 KiB hard ceiling; an oversized view produces one
  fixed content-free diagnostic, never a partial configuration. Calls are repeatable under the
  prompt loop's shared per-turn tool-call and model-step bounds, with no inspection-specific limit.
- **Repeats:** the view is built once per session from one fresh broker answer and cannot change
  while the session runs. A tool result stays in the message vector and is re-sent on every later
  turn, so only the first call carries the configuration; every later call gets a short pointer to
  it.
- **Without the flag:** the tool is not offered; the model's tool list lacks it and a scripted call
  is an unknown tool. That removes the structured dump and nothing else: the `instructions` are
  still the system prompt, so secrecy from a determined user is the model's obedience, not a gate.
- **Authority:** inspection happens only after the ordinary authorization gate, makes no broker
  invocation, spends no capability-call budget, grants nothing, and creates no durable broker audit
  record. Standing instructions are visible to every sender authorized to use the agent. They were
  already model input, must never contain credentials, and are not a secret from the agent's users.

## Telemetry

Spans follow [`observability.md`](observability.md):

| Span | Fields |
|---|---|
| `transport.receive` | `transport.kind` (`fauxlack` in feature tests, `slack`, `discord`, `telegram`, `whatsapp`, `local`), `message.id`, `drop.reason`; the trace root |
| `gateway.message` | `transport`, `agent`, `outcome` (`answered`, `declined`, `refused`, `unauthorized`, `steered`, `queued`, `busy`, `failed`, `cancelled`, `sealed`, `reply-failed`), `busy.cause` (`same-conversation` or `saturated`, on `busy` only) |
| `gateway.session` | `agent`, `conversation.turns`, `conversation.bytes`; wraps the broker leg and the model session |

A message's trace starts in the transport that received it, not at routing: `transport.receive` is opened before the payload is parsed, so Slack's envelope acknowledgment, WhatsApp's signature check and its 200, Telegram's `offset` advance, Discord's addressing decision, the local transport's line parse, and the routing decision itself are all inside it. `gateway.message` nests under it and closes it. `message.id` is the service's own identifier for the turn — a Slack `ts`, a Discord snowflake, a Telegram `message_id`, a WhatsApp `wamid`, the development transport's boot-scoped counter — and a receipt that routes nothing closes without one, which is the trace that answers why a message went unanswered. The sender and the text stay off it; they ride `gateway.message.received` below. A receipt the transport declines to route records `drop.reason` instead — one word for why, such as `self-authored`, `content-withheld`, or `duplicate` — so a message that produced no reply says so in its own trace.

The prompt loop's own spans (`prompt.session`, `prompt.model_turn`, `prompt.script`, `shell.script`, `shell.command`) nest under `gateway.session`, and the broker's `broker.invocation` joins the same trace through the proposal's `traceParent` field (a W3C `traceparent` value); [`observability.md`](observability.md#gateway-spans) is the authoritative list.

Chat text and canonical subject identifiers reach telemetry as the `gateway.message.received` log event, on every message. The prompt cache key rides its own log event, `gateway.session.cache_key`, so a key and a canonical subject never appear on one line. Every session writes its `agent.improvement.suggested` records beside them. None of this decides model input: the gateway-authored canonical participant label is sent to the selected model on every person-originated turn in every scope regardless.

`gateway.session` carries `conversation.turns` and `conversation.bytes` — how much history this message replayed, as a count and a byte total and never as text; both are zero on a `oneShot` route and on the first message of any conversation. `gateway_conversation_evicted` is in the lifecycle events below with a reason of `idle`, `capacity`, `grant-changed`, or `sealed`. On a seeded session `message.count` counts the replayed window plus this exchange rather than this exchange alone. [`observability.md`](observability.md#what-conversation-history-changes) has the dashboard consequences.

Lifecycle events on stdout as structured JSON (this is the lifecycle subset, not every `gateway_*` record the daemon emits): `gateway_broker_ready`, `gateway_transport_connected`, `gateway_started` (transport and route counts), `gateway_session_rejected`, `gateway_session_failed`, `gateway_session_refused` (a token budget ended the turn), `gateway_proxy_listening` (the guest model proxy's port), `gateway_session_cancelled`, `gateway_session_stop_requested`, `gateway_progress_degraded`, `gateway_conversation_evicted`, `gateway_transport_silent` (transport and phase), `gateway_transport_jitter_unavailable` (an operating system that refused the entropy every reconnect delay is jittered with), `gateway_transport_stopped` and `gateway_transport_task_failed` (additional reader failures observed during drain), `gateway_transport_recovering` (configured name, error category, episode failure count and delay in milliseconds), `gateway_stopped` (`shutdown`, `transport-failed` or `transports-lost`). Beyond lifecycle: `gateway_message_ignored` (debug for an unrouted or unaddressed message, and for a WhatsApp group payload, which carries `reason` and its `message.index` inside the delivery) and `gateway_local_request_rejected` (debug); `gateway_reply_failed`, `gateway_memory_record_failed`, `gateway_session_stop_ignored` (debug), `gateway_steer_refused` (`mailbox-full`), `gateway_steer_ack_failed` (debug); `gateway_sessions_abandoned` and `gateway_session_task_failed` (shutdown grace expired, or a session task panicked); `gateway_whatsapp_accept_failed` and `gateway_whatsapp_media_refused` (the accept/listener names also describe the shared socket), plus reason-only `gateway_github_webhook_refused` and the `gateway_whatsapp_webhook_refused`, `gateway_whatsapp_reply_partial`, and `gateway_whatsapp_listener_stopped` records named in that transport's section; `gateway_wake_fired`, `gateway_wake_orphaned`, `gateway_wake_cancelled`,
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
