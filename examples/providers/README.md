# Provider examples

- [`echo/`](echo/) is the Rust source for a provider implementing `dekopon_provider_sdk::Provider` with plain echo and deterministic reverse, uppercase, lowercase, and ransom-case capabilities. It also compiles `dekopon-provider-storage` with no features; the generated component's zero-import table proves that the facade's default feature set grants nothing.
- [`echo-provider.wasm`](echo-provider.wasm) is the generated component checked in so `dekopon-run` is usable without first installing a Wasm build toolchain.
- [`http-probe/`](http-probe/) composes the provider exports with `dekopon:http/client@1.0.0` and validates the generalized SDK world adapter plus the broker component host. Its `conditional-write` is the in-tree two-call capability — read, then write only if the observed etag holds — which is what keeps `maxRequests`, per-call evidence, and the host-call limit covered here. [`../conditional-write/`](../conditional-write/README.md) is the end-to-end deployment built on it.
- [`http-probe-provider.wasm`](http-probe-provider.wasm) is its generated component fixture. The direct runner intentionally rejects it because the immediate linker remains empty; broker-host tests execute it only against ephemeral loopback servers under exact constraints.
- [`jsonplaceholder/`](jsonplaceholder/) implements separately named post-read and external-write operations with bounded typed inputs and production-origin or literal-loopback endpoint validation.
- [`jsonplaceholder-provider.wasm`](jsonplaceholder-provider.wasm) is its generated component. Native and broker tests use injected or ephemeral loopback mocks and never contact the public JSONPlaceholder service.
- The nineteen-capability GitHub provider is no longer here. A provider meant for real use lives in
  its own repository with its own tags, issues, and release cadence, and `gh` is the first to leave:
  it ships from [`dekopon-provider-gh`](https://github.com/dekopon-agents/dekopon-provider-gh), and the image
  fetches its component at a pinned tag rather than vendoring it. Its end-to-end walkthrough,
  [`examples/pr-summarizer-linter/`](https://github.com/dekopon-agents/dekopon-provider-gh/blob/main/examples/pr-summarizer-linter/README.md),
  went with it.
- The opt-in, unofficial, private, unsupported, mock-only Skylight Exploration has been removed from this tree. Its designated standalone home is [`dekopon-provider-skylight-private`](https://github.com/dekopon-agents/dekopon-provider-skylight-private), with ownership of its [source](https://github.com/dekopon-agents/dekopon-provider-skylight-private/blob/main/src/lib.rs), [build](https://github.com/dekopon-agents/dekopon-provider-skylight-private/blob/main/build.sh), [tests](https://github.com/dekopon-agents/dekopon-provider-skylight-private/tree/main/tests), and [notices](https://github.com/dekopon-agents/dekopon-provider-skylight-private/blob/main/THIRD_PARTY_NOTICES.md). Publication of this prepared core extraction waits for that repository's public green `main`; these links name the destination and do not assert that it is already available.
- Any standalone `skylight-private` Wasm is CI-generated and untracked. No provider release exists yet, and Dekopon core does not package or deploy it or add it to default catalogs, images, policies, or deployments.
- [`memory-chat/`](memory-chat/) is the generated optional JSONL-only durable chat-memory provider.
  It exposes recent/search to an authorized model and never resolves hidden record.
- [`memory-reservation-probe/`](memory-reservation-probe/) is an import-free malicious fixture that
  occupies both the reserved provider ID and capability prefix. It is test-only and never packaged.
- [`provider-v0-1-compat/`](provider-v0-1-compat/) is an import-free component generated against
  the immutable two-export `dekopon:provider@0.1.0` world for real host compatibility coverage.
- [`storage-probe/`](storage-probe/) is the durable-files conformance fixture. Its generated artifact
  is deliberately not packaged in any scanned image directory.

Regenerate each component with the commands in its source README. Do not edit `.wasm` artifacts directly.

These are fixtures: they exist to prove a host property, and they are sized and scoped for that.
A provider meant for real use lives in its own repository with its own tags, issues, and release
cadence — [`dekopon-provider-turso-sql`](https://github.com/dekopon-agents/dekopon-provider-turso-sql),
a SQLite-compatible SQL engine over `durable-files`, is the first. It ships as a signed release
asset rather than a checked-in component, because an 11 MB artifact and a SQL engine's dependency
graph are not things a fixture directory should carry.
