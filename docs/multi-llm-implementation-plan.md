# Multi-LLM API implementation plan

**Status: Exploration, ready for design review.** This is a proposed implementation sequence, not implemented behavior or approval to deploy, record paid calls, or publish a release. Initial source baseline: `3c155fe68e47d4e656f9321d7a0a4dd8b5b0a817`.

## Outcome and deliberate limits

Run the **same Dekopon prompt/tool loop** against Codex subscription and OpenRouter, selecting a model in configuration rather than branching orchestration code. Preserve existing OpenAI-compatible endpoints. Make differences visible through typed requests, provider-specific options, useful errors and comparable traces. Keep the library usable without the gateway.

This is a small inference-client API, not an agent framework. Borrow the chat gateway's separation of shared policy from protocol adapters, not its older boxed-future implementation style. RubyLLM is the ergonomics benchmark; Rig and genai are useful examples, not proposed orchestration dependencies.

**Ship first:** async inference I/O through `reqwest`; typed core contract; Codex, OpenRouter Chat and existing OpenAI-compatible Chat; safe replay; a few useful cache/reasoning controls; shared instrumentation; offline cassettes; one reusable loop example.

**Defer:** Vertex Gemini **and Claude**, Bedrock, cache-resource CRUD, direct public OpenAI Responses beyond the existing Codex dialect, embeddings, generated audio/images, batch/realtime APIs, provider-side tools, an online model catalog, automatic fallback/retries, price catalogs, dashboards, benchmark infrastructure and crate extraction/publication. Ordinary image/document input already supported by Dekopon must continue to work.

Future providers get documented module/trait extension points, not SDK dependencies, config variants with no implementation, or placeholder public types. Repository lints reject `todo!()`/`unimplemented!()` and unused public APIs; represent the requested TODOs in this plan. An unavailable backend fails config decoding/construction, never panics after accepting a chat.

## 1. Source-grounded decisions

| Current seam | Proposed change |
|---|---|
| `dekopon-model::model::ChatModel` is synchronous; two clients use `ureq` | Add one native-async inference trait and shared `reqwest` generation transport; keep a narrow synchronous adapter for the existing loop |
| `dekopon-agent::prompt` and `ScriptRuntime` are synchronous; gateway runs them in `spawn_blocking` | Retain that ownership model in the first release; no shell or broker-I/O rewrite |
| `ModelMessage`, calls and replay lean toward OpenAI wire shapes | Introduce role/content enums and private, dialect-bound continuation; preserve current convenience constructors where useful |
| `ModelText`/`TurnEvent` restrict what can reach chat progress | Preserve their provenance boundary; reasoning/arguments remain internal |
| `ModelCache` shares clients by configured model; options are request-scoped | Preserve pool/client reuse; immutable model defaults plus per-request affinity/control |
| `CredentialFile` owns shared Codex refresh, also consumed by broker credentials | Keep that implementation and its lock/write-back semantics; do not migrate account lifecycle just to change inference HTTP |
| Codex currently refreshes and resends once after HTTP 401 | Make this behavior change explicit: refresh before send; return 401 as a typed failure, no automatic inference resend |
| Root already has `reqwest` 0.12 with rustls/http2/stream and `futures-util` | Reuse them; add model-crate dependency/features only as needed, no second async HTTP stack |

Do not depend on `dekopon-http-host` to reuse its HTTP code: that is a privileged effect boundary excluded from the gateway. A small unprivileged transport helper in `dekopon-model` is the right boundary.

## 2. Library contract: narrow traits, closed runtime selection

Keep the work in `dekopon-model`, with modules rather than new crates. Representative design notation below omits imports/supporting types; implementation must compile on the pinned toolchain.

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

Concrete adapters implement the trait; the enum implements it by matching. The loop need not carry an adapter type parameter through every gateway object. No `async_trait`, boxed-future signatures, dynamic plugin registry or GAT-based public prepared-request hierarchy is needed.

A single `generate` streams safe progress and returns one complete turn. Collecting without displaying progress uses a no-op observer; do not add a second nonstreaming orchestration path. Preserve `stream:false` for compatible endpoints using a private buffered-response decoder feeding the same normalized result rules.

Inside each adapter, **prepare/validate before HTTP**, using private typed wire structs. A public builder produces structural validity; private preparation checks dialect-specific combinations. Do not expose a generic raw `extra_body`, nor force users to construct SDK wire types.

### Message, tool and replay types

