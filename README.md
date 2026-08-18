# Dekopon

Dekopon is a capability-oriented control plane for self-hosted AI agents. **Version 0.3.0** pairs a declarative local agent catalog with a one-tool model runner, a JSON-native sandboxed scripting language, isolated WebAssembly providers, a separately deployed authorization broker, an unprivileged chat gateway, durable hash-linked audit, and correlated OpenTelemetry traces and logs.

> **Status:** this tree is a substantial, testable foundation, but it is not production-ready. `dekopon` manages the local catalog and model-account login. `dekopon-run` can call an operator-selected model, execute import-free read-only components, or submit identity-free proposals as an unprivileged broker client; it has no broker authority or provider credentials. The separate Unix-only `dekopon-brokerd` executable authenticates one owner-UID trust domain, evaluates a deny-by-default Cedar policy set against owner-authored execution constraints, resolves destination-bound provider credentials, invokes constrained providers, and records durable audit. The Unix-only `dekopond` daemon connects to chat services and routes messages to catalog agents, holding chat and model credentials but no broker authority. The operator CLI is integrated with neither.

## Design documentation

Start with [`docs/design.md`](docs/design.md) for the product model, authority flow, component boundaries, and accepted decisions. [`docs/development.md`](docs/development.md) maps source, tests, generated artifacts, separate workspaces, and validation. [`docs/README.md`](docs/README.md) provides task-based reading paths; repository-wide agent instructions live in [`AGENTS.md`](AGENTS.md).

## What works today in 0.3.0

- Strict YAML and JSON resources for agents, capabilities, and providers.
- Cross-reference validation with duplicate and unknown-field detection.
- A local, deterministic `dekopon` operator CLI with catalog commands, model-account authentication, and table, wide, JSON, YAML, and name output.
- Strongly typed identifiers and an invocation typestate that distinguishes proposals from broker authorization.
- A realistic local GitHub catalog with no embedded credentials.
- A Rust provider SDK plus a bounded Wasmtime component host with a fresh store per call.
- A published buffered `dekopon:http@1.0.0` contract, guest Rust facade, bounded native HTTP engine, asynchronous broker component host, deny-by-default authorization/evidence/audit core, and bounded identity-free Unix protocol.
- A separately deployed `dekopon-brokerd` that owns a private Unix socket, derives trusted context from peer UID mapping, restores replay state from verified durable audit, atomically checkpoints the count/head and rejects rollback relative to retained local state, and drains bounded connections on shutdown.
- A checked-in JSONPlaceholder broker provider with separately authorized post-read and external-write capabilities; all automated network tests use loopback mocks.
- `dekopon-run` direct invocation, an OpenAI-compatible or ChatGPT-subscription prompt loop offering a single sandboxed scripting tool, local Chrome traces, correlated OTLP/HTTP traces and audit-safe lifecycle logs, and explicit bounded broker capability/invocation client commands.
- A sandboxed bash-flavored script interpreter (`dekopon-shell`) whose command words dispatch to provider capabilities instead of operating-system processes. `dekopon-run shell` runs one script by hand and `dekopon-run prompt` hands the same interpreter to a model as its only tool, so a multi-step plan is one tool call rather than many round trips.

New in 0.3.0:

- **Cedar authorization** (`dekopon-policy`). A schema generated from the deployment's declared world, strict startup validation, deny on any evaluation error, the determining `policy_ids` and a `policy_digest` in every audit record. It replaced the exact-match evaluator outright. Execution bounds stayed outside the policy language as owner-authored constraint sets, so a policy edit can broaden who may act and can never widen how far an action reaches.
- **Broker-owned credentials.** Destination-bound secrets in a separate stricter owner-only file, bound per capability constraint set with optional per-agent overrides, injected inside the native HTTP engine after guest headers were validated. Audit and evidence record `credentialInjected: true` and never a value.
- **Identity and attestation.** Canonical external subjects, owner-controlled subject-to-principal mappings, per-peer attestor grants, and `via`-scoped policy that keeps attested and direct authority disjoint — adding a gateway cannot widen a grant that already existed.
- **`dekopond`**, the unprivileged chat gateway: Slack Socket Mode, Telegram long-poll, and local development transports; configuration naming environment variables rather than secrets; first-match routing to catalog agents, including routes that match any channel the bot is summoned in; admission-bounded sessions; and attested on-behalf-of proposals, so the broker's audit attributes an effect to the person who asked for it.
- **Bounded conversation memory.** A gateway session keeps a bounded per-sender window of earlier turns and replays it into the next prompt, under a first-class per-transport conversation identity and a minted per-conversation prompt cache key that is never derived from message content.
- **`dekopon-agent`**, the shared bounded prompt loop and session capability dispatch consumed by both `dekopon-run` and `dekopond`.
- **A `gh` provider and a `gh` shell builtin.** Nineteen narrow GitHub capabilities in one checked-in component — SHA-pinned writes, no `gh.api.*` passthrough — reachable as `gh pr view 7 -R owner/repo` inside a sandboxed script.
- **`dekopon-run chat`**, a client for the gateway's local development transport, so a gateway route can be exercised without a chat service.
- **[`examples/rubber-stamper`](examples/rubber-stamper/README.md)**, the end-to-end walkthrough assembling all of it, pinned against the real machinery by three integration test suites.
- Three new publishable crates (`dekopon-agent`, `dekopon-policy`, `dekopond`) bring the release to 20, up from the 17 published at `0.2.0`.

