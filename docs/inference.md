# Model inference, prompt caching, and memory

This document follows a Slack message from `dekopond` into the ChatGPT subscription transport. It explains what Dekopon caches, what it remembers, and what reaches the wire.

**Status: Current, except where marked Exploration.** Dekopon sends cache-affinity hints,
preserves append-only model turns, reports provider-declared cache usage, can keep a bounded
conversation in gateway memory, can explicitly generate one bounded outbound image, and optionally
stores/retrieves namespace-isolated durable chat turns through a generated JSONL provider. It does
not cache completed answers, request extended provider retention, use provider-managed conversation
objects, retain generated image bytes, or automatically replay durable memory.

The ChatGPT subscription transport uses a fixed, undocumented ChatGPT/Codex backend rather than the public OpenAI Platform API. Public OpenAI documentation is useful context, but it is not a contract for that endpoint. This distinction is load-bearing throughout this document.

## Answers at a glance

| Question | Current answer |
|---|---|
| Is Dekopon's ChatGPT path optimized for caching? | **Yes, deliberately, but only as a best-effort optimization.** Requests carry `prompt_cache_key`; tool-loop turns grow by appending stable replay items; system instructions and tool definitions stay stable; and tests pin that shape. Only provider-reported `cached_input_tokens` proves a hit. |
| How do we cache calls? | Dekopon relies on the provider's prompt-prefix cache. It sends the complete request every time. It does not memoize model answers or tool effects locally. |
| How long is the cache fresh on a ChatGPT subscription? | **OpenAI does not publish a retention contract for the subscription endpoint Dekopon calls.** Public API policies range from short in-memory retention to model-specific extended retention, but those values cannot be promised here. |
| Can a long-lived agent keep the cache alive? | Keeping a Rust object, process, HTTP connection, response ID, or conversation entry alive does not documentably pin a provider cache. Only provider-side reuse policy and actual matching requests matter. `dekopond` already shares one client per configured model, which reuses connections and coordinates credential refresh; that is a transport optimization, not a cache lease. |
| How does outbound image generation work? | A route may explicitly name a separate public OpenAI Images backend. Its chat model can call `generate_image` once; one bounded PNG is carried outside the transcript to the authenticated Slack/Discord/Telegram/local reply target. Existing Chat Completions and private ChatGPT subscription contracts are not claimed to generate images themselves. |
| How does chat memory work? | `oneShot` routes remember nothing. `persistent` routes keep bounded execution-aware job records per sender through `dekopon-harness` in gateway memory, bounded by idle time, turns, bytes, and total conversation count. Every message is authorized afresh. |
| Does Dekopon have a memory framework? | **No general framework.** It has a focused conversation window plus optional durable on-demand recent/literal-search chat turns—not task, semantic, vector, editable-fact, or automatically replayed memory. |

## Three different mechanisms

“Cache,” “conversation,” and “memory” are easy to collapse into one idea. They solve different problems.

| Mechanism | Owner | Purpose | Current Dekopon behavior |
|---|---|---|---|
| Prompt-prefix cache | Model provider | Avoid recomputing an identical leading prompt | Sends a stable key and stable prefixes; cannot inspect, create, refresh, or delete provider entries |
| Conversation history | `dekopon-harness`, in the gateway process | Follow-up context and portable execution observations | Bounded job records, tool groups and separately accepted text; not durable audit |
| Durable chat-turn memory | `dekopon-brokerd` provider storage | On-demand recent/literal search across restarts inside one attested scope | Optional JSONL turns + permanent finite dedup; no automatic replay, deletion/export, semantic index, or encryption-at-rest claim |

A cache hit never substitutes an old answer. The provider still evaluates the complete current request and produces a new response. “Fresh” therefore refers to whether prefix computation can be reused, not whether the answer or its underlying data is fresh.

## The inference path

A Slack message does not go straight to OpenAI:

