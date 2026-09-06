# Simplification execution ledger

Authority: [SIMPLIFY-BRIEF.md](SIMPLIFY-BRIEF.md). Core integration branch:
`simplify/2026-09`, base `542430e`. Console base `ef0bf3f`, separate repository and branch.
No releases, crate publication, yanks, PR merges, or live deployments are authorized.

## Milestone state

Brief corrections committed as `5a8687c` before implementation. The residue gate and
seven regression tests are installed in the gate commit (`Simplify-Unit: gate`). Next:
wave 0 with D6 first. Initial physical free space: 90 GiB; at most two simultaneous
build workers until measured headroom permits more. Other pre-existing worktrees remain
untouched. No targets existed in the inspected core/console worktrees at preflight.

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
