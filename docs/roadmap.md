# Roadmap

Roadmap items describe sequencing, not shipped behavior or permission to bypass the invariants in [`design.md`](design.md) and [`security-model.md`](security-model.md). They are intentions rather than delivery commitments.

## 0.1 — local control and immediate provider tooling (implemented)

- Strict `v1alpha1` agent, capability, and provider resources.
- Local YAML/JSON discovery and cross-reference validation.
- Deterministic get, describe, validate, and config-view commands.
- Proposal/authorization typestate and documented process boundary.
- Experimental Rust provider SDK and import-free Wasmtime component host.
- One-shot direct invocation, OpenAI-compatible and ChatGPT/Codex subscription prompt tools, isolated device login, timing reports, and Chrome trace export for read-only provider computation.

## 0.2 — privileged local broker foundation (released)

- Immutable buffered `dekopon:http@1.0.0` WIT package and statically compiled Rust guest facade.
- Caller-generated provider worlds plus a checked-in HTTP-importing component that the immediate runner rejects.
- Exact per-invocation HTTP authorization constraints beneath independent native ceilings.
- Statically linked HTTP engine with bounded buffers, DNS/IP and redirect controls, and sanitized evidence metadata.
- Asynchronous broker component-host library with one shared Wasmtime engine, compiled components, fresh bounded stores, Tokio host calls, and a single-use `AuthorizedInvocation` public execution boundary.
- Deny-by-default broker authorization, authenticated-context binding, replay rejection/restoration, digest evidence, and bounded metadata-only in-memory or owner-only durable verified audit chains.
- Strict versioned length-delimited broker messages and explicit `dekopon-run broker` client commands whose invocation payload cannot carry identity or authority.
- Unix-only `dekopon-brokerd` with owner-controlled strict configuration, private socket lifecycle, peer-UID context mapping, bounded connections/draining, provider execution, durable replay restoration, and an atomic owner-only checkpoint file that detects audit rollback relative to retained state.
- Mock-backed JSONPlaceholder post-read and separately classified external-write capabilities using exact broker HTTP grants.

Version 0.2 shipped an exact-match policy evaluator. It has since been replaced by Cedar; see milestone 3 below.

Version 0.2.0 is published as 17 public crates and provenance-attested CLI archives. The broker process is deployable for one local owner-UID trust domain and has an explicit unprivileged `dekopon-run` client; at that release the operator CLI and an agent daemon remained unintegrated, and it had no provider credential resolver.

Since that release the tree has gained Cedar, broker-owned credentials, identity/attestation, and the `dekopond` gateway, and now holds 20 publishable crates. What has *not* changed is the checkpoint story: there is still no independently retained, signed, or remote anchor.

## Next milestones

