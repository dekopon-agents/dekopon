# dekopon-agent

The reusable agent session layer shared by Dekopon's embedding binaries. `dekopon-run`
drives one prompt session from a CLI; the `dekopond` daemon drives many from chat
transports. Both consume the same pieces from this crate, so there is exactly one
authoritative copy of each:

- `prompt::run_prompt` — the bounded model tool loop that offers a single sandboxed
  scripting tool (`bash`) instead of one tool per capability.
- `prompt::run_prompt_with_history` — the same loop run as the continuation of a
  `prompt::History`: a window of earlier exchanges bounded by turn count and bytes, since
  there is no tokenizer here and token counts only arrive after a call has been billed.
  Only the prompt and the final answer of each exchange are remembered; the assistant
  turns carrying `tool_calls` and the `tool` results answering them are dropped together,
  which is both the cost control and what makes trimming safe. A `ConversationTurn` holds
  text rather than messages, so it cannot remember a system prompt, cannot hold half a
  tool call, and cannot carry provider replay state out of the session that produced it —
  which is also what lets one conversation replay identically on either model backend
  rather than losing encrypted reasoning silently on the way across. The session's own
  exchange is recorded even when the session fails, so a failed turn is not silently lost.
- `ShellRuntime` — runs each model-authored script on a fresh `dekopon-shell`
  interpreter while spending one session-wide capability budget.
- `SessionInvoker` — capability dispatch that prefers a local read-only leg and falls
  through to a broker leg.
- `BrokerLeg` — a synchronous `CapabilityInvoker` facade over the asynchronous
  `dekopon-broker-protocol` client, valid only on a blocking task; `connect_attested`
  additionally proposes on behalf of a transport-authenticated external subject, which
  the broker honors only under an owner-configured attestor grant.
- `IdSequence` — collision-free trace and invocation identifiers under a caller-chosen
  session prefix.

Nothing in this crate holds authority. The broker leg submits identity-free proposals
over an authenticated Unix socket and reports back whatever the broker decided; this
crate never interprets policy, resolves credentials, or constructs authorization state.
It depends only on the client half of the broker protocol, never on broker internals —
the same dependency discipline CI enforces for `dekopon-run`.

Telemetry follows `docs/observability.md`: spans (`prompt.session`, `prompt.model_turn`,
`prompt.script`) and `dekopon_agent::audit` accounting events carry counts, durations,
and stable categories; prompts, model answers, and script text ride the log stream only
when the embedding binary opts into payload telemetry.

Part of the [Dekopon](https://github.com/dekopon-agents/dekopon) workspace; see
`docs/design.md` for the authority model this crate deliberately sits outside of.
