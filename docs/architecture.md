# Architecture

Read [`design.md`](design.md) first for the product model and accepted invariants. This document maps that design to current crate boundaries and the planned deployment topology; it does not make planned components current.

## Published baseline and current 0.2 foundation

The published `0.1.0` baseline has two deliberately separate synchronous execution surfaces. The current `0.2.0` development line adds a privileged asynchronous host library without yet adding a broker process or operator-facing path.

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

The immediate host links no WASI or custom imports, rejects non-read-only manifests, resolves no credentials, and cannot access external systems. It is provider computation tooling, not the privileged provider host. There is still no daemon, authenticated broker, policy evaluator, credential resolver, audit store, or operator-accessible external effect path.

Crate boundaries are:

- `dekopon-core`: validated identifiers and dependency-light domain enums.
- `dekopon-capability`: capability metadata and proposal/authorization invocation states.
- `dekopon-protocol`: strict `dekopon.dev/v1alpha1` resources and list responses.
- `dekopon-config`: discovery, parsing, duplicate detection, and reference validation.
- `dekopon-provider-sdk`: typed Rust guest trait, manifest/response wire types, and adapters for its default or a caller-generated provider world.
- `dekopon-provider-http`: guest-only Rust facade for the published buffered HTTP interface.
- `dekopon-provider-host`: bounded synchronous Wasmtime host and deterministic capability registry for immediate import-free execution.
- `dekopon-http-host`: statically linked native buffered HTTP engine that consumes HTTP constraints beneath independent ceilings.
- `dekopon-broker-host`: privileged asynchronous Wasmtime component host that adapts `dekopon:http@1.0.0` to the native engine and accepts only authorized invocations.
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

The privileged component-linking library is now present, but the privileged provider **process** remains future work. `dekopon-http-host` consumes exact destinations, methods, call counts, byte limits, and deadlines beneath independent ceilings. It checks and pins DNS results, disables redirects and ambient proxies, and emits sanitized metadata. `dekopon-broker-host` adds a shared async Wasmtime engine, compiled components, fresh fuel/memory-bounded stores, wall-clock cancellation, typed WIT adaptation, and rejection tracking that guest code cannot mask. It consumes one non-cloneable `AuthorizedInvocation` at its public execution boundary and has no credential injection.

Neither crate authenticates callers, evaluates policy, constructs authorization, resolves credentials, or writes audit records. No executable currently exposes the privileged host. Direct `dekopon-run` execution must not grow those privileges in-process; future broker-backed runner operations remain unprivileged clients. The complete boundary is described in [`broker-http.md`](broker-http.md).

## Resource evolution

`dekopon.dev/v1alpha1` rejects unknown authored fields to expose typos and ignored authority settings. Transport negotiation is out of scope for the local release. A future daemon API must version resources explicitly and document any field-preservation or compatibility rules before relaxing strict decoding.

The monorepo shares versions, CI, issues, and releases while crate boundaries are still changing together. Crates should move to separate repositories only when ownership or release cadence genuinely diverges.
