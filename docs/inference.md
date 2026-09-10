# Model inference, prompt caching, and memory

This document follows a Slack message from `dekopond` into the ChatGPT subscription transport: what Dekopon caches, what it remembers, and what reaches the wire.

**Status: Current, except where marked Exploration.** Dekopon sends cache-affinity hints, preserves
append-only model turns, reports provider-declared cache usage, keeps a bounded conversation in
gateway memory, generates one bounded outbound image on an opted-in route, and optionally stores and
retrieves namespace-isolated durable chat turns through a JSONL provider. It does not cache completed
answers, request extended provider retention, use provider-managed conversation objects, retain
generated image bytes, or automatically replay durable memory.

The ChatGPT subscription transport uses a fixed, undocumented ChatGPT/Codex backend rather than the
public OpenAI Platform API. Public OpenAI documentation is context, not a contract for that endpoint.

## Three different mechanisms

“Cache,” “conversation,” and “memory” are easy to collapse into one idea. They solve different problems.

| Mechanism | Owner | Purpose | Current Dekopon behavior |
|---|---|---|---|
| Prompt-prefix cache | Model provider | Avoid recomputing an identical leading prompt | Sends a stable key and stable prefixes; cannot inspect, create, refresh, or delete provider entries |
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
  -> ChatGptCodexModel builds a Responses-shaped serde_json::Value
  -> POST https://chatgpt.com/backend-api/codex/responses
  -> SSE events become AssistantTurn
  -> tool call? append opaque replay items + tool output and call the model again
       `generate_image`? one fixed-endpoint request, one PNG leaves through a byte-free output slot
  -> exact bounded text plus optional PNG receives complete Slack transport acceptance
     or an optional owned-thread continuation declines and sends no reply
  -> one fresh hidden record request only after an accepted reply and effective durable surface
  -> persistent route stores only the new question and final answer, or a declined user-only turn
```

The broker authorization leg is new for every Slack message. Neither remembered text nor a prompt cache key enters Cedar policy or grants a capability.

## What is optimized today

Five intentional cache-friendly properties:

1. **One opaque key per useful reuse lane.** A persistent conversation gets one minted key. A one-shot route gets one key shared by that route's requests, where only the common agent prefix can match.
2. **Append-only turns inside one model session.** If the model calls a tool, the next request retains the earlier `input` items byte-for-byte and appends the reasoning replay, function call, and function result.
3. **Stable provider replay.** The subscription transport requests `reasoning.encrypted_content` and replays the opaque provider items on the next tool-loop turn instead of reconstructing them.
4. **Stable instructions and tools.** System messages are hoisted to `instructions`; tool definitions are built once for the session. Tests fail if appending a turn mutates either.
5. **Measured rather than assumed hits.** Responses usage is normalized into `ModelUsage::cached_input_tokens` and exported on `prompt.model_turn` plus `accounting.model.turn`.

The source contracts are in:

- [`crates/dekopon-model/src/model.rs`](../crates/dekopon-model/src/model.rs) — `ModelMessage`, `ModelTool`, `CompletionOptions`, `AssistantTurn`, `ModelUsage`, and `ChatModel`;
- [`crates/dekopon-model/src/chatgpt.rs`](../crates/dekopon-model/src/chatgpt.rs) — the subscription request builder, SSE parser, and prefix-stability tests;
- [`crates/dekopon-agent/src/prompt.rs`](../crates/dekopon-agent/src/prompt.rs) — the bounded model/tool loop;
- [`crates/dekopon-agent/src/prompt/history.rs`](../crates/dekopon-agent/src/prompt/history.rs) — compacted cross-message history; and
- [`crates/dekopond/src/cache_key.rs`](../crates/dekopond/src/cache_key.rs), [`conversation.rs`](../crates/dekopond/src/conversation.rs), and [`session.rs`](../crates/dekopond/src/session.rs) — key lifetime, history lifetime, and Slack-session assembly.

### What is not optimized or cached

- Requests do not set `prompt_cache_retention`, `prompt_cache_options`, or explicit cache breakpoints.
- Requests set `store: false` and use neither `previous_response_id` nor a provider conversation identifier.
- Completed answers are not memoized. Each incoming message makes a fresh model request after authorization.
- `dekopond` builds one model client per configured model on first use and shares it across every later message and session (`ModelCache` in [`crates/dekopond/src/session.rs`](../crates/dekopond/src/session.rs)); the prompt cache key and `CompletionOptions` stay request-scoped. Sharing the client reuses TCP/TLS connections and the loaded credential; it does not make the remote prompt cache more durable.
- The gateway does not estimate tokens before a request. Its history bound is bytes plus whole turns because provider token counts arrive only after a billed call.
- Cross-message compaction preserves conversational meaning, not the full prior wire transcript. A follow-up can reuse a leading prefix, but it is not necessarily an append-only extension of the last tool-loop request.

Within one session, the second request is the first request plus more items. Between Slack messages, `History` reconstructs only the previous question and final answer; tool calls, tool outputs, and encrypted reasoning are gone. That keeps memory bounded and portable across model backends, and it can shorten the matching provider-cache prefix.

## Prompt cache key lifecycle

A key is a routing hint, not a cache handle. Dekopon cannot use it to read another response,
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

Dekopon sends none of those retention or breakpoint controls. Even if its configured model has the same name as a public API model, the ChatGPT subscription endpoint and account policy are different surfaces. Do not copy a public API TTL into an availability or cost forecast for the subscription.

## Can a long-lived agent keep a cache warm?

Not by staying alive.

A local object has no lease on provider memory. The official public API model is request-driven: matching requests are routed toward cached prefixes, and the provider controls retention and eviction. There is no documented mechanism where any of these pins the cache:

- a running `dekopond` process;
- a live `ChatGptCodexModel` or `ureq::Agent`;
- an HTTP keep-alive connection;
- an OAuth access token or ChatGPT login;
- a response ID or provider conversation object; or
- a Dekopon conversation entry that receives no model calls.

Synthetic keep-alive prompts would consume quota, create more retained input, and buy no subscription-endpoint guarantee. Dekopon does not send them.

One long-lived optimization is in place: `dekopond` shares one model client per configured model across gateway messages, reusing connections and the loaded credential, with refreshes coordinated through the client's credential mutex and the cross-process advisory lock beside the auth file. `CompletionOptions` stays request-scoped so a shared client cannot apply one conversation's key to another.

## How scoped conversation memory works

A route opts in with a `conversation:` block, and [`dekopond.md`](dekopond.md#conversations) owns its
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

## Outbound image generation

A route sets `imageGenerator: true` to add the gateway-owned `generate_image` meta tool to its chat
model; [`dekopond.md`](dekopond.md#generated-images) owns the endpoint, credential, delivery path, and
per-session bounds. What it means for inference is that the generated bytes leave through a
request-local output slot and never become a `ModelMessage`, tool result, prompt transcript, or
`PromptOutcome`: the model reads one fixed success or failure sentence and writes its caption around
that. The generator is a separate model client, so the OpenAI-compatible Chat Completions and
ChatGPT/Codex subscription contracts remain the orchestrators they already are. Persistent and durable
memory keep the final text, not the PNG or the generation prompt, so a follow-up can discuss the
caption and cannot edit prior pixels without generating a new image.

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

The following is production-shaped, executable-style Rust with fake inline values. The two tool descriptions are shortened to keep the wire readable; their names, schemas, message shapes, options, endpoint, headers, and Responses fields match the current implementation. Credential values are intentionally fake.

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
        "Summarize example-org/example PR #7 and tell me whether it is merged.",
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

let first_turn: AssistantTurn = model.complete_with(&messages, &tools, &options)?;
```

