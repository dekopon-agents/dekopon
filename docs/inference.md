# Model inference, prompt caching, and memory

This document follows a Slack message from `dekopond` into the selected async model adapter: what Dekopon caches, what it remembers, and what reaches the wire.

**Status: Current, except where marked Exploration.** Dekopon sends cache-affinity hints, preserves
append-only model turns, reports provider-declared cache usage, streams a turn's visible text to its
caller as it arrives and can stop a turn mid-answer, keeps a bounded conversation in
gateway memory, delivers bounded provider-produced attachments on opted-in routes, and optionally
stores and retrieves namespace-isolated durable chat turns through a JSONL provider. It does not
cache completed answers, manage provider cache resources, use provider-managed conversation
objects, generate images itself, retain attachment bytes in model messages, or automatically replay durable memory.

The ChatGPT subscription transport uses a fixed, undocumented ChatGPT/Codex backend rather than the
public OpenAI Platform API. Public OpenAI documentation is context, not a contract for that endpoint.

## Three different mechanisms

“Cache,” “conversation,” and “memory” are easy to collapse into one idea. They solve different problems.

| Mechanism | Owner | Purpose | Current Dekopon behavior |
|---|---|---|---|
| Prompt-prefix cache | Model provider | Avoid recomputing an identical leading prompt | Sends stable prefixes and dialect-specific hints; OpenRouter can mark an explicit system prefix; cannot manage provider entries |
| Conversation history | `dekopond` | Let a person—or an explicitly configured exact-conversation audience—ask a follow-up | Optional bounded `(question, final answer)` window in process memory, private per subject by default |
| Durable chat-turn memory | `dekopon-brokerd` provider storage | On-demand recent/literal search across restarts inside one attested scope | Optional JSONL turns + permanent finite dedup; no automatic replay, deletion/export, semantic index, or encryption-at-rest claim |

A cache hit never substitutes an old answer. The provider evaluates the complete request and produces a new response, so “fresh” refers to whether prefix computation can be reused, not to the answer or its underlying data.

## The inference path

A Slack message does not go straight to OpenAI:

```text
Slack event
  -> transport authenticates and normalizes the sender
  -> route selects one catalog agent and model
  -> broker returns a fresh subject-and-agent capability surface
       empty/refused -> fixed unauthorized reply; no model call
  -> conversation store optionally supplies compacted history + an opaque cache key
  -> prompt loop builds ModelMessage values and ModelTool definitions
  -> a per-session BlockingModel enters the supplied tokio runtime from spawn_blocking
  -> the shared ModelClient selects Codex, OpenRouter, or OpenAI-compatible encoding
  -> pooled async reqwest sends compact typed JSON with Content-Length
  -> SSE events stream visible text back to the caller and become AssistantTurn
       caller says stop -> the in-flight response is dropped, the turn is Cancelled
  -> tool call? append opaque replay items + tool output and call the model again
       typed output descriptors? files join the scoped table; only authorized asset.send queues delivery
  -> exact bounded text plus any accepted attachments receive complete Slack transport acceptance
     or an optional owned-thread continuation declines and sends no reply
  -> one fresh hidden record request only after an accepted reply and effective durable surface
  -> persistent route stores only the new question and final answer, or a declined user-only turn
```

The broker authorization leg is new for every Slack message. Neither remembered text nor a prompt cache key enters Cedar policy or grants a capability.

## Streaming and interruption

The prompt loop stays synchronous: `ChatModel::complete` takes a callback,
`&mut (dyn FnMut(TurnEvent) -> ControlFlow<()> + Send)`. `BlockingModel` implements it over
async `InferenceModel::generate`, using the caller-supplied tokio handle only on the blocking
session task. Events arrive in wire order. It sees two things and can see no others: `TurnEvent::TextDelta`, carrying
a fragment of the visible answer in a `ModelText`, and `TurnEvent::ToolCallStarted { index }`, a
counter. Reasoning, tool-call arguments, tool output, and attachment bytes have no variant to
travel in, and `ModelText` can be constructed from bytes only inside `dekopon-model`, so the rule
that only answer text reaches a chat surface is enforced by the types rather than by review.

