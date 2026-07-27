# Dekopon

Dekopon is a capability-oriented control plane for self-hosted AI agents. The initial `0.1.0` release provides a declarative local agent catalog and a kubectl-inspired CLI. Future releases will add a separately deployed agent runtime, authorization broker, and sandboxed capability providers.

> **Status:** early and not production-ready. The CLI does not yet run models or execute tools.

## Design documentation

Start with [`docs/design.md`](docs/design.md) for the product model, authority flow, component boundaries, and accepted decisions. [`docs/README.md`](docs/README.md) provides task-based reading paths for humans and coding agents; repository-wide agent instructions live in [`AGENTS.md`](AGENTS.md).

## What works today

- Strict YAML and JSON resources for agents, capabilities, and providers.
- Cross-reference validation with duplicate and unknown-field detection.
- A local, deterministic `dekopon` operator CLI with table, wide, JSON, YAML, and name output.
- Strongly typed identifiers and an invocation typestate that distinguishes proposals from broker authorization.
- A realistic local GitHub catalog with no embedded credentials.

## What does not work yet

There is no daemon, model integration, network API, policy engine, credential broker, provider execution, Wasm host, task store, or agent memory in `0.1.0`. Provider and status resources are declarations only. In particular, this release cannot post the review comment represented by the example capability.

## Install

The workspace uses stable Rust (MSRV 1.85.0, edition 2024):

```console
git clone https://github.com/dekopon-agents/dekopon.git
cd dekopon
cargo install --locked --path crates/dekopon
dekopon version
```

The crates.io release is prepared but publication requires explicit maintainer authorization. Until it is published, install from the repository as shown above.

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

## Security model

A model may propose an invocation, but only the broker may turn it into an authorized invocation. Proposals carry untrusted intent; authorization, credentials, provider execution, evidence, and audit records belong to a separate privileged boundary. Rust type visibility reinforces this distinction but never replaces process isolation, authentication, or policy enforcement. The broker does not exist yet, so `0.1.0` executes no external effects.

Read [`docs/security-model.md`](docs/security-model.md) for trust assumptions and current limitations.

## Roadmap

The next architectural milestones are a separate unprivileged `dekopond`, an authenticated privileged broker, declarative policy evaluation, and bounded Wasm capability providers. See [`docs/roadmap.md`](docs/roadmap.md); roadmap items are intentions, not shipped features.

## Organization and package names

[`dekopon-agents`](https://github.com/dekopon-agents) is the GitHub organization that hosts the project. **Dekopon** is the product, `dekopon` is the CLI binary and Cargo workspace, and `dekopon` is the intended primary crates.io package. Organization naming does not change the product or package name.

## Contributing and license

See [`CONTRIBUTING.md`](CONTRIBUTING.md), [`SECURITY.md`](SECURITY.md), and the [Code of Conduct](CODE_OF_CONDUCT.md). Dekopon is dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
