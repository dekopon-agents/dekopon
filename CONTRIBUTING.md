# Contributing to Dekopon

Dekopon is early-stage security infrastructure. Small, reviewable changes with explicit trust assumptions are preferred over speculative framework code.

## Development setup

Install stable Rust with `rustfmt` and Clippy. The workspace MSRV is 1.85.0.

```console
rustup component add rustfmt clippy
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo doc --workspace --all-features --no-deps
```

Install `cargo-deny` to run the dependency policy:

```console
cargo install cargo-deny --locked
cargo deny check
```

Exercise the CLI before submitting a CLI or config change:

```console
cargo run -p dekopon -- --config examples/local/dekopon.yaml validate
cargo run -p dekopon -- --config examples/local/dekopon.yaml get agents
```

## Change guidelines

- Open an issue or draft pull request before a large architectural change.
- Keep model proposals, broker authorization, and effect execution distinct in APIs and documentation.
- Do not commit credentials, real private endpoints, generated coverage data, or local configuration.
- Reject unknown authored fields unless a documented compatibility need overrides that default.
- Add behavior-focused tests, including failure paths and stable CLI output where relevant.
- Avoid `unsafe`, panics on user input, unnecessary async dependencies, and public APIs based on `anyhow`.
- Use conventional commit subjects when practical, for example `feat(config): detect duplicate agents`.

## Pull requests

Describe the user-visible behavior, security implications, checks run, and remaining limitations. Pull requests require CI and human review. Automated agents must not approve their own changes.

Security vulnerabilities should follow [`SECURITY.md`](SECURITY.md), not the public issue tracker.
