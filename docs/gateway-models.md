# Gateway model budgets and proxy

Token budgets, the guest model proxy and isolated model authentication for `dekopon-gatewayd`.

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
model call writes one `meter` record. Summary calls are
recorded with `via = "summary"` but never admitted or refused, so they never use room a reply step
needs, and a ledger rebuilt from records skips them. A budget added
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
  The jail also sets `x-dekopon-vm-session`; the proxy records it on the call's
  `model.proxy.call` span and nothing else.
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

## Isolated model authentication

`dekopon-gatewayd auth chatgpt {login,status,logout,export}` runs before gateway configuration,
telemetry, transports, or runtime creation. It uses only Dekopon's isolated model credential;
ordinary serving requires `--config PATH`. See `cli.md` for auth-only flags, output, exit codes and both export guards.
