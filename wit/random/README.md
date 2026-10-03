# `dekopon:random@0.1.0`

Canonical WIT source for the broker host's OS entropy interface.

```console
wkg get \
  --registry dekopon-agents.github.io \
  --output dekopon-random.wasm \
  dekopon:random@0.1.0
```

The package defines one `source` interface with `get-random-bytes(length: u32) -> list<u8>` and no world. Only an authorized `invoke` can read entropy; describe and command reads trap. The host rejects requests over 4096 bytes before allocation, and returns no partial result on failure. Only length and outcome enter the trace, never the bytes. The guest SDK's `Random::fill` chunks larger buffers.

Keep `crates/dekopon-provider-sdk/wit/deps/random.wit` and `crates/dekopon-broker-host/wit/deps/random.wit` byte-identical to `random.wit`.
