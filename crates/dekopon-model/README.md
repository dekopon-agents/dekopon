# dekopon-model

`dekopon-model` contains Dekopon's bounded model-client boundary:

- the generic `ChatModel` request/response contract;
- an OpenAI-compatible Chat Completions client;
- native ChatGPT/Codex subscription device authentication, token refresh, and Responses streaming;
- request-scoped prompt-cache routing hints plus normalized provider-reported cached-token usage; and
- multimodal message content, where a message carries `ContentPart`s — text, images, documents —
  instead of a single string.

A message is text unless it is built with `ModelMessage::user_with_parts`, and a text message
serializes to exactly the bytes it did before parts existed on both wire formats. Attachment bytes
are encoded only while a request is being built: `ModelMessage`'s own `Debug` and `Serialize` render
them as `[image/png, 214 KB]`, because those are what reach the prompt transcript in the audit log.

The `dekopon` CLI owns account lifecycle through `dekopon auth`; execution clients such as
`dekopon-run` consume the resulting credentials. Model credentials are never passed to Wasm
provider components. [`docs/inference.md`](../../docs/inference.md) traces these types into literal
ChatGPT wire JSON and distinguishes cache affinity, gateway conversation history, optional broker-provider durable chat-turn retrieval, and broader agent memory.
