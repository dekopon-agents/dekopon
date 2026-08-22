# JSONPlaceholder provider

A narrow Rust demonstration provider that composes `dekopon:provider@0.2.0` with the broker-owned `dekopon:http/client@1.0.0` import.

It deliberately separates authority:

- `jsonplaceholder.posts.get` is a low-risk, idempotent `read-only` GET for one post ID.
- `jsonplaceholder.posts.create` is a medium-risk, non-idempotent `external-write` POST. JSONPlaceholder returns a synthetic record and does not persist it, but the capability remains classified as an external write.

Both operations default to `https://jsonplaceholder.typicode.com`. The optional `endpoint` input exists only to support deterministic deployments and mock tests: the guest accepts the production origin or an explicit literal loopback HTTP socket, while broker policy must independently grant the exact authority and method. It never accepts arbitrary HTTP hosts, URL credentials, paths, queries, or fragments.

Native unit tests inject mock responses and prove request method/path/body semantics, bounded input/response validation, separated metadata, and transport-error redaction. Broker integration tests use ephemeral loopback servers and never contact JSONPlaceholder.

A production broker must declare each operation independently, in two places. `constraintSets`
says how narrowly each one executes; the Cedar policy says who may reach it. Omit the create half
of either to provide read-only access.

```yaml
# broker.yaml
constraintSets:
  jsonplaceholder.posts.get:
    provider: jsonplaceholder
    effect: read-only
    risk: Low
    idempotency: idempotent
    constraints:
      timeoutMs: 5000
      maxOutputBytes: 65536
      http:
        allowedHosts: [jsonplaceholder.typicode.com]
        allowedMethods: [GET]
        maxRequests: 1
        maxRequestBytes: 8192
        maxResponseBytes: 65536
        allowPlaintextLoopback: false
  jsonplaceholder.posts.create:
    provider: jsonplaceholder
    effect: external-write
    risk: Medium
    idempotency: non-idempotent
    constraints:
      timeoutMs: 5000
      maxOutputBytes: 65536
      http:
        allowedHosts: [jsonplaceholder.typicode.com]
        allowedMethods: [POST]
        maxRequests: 1
        maxRequestBytes: 8192
        maxResponseBytes: 65536
        allowPlaintextLoopback: false
```

```cedar
// policies.cedar — the read and the write are separate statements, because read authority never
// implies write authority.
@id("demo-agent-posts-read")
permit(principal == Dekopon::Principal::"local-user",
       action == Dekopon::Action::"jsonplaceholder.posts.get",
       resource == Dekopon::Provider::"jsonplaceholder")
when { context has agent && context.agent == "demo-agent" };

@id("demo-agent-posts-create")
permit(principal == Dekopon::Principal::"local-user",
       action == Dekopon::Action::"jsonplaceholder.posts.create",
       resource == Dekopon::Provider::"jsonplaceholder")
when { context has agent && context.agent == "demo-agent"
    && context.effect == "external-write" };
```

Run native checks:

```console
cargo fmt --manifest-path examples/providers/jsonplaceholder/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/jsonplaceholder/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/jsonplaceholder/Cargo.toml
```

Build and inspect the generated component:

```console
examples/providers/jsonplaceholder/build.sh
wasm-tools validate examples/providers/jsonplaceholder-provider.wasm
wasm-tools component wit examples/providers/jsonplaceholder-provider.wasm
```

The decoded component must export only `describe` and `invoke`, import exactly `dekopon:http/client@1.0.0`, and import no WASI interfaces. Direct `dekopon-run` rejects it because the immediate linker remains empty.
