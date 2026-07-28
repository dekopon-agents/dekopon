# Architecture

Read [`design.md`](design.md) first for the product model and accepted invariants. This document maps that design to current crate boundaries and the planned deployment topology; it does not make planned components current.

## Present in 0.1.0

Dekopon currently consists of a synchronous local CLI and a typed Cargo workspace. The runtime path is deliberately small:

```text
main
  -> parse CLI
  -> discover one local config file
  -> load and validate a typed catalog
  -> execute a typed read command
  -> render a typed result
```

`ResourceReader` separates command execution from `LocalConfigReader`, leaving room for a daemon-backed reader without spreading YAML handling through commands. There is no daemon, broker, policy evaluator, provider host, or model client today.

Crate boundaries are:

- `dekopon-core`: validated identifiers and dependency-light domain enums.
- `dekopon-capability`: capability metadata and proposal/authorization invocation states.
- `dekopon-protocol`: strict `dekopon.dev/v1alpha1` resources and list responses.
- `dekopon-config`: discovery, parsing, duplicate detection, and reference validation.
- `dekopon-testkit`: private builders used by workspace tests.
- `dekopon`: command parsing, resource reads, execution, rendering, and process exits.

## Planned deployment boundary

The intended deployment is three operator-visible roles:

```text
dekopond
    model interaction, orchestration, context, memory

dekopon-brokerd
    authorization, credentials, provider execution, external effects

dekopon
    human/operator control CLI
```

`dekopond` will be unprivileged. The agent and broker will be separate processes and separate pods once any external-write authority exists. Authenticated, replay-resistant message envelopes will carry principal and workload identity across that boundary.

A model-facing tool call is only a proposal. The broker owns the authority transition from `ProposedInvocation` to `AuthorizedInvocation`; it evaluates policy, attaches constraints, invokes a provider, and records evidence. Agent code never receives raw provider credentials.

## Planned provider isolation

Capability providers are expected to become WebAssembly components hosted by Wasmtime. The broker will eventually share Wasmtime's compiled engine and component cache, while creating a fresh store for every invocation. Wasm providers will run as bounded asynchronous invocations integrated with Tokio, with explicit time, memory, output, network, and host-call constraints.

These are design constraints, not implemented features. Wasmtime and Tokio are intentionally absent from `0.1.0` because no current command executes providers.

## Resource evolution

`dekopon.dev/v1alpha1` rejects unknown authored fields to expose typos and ignored authority settings. Transport negotiation is out of scope for the local release. A future daemon API must version resources explicitly and document any field-preservation or compatibility rules before relaxing strict decoding.

The monorepo shares versions, CI, issues, and releases while crate boundaries are still changing together. Crates should move to separate repositories only when ownership or release cadence genuinely diverges.
