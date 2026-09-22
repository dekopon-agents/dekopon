# Multi-LLM API implementation plan

**Status: Exploration, reviewed by Fable on 2026-09-22 and decided.** The review verdict and its
findings are recorded in the [Fable review brief](multi-llm-fable-review.md). This document is
the decided input for a `pi-subagent-plan` driver brief; it is not implemented behavior, and it
is not approval to deploy, record paid calls, or publish a release. Source baseline:
`3c155fe68e47d4e656f9321d7a0a4dd8b5b0a817` (`origin/main` at v0.19.0). Every locator below is
`path:line` at that SHA; verify with `git show` before relying on one.

## Outcome and deliberate limits

Run the **same Dekopon prompt/tool loop** against Codex subscription and OpenRouter, selecting a
model in configuration rather than branching orchestration code. Preserve existing
OpenAI-compatible endpoints. Make differences visible through typed requests, provider-specific
options, useful errors and comparable traces. Keep the library usable without the gateway.

This is a small inference-client API, not an agent framework. Borrow the chat gateway's
separation of shared policy from protocol adapters (PR #257's `ChatDriver` + `ProgressPolicy`
over per-transport code), not the `#[async_trait]`/`BoxFuture` style its `Transport` trait uses
(`crates/dekopond/src/transport.rs:14-25`). Nothing in `dekopon-model` or `dekopon-agent` uses
that style today, so there is nothing to migrate away from, only a style not to introduce.

**Ship first:** async generation I/O through `reqwest`; typed core contract; Codex, OpenRouter
Chat and the existing OpenAI-compatible Chat; safe replay; two cache modes and a typed reasoning
control; shared instrumentation; synthetic offline interaction fixtures; one reusable loop
example.

**Defer:** Vertex Gemini and Claude, Bedrock, cache-resource CRUD, OpenAI's explicit
`prompt_cache_breakpoint` vocabulary, direct public OpenAI Responses beyond the existing Codex
dialect, embeddings, generated audio/images, batch/realtime APIs, provider-side tools, an online
model catalog, automatic fallback/retries, price catalogs, dashboards, benchmark infrastructure,
live recording tooling, and crate extraction/publication. Ordinary image/document input already
supported by Dekopon must continue to work.

Future providers get documented module/trait extension points, not SDK dependencies, config
variants with no implementation, or placeholder public types. `clippy::todo` and
`clippy::unimplemented` are denied workspace-wide (`Cargo.toml:141-142`); "no unused public API"
is a review rule, not a lint (`CONTRIBUTING.md:74`: every new public item, dependency, config
field and error variant needs a non-test consumer in the same PR). An unavailable backend fails
config decoding/construction, never panics after accepting a chat.

## 1. Source-grounded decisions

| Current seam | Proposed change |
|---|---|
| `ChatModel::complete` is synchronous (`crates/dekopon-model/src/model.rs:408-433`); both clients send over `ureq` (`model.rs:9`, `chatgpt.rs:360-378`), including Codex generation | Add one native-async inference trait and a shared `reqwest` generation transport; keep one synchronous adapter for the existing loop |
| `run_prompt_session` and `ScriptRuntime` are synchronous (`crates/dekopon-agent/src/prompt.rs:501`); the gateway runs the loop on `spawn_blocking` (`crates/dekopond/src/session.rs:1040`) | Retain that ownership model; no shell or broker-I/O rewrite |
| `ModelMessage` is a private-field struct with a string role (`model.rs:199-210`); `AssistantTurn::replay_items: Vec<Value>` is public but doc-hidden (`model.rs:358-359`) | Role/content enums and a private, dialect-bound continuation; preserve the convenience constructors the loop uses |
| `ModelText`/`TurnEvent` restrict what reaches chat progress (`crates/dekopon-model/src/stream.rs:28,79`) | Preserve the provenance boundary; reasoning/arguments remain internal |
| `ModelCache` in the gateway shares clients by configured model name (`crates/dekopond/src/session.rs:192-227`); options are request-scoped | Cache the async clients; build the lightweight blocking bridge per session |
| `CredentialFile` owns Codex refresh with a cross-process lock (`chatgpt.rs:119-124,270`); `dekopon-brokerd` consumes it too (`crates/dekopon-brokerd/src/credentials.rs:35`) | Keep it, on blocking `ureq`, called from a blocking task; do not migrate account lifecycle |
| Codex refreshes before send and resends once after HTTP 401 (`chatgpt.rs:412-433`, test at `chatgpt.rs:3134-3177`) | **Keep it** (D4); the generation helper itself never retries |
| Root already has `reqwest` 0.12 with `blocking`, `http2`, `rustls-tls-webpki-roots`, `stream` and `futures-util` (`Cargo.toml:50,73`) | `dekopon-model` adds `reqwest` (no `blocking`), `tokio`, `futures-util` and `dekopon-process` dependency lines; it has none of them today (`crates/dekopon-model/Cargo.toml:16-25`) |