- Replace string roles plus unrelated optional fields with `ModelMessage::{System, User, Assistant, ToolResults}`. Keep ergonomic constructors and read-only accessors for the current loop.
- Reuse text/image/document content and scoped `BlobReference` handles. No audio/video/embedding variants without an actual first-release consumer.
- Use a `ToolCallId` newtype and complete call structs. JSON Schema and arbitrary tool arguments legitimately use `serde_json::Value`; provider options and lifecycle states do not.
- Preserve ordered assistant content and tool-call order. Completed tool calls are accessible only after a successful terminal reduction; partial argument fragments never become executable calls.
- Replace application-constructible `replay_items: Vec<Value>` with private `Continuation::{Portable, Codex(...), OpenRouter(...)}`. Private bounded native JSON subtrees may preserve unknown fields losslessly. Bind continuation to the configured client/model/dialect; record actual router upstream when supplied.
- OpenRouter must preserve structured `reasoning_details`, including signed/encrypted data. Do not reconstruct it from visible reasoning text or discard it because the user does not display reasoning.
- One authoritative assistant record owns replay and projections. Do not maintain independently editable native and normalized transcripts.
- Existing compact cross-message history is intentionally portable/lossy; exact continuation applies inside a tool loop. Switching configured providers starts a fresh tool loop, not an implicit replay conversion.

`ModelText` remains constructible from provider-visible answer content only inside the model crate. `Debug` and audit rendering of assets/replay are deliberate projections, never the provider wire serializer.

### Keep the synchronous loop: one explicit bridge

Add `BlockingModel` implementing the existing synchronous `ChatModel`, wrapping a shared `ModelClient`, an explicitly supplied Tokio `Handle`, and request/session cancellation control. Cache only the underlying clients; create the lightweight bridge per session so cancellation/affinity never sticks to the first conversation. It calls `Handle::block_on` **only on the existing blocking session task**, while real network I/O uses async `reqwest`. No new runtime per model/turn and no `block_on` on an async worker thread. The externally reusable API is native async; the bridge is only for synchronous embedders.

Audit the callback signature's `Send` bound through existing prompt/test callers. Keep one synchronous prompt loop, not parallel async/sync copies. The standalone example owns a Tokio runtime, then runs the same core loop on `spawn_blocking`. That is sufficient for this release; making the entire shell runtime async is independent future work.

Connect `SessionCancellation` (currently atomic state plus `Notify`) to a race-safe async wait used by `TurnControl`: check/register/check or a retained watch-state notification, not polling/sleeping. The same authoritative cancelled state drives the current probe and the async wait. Put only the small shared cancellation primitive in a dependency-safe lower crate/module; no dependency from model to agent/gateway. Prefer existing Tokio primitives over a new cancellation framework. Verify no lost notification when cancellation precedes registration.

Dropping/abandoning a gateway session signals cancellation. Async send/read selects that signal and the overall deadline, drops the response, and returns a failure; it does not return a successful partial turn. The blocking loop still owns history, script execution and final arbitration. Cancel does not undo tools already run or guarantee upstream billing stopped.

## 3. One instrumented async HTTP path

Own a small `InferenceHttp` around cloned `reqwest::Client` handles. Construct once per configured client/security policy, not per request. Defaults: rustls verification, bounded connect and total request deadlines, redirects disabled, shared keep-alive pools, no inference retries or automatic fallback. Do not change workspace TLS/features or broker transport defaults unnecessarily.

The helper owns request send, status/error handling, byte counts, duration and cancellation. Adapters own endpoint/auth headers, serde wire mapping and protocol reducers. Do not build a generic middleware/plugin pipeline. `tracing` plus one helper is enough; a middleware crate is not required merely to instrument requests.

Use `Serialize`/`Deserialize` derives on **small hand-written wire structs/enums** for fields actually used. Serde is serialization/code generation, not a provider-spec downloader. No build-time OpenAPI fetching, enormous generated SDK model trees or `Value` for entire requests. Tolerate additive unknown response fields; reject unknown required semantic events rather than silently completing a damaged turn. For preserved replay records, retain unknown native fields locally instead of inventing variants globally.

Adapt the existing bounded SSE framing to async chunks and reuse it across Codex/Chat reducers. HTTP chunk boundaries are not SSE/JSON boundaries. Do not collect a whole streaming response; retain existing event/body limits and explicit ownership of accumulated text, arguments and replay. Preserve tolerant Chat decoding already tested for compatible endpoints rather than imposing stricter OpenAI-only assumptions.

