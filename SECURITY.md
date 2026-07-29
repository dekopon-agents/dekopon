# Security policy

## Supported versions

Dekopon has not yet made a production release. The latest pre-1.0 development line receives security fixes during bootstrap; no production security guarantees are made.

## Reporting a vulnerability

Use GitHub private vulnerability reporting for [`dekopon-agents/dekopon`](https://github.com/dekopon-agents/dekopon/security/advisories/new). Include affected versions or commits, impact, reproduction steps, and suggested mitigations if known.

Do not open a public issue for an unpatched vulnerability and do not include real credentials or sensitive third-party data in a report. If private reporting is unavailable, contact the organization owners through GitHub before disclosing details.

Maintainers will acknowledge a report when a human is available, assess scope, coordinate a fix and advisory, and credit reporters who want attribution. Because this is a volunteer pre-release project, no response-time SLA is promised.

## Scope notes

The workspace has two CLI surfaces. `dekopon` reads local operator-provided catalogs and manages an isolated ChatGPT/Codex model login. The experimental `dekopon-run` can contact an operator-selected model endpoint and execute bounded, import-free, read-only Wasm components. It has no broker authority, provider credentials, provider host I/O, provider external-read authority, or local/external-write path.

The `0.2.0` development line also contains a privileged asynchronous component-host library and bounded native HTTP engine. They are exercised only by deterministic loopback tests; no workspace executable authenticates callers, constructs authorization, or exposes that host path. Findings in destination validation, DNS/IP controls, bounds, WIT adaptation, Wasmtime isolation, or authorization binding are in scope now.

Model credentials stay in the selected model client and never enter provider components. Dekopon does not import OAuth material from other applications. See [`docs/security-model.md`](docs/security-model.md) for current trust boundaries and limitations.

Future daemons, policy, broker credentials, deployable privileged provider calls, durable evidence, and external effects will materially expand the threat model; their introduction requires dedicated review and updated documentation.
