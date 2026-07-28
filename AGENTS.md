# Guidance for coding agents

This file applies to the entire repository. Humans can use the same reading guide; agents are expected to follow it before editing code or documentation.

## Required reading

**Always read [`docs/design.md`](docs/design.md) first.** It is the canonical overview of Dekopon's product model, vocabulary, authority transition, component ownership, and the distinction between current behavior and committed direction. Reading only the roadmap is not enough: roadmap entries are sequencing ideas, not proof that a feature exists or permission to weaken a boundary.

Then read the documents selected by the work:

| If the change touches… | Read… | Because… |
|---|---|---|
| Any product behavior or architecture | [`docs/design.md`](docs/design.md) | It defines the non-negotiable invariants and accepted design decisions. |
| Capabilities, actors, identity, policy, credentials, providers, evidence, audit, or external effects | [`docs/security-model.md`](docs/security-model.md) | It defines trusted inputs, untrusted content, threats, and current limitations. |
| Crate boundaries, dependencies, protocols, daemon/broker separation, async, Wasmtime, or Cedar | [`docs/architecture.md`](docs/architecture.md) | It maps design responsibilities to current and future implementation boundaries. |
| Operator auth, catalog CLI parsing, config discovery, resource reads, output, or exit codes | [`docs/cli.md`](docs/cli.md) | It is the current operator contract. |
| Immediate providers, Wasm components, prompt tools, model endpoints, limits, or traces | [`docs/run.md`](docs/run.md) | It defines the experimental current runner and the privileges it must not gain. |
| Scope, priority, package names, or a proposed new crate | [`docs/roadmap.md`](docs/roadmap.md) | It records sequencing and explicit non-goals; it does not make future components current. |

[`docs/README.md`](docs/README.md) is the complete documentation map. Read [`CONTRIBUTING.md`](CONTRIBUTING.md) for validation and pull-request expectations.

## Rules that must survive every change

- A model may propose an invocation, but only the broker may authorize it.
- Capability declarations permit proposals; they do not grant ambient process authority.
- Read authority never implies write authority. External writes require explicit narrow capabilities.
- Trusted actor identity comes from an authenticated envelope, never from model or repository content.
- Provider credentials remain inside the broker boundary and out of prompts, config, evidence, and logs; model credentials stay inside the selected model client and never enter provider components.
- `dekopond` and `dekopon-brokerd` remain separate processes once external-write authority exists.
- `dekopon-run` remains read-only and import-free; do not add provider credentials, WASI, host I/O, local writes, external writes, or authorization claims to immediate mode.
- Do not describe unimplemented daemons, policy, privileged provider interfaces, or external effects as available.
- Do not add empty crates or heavy future dependencies without meaningful, tested behavior.
- Parse config once into typed resources; do not spread YAML handling through command execution.
- Do not publish crates, create releases, weaken branch protection, or add credentials without explicit human authorization.

## Working method

1. Classify the requested behavior as **Current**, **Committed direction**, or **Exploration** using `docs/design.md`.
2. Identify the process that owns the data and the process that owns the authority.
3. Inspect the relevant implementation and tests; do not infer current behavior from roadmap prose.
4. Make the smallest coherent change that preserves the authority boundary.
5. Add failure-path, serialization, CLI, or compile-fail tests appropriate to the change.
6. Update affected documentation and examples in the same pull request.
7. Run the checks in `CONTRIBUTING.md`; never report a check or remote operation as successful unless it was verified.

If the requested implementation conflicts with the design or security model, stop and surface the conflict for human decision rather than silently choosing a new architecture.
