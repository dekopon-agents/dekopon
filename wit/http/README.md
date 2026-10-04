# `dekopon:http@1.1.0`

Canonical WIT source for Dekopon's buffered and asset-streamed broker-mediated HTTP client interface.

```console
wkg get \
  --registry dekopon-agents.github.io \
  --output dekopon-http.wasm \
  dekopon:http@1.1.0
```

The package defines one `client` interface and no world. It transports arbitrary valid HTTP method tokens, ordered byte-valued headers, and complete byte-buffer bodies for `send`; `stream` accepts asset-backed parts and spools the bounded response. It does not grant network access: a provider world must import the interface, and only an authorized broker host may implement it.

All four guest, host and probe mirrors must match `http.wit` byte-for-byte. The previously published `@1.2.0` remains a historical remote artifact, not a supported broker import.
