# Roadmap

Roadmap items describe sequencing. They are intentions, not shipped behavior, not delivery commitments, and not permission to bypass the invariants in [`design.md`](design.md) and [`security-model.md`](security-model.md). Shipped history lives in [`CHANGELOG.md`](../CHANGELOG.md).

## Next milestones

- Identity, context, memory, observability, MCP interoperability, and multi-agent review each arrive only with tested user-facing behavior.
- Memory that outlives one conversation: automatic replay across conversations, sharing across agents, task and fact storage, semantic or vector retrieval, durable shared namespaces, and deletion and export.
- `dekopon policy explain` and `auth can-i`: ask the broker for the determining policy identifiers without making an effect happen. This is also the first CLI-to-broker integration and inherits that whole boundary discussion.
- Cedar context conditioned on arbitrary provider input. Untrusted open JSON has no settled schema; the public DRN is the narrow exception that proves the rule, since one strongly typed top-level resource goes through a separate `secret.use` action and can never widen its owner binding.
- Actor kind, human versus service, in policy context. The broker knows it and policy cannot read it. Cheap to add and easy to add wrongly, since it invites rules that look like identity checks but are transport facts.
- The principal axis of credential selection — approve as the person who asked. That requires per-person credentials and explicit authorization. The legacy `credential`/`credentialByAgent` bindings will be replaced by public DRNs, so this must build on that direction rather than add another legacy override ([migration requirements](design.md#legacy-credential-bindings)).

## Intended package namespace

These names are reserved for future crates. None is present in the workspace, and none is a crates.io reservation or a published package. A crate arrives only with meaningful, tested behavior an implemented milestone needs; tightly coupled crates stay in this monorepo and share one pre-1.0 release line.

- `dekopon-identity`
- `dekopon-context`
- `dekopon-memory`
- `dekopon-tribunal`
- `dekopon-mcp`
- `dekopon-observe`

## Non-goals

The non-goals are in [`design.md`](design.md#non-goals); a roadmap item does not reopen one.