`ModelMessage` is the backend-neutral transcript. `ModelTool` is the model-facing function schema. `CompletionOptions` carries routing metadata without changing the prompt. `ChatGptCodexModel` turns those values into private wire JSON, and `AssistantTurn` normalizes text, function calls, replay state, and usage from SSE.

The gateway-only types around them are `ConversationKey`, `ConversationSeed`, `ConversationStore`, and `BoundRoute`. They are crate-private because transports should not manufacture or serialize conversation state directly.

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
            "text": "Summarize example-org/example PR #7 and tell me whether it is merged."
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
    .header("user-agent", &format!("dekopon/{}", env!("CARGO_PKG_VERSION")))
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
                "script": "gh pr view 7 -R example-org/example"
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
            "arguments": "{\"script\":\"gh pr view 7 -R example-org/example\"}"
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
        "{\"number\":7,\"state\":\"MERGED\",",
        "\"title\":\"fix(api): reject unbounded page size\"}\n",
        "[exit code: 0]"
    ),
));

let second_turn = model.complete_with(&messages, &tools, &options)?;
```

All top-level fields remain the same. The exact `input` immediately before the second call is:

```rust
let request_2_input = json!([
    {
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": "Summarize example-org/example PR #7 and tell me whether it is merged."
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
        "arguments": "{\"script\":\"gh pr view 7 -R example-org/example\"}"
    },
    {
        "type": "function_call_output",
        "call_id": "call_01",
        "output": concat!(
            "{\"number\":7,\"state\":\"MERGED\",",
            "\"title\":\"fix(api): reject unbounded page size\"}\n",
            "[exit code: 0]"
        )
    }
]);
```

This is the strongest cache opportunity: request 2 keeps request 1's instructions, user item, and tools stable and appends the provider's own replay items plus the result. It also carries the same `prompt_cache_key`.

Assume the final SSE turn says:

```text
PR #7, “fix(api): reject unbounded page size,” caps the list endpoint's
page size at 100. It is merged.
```

The provider may report some of request 2's input as cached. Dekopon records the reported count; it does not infer one from the identical Rust values.

### Request 3: a Slack follow-up

At the end of the first Slack message, the persistent history stores only:

```rust
let remembered = dekopon_agent::prompt::ConversationTurn::completed(
    "Summarize example-org/example PR #7 and tell me whether it is merged.",
    "PR #7, “fix(api): reject unbounded page size,” caps the list endpoint's \
     page size at 100. It is merged.",
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
            "text": "Summarize example-org/example PR #7 and tell me whether it is merged."
        }]
    },
    {
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "output_text",
            "text": concat!(
                "PR #7, “fix(api): reject unbounded page size,” ",
                "caps the list endpoint's page size at 100. It is merged."
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

The earlier encrypted reasoning, function call, and tool result are absent. This request shares a stable beginning with the earlier calls, but it is a compacted conversation rather than a replay of the full execution transcript. That is the trade: bounded, portable memory and safer trimming in exchange for a potentially shorter provider-cache match.

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
