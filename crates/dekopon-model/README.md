# dekopon-model

`dekopon-model` contains Dekopon's bounded model-client boundary:

- the generic `ChatModel` request/response contract, whose one method reports a turn's visible text
  through a caller-supplied callback while the turn is still arriving, and stops the turn when that
  callback says to;
- `ModelText`, the only container model-authored visible text travels in, which nothing outside this
  crate can fill;
- async `InferenceModel` generation with a per-session `BlockingModel` bridge into the prompt loop;
- shared async Server-Sent Events framing bounded at 16 MiB for Codex, OpenAI-compatible and
  OpenRouter generation, also used by offline transcript replay;
- `OpenAiClient`, an async OpenAI-compatible Chat Completions client, streaming by default and
  tolerant of what "compatible" endpoints actually send, with a 10 MiB buffered-response ceiling;
- `OpenRouterClient`, fixed-endpoint streaming with authored generation/reasoning/routing controls,
  optional explicit system-prefix caching, private merged reasoning replay;
- native ChatGPT/Codex subscription device authentication, token refresh, and Responses streaming;
- `chatgpt::CredentialFile`, the one implementation of "use the credential at this path": the
  cross-process advisory lock, the adoption of a newer record another process wrote, the refresh
  60 s before expiry, and the atomic write-back;
- request-scoped prompt-cache routing hints plus normalized provider-reported cached-token usage;
- multimodal message content, where a message carries `ContentPart`s — text, images, documents —
  instead of a single string.

A message is text unless it is built with `ModelMessage::user_with_parts`. A text-only message
serializes as a bare string on the chat-completions wire and as one `input_text` part on
Responses. Attachment bytes are encoded only while a request is being built: `ModelMessage`'s own
`Debug` and `Serialize` render a summary instead, because those are what reach the prompt
transcript in the audit log. `Serialize` writes
`[image/png, 219136 bytes]` for an image and `[report.pdf (application/pdf), 219136 bytes]`
for a file; `Debug` writes the same counts as a `bytes: 219136` field. The count is raw bytes,
never a scaled unit.

`CredentialFile` has two consumers and must not grow a third definition. The async `CodexClient` is one;
`dekopon-brokerd`'s `kind: chatgptSubscription` provider credential is the other, which is why this
crate is in the privileged broker's dependency tree at all. The refresh token rotates and the
authorization server retires its predecessor, so a second implementation of that sequence is a second
way to revoke a token family. Two holders of one *file* are coordinated by the snapshot and the lock;
two holders of one *account* want two files and two logins.

*Committed direction:* the broker's `credential`/`credentialByAgent` bindings will be replaced by
public DRNs without duplicating or removing this refresh implementation
([migration requirements](../../docs/design.md#legacy-credential-bindings)). The gateway's model
credential is not part of that provider-binding migration.

The gateway executable owns account lifecycle through `dekopond auth`; execution clients such as
external embeddings consume the resulting credentials. Model credentials are never passed to Wasm
provider components. [`docs/inference.md`](../../docs/inference.md) traces these types into literal
ChatGPT wire JSON and distinguishes cache affinity, gateway conversation history, optional
broker-provider durable chat-turn retrieval, and broader agent memory.

## Outside embedding

Construct `CodexClient`, `OpenRouterClient` or `OpenAiClient`, select it with `ModelClient`, and bridge
into the synchronous agent loop with a per-session `BlockingModel` on `spawn_blocking`.
`with_loopback_endpoint` accepts only literal loopback over plain HTTP and never refreshes credentials.
See [`docs/inference.md`](../../docs/inference.md) and
[`docs/observability.md`](../../docs/observability.md#model-exchange-fields).
