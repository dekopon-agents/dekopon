# Contributing to Dekopon

Dekopon is early-stage security infrastructure. Small, reviewable changes with explicit trust assumptions are preferred over speculative framework code.

Read [`docs/design.md`](docs/design.md) before changing behavior or architecture. Then read [`docs/development.md`](docs/development.md) for the repository map, generated artifacts, separate provider workspace, validation matrix, and PR workflow. Area-specific contracts are indexed in [`docs/README.md`](docs/README.md).

## Development setup

Install stable Rust with `rustfmt` and Clippy. The workspace MSRV is 1.89.0.

```console
rustup component add rustfmt clippy
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
```

Install `cargo-deny` to run the dependency policy:

```console
cargo install cargo-deny --locked
cargo deny check
```

Run `cargo +1.89.0 test --workspace --all-features --locked` for MSRV-sensitive changes. Run `cargo package --workspace --locked` from a clean tree when changing package metadata, crate dependencies, or include lists.

## Exercise changed behavior

Exercise the affected executable before submitting a CLI, config, or provider-host change:

```console
ci/fetch-external-provider-components.sh examples/providers
cargo run -p dekopon -- --config examples/local/dekopon.yaml validate
cargo run -p dekopon -- --config examples/local/dekopon.yaml get agents
cargo run -p dekopon-run -- inspect --provider examples/providers/echo-provider.wasm
cargo run -p dekopon-run -- invoke --provider examples/providers/echo-provider.wasm echo.echo --input '{}'
```

Repository-owned fixtures under `examples/providers/` are excluded from the root workspace and have their own lockfiles. Provider changes require separate format, Clippy, test, and `wasm32-unknown-unknown` checks for every affected in-tree fixture. If fixture source or the guest contract changes, regenerate—not hand-edit—its checked component. Echo, JSONPlaceholder, and memory-chat are standalone repositories; core tests fetch their exact v0.1.0 release bytes with `ci/fetch-external-provider-components.sh`. Storage work must additionally compare the remaining in-tree `wit/storage/storage.wit` mirrors and inspect generated imports: fetched memory is JSONL-only, the probe is durable-files-only, and neither may import WASI. Runner OTLP, OpenObserve example, or observability CI changes require `examples/otel-traces/smoke-test.sh` against its disposable single-container stack. Exact commands are in [`docs/development.md`](docs/development.md).

## Change guidelines

- Open an issue or draft pull request before a large architectural change.
- Keep model proposals, broker authorization, storage grants, and effect execution distinct in APIs and documentation.
- Do not commit credentials, real private endpoints, generated coverage data, or local configuration.
- Reject unknown authored fields unless a documented compatibility need overrides that default.
- Treat model tool arguments and provider responses as untrusted; providers validate their capability-specific input.
- Add behavior-focused tests, including failure paths and stable CLI output where relevant.
- Avoid `unsafe`, panics on user input, unnecessary async dependencies, and public APIs based on `anyhow`.
- Use conventional commit subjects when practical, for example `feat(config): detect duplicate agents`.

## Pull requests

Describe the user-visible behavior, security implications, checks run, and remaining limitations. Pull requests require CI and human review. Automated agents must not approve their own changes. Verify required checks after pushing; do not infer remote success from local validation.

Security vulnerabilities should follow [`SECURITY.md`](SECURITY.md), not the public issue tracker.
