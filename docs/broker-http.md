# Broker-mediated HTTP providers

This document records the **committed direction** for the first privileged Dekopon provider host. It does not describe current behavior until the corresponding broker, host interface, and client paths are implemented and tested.

## Current foundation

The immutable `dekopon:http@1.0.0` WIT package, the `dekopon-provider-http` Rust guest facade, and SDK support for caller-generated provider worlds are current once their slices land. The checked-in HTTP probe demonstrates the exact import and direct-host rejection. These pieces define and call the buffered import but provide no transport or authority. The broker, host implementation, authorization, audit, client, and external interaction described below remain committed direction until their respective slices are implemented and tested.

## Decision

Dekopon will expose a project-owned, buffered WebAssembly Component Model interface named `dekopon:http@1.0.0`. Provider components may import that interface, but an import is only a structural requirement. It is not authority to contact a destination.

The native HTTP implementation belongs to `dekopon-brokerd` and is statically linked into the broker. Provider-side Rust bindings are compiled into each guest component. Dekopon will not dynamically load Rust `dylib` plugins or download an implementation in response to an untrusted component import.

The existing immediate mode remains distinct:

- direct `dekopon-run` providers remain read-only and import-free;
- broker-backed `dekopon-run` operations send proposals to a separately running broker;
- `dekopon-run` never links `dekopon:http`, resolves provider credentials, or constructs an `AuthorizedInvocation`;
- the broker authenticates the caller, authorizes the proposal, resolves supported imports, executes the provider, and records the result.

This preserves the rule that orchestration does not gain provider authority merely because it can ask for a tool call.

## Process and authority flow

```text
model or direct dekopon-run request
              |
              | untrusted capability + arguments
              v
      ProposedInvocation request
              |
              | authenticated local transport
              v
        dekopon-brokerd
          authenticate peer
          reject replay
          resolve trusted capability metadata
          evaluate operator policy
          create and consume AuthorizedInvocation
          attach HTTP and execution constraints
              |
              | broker-owned Wasmtime linker
              v
        provider component
          import dekopon:http/client@1.0.0
              |
              | bounded native host call
              v
          external endpoint
              |
              v
       result + evidence + audit
```

External-write capabilities require this separate broker process even when a demonstration endpoint does not persist writes. A provider manifest remains untrusted input; trusted policy and capability configuration determine whether an operation is authorized.

## Local broker protocol

The first implementation will use a local Unix-domain socket. The broker owns the socket path, creates it with owner-only permissions, and authenticates peers from operating-system socket credentials. Payload fields cannot override the authenticated principal.

Each request carries a unique invocation identifier, trace identifier, capability identifier, bounded JSON input, and protocol version. The broker rejects duplicate invocation identifiers for the lifetime of its replay window. Requests and responses use length-delimited, size-bounded messages so a peer cannot force unbounded buffering.

The initial protocol exposes only the operations needed by a broker client:

- inspect the broker's available provider capabilities;
- submit one invocation proposal;
- receive a denied, succeeded, or failed result with bounded public evidence metadata.

`AuthorizedInvocation` is never accepted from or returned to the client. It is created and consumed inside the broker process.

## HTTP component contract

`dekopon:http@1.0.0` is a high-level request/response interface rather than a socket or generic I/O interface. Its request shape carries:

- an arbitrary valid HTTP method token, including standard and extension methods;
- an absolute URI;
- ordered headers with duplicate values preserved;
- a buffered byte body.

Its response carries a status code, ordered headers, and a buffered byte body. Failures use a bounded typed error rather than traps or transport-library error text.

The first version intentionally has no streams, polling, sockets, DNS, filesystem, environment, or raw credential imports. The broker may use a streaming native HTTP client internally, but it presents bounded buffers at the component boundary. If measured workloads later require guest-visible streaming, a separate interface can use standard `wasi:io` resources without granting raw sockets.

The interface can represent every HTTP method, but representation is not permission. Each invocation's policy constraints determine the allowed methods and destinations.

## Broker HTTP enforcement

Before performing a request, the broker host validates all of the following against trusted authorization state:

- the invocation is authorized for the currently executing capability;
- the HTTP host and effective port are explicitly allowed;
- the method is explicitly allowed;
- the scheme is HTTPS, except for explicitly enabled loopback test endpoints;
- user information and URI fragments are absent;
- header names and values are syntactically valid and within count and byte limits;
- authority-defining, hop-by-hop, proxy, and broker-managed credential headers are not guest controlled;
- the request body and complete encoded request remain within their limits;
- the invocation has remaining host-call budget.

The native client does not inherit proxy configuration from the environment and does not follow redirects. DNS results are checked against the authorized destination policy before connection, and redirects cannot be used to escape that decision. Responses have status, header, body, decompression, and wall-clock bounds. Dropping or timing out an invocation cancels outstanding host work and releases its resources.

Provider credentials remain broker-owned. A future credential resolver may inject destination-bound headers after guest headers and policy have been validated; raw credentials are never returned to the provider, model, client, trace, evidence, or normal audit fields.

## Authorization policy

The first broker policy is explicit and deny-by-default. Trusted operator configuration binds an authenticated principal to named capabilities and per-capability execution constraints. HTTP constraints include exact destination hosts, allowed methods, host-call count, request bytes, response bytes, and timeout.

A component's import table and manifest can narrow what it is able to request, but neither can widen policy. The broker rejects unknown imports before invocation. A provider that declares a read-only capability cannot use a method authorized only for an external-write capability, and one capability's grant is not reusable by another invocation.

Policy decisions produce a stable decision identifier and policy revision. Authorization binds that decision, the original proposal, the selected provider and capability, and the exact execution constraints used by the host.

## Evidence and audit

Each broker request produces an append-only audit entry for denial, success, or failure. Entries correlate authenticated principal, invocation and trace identifiers, capability, provider, policy decision, effect classification, timing, outcome, and bounded HTTP metadata. Request and response bodies, authorization headers, cookies, credentials, model text, and provider-returned sensitive values are not written to normal audit fields.

Audit entries are hash-linked within one broker log so truncation or reordering after a known checkpoint is detectable. This is local integrity evidence, not a claim of tamper-proof storage against a compromised broker host. Durable remote anchoring, key-backed signatures, tenancy, and incident-response machinery remain separate work.

## Version and implementation policy

WIT package versions and Rust crate versions are independent:

- provider crates depend on the WIT interface version they import;
- one broker host crate may register adapters for multiple supported WIT versions;
- compatible native HTTP-library upgrades do not require provider rebuilds;
- breaking component-contract changes receive a new WIT interface version;
- published WIT versions are immutable and resolved through the checked registry configuration and lock file.

The broker fails closed when no approved implementation exists for an import. Runtime provider requests never trigger package or native-code downloads. Optional executable adapters, if introduced later, must be operator-installed, digest-pinned Wasm components rather than native Rust plugins.

## Delivery sequence

The behavior can land in reviewable slices without temporarily granting authority to the immediate host:

1. publish and validate the HTTP WIT contract and guest bindings;
2. generalize provider guest world generation and add an HTTP-importing fixture that the immediate host still rejects;
3. add the bounded asynchronous broker-owned host implementation with local mock-server tests;
4. add authenticated broker transport, deny-by-default policy, authorization, evidence, and audit;
5. add broker client mode and a JSONPlaceholder demonstration provider with separately named read and write capabilities;
6. add release artifacts and end-to-end CI after every preceding boundary is independently tested.

Until a slice is implemented, its behavior remains committed direction rather than current functionality.