```text
Slack event
  -> transport authenticates and normalizes the sender
  -> route selects one catalog agent and model
  -> broker returns a fresh subject-and-agent capability surface
       empty/refused -> fixed unauthorized reply; no model call
  -> conversation store optionally supplies bounded portable history + an opaque cache key
  -> prompt loop builds ModelMessage values and ModelTool definitions
  -> ChatGptCodexModel builds a Responses-shaped serde_json::Value
  -> POST https://chatgpt.com/backend-api/codex/responses
  -> SSE events become AssistantTurn
  -> tool call? append opaque replay items + tool output and call the model again
       `generate_image`? one fixed-endpoint request, one PNG leaves through a byte-free output slot
  -> exact bounded text plus optional PNG receives complete Slack transport acceptance
     or an optional owned-thread continuation declines and sends no reply
  -> one fresh hidden record request only after an accepted reply and effective durable surface
  -> harness retains bounded job/execution records and separate delivery disposition
```

The broker authorization leg is new for every Slack message. Neither remembered text nor a prompt cache key enters Cedar policy or grants a capability.

## What is optimized today

The current implementation has five intentional cache-friendly properties:

1. **One opaque key per useful reuse lane.** A persistent conversation gets one minted key. A one-shot route gets one key shared by that route's requests, where only the common agent prefix can match.
2. **Append-only turns inside a compatible, untrimmed model segment.** If the model calls a tool, the next request retains the earlier `input` items byte-for-byte and appends the reasoning replay, function call, and function result.
3. **Stable provider replay.** The subscription transport requests `reasoning.encrypted_content` and replays the opaque provider items on the next tool-loop turn instead of reconstructing them.
4. **Stable instructions and tools.** System messages are hoisted to `instructions`; tool definitions stay stable until a configured switch rebuilds the segment. Tests fail if appending a turn mutates either.
5. **Measured rather than assumed hits.** Responses usage is normalized into `ModelUsage::cached_input_tokens` and exported on the `accounting.model.call` record and matching span.

The source contracts are in:

- [`crates/dekopon-model/src/model.rs`](../crates/dekopon-model/src/model.rs) — `ModelMessage`, `ModelTool`, `CompletionOptions`, `AssistantTurn`, `ModelUsage`, and `ChatModel`;
- [`crates/dekopon-model/src/chatgpt.rs`](../crates/dekopon-model/src/chatgpt.rs) — the subscription request builder, SSE parser, and prefix-stability tests;
- [`crates/dekopon-harness/src/session.rs`](../crates/dekopon-harness/src/session.rs) — the bounded model/tool loop;
- [`crates/dekopon-harness/src/history.rs`](../crates/dekopon-harness/src/history.rs) — compacted cross-message history; and
- [`crates/dekopond/src/cache_key.rs`](../crates/dekopond/src/cache_key.rs), [`harness conversation.rs`](../crates/dekopon-harness/src/conversation.rs), and [`session.rs`](../crates/dekopond/src/session.rs) — key lifetime, history lifetime, and Slack-session assembly.

### What is not optimized or cached

- Requests do not set `prompt_cache_retention`, `prompt_cache_options`, or explicit cache breakpoints.
- Requests set `store: false` and use neither `previous_response_id` nor a provider conversation identifier.
- Completed answers are not memoized. Each incoming message makes a fresh model request after authorization.
- `dekopond` builds one model client per configured model on first use and shares it across every later message and session (`ModelCache` in [`crates/dekopond/src/session.rs`](../crates/dekopond/src/session.rs)); the prompt cache key and `CompletionOptions` stay request-scoped. Sharing the client reuses TCP/TLS connections and the loaded credential; it does not make the remote prompt cache more durable.
- The gateway does not estimate tokens before a request. Its history bound is bytes plus whole turns because provider token counts arrive only after a billed call.
- Cross-message compaction preserves conversational meaning, not the full prior wire transcript. A follow-up can reuse a leading prefix, but it is not necessarily an append-only extension of the last tool-loop request.

That last distinction matters. Within a compatible, untrimmed segment, requests extend prior
provider items. Across messages or switches, the harness selects bounded portable tool groups and
execution observations instead. Opaque reasoning/continuation is absent. Context trimming and
switching can shorten a matching cache prefix; no cache hit is guaranteed.

## Configured transitions (Unreleased)

`CompletionOptions::with_effort(Effort)` distinguishes `providerDefault` from `low`, `medium`
and `high`. Chat Completions encodes explicit settings as `reasoning_effort`; Codex Responses
encodes `reasoning.effort`. Default omits the setting. Unaware adapters refuse explicit effort
before I/O rather than silently dropping it; a backend's rejection is a failed inference, not
permission to silently choose another setting or model. Loopback wire tests cover all four states.

