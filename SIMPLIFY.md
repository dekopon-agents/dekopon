# Simplification execution ledger

Authority: [SIMPLIFY-BRIEF.md](SIMPLIFY-BRIEF.md). Core integration branch:
`simplify/2026-09`, base `542430e`. Console base `ef0bf3f`, separate repository and branch.
No releases, crate publication, yanks, PR merges, or live deployments are authorized.

## Milestone state

**Blocked before implementation: native async child startup failed.** Brief corrections
are committed as `5a8687c`; the residue gate is `2010f55` and its seven regression tests
pass. Workflow `e8209c58-a289-47fd-89b7-1402eef7fb73` failed at `review-foundation` before
any child session was created (attempt `06cff1ef-0486-4a77-b678-650402a1c5af`). No unit
worker ran. The run cannot resume: no child session file was persisted.

Exact setup failure: the installed `@earendil-works/pi-coding-agent` package does not
provide `@earendil-works/pi-server`, `@earendil-works/pi-server/unix`, or
`@earendil-works/pi-client/unix`; its async runner therefore cannot create child sessions.
Repair native package/dependency availability before a clear same-protocol retry. Do not
switch to foreground agents, standalone CLI runners, or another execution mode.

At failure inspection: core integration and all 16 core unit worktrees are clean at
`2010f55`; console integration and its two unit worktrees are clean at `9b6c068` (base
`ef0bf3f`). All are preserved. No cargo/rustc processes or owned target directories exist.
Core brief/gate were pushed to `origin/simplify/2026-09`; no PR was opened. This failure
checkpoint records infrastructure state only, not a separately updated implementation unit.

Initial physical free space: 90 GiB; after preparing worktrees, 89 GiB. Permit at most two
simultaneous build workers until measured headroom allows more. Other pre-existing
worktrees and the shared compiler cache remain untouched. Next executable action after
repair: rerun the bounded native workflow, starting with foundation review and D6a.

## Contract reconciliation and discoveries

- D1 has two worker slots as §9 and the effort list require: D1a Slack retry; D1b both
  policy-world and transport aggregate refusals. No refusal behavior was discarded.
- D3d is inside D3c; D3g precedes it. D3c owns atomic packaging residue; D3f is prose only.
- D6b integrates after D6a in wave 0. Cross-UID proof, not chmod-only tests, is required.
- Raw residue must report historical brief/ledger/changelog hits and explicitly named
  future D2c/D2d ownership. Undescribed active residue blocks; workers cannot invent waivers.
- A commit cannot store its own SHA. A unit's work commit stores `Simplify-Unit: <ID>`
  and its status; the next integration update records the resolved SHA. No separate
  ledger-only update commits. Final housekeeping removes this ledger before the PR.
- Baseline doc inventory shows examples/local has surviving config/skill readers; D7b's
  amended contract preserves those invariants through remaining fixtures before deletion.

## Units

### D6a
pending — distinct-UID daemon/protocol socket boundary; commit: —; discoveries: —.

### D6b
pending — chart/init ownership and deployment proof/docs, after D6a; commit: —; discoveries: —.

### D1a
pending — Slack single 429 retry; commit: —; discoveries: —.

### D1b
pending — policy and transport aggregate refusals; commit: —; discoveries: —.

### D8a
pending — console chat mode (console repo); commit: —; discoveries: —.

### D8b
pending — existing console shell without model credential (console repo); commit: —; discoveries: —.

### D3a
pending — recorded-session/replay deletion; commit: —; discoveries: —.

### D3b
pending — two-daemon OTLP smoke and stub model; commit: —; discoveries: —.

### D3g
pending — authenticated broker probe/chart consumers; commit: —; discoveries: —.

### D2a
pending — direct storage handle, no transaction/GC; commit: —; discoveries: —.

### D2b
pending — checkpoint removal; commit: —; discoveries: —.

### D7a
pending — webui and brokerd embedding retirement; commit: —; discoveries: —.

### D7b
pending — gateway ChatGPT auth then CLI retirement; commit: —; discoveries: —.

### D2c
pending — append-only audit JSONL; commit: —; discoveries: —.

### D3c
pending — atomic run/provider-host retirement and packaging; commit: —; discoveries: —.

### D3e
pending — opposite-direction daemon dependency gates; commit: —; discoveries: —.

### D2d
pending — durability documentation sweep; commit: —; discoveries: —.

### D3f
pending — final changelog/component ownership; commit: —; discoveries: —.