Preserve attachment handle lifetime checks and current allocation discipline. Do not introduce another cloned prompt/base64 buffer. Reuse current measured request serialization where practical, offloading unavoidable blocking asset reads/encoding from runtime workers. Streaming request uploads can be a later measured optimization; migrating to async is not permission to load entire conversation assets repeatedly.

**Auth exception:** existing Codex device login/refresh can keep bounded blocking `ureq` temporarily. Call the single `CredentialFile` path on a blocking task, return a redacted token snapshot, and release credential locks before generation HTTP. Do not hold a credential mutex across a streamed answer or duplicate refresh logic. Thus all **generation** requests share async reqwest; this plan does not promise immediate removal of ureq from the workspace. On a 401, invalidate the rejected access snapshot for the next call's preflight refresh without resending the failed turn; retain cross-process refresh serialization. An auth failure remains visible and terminal for that turn.

## 4. A sane configuration surface

Keep the existing `models:` list and route model names. Preserve existing `kind: chatgptSubscription`, `kind: openaiCompatible`, `timeoutMs`, classes and modalities. Add `kind: openrouter` instead of requiring users to disguise it as a generic endpoint.

Illustrative proposed entries (not valid in current releases):

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
    routing:
      allowFallbacks: false
      requireParameters: true
    cache:
      mode: providerDefault
