# Security policy

## Supported versions

Dekopon has not made a production release. The latest pre-1.0 development line receives security fixes; no production security guarantees are made.

## Reporting a vulnerability

Use GitHub private vulnerability reporting for [`dekopon-agents/dekopon`](https://github.com/dekopon-agents/dekopon/security/advisories/new). Include affected versions or commits, impact, reproduction steps, and suggested mitigations if known.

Do not open a public issue for an unpatched vulnerability and do not include real credentials or sensitive third-party data in a report. If private reporting is unavailable, contact the organization owners through GitHub before disclosing details.

Maintainers acknowledge a report when a human is available, assess scope, coordinate a fix and advisory, and credit reporters who want attribution. This is a volunteer pre-release project with no response-time SLA.

## Scope notes

The workspace has two executable surfaces: the unprivileged `dekopond` gateway and the
privileged `dekopon-brokerd` broker. `dekopond auth` manages an isolated ChatGPT/Codex model
login before gateway configuration or runtime startup. The shared agent layer holds the
bounded model/shell loop and catalog-mounted skills, without broker authority or provider
credentials. Provider component loading and effect execution belong only to the broker. The
gateway's owner-only local development transport is the only local invocation surface.

The broker executable authenticates distinct peer UIDs over the protected Unix socket under the [current local process boundary](docs/security-model.md#current-local-process-boundary), maps peer credentials through strict trusted configuration, appends owner-only JSONL audit records, resolves credentials-file entries or separately authorized public-DRN/private-map sources that no guest component can observe, and may expose policy-authorized provider HTTP. It runs a privileged asynchronous component host, a bounded native HTTP engine, Cedar authorization over owner-authored execution constraints, an evidence/audit core, and a bounded identity-free Unix client protocol. The in-tree `http-probe` fixture and the fetched standalone JSONPlaceholder component separate read-only and external-write capabilities and are tested only with injected or loopback mocks; the `gh` provider is maintained out of tree in [`dekopon-provider-gh`](https://github.com/dekopon-agents/dekopon-provider-gh). Findings in framing/deadlines, socket lifecycle/permissions, peer/server-UID validation, configuration ownership, authority omission, trusted-context binding, replay controls, policy matching, audit redaction/private-file containment, destination validation, DNS/IP controls, bounds, WIT adaptation, Wasmtime isolation, or authorization binding are in scope.

The unprivileged `dekopond` gateway connects to chat services, listens on an owner-only Unix development transport, routes authenticated messages to catalog agents, and submits attested on-behalf-of proposals to `dekopon-brokerd`. It holds chat bot and model credentials and no provider credentials, policy, or authorization; message text and agent instructions are untrusted throughout. Findings in transport authentication, message-to-subject derivation, attestation claims, session bounds, credential handling, or the daemon's configuration hygiene are in scope.

Model credentials stay in the selected model client and never enter provider components. Dekopon does not import OAuth material from other applications. See [`docs/security-model.md`](docs/security-model.md) for current trust boundaries and limitations.

Scope-aware conversation history—private per canonical subject by default and explicitly shared only within one exact routed conversation ([`docs/security-model.md`](docs/security-model.md#conversation-memory-as-a-trust-surface))—durable on-demand chat memory, and explicit external-write capabilities are in scope. The chart separates broker UID 65532 from gateway UID 65533 through IPC group 65534; see the [current local process boundary](docs/security-model.md#current-local-process-boundary). A non-Unix or multi-tenant broker transport is absent; its introduction requires dedicated review and updated documentation. Crash durability, reconcilable state, and audit tamper-detection are [non-goals](docs/design.md#non-goals).