One successful generation returns one complete `AssistantTurn`, whether or not anyone watches
progress. All streaming adapters share chunk-safe SSE framing with a 16-MiB ceiling; compatible
buffered responses have a 10-MiB ceiling. Partial tool arguments never become executable.

An observer `Break` or session watch cancellation returns `InferenceError::Cancelled` and drops
the in-flight response, never returning a shortened successful turn. The transport selects on
cancellation while sending and reading, including a silent reasoning phase. The synchronous
loop also checks its cancellation probe between events and before later work. Already displayed
text stays visible after a stop, but the incomplete turn does not enter successful history.

Each `complete` creates a fresh `TurnControl` with the session's watch receiver and a total
`timeoutMs` deadline. Preparation, credential work, send, read and Codex's one pre-body 401
refresh/resend share that deadline; it is not reset for the resend. Connect time is additionally
capped at ten seconds. A deadline is `InferenceError::DeadlineExceeded`, not a zero-token success.
Credential login/refresh remains blocking `ureq` on blocking tasks; generation uses async reqwest.
Dropping an HTTP future does not roll back remote work or preempt credential/file blocking IO.

Per backend:

- `chatgptSubscription` always streams. The Codex Responses endpoint is event-stream-only, the
  request has carried `"stream": true` since before any of this, and the configuration has no
  `stream` field to set — one that could be written but not honored would be worse than none.
- `openrouter` always streams at `https://openrouter.ai/api/v1/chat/completions`; there is no
  configurable endpoint or stream switch. It always sends `X-OpenRouter-Cache: false` and never
  sends `stream_options` or `usage.include`; the prompt cache key travels as `session_id`.
- `openaiCompatible` streams by default and takes `stream: false` for an endpoint that gets it
  wrong: a proxy that buffers the whole event stream, or a server that drops `usage` when asked to
  stream. With streaming on, the request adds `stream: true` and
  `stream_options: {"include_usage": true}` to request usage. With it off, neither field is sent — the endpoint receives the request it received before
  streaming existed — and the callback is never called.

"Compatible" is a claim rather than a specification, so the chat-completions accumulator tolerates
what those endpoints actually send: `delta.content` null, `usage` null on every chunk before the
last, a whole tool call delivered in one fragment (llama.cpp), every call of a parallel batch
reported at `index: 0` (Ollama), a chunk split across two `data:` lines, and a missing `[DONE]`
when a chunk already carried a finish reason. A stream that ends before either is a truncated turn
and fails naming that, rather than passing a half answer off as the whole one.

## What is optimized today

