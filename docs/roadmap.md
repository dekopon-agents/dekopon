# Roadmap

Roadmap items describe sequencing, not shipped behavior or permission to bypass the invariants in [`design.md`](design.md) and [`security-model.md`](security-model.md). They are intentions rather than delivery commitments.

## 0.1 — local control CLI (implemented)

- Strict `v1alpha1` agent, capability, and provider resources.
- Local YAML/JSON discovery and cross-reference validation.
- Deterministic get, describe, validate, and config-view commands.
- Proposal/authorization typestate and documented process boundary.

## Next milestones

1. Define an authenticated local daemon protocol and add an unprivileged `dekopond` catalog/task service without external-write authority.
2. Build a separately deployed broker prototype with policy decisions, constrained authorization receipts, evidence, and append-only audit records.
3. Add one meaningful read-only provider end to end before introducing external writes.
4. Introduce Cedar policy only after authorization inputs and explainability requirements are proven by the broker prototype.
5. Host component-model providers with Wasmtime, a shared engine, a fresh bounded store per invocation, and async Tokio integration.
6. Add identity, model, context, memory, observability, MCP interoperability, and multi-agent review only when each has tested user-facing behavior.

## Intended package namespace

The following names are reserved in the project architecture for future meaningful crates. They are **not** present in the workspace and are not claimed as crates.io reservations or published packages:

- `dekopon-agent`
- `dekopon-broker`
- `dekopon-policy`
- `dekopon-identity`
- `dekopon-provider-sdk`
- `dekopon-provider-host`
- `dekopon-model`
- `dekopon-context`
- `dekopon-memory`
- `dekopon-tribunal`
- `dekopon-mcp`
- `dekopon-observe`

A crate should be added only with meaningful, tested behavior needed by an implemented milestone. Tightly coupled crates remain in this monorepo and initially share the `0.1.x` release line.

## Explicit non-goals for 0.1

Interactive TUI, plugin loading, daemon networking, shell-completion installation, model calls, credential access, policy evaluation, and effect execution are intentionally deferred.
