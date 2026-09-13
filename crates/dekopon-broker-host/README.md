# dekopon-broker-host

Broker-owned asynchronous Wasmtime host for provider components that import the project-owned
`dekopon:http@1.0.0`, `dekopon:storage@0.1.0`, or `dekopon:clock@1.0.0` interfaces.

This crate is privileged machinery. Its public invocation API consumes one non-cloneable
`AuthorizedInvocation`; each call receives a fresh bounded store, exact HTTP constraints, and a
statically linked native HTTP implementation. It is for `dekopon-brokerd`, never for unprivileged
orchestration.

Output limits bound what the host parses, not peak allocation: the JSON-over-WIT adapter lifts the
whole guest string before measuring it. Per-memory size and memory/table/instance count ceilings
bound that allocation path independently. For command words the input ceiling counts argv plus the
piped value before creating a store. A refusal is an opaque routed `provider-error` response; only
broker-side diagnostics name the bound.

## Execution boundary

`BrokerProviderRegistry` compiles configured components once, validates their manifests, and builds
deterministic capability routes. Components compile concurrently on the blocking pool — Cranelift is
a cold start's whole cost — while manifests are consumed in configured order, so the conflict report
and the first reported failure do not depend on which compile finished first. Each artifact is read
once under a 64 MiB source ceiling and the recorded SHA-256 is of that same buffer, so the published
digest describes exactly what was compiled. A managed `dekopon-brokerd` provider lock additionally
supplies expected byte length, SHA-256, and provider ID; the host compares all three at that same
read/describe boundary before accepting the component, rather than trusting a preflight hash of
different bytes. A directly named path carries no expected lock identity.

The registry retains artifact path, size, and SHA-256; broker validation and the provider manager
consume loaded-provider metadata. Imports are resolved into one `InstancePre` per provider
at load, so a description, invocation, or command run instantiates without rebuilding a linker or
re-resolving imports, and each gets a fresh store and component instance.

A provider declaring command words must export `run-command` (`argv` plus an optional piped
`stdin`). The host reads whether the component's type offers it at load, from
`dekopon-provider-sdk::host::command_export`, and refuses a manifest whose words have no callable
export — absent and wrong-typed are separate refusals, because they are separate operator problems.
The `RunCommandUsedHostImport` tripwire refuses a run that reached for any host import. `clock-probe`'s
test-only `date --clock-in-run-command` is the checked-in path that drives it; the HTTP and storage
halves share its code path with the describe-mode states and no fixture drives them directly.

`BrokerHostOptions` carries operational settings that are not host ceilings and are not committed
into the broker's authority surface:

- `compile_cache_dir` enables Wasmtime's content-addressed on-disk cache, so a restart reads
  compiled code back instead of running Cranelift again. The directory holds code the privileged
  broker executes, so it must be writable by the broker and nobody else.
- `max_total_memory_bytes` bounds the guest linear memory reservable across concurrently live
  stores, and defaults to 256 MiB — four stores at the default per-store ceiling. Per-store limits
  bound one invocation; with this set to `None` the worst case is the daemon's connection ceiling
  times `max_memory_bytes`, and the container's OOM killer arrives instead of a refusal. A store
  that cannot reserve its share is refused before it exists.

Stores have per-memory size, memory/table/instance count, table-element, fuel, input, output, and
wall-clock ceilings. Every description, command run, and invocation records `stores` and
`instantiations` on its own span, so the one-store-one-instance shape of an operation is readable
without a process-global counter. Wasm execution yields on bounded fuel intervals so Tokio deadlines can cancel
computation without a process-wide epoch interrupt or a global execution mutex. The broker default
fuel ceiling includes headroom for a valid default multi-megabyte memory compaction; `chatMemory`
composition rejects a lower configured ceiling that would make a full store deterministically trap,
while the independent wall-clock limit remains enforced.

The linker exposes only these imports; generic WASI and unknown imports fail before execution:

| Import | Answered during | Outside `invoke` |
|---|---|---|
| `dekopon:http/client@1.0.0` | an invocation carrying an exact HTTP grant | typed `denied`, then the describe or command-run tripwire |
| `dekopon:storage/jsonl@0.1.0`, `dekopon:storage/durable-files@0.1.0` | an invocation carrying an exact storage grant of that interface | typed `permission-denied`, then the tripwire |
| `dekopon:clock/wall@1.0.0` | every invocation; no grant | traps, then the tripwire |

Provider description is linked so an importing component can instantiate, but any host call during
`describe` rejects the component. Invocation requires an `AuthorizedInvocation`; its provider must
match the trusted capability route, and absent exact constraints supply no HTTP or storage
authority.

## Wall clock

`now-unix-millis` returns the host's `SystemTime` as milliseconds since the Unix epoch, saturating
at `0` for a clock set before 1970. The import has no error channel, so a store built for a
description or command run traps the read and records the attempt, and the operation fails as
`DescribeUsedHostImport` or `RunCommandUsedHostImport`. A read is not charged against any host-call
limit: it has no effect and allocates nothing, and fuel and the operation deadline already bound
the guest loop around it. Each read inside an invocation emits one info-level `provider_clock_read`
event carrying `unix_millis`, parented by `provider.invoke`, so the value the guest received is in
the trace.

A component importing the clock does not load on a host older than this import: instantiation
fails with `component imports instance \`dekopon:clock/wall@1.0.0\`, but a matching implementation
was not found in the linker`.

## Buffered HTTP enforcement

The host:

- accepts arbitrary syntactically valid method tokens but requires an exact method grant;
- requires an exact DNS name/IP authority and effective port grant;
- permits HTTPS to public destinations;
- permits plaintext HTTP only when the authorization explicitly enables it and every resolved
  address is loopback;
- resolves and checks every destination address, then pins those addresses into the request client;
- disables environment proxies, redirects, and automatic content decompression;
- rejects guest-controlled authority, framing, hop-by-hop, cookie, and authorization headers;
- strips hop-by-hop and credential-bearing response headers;
- preserves other ordered duplicate headers and buffered body bytes;
- streams native response chunks into a bounded buffer;
- enforces host ceilings in addition to narrower per-invocation request count, request byte,
  response byte, and timeout constraints;
- returns only bounded provider-safe transport messages and sanitized HTTP evidence metadata.

A denied destination or method cannot be hidden by provider code: the host marks the invocation as
rejected even if the guest catches the WIT error.

## Limitations

This crate does not authenticate callers, evaluate policy, construct authorization, resolve
credentials, or write audit records. Those belong to the broker layer. A broker-resolved
destination-bound credential rides alongside an authorized invocation, never inside serializable
secret material; the native engine injects it after guest-header validation, and the guest never
observes it. For public DRNs, authorization commits the inert DRN, sink, and binding scope while the
resolved bytes ride separately, and the host refuses any credential whose identity does not match
that commitment. *Committed direction:* the broker's legacy `credential`/`credentialByAgent`
selection will be replaced by public DRNs; this host retains authorization-bound native injection
([migration requirements](../../docs/design.md#legacy-credential-bindings)).
It supports buffered HTTP request/response exchanges, not CONNECT tunnels,
upgrades, WebSockets, streaming guest handles, redirects, cookies, or ambient proxy configuration.

## Provider storage

The linker also implements `dekopon:storage@0.1.0`. A storage call succeeds only with a consumed
`StorageGrant` matching host, invocation, capability, provider, interface, access, namespace, and
limits. Description and command runs get a disabled sticky context. Wrong-interface, permission,
quota, budget, corruption, and timeout errors stay terminal after a guest catches the WIT enum.
Writes take effect per host call, and a completed write is not undone by invocation failure. A
successful provider result requires storage resource finalization, whose deadline starts before
already-dispatched blocking jobs drain; no later filesystem step starts after expiry. Storage spans
and evidence omit identity, scope, provider, capability, and exact provider byte totals, retaining
only content-free operation/sync/quota counts and coarse byte buckets.