Five cache-friendly properties of the Codex path (OpenRouter's explicit policy is described below):

1. **One opaque key per useful reuse lane.** A persistent conversation gets one minted key. A one-shot route gets one key shared by that route's requests, where only the common agent prefix can match.
2. **Append-only turns inside one model session.** If the model calls a tool, the next request retains the earlier `input` items byte-for-byte and appends the reasoning replay, function call, and function result.
3. **Stable provider replay.** The subscription transport requests `reasoning.encrypted_content` and replays the opaque provider items on the next tool-loop turn instead of reconstructing them.
4. **Stable instructions and tools.** System messages are hoisted to `instructions`; tool definitions are built once for the session. Tests fail if appending a turn mutates either.
5. **Measured rather than assumed hits.** Usage is normalized without inventing missing counts or double-counting cached/reasoning tokens. OpenRouter also reports optional cache writes.

The source contracts are in:

- [`crates/dekopon-model/src/model.rs`](../crates/dekopon-model/src/model.rs) — `ModelMessage`, `ModelTool`, `CompletionOptions`, `AssistantTurn`, `ModelUsage`, `ChatModel`, and the portable audit projection;
- [`crates/dekopon-model/src/stream.rs`](../crates/dekopon-model/src/stream.rs) — `ModelText` and `TurnEvent`, the two types a turn reports itself with;
- [`crates/dekopon-model/src/sse.rs`](../crates/dekopon-model/src/sse.rs) — the one event-stream reader and its 16 MiB bound;
- [`crates/dekopon-model/src/chatgpt.rs`](../crates/dekopon-model/src/chatgpt.rs) — credential-file lifecycle;
- [`codex.rs`](../crates/dekopon-model/src/codex.rs), [`openai.rs`](../crates/dekopon-model/src/openai.rs), and [`openrouter.rs`](../crates/dekopon-model/src/openrouter.rs) — the three typed wire codecs and reducers;
- [`crates/dekopon-agent/src/prompt.rs`](../crates/dekopon-agent/src/prompt.rs) — the bounded model/tool loop;
- [`crates/dekopon-agent/src/prompt/history.rs`](../crates/dekopon-agent/src/prompt/history.rs) — compacted cross-message history; and
- [`crates/dekopond/src/cache_key.rs`](../crates/dekopond/src/cache_key.rs), [`conversation.rs`](../crates/dekopond/src/conversation.rs), and [`session.rs`](../crates/dekopond/src/session.rs) — key lifetime, history lifetime, and Slack-session assembly.

### What is not optimized or cached

- Codex and compatible requests do not set `prompt_cache_retention` or `prompt_cache_options`.
  OpenRouter alone supports the explicit system-prefix marker described below.
- Codex requests set `store: false` and use neither `previous_response_id` nor a provider conversation identifier.
- Completed answers are not memoized. Each incoming message makes a fresh model request after authorization.
- `dekopond` builds one model client per configured model on first use and shares it across every later message and session (`ModelCache` in [`crates/dekopond/src/session.rs`](../crates/dekopond/src/session.rs)); each session gets a fresh bridge and watch receiver, while the prompt cache key and `CompletionOptions` stay request-scoped. Sharing the client reuses TCP/TLS connections and the loaded credential; it does not make the remote prompt cache more durable.
- The gateway does not estimate tokens before a request. Its history bound is bytes plus whole turns because provider token counts arrive only after a billed call.
- Cross-message compaction preserves conversational meaning, not the full prior wire transcript. A follow-up can reuse a leading prefix, but it is not necessarily an append-only extension of the last tool-loop request.

Within one session, the second request is the first request plus more items. Between Slack messages, `History` reconstructs only the previous question and final answer; tool calls, tool outputs, and encrypted reasoning are gone. That keeps memory bounded and portable across model backends, and it can shorten the matching provider-cache prefix.

## OpenRouter controls and cache anchors

`generation`, `reasoning`, `routing` and `cache` are immutable client settings, not arbitrary JSON
or request-scoped overrides. The strict configuration and every accepted spelling/bound are in
[`dekopond.md`](dekopond.md#openrouter-model-settings). Omitted members remain absent on the wire:
`maxOutputTokens` maps to `max_tokens`, `topP` to `top_p`, reasoning effort to `reasoning.effort`,
and routing to snake-case members of `provider`. Forwarding a control does not prove that a remote
provider honored it; telemetry labels these as requested settings.

Automatic cache mode sends no marker. `explicitPrefix` marks the final content part of the last
leading system message, before the first non-system message. A string becomes a text-part array:

```json
{"type":"text","text":"stable instructions","cache_control":{"type":"ephemeral","ttl":"5m"}}
```

TTL is absent unless authored (`5m` or `1h`). This handles instructions, the skills listing and
optional-reply guidance without marking a user message. Zero leading system messages is a local
`InvalidRequest` before send. The marker is a hint, not a cache lease or a promise of credited
usage. `X-OpenRouter-Cache: false` disables response-cache reuse independently of prompt caching.

## Typed failures and finish reasons

`InferenceError` distinguishes local `InvalidRequest`, `Unsupported`, `Authentication` (401/403),
`RateLimited` (429), `Provider`, `Transport`, `Protocol`, `Attachment`, `Cancelled` and
`DeadlineExceeded`. Provider/transport contexts retain the phase, optional status/code/request ID,
integer-seconds Retry-After and a bounded sanitized diagnostic; safe typed sources are retained.
Other non-2xx statuses and HTTP-200 error frames are provider failures. Bad JSON, absent choices,
missing terminal events and incomplete tool arguments are protocol failures. An unexecutable tool
kind is unsupported. Redirects and automatic retries are off; only Codex's pre-body 401 has one
refresh/resend, within the original deadline.

For compatible and OpenRouter chat completions, `stop`, `length` or an extension finish reason
with complete text and no open call succeeds. `length` marks partial output in telemetry; it does
not silently discard usable text. Complete tool calls succeed; cut/non-object arguments never
execute. `content_filter` is a provider failure. Codex `response.incomplete` remains a failure.
Usage is retained through trailing usage-only chunks. Missing usage remains absent, and reasoning
counts are a subset of output, not an extra charge added to it.

## Prompt cache key lifecycle

A key is a routing hint, not a cache handle: `prompt_cache_key` on Codex and compatible requests,
`session_id` on OpenRouter. Dekopon cannot use it to read another response,
enumerate cache contents, or delete provider state, and it is minted from entropy rather than from a
subject, channel, phone number, or account ID. [`dekopond.md`](dekopond.md#the-prompt-cache-key) owns
its scope, minting, and rotation.

Sharing a one-shot route's key does not share answers: two requests reuse only their identical
prefix, and sender-specific text diverges where it differs. Once history is discarded the replacement
prompt shares no prefix with the one it replaced, which is why the key rotates with the conversation
it names.

### Getting the reuse available today

There is no cache-population call to make. The first eligible request warms whatever the provider supports; later requests either match or do not. For the best current odds:

- keep the model, agent instructions, the mounted-skills listing (skill names and descriptions, hoisted into `instructions` with the agent instructions), tool descriptions, schemas, and ordering stable;
- put changing information in the new user turn rather than in standing instructions;
- use `persistent` only when the product should remember the conversation, not merely to chase a discount;
- set a conversation window large enough that it does not rewrite the front on every follow-up — when `maxTurns` or `maxBytes` drops the oldest exchange, the front of the request is rewritten and the cached prefix ends at the first changed token, so a generous window that trims rarely beats a tight one that trims constantly;
- avoid sending an attachment again when only its compact reference is needed; and
- measure reported cached input on real second and later turns.

A short, stable prefix can sit below the provider's eligibility threshold, and a long, identical one can be evicted or routed elsewhere. Dekopon preserves reuse opportunities and reports outcomes; it does not turn a provider optimization into a correctness dependency.

### Why completed responses are not cached

A local response cache would be a separate feature with different safety rules. A correct key would need at least the backend, exact model, instructions, tools, full messages, attachments, and generation settings. A tool-enabled answer can also depend on fresh broker authorization and external data, and a prior turn may have caused an effect. Returning an old final answer could hide a revocation, report stale provider state, or make a caller believe an effect just ran when no proposal was submitted.

Dekopon makes the model call and reauthorizes the effects every time. Nothing about a response cache may be inferred from `prompt_cache_key`.

## Provider retention: what can be said

### ChatGPT subscription endpoint

Dekopon posts subscription inference to:

```text
https://chatgpt.com/backend-api/codex/responses
```

OpenAI's public documentation does not name that endpoint or publish its cache eligibility, retention, eviction, pricing, routing, or parameter-support contract. It may behave like the public Responses API in some respects; a successful request or observed cache count is evidence for that request, not a promise for the next one.

> The ChatGPT subscription cache lifetime is undocumented. Treat every request as able to miss, and use `usage.cached_input_tokens` to measure observed reuse.

A missing cached-token field means **unreported**, not zero. A reported zero means that call received no credited cached input; it does not reveal why.

### Public OpenAI API context

The following is context from OpenAI's public [Prompt Caching guide](https://developers.openai.com/api/docs/guides/prompt-caching), read on **2026-08-20**. It applies only to the documented public API and supported models.

- Caching is automatic for eligible prompts and depends on an exact matching prefix.
- Eligibility starts at a model-dependent minimum: 1,024 tokens for GPT-5.6 and later, and 1,024–2,048 for earlier models. Earlier-model cache hits are reported in 128-token increments.
- Public in-memory retention for supported earlier models is generally 5–10 minutes of inactivity, with a maximum of one hour.
- Supported earlier models may offer extended retention up to 24 hours through `prompt_cache_retention`; 24 hours is a maximum, not a guaranteed hit.
- GPT-5.6 and later use cache breakpoints and document a 30-minute TTL that refreshes on reuse through `prompt_cache_options.ttl`.
- Model support, defaults, write pricing, retention controls, and zero-data-retention interactions are version-sensitive.

Dekopon sends none of those OpenAI retention or breakpoint controls. Even if its configured model has the same name as a public API model, the ChatGPT subscription endpoint and account policy are different surfaces. Do not copy a public API TTL into an availability or cost forecast for the subscription.

## Can a long-lived agent keep a cache warm?

Not by staying alive.

A local object has no lease on provider memory. The official public API model is request-driven: matching requests are routed toward cached prefixes, and the provider controls retention and eviction. There is no documented mechanism where any of these pins the cache:

- a running `dekopond` process;
- a live `CodexClient`, `OpenRouterClient`, or pooled `reqwest::Client`;
- an HTTP keep-alive connection;
- an OAuth access token or ChatGPT login;
- a response ID or provider conversation object; or
- a Dekopon conversation entry that receives no model calls.

Synthetic keep-alive prompts would consume quota, create more retained input, and buy no subscription-endpoint guarantee. Dekopon does not send them.

One long-lived optimization is in place: `dekopond` shares one model client per configured model across gateway messages, reusing connections and the loaded credential, with refreshes coordinated through the client's credential mutex and the cross-process advisory lock beside the auth file. `CompletionOptions` stays request-scoped so a shared client cannot apply one conversation's key to another.

## How scoped conversation memory works

A route opts in with a `memory:` block, and [`dekopond.md`](dekopond.md#conversations) owns its
keys, bounds, and eviction. What matters at the wire is what enters the prompt.

`oneShot` is the route default and sends no history at all. A persistent route seeds the prompt with
compacted `(question, final answer)` pairs ahead of the new message, oldest dropped first until both
the turn and byte bounds hold. A shared turn is prefixed with
`[gateway: authenticated participant: <canonical-subject>]` before it is sent and retained, so that
canonical ID is model input whatever the telemetry gate says; private and one-shot prompt bytes carry
no prefix and go out unchanged.

Every message opens a fresh attested broker leg before inference. An empty grant stops before the
model call and removes remembered state for that key; a broker or attestation failure stops before
inference with no fresh grant vector to replace state with. A capability set that differs from the one
stored beside the conversation drops the entry and closes its attachment generation, and on a shared
route participants with different grant vectors reset the window between them. The new exchange is
appended only if its generation remains current.

### What history drops

- model reasoning, including encrypted replay items;
- function/tool calls;
- scripts and capability outputs;
- system instructions, which are supplied fresh from the catalog;
- the gateway's fixed failure sentence; and
- any synthetic assistant text for a no-reply decision — there is none, so a declined turn keeps the
  user message alone.

This is conversation continuity, not evidence continuity. The broker audit is where authorized
effects remain verifiable.

### Memory never becomes authority

History is untrusted prompt text. It is not sent to the broker as policy input. The prompt cache key also stays out of authorization. Every capability invocation becomes a fresh proposal that only the broker may authorize.

Grant-set invalidation has one known limit: it compares capability identifiers. Tightening a capability's execution constraints or changing its credential while retaining the same identifier does not invalidate history. [`security-model.md`](security-model.md#conversation-memory-as-a-trust-surface) records that live limitation.

## Outbound attachments are not inference

Producing an image is a provider effect, not a model call. The gateway holds no image credential, and
the OpenAI-compatible, OpenRouter and Codex adapters are not image generators; they remain
orchestrators.
What reaches this document's path is the *delivery* half: a capability the broker authorized returns
bytes, and the gateway carries them to chat without letting them through the model.

Provider assets are typed descriptor outputs, admitted only on successful invocation. The gateway
numbers them in its scoped table and returns a bounded metadata note to the model. Attaching does
not deliver: `asset.send` is a separately authorized external write, queued for the reply under a
four-send turn allowance. A failed turn sends none; a failed delivery yields a bounded notice in the
next turn without retrying. Persistent sent flags make later duplicate sends no-ops.

Every exact proposal reference is discovered without expansion, pinned and passed read-only beside
unchanged JSON. Model-facing `fetch_chat_asset` retains its own limits and weak references; encoded
storage is decoded by the native codec on consumption. Model wire serialization is unchanged here;
streaming model request bodies is a separate change, not claimed by descriptor integration.

For adapter types, storage ceilings and transport paths, see
[asset handles and delivery](dekopond.md#asset-handles-and-delivery). Memory retains text and scoped
reference notes, never provider payload bytes. A partial native delivery suppresses durable recording.


## Optional durable chat-turn retrieval

The independently released `memory-chat` component imports JSONL only and stores versioned
`turns.jsonl` and `dedup.jsonl` inside an opaque broker-derived namespace. Scope always includes
provider, agent, canonical sender, configured transport, channel, and conversation. `authority-bound`
(default) rotates a persisted random epoch when effective capability metadata, constraints, selected
symbolic credential, provider artifact bytes, host/storage ceilings, backend, or memory limits change;
A→B→A never reopens A. Explicit `stable` preserves continuity across those changes while every read
and write is freshly authorized.

The model sees only:

```text
memory recent --last N
memory search --query TEXT
```

Recent returns whole chronological turns. Search examines the bounded newest lookback with Unicode
lowercase plus literal substring matching and returns whole turns chronologically. Compaction has a
lower target and higher threshold for hysteresis; dedup records are never compacted. The same ID and
content succeeds without mutation, a changed commitment is `dedup-conflict`, malformed complete
records are `memory-corrupt`, and finite dedup exhaustion is `dedup-capacity` while reads continue.

Parsing, search, and compaction run inside provider Wasm; the broker owns only opaque namespace-bound
files, quotas, and commit. Conversation content therefore lives under the privileged broker's storage
root and never in its audit, spans, metrics, public errors, or provider metadata.
[`dekopond.md`](dekopond.md#durable-memory-after-transport-acceptance) owns when a turn is recorded.
Retrieval is explicit: a durable turn never enters a later prompt on its own, and what comes back is
untrusted model context.

## Beyond the chat-turn window

**Status: Exploration.** Durable on-demand chat turns are the accepted design. Editable facts, tasks,
semantic or vector retrieval, cross-agent sharing, deletion and export UX, automatic prompt insertion,
and an explicitly documented retention mode on a public API backend are none of them designed or
committed. A proposal answers first: who owns the memory and authenticates each read and write; what
it is scoped to; retention, export, deletion, and incident response; prompt-injection persistence and
cross-sender retrieval; invalidation beyond the capability-identifier comparison described above;
whether a model may propose a write and what trusted component validates it; and how retrieved memory
is proven never to become identity or authorization input.

## Literal Rust walkthrough

The compiled, runtime-owning example is
[`compare_models.rs`](../crates/dekopon-agent/examples/compare_models.rs). It uses only public APIs:
`--kind codex|openrouter --model <id>`, optional `--label <experiment>`, `--prompt <text>`,
`--loopback <ip:port>` and `--auth-file <path>`. OpenRouter reads `OPENROUTER_API_KEY`, or uses a
synthetic key under `--loopback`; Codex uses
`--auth-file` when supplied, otherwise its existing credential file (`DEKOPON_CHATGPT_AUTH_FILE`
or the default). Codex with `--loopback` requires an explicit `--auth-file` pointing to a fresh
synthetic credential, so it never implicitly loads or rotates the operator's credential.
It runs `run_prompt_session` on `spawn_blocking`
with a `BlockingModel` and a local `ScriptRuntime` that returns a fixed synthetic result. It never
executes the proposed script or opens a broker leg. No gateway configuration or global tracing
subscriber is required. Without `--loopback`, running it makes a real, potentially billed request:
that requires explicit operator authorization, and is not part of validation.

Compile without making an inference call:

```sh
cargo test -p dekopon-agent --locked --no-run --example compare_models
cargo test -p dekopon-agent --locked --test compare_models
```

The second command is the offline proof: `LoopbackServer::sequence` supplies synthetic tool and
answer streams for both adapters, and the same prompt loop executes one synthetic runtime call
and sends its result back with native continuation state. No production endpoint is contacted.
Codex's fixture file contains fake `access`, `refresh`, `accountId`, `version: 1`, and an
`expiresAt` one hour ahead. The loopback override never refreshes credentials: a `401` or a token
inside its refresh margin is an `Authentication` error. `localhost`, HTTPS,
remote addresses, userinfo and URL fragments are refused; only literal `http://127.0.0.1` or
`http://[::1]` are accepted. Changing the destination invalidates old native continuations.

### Direct async generation

An embedder can also use `InferenceModel` directly. Construct `CodexClient` on a blocking thread
(credential-file IO), or `OpenRouterClient` with its caller-supplied key and immutable `Settings`,
and wrap it in `ModelClient`. The following function runs on the embedder's existing tokio runtime;
it supplies a fresh deadline for each call and never constructs provider wire JSON or native state:

```rust
use dekopon_model::{
    control::TurnControl,
    error::InferenceError,
    inference::{GenerateRequest, InferenceModel, ModelClient},
    model::{AssistantTurn, CompletionOptions, ModelMessage, ModelTool, assistant_message},
};
use std::{ops::ControlFlow, time::Duration};

async fn two_turns(
    client: &ModelClient,
    signal: tokio::sync::watch::Receiver<bool>,
) -> Result<AssistantTurn, InferenceError> {
    let mut messages = vec![
        ModelMessage::system("Use the synthetic tool, then summarize its result."),
        ModelMessage::user("Say hello."),
    ];
    let tools = vec![ModelTool {
        name: "bash".into(),
        description: "Return a synthetic result; no script executes.".into(),
        parameters: serde_json::json!({
            "type":"object", "properties":{"script":{"type":"string"}},
            "required":["script"], "additionalProperties":false
        }),
    }];
    let options = CompletionOptions::default();
    let mut observe = |_| ControlFlow::Continue(());
    let first = client.generate(
        GenerateRequest { messages: &messages, tools: &tools, options: &options },
        &mut observe,
        &TurnControl::new(signal.clone(), Duration::from_secs(120))?,
    ).await?;
    if first.tool_calls.is_empty() { return Ok(first); }
    messages.push(assistant_message(&first));
    for call in &first.tool_calls {
        messages.push(ModelMessage::tool(call.id.clone(), "synthetic-result"));
    }
    client.generate(
        GenerateRequest { messages: &messages, tools: &tools, options: &options },
        &mut observe,
        &TurnControl::new(signal, Duration::from_secs(120))?,
    ).await
}
```

This deliberately does not execute tools. Production effects still require the broker-authorized
runtime. `AssistantTurn::new(content, calls, usage)` constructs a portable turn for doubles;
applications cannot manufacture or edit the private native continuation. `assistant_message`
retains the successful turn's continuation. Its Debug/audit projection omits reasoning and secrets.

### What the second request retains

For Codex, the first request has `model`, `store: false`, `stream: true`, hoisted `instructions`,
`input`, function `tools`, `tool_choice: "auto"`, `parallel_tool_calls: true`,
`include: ["reasoning.encrypted_content"]`, and `text: {"verbosity":"low"}`; the optional
`prompt_cache_key` is the request's affinity hint. Authorization, account ID, `originator: dekopon`,
versioned user-agent, `openai-beta: responses=experimental` and SSE accept headers stay private to
this adapter. The compact body has Content-Length. A completed tool turn appends the native
reasoning and function items, followed by a `function_call_output` bearing the typed call ID and
synthetic result. Instructions, tools and earlier input remain stable.

For OpenRouter, the request has `model`, `messages`, `tools`, `tool_choice: "auto"` and
`stream: true`, plus only authored controls and `session_id` (the prompt cache key, for sticky
provider routing). It has no `parallel_tool_calls`, `stream_options`, `prompt_cache_key` or
response-cache reuse. Its next assistant message contains `content` (text or null), native
`tool_calls`, and merged `reasoning_details` when nonempty. Indexed reasoning groups retain
first-seen order, concatenate `text`/`summary`/`data` in arrival order, and keep the first non-null
other fields, including unknown ones. Unindexed items remain separate. The next tool message
carries `tool_call_id` and the synthetic result. The continuation is bound to the selected client;
`reasoning_details` are forwarded unchanged even if the upstream provider changed, so pin
`routing.only` when cost or cache locality matters.

Across separate chat messages, `History` stores only the question and final answer (or an
unanswered user turn). Native reasoning, calls and tool outputs do not cross that compacted
boundary. Switching configured clients starts a fresh tool loop; portable history can still move
between dialects. Equal visible text is not proof of equal native replay or a cache hit.

## How to evaluate caching in a deployment

1. Use a real second or later model turn; the first eligible request normally has nothing earlier to hit.
2. Query `usage.input_tokens` and `usage.cached_input_tokens` on `prompt.model_turn` or `accounting.model.turn`.
3. Treat a missing cached field as unreported. Do not coerce it to zero.
4. Compare the ratio only across calls whose provider reported both values.
5. Check whether instructions, tools, model, attachment parts, or the front of history changed.
6. Check whether a history trim, idle eviction, capacity eviction, grant change, or process restart rotated or rewrote the lane.
7. Remember the public API eligibility minimum: a short common prefix can be perfectly stable and too small to cache. The subscription endpoint's threshold is undocumented.

The useful metric is observed reuse, not the existence of a key:

```text
cache ratio = sum(cached_input_tokens) / sum(input_tokens)
```

Compute it only over calls where both fields were reported. A key proves Dekopon asked for cache affinity. It never proves the provider supplied it.

## Related documents

- [`dekopond.md`](dekopond.md) — routing, persistent-conversation bounds, cache-key scope and rotation, generated images, durable recording, and telemetry.
- [`security-model.md`](security-model.md#conversation-memory-as-a-trust-surface) — retained text and prompt-injection dwell time.
- [`cli.md`](cli.md) — isolated model-account login.
- [`chatgpt-credential.md`](chatgpt-credential.md) — rotating subscription credential lifecycle.
- [`observability.md`](observability.md) — model usage fields and payload gating.
- [`design.md`](design.md#non-goals) — the project's non-goals, which reject a memory feature argued on durability grounds.
- [OpenAI Prompt Caching](https://developers.openai.com/api/docs/guides/prompt-caching) — public API behavior, not a subscription-endpoint guarantee.
- [OpenAI conversation state](https://developers.openai.com/api/docs/guides/conversation-state) — public Responses state patterns Dekopon does not use.