1. Add independent checkpoint retention/export or signing so rollback of both local audit and checkpoint files is detectable outside the broker host.
2. ~~Add broker-owned credential resolution only after destination binding and redaction are independently tested.~~ Done: destination binding and redaction ship with independent engine-, broker-, and service-level tests.
3. ~~Introduce Cedar only after authorization inputs and explainability requirements are proven by the broker prototype.~~ Done: the exact-match evaluator proved which inputs a decision needs, and Cedar replaced it. `dekopon-policy` generates a schema from the deployment's declared world, validates the policy set in strict mode at startup, denies on any evaluation error, and reports the determining policy identifiers plus a policy-set digest in every audit record. Execution constraints stayed outside the policy language as owner-authored constraint sets, so a policy edit can broaden who may act and can never widen how far an action reaches.
4. Add identity, context, memory, observability, MCP interoperability, and multi-agent review only when each has tested user-facing behavior. Broker-side identity is now current: canonical external subjects, owner-controlled subject-to-principal mappings, per-peer attestor grants, and `via`-scoped rules that keep attested and direct authority disjoint. The unprivileged agent daemon is now current too: `dekopond` connects to chat services, routes each authenticated message to a catalog agent, and submits attested proposals with no authority of its own ([`dekopond.md`](dekopond.md)). Conversation context is current too: a route set to `mode: persistent` keeps a bounded per-sender history in gateway memory, compacted to question-and-answer pairs, dropped on an idle timeout, an LRU ceiling, or a changed capability grant, with the contract in [`dekopond.md`](dekopond.md#conversations) and the trust surface it accepts in [`security-model.md`](security-model.md#conversation-memory-as-a-trust-surface). `oneShot` is the default, so a message on a route that did not ask for memory is still an independent session. Agent memory that outlives a conversation, a dedicated gateway UID — the deployment in which `via` scoping is real isolation rather than attribution — and the rest of this item remain future.

## Follow-ups accepted during the gateway/identity/Cedar work

Each of these was raised, deliberately scoped out, and accepted as a follow-up rather than dropped. None was committed direction in the design sense when it was raised; a struck entry has since had a design pass of its own, which is what promoted it.

- **A dedicated gateway UID and a 0660 socket transport.** The one change that turns `via` and namespace scoping from attribution into real isolation. It widens who may connect to the privileged socket, so it needs its own security review rather than a permission-bit edit.
- **WhatsApp as a transport.** Unlike Socket Mode and long polling, it requires a public webhook endpoint — an inbound HTTP surface on the unprivileged daemon, with signature verification and replay handling of its own.
- **Per-principal credential overrides for one capability.** Today a constraint set binds one credential per capability for every principal that reaches it. "Approve as the person who asked" needs a second axis, and the naive version leaks which principals exist into a file that currently declares only capabilities.
- ~~**Conversation memory and multi-turn threads in `dekopond`.** Each message is an independent session with no history. Memory is a new trust surface — text that persists across sessions and is replayed into a prompt — and belongs behind its own design pass.~~ Done: the design pass this item asked for is the [conversation contract](dekopond.md#conversations) and the [trust surface](security-model.md#conversation-memory-as-a-trust-surface) it accepts, and `dekopond` now implements it. History lives in gateway memory, is keyed on the transport, the conversation identity, and the sender's canonical subject, is compacted to question-and-answer pairs inside a sliding window, and is dropped on an idle timeout, an LRU ceiling, or a changed capability grant. Authorization is never cached; what persistence widens is an injected instruction's dwell time, which is why the security model states it rather than the roadmap.
- **`dekopon policy explain` and `auth can-i` operator commands.** The broker already computes the determining `policy_ids` for every decision; the missing half is an operator path to ask the question without making an effect happen. It is also the first CLI-to-broker integration and inherits that whole boundary discussion.
- **Cedar context conditioned on provider input.** Deliberately absent: conditioning authorization on untrusted open JSON needs a settled schema treatment before any policy may depend on a caller-supplied value.
- **Actor kind (human versus service) in policy context.** The broker knows it; policy cannot currently read it. Cheap to add and easy to add wrongly, since it invites rules that look like identity checks but are transport facts.
- **`AuditEvent` field casing.** Variants rename to camelCase while their fields stay snake_case, so a record mixes `attested_subject` with `credentialInjected`. Fixing it breaks the audit chain format, which is worth doing only alongside another change that already does.

## Intended package namespace

`dekopon-model` is now present with tested OpenAI-compatible and ChatGPT/Codex transports plus model-account authentication. `dekopon-agent` is now present with the shared bounded prompt loop and session capability dispatch, consumed by both `dekopon-run` and `dekopond`. `dekopond` itself is now present as the unprivileged chat gateway. `dekopon-policy` is now present too, as the bounded Cedar adapter behind the broker's authorization decisions. The following remaining names are reserved for future meaningful crates. They are **not** present in the workspace and are not claimed as crates.io reservations or published packages:

- `dekopon-identity`
- `dekopon-context`
- `dekopon-memory`
- `dekopon-tribunal`
- `dekopon-mcp`
- `dekopon-observe`

A crate should be added only with meaningful, tested behavior needed by an implemented milestone. Tightly coupled crates remain in this monorepo and share one pre-1.0 release line. The gateway's conversation history needed none of these names: it is a bounded map inside the daemon, and `dekopon-memory` stays reserved for memory that outlives a process.

## Explicit non-goals for 0.1

Interactive TUI, daemon networking, shell-completion installation, provider credential access, operator-accessible provider host I/O, policy evaluation, durable evidence/audit, and local or external effect execution are intentionally absent from 0.1. Their accepted broker-mediated HTTP direction is documented in [`broker-http.md`](broker-http.md), but documentation does not make those paths current. Model-account lifecycle is exposed through `dekopon auth`; model inference and component loading remain confined to the explicitly experimental `dekopon-run` executable.