```

Changing the route from `primary` to `explore` changes the client, not the loop. OpenRouter uses a fixed trusted base URL; keep custom endpoint configuration on `openaiCompatible`. Codex endpoint and account lifecycle stay pinned. Secret values never appear in YAML; the gateway resolves names/paths into `Redacted`, while library constructors accept caller-provided credentials rather than reading environment/config themselves.

Use tagged config enums with `deny_unknown_fields`; do not rely on unsupported serde flatten/unknown-field combinations. Convert authored config once into validated model settings. Aggregate independent static errors at startup, then refuse startup; “fail fast” does not mean hide the second invalid model behind the first. Defaults and supported omitted fields are documented in one place.

**OpenRouter must remain open to unfamiliar model IDs.** Accept syntactically valid model names without a hardcoded catalog allowlist. Validate things we know locally (bounds, invalid option combinations, Codex-specific unsupported controls); let the actual endpoint reject unknown model/feature combinations with a useful error. No preflight paid probes, no model-registry service. `requireParameters:true` helps routing but is not proof every backend preserves every parameter. Record observed provider and effective local settings; do not label a forwarded setting as remotely honored.

Optional `routing.only` is an endpoint allowlist for repeatable evaluations. Default no automatic router fallback; ordering/allowlists do not guarantee signed reasoning portability. If a reported upstream changes during a bound continuation, fail instead of silently stripping replay. Unknown upstream is recorded as unknown, not promoted to a verified pin.

Only implement generation knobs actually consumed by both adapter code and a useful call site: output limit; optional validated temperature/top-p where supported; typed reasoning intent/native router settings; existing tool policy. Codex retains its current supported defaults until endpoint-specific support is evidenced; never send public OpenAI parameters there by model-name analogy. Unknown optional remote support is diagnosed on response, not silently dropped. Omit a general best-effort mode from v1: local unsupported controls fail.

### Cache configuration: modest controls, correct distinctions

Ship `providerDefault`, `noExplicitControls`, and OpenRouter-native prefix modes below. Request-scoped random affinity continues to come from the conversation/route, not an operator-maintained key. Distinguish router session affinity from upstream fields in the codec.

| Proposed mode | Authored options | Local meaning |
|---|---|---|
| `providerDefault` | None | Provider may cache automatically; no claimed lifetime |
| `noExplicitControls` | None | Omit controls, not a no-retention guarantee |
| `claudePrefix` | `ttl: 5m \| 1h`, placement `stablePrefix \| automatic` | Explicit stable-prefix marker, or router's top-level automatic placement |
| `openaiPrefix` | `ttl: 30m`, placement `stablePrefix \| automatic` | Router's native breakpoint/options vocabulary, not Claude controls |
| `geminiPrefix` | placement `stablePrefix` | Router-managed fixed five-minute cache; no duration selector or named resource |

The routine operator example needs none of these advanced modes. A `stablePrefix` anchor is issued by the prompt builder immediately after its stable instruction prefix, before history/current user input. Do not guess array index zero, cache changing timestamps, or duplicate prompts. Library code can accept a typed point at a real block boundary; v1 YAML need not expose arbitrary lists of message indices or mixed TTL plans. If a requested anchor is absent/ineligible, fail before send.

Use `ClaudeTtl::{FiveMinutes, OneHour}` and `OpenAiCacheTtl::ThirtyMinutes`, not one global duration enum. Points and exact valid wire options stay coupled to the selected cache style. An unfamiliar model with a router-native style can be forwarded as an explicit experiment; trace the request and fail on rejection, rather than claim a model-capability certification. No automatic mapping from a model-name substring to cache policy.

Codex rejects all explicit prefix/TTL modes; it supports current affinity only. No direct Gemini named-resource construction/renewal/deletion, response memoization, cache warming or keepalive calls. Send `X-OpenRouter-Cache: false` so a preset cannot accidentally turn a model evaluation into response replay. Exact duration/placement restrictions not needed by these implemented paths stay in future work, not a shipped capability database.

## 5. Unified errors and compatibility policy

Use a public `thiserror` error with action-oriented variants, private source details where appropriate, and stable low-cardinality `kind()`/`phase()` accessors:

```rust
pub enum InferenceError {
    InvalidRequest(RequestError),
    Unsupported(UnsupportedFeature),
    Authentication(AuthError),
    RateLimited(RateLimitError),
    Provider(ProviderFailure),
    Transport(TransportFailure),
    Protocol(ProtocolFailure),
    Cancelled,
    DeadlineExceeded,
}
```

Construct only variants with real consumers. Provider/transport failures carry phase (before send / awaiting headers / reading body), optional HTTP status, provider code, request ID and Retry-After, plus a bounded sanitized diagnostic. Preserve error sources; no string matching on `Display`. Unknown upstream code stays a bounded string inside `ProviderFailure`, not an enum variant per vendor error.

Streaming error frames under HTTP 200 are failures. EOF is only successful when that dialect's terminal rule is satisfied; a finished text block is not a finished response. Retain trailing usage before completing. HTTP failures and rate limits terminate the turn once; no resend/backoff/failover in the client. Retry metadata is diagnostic, not a promise of safe replay.

A protocol-complete response can still have output-limit/refusal/filter/tool-call finish reasons. Never execute incomplete tool JSON. Keep already displayed partial answer text and trustworthy observed usage separately from successful assistant history. The error path emits one useful failure with its cause, not a fabricated zero-token success.

Compatibility super-cycle: see the trace → reproduce with a small cassette → change one adapter/reducer → run scoped tests/CI → release/deploy through normal owner workflow. Do not implement in-chat repair, parameter guessing or fallback to hide compatibility errors.

## 6. Tracing that allows honest comparison

Reuse `prompt.model_turn` and `accounting.model.turn`; avoid an alternative accounting pipeline. The shared HTTP helper adds one child span per generation exchange. The core library uses `tracing` but never installs a subscriber or OTLP exporter. Gateway/external embedding owns those.

| Recorded field group | Semantics |
|---|---|
| Identity | Configured model name, requested provider/model, API dialect, returned model/provider when present; unknown stays absent |
| Controls | Requested/effective-local generation and cache style, requested TTL, stream/buffered mode; no credential or raw affinity identifier |
| Timing | Total generation duration, headers/first-event/first-visible-text timing, completion outcome; no invented TTFT for tool-only responses |
| Work | Input/output tokens, cached read/write, reasoning tokens when reported, tool-call count, response bytes |
| Failure | Stable error kind and phase, status/provider code/request ID when safe, partial-output indicator |

Each metric has one owner: HTTP helper measures transport; reducer reports native usage/finish; agent records session turn/budget. Define the field mapping in `docs/observability.md` during implementation. Keep native provider counters as bounded detail when their meaning differs; absent means unknown, not zero. Normalize included-versus-additive cache tokens per codec, and don't double-count reasoning within output. Preserve existing prompt/answer/tool audit records and trace propagation across the blocking bridge. Never hold an entered span guard across `.await`; instrument futures. Do not forward traceparent to third-party endpoints.

For comparisons, run the same core-loop example with the same prompt/tool surface/limits and choose another model name. Record a caller-supplied experiment label, not a new evaluator service. Report cold versus subsequent tool turns, cache hits actually reported, tool count, TTFT and elapsed time; do not claim equal tokenizer counts or compare a reasoning provider's first text with another's first transport event. No stored price registry, scorecard automation or p95 benchmark suite in v1. The example uses a harmless local tool; never automatically replay real external effects across providers.

## 7. Small VCR proof, not a conformance campaign

**Preferred first candidate: `httpmock` as a dev-only record/playback server.** It has documented forwarding/recording/playback and keeps the real reqwest stack/parser under test without production middleware. Spend one focused spike confirming our pinned compiler/dependency compatibility, deterministic two-request matching, complete SSE-body preservation and offline-only playback. No new production dependency for test recording. If cassette playback buffers SSE, that is acceptable for deserialization/replay tests, not proof of streaming timing.

Do not combine several VCR libraries or build a cassette service. If the small spike fails, retain current transcript fixtures plus a tiny loopback response peer; record the reason and stop shopping. A simple crate-local helper is enough. Scope endpoint injection to tests; do not add an arbitrary production Codex endpoint override for fixture convenience.

**Repository fixture policy:** current `AGENTS.md` prohibits committing fetched provider fixtures. Live recordings therefore remain opt-in, local, outside the checkout, using approved disposable prompts/accounts; never auto-record on a cassette miss or in CI. Raw recording must not persist credentials, auth cookies/account identifiers or private conversation content; if the recorder cannot exclude them before writing, do not enable recording. CI gets a few deliberately synthetic public-safe cassettes in the same format, plus existing parser transcripts. Checking in sanitized derivatives of live recordings needs an explicit policy decision; it is not silently authorized by this plan. Keep security policy separate from whether a VCR library is useful.

Minimum new evidence, reusing existing tests wherever possible:

| Case | Evidence |
|---|---|
| Codex two-turn tool exchange | One cassette sequence: completed tool call + encrypted replay placeholder, then final text/usage; assert second request replay and stable prefix |
| OpenRouter two-turn tool exchange | One cassette sequence with structured reasoning details, tool arguments and final usage; prove both parser and subsequent serialization |
| Existing OpenAI-compatible behavior | Retain current streaming/buffered regressions; one representative HTTP playback, not every provider brand |
| Refused/malformed turn | Small table: HTTP 401/429/provider failure, HTTP-200 error event, truncated stream or invalid final tool arguments; typed errors and no extra request |
| Cache/config validation | Table: defaults, explicit legacy settings, each implemented native style, two simultaneous config errors, Codex TTL rejection and stale/absent anchor |
| Cancellation/trace boundary | One loopback stalled-body test that cancels without waiting for a socket timeout; one trace assertion for both dialects, missing usage and no credential sentinel |

Use one split-chunk parser regression and existing limit-at-edge/one-past tests rather than a combinatorial provider matrix. A cassette cannot prove cancellation timing or remote cache hits. No model quality goldens, snapshots of every trace, property tests for every enum, branch-coverage target or new CI job. The normal package/workspace gates remain authoritative; simplifying tests does not disable required gates.

## 8. Implementation sequence and ownership

One writer owns this coupled model/config/loop integration on a dedicated worktree. These are reviewable milestones, not authorization for parallel writers or a paid multi-agent pipeline. Fable reviews this plan before implementation; normal repository review applies to the later code.

### A. Establish the seam and replay proof

Files: model crate modules/tests and manifest; test-support adapters as needed.

- Land domain role/call/continuation/error types with real existing-backend consumers.
- Define the async trait/enum, shared reqwest helper and private serde wire types.
- Prove the dev-only cassette approach with one existing Codex-shaped exchange; keep the result or the simple transcript fallback, not both elaborate harnesses.
- Keep old clients working until their replacements are wired; transitional code is deleted in B, not a permanent compatibility framework.

**Exit:** scoped model tests compile; one complete request/replay/response path; no public unused cloud variants. **Stop:** if the async bridge requires changing the shell/broker contract, return with the concrete issue rather than silently expanding the milestone.

### B. Migrate existing clients and connect the loop

Files: `dekopon-model`, `dekopon-agent` callback/adapter seam, `dekopon-test-support`, gateway `session.rs` and cancellation wiring.

- Port Codex generation and OpenAI-compatible generation to the helper; preserve subscription auth and attachment/request behavior.
- Add the explicit blocking adapter and race-safe cancellation link. Keep the existing loop, tool limits, history semantics and terminal arbitration.
- Remove replaced generation ureq paths; keep auth's one implementation.
- Make 401 no-resend behavior explicit in tests and upgrading docs.

**Exit:** current backend replay/stream/buffered regressions pass; silent-body cancellation works; credentials never enter traces; clients remain shared. **Stop:** auth lock/refresh behavior changes beyond the scoped token snapshot/invalidation seam require a focused decision, not a duplicate implementation.

### C. OpenRouter, settings and interchangeable evaluation

Files: OpenRouter adapter; gateway config/factory/validation; example config and one small core-loop example.

- Reuse Chat framing/common wire pieces without pretending OpenRouter reasoning/routing is generic OpenAI behavior.
- Implement strict typed routing/cache settings, replay preservation, error mapping and usage.
- Add OpenRouter config, startup validation and two-turn cassette.
- Demonstrate selecting Codex versus an arbitrary OpenRouter model with no loop changes, subscriber owned by the example, and no broker effects.

**Exit:** same example runs offline against both dialects; model IDs are not catalog-whitelisted; advanced settings map exactly or fail; no response memoization. A live sanity check is optional and separately authorized, never a CI requirement.

### D. Observability, documentation and cleanup

Files: existing tracing call sites; `docs/inference.md`, `docs/dekopond.md`, `docs/observability.md`, `docs/upgrading.md`; crate README/examples; `[Unreleased]` changelog.

- Consolidate timing/usage/error attribution with the existing turn spans and accounting.
- Run scoped tests while iterating, then normal selected CI gates once on the assembled head. Run the existing OTLP smoke gate when the tracing changes require it; do not invent a second harness.
- Remove transitional clients/adapters with no remaining consumer. Document exactly which auth path remains blocking and why.
- Review the source-to-doc field mapping, all error exits, no retries, and primary gateway/broker dependency boundaries.

**Exit:** required CI passes, implementation docs describe reality, no hidden fallback, and an outside embedder can construct a client without gateway configuration or a global subscriber. Preserve normal artifact cleanup/ownership rules. No benchmark campaign is a completion gate.

## 9. Future extension recipe, not future scaffolding

A new provider should normally add: one concrete client implementing `InferenceModel`; private serde request/response types and reducer; one enum/factory arm; relevant config conversion; unified error/usage mapping; one representative two-turn cassette. Shared loop, HTTP observation and accounting should not change unless the provider exposes genuinely new semantics.

When actually implementing Bedrock or Vertex, add only the needed types then:

- Bedrock Converse has `cachePoint` unions and non-SSE AWS event streams; use a mature signing/event codec, not hand-rolled crypto. An SDK exception to the HTTP helper needs a reason and the same observation contract, not premature transport abstraction now.
- Vertex Gemini uses native parts/thought signatures; named content is prompt input with separate retained-resource authority, not a cache hint. Resource lifecycle remains deferred.
- Vertex Claude uses Anthropic Messages via its own publisher endpoint; not Gemini JSON. Different lifetime rules stay in provider-specific enums.
- Unknown provider fields belong in private lossless continuation where necessary, not in every public message or a universal map of settings.

Reuse outside Dekopon means keeping configuration discovery, routing, subscriber/exporter setup and tool execution out of `dekopon-model`. Existing small domain/asset dependencies are acceptable; extraction to a new package is an earned later decision, not a prerequisite. Provide rustdoc and one usable example, not a new SDK platform.

## 10. Definition of done and review boundary

The implementation succeeds when Codex and OpenRouter run the same core loop with shared async generation transport/tracing, typed settings/replay/errors, bounded streaming/cancellation and a few offline interaction proofs. Existing compatible clients, asset safety, tool authorization and Codex credential ownership must survive. Runtime provider incompatibility can fail clearly and be repaired outside the chat; undocumented behavior need not be solved in advance.

The review companion is [Fable review brief](multi-llm-fable-review.md). It asks whether this plan is implementable and appropriately small, not whether it pre-solves the entire cache catalog.

### Reference points

- [Current inference path](inference.md), [gateway config](dekopond.md), [observability](observability.md), [credential ownership](chatgpt-credential.md), [development gates](development.md).
- [Gateway unification PR #257](https://github.com/dekopon-agents/dekopon/pull/257).
- [OpenRouter prompt caching](https://openrouter.ai/docs/guides/best-practices/prompt-caching), [reasoning](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens), [routing](https://openrouter.ai/docs/guides/routing/provider-selection), [response caching](https://openrouter.ai/docs/guides/features/response-caching).
- [httpmock recording](https://httpmock.rs/record-and-playback/recording/) and [playback](https://httpmock.rs/record-and-playback/playback/): documentation evidence only, compatibility not yet tested.

Cloud/research findings inform extension points; they are not an implementation requirement to copy a large provider/model table into code. No benchmark, live call, cassette recording or compiled prototype was performed for this plan.
