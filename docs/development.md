# Development guide

Read [`design.md`](design.md) before this guide. The design defines authority; this document maps common changes to source, tests, generated artifacts, and validation commands.

## Start here

From the repository root (`Cargo.toml`, `AGENTS.md`, and `docs/` should be present):

1. Run `git status --short --branch` and preserve unrelated work.
2. Classify the change as **Current**, **Committed direction**, or **Exploration**.
3. Read the area document selected by [`../AGENTS.md`](../AGENTS.md).
4. Find the implementation and its nearest tests before editing.
5. Check whether the change crosses the root workspace, the separate echo-provider workspace, a generated artifact, or a mirrored contract.

Prefer targeted tests while iterating, then run the scope-appropriate checks below. Do not claim a command or remote check passed unless it was actually observed.

## Repository map

| Area | Primary implementation | Behavior tests and fixtures |
|---|---|---|
| Domain identifiers and enums | `crates/dekopon-core/src/lib.rs` | Inline unit and compile-fail tests |
| Proposal/authorization typestate | `crates/dekopon-capability/src/lib.rs` | Inline unit tests |
| Resource wire types | `crates/dekopon-protocol/src/lib.rs` | Inline schema and round-trip tests |
| Config discovery and validation | `crates/dekopon-config/src/` | `crates/dekopon-config/src/tests.rs`, `examples/local/dekopon.yaml` |
| Operator CLI and model auth commands | `crates/dekopon/src/` | `crates/dekopon/tests/cli.rs` |
| Model clients and ChatGPT auth | `crates/dekopon-model/src/` | Inline mock HTTP/OAuth/SSE tests |
| Provider guest API and adapter | `crates/dekopon-provider-sdk/src/lib.rs`, `crates/dekopon-provider-sdk/wit/` | Inline adapter tests |
| Buffered HTTP WIT and guest facade | `wit/http/`, `crates/dekopon-provider-http/` | Guest validation and mirrored-contract tests plus WIT package workflow |
| Bounded native HTTP host | `crates/dekopon-http-host/src/` | Inline destination, method, DNS, header, bound, and loopback mock-server tests |
| Broker async component host | `crates/dekopon-broker-host/src/`, `crates/dekopon-broker-host/wit/` | Inline adapter tests plus `crates/dekopon-broker-host/tests/host.rs` authorization-boundary, Wasmtime, and loopback tests |
| Broker policy, evidence, and audit core | `crates/dekopon-broker/src/lib.rs` | Inline hash-chain/context tests plus `crates/dekopon-broker/tests/broker.rs` exact-policy and redaction tests |
| Immediate Wasmtime host | `crates/dekopon-provider-host/src/lib.rs`, `crates/dekopon-provider-host/wit/` | `crates/dekopon-provider-host/tests/host.rs` |
| Immediate runner, prompt loop, tracing | `crates/dekopon-run/src/` | `crates/dekopon-run/tests/cli.rs` |
| Shared internal fixtures | `crates/dekopon-testkit/` | `crates/dekopon-testkit/tests/` |
| Rust provider examples | `examples/providers/echo/`, `examples/providers/http-probe/` | Inline tests plus host/runner tests against the checked-in components |
| CI, dependency policy, release | `.github/workflows/`, `deny.toml`, `release.toml` | Required GitHub checks and `cargo package` |

Tests intentionally live beside the crate that owns the behavior. The top-level `tests/` directory is only a placeholder.

## Change maps

### Catalog resources or validation

Update protocol types first, then config validation, testkit builders, CLI rendering, examples, schemas, and docs as applicable. Authored fields are strict: unknown fields fail rather than being silently ignored. Parse config once; command handlers should consume typed resources, not YAML values.

### CLI behavior

Keep Clap syntax in `cli.rs`, execution separate from rendering, and process exits documented. Add parser tests and black-box tests. Machine-readable JSON/YAML shapes and exit codes need compatibility consideration even when table output can evolve.

`dekopon auth` does not load the catalog. `dekopon-run` consumes model credentials but does not own account-lifecycle commands.

### Model clients or prompt tools

Generic model types and transports belong in `dekopon-model`; the immediate bounded tool loop belongs in `dekopon-run`. Keep credentials inside the selected model client and out of providers and traces. Mock network protocols in tests; never read or import another application's credential store.

Provider JSON Schemas are exposed to the model, but there is no general JSON Schema validator in the host. The host requires an object-shaped schema and object invocation input; each provider must still validate its capability-specific fields and constraints.

### Provider contract or host

The SDK and host provider WIT files are mirrored and must remain byte-identical:

- `crates/dekopon-provider-sdk/wit/provider.wit`
- `crates/dekopon-provider-host/wit/provider.wit`

The buffered HTTP WIT package and guest/host copies are also mirrored:

