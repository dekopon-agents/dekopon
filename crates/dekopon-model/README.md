# dekopon-model

`dekopon-model` contains Dekopon's bounded model-client boundary:

- the generic `ChatModel` request/response contract;
- an OpenAI-compatible Chat Completions client;
- native ChatGPT/Codex subscription device authentication, token refresh, and Responses streaming;
- request-scoped prompt-cache hints, explicit validated effort, and normalized provider usage;
- a fixed-endpoint OpenAI Images client producing one signature-validated PNG under explicit prompt,
  encoded-response, and 8 MiB decoded bounds; and
- multimodal message content, where a message carries `ContentPart`s — text, images, documents —
  instead of a single string.

A message is text unless it is built with `ModelMessage::user_with_parts`, and a text message
serializes to exactly the bytes it did before parts existed on both wire formats. Attachment bytes
are encoded only while a request is being built: `ModelMessage`'s own `Debug` and `Serialize` render
a summary instead, because those are what reach the prompt transcript in the audit log. `Serialize`
writes `[image/png, 219136 bytes]` for an image and `[report.pdf (application/pdf), 219136 bytes]`
for a file; `Debug` writes the same counts as a `bytes: 219136` field. The count is raw bytes, never
a scaled unit.

`GeneratedImage` renders only media type and byte count under `Debug`; its raw bytes are exposed
only to the embedding delivery path. The Images client never reuses the undocumented ChatGPT/Codex
subscription endpoint and never accepts a model-selected endpoint.

The `dekopon` CLI owns account lifecycle through `dekopon auth`; execution clients such as
`dekopon-run` consume the resulting credentials. Model credentials are never passed to Wasm
provider components. [`docs/inference.md`](../../docs/inference.md) traces these types into literal
ChatGPT wire JSON and distinguishes cache affinity, gateway conversation history, optional broker-provider durable chat-turn retrieval, and broader agent memory.

`CompletionOptions::with_effort` accepts `dekopon_core::Effort::{ProviderDefault, Low, Medium,
High}`. Default omits the wire setting; explicit settings encode Chat Completions
`reasoning_effort` or Responses `reasoning.effort`. An unaware adapter refuses explicit effort
before I/O. Adapter support means encoding support, not a guarantee the remote model accepts it;
there is no automatic effort fallback. Options remain per-call even on shared cached clients.

## Required inference accounting (Unreleased)

`ChatModel::complete`/`complete_with` and `ImageGenerator::generate` require an `AttemptRecorder`.
Reserve each inference HTTP attempt before transmission and observe normalized optional fields
before content validation. The built-in subscription adapter counts its one explicit-401 retry
separately; credential refresh is not inference. `AttemptLog` is a bounded standalone recorder,
not a job accumulator. Harness consumers supply the job-owned checkpointed recorder. Cached input
and reasoning output are subsets; no dollar pricing is inferred.
