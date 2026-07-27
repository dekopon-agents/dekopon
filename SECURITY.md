# Security policy

## Supported versions

Dekopon has not yet made a production release. The latest `0.1.x` code on `main` receives security fixes during bootstrap; no production security guarantees are made.

## Reporting a vulnerability

Use GitHub private vulnerability reporting for [`dekopon-agents/dekopon`](https://github.com/dekopon-agents/dekopon/security/advisories/new). Include affected versions or commits, impact, reproduction steps, and suggested mitigations if known.

Do not open a public issue for an unpatched vulnerability and do not include real credentials or sensitive third-party data in a report. If private reporting is unavailable, contact the organization owners through GitHub before disclosing details.

Maintainers will acknowledge a report when a human is available, assess scope, coordinate a fix and advisory, and credit reporters who want attribution. Because this is a volunteer pre-release project, no response-time SLA is promised.

## Scope notes

The `0.1.0` CLI parses local operator-provided files and performs no model interaction, provider execution, credential access, or network daemon communication. Future broker and provider-host components will materially expand the threat model; their introduction must include dedicated review and updated documentation.
