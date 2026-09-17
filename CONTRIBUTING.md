# Contributing to Dekopon

A change is judged by the three goals in the [constitution](docs/design.md#constitution); a subsystem that serves none of them is a deletion candidate. Small, reviewable changes with explicit trust assumptions beat speculative framework code.

For behavior or architecture changes, read the relevant sections of [`docs/design.md`](docs/design.md). Select the applicable [change map](docs/development.md#change-maps) and [validation group](docs/development.md#validation), using the [repository map](docs/development.md#repository-map) to locate source and tests. Other contracts are routed by the [area index](docs/README.md#change-a-specific-area); reading every manual is not a prerequisite.

## Development setup

Install `rustup`. [`rust-toolchain.toml`](rust-toolchain.toml) selects the pinned compiler with `rustfmt` and Clippy, and that compiler is also the workspace MSRV.

```console
rustup component add rustfmt clippy
cargo install cargo-machete --version 0.9.2 --locked
ci/fetch-external-provider-components.sh examples/providers
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
cargo machete
```

The fetch installs the ignored echo, JSONPlaceholder, and memory-chat fixtures that core tests read. The complete gate list is in [Root workspace](docs/development.md#root-workspace).

Install `cargo-deny` to run the dependency policy:

```console
cargo install cargo-deny --locked
cargo deny --all-features check
```

The pinned compiler is the MSRV, so the required `test (Rust)` check is the compile/run/doctest sequence in [Root workspace](docs/development.md#root-workspace) and there is no second MSRV run. Run `cargo package --workspace --locked` from a clean tree when changing package metadata, crate dependencies, or include lists. Validate workflow and shell-script edits with `actionlint .github/workflows/*.yml` and `shellcheck <SCRIPT>`; the exact file list CI shellchecks is the `shellcheck` line in [Root workspace](docs/development.md#root-workspace).

Documentation edits are gated too: run the duplicate-entry check below, and add every new `audit.event` name, backticked, to [`docs/observability.md`](docs/observability.md) in the same change ([details](docs/development.md#documentation-gates)).

```console
python3 .github/scripts/check_docs_duplicates.py docs README.md AGENTS.md crates/*/README.md
```

## Exercise changed behavior

Exercise the affected executable before submitting a CLI, config, or provider-host change:

```console
ci/fetch-external-provider-components.sh examples/providers
cargo test -p dekopon-config --test examples --locked
cargo run -p dekopond -- auth chatgpt --help
```

The fixtures under `examples/providers/` are separate Cargo workspaces that root commands do not cover. They, the WIT mirrors, generated `.wasm` files, and the OpenObserve smoke test have their own validation rules: run the commands in [Provider example workspaces](docs/development.md#provider-example-workspaces) for every affected fixture, and [OpenObserve OTLP end-to-end test](docs/development.md#openobserve-otlp-end-to-end-test) for daemon telemetry, OpenObserve example, or observability CI changes.

## Change guidelines

- Open an issue or draft pull request before a large architectural change.
- Keep model proposals, broker authorization, storage grants, and effect execution distinct in APIs and documentation.
- Do not commit credentials, real private endpoints, local paths, generated coverage data, or local configuration.
- Reject unknown authored fields unless a documented compatibility need overrides that default.
- Treat model tool arguments and provider responses as untrusted; providers validate their capability-specific input.
- Name tests for the behavior they pin and keep them beside the owning crate. Failure-path tests assert the surfaced error or log carries the cause; validation tests construct at least two simultaneous conflicts and assert both are reported. Mock network peers on loopback; never read another application's credential store. Cover stable CLI output where relevant.
- Record user-visible changes under `[Unreleased]` in [`CHANGELOG.md`](CHANGELOG.md) using Keep a Changelog categories; pull-request CI validates the file's shape ([details](docs/development.md#dependencies-crates-ci-or-releases)).
- Avoid `unsafe`, panics on user input, unnecessary async dependencies, and public APIs based on `anyhow`.
- Use conventional commit subjects when practical, for example `feat(config): detect duplicate agents`. Preserve `Co-Authored-By:` and `Claude-Session:` trailers added by the agent harness; model and session identifiers belong nowhere else in source or documentation.

## Review checklist

These recurring failure patterns are review requirements, not just lint suggestions:

- Preserve error causes in a returned error naming the failed check or a tracing event at the discard site carrying the cause kind or errno. This includes `map_err(|_| …)`, `let _ = fallible()`, and bool/Option results from multi-cause checks. Emit every refusal or failure cause once.
- Classify errors on the axis callers act on: retryable versus permanent and executed versus not-executed. Never report permanent exhaustion as transient, completed work as timed out, or exit successfully with daemon work dead.
- Report every validation conflict together, then fail; never stop at the first conflict or use last-wins duplicate keys.
- Never hold `Entered`/`EnteredSpan` guards across `.await`; use `.instrument(span)` or `in_scope`.
- Bound everything that grows or blocks and give it an owner. Enforce peer-claimed lengths rather than preallocating from them; deduplicate or evict state retained across turns; give spawned threads, connections, and network reads deadlines and exit observers.
- Construct expensive HTTP/model clients, Wasmtime engines, linkers, compiled components, and workers once at process or session scope, not per request or invocation.
- Every new public item, dependency, config field, and error variant needs a non-test consumer in the same PR; otherwise make it private or delete it. Parsed-but-unread config and unreachable variants are not scaffolding to retain.
- Keep one definition per fact. A validator or constant mirroring an authority must share the definition or carry an equality-pinning test; a mirror must not accept what the authority rejects.
- Preserve the lints defined in [`Cargo.toml`](Cargo.toml) and [`clippy.toml`](clippy.toml), including the bans on `dbg!`, `todo!`, and `unimplemented!`. Any justified allowance is site-scoped with a reason explaining why it is safe, never widened to a module or crate.

## Pull requests

Pull requests require CI and human review. Automated agents must not approve their own changes. Follow the checklist in [Before opening a pull request](docs/development.md#before-opening-a-pull-request).

Security vulnerabilities follow [`SECURITY.md`](SECURITY.md), not the public issue tracker.
