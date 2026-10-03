# dekopon-model-proxy

`dekopon-model-proxy` lets `claude`, `pi` or `codex` inside a Firecracker guest call a model
with no credentials of their own, spending the agent's token budget. `dekopond` serves it on its
own mTLS listener. The guest reaches it through the jail's egress gateway, which authenticates
with a client certificate and asserts which VM it serves in `x-dekopon-vm-subject`.

- `ModelProxy::router`: `POST /v1/messages`, `/v1/messages/count_tokens` (Anthropic Messages),
  `/v1/responses` (OpenAI Responses, to Codex) and `/v1/chat/completions` (to OpenRouter).
  - A guest names a configured model. The proxy checks the guest's grant, rewrites `model` to the
    upstream id, and injects the upstream credential.
  - It drops the guest's `authorization` and `x-api-key`, and passes `anthropic-version`,
    `anthropic-beta` and `x-request-id`.
  - Every other byte of the body is forwarded as sent.
- Each metered call is admitted through `dekopon_model_token_governor::Metering` before it is
  sent. A refusal is the dialect's own throttling error with `retry-after`, or a plain 400 when the
  request can never fit. `count_tokens` is never charged.
- The response streams back unbuffered. Each SSE event is read through `dekopon_model::wire`'s
  usage structs, and the call settles when the stream ends. A client that disconnects is charged
  what was observed. While the upstream is silent the proxy writes `: ping` every 20 s.
- `tls::Listener` requires a client certificate that chains to the configured CA and carries the
  configured URI SAN, and rereads its files when their mtime changes.
- Bodies are capped at 8 MiB (`MAX_BODY_BYTES`); past it the proxy answers 413. `Listener::serve`
  takes the connection cap (`dekopond`'s `proxy.maxConnections`, default 16). Each connection
  briefly holds two copies of its body while reading and rewriting it, so request bodies take up to
  `max_connections × 2 × 8 MiB` together: 256 MiB at the default.

It owns no budgets, usage parsing or credential refresh: those live in
`dekopon-model-token-governor`, `dekopon_model::wire` and `dekopon_model::chatgpt::CredentialFile`.
