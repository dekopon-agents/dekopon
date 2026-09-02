# Contributing to Dekopon

Dekopon is early-stage security infrastructure. Small, reviewable changes with explicit trust assumptions are preferred over speculative framework code.

Read [`docs/design.md`](docs/design.md) before changing behavior or architecture. Then read [`docs/development.md`](docs/development.md) for the repository map, generated artifacts, separate provider workspace, validation matrix, and PR workflow. Area-specific contracts are indexed in [`docs/README.md`](docs/README.md).

## Development setup

Install stable Rust with `rustfmt` and Clippy. The workspace MSRV is 1.89.0.

```console
rustup component add rustfmt clippy
cargo install cargo-machete --version 0.9.2 --locked
ci/fetch-external-provider-components.sh examples/providers
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
cargo check -p dekopon-core -p dekopon-capability -p dekopon-protocol --locked
cargo machete
```

The fetch installs the ignored echo, JSONPlaceholder, and memory-chat fixtures that core tests read. The `cargo check` compiles the foundational crates with their opt-in `schemars` feature off, and `cargo machete` (0.9.2 is the version CI pins) detects unused dependencies. The complete gate list is in [Root workspace](docs/development.md#root-workspace).

Install `cargo-deny` to run the dependency policy:

```console
cargo install cargo-deny --locked
cargo deny --all-features check
```

For MSRV-sensitive changes run the two `cargo +1.89.0` commands in [Root workspace](docs/development.md#root-workspace) (`--no-run`, then `--doc`), which is what the required `test (Rust 1.89.0)` check runs. Run `cargo package --workspace --locked` from a clean tree when changing package metadata, crate dependencies, or include lists. Validate workflow and shell-script edits with `actionlint .github/workflows/*.yml` and `shellcheck <SCRIPT>`; CI runs shellcheck over the repository scripts (the exact file list is the `shellcheck` line in [Root workspace](docs/development.md#root-workspace)).

Documentation edits are gated too: run the duplicate-entry check below, and add every new `audit.event` name, backticked, to [`docs/observability.md`](docs/observability.md) in the same change ([details](docs/development.md#documentation-gates)).

```console
python3 .github/scripts/check_docs_duplicates.py docs README.md AGENTS.md crates/*/README.md
```

## Exercise changed behavior

Exercise the affected executable before submitting a CLI, config, or provider-host change:

```console
ci/fetch-external-provider-components.sh examples/providers
cargo run -p dekopon -- --config examples/local/dekopon.yaml validate
cargo run -p dekopon -- --config examples/local/dekopon.yaml get agents
cargo run -p dekopon-run -- inspect --provider examples/providers/echo-provider.wasm
cargo run -p dekopon-run -- invoke --provider examples/providers/echo-provider.wasm echo.echo --input '{}'
```

The fixtures under `examples/providers/` are separate Cargo workspaces that root commands do not cover. They, the WIT mirrors, generated `.wasm` files, and the OpenObserve smoke test have their own validation rules: run the commands in [Provider example workspaces](docs/development.md#provider-example-workspaces) for every affected fixture, and [OpenObserve OTLP end-to-end test](docs/development.md#openobserve-otlp-end-to-end-test) for runner telemetry, OpenObserve example, or observability CI changes.

## Change guidelines

- Open an issue or draft pull request before a large architectural change.
- Keep model proposals, broker authorization, storage grants, and effect execution distinct in APIs and documentation.
- Do not commit credentials, real private endpoints, generated coverage data, or local configuration.
- Reject unknown authored fields unless a documented compatibility need overrides that default.
- Treat model tool arguments and provider responses as untrusted; providers validate their capability-specific input.
- Add behavior-focused tests, including failure paths and stable CLI output where relevant.
- Record user-visible changes under `[Unreleased]` in [`CHANGELOG.md`](CHANGELOG.md) using Keep a Changelog categories; pull-request CI validates the file's shape ([details](docs/development.md#dependencies-crates-ci-or-releases)).
- Avoid `unsafe`, panics on user input, unnecessary async dependencies, and public APIs based on `anyhow`.
- Use conventional commit subjects when practical, for example `feat(config): detect duplicate agents`.

## Pull requests

Pull requests require CI and human review. Automated agents must not approve their own changes. Follow the checklist in [Before opening a pull request](docs/development.md#before-opening-a-pull-request).

Security vulnerabilities should follow [`SECURITY.md`](SECURITY.md), not the public issue tracker.
