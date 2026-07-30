# dekopon-brokerd

`dekopon-brokerd` is the separately deployed privileged Unix service for Dekopon provider components. It derives caller identity from Unix peer credentials, applies exact deny-by-default policy, restores replay identifiers from a verified owner-only audit chain, and executes only statically linked Dekopon host interfaces.

The owner-only socket intentionally supports one Unix UID trust domain. Every process running under that UID can act as its configured principal/actor; use a dedicated service/client UID when process-level separation matters. Request payloads cannot provide or override identity, policy, constraints, credentials, or authorization.

## Configuration

The configuration must be a regular single-link file owned by the server UID and must not be group/world writable. Socket and audit parent directories must be owner-only. Provider components must be regular single-link files owned by the server UID and must not be group/world writable; their canonical parent directories must also be server-owned and not group/world writable. Writable non-sticky path ancestors are rejected.

```yaml
apiVersion: dekopon.dev/brokerd/v1alpha1
socketPath: /home/dekopon/.local/run/dekopon/broker.sock
auditPath: /home/dekopon/.local/state/dekopon/audit.jsonl
brokerPrincipal: local-broker
policyRevision: policy-2026-01
providers:
  - /home/dekopon/lib/dekopon/echo-provider.wasm
identities:
  - uid: 1000
    principal: local-user
    actor:
      type: human
      principal: local-user
rules:
  - principal: local-user
    actor:
      type: human
      principal: local-user
    capability: echo.echo
    provider: echo
    effect: read-only
    risk: Low
    idempotency: idempotent
    constraints:
      timeoutMs: 30000
      maxOutputBytes: 1048576
```

Host, broker, and server limits have conservative defaults (including a 2 MiB frame ceiling) when their entire sections are omitted. When a section is present, every field is required. Unknown fields and unknown API versions are rejected. Startup also requires aggregate provider metadata and each mapped capability response to fit the frame ceiling. Shutdown grace must cover one configured host deadline plus two complete frame deadlines.

```console
chmod 0700 /home/dekopon/.local/run/dekopon /home/dekopon/.local/state/dekopon
chmod 0600 /path/to/broker.yaml
dekopon-brokerd --config /path/to/broker.yaml
```

SIGINT and SIGTERM stop acceptance, drain bounded in-flight connections, synchronize audit appends, log the verified chain head, and remove only the socket inode created by this process.

## Boundaries

- The service accepts one strict bounded request per fresh Unix connection.
- Peer UID mapping is trusted configuration; payload identity claims do not exist.
- Generic WASI and ambient I/O imports remain unavailable.
- The durable JSONL chain is mutation-evident and replay-restoring, but its logged checkpoint is not an externally anchored transparency service.
- Credential resolution is not implemented. Providers receive only explicitly linked Dekopon host interfaces and policy constraints.
- Direct `dekopon-run` subcommands retain their import-free host. Only explicit `dekopon-run broker` subcommands connect as unprivileged identity-free clients.