Do not depend on `dekopon-http-host`: it is the credential-bound HTTP engine used only by
`dekopon-broker-host` and `dekopon-brokerd` (`docs/architecture.md:29`), and the gateway does not
depend on it today. A small unprivileged transport helper in `dekopon-model` is the right
boundary.

## 2. Library contract: narrow traits, closed runtime selection

Keep the work in `dekopon-model`, with modules rather than new crates. Design notation below
omits imports and supporting types; the implementation must compile on the pinned toolchain
(`rust-toolchain.toml:11`, 1.98.1). None of the proposed names collide with an existing symbol
(verified by grep across `crates/`).

```rust
pub trait InferenceModel: Send + Sync {
    fn generate<'a>(
        &'a self,
        request: GenerateRequest<'a>,
        observe: &'a mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send),
        control: &'a TurnControl,
    ) -> impl Future<Output = Result<AssistantTurn, InferenceError>> + Send + 'a;
}

pub enum ModelClient {
    Codex(CodexClient),
    OpenRouter(OpenRouterClient),
    OpenAiCompatible(OpenAiClient),
}

pub struct GenerateRequest<'a> {
    pub messages: &'a [ModelMessage],
    pub tools: &'a [ModelTool],
    pub options: &'a GenerateOptions,
}
```

Concrete adapters implement the trait; the enum implements it by matching. The loop does not
carry an adapter type parameter. No `async_trait`, boxed-future signatures, dynamic plugin
registry or GAT-based prepared-request hierarchy.

A single `generate` streams safe progress and returns one complete turn. Collecting without
displaying progress uses a no-op observer. Preserve `stream: false` for compatible endpoints
through a private buffered-response decoder feeding the same normalized result rules.

Inside each adapter, **prepare/validate before HTTP**, using private typed wire structs. A public
builder produces structural validity; private preparation checks dialect-specific combinations.
No generic raw `extra_body`; users never construct wire types.

### Message, tool and replay types

- Replace the string role plus unrelated optional fields with `ModelMessage::{System, User,
  Assistant, ToolResults}`. Keep `system`, `user`, `user_with_parts` and `tool` constructors and
  the read-only accessors the loop uses (`model.rs:215-304`).
- Reuse text/image/document content and scoped `BlobReference` handles. No audio/video/embedding
  variants without a first-release consumer.
- `ToolCallId` newtype (new; no such type exists) and complete call structs. JSON Schema and tool
  arguments legitimately use `serde_json::Value`; provider options and lifecycle states do not.
- Preserve ordered assistant content and tool-call order. Completed tool calls are accessible
  only after a successful terminal reduction; partial argument fragments never become executable
  calls.
- Replace the public `AssistantTurn::replay_items: Vec<Value>` with a private
  `Continuation::{Portable, Codex(..), OpenRouter(..)}`. Bounded native JSON subtrees preserve
  unknown fields losslessly. Bind continuation to the configured client/model/dialect; record the
  reported upstream when supplied.
- OpenRouter preserves the structured `reasoning_details` array verbatim, including
  `reasoning.encrypted` items, and sends it back unmodified on the assistant message of the next
  turn (the documented rule: the sequence must match the original output, unmodified).
- One authoritative assistant record owns replay and projections; no independently editable
  native and normalized transcripts.
- Compact cross-message history stays portable and lossy; exact continuation applies inside a
  tool loop. Switching configured providers starts a fresh tool loop.

`ModelText` remains constructible only inside the model crate (`stream.rs:32`). `Debug` and audit
rendering of assets/replay are deliberate projections, never the wire serializer.

### Keep the synchronous loop: one explicit bridge