The gateway's opt-in [route controls](dekopond.md#configured-model-and-effort-controls-unreleased)
select configured cached clients through `dekopon-harness::control::ModelRegistry`, not arbitrary
model endpoints. Only a live `VerifiedControlDecision` from the server-UID-verified broker client
can admit application. The harness never deserializes admission from a provider or checkpoint.
Each request must be its own tool turn, with a job-wide maximum of four attempts including local
refusals. Selecting the current model/effort is a refused no-op, not a new segment.

An applied switch—including effort-only changes—invalidates all opaque replay and rotates the
cache key before further inference. Portable whole call/result groups and independently observed
execution evidence survive, bounded separately from model context. System/bootstrap identity,
tool definitions and history are rebuilt for the new selection; inspection/skill repeat pointers
are reset, but consumed budgets and image-generation attempts are never reset. Cross-provider
transitions use reconstructed portable tool correlations rather than another provider's encrypted
reasoning. No transport optimization, persistent provider conversation, or cache-retention claim
is introduced. Direct/replay runners omit controls even when they have a provider broker leg.

`SessionState.transitions` retains typed immutable from/to metadata, requesting model-call index,
charged attempt, decision reference and application/refusal outcome. It contains no guessed token
or dollar totals; the strict per-job accounting tracker owns accounting across these boundaries.
Checkpoint receipts remain process-local bounded storage receipts, not crash-durability guarantees.

## Prompt cache key lifecycle

A key is a routing hint, not a cache handle. Dekopon cannot use it to read another response, enumerate cache contents, or delete provider state.

| Route mode | Key scope | Rotation |
|---|---|---|
| `persistent` | One `(transport, conversation identity, sender)` conversation | Idle, capacity, changed capability grant, or process restart |
| `oneShot` | One bound route, shared by its senders | Process restart |

The key is minted from entropy. It is not a subject, channel, phone number, account ID, or hash of one. Sharing a one-shot route key does not share answers: requests can reuse only their identical prefix, and sender-specific text diverges where it differs.

Rotation serves privacy and correctness. Once history is discarded, the replacement prompt no longer shares the old conversation's prefix, so retaining the old routing key would create linkability without a useful cache hit.

### Getting the reuse available today

There is no cache-population call to make. The first eligible request warms whatever the provider supports; later requests either match or do not. For the best current odds:

- keep the model, agent instructions, the mounted-skills listing (skill names and descriptions, hoisted into `instructions` with the agent instructions), tool descriptions, schemas, and ordering stable;
- put changing information in the new user turn rather than in standing instructions;
- use `persistent` only when the product should remember the conversation, not merely to chase a discount;
- set a conversation window large enough that it does not rewrite the front on every follow-up;
- avoid sending an attachment again when only its compact reference is needed; and
- measure reported cached input on real second and later turns.

A short, stable prefix may still be below the provider's eligibility threshold. A long, identical prefix may still be evicted or routed elsewhere. Dekopon's responsibility is to preserve reuse opportunities and report outcomes, not to turn a provider optimization into a correctness dependency.

### Why completed responses are not cached

A local response cache would be a separate feature with different safety rules. A correct key would need at least the backend, exact model, instructions, tools, full messages, attachments, and generation settings. A tool-enabled answer can also depend on fresh broker authorization and external data, and a prior turn may have caused an effect. Returning an old final answer could hide a revocation, report stale provider state, or make a caller believe an effect just ran when no proposal was submitted.

Dekopon therefore makes the model call and reauthorizes effects. Any future response cache should begin with a narrowly defined, effect-free inference class plus explicit freshness, invalidation, privacy, and accounting semantics; it must not be inferred from `prompt_cache_key`.

## Provider retention: what can be said

### ChatGPT subscription endpoint

Dekopon posts subscription inference to:

```text
https://chatgpt.com/backend-api/codex/responses
```

OpenAI's public documentation does not name that endpoint or publish its cache eligibility, retention, eviction, pricing, routing, or parameter-support contract. It may currently behave like the public Responses API in some respects; a successful request or observed cache count is evidence for that request, not a promise for the next one.

Therefore the honest operational answer is:

