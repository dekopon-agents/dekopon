# Dekopon

Dekopon is a capability-oriented control plane for self-hosted AI agents. The initial `0.1.0` release provides a declarative local agent catalog, a kubectl-inspired operator and model-auth CLI, and an experimental immediate-mode runner for developing read-only WebAssembly providers. Future releases will add a separately deployed agent runtime and authorization broker.

> **Status:** early and not production-ready. `dekopon` manages the local catalog and model-account login only. `dekopon-run` can call an operator-selected model, execute import-free read-only components, or explicitly submit identity-free proposals as an unprivileged broker client; it has no broker authority or provider credentials. The separate Unix-only `dekopon-brokerd` executable authenticates one owner-UID trust domain, enforces exact policy, and invokes privileged providers. The `dekopon` operator CLI is not broker-integrated.

## Design documentation

Start with [`docs/design.md`](docs/design.md) for the product model, authority flow, component boundaries, and accepted decisions. [`docs/development.md`](docs/development.md) maps source, tests, generated artifacts, separate workspaces, and validation. [`docs/README.md`](docs/README.md) provides task-based reading paths; repository-wide agent instructions live in [`AGENTS.md`](AGENTS.md).

## What works today

- Strict YAML and JSON resources for agents, capabilities, and providers.
- Cross-reference validation with duplicate and unknown-field detection.
- A local, deterministic `dekopon` operator CLI with catalog commands, model-account authentication, and table, wide, JSON, YAML, and name output.
- Strongly typed identifiers and an invocation typestate that distinguishes proposals from broker authorization.
- A realistic local GitHub catalog with no embedded credentials.
- A Rust provider SDK plus a bounded Wasmtime component host with a fresh store per call.
- A published buffered `dekopon:http@1.0.0` contract, guest Rust facade, bounded native HTTP engine, asynchronous broker component host, deny-by-default authorization/evidence/audit core, and bounded identity-free Unix protocol.
- A separately deployed `dekopon-brokerd` that owns a private Unix socket, derives trusted context from peer UID mapping, restores replay state from verified durable audit, atomically checkpoints the count/head and rejects rollback relative to retained local state, and drains bounded connections on shutdown.
- A checked-in JSONPlaceholder broker provider with separately authorized post-read and external-write capabilities; all automated network tests use loopback mocks.
- `dekopon-run` direct invocation, an OpenAI-compatible or ChatGPT-subscription prompt loop offering a single sandboxed scripting tool, local Chrome traces, correlated OTLP/gRPC traces and audit-safe lifecycle logs, and explicit bounded broker capability/invocation client commands.
- A sandboxed bash-flavored script interpreter (`dekopon-shell`) whose command words dispatch to provider capabilities instead of operating-system processes. `dekopon-run shell` runs one script by hand and `dekopon-run prompt` hands the same interpreter to a model as its only tool, so a multi-step plan is one tool call rather than many round trips.

## What does not work yet

There is no unprivileged agent daemon, credential broker service, independently retained/signed/remote audit checkpoint service, task store, agent memory, or operator-CLI integration with the broker. Catalog provider and status resources remain declarations only. The immediate host exposes no WASI or custom imports and rejects every mutating capability, so it cannot read GitHub or post the review comment represented by the catalog example.

## Install

The workspace uses stable Rust (MSRV 1.86.0, edition 2024):

```console
git clone https://github.com/dekopon-agents/dekopon.git
cd dekopon
cargo install --locked --path crates/dekopon
cargo install --locked --path crates/dekopon-run
cargo install --locked --path crates/dekopon-brokerd
dekopon version
dekopon-run --version
```

The `0.1.0` crates are published. The workspace now targets the `0.2.0` development line; install from the repository as shown above until that release is cut.

`dekopon-brokerd` requires an owner-controlled strict configuration, private socket/audit/checkpoint directories, and pinned provider component paths:

```console
dekopon-brokerd --config /path/to/broker.yaml
```

See [`crates/dekopon-brokerd/README.md`](crates/dekopon-brokerd/README.md) before enabling this privileged process. Direct `inspect`, `invoke`, and `prompt` never connect to it; only explicit `dekopon-run broker ...` commands do.

## Run the example

