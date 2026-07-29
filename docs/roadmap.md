# Roadmap

Roadmap items describe sequencing, not shipped behavior or permission to bypass the invariants in [`design.md`](design.md) and [`security-model.md`](security-model.md). They are intentions rather than delivery commitments.

## 0.1 — local control and immediate provider tooling (implemented)

- Strict `v1alpha1` agent, capability, and provider resources.
- Local YAML/JSON discovery and cross-reference validation.
- Deterministic get, describe, validate, and config-view commands.
- Proposal/authorization typestate and documented process boundary.
- Experimental Rust provider SDK and import-free Wasmtime component host.
- One-shot direct invocation, OpenAI-compatible and ChatGPT/Codex subscription prompt tools, isolated device login, timing reports, and Chrome trace export for read-only provider computation.

## 0.2 — privileged host foundation (in development)

- Immutable buffered `dekopon:http@1.0.0` WIT package and statically compiled Rust guest facade.
- Caller-generated provider worlds plus a checked-in HTTP-importing component that the immediate runner rejects.
- Exact per-invocation HTTP authorization constraints beneath independent native ceilings.
- Statically linked HTTP engine with bounded buffers, DNS/IP and redirect controls, and sanitized evidence metadata.
- Asynchronous broker component-host library with one shared Wasmtime engine, compiled components, fresh bounded stores, Tokio host calls, and a single-use `AuthorizedInvocation` public execution boundary.
- Exact deny-by-default broker rules, authenticated-context binding, replay rejection/restoration, digest evidence, and bounded metadata-only in-memory or owner-only durable verified audit chains.
- Strict versioned length-delimited broker messages and an unprivileged Unix client whose invocation payload cannot carry identity or authority.

These libraries are not a deployable broker and are not reachable from an operator command.

## Next milestones

1. Build a bounded Unix listener with secure socket ownership and map OS peer credentials into trusted broker context.
2. Build a separately deployed broker process around the current core with external checkpoint handling.
3. Integrate the current unprivileged client while keeping direct `dekopon-run` on its import-free host.
4. Add a JSONPlaceholder demonstration provider using narrowly scoped destinations and separately named read and external-write capabilities; keep tests deterministic and mock-backed.
5. Add broker-owned credential resolution only after destination binding and redaction are independently tested.
6. Introduce Cedar only after authorization inputs and explainability requirements are proven by the broker prototype.
7. Add identity, context, memory, observability, MCP interoperability, and multi-agent review only when each has tested user-facing behavior.

## Intended package namespace

`dekopon-model` is now present with tested OpenAI-compatible and ChatGPT/Codex transports plus model-account authentication. The following remaining names are reserved for future meaningful crates. They are **not** present in the workspace and are not claimed as crates.io reservations or published packages:

- `dekopon-agent`
- `dekopon-policy`
- `dekopon-identity`
- `dekopon-context`
- `dekopon-memory`
- `dekopon-tribunal`
- `dekopon-mcp`
- `dekopon-observe`

A crate should be added only with meaningful, tested behavior needed by an implemented milestone. Tightly coupled crates remain in this monorepo and share one pre-1.0 release line.

## Explicit non-goals for 0.1

Interactive TUI, daemon networking, shell-completion installation, provider credential access, operator-accessible provider host I/O, policy evaluation, durable evidence/audit, and local or external effect execution are intentionally absent from 0.1. Their accepted broker-mediated HTTP direction is documented in [`broker-http.md`](broker-http.md), but documentation does not make those paths current. Model-account lifecycle is exposed through `dekopon auth`; model inference and component loading remain confined to the explicitly experimental `dekopon-run` executable.