> The ChatGPT subscription cache lifetime is undocumented. Treat every request as able to miss, and use `usage.cached_input_tokens` to measure observed reuse.

A missing cached-token field means **unreported**, not zero. A reported zero means that call received no credited cached input; it does not reveal why.

### Public OpenAI API context

The following is context from OpenAI's public [Prompt Caching guide](https://developers.openai.com/api/docs/guides/prompt-caching), read on **2026-08-20**. It applies only to the documented public API and supported models.

- Caching is automatic for eligible prompts and depends on an exact matching prefix.
- Eligibility starts at a model-dependent minimum. The current guide says 1,024 tokens for GPT-5.6 and later, and 1,024–2,048 for earlier models. Earlier-model cache hits are reported in 128-token increments.
- Public in-memory retention for supported earlier models is generally 5–10 minutes of inactivity, with a maximum of one hour.
- Supported earlier models may offer extended retention up to 24 hours through `prompt_cache_retention`; 24 hours is a maximum, not a guaranteed hit.
- GPT-5.6 and later use cache breakpoints and currently document a 30-minute TTL that refreshes on reuse through `prompt_cache_options.ttl`.
- Model support, defaults, write pricing, retention controls, and zero-data-retention interactions are version-sensitive.

Dekopon currently sends none of those retention or breakpoint controls. Even if its configured model has the same name as a public API model, the ChatGPT subscription endpoint and account policy are different surfaces. Do not copy a public API TTL into an availability or cost forecast for the subscription.

## Can a long-lived agent keep a cache warm?

Not by staying alive.

A local object has no lease on provider memory. The official public API model is request-driven: matching requests are routed toward cached prefixes, and the provider controls retention and eviction. There is no documented mechanism where any of these pins the cache:

- a running `dekopond` process;
- a live `ChatGptCodexModel` or `ureq::Agent`;
- an HTTP keep-alive connection;
- an OAuth access token or ChatGPT login;
- a response ID or provider conversation object; or
- a Dekopon conversation entry that receives no model calls.

Sending synthetic keep-alive prompts would consume quota, create more retained input, and still provide no subscription-endpoint guarantee. Dekopon should not do that.

One long-lived optimization is already in place: `dekopond` shares one model client per configured model across gateway messages, reusing connections and the loaded credential, with refreshes coordinated through the client's credential mutex and the cross-process advisory lock beside the auth file. `CompletionOptions` stays request-scoped so a shared client cannot apply one conversation's key to another.

The remaining candidate is an explicitly documented retention mode on a public API backend. It would require a modeled configuration field, supported-model validation, data-retention review, wire tests, and accounting, and it would not establish support on the ChatGPT subscription backend. That one is a future implementation choice, not current behavior.

## How Slack conversation memory works

A route opts into memory explicitly:

```yaml
conversation:
  mode: persistent
  idleTimeoutMs: 900000
  maxTurns: 12
  maxBytes: 65536
```

`oneShot` remains the default.

A persistent conversation is keyed by agent/route/transport/channel/conversation/sender. Fresh
broker admission precedes a harness generation lease; full metadata plus startup epoch invalidate
old history. Entries and total bytes are bounded, and idle/LRU eviction fences late appends. Jobs
retain whole tool groups and independently observed execution outcomes even after inference fails
or Stop wins. Generated text is distinct from exact accepted text. Reasoning, binary assets and
provider continuation are excluded; selected model context has independent bounds.

History is untrusted prompt text, never policy input. The broker still authorizes each invocation.
Unknown effects fence further work. Memory checkpoints are supplied process-local storage, not
crash durability or broker audit. See [harness.md](harness.md) for exact bounds, retention trade-offs
and remaining recording and validation limitations.

## Outbound image generation

A route can set `imageGenerator: true` against the gateway's single `imageGenerator:` block. That
explicit opt-in adds a gateway-owned `generate_image` meta tool to the existing chat model; no
opt-in means no tool, no image credential read, and byte-identical text-only replies. The generator is a separate model client so
the existing OpenAI-compatible Chat Completions and undocumented ChatGPT/Codex subscription
endpoints remain only the orchestrators they already are. Dekopon does not claim either contract
natively emits generated images.