```console
dekopon --config examples/local/dekopon.yaml get agents
dekopon --config examples/local/dekopon.yaml get agents -o wide
dekopon --config examples/local/dekopon.yaml get agent reviewer -o yaml
dekopon --config examples/local/dekopon.yaml get capabilities -o name
dekopon --config examples/local/dekopon.yaml get providers
dekopon --config examples/local/dekopon.yaml describe agent reviewer
dekopon --config examples/local/dekopon.yaml validate
dekopon --config examples/local/dekopon.yaml config view -o json
```

The `reviewer` may read pull requests and may propose a review comment only through the explicit `github.pull-request.comment` external-write capability. It has no pull-request approval capability. The disabled `snooper` has one read-only repository capability.

See [`docs/cli.md`](docs/cli.md) for discovery precedence, formats, and exit codes.

## Run a Rust provider immediately

The checked-in component is generated from [`examples/providers/echo/src/lib.rs`](examples/providers/echo/src/lib.rs), which implements `dekopon_provider_sdk::Provider`:

```console
cargo run -p dekopon-run -- inspect \
  --provider examples/providers/echo-provider.wasm
cargo run -p dekopon-run -- invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.echo --input '{"message":"hello"}'
cargo run -p dekopon-run -- invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.ransom-case --input '{"message":"Hello, World!"}'
cargo run -p dekopon-run -- --trace trace.json invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.echo --input '{}'
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:7281 \
  cargo run -p dekopon-run -- invoke \
    --provider examples/providers/echo-provider.wasm \
    echo.echo --input '{}'
```

Prompt mode targets an OpenAI-compatible endpoint (defaulting to local Ollama at `http://127.0.0.1:11434/v1`) or uses the isolated ChatGPT/Codex device login managed by `dekopon auth chatgpt`. See [`docs/run.md`](docs/run.md) for subscription login, provider builds, prompt usage, limits, benchmarking, and authority restrictions.

## Script several capability calls as one plan

[`crates/dekopon-shell`](crates/dekopon-shell/README.md) is a sandboxed bash-flavored interpreter whose command words are capability invocations rather than operating-system processes, with `jq` and the usual text builtins alongside them. `dekopon-run shell` runs one script by hand:

```console
cargo run -p dekopon-run -- shell \
  --provider examples/providers/echo-provider.wasm \
  'echo.echo --message hi | jq -r .message'
```

Every variable is a JSON value, every bound is hand-built and configurable, and every dropped bash construct either fails by name or is documented as inert.

`dekopon-run prompt` hands that same interpreter to a model as its **only** tool, so one tool call carries a whole multi-step plan instead of one capability per turn, and the tool surface stays a single schema however many capabilities an operator grants. Adding `--broker` lets those scripts reach capabilities direct mode provably cannot — anything performing I/O — while the broker remains the sole authority over them.

## Security model

A model may propose an invocation, but only the broker may turn it into an authorized invocation. Proposals carry untrusted intent; authorization, provider credentials, privileged host I/O, evidence, and audit records belong to a separate boundary. Rust type visibility reinforces this distinction but never replaces process isolation, authentication, or policy enforcement. `dekopon-brokerd` establishes that context only from Unix peer credentials and an owner-controlled exact mapping; payloads cannot claim identity or authority. `dekopon-run` never creates or receives authorized invocations: direct mode executes only import-free components declaring `read-only`, while broker mode submits untrusted proposals and prints broker results.

Read [`docs/security-model.md`](docs/security-model.md) for trust assumptions and current limitations.

## Roadmap

The next architectural milestones are broker-owned credentials, independent checkpoint retention or signing, operator/agent integration, and a separate unprivileged `dekopond`. See [`docs/roadmap.md`](docs/roadmap.md); roadmap items are intentions, not shipped features.

## Organization and package names

[`dekopon-agents`](https://github.com/dekopon-agents) is the GitHub organization that hosts the project. **Dekopon** is the product, `dekopon` is the CLI binary and Cargo workspace, and `dekopon` is the intended primary crates.io package. Organization naming does not change the product or package name.

## Contributing and license

See [`CONTRIBUTING.md`](CONTRIBUTING.md), [`SECURITY.md`](SECURITY.md), and the [Code of Conduct](CODE_OF_CONDUCT.md). Dekopon is dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
