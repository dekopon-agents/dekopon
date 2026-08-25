# Security policy

## Supported versions

Dekopon has not yet made a production release. The latest pre-1.0 development line receives security fixes during bootstrap; no production security guarantees are made.

## Reporting a vulnerability

Use GitHub private vulnerability reporting for [`dekopon-agents/dekopon`](https://github.com/dekopon-agents/dekopon/security/advisories/new). Include affected versions or commits, impact, reproduction steps, and suggested mitigations if known.

Do not open a public issue for an unpatched vulnerability and do not include real credentials or sensitive third-party data in a report. If private reporting is unavailable, contact the organization owners through GitHub before disclosing details.

Maintainers will acknowledge a report when a human is available, assess scope, coordinate a fix and advisory, and credit reporters who want attribution. Because this is a volunteer pre-release project, no response-time SLA is promised.

## Scope notes

The workspace has four executable surfaces. `dekopon` reads local operator-provided catalogs and manages an isolated ChatGPT/Codex model login. The experimental `dekopon-run` can contact an operator-selected model endpoint, execute bounded import-free read-only Wasm components, or explicitly submit identity-free proposals to `dekopon-brokerd`. It has no broker authority or provider credentials; direct subcommands retain no provider host I/O, external-read authority, or local/external-write path.

This development line also contains a privileged asynchronous component host, bounded native HTTP engine, Cedar authorization over owner-authored execution constraints, an evidence/audit core, broker-owned destination-bound credential resolution, bounded identity-free Unix client protocol, and `dekopon-brokerd`. The broker executable accepts one owner-UID trust domain over a private socket, maps peer credentials through strict trusted configuration, restores replay state from verified durable audit, rejects rollback relative to an atomic owner-only checkpoint file, resolves legacy credentials or separately authorized public-DRN/private-map sources that no guest component can observe, and may expose policy-authorized provider HTTP. The JSONPlaceholder and `gh` demonstration providers separate read and external-write capabilities and are tested only with injected or loopback mocks. Findings in framing/deadlines, socket lifecycle/permissions, peer/server-UID validation, configuration ownership, authority omission, trusted-context binding, replay controls, policy matching, audit redaction/integrity, destination validation, DNS/IP controls, bounds, WIT adaptation, Wasmtime isolation, or authorization binding are in scope now.

The unprivileged `dekopond` gateway connects to chat services, routes authenticated messages to catalog agents, and submits attested on-behalf-of proposals to `dekopon-brokerd`. It holds chat bot and model credentials and no provider credentials, policy, or authorization; message text and agent instructions are untrusted throughout. Findings in transport authentication, message-to-subject derivation, attestation claims, session bounds, credential handling, or the daemon's configuration hygiene are in scope now.

Model credentials stay in the selected model client and never enter provider components. Dekopon does not import OAuth material from other applications. See [`docs/security-model.md`](docs/security-model.md) for current trust boundaries and limitations.

A dedicated gateway UID, conversation context and memory, non-Unix/multi-tenant broker transport, independently retained or signed evidence/audit anchors, and real third-party effects will materially expand the threat model; their introduction requires dedicated review and updated documentation.