One valid tool call supplies one non-empty prompt of at most 4 KiB. The fixed public OpenAI Images
client asks the configured GPT Image model for one 1024×1024 PNG, bounds the encoded response,
decodes at most 8 MiB, validates the PNG signature, and gives the bytes to a request-local output
slot. A second call is refused even when the first failed, because a failed request may still have
incurred cost. The model reads only a fixed success/failure sentence and then produces the textual
caption; generated bytes never become a `ModelMessage`, tool result, prompt transcript, or
`SessionExit`.

The gateway owns the filename/media type and sends the image only to the reply coordinates from the
authenticated inbound envelope. Slack uses the external file-upload sequence, Discord a multipart
attachment, Telegram `sendPhoto`, and the local socket an omitted-when-empty base64 `images` field.
A receipt means the complete text/image reply was accepted; a non-atomic later failure is partial
delivery and suppresses durable recording. Persistent and durable memory keep only final text, not
the PNG or the generation prompt, so a follow-up can discuss the caption but cannot edit prior
pixels without generating a new image.

## Optional durable chat-turn retrieval

The independently released `memory-chat` component imports JSONL only and stores versioned `turns.jsonl` and
`dedup.jsonl` inside an opaque broker-derived namespace. Scope always includes provider, agent,
canonical sender, configured transport, channel, and conversation. `authority-bound` (default)
rotates a persisted random epoch when effective capability metadata, constraints, selected symbolic
credential, provider artifact bytes, host/storage ceilings, backend, or memory limits change;
A→B→A never reopens A. Explicit `stable` preserves continuity across those changes while every read
and write is still freshly authorized.

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

Recording occurs only after fresh authorization, model success, and complete service/kernel
transport acceptance. It is gateway-attested—not broker proof of delivery and not proof a person
read it. A deliberately declined owned-thread continuation makes no reply call and therefore has
no receipt and no durable record; configured native activity still receives its cosmetic cleanup.
There is exactly one record request after an accepted answer
and no automatic retry after any uncertain outcome. Durable text remains untrusted model context
after explicit retrieval and never enters identity or Cedar as content.

## What other projects do

These projects illustrate common patterns; Dekopon does not depend on or endorse one of them. The links were checked on **2026-08-20** and their APIs remain version-sensitive.

