# dekopon-harness

The unprivileged runtime for configured Dekopon agents, replacing `dekopon-agent` without a facade.
**Unreleased:** this new crate name is not the published 0.12.0 session crate.

- `session::SessionEngine` consumes `bootstrap::SessionBootstrap` and returns `SessionExit`.
- `runtime::ShellRuntime` observes actual nested direct-read-only/broker submissions and spends
  one job-wide capability budget. Help and builtins are not execution evidence.
- Bootstrap supplies fresh sorted capability descriptions and complete schemas before request one,
  using the same scoped snapshot as inspection and fallback shell discovery; explicit bounds refuse
  overflow rather than truncating schemas.
- `history`, `context` and `conversation` own bounded portable tool groups, execution evidence,
  generated-versus-accepted delivery and scoped append leases. `checkpoint` provides versioned
  bounded memory CAS storage, not crash durability or an effect receipt.
- `accounting::TokenTracker` is mandatory across calls, HTTP attempts, model segments and resume.
  Retain `JobAccounting` through delivery and finalize with the actual disposition. Unknown usage
  stays unknown; subset tokens and aggregation levels are never added twice.
- Optional controls consume fresh verified broker admission for configured model/effort choices;
  safe switches preserve evidence and budgets and discard incompatible continuation. Direct/replay
  runners omit controls even with provider broker legs.
- Skills, inspection, suggestions, assets/images, no-reply and exact-script replay remain supported.
  Structured activity is a nonblocking cosmetic seam, not authorization, history or delivery.

Only the separate broker authorizes effects. The harness depends on its lightweight protocol client,
never policy, privileged hosts or provider credentials. The gateway owns ingress, client caching,
Stop and transport receipts; the direct runner remains import-free/read-only.

See [`docs/harness.md`](../../docs/harness.md) for APIs, enforced bounds and **remaining integration
limitations**, [`docs/observability.md`](../../docs/observability.md#accounting) for accounting fields,
and [`docs/upgrading.md`](../../docs/upgrading.md) for lockstep migration. Publishing this new name
requires a later explicitly authorized bootstrap; no published-library compatibility is implied.

Ordinary broker-backed safe boundaries revalidate the authenticated surface/epoch; changed or
uncertain checks fence inference/disclosure without erasing observations. Inactive fenced jobs may
be evicted (restore then fails). Portable replay context uses job/call-local tool IDs. Recording
accounting includes failed and image calls independently of assistant turns; the legacy portable
history/multi-revision decoder remains incomplete. See [harness sessions](../../docs/harness.md).