## What does not work yet

Agent memory that outlives a conversation. A gateway session replays a bounded per-sender window of earlier turns; nothing carries across conversations, and there is no task store. `dekopond` also runs under the same UID as the broker, so its attestor grant buys attribution and deny-by-default scoping rather than isolation; a dedicated gateway UID, where `via` becomes real separation, remains committed direction. See [`docs/dekopond.md`](docs/dekopond.md).

There is still no independently retained/signed/remote audit checkpoint service and no operator-CLI integration with the broker or the daemon — `dekopon` reads the catalog and nothing else. Catalog provider and status resources remain declarations only. The immediate `dekopon-run` host exposes no WASI or custom imports and rejects every mutating capability, so it cannot read GitHub or post the review comment represented by the catalog example; only the broker can.

## Install

The 20 public crates that make up `0.3.0` are published on [crates.io](https://crates.io/crates/dekopon), three of them — `dekopon-agent`, `dekopon-policy`, and `dekopond` — for the first time. With stable Rust (MSRV 1.89.0, edition 2024), install the four released executables directly:

```console
cargo install --locked dekopon --version 0.3.0
cargo install --locked dekopon-run --version 0.3.0
cargo install --locked dekopon-brokerd --version 0.3.0
cargo install --locked dekopond --version 0.3.0
dekopon version
dekopon-run --version
```

Prebuilt, provenance-attested archives for Linux and macOS on x86-64 and ARM64 are attached to the [v0.3.0 GitHub release](https://github.com/dekopon-agents/dekopon/releases/tag/v0.3.0). Each carries all four executables alongside the broker and gateway configuration contracts. To develop from a checkout instead:

```console
git clone https://github.com/dekopon-agents/dekopon.git
cd dekopon
cargo install --locked --path crates/dekopon
cargo install --locked --path crates/dekopon-run
cargo install --locked --path crates/dekopon-brokerd
cargo install --locked --path crates/dekopond
```

`dekopon-brokerd` requires an owner-controlled strict configuration, private socket/audit/checkpoint directories, and pinned provider component paths:

```console
dekopon-brokerd --config /path/to/broker.yaml
```

See [`crates/dekopon-brokerd/README.md`](crates/dekopon-brokerd/README.md) before enabling this privileged process. Direct `inspect`, `invoke`, and `prompt` never connect to it; only explicit `dekopon-run broker ...` commands do.

## Run the flagship example

[`examples/rubber-stamper`](examples/rubber-stamper/README.md) is the whole system in one deployment: a boss sends a Slack DM, the gateway attests to the sender's identity and decides nothing, the broker maps that identity to a principal, checks Cedar policy, injects a GitHub token bound to `api.github.com`, runs the `gh` component, and hash-links an audit record naming the person who asked. The token is never visible to the model, the shell session, or the component that uses it. The walkthrough is a complete, internally consistent configuration set — catalog, broker configuration, Cedar policy, credentials template, gateway configuration — pinned against the real machinery by `crates/dekopon-brokerd/tests/examples.rs`.

## Run the catalog example

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

The `reviewer` may read pull requests and may propose a review comment only through the explicit `github.pull-request.comment` external-write capability. It has no pull-request approval capability — and that contrast is now load-bearing rather than illustrative: the rubber-stamper example above holds `gh.pull-request.approve` and this one deliberately does not, because approval is a separately named capability with its own policy statement rather than a stronger grade of "write". The disabled `snooper` has one read-only repository capability.

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
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:5080/api/default \
OTEL_EXPORTER_OTLP_HEADERS='Authorization=Basic%20<INGESTION_TOKEN>,organization=default,stream-name=dekopon' \
  cargo run -p dekopon-run -- invoke \
    --provider examples/providers/echo-provider.wasm \
    echo.echo --input '{}'
```

[`examples/otel-traces`](examples/otel-traces/README.md) provides a one-container OpenObserve receiver, UI walkthrough, and automated smoke test.

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

A model may propose an invocation, but only the broker may turn it into an authorized invocation. Proposals carry untrusted intent; authorization, provider credentials, privileged host I/O, evidence, and audit records belong to a separate boundary. Rust type visibility reinforces this distinction but never replaces process isolation, authentication, or policy enforcement. `dekopon-brokerd` establishes that context only from Unix peer credentials and an owner-controlled exact mapping; payloads cannot claim identity or authority. Its authorization decisions come from Cedar and its execution bounds from a separate owner-authored constraint catalog, so a policy edit can broaden who may act and can never widen how far an action reaches. `dekopon-run` never creates or receives authorized invocations: direct mode executes only import-free components declaring `read-only`, while broker mode submits untrusted proposals and prints broker results.

Read [`docs/security-model.md`](docs/security-model.md) for trust assumptions and current limitations.

## Roadmap

The next architectural milestones are independent checkpoint retention or signing, operator-CLI integration with the broker and the daemon, a dedicated gateway UID, and agent memory that outlives a conversation. Broker-owned credentials, Cedar, identity/attestation, the unprivileged `dekopond`, and its bounded per-sender conversation history shipped in 0.3.0. See [`docs/roadmap.md`](docs/roadmap.md); roadmap items are intentions, not shipped features.

## Maintainer release process

Releases deliberately separate preparation, GitHub artifacts, and crates.io publication:

1. Start from a clean, current `main`. Update release-facing status/install text in the root and crate READMEs before tagging—the packaged README is immutable on crates.io. Run the full validation matrix in [`docs/development.md`](docs/development.md), including `cargo package --workspace --exclude dekopon-testkit --locked`.
2. Use `cargo release <VERSION>` to preview the shared-version commit and tag, then `cargo release <VERSION> --execute` after review. [`release.toml`](release.toml) creates the commit and tag but intentionally does not push or publish anything.
3. Land the version commit on `main`, then push the matching `v<VERSION>` tag. The `Release` workflow verifies tag/version alignment, formatting, clippy, tests, rustdoc, package contents, dependency policy, and the runner privilege boundary. It builds three CLI archives, attests them, and creates the GitHub release.
4. A tag push **does not publish crates**. Ensure every public package has the crates.io GitHub trusted publisher `dekopon-agents/dekopon`, workflow `release.yml`, environment `crates-io`; a brand-new crate name must be bootstrapped with an explicitly authorized scoped credential, then registered immediately. Dispatch the same `Release` workflow with the existing tag and `publish_to_crates=true`, then approve the protected environment. The job obtains a short-lived OIDC token and publishes every public crate in checked dependency order. It skips an immutable version already present, making a partially completed publication recoverable; an explicit crates.io new-package `429` waits until the server's retry time, while every other publication failure stops the job.
5. Verify the GitHub release, every crates.io package version, and fresh `cargo install --locked ... --version <VERSION>` commands before announcing the release.

The dependency-ordered crate list lives in [`.github/workflows/release.yml`](.github/workflows/release.yml). Release validation fails if that list omits a publishable workspace crate, includes a private/unknown crate, contains duplicates, or places a dependent before its dependency. Never move an existing tag or attempt to overwrite a published crate version; fix release automation on `main` and cut a new patch version when published bytes must change.

## Organization and package names

[`dekopon-agents`](https://github.com/dekopon-agents) is the GitHub organization that hosts the project. **Dekopon** is the product, `dekopon` is the CLI binary and Cargo workspace, and `dekopon` is the intended primary crates.io package. Organization naming does not change the product or package name.

## Contributing and license

See [`CONTRIBUTING.md`](CONTRIBUTING.md), [`SECURITY.md`](SECURITY.md), and the [Code of Conduct](CODE_OF_CONDUCT.md). Dekopon is dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