Add `BlockingModel` implementing the existing synchronous `ChatModel`, wrapping a shared
`Arc<ModelClient>`, an explicitly supplied Tokio `Handle`, and a `TurnControl`. The gateway's
`ModelCache` stores `Arc<ModelClient>`; the bridge is built per session so cancellation never
sticks to the first conversation. It calls `Handle::block_on` **only on the existing blocking
session task**, exactly as the broker leg already does
(`crates/dekopon-agent/src/lib.rs:1023-1027`, "safe specifically because this runs on a
spawn_blocking thread"); reuse that comment's framing. No new runtime per model or turn; no
`block_on` on a runtime worker. The externally reusable API is native async; the bridge exists
for synchronous embedders.

The observer is `+ Send` because the `generate` future is `Send`, and `ChatModel::complete`'s
callback (`model.rs:424`, currently `&mut dyn FnMut(TurnEvent) -> ControlFlow<()>` with no `Send`)
gains the same bound: a `&mut dyn FnMut` object cannot be widened to `+ Send` later. Every
current callback is already `Send` (`TurnStream` holds only `&dyn ProgressSink` and
`&dyn CancellationProbe`, both `Send + Sync`), so this is a signature change across the
implementors listed in §Facts, not a behavior change.

**Cancellation reuses `dekopon_process::CancelSignal`.** `SessionCancellation` already owns a
`CancelHandle`/`CancelSignal` pair fired by the winner of `cancel()`
(`crates/dekopond/src/session.rs:293,337-359`) and exposes `signal()` (`session.rs:382`);
`CancelSignal` is a `tokio::sync::watch` receiver whose `cancelled()` wait is race-safe but
private (`crates/dekopon-process/src/lib.rs:99-153`). `TurnControl` wraps a `CancelSignal` plus
the total deadline; `CancelSignal::cancelled` becomes `pub`. No new primitive, nothing added to
`dekopon-core` (which is wasm-guest-reachable and carries no tokio,
`crates/dekopon-core/Cargo.toml:16-27`). `SessionCancellation::cancelled()`
(`session.rs:369-379`) keeps serving the progress policy; it is not the model's wait.

Dropping a gateway session signals cancellation (`CancellationOnDrop`, `session.rs:404-415`).
Async send/read selects that signal and the deadline, drops the response, and returns
`InferenceError::Cancelled` or `DeadlineExceeded`; never a partial turn. The blocking loop still
owns history, script execution and terminal arbitration. Cancel does not undo tools already run
or guarantee upstream billing stopped.

## 3. One instrumented async HTTP path

Own a small `InferenceHttp` around cloned `reqwest::Client` handles. Construct once per configured
client, not per request. Defaults: rustls verification, bounded connect and total deadlines
(`timeoutMs` becomes the total deadline, as it is for `ureq` today), redirects disabled, shared
keep-alive pools, no retries or fallback. Do not change workspace TLS features or broker transport
defaults.

The helper owns send, status/error handling, byte counts, duration and cancellation. Adapters own
endpoint/auth headers, serde wire mapping and protocol reducers. No middleware pipeline;
`tracing` plus one helper is enough.

Use `Serialize`/`Deserialize` derives on small hand-written wire structs for fields actually
used. No build-time OpenAPI fetching or `Value` for whole requests. Tolerate additive unknown
response fields; reject unknown required semantic events rather than silently completing a
damaged turn.

Adapt the bounded SSE framing (`crates/dekopon-model/src/sse.rs`, `MAX_STREAM_BYTES` 16 MiB at
`sse.rs:24`) to async chunks and reuse it across the Codex and Chat reducers. HTTP chunk
boundaries are not SSE/JSON boundaries. Retain the existing event/body limits and explicit
ownership of accumulated text, arguments and replay. Preserve the tolerant Chat decoding already
tested for llama.cpp/Ollama (`model.rs:1864-1963`).

Preserve attachment handle lifetime checks (`asset.rs:35-46,170-209`) and the two-pass
`compact_json_body` serialization (`model.rs:184-193`), already shared by both clients. Blob reads
stay whole-buffer as today (`asset.rs:302-335`); run them on a blocking task, never on a runtime
worker, and do not add another cloned prompt/base64 buffer.

**Auth exception:** Codex device login/refresh keeps the bounded blocking `ureq` agent
(`lib.rs:44`) inside `CredentialFile`. The adapter calls `refresh_if_needed` on a blocking task,
takes a redacted token snapshot, and holds no credential lock across the streamed answer (the
in-process mutex is already only held for clone/install, `chatgpt.rs:224-243`). Only generation
moves to async `reqwest`; ureq stays in the workspace for auth. On HTTP 401 the adapter does what
it does today: force-refresh once and resend once, before any body byte has been read (D4).

## 4. A sane configuration surface

`models:` entries are the internally tagged `ModelConfig` enum
(`crates/dekopond/src/config.rs:527-582`: `tag = "kind"`, `deny_unknown_fields`,
`rename_all_fields = "camelCase"`, no `flatten` anywhere). `kind: openrouter` is its third arm.
Every helper match (`config.rs:597-631`: `name`, `classes`, `accepts_images`, `timeout_ms`)
gains the arm. Nested blocks are plain structs with their own `deny_unknown_fields`.

```yaml
models:
  - name: primary
    kind: chatgptSubscription
    model: gpt-5.6-sol
    timeoutMs: 120000
    classes: [general]

  - name: explore
    kind: openrouter
    model: anthropic/claude-sonnet-4.5
    apiKeyEnv: OPENROUTER_API_KEY
    timeoutMs: 120000
    classes: [general]
    generation:
      maxOutputTokens: 4096
    reasoning:
      effort: medium
    routing:
      allowFallbacks: false
      requireParameters: true
    cache:
      style: explicitPrefix
      ttl: 5m
```

Changing a route from `primary` to `explore` changes the client, not the loop. OpenRouter uses the
fixed base URL `https://openrouter.ai/api/v1`; custom endpoints stay on `openaiCompatible`. Codex
endpoint and account lifecycle stay pinned. Secrets never appear in YAML: the gateway resolves
`apiKeyEnv` to a plain string exactly as `model_credential` does for `openaiCompatible`
(`crates/dekopond/src/session.rs:122-132`) and the library constructor wraps it in `Redacted`
(`model.rs:471-474`). `OpenRouterClient` follows `OpenAiChatModel::new`, taking the caller's
secret; `ChatGptCodexModel::new` reading `DEKOPON_CHATGPT_AUTH_FILE` itself (`chatgpt.rs:1541`)
is the one pre-existing exception and stays.

Validation pushes into the existing aggregated `Vec<ConfigProblem>` (`config.rs:1135`, rendered by
`render_problems` at `config.rs:2127-2143`) and, for credentials, the existing `StartupProblem`
collection (`crates/dekopond/src/lib.rs:616-638`). No new first-error path. Defaults and supported
omitted fields are documented in `docs/dekopond.md` next to the existing `models:` example
(`docs/dekopond.md:86-105`).

**OpenRouter stays open to unfamiliar model IDs.** Accept syntactically valid names without a
catalog. Validate what is known locally (bounds, invalid combinations, Codex-unsupported
controls); let the endpoint reject unknown model/feature combinations with a useful error. No
preflight probes. `requireParameters` defaults to false upstream, which means unsupported
parameters are silently dropped per provider; record effective local settings, never label a
forwarded setting as remotely honored.

`routing.only` is an endpoint allowlist for repeatable evaluations. If a reported upstream changes
during a bound continuation, fail instead of stripping replay. Unknown upstream is recorded as
unknown.

Generation knobs shipped: `maxOutputTokens`; validated `temperature`/`topP`; `reasoning.effort`
as the closed enum OpenRouter documents (`none`, `minimal`, `low`, `medium`, `high`, `xhigh`,
`max`); existing tool policy. Codex keeps its current frozen request shape (`store: false`,
`stream: true`, `tool_choice: auto`, `parallel_tool_calls: true`,
`include: ["reasoning.encrypted_content"]`, `text.verbosity: low`, optional `prompt_cache_key`;
`chatgpt.rs:1107-1116`) and refuses every OpenRouter-only block at config validation. No
best-effort mode: a locally unsupported control fails.

### Cache configuration: two modes

| Mode | Authored options | Local meaning |
|---|---|---|
| `automatic` (default) | None | No cache fields sent; the upstream may cache automatically |
| `explicitPrefix` | `ttl: 5m \| 1h`, optional | `cache_control: {type: "ephemeral"[, ttl]}` on the last content part of the system message |

`explicitPrefix` is the one control OpenRouter documents as a request field. Anthropic upstreams
honor the marker and the `ttl` (`5m` and `1h` are the only values); Gemini upstreams honor the
marker with a fixed five-minute lifetime that does not extend on hit; every other upstream
ignores it. The trace records the requested style, never a remote guarantee. OpenAI's explicit
vocabulary (`prompt_cache_breakpoint`, restricted to GPT-5.6 and newer) is deferred because
OpenAI caching is automatic and the "automatic placement" the earlier draft named is not a real
field. A separate Gemini mode is not needed: it would send the same marker.

The anchor is the end of the system message the prompt builder already emits
(`crates/dekopon-agent/src/prompt.rs:544`); no public anchor type in v1. `explicitPrefix` with no
system message fails at prepare, before send. Request-scoped random affinity continues to come
from the conversation/route (`crates/dekopond/src/cache_key.rs:40-65`), not from an operator key.

Codex accepts only `automatic`. No named-resource CRUD, response memoization, warming or
keepalive. Always send the request header `X-OpenRouter-Cache: false` so a preset cannot turn a
model evaluation into response replay.

## 5. Unified errors and compatibility policy

One public `thiserror` error replaces `ModelError` (`model.rs:1158-1184`) everywhere, including
the synchronous `ChatModel` and the agent's `PromptError` mapping; `Interrupted` becomes
`Cancelled`. No parallel error worlds.

```rust
pub enum InferenceError {
    InvalidRequest(RequestError),
    Unsupported(UnsupportedFeature),
    Authentication(AuthError),
    RateLimited(RateLimitError),
    Provider(ProviderFailure),
    Transport(TransportFailure),
    Protocol(ProtocolFailure),
    Attachment(BlobError),
    Cancelled,
    DeadlineExceeded,
}
```

Construct only variants with real consumers. Provider/transport failures carry phase (before send,
awaiting headers, reading body), optional HTTP status, provider code, request ID and Retry-After,
plus a bounded sanitized diagnostic (the existing `MAX_ERROR_BODY_BYTES` cap, `model.rs:1196`).
Preserve sources; no string matching on `Display`. An unknown upstream code stays a bounded string
inside `ProviderFailure`.

Streaming error frames under HTTP 200 are failures. EOF succeeds only when the dialect's terminal
rule is satisfied. Retain trailing usage before completing. HTTP failures and rate limits
terminate the turn once; the only resend anywhere is the Codex 401 auth repair (D4).

A protocol-complete response can still carry output-limit/refusal/filter/tool-call finish
reasons. Never execute incomplete tool JSON. Keep already displayed partial text and observed
usage separate from successful history. The error path emits one failure with its cause, not a
zero-token success.

Compatibility cycle: see the trace → write a small synthetic fixture from it → change one
adapter/reducer → scoped tests → normal owner release. No in-chat repair, parameter guessing or
fallback.

## 6. Tracing that allows honest comparison

Reuse `prompt.model_turn` (`prompt.rs:712-724`) and `accounting.model.turn` (`prompt.rs:776,802,
826`); avoid a second accounting pipeline. The shared helper keeps one `model.complete` child span
per generation exchange (the name both clients emit today, `chatgpt.rs:404`, `model.rs:512`).
`dekopon-model` depends on `tracing` only, never `dekopon-telemetry`; `dekopon-agent`'s
`current_trace_context()` read (`lib.rs:225`) is not a precedent for a subscriber.

| Recorded field group | Semantics |
|---|---|
| Identity | Configured model name, requested provider/model, API dialect, returned model and upstream provider when present; unknown stays absent |
| Controls | Requested generation and cache style, requested TTL, stream/buffered mode; no credential or affinity identifier |
| Timing | Total duration, headers/first-event/first-visible-text timing, outcome; no invented TTFT for tool-only responses |
| Work | Input/output tokens, cached read/write, reasoning tokens when reported, tool-call count, response bytes |
| Failure | Stable error kind and phase, status/provider code/request ID when safe, partial-output indicator |

Only the `usage.*` fields, `stream.deltas`, `stream.first_delta_ms`, counts and the coarse
`outcome`/`error` string exist today; identity beyond the configured name, controls, response
bytes, request ID and the whole failure row are new. Each metric has one owner: the helper measures
transport; the reducer reports native usage/finish; the agent records session turn/budget. Extend
`docs/observability.md:31-59` and `:948-957` with the mapping. Absent means unknown, not zero.
Normalize included-versus-additive cache tokens per codec; do not double-count reasoning within
output. Preserve the existing audit records (`agent.model.prompt`, `agent.model.answer`,
`agent.tool.*`) and the session span carried across the blocking boundary (`session.rs:867,1041`).
Never hold an entered span guard across `.await` (`clippy.toml` already denies it); instrument
futures. Do not forward `traceparent` to inference endpoints.

For comparisons, run the same core-loop example with the same prompt/tool surface/limits and a
different model name, with a caller-supplied experiment label. Report cold versus subsequent tool
turns, reported cache hits, tool count, TTFT and elapsed time. No price registry or benchmark
suite.

## 7. Offline proof with the fixtures we have

**No VCR dependency.** `crates/dekopon-model/src/mock.rs` already scripts a sequence of loopback
responses (`json`, `sse`, `failure`, `hang_up`) and records every request
(`mock.rs:18-122`), and the Codex two-turn replay test already runs against it
(`chatgpt.rs:2929-2994`). Extend that server, not a third loopback beside
`dekopon-test-support`'s. `httpmock` would be a new dependency pulling a second
`hyper-rustls` stack under `[bans] multiple-versions = "deny"` and an MPL-licensed optional
feature; the spike is removed from scope (D6).

Fixtures are hand-written synthetic SSE bodies derived from the documented wire shape (or, later,
from a trace), committed as `include_str!` files. `AGENTS.md:30` still prohibits committing fetched
provider fixtures; live recording tooling is out of v1, so nothing sanitizes a real capture. The
four transcript fixtures in `crates/dekopon-test-support/transcripts/` stay as they are.

| Case | Evidence |
|---|---|
| Codex two-turn tool exchange | Port the existing test to the async adapter: completed tool call + encrypted reasoning replay, then final text/usage; assert second-request replay and stable prefix |
| OpenRouter two-turn tool exchange | One sequence with `reasoning_details` (text + encrypted items), streamed `tool_calls` deltas, the final usage-only chunk (empty delta, repeated `finish_reason`); prove parse and re-serialization |
| Existing OpenAI-compatible behavior | Keep the streamed/buffered equality table (`model.rs:1864-1963`) |
| Refused/malformed turn | Table: HTTP 401 (Codex: exactly two requests; OpenRouter: one), 429, provider failure, HTTP-200 error frame, truncated stream, invalid final tool arguments; typed errors, request counts asserted |
| Config validation | Defaults, each legacy kind, `openrouter` with every block, two simultaneous problems reported together, Codex refusing `cache`/`reasoning`/`routing`, `explicitPrefix` with no system message |
| Cancellation | One `hang_up`-style stalled body cancelled through `TurnControl` without waiting for the deadline; one deadline test |
| Traces | One assertion per dialect on the recorded field set, missing usage staying absent, no credential sentinel |

Use one split-chunk parser regression and the existing limit tests; add the missing
"exactly at `MAX_STREAM_BYTES`" companion (`sse.rs` has only the over-limit case). No model quality
goldens, no provider matrix, no new CI job. Normal package/workspace gates remain authoritative.

## 8. Decisions (settled)

| # | Decision | Where recorded |
|---|---|---|
| D1 | Keep the synchronous loop; `BlockingModel` bridges over `Handle::block_on` on the blocking session task, precedent `dekopon-agent/src/lib.rs:1023-1027` | §2 |
| D2 | `generate` futures are `Send`; the observer and `ChatModel::complete`'s callback both carry `+ Send` | §2 |
| D3 | `TurnControl` wraps `dekopon_process::CancelSignal` + deadline; `CancelSignal::cancelled` goes `pub`; nothing enters `dekopon-core` | §2 |
| D4 | Keep the one-shot Codex 401 refresh-and-resend, inside the Codex adapter, only before any body byte was read; the test asserting two requests stays; no `upgrading.md` entry | §3 |
| D5 | Cache modes are `automatic` and `explicitPrefix { ttl?: 5m \| 1h }`; anchor is the end of the system message; Codex accepts only `automatic`; OpenAI explicit vocabulary and a Gemini mode deferred | §4 |
| D6 | No `httpmock`; extend `dekopon-model/src/mock.rs`; fixtures are synthetic `include_str!` files; no live recording tooling | §7 |
| D7 | OpenRouter wire: fixed base URL; `provider: {allow_fallbacks, require_parameters, only}`; `reasoning: {effort}`; header `X-OpenRouter-Cache: false`; no `usage.include` or `stream_options` (deprecated no-ops); top-level response `provider` optional; `openrouter_metadata` not parsed | §4, §Facts |
| D8 | `kind: openrouter` is the third `ModelConfig` arm; `apiKeyEnv` required; blocks `generation`, `reasoning`, `routing`, `cache` as nested `deny_unknown_fields` structs; problems join the existing `Vec<ConfigProblem>` | §4 |
| D9 | One error type: `InferenceError` replaces `ModelError` in every crate; `Interrupted` becomes `Cancelled` | §5 |
| D10 | `ModelCache` stores `Arc<ModelClient>`; `ChatGptCodexModel` and `OpenAiChatModel` are deleted once their adapters land; `ureq` remains only inside `CredentialFile` | §2, §3 |
| D11 | The reusable example is `crates/dekopon-agent/examples/compare_models.rs`: owns a runtime, runs `run_prompt_session` on `spawn_blocking` over a harmless local `ScriptRuntime`, takes the model kind/id from arguments and the secret from the environment; the offline proof is the integration test that drives the same loop over both adapters against the mock server | §8 stage 5 |
| D12 | Fixture placement (in-crate `include_str!` under `dekopon-model`) and every crate-internal name, module layout, buffer size under a stated cap, and test structure are the driver's to decide and report under "Driver decisions" | `AGENTS.md` §Authority |

## 9. Implementation sequence

One driver, one worktree, one PR, one commit per stage, each stage ≤ ~1k changed lines with a
scoped gate while iterating and the full workspace gate at commit (`docs/development.md:240-292`;
the exact clippy line is `cargo clippy --workspace --all-targets --all-features --locked -- -D
warnings`, rustdoc is `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
--locked`). Every stage compiles workspace-wide because the driver updates consumers in the same
commit; no transitional shims. Rebase onto `origin/main` at the start of each stage. CI runs the
OTLP OpenObserve smoke job on every crate change automatically
(`.github/scripts/classify_ci_changes.py` cascades `run_rust` into `run_otel`); it is not a
discretionary local step. `cargo deny --all-features check` runs whenever `Cargo.toml`,
`Cargo.lock` or `deny.toml` change.

1. **Types and errors.** `ModelMessage` enum, `ToolCallId`, private `Continuation`,
   `InferenceError` replacing `ModelError`, `+ Send` on the `ChatModel` callback. Consumers in
   `dekopon-agent`, `dekopond`, `dekopon-test-support` updated. Exit: workspace green; no public
   unused type.
2. **Async transport and the Codex adapter.** `InferenceHttp`, `TurnControl`, async SSE reader,
   `CodexClient` on `reqwest` with the frozen request shape, `BlockingModel`, `ModelClient` with
   its first arm; gateway wires the bridge and `SessionCancellation::signal()`;
   `ChatGptCodexModel` deleted. Ported two-turn, 401, refresh and cancellation tests. Exit: Codex
   regressions pass on the new path; credentials never enter traces.
3. **OpenAI-compatible adapter.** `OpenAiClient` on the helper, streamed and buffered; tolerant
   decoding table retained; `OpenAiChatModel` deleted; `ureq` gone from generation. Exit: the
   equality table and limit tests pass; `cargo tree -p dekopon-model` shows `ureq` only through
   the credential path.
4. **OpenRouter.** Adapter with reasoning/routing/cache codec, replay preservation, error and
   usage mapping; `kind: openrouter` config, validation, and the two-turn fixture. Exit: the
   validation table passes; advanced settings map exactly or fail; `X-OpenRouter-Cache: false`
   asserted.
5. **Example, tracing, docs.** The example (D11) and its offline integration test; the new trace
   fields and the mapping in `docs/observability.md`; `docs/inference.md`, `docs/dekopond.md`,
   crate README, `[Unreleased]` in `CHANGELOG.md` (currently empty). Exit: required CI green,
   docs describe reality, an outside embedder can construct a client without gateway config or
   a global subscriber.

**Stops** are the four in `AGENTS.md` §Authority terms: forking, patching or bumping a dependency
to make something pass; moving a config key/value, wire field or one of D1–D12 away from this
table; deleting something this plan keeps or keeping what it deletes; owner-only actions
(releases, tags, publishing, merging, any real call to `chatgpt.com` or `openrouter.ai`).
Everything inside a crate is the driver's.

## 10. Facts an implementer must be told

- `ChatModel` implementors that change with D2 and D9: `model.rs:504`, `chatgpt.rs:396`,
  `crates/dekopon-test-support/src/model.rs:152`, `crates/dekopon-agent/src/prompt.rs:1961,2200,
  2242,3048`, `crates/dekopond/src/tests.rs:2267,3274,6380,12391`,
  `crates/dekopond/src/tests/late_photos.rs:14`. Callers bound `M: ChatModel + ?Sized`
  (`prompt.rs:244-647`); `SharedModel = Arc<dyn ChatModel + Send + Sync>` (`session.rs:88`).
- The loop's callback is a local closure over `TurnStream` (`prompt.rs:762-763`), which checks
  cancellation after every event (`prompt.rs:1153-1160`); that stays the sync-side probe.
- `dekopon-model` has no `tests/` directory; all tests are inline `#[cfg(test)]` modules, and
  the model crate's loopback server is `#[cfg(test)]` (`lib.rs:18-19`).
- OpenRouter streams tool calls as OpenAI-style `tool_calls` deltas with `index`; usage and
  cost always arrive in a final chunk with an empty delta; `reasoning_details` items carry
  `type` (`reasoning.text`, `reasoning.summary`, `reasoning.encrypted`), `id`, `format`,
  `index`; `reasoning.exclude` is prose-only and not in the schema, so it is not shipped;
  `prompt_tokens_details.cached_tokens` and `cache_write_tokens` are the cache counters.
- Codex sends `authorization`, `chatgpt-account-id` (decoded from the JWT, `chatgpt.rs:871-884`),
  `originator: dekopon`, `user-agent`, `openai-beta: responses=experimental`, `accept:
  text/event-stream` (`chatgpt.rs:363-377`); there is no session header. Replay items are
  forwarded verbatim as raw values (`chatgpt.rs:1062-1064`).
- No request ID or response header is read anywhere today; the failure row in §6 is entirely
  new instrumentation. `ModelUsage` (`model.rs:334-346`) already normalizes both dialects'
  token names and never invents zero.
- `expect_used` is not yet denied (`Cargo.toml:154`), but `AGENTS.md` §Panics forbids it on
  untrusted input; `unwrap_used` and `clone_on_ref_ptr` are denied.
- `docs/upgrading.md` entries are `## <name> (0.20.0)` prose sections at the top; only the
  `Interrupted` → `Cancelled` rename and the `InferenceError` surface need one.
- `docs/chatgpt-credential.md:33-36` documents the forced refresh a 401 triggers; with D4 it
  stays accurate and gains one sentence saying the turn is resent once.

## 11. Future extension recipe, not future scaffolding

A new provider adds: one concrete client implementing `InferenceModel`; private serde
request/response types and reducer; one enum/factory arm; config conversion; error/usage
mapping; one two-turn fixture. Shared loop, HTTP observation and accounting do not change unless
the provider exposes new semantics.

When actually implementing Bedrock or Vertex, add only the needed types then:

- Bedrock Converse has `cachePoint` unions and non-SSE AWS event streams; use a mature
  signing/event codec. An SDK exception to the helper needs a reason and the same observation
  contract.
- Vertex Gemini uses native parts/thought signatures; named content is prompt input with separate
  retained-resource authority. Resource lifecycle remains deferred.
- Vertex Claude uses Anthropic Messages via its own publisher endpoint; different lifetime rules
  stay in provider-specific enums.
- Unknown provider fields belong in private lossless continuation, not in every public message.

Reuse outside Dekopon means keeping configuration discovery, routing, subscriber setup and tool
execution out of `dekopon-model`. Extraction to a new package is an earned later decision.

## 12. Definition of done and review boundary

The implementation succeeds when Codex and OpenRouter run the same core loop with shared async
generation transport/tracing, typed settings/replay/errors, bounded streaming/cancellation and
the offline proofs in §7. Existing compatible clients, asset safety, tool authorization and
Codex credential ownership survive. Runtime provider incompatibility fails clearly and is
repaired outside the chat.

The next step is a `pi-subagent-plan` driver brief built from §8–§10, rehearsed, then driven
with `pi-drive`; the landed PR gets one fresh Fable adversarial review. The review companion is
the [Fable review brief](multi-llm-fable-review.md).

### Reference points

- [Current inference path](inference.md), [gateway config](dekopond.md),
  [observability](observability.md), [credential ownership](chatgpt-credential.md),
  [development gates](development.md).
- [Gateway unification PR #257](https://github.com/dekopon-agents/dekopon/pull/257).
- [OpenRouter prompt caching](https://openrouter.ai/docs/guides/best-practices/prompt-caching),
  [reasoning](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens),
  [routing](https://openrouter.ai/docs/guides/routing/provider-selection),
  [response caching](https://openrouter.ai/docs/guides/features/response-caching),
  [OpenAPI schema](https://openrouter.ai/openapi.json) (the authority where prose and schema
  disagree).

No benchmark, live call or compiled prototype was performed for this plan.