| Project | Short-term context | Longer-lived memory |
|---|---|---|
| [OpenAI Agents SDK Sessions](https://openai.github.io/openai-agents-python/sessions/) | A session loads stored conversation items before a run and writes new messages/tool items after it. Built-in stores include local and database-backed options. | OpenAI Conversations-backed sessions and compaction are available, but SDK session memory is distinct from `previous_response_id`/provider continuation and from prompt caching. |
| [OpenAI Responses conversation state](https://developers.openai.com/api/docs/guides/conversation-state) | Applications can resend history or chain a response with `previous_response_id`. | A Conversations object can persist items across sessions, devices, or jobs. This is provider-managed state, not a prompt-cache lease. |
| [LangGraph memory](https://docs.langchain.com/oss/python/langgraph/add-memory) | Thread-level state is checkpointed; production checkpointers can use databases. It provides trimming, deletion, and summarization patterns. | A separate store holds user- or application-level data across threads and can support semantic search. |
| [Mem0](https://docs.mem0.ai/open-source/overview) | Extracts and retrieves selected conversational memories instead of replaying an unlimited transcript. | Positions a memory layer across sessions with vector/graph storage options and managed memory operations. |
| [Letta stateful agents](https://docs.letta.com/v1-sdk/concepts/stateful-agents) | Messages and in-context memory blocks form the current context; compaction manages the window. | Editable memory blocks can remain attached to an agent or be shared, while older messages remain retrievable. |
| [Zep concepts](https://help.getzep.com/concepts) | Builds context from chat and other sources rather than treating the raw transcript as the only memory. | A temporal knowledge graph tracks changing facts and relationships for retrieval. |

Across these systems, the recurring split is:

- **thread state** for immediate continuity;
- **compaction/summarization** to fit a context window;
- **durable stores** for cross-session facts or tasks; and
- **retrieval** to select a small relevant subset for the next prompt.

Prompt caching can make any repeated prefix cheaper. It does not implement any of those memory policies.

## What a broader memory framework could buy

**Status: Exploration.** The current accepted design is deliberately only durable on-demand chat
turns. Dekopon has no accepted general design for editable facts, tasks, semantic/vector retrieval,
cross-agent sharing, deletion/export UX, or automatic prompt insertion.

A framework could provide:

- durable continuity across daemon restarts, chat threads, transports, or devices;
- typed user preferences, task state, facts, summaries, and episodes instead of one undifferentiated transcript;
- provenance, timestamps, confidence, supersession, conflict handling, and explicit deletion;
- retrieval under a token budget rather than replaying every stored item;
- compaction and summarization policies with evaluations for information loss;
- storage adapters, encryption, retention controls, backups, and tenant isolation;
- observability for what was stored, retrieved, ignored, or forgotten; and
- a provider-neutral memory layer rather than coupling history to one API's response IDs.

It would also create a new high-risk data system. Before adopting a framework, Dekopon would need decisions on:

- who owns the memory and authenticates reads and writes;
- whether memory is scoped to a sender, agent, organization, task, or some combination;
- consent, retention, export, deletion, and incident-response behavior;
- prompt-injection persistence, poisoned memories, stale facts, and cross-sender retrieval;
- safe live-job invalidation on authority changes;
- whether a model may propose a memory write and what trusted component validates it; and
- how to prove retrieved memory never becomes trusted identity or authorization input.

The current narrow implementation keeps parsing/search/compaction in provider Wasm while the
broker owns only opaque namespace-bound files, quotas, and commit. Conversation content therefore
lives under the privileged broker's storage root, but never in its audit, spans, metrics, public
errors, or provider metadata. Any retrieved memory remains untrusted prompt context and every
effect remains freshly authorized. A broader framework must preserve those properties.

## Literal Rust walkthrough

The following is an abbreviated adapter-wire illustration with fake inline values, not a complete harness driver. Request-one harness bootstrap/schema messages and some portable history are omitted for readability. The two tool descriptions are shortened to keep the wire readable; their names, schemas, message shapes, options, endpoint, headers, and Responses fields match the current implementation. Credential values are intentionally fake.

### The key Rust types

```rust
use dekopon_model::{
    chatgpt::ChatGptCodexModel,
    model::{
        AssistantTurn, ChatModel, CompletionOptions, ModelFunctionCall,
        ModelMessage, ModelTool, ModelToolCall, ModelUsage,
    },
};
use serde_json::json;
use std::{path::Path, time::Duration};

let model = ChatGptCodexModel::new(
    "gpt-5.6-sol",
    Some(Path::new("/var/lib/dekopon/chatgpt/chatgpt-auth.json")),
    Duration::from_secs(120),
)?;

let mut messages = vec![
    ModelMessage::system(
        "You review pull requests. Be concise and cite the evidence you inspect.",
    ),
    ModelMessage::user(
        "Summarize dekopon-agents/dekopon PR #110 and tell me whether it is merged.",
    ),
];

let tools = vec![
    ModelTool {
        name: "bash".to_owned(),
        description: "Run one script in Dekopon's sandboxed shell.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "The script to run. Multiple lines are expected and encouraged."
                }
            },
            "required": ["script"],
            "additionalProperties": false
        }),
    },
    ModelTool {
        name: "inspect_agent_config".to_owned(),
        description: "Inspect this session's credential-free agent configuration.".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    },
];

let options = CompletionOptions::default().with_prompt_cache_key(
    "dekopond-conversation-7e91c87d8d6a4c13",
);

// Inside a harness logical call: the driver supplies the mandatory attempt recorder.
let first_turn: AssistantTurn = model.complete_with(&messages, &tools, &options, recorder)?;
```

`ModelMessage` is the backend-neutral transcript. `ModelTool` is the model-facing function schema. `CompletionOptions` carries routing metadata without changing the prompt. `ChatGptCodexModel` turns those values into private wire JSON, and `AssistantTurn` normalizes text, function calls, replay state, and usage from SSE.

`ConversationKey`, `ConversationSeed` and `BoundedConversationStore` belong to the harness; the gateway supplies their trusted routing coordinates from `BoundRoute` and the authenticated envelope.

### Request 1: Slack question

Immediately before `send_json`, the subscription request is equivalent to:

```rust
let request_1 = json!({
    "model": "gpt-5.6-sol",
    "store": false,
    "stream": true,
    "instructions":
        "You review pull requests. Be concise and cite the evidence you inspect.",
    "input": [{
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": "Summarize dekopon-agents/dekopon PR #110 and tell me whether it is merged."
        }]
    }],
    "tools": [
        {
            "type": "function",
            "name": "bash",
            "description": "Run one script in Dekopon's sandboxed shell.",
            "parameters": {
                "type": "object",
                "properties": {
                    "script": {
                        "type": "string",
                        "description": "The script to run. Multiple lines are expected and encouraged."
                    }
                },
                "required": ["script"],
                "additionalProperties": false
            }
        },
        {
            "type": "function",
            "name": "inspect_agent_config",
            "description": "Inspect this session's credential-free agent configuration.",
            "parameters": {
                "type": "object",
                "properties": {},
                "required": [],
                "additionalProperties": false
            }
        }
    ],
    "tool_choice": "auto",
    "parallel_tool_calls": true,
    "include": ["reasoning.encrypted_content"],
    "text": {"verbosity": "low"},
    "prompt_cache_key": "dekopond-conversation-7e91c87d8d6a4c13"
});
```

The actual HTTP operation is equivalent to this source path:

```rust
let config = ureq::Agent::config_builder()
    .timeout_global(Some(Duration::from_secs(120)))
    .max_redirects(0)
    .http_status_as_error(false)
    .build();
let agent: ureq::Agent = config.into();

let response = agent
    .post("https://chatgpt.com/backend-api/codex/responses")
    .header("authorization", "Bearer eyJ.fake-access-token.REDACTED")
    .header("chatgpt-account-id", "acct_example")
    .header("originator", "dekopon")
    .header("user-agent", &format!("dekopon-run/{}", env!("CARGO_PKG_VERSION")))
    .header("openai-beta", "responses=experimental")
    .header("accept", "text/event-stream")
    .send_json(&request_1)?;
```

`send_json` supplies the JSON content type. The production code exposes the access token only while constructing the authorization header; it never formats the credential into telemetry or a provider invocation.

Suppose the SSE stream asks for the `bash` tool. After parsing, the important normalized value looks like:

```rust
let first_turn = AssistantTurn {
    content: None,
    tool_calls: vec![ModelToolCall {
        id: "call_01".to_owned(),
        kind: "function".to_owned(),
        function: ModelFunctionCall {
            name: "bash".to_owned(),
            arguments: json!({
                "script": "gh pr view 110 -R dekopon-agents/dekopon"
            })
            .to_string(),
        },
    }],
    usage: Some(ModelUsage {
        input_tokens: Some(2_240),
        cached_input_tokens: Some(0),
        output_tokens: Some(96),
        reasoning_output_tokens: Some(54),
        total_tokens: Some(2_336),
    }),
    replay_items: vec![
        json!({
            "type": "reasoning",
            "id": "rs_01",
            "encrypted_content": "opaque-provider-state"
        }),
        json!({
            "type": "function_call",
            "id": "fc_01",
            "call_id": "call_01",
            "name": "bash",
            "arguments": "{\"script\":\"gh pr view 110 -R dekopon-agents/dekopon\"}"
        }),
    ],
};
```

The numbers are illustrative provider reports. In real code, `replay_items` is intentionally opaque and should not be constructed by an application.

### Request 2: tool result

The prompt loop appends the assistant turn and tool result:

```rust
messages.push(dekopon_model::model::assistant_message(&first_turn));
messages.push(ModelMessage::tool(
    "call_01",
    concat!(
        "{\"number\":110,\"state\":\"MERGED\",",
        "\"title\":\"fix(gateway): make agent config inspection repeatable\"}\n",
        "[exit code: 0]"
    ),
));

let second_turn = model.complete_with(&messages, &tools, &options, recorder)?;
```

All top-level fields remain the same. The exact `input` immediately before the second call is:

```rust
let request_2_input = json!([
    {
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": "Summarize dekopon-agents/dekopon PR #110 and tell me whether it is merged."
        }]
    },
    {
        "type": "reasoning",
        "id": "rs_01",
        "encrypted_content": "opaque-provider-state"
    },
    {
        "type": "function_call",
        "id": "fc_01",
        "call_id": "call_01",
        "name": "bash",
        "arguments": "{\"script\":\"gh pr view 110 -R dekopon-agents/dekopon\"}"
    },
    {
        "type": "function_call_output",
        "call_id": "call_01",
        "output": concat!(
            "{\"number\":110,\"state\":\"MERGED\",",
            "\"title\":\"fix(gateway): make agent config inspection repeatable\"}\n",
            "[exit code: 0]"
        )
    }
]);
```

This is the strongest cache opportunity: request 2 keeps request 1's instructions, user item, and tools stable and appends the provider's own replay items plus the result. It also carries the same `prompt_cache_key`.

Assume the final SSE turn says:

```text
PR #110, “fix(gateway): make agent config inspection repeatable,” removes the
one-call limit from agent configuration inspection. It is merged.
```

The provider may report some of request 2's input as cached. Dekopon records the reported count; it does not infer one from the identical Rust values.

### Request 3: a Slack follow-up

For a text-only accepted job, the portable summary can be illustrated as follows (a tool-using job also retains bounded groups and execution observations):

```rust
let remembered = dekopon_harness::history::JobRecord::completed(
    "Summarize dekopon-agents/dekopon PR #110 and tell me whether it is merged.",
    "PR #110, “fix(gateway): make agent config inspection repeatable,” removes the \
     one-call limit from agent configuration inspection. It is merged.",
);
```

When the same sender follows up with “What files did it change?”, the gateway authorizes the message again and reuses both the shared model client and the conversation's opaque key. The new first request's `input` is:

```rust
let request_3_input = json!([
    {
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": "Summarize dekopon-agents/dekopon PR #110 and tell me whether it is merged."
        }]
    },
    {
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": concat!(
                "PR #110, “fix(gateway): make agent config inspection repeatable,” removes ",
                "the one-call limit from agent configuration inspection. It is merged."
            ),
            "annotations": []
        }]
    },
    {
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": "What files did it change?"
        }]
    }
]);
```

Encrypted reasoning is absent. This abbreviated example omits retained portable tool groups and execution summaries; real selection can include them under independent byte limits. It is not an exact replay of opaque provider continuation or a promised cache match.

## How to evaluate caching in a deployment

1. Use a real second or later model turn; the first eligible request normally has nothing earlier to hit.
2. Query `usage.input_tokens` and `usage.cached_input_tokens` on `accounting.model.call`.
3. Treat a missing cached field as unreported. Do not coerce it to zero.
4. Compare the ratio only across calls whose provider reported both values.
5. Check whether instructions, tools, model, attachment parts, or the front of history changed.
6. Check whether a history trim, idle eviction, capacity eviction, grant change, or process restart rotated or rewrote the lane.
7. Remember the public API eligibility minimum: a short common prefix may be perfectly stable and still too small to cache. The subscription endpoint's threshold remains undocumented.

The useful metric is observed reuse, not the existence of a key:

```text
cache ratio = sum(cached_input_tokens) / sum(input_tokens)
```

Compute it only over calls where both fields were reported. A key proves Dekopon asked for cache affinity. It never proves the provider supplied it.

## Related documents

- [`dekopond.md`](dekopond.md) — routing, persistent-conversation bounds, cache-key privacy, and telemetry.
- [`security-model.md`](security-model.md#conversation-memory-as-a-trust-surface) — retained text and prompt-injection dwell time.
- [`run.md`](run.md#chatgptcodex-subscription) — account login and the one-shot subscription runner.
- [`chatgpt-credential.md`](chatgpt-credential.md) — rotating subscription credential lifecycle.
- [`observability.md`](observability.md) — model usage fields and payload gating.
- [OpenAI Prompt Caching](https://developers.openai.com/api/docs/guides/prompt-caching) — public API behavior, not a subscription-endpoint guarantee.
- [OpenAI conversation state](https://developers.openai.com/api/docs/guides/conversation-state) — public Responses state patterns, not current Dekopon behavior.

Token accounting is owned by the mandatory harness ledger, across attempts, model/effort segments,
checkpoint restore and terminal delivery dispositions. See [Accounting](observability.md#accounting)
for optional usage, subset arithmetic, unknown spend and aggregation levels. Direct model adapters
must accept an `AttemptRecorder`; standalone callers can supply a bounded `AttemptLog`, while the
harness supplies its checkpoint-backed recorder. A completion return value is not the accounting
commit point.
