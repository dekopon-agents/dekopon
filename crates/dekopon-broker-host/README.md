# dekopon-broker-host

Broker-owned asynchronous Wasmtime host for provider components that import the project-owned `dekopon:http@1.0.0` or `dekopon:storage@0.1.0` interfaces.

Unlike `dekopon-provider-host`, this crate is privileged machinery. Its public invocation API consumes one non-cloneable `AuthorizedInvocation`; each call receives a fresh bounded store, exact HTTP constraints, and a statically linked native HTTP implementation. It is intended for `dekopon-brokerd`, never direct `dekopon-run` execution.

## Execution boundary

`BrokerProviderRegistry` compiles configured components once, validates their manifests, and builds deterministic capability routes. Components are compiled concurrently on the blocking pool — Cranelift is a cold start's whole cost — while manifests are consumed in configured order, so the conflict report and the first reported failure do not depend on which compile finished first. Each artifact is read once under a 64 MiB source ceiling and the recorded SHA-256 is of that same buffer, so the published digest describes exactly what was compiled. A managed `dekopon-brokerd` provider lock additionally supplies expected byte length, SHA-256, and provider ID; the host compares all three at this same read/describe boundary before accepting the component, rather than trusting a preflight hash of different bytes. Legacy directly named paths remain supported without an expected lock identity. The registry retains artifact path/size/SHA-256, bounded Wasmtime-visible import/export documentation, and a cloneable process-local metrics handle for `dekopon-webui`. Imports are resolved into one `InstancePre` per provider at load, so a description, invocation, or command rewrite instantiates without rebuilding a linker or re-resolving imports. Every description or invocation gets a fresh store and component instance.

`BrokerHostOptions` carries operational settings that are deliberately not host ceilings and are not committed into the broker's authority surface:

- `compile_cache_dir` enables Wasmtime's content-addressed on-disk cache, so a restart reads compiled code back instead of running Cranelift again. The directory holds code the privileged broker executes, so it must be writable by the broker and nobody else.
- `max_total_memory_bytes` bounds the guest linear memory reservable across concurrently live stores. Per-store limits bound one invocation; without this, the worst case is the daemon's connection ceiling times `max_memory_bytes`, and the container's OOM killer arrives instead of a refusal. A store that cannot reserve its share is refused before it exists. Stores have per-memory size, memory/table/instance count, table-element, fuel, input, output, and wall-clock ceilings. Host metrics observe compilation, stores, successful instantiations, invocations, fuel readings, resource-limiter memory/table requests, and sanitized HTTP byte/count evidence. Wasmtime exposes no allocator-wide resident-memory or JIT-cache statistic through this embedding API, so the UI labels that absence rather than estimating it. Wasm execution yields on bounded fuel intervals so Tokio deadlines can cancel computation without a process-wide epoch interrupt or global execution mutex. The broker default fuel ceiling includes conservative headroom for a valid default multi-megabyte memory compaction; `chatMemory` composition rejects a lower configured ceiling that would make a full store deterministically trap, while the independent wall-clock limit remains enforced.

The linker exposes only `dekopon:http@1.0.0` and `dekopon:storage@0.1.0`; generic WASI and unknown imports fail before execution. Provider description is linked so an importing component can instantiate, but any host call during `describe` rejects the component. Invocation requires an `AuthorizedInvocation`; its provider must match the trusted capability route, and absent exact constraints supply no HTTP or storage authority.

## Buffered HTTP enforcement

The host:

- accepts arbitrary syntactically valid method tokens but requires an exact method grant;
- requires an exact DNS name/IP authority and effective port grant;
- permits HTTPS to public destinations;
- permits plaintext HTTP only when the authorization explicitly enables it and every resolved address is loopback;
- resolves and checks every destination address, then pins those addresses into the request client;
- disables environment proxies, redirects, and automatic content decompression;
- rejects guest-controlled authority, framing, hop-by-hop, cookie, and authorization headers;
- strips hop-by-hop and credential-bearing response headers;
- preserves other ordered duplicate headers and buffered body bytes;
- streams native response chunks into a bounded buffer;
- enforces host ceilings in addition to narrower per-invocation request count, request byte, response byte, and timeout constraints;
- returns only bounded provider-safe transport messages and sanitized HTTP evidence metadata.

A denied destination or method cannot be hidden by provider code: the host marks the invocation as rejected even if the guest catches the WIT error.

## Deliberate limitations

This crate does not authenticate callers, evaluate policy, construct authorization, resolve credentials, or write audit records. Those responsibilities belong to the broker layer. A broker-resolved destination-bound credential may ride alongside an authorized invocation (never inside it); the native engine injects it after guest-header validation, and the guest never observes it. It supports buffered HTTP request/response exchanges, not CONNECT tunnels, upgrades, WebSockets, streaming guest handles, redirects, cookies, or ambient proxy configuration.

## Provider storage

The linker also implements `dekopon:storage@0.1.0`. A storage call succeeds only with a consumed
`StorageGrant` matching host, invocation, capability, provider, interface, access, namespace, and
limits. Description/command resolution get a disabled sticky context. Wrong-interface,
permission/quota/budget/corruption/timeout errors stay terminal after a guest catches the WIT enum.
A successful provider result is returned only after storage commit finalization. Its deadline starts
before already-dispatched blocking jobs drain, and no later filesystem step starts after expiry.
Storage spans and metrics omit identity/scope/provider/capability and exact provider byte totals;
only content-free operation/sync/quota counts and coarse byte buckets are retained.
