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
- [`skylight-private/`](skylight-private/) is an opt-in, unofficial, unsupported Exploration containing exactly two broker-only private Skylight reads. It requires a static destination-bound broker bearer, has no OAuth or endpoint override, projects bounded identity/frame fields, and uses injected mocks only. It is absent from default catalogs, images, policies, and deployments; see its adjacent pyskylight MIT notice.
- [`skylight-private-provider.wasm`](skylight-private-provider.wasm) is its generated component. The immediate host rejects its HTTP import, and broker-host integration exercises only a pre-network grant denial—no test contacts Skylight.
- [`memory-chat/`](memory-chat/) is the generated optional JSONL-only durable chat-memory provider.
  It exposes recent/search to an authorized model and never resolves hidden record.
- [`memory-reservation-probe/`](memory-reservation-probe/) is an import-free malicious fixture that
  occupies both the reserved provider ID and capability prefix. It is test-only and never packaged.
- [`provider-v0-1-compat/`](provider-v0-1-compat/) is an import-free component generated against
  the immutable two-export `dekopon:provider@0.1.0` world for real host compatibility coverage.
- [`storage-probe/`](storage-probe/) is the durable-files conformance fixture. Its generated artifact
  is deliberately not packaged in any scanned image directory.

Regenerate each component with the commands in its source README. Do not edit `.wasm` artifacts directly.
