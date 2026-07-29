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
| Immediate Wasmtime host | `crates/dekopon-provider-host/src/lib.rs`, `crates/dekopon-provider-host/wit/` | `crates/dekopon-provider-host/tests/host.rs` |
| Immediate runner, prompt loop, tracing | `crates/dekopon-run/src/` | `crates/dekopon-run/tests/cli.rs` |
| Shared internal fixtures | `crates/dekopon-testkit/` | `crates/dekopon-testkit/tests/` |
| Rust provider example | `examples/providers/echo/src/lib.rs` | Inline tests plus `crates/dekopon-provider-host/tests/host.rs` and `crates/dekopon-run/tests/cli.rs` against the checked-in component |
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

The SDK and host WIT files are mirrored and must remain byte-identical:

- `crates/dekopon-provider-sdk/wit/provider.wit`
- `crates/dekopon-provider-host/wit/provider.wit`

Update both copies together and keep `host_and_guest_sdk_use_the_same_wit_contract` passing. The SDK copy is the publication source for the `dekopon:provider@0.1.0` WIT package. That package contains the same `provider` world—exactly the `describe` and `invoke` exports and zero imports—and is stored at `ghcr.io/dekopon-agents/dekopon/provider:0.1.0`. Packaging this existing contract adds distribution, not guest authority: the immediate linker remains empty.

The root [`wkg.toml`](../wkg.toml), [`wkg.lock`](../wkg.lock), and [`wkg/config.toml`](../wkg/config.toml) define package metadata, dependency resolution, and the namespace-to-GHCR mapping. Published package versions are immutable. Change both mirrored WIT files and increment the WIT package version before publishing a changed contract; the publication workflow fetches an existing version and rejects different bytes.

Immediate providers must remain read-only and import-free; adding WASI or a host import is an authority change, not a convenience refactor.

The checked-in component is generated:

- source: `examples/providers/echo/src/lib.rs`
- build script: `examples/providers/echo/build.sh`
- artifact: `examples/providers/echo-provider.wasm`

Never edit the `.wasm` file directly. `examples/providers/echo` is a separate Cargo workspace with its own lockfile, so root workspace format, lint, and test commands do **not** cover it.

### Dependencies, crates, CI, or releases

Declare shared versions and path dependencies in the root `Cargo.toml`; commit `Cargo.lock`. New publishable crates also require release publication ordering, packaging validation, architecture/roadmap updates, and a meaningful tested responsibility. Keep justified duplicate dependency exceptions narrow in `deny.toml`.

GitHub Actions are pinned by full commit SHA. Required check names such as `test (Rust 1.86.0)` are branch-protection contexts: renaming a job without coordinating the repository setting leaves a permanently pending required check. Validate workflow and shell-script edits with `actionlint .github/workflows/*.yml` and `shellcheck <SCRIPT>` when those tools are available. Do not change branch protection, publish crates, create a release, or add credentials without explicit maintainer authorization.

## Runtime facts that are easy to miss

- A `ProviderRegistry` retains compiled Wasmtime `Component` values for its lifetime. There is no cross-process or on-disk compilation cache.
- Every describe or invoke operation creates a fresh bounded store and component instance.
- One shared runtime mutex serializes immediate component execution; current calls are not parallel.
- The linker is empty: no WASI, filesystem, network, environment, clock, random, or credential imports reach a component.
- The host validates bounds, routing, read-only manifests, object-shaped inputs, and typed wire responses. Capability-specific argument validation remains provider-owned.
- Immediate provider output is raw JSON. It is not broker evidence, an `InvocationResult`, or an authorization receipt.
- Prompt-visible tool names are deterministic adaptations of capability IDs. Model tool selection and arguments remain untrusted.
- The prompt loop is bounded by `--max-steps` and at most 32 tool calls per model turn.

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

The host package intentionally excludes its repository-only component integration fixture, so Cargo may warn that `tests/host.rs` is not included in the published package.

### Echo provider workspace

```console
cargo fmt --manifest-path examples/providers/echo/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/echo/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/echo/Cargo.toml
cargo check --locked --manifest-path examples/providers/echo/Cargo.toml --target wasm32-unknown-unknown
```

If provider source, SDK exports, WIT, or tool manifests change, install the pinned component tool, regenerate, validate, and exercise the checked-in artifact:

```console
cargo install wasm-tools --version 1.236.1 --locked
examples/providers/echo/build.sh
wasm-tools validate examples/providers/echo-provider.wasm
cargo test -p dekopon-provider-host --test host --locked
cargo test -p dekopon-run --test cli --locked
```

A deterministic rebuild should leave the artifact unchanged when the source and toolchain are unchanged.

### Published WIT package

Install the pinned package and component tools, then build and inspect the package from the repository root:

```console
cargo install wkg --version 0.16.0 --locked
cargo install wasm-tools --version 1.236.1 --locked
mkdir -p target/wit-package
wkg build \
  --wit-dir crates/dekopon-provider-sdk/wit \
  --output target/wit-package/dekopon-provider.wasm \
  --config wkg/config.toml
wasm-tools validate target/wit-package/dekopon-provider.wasm
wasm-tools component wit target/wit-package/dekopon-provider.wasm
```

The build must leave `wkg.lock` unchanged and the decoded package must identify `dekopon:provider@0.1.0`, one `provider` world, two exports, and zero imports. Exercise the configured fetch path with:

```console
wkg get \
  --config wkg/config.toml \
  --output target/wit-package/fetched-provider.wasm \
  dekopon:provider@0.1.0
```

`.github/workflows/wit-package.yml` performs a local publish/fetch round trip on pull requests. When the relevant files reach `main`, it publishes the immutable package to GHCR and verifies that fetching the same package returns identical bytes.

## Before opening a pull request

- Rebase or branch from current `main`; do not stack accidentally on an already merged feature branch.
- Keep the diff scoped and preserve generated/source consistency.
- Update current-behavior docs in the same change; do not edit the roadmap as proof of implementation.
- Describe user-visible behavior, security implications, validation run, and known limitations.
- Use a conventional commit subject where practical.
- Push the branch, open the PR, and verify the required checks rather than assuming local success implies remote success.
