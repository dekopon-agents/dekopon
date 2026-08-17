# dekopond

The unprivileged Dekopon chat gateway and agent daemon. It connects to chat services,
waits for a wakeup, routes each authenticated message to a named agent from the catalog,
runs one bounded model session whose only tool is the sandboxed shell, and replies with
the answer.

- **Transports** — Slack Socket Mode (outbound WebSocket, so no public HTTP endpoint),
  Telegram long polling, and an owner-only Unix development socket.
- **Routing** — first match wins on (transport, direct message or named channel).
  Unmatched traffic is ignored, and a shared channel additionally requires the bot to be
  @-mentioned.
- **Sessions** — a process-wide concurrency ceiling plus per-conversation serialization,
  bounded model turns, bounded capability calls, and one fixed line on failure.
- **Authorization** — every session opens an *attested* broker leg naming the sender's
  canonical subject. An empty capability set ends the session before any model call.
- **Conversations** — one independent session per message today. A per-sender history in
  gateway memory, compacted to question-and-answer pairs inside a sliding window and
  dropped on an idle timeout or a changed capability grant, is designed and is committed
  direction; it caches no authorization.

## Authority

`dekopond` has none. It holds chat bot credentials and model credentials — the things it
needs to hear a question and to ask a model — and it never holds a provider credential, a
policy, or an authorization. Every effect a session drives is submitted to
`dekopon-brokerd` as an on-behalf-of proposal, and the broker alone maps the subject to a
principal, decides what it may do, resolves credentials, and executes it. The dependency
set excludes `dekopon-broker`, `dekopon-broker-host`, `dekopon-http-host`, and
`dekopon-brokerd`, and CI enforces that.

Message text is untrusted end to end, and so are the agent's own standing orders from the
catalog: neither can assert identity, name a principal, or widen a grant.

The development transport is the one deliberate exception to "identity comes from
authenticated transport": it trusts its local caller to declare a subject. It grants
nothing by doing so — the broker's attestor grant and identity mapping still gate
everything — but it is a development tool, not a production transport.

Configuration, transport semantics, session bounds, telemetry, the committed conversation
contract, and the single-UID caveat are documented in
[`docs/dekopond.md`](../../docs/dekopond.md).

## Run

```console
dekopond --config /path/to/dekopond.yaml
```

Part of the [Dekopon](https://github.com/dekopon-agents/dekopon) workspace; see
`docs/design.md` for the authority model and `docs/security-model.md` for the trust
boundaries this daemon sits outside of.
