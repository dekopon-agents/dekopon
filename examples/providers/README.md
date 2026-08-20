# Provider examples

- [`echo/`](echo/) is the Rust source for a provider implementing `dekopon_provider_sdk::Provider` with plain echo and deterministic reverse, uppercase, lowercase, and ransom-case capabilities.
- [`echo-provider.wasm`](echo-provider.wasm) is the generated component checked in so `dekopon-run` is usable without first installing a Wasm build toolchain.
- [`http-probe/`](http-probe/) composes the provider exports with `dekopon:http/client@1.0.0` and validates the generalized SDK world adapter plus the broker component host.
- [`http-probe-provider.wasm`](http-probe-provider.wasm) is its generated component fixture. The direct runner intentionally rejects it because the immediate linker remains empty; broker-host tests execute it only against ephemeral loopback servers under exact constraints.
- [`jsonplaceholder/`](jsonplaceholder/) implements separately named post-read and external-write operations with bounded typed inputs and production-origin or literal-loopback endpoint validation.
- [`jsonplaceholder-provider.wasm`](jsonplaceholder-provider.wasm) is its generated component. Native and broker tests use injected or ephemeral loopback mocks and never contact the public JSONPlaceholder service.
- [`gh/`](gh/) is the "fake `gh`" GitHub provider: nineteen separately named repository, pull-request, and issue capabilities with fixed request shapes, bounded output projections, and SHA-pinned review/merge writes. There is deliberately no `gh.api.*` passthrough. The guest never sets `authorization`; the broker injects a destination-bound credential at the native HTTP boundary, where no guest can observe it. [`../pr-summarizer-linter/`](../pr-summarizer-linter/README.md) is the end-to-end deployment that uses those boundaries to inspect a pull request and post one review comment.
- [`gh-provider.wasm`](gh-provider.wasm) is its generated component. Native tests script exact request/response exchanges; nothing contacts the public GitHub API.
- [`skylight-private/`](skylight-private/) is an opt-in, unofficial, unsupported Exploration containing exactly two broker-only private Skylight reads. It requires a static destination-bound broker bearer, has no OAuth or endpoint override, projects bounded identity/frame fields, and uses injected mocks only. It is absent from default catalogs, images, policies, and deployments; see its adjacent pyskylight MIT notice.
- [`skylight-private-provider.wasm`](skylight-private-provider.wasm) is its generated component. The immediate host rejects its HTTP import, and broker-host integration exercises only a pre-network grant denial—no test contacts Skylight.

Regenerate each component with the commands in its source README. Do not edit `.wasm` artifacts directly.
