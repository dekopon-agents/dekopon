# Roadmap

Roadmap items describe sequencing, not shipped behavior or permission to bypass the invariants in [`design.md`](design.md) and [`security-model.md`](security-model.md). They are intentions rather than delivery commitments.

## 0.1 — local control and immediate provider tooling (implemented)

- Strict `v1alpha1` agent, capability, and provider resources.
- Local YAML/JSON discovery and cross-reference validation.
- Deterministic get, describe, validate, and config-view commands.
- Proposal/authorization typestate and documented process boundary.
- Experimental Rust provider SDK and import-free Wasmtime component host.
- One-shot direct invocation, OpenAI-compatible and ChatGPT/Codex subscription prompt tools, isolated device login, timing reports, and Chrome trace export for read-only provider computation.

## Next milestones

1. Define an authenticated local daemon protocol and add an unprivileged `dekopond` catalog/task service without external-write authority.
2. Build a separately deployed broker prototype with policy decisions, constrained authorization receipts, evidence, and append-only audit records.
3. Add the buffered `dekopon:http@1.0.0` contract and one broker-mediated read-only integration with narrowly scoped destinations, authenticated identity, and evidence before enabling its separately named external-write capabilities.
4. Introduce Cedar policy only after authorization inputs and explainability requirements are proven by the broker prototype.
5. Evolve the immediate Wasmtime machinery into a broker-owned asynchronous host with a shared engine, compiled component cache, fresh bounded stores, and Tokio-integrated host calls, while retaining import-free direct runner execution.
6. Add identity, model, context, memory, observability, MCP interoperability, and multi-agent review only when each has tested user-facing behavior.

## Intended package namespace

`dekopon-model` is now present with tested OpenAI-compatible and ChatGPT/Codex transports plus model-account authentication. The following remaining names are reserved for future meaningful crates. They are **not** present in the workspace and are not claimed as crates.io reservations or published packages:

- `dekopon-agent`
- `dekopon-broker`
- `dekopon-policy`
- `dekopon-identity`
- `dekopon-context`
- `dekopon-memory`
- `dekopon-tribunal`
- `dekopon-mcp`
- `dekopon-observe`

A crate should be added only with meaningful, tested behavior needed by an implemented milestone. Tightly coupled crates remain in this monorepo and initially share the `0.1.x` release line.

## Explicit non-goals for 0.1

Interactive TUI, daemon networking, shell-completion installation, provider credential access, provider host I/O, policy evaluation, durable evidence/audit, and local or external effect execution are intentionally absent from 0.1. Their accepted broker-mediated HTTP direction is documented in [`broker-http.md`](broker-http.md), but documentation does not make those paths current. Model-account lifecycle is exposed through `dekopon auth`; model inference and component loading remain confined to the explicitly experimental `dekopon-run` executable.
