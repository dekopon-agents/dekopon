# `dekopon:http@1.0.0`

Canonical WIT source for Dekopon's buffered broker-mediated HTTP client interface.

```console
wkg get \
  --registry dekopon-agents.github.io \
  --output dekopon-http.wasm \
  dekopon:http@1.0.0
```

The package defines one `client` interface and no world. It transports arbitrary valid HTTP method tokens, ordered byte-valued headers, and complete byte-buffer bodies. It does not grant network access: a provider world must import the interface, and only an authorized broker host may implement it.

The guest binding mirror is `crates/dekopon-provider-http/wit/deps/http.wit`. Keep it byte-identical to `http.wit`.
