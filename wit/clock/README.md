# `dekopon:clock@1.0.0`

Canonical WIT source for the broker host's wall clock.

```console
wkg get \
  --registry dekopon-agents.github.io \
  --output dekopon-clock.wasm \
  dekopon:clock@1.0.0
```

The package defines one `wall` interface with one function, `now-unix-millis`, and no world. It is the clock a provider reads without any other grant: a component that imports it may call it while the broker runs an authorized `invoke`, and a read during `describe` or a command run traps, because those exports are pure by contract. The durable-files interface in `dekopon:storage` carries its own `wall-time-ms`, reachable only under a storage grant.

The guest binding mirror is `crates/dekopon-provider-clock/wit/deps/clock.wit`. Keep it and every mirror listed in `docs/development.md` byte-identical to `clock.wit`.
