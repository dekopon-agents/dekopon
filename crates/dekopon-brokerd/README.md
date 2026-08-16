# dekopon-brokerd

`dekopon-brokerd` is the separately deployed privileged Unix service for Dekopon provider components. It derives caller identity from Unix peer credentials, applies exact deny-by-default policy, restores replay identifiers from a verified owner-only audit chain, maintains a separate atomic audit checkpoint, and executes only statically linked Dekopon host interfaces.

The owner-only socket intentionally supports one Unix UID trust domain. Every process running under that UID can act as its configured principal/actor; use a dedicated service/client UID when process-level separation matters. Request payloads cannot provide or override identity, policy, constraints, credentials, or authorization.

## Configuration

The configuration must be a regular single-link file owned by the server UID and must not be group/world writable. Socket, audit, checkpoint, and checkpoint-lock parent directories must be owner-only. Provider components must be regular single-link files owned by the server UID and must not be group/world writable; their canonical parent directories must also be server-owned and not group/world writable. Writable non-sticky path ancestors are rejected.

```yaml
apiVersion: dekopon.dev/brokerd/v1alpha1
socketPath: /home/dekopon/.local/run/dekopon/broker.sock
auditPath: /home/dekopon/.local/state/dekopon/audit.jsonl
checkpointPath: /home/dekopon/.local/state/dekopon/audit-checkpoint.json
checkpointLockPath: /home/dekopon/.local/state/dekopon/audit-checkpoint.lock
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

An optional `telemetry` section enables OTLP export of broker spans:

```yaml
telemetry:
  endpoint: http://rpi.localdomain
  transport: grpc            # grpc | http
  serviceName: dekopon-brokerd
  exportTimeoutMs: 5000
  telemetryPayloads: false
```

`telemetryPayloads: true` adds provider input and HTTP URLs to spans, declaring the telemetry sink in
scope for the data this broker handles. It never exposes a credential: `Redacted` values render
their marker in either mode, and durable audit records are unaffected either way.

It has no credential field by design. Ingest authentication is read from the standard
`OTEL_EXPORTER_OTLP_HEADERS` environment variable by the OpenTelemetry SDK, so a token never enters
this configuration file, the process command line, or a span attribute. Receiver routing travels
the same way: over gRPC OpenObserve reads the organization from an `organization` header and
rejects exports without it, so include `organization=<org>` alongside the token and
`stream-name`. Export failures disable
telemetry and log the reason rather than preventing startup. Broker logs are structured JSON on
stdout, filtered by `RUST_LOG`.

Host, broker, and server limits have conservative defaults (including a 2 MiB frame ceiling) when their entire sections are omitted. When a section is present, every field is required. Unknown fields and unknown API versions are rejected. Startup also requires aggregate provider metadata and each mapped capability response to fit the frame ceiling. Shutdown grace must cover one configured host deadline plus two complete frame deadlines.

```console
chmod 0700 /home/dekopon/.local/run/dekopon /home/dekopon/.local/state/dekopon
chmod 0600 /path/to/broker.yaml
dekopon-brokerd --config /path/to/broker.yaml
```

SIGINT and SIGTERM stop acceptance, drain bounded in-flight connections, synchronize audit/checkpoint appends, log the verified chain head, and remove only the socket inode created by this process.

## Audit checkpoint and recovery

The checkpoint is one strict, hard-4-KiB-bounded, newline-terminated JSON object with API version `dekopon.dev/audit-checkpoint/v1alpha1`, the retained record count, and the SHA-256 chain head. A dedicated owner-only lock permits one broker writer. Every audit append is synchronized before the checkpoint is written to a new owner-only file, synchronized, atomically renamed, and followed by a parent-directory synchronization.

At startup, the checkpoint must identify an exact prefix of the fully verified audit chain. This detects replacement, truncation, and valid-prefix rollback relative to the retained checkpoint. An audit that is exactly one record ahead of a valid checkpoint is the recoverable crash window and advances the checkpoint; a larger gap fails closed. A non-empty audit without a checkpoint, or any checkpoint that is not a retained prefix, fails closed and requires explicit operator recovery from trusted copies. Do not delete only one file to bypass recovery.

The backing filesystem must honor Unix no-follow opens, advisory exclusive locks, same-directory atomic rename, and file/directory synchronization. Retain or export checkpoint generations in an independently protected system if rollback by the host owner or storage administrator is in scope. Deleting or rolling back both local files together cannot be detected by local state alone.

## Boundaries

- The service accepts one strict bounded request per fresh Unix connection.
- Peer UID mapping is trusted configuration; payload identity claims do not exist.
- Generic WASI and ambient I/O imports remain unavailable.
- The durable JSONL chain is mutation-evident and replay-restoring. The separate atomic checkpoint makes the retained head externally inspectable, but is not signed, remote, append-only, or a transparency service by itself.
- Credential resolution is not implemented. Providers receive only explicitly linked Dekopon host interfaces and policy constraints.
- Direct `dekopon-run` subcommands retain their import-free host. Only explicit `dekopon-run broker` subcommands connect as unprivileged identity-free clients.
