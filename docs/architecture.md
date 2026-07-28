# Architecture

Read [`design.md`](design.md) first for the product model and accepted invariants. This document maps that design to current crate boundaries and the planned deployment topology; it does not make planned components current.

## Present in 0.1.0

Dekopon has two deliberately separate synchronous execution surfaces.

The stable local catalog path remains:

```text
dekopon
  -> discover one local config file
  -> load and validate a typed catalog
  -> execute a typed read command
  -> render a typed result
```

`ResourceReader` separates command execution from `LocalConfigReader`, leaving room for a daemon-backed reader without spreading YAML handling through commands.

The experimental immediate path is:

```text
dekopon-run
  -> compile one or more Wasm components
  -> call and validate read-only provider manifests
  -> direct invoke: route one capability and emit timings
     or
  -> prompt: expose schemas to an OpenAI-compatible model and execute selected tools
  -> create a fresh bounded Wasmtime store for every component call
```

The immediate host links no WASI or custom imports, rejects non-read-only manifests, resolves no credentials, and cannot access external systems. It is provider computation tooling, not the planned privileged provider host. There is still no daemon, authenticated broker, policy evaluator, provider I/O interface, audit store, or external effect path.

Crate boundaries are:

- `dekopon-core`: validated identifiers and dependency-light domain enums.
- `dekopon-capability`: capability metadata and proposal/authorization invocation states.
- `dekopon-protocol`: strict `dekopon.dev/v1alpha1` resources and list responses.
- `dekopon-config`: discovery, parsing, duplicate detection, and reference validation.
- `dekopon-provider-sdk`: typed Rust guest trait, manifest/response wire types, and WIT export adapter.
- `dekopon-provider-host`: bounded synchronous Wasmtime host and deterministic capability registry.
- `dekopon-run`: Clap CLI, direct invocation reports, OpenAI-compatible prompt loop, and trace export.
- `dekopon-testkit`: private builders used by workspace tests.
- `dekopon`: catalog command parsing, resource reads, rendering, and process exits.

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

## Immediate isolation and planned provider authority

The immediate host establishes a small current subset of the planned mechanism: component-model providers run in Wasmtime, components compile once per process, and each description or invocation gets a fresh store with explicit fuel, time, memory, input, and output limits. An empty linker is the authority boundary: no filesystem, network, clock, random, environment, credential, or other host function is available to the guest.

The privileged provider design remains future work. The broker will eventually share Wasmtime's compiled engine and component cache, create a fresh bounded store for every authorized invocation, and expose narrowly scoped asynchronous host interfaces integrated with Tokio. Network destinations, credentials, retries, evidence, and host calls will be derived from an authenticated `AuthorizedInvocation`, not from an immediate prompt session. `dekopon-run` must not grow those privileges in-process.

## Resource evolution

`dekopon.dev/v1alpha1` rejects unknown authored fields to expose typos and ignored authority settings. Transport negotiation is out of scope for the local release. A future daemon API must version resources explicitly and document any field-preservation or compatibility rules before relaxing strict decoding.

The monorepo shares versions, CI, issues, and releases while crate boundaries are still changing together. Crates should move to separate repositories only when ownership or release cadence genuinely diverges.
