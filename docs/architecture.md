# Architecture

Read [`design.md`](design.md) first for the product model and accepted invariants. This document maps that design to current crate boundaries and the planned deployment topology; it does not make planned components current.

## Present in 0.1.0

Dekopon has two deliberately separate synchronous execution surfaces.

The operator CLI retains its local catalog path:

```text
dekopon
  -> discover one local config file
  -> load and validate a typed catalog
  -> execute a typed read command
  -> render a typed result
```

`ResourceReader` separates command execution from `LocalConfigReader`, leaving room for a daemon-backed reader without spreading YAML handling through commands. The same operator surface owns model-account lifecycle without loading the catalog:

```text
dekopon auth chatgpt
  -> fixed OpenAI device-auth host
  -> isolated Dekopon credential file
```

The experimental immediate path is:

```text
dekopon-run
  -> compile one or more Wasm components
  -> call and validate read-only provider manifests
  -> direct invoke: route one capability and emit timings
     or
  -> prompt: expose schemas through OpenAI-compatible or ChatGPT/Codex subscription transport and execute selected tools
  -> create a fresh bounded Wasmtime store for every component call
```

The immediate host links no WASI or custom imports, rejects non-read-only manifests, resolves no credentials, and cannot access external systems. It is provider computation tooling, not the planned privileged provider host. There is still no daemon, authenticated broker, policy evaluator, provider I/O interface, audit store, or external effect path.

Crate boundaries are:

- `dekopon-core`: validated identifiers and dependency-light domain enums.
- `dekopon-capability`: capability metadata and proposal/authorization invocation states.
- `dekopon-protocol`: strict `dekopon.dev/v1alpha1` resources and list responses.
- `dekopon-config`: discovery, parsing, duplicate detection, and reference validation.
- `dekopon-provider-sdk`: typed Rust guest trait, manifest/response wire types, and adapters for its default or a caller-generated provider world.
- `dekopon-provider-http`: guest-only Rust facade for the published buffered HTTP interface; no current host implements it.
- `dekopon-provider-host`: bounded synchronous Wasmtime host and deterministic capability registry.
- `dekopon-model`: bounded model contract, OpenAI-compatible transport, and isolated ChatGPT/Codex authentication and Responses client.
- `dekopon-run`: Clap CLI, direct invocation reports, bounded prompt loop, and trace export.
- `dekopon-testkit`: private builders used by workspace tests.
- `dekopon`: catalog and model-auth command parsing, resource reads, rendering, and process exits.

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

A model-facing tool call is only a proposal. The authenticated daemon-to-broker request carries that proposal and trusted envelope context, not an `AuthorizedInvocation`. The broker owns the authority transition from `ProposedInvocation` to `AuthorizedInvocation`; it creates and consumes that state inside the broker-owned execution boundary while evaluating policy, attaching constraints, invoking a provider, and recording evidence. `dekopond` never receives or presents serialized authorization state as a bearer grant, and agent code never receives raw provider credentials.

## Immediate isolation and planned provider authority

The immediate host establishes a small current subset of the planned mechanism: component-model providers run in Wasmtime, each `ProviderRegistry` retains its compiled components in memory, and each description or invocation gets a fresh store and instance with explicit fuel, time, memory, input, and output limits. There is no cross-process or on-disk compilation cache. A shared runtime mutex serializes component execution. An empty linker is the authority boundary: no filesystem, network, clock, random, environment, credential, or other host function is available to the guest.

Capability JSON Schemas are exposed to models and must be object-shaped, but the host is not a general JSON Schema validator. Providers validate their operation-specific fields. Immediate success output remains raw JSON rather than an authorized invocation, broker evidence, or an audit record.

Model authentication terminates in the model client, separately from provider authority. ChatGPT subscription mode owns a distinct device-flow credential file, refreshes tokens only against OpenAI's fixed authentication host, and sends inference only to the fixed Codex Responses host. It does not import another application's token store or expose model credentials to a component.

The privileged provider design remains future work. The broker will share Wasmtime's compiled engine and component cache, create a fresh bounded store for every authorized invocation, and expose narrowly scoped asynchronous host interfaces integrated with Tokio. The first such interface is the committed buffered `dekopon:http@1.0.0` contract described in [`broker-http.md`](broker-http.md). Its native Rust implementation is statically linked into `dekopon-brokerd`; guest bindings are statically linked into provider components. Network destinations, methods, credentials, retries, evidence, and host calls are derived from an authenticated `AuthorizedInvocation`, not from an immediate prompt session. Direct `dekopon-run` execution must not grow those privileges in-process; broker-backed runner operations remain unprivileged clients.

## Resource evolution

`dekopon.dev/v1alpha1` rejects unknown authored fields to expose typos and ignored authority settings. Transport negotiation is out of scope for the local release. A future daemon API must version resources explicitly and document any field-preservation or compatibility rules before relaxing strict decoding.

The monorepo shares versions, CI, issues, and releases while crate boundaries are still changing together. Crates should move to separate repositories only when ownership or release cadence genuinely diverges.
