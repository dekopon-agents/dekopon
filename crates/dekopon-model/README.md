# dekopon-model

`dekopon-model` contains Dekopon's bounded model-client boundary:

- the generic `ChatModel` request/response contract;
- an OpenAI-compatible Chat Completions client;
- native ChatGPT/Codex subscription device authentication, token refresh, and Responses streaming.

The `dekopon` CLI owns account lifecycle through `dekopon auth`; execution clients such as
`dekopon-run` consume the resulting credentials. Model credentials are never passed to Wasm
provider components.