- `wit/http/http.wit`
- `crates/dekopon-provider-http/wit/deps/http.wit`
- `crates/dekopon-broker-host/wit/deps/http.wit`
- `examples/providers/http-probe/wit/deps/http.wit`

The HTTP probe and broker host also mirror the provider package under `examples/providers/http-probe/wit/deps/provider.wit` and `crates/dekopon-broker-host/wit/deps/provider.wit`. Update all copies together and keep their equality checks passing. The SDK copy is the publication source for the `dekopon:provider@0.1.0` WIT package. That package contains the same `provider` world—exactly the `describe` and `invoke` exports and zero imports—and is stored at `ghcr.io/dekopon-agents/dekopon/provider:0.1.0`. Packaging this existing contract adds distribution, not guest authority: the immediate linker remains empty.

The root [`wkg.toml`](../wkg.toml) and [`wkg.lock`](../wkg.lock) retain the immutable provider package metadata and dependencies. [`../wit/http/wkg.toml`](../wit/http/wkg.toml) and [`../wit/http/wkg.lock`](../wit/http/wkg.lock) independently define the HTTP package. The shared [`wkg/config.toml`](../wkg/config.toml) maps the namespace to GHCR. The workflow publishes the import-free `dekopon:provider@0.1.0` world and the interface-only `dekopon:http@1.0.0` package independently. Published package versions are immutable. Change both mirrored WIT files and increment the WIT package version before publishing a changed contract; the publication workflow fetches an existing version and rejects different bytes.

Immediate providers must remain read-only and import-free; adding WASI or a host import there is an authority change, not a convenience refactor. `dekopon-broker-host` is the separate privileged adapter: it links only the project-owned HTTP interface, consumes `AuthorizedInvocation`, and maps WIT values to `dekopon-http-host`. The native engine consumes one exact HTTP grant beneath independent host ceilings, disables redirects, ambient proxies, and automatic decompression, validates and pins DNS results, and returns sanitized HTTP evidence metadata. Neither host authenticates callers, evaluates policy, constructs authorization, injects credentials, or writes audit records.

The checked-in components are generated:

| Source | Build script | Artifact |
|---|---|---|
| `examples/providers/echo/src/lib.rs` | `examples/providers/echo/build.sh` | `examples/providers/echo-provider.wasm` |
| `examples/providers/http-probe/src/lib.rs` | `examples/providers/http-probe/build.sh` | `examples/providers/http-probe-provider.wasm` |

Never edit `.wasm` files directly. Each source directory is a separate Cargo workspace with its own lockfile, so root workspace format, lint, and test commands do **not** cover it. The HTTP probe must decode to the two provider exports, exactly one `dekopon:http/client@1.0.0` import, and no WASI imports; the direct host test proves that the empty linker rejects it.

### Dependencies, crates, CI, or releases

Declare shared versions and path dependencies in the root `Cargo.toml`; commit `Cargo.lock`. New publishable crates also require release publication ordering, packaging validation, architecture/roadmap updates, and a meaningful tested responsibility. Keep justified duplicate dependency exceptions narrow in `deny.toml`.

GitHub Actions are pinned by full commit SHA. Required check names such as `test (Rust 1.86.0)` are branch-protection contexts: renaming a job without coordinating the repository setting leaves a permanently pending required check. Validate workflow and shell-script edits with `actionlint .github/workflows/*.yml` and `shellcheck <SCRIPT>` when those tools are available. Do not change branch protection, publish crates, create a release, or add credentials without explicit maintainer authorization.

## Runtime facts that are easy to miss

Immediate host:

- A `ProviderRegistry` retains compiled Wasmtime `Component` values for its lifetime. There is no cross-process or on-disk compilation cache.
- Every describe or invoke operation creates a fresh bounded store and component instance.
- One shared runtime mutex serializes immediate component execution; current calls are not parallel.
- The linker is empty: no WASI, filesystem, network, environment, clock, random, or credential imports reach a component.
- The host validates bounds, routing, read-only manifests, object-shaped inputs, and typed wire responses. Capability-specific argument validation remains provider-owned.
- Immediate provider output is raw JSON. It is not broker evidence, an `InvocationResult`, or an authorization receipt.
- Prompt-visible tool names are deterministic adaptations of capability IDs. Model tool selection and arguments remain untrusted.
- The prompt loop is bounded by `--max-steps` and at most 32 tool calls per model turn.

Privileged host foundation:

- `BufferedHttpClient` accepts a broker-produced `HttpConstraints` grant but performs no authorization transition itself.
- Grants can narrow but never widen native ceilings for HTTP call count, request bytes, response bytes, and headers.
- Native HTTP disables redirects, ambient proxies, and decompression; DNS results are checked and pinned before connection.
- `BrokerProviderRegistry` retains one async Wasmtime engine and compiled components, then creates a fresh bounded store and component instance for each description or invocation.
- Description uses a disabled HTTP context; any attempted host call rejects loading even if the guest catches the WIT error.
- Public execution consumes `AuthorizedInvocation`; policy rejections remain terminal after guest code returns.
- `dekopon-broker` validates exact trusted rules against loaded routes and host ceilings, reserves invocation IDs before policy evaluation, creates single-use authorization, and audits only metadata/digests.
- Its `AuthenticatedContext` is a transport input, not authentication; its replay ledger and in-memory hash chain do not survive process restart.
- The component host has no credential resolver and no workspace executable invokes the broker core yet. Direct `dekopon-run` remains on the independent empty-linker host.

See [`run.md`](run.md) for the user-facing contract and [`security-model.md`](security-model.md) for the trust boundary.

## Validation

Use `--locked` for reproducible validation. Start with `git diff --check`. Targeted checks are encouraged during development; run every relevant group before opening a PR.

### Root workspace

```console
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
cargo deny check
```

For MSRV-sensitive code or dependency changes:

```console
cargo +1.86.0 test --workspace --all-features --locked
```

For package metadata, include lists, or dependency-boundary changes, run from a clean tree:

```console
cargo package --workspace --exclude dekopon-testkit --locked
```

The immediate host, broker host, and broker-core packages intentionally exclude repository-only component integration fixtures, so Cargo may warn that `tests/host.rs` or `tests/broker.rs` is not included in the published package.

### Provider example workspaces

Run these commands for each affected provider manifest (`echo` and `http-probe`):

```console
cargo fmt --manifest-path examples/providers/<PROVIDER>/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/<PROVIDER>/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/<PROVIDER>/Cargo.toml
cargo check --locked --manifest-path examples/providers/<PROVIDER>/Cargo.toml --target wasm32-unknown-unknown
```

If provider source, SDK exports, WIT, or tool manifests change, install the pinned component tool, regenerate, validate, and exercise each affected checked-in artifact:

```console
cargo install wasm-tools --version 1.236.1 --locked
examples/providers/echo/build.sh
examples/providers/http-probe/build.sh
wasm-tools validate examples/providers/echo-provider.wasm
wasm-tools validate examples/providers/http-probe-provider.wasm
wasm-tools component wit examples/providers/http-probe-provider.wasm
cargo test -p dekopon-provider-host --test host --locked
cargo test -p dekopon-broker-host --locked
cargo test -p dekopon-broker --locked
cargo test -p dekopon-run --test cli --locked
```

A deterministic rebuild should leave the artifact unchanged when the source and toolchain are unchanged.

### Published WIT packages

Install the pinned package and component tools, then build and inspect the package from the repository root:

```console
cargo install wkg --version 0.16.0 --locked
cargo install wasm-tools --version 1.236.1 --locked
mkdir -p target/wit-package
wkg build \
  --wit-dir crates/dekopon-provider-sdk/wit \
  --output target/wit-package/dekopon-provider.wasm \
  --config wkg/config.toml
(
  cd wit/http
  wkg build \
    --wit-dir . \
    --output ../../target/wit-package/dekopon-http.wasm \
    --config ../../wkg/config.toml
)
wasm-tools validate target/wit-package/dekopon-provider.wasm
wasm-tools validate target/wit-package/dekopon-http.wasm
wasm-tools component wit target/wit-package/dekopon-provider.wasm
wasm-tools component wit target/wit-package/dekopon-http.wasm
```

The builds must leave both `wkg.lock` files unchanged. The decoded provider package must identify `dekopon:provider@0.1.0`, one `provider` world, two exports, and zero imports. The HTTP package must identify `dekopon:http@1.0.0`, one `client` interface with a single buffered `send` function, and no worlds. Exercise the configured fetch path with:

```console
wkg get \
  --config wkg/config.toml \
  --output target/wit-package/fetched-provider.wasm \
  dekopon:provider@0.1.0
wkg get \
  --config wkg/config.toml \
  --output target/wit-package/fetched-http.wasm \
  dekopon:http@1.0.0
```

`.github/workflows/wit-package.yml` performs local publish/fetch round trips for both packages on pull requests. When the relevant files reach `main`, it publishes the immutable packages to GHCR and verifies that fetching each package returns identical bytes.

## Before opening a pull request

- Rebase or branch from current `main`; do not stack accidentally on an already merged feature branch.
- Keep the diff scoped and preserve generated/source consistency.
- Update current-behavior docs in the same change; do not edit the roadmap as proof of implementation.
- Describe user-visible behavior, security implications, validation run, and known limitations.
- Use a conventional commit subject where practical.
- Push the branch, open the PR, and verify the required checks rather than assuming local success implies remote success.
