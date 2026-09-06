# dekopond

**Status: current.** Chat-transport wakeups, attested routing, opt-in native in-flight activity,
cooperatively cancellable bounded sessions, persistent conversations, catalog-mounted skills, and
opt-in improvement suggestions are implemented and tested.

The unprivileged Dekopon chat gateway and agent daemon. It connects to chat services,
waits for a wakeup, routes each authenticated message to a named agent from the catalog,
runs one bounded model session with the sandboxed shell plus safe on-demand meta tools,
and replies with the answer.

- **Transports** — Slack Socket Mode and Discord Gateway over outbound WebSockets, Telegram long
  polling, a raw-body-HMAC-authenticated text-only WhatsApp Cloud API webhook with pinned Graph
  replies, and an owner-only Unix development socket. WhatsApp is the only public wakeup surface;
  it expects operator-owned TLS termination and exact-path routing.
- **Routing** — first match wins on (transport, direct message or channel), and a channel
  route names one channel or, with the name left out, any channel the bot is invited to.
  Declaration order is the precedence rule: a named channel written above a catch-all keeps
  its own traffic. Unmatched traffic is ignored, and a channel initially requires the bot to be
  @-mentioned. In Slack Agent mode, fresh authorization claims that exact sender/thread so later
  unmentioned follow-ups can continue; every other ambient channel message remains ignored.
- **Chat assets** — Slack, Discord, and Telegram photos/files become numbered, bounded references
  that a model opens on demand. Discord signed CDN URLs are host-checked, streamed under the same
  8 MiB ceiling, and refreshed from the exact source message after expiry.
- **Generated images** — a route may explicitly name a fixed-endpoint OpenAI Images backend. Its
  model gets one `generate_image` attempt yielding one validated PNG up to 8 MiB, delivered through
  native Slack/Discord/Telegram uploads or the local protocol without entering prompts or memory.
- **Activity** — after fresh authorization, Discord typing and Telegram chat actions renew under
  their native leases; Slack Agent sessions use `processing`/`active` and an authenticated Stop
  event, with an opt-in fixed `:tangerine:` reaction fallback for classic/free workspaces. Cosmetic
  failures never alter the terminal reply.
- **Sessions** — a process-wide concurrency ceiling plus per-conversation serialization,
  bounded model turns, bounded capability calls, cooperative Stop checks, and one fixed line on
  failure. An unaddressed owned-thread follow-up also offers `decline_chat_reply`, which ends a
  no-work session without sending anything to chat instead of making the agent take the last word.
- **Authorization** — every session opens an *attested* broker leg naming the sender's
  canonical subject. An empty capability set ends the session before any model call.
- **Conversations** — one independent session per message unless a route sets
  `mode: persistent`, whose `privateConversation` default keeps per-subject history and whose
  explicit `sharedConversation` scope shares one exact agent/transport/conversation window.
  Shared turns carry gateway-authored canonical participant labels; those identifiers reach the
  model provider even when telemetry payloads are disabled. History is compacted and bounded;
  transcript commits, attachment inventory/publication/fetch, and opaque cache-lane lifetime share
  one generation that is retired on idle/LRU/grant change or empty-grant removal. It caches no
  authorization.
- **Skills** — the agent's catalog skills ride its bound route, read whole into memory when the
  catalog loads and shared by every session on that route, so a session never touches the
  filesystem. When any are mounted, a second system message after the instructions lists each by
  name and description only, and the `read_skill` tool returns one skill's instructions, or one
  of its resource files, on demand; a repeat read is answered with a one-line pointer to the
  earlier result. An unknown name or resource path is a refusal the model reads, naming the
  mounted skills or the skill's resource paths, and the session continues. Skill text is
  untrusted model text exactly like `instructions`: it shapes answers and grants nothing.
- **Improvement suggestions** — a route with `improvementSuggestions: true` (default `false`)
  also offers `suggest_improvement`, a bounded channel for the model to tell the operator what
  to fix, at most three notes per session. Each note is written to telemetry as
  `agent.improvement.suggested` whether or not `telemetryPayloads` is on, which is why the flag
  is off by default: the record carries model-authored text, and setting the flag is that
  consent. A suggestion is advisory by construction — no instruction, skill, limit, or grant
  moves because a model asked — and the gateway never relays it to chat.
- **Self-inspection** — every authorized session offers `inspect_agent_config`, returning its
  standing prompt, mounted skills by name, description, and resource file paths (never their
  text; `skills` is absent when nothing is mounted), route limits, and fresh subject-specific
  effective Cedar grants. The fixed shape omits raw policy, identity, endpoints, broker paths,
  skill directories, and all credential names and values. Calls are repeatable under the prompt
  loop's shared bounds, with no inspection-specific counter; a repeat points at the copy already
  in the conversation instead of appending a second one.
- **Informational status** — after the broker probe, the gateway best-effort reports a bounded
  content-free catalog inventory; each session separately coalesces provider-reported model usage.
  These feed only the broker-hosted web UI, reset with the broker process, and never affect a
  session, policy, credentials, execution, evidence, or durable audit.

## Authority

`dekopond` has none. It holds chat bot credentials and model credentials — the things it
needs to hear a question and to ask a model — and it never holds a provider credential, a
policy, or an authorization. Every effect a session drives is submitted to
`dekopon-brokerd` as an on-behalf-of proposal, and the broker alone maps the subject to a
principal, decides what it may do, resolves credentials, and executes it. Its normal dependency
graph excludes `dekopon-broker`, `dekopon-broker-host`, `dekopon-brokerd`, `dekopon-http-host`,
`dekopon-storage-host`, and `dekopon-policy`, and CI's `cargo tree` gate enforces that; only its
tests link `dekopon-brokerd` and `dekopon-storage-host`, as dev-dependencies.

Image generation is model inference, not provider authority. The gateway holds its separately
named model credential; owner configuration fixes the backend/model and authenticated envelopes fix
the reply target. The model chooses only one bounded prompt—not an endpoint, credential, filename,
media type, or destination—and generated bytes stay out of broker protocol, telemetry, and memory.

Message text is untrusted end to end, and so are the agent's own standing orders and mounted
skills from the catalog: none of them can assert identity, name a principal, or widen a grant. An
authorized sender can ask the agent to quote those standing orders through self-inspection and to
read any mounted skill in full through `read_skill`, so neither is confidential.
Standing orders, chat content, subjects, and credentials remain excluded from informational status
reports.

The development transport is the one deliberate exception to "identity comes from
authenticated transport": it trusts its local caller to declare a subject. It grants
nothing by doing so — the broker's attestor grant and identity mapping still gate
everything — but it is a development tool, not a production transport.

Configuration, transport semantics, session bounds, telemetry, the conversation contract,
and the single-UID caveat are documented in
[`docs/dekopond.md`](../../docs/dekopond.md).

## Run

```console
dekopond --config /path/to/dekopond.yaml
```

Part of the [Dekopon](https://github.com/dekopon-agents/dekopon) workspace; see
`docs/design.md` for the authority model and `docs/security-model.md` for the trust
boundaries this daemon sits outside of.

## Durable memory receipts

When the broker returns an effective all-three memory surface, the prompt notes only the on-demand
`memory recent` and `memory search` forms. After model success, the gateway bounds the answer once,
requires complete service/kernel transport acceptance, and sends exactly one fresh hidden record
request containing the original bounded sender text and exact accepted answer. It never retries;
record failure cannot change an already delivered answer. Receipts do not prove human receipt.
