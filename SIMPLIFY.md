# Simplification execution ledger

Authority: [SIMPLIFY-BRIEF.md](SIMPLIFY-BRIEF.md). Core integration branch:
`simplify/2026-09`, base `542430e`. Console base `ef0bf3f`, separate repository and branch.
No releases, crate publication, yanks, PR merges, or live deployments are authorized.

## Milestone state

**Native child startup cleared; wave 0 implementation reached validation.** The successful
native async smoke receipt is `19a218a8-f0d8-472a-8465-2fe3d73c9151`, supplied to the
read-only preflight (`.validation/simplify-2026-09/wave0-preflight.md` in the workspace).
This worker did not independently rerun that smoke. The foundation residue tool's seven
regression tests were rerun successfully by D6a.

**D6a and D8a are freshly reviewed and integrated; sequential pair gates passed.**
The owner authorized the focused BrokenPipe fixture correction described in the D6a brief
contract. The unchanged test reproduced the failure; native partial-write and source proof
established the invalid atomic-write assumption. Sending the real oversized `runCommand`
frame's length alone preserves every existing wire-code, log-cause, bound and redaction
assertion, and proves pre-body refusal without draining or raising broker limits.
The new socket code's Clippy `collapsible_if` was corrected with an equivalent let-chain,
within the owner's authorization to repair defects in the new D6a implementation; no
suppression or assertion weakening was added.

Native package check/test, fmt, scoped all-target/all-feature Clippy, doctest/rustdoc,
documentation/audit-event, metadata and mechanical gates passed. Residue remains exactly
two permitted execution-brief mentions, not zero raw hits. Required Linux-root disposable
container acceptance passed with broker UID 65532, gateway UID 65533 and IPC group 65534;
owner access, unmapped-peer, server pin/live-peer, GID and private-file refusals all ran.
Evidence: workspace `.validation/simplify-2026-09/D6a/repair-report.md`, commit identity
`Simplify-Unit: D6a`. Fresh review passed at worker commit
`5741a42817c78b3ee4d8d779c51686f5dfbd8f69` (`D6a/review.md` in that evidence directory).
Console D8a integrated commit: `97e8625617aa44dbc4b62a58d6f005b98d0c7af6`, reviewed
worker `b5aa18afbe862ff48c85ac9a6c209b85fc4a6873` (`D8a/rereview.md`).
This commit identifies itself by `Simplify-Unit: D6a`; resolve its integrated SHA in
the next work metadata. Pair results and exact heads are recorded in workspace
`.validation/simplify-2026-09/wave0-pair1-integrated.md`. Integrated touched-package
check/test (core 132, console 91; zero failed/ignored), fmt, scoped Clippy, doctest/rustdoc,
doc/script/release-metadata/residue and privilege gates passed. All 224 Linux acceptance
source/fixture hashes match the integrated core; native owner-only proof is not cross-UID.
Only ledger metadata differs from either reviewed tree. D6b chart/init acceptance, the
full wave workspace gates, final exact-head CI and physical-TTY proof are not claimed.

### Historical infrastructure failure

Workflow `e8209c58-a289-47fd-89b7-1402eef7fb73`, attempt
`06cff1ef-0486-4a77-b678-650402a1c5af`, failed at `review-foundation` before any child
session was persisted. The installed package lacked `@earendil-works/pi-server`,
`@earendil-works/pi-server/unix`, and `@earendil-works/pi-client/unix`. No unit ran in
that attempt. At inspection, core worktrees were clean at `2010f55`, console worktrees
at `9b6c068`; sources and compiler cache were preserved. That historical failure is
superseded by the successful native-smoke receipt above, not an active setup blocker.

The coordinator removed the old worker targets after the prior stop, restoring 89 GiB.
This repair started at 89 GiB; native target reached 5.5 GiB with 83 GiB physical free.
Build variants were serialized. Linux acceptance's disposable target reached 3.9 GiB;
physical free was 80 GiB before and after owned-container cleanup (Docker guest reclamation
is not an immediate host physical-space increase). The container, its source/build/registry
artifacts and the 5.9 MB staging archive were removed; no host mounts or real credentials
were used. Native target was retained for fresh review/integration, then the integrator removed
its exact inactive ignored 5.5 GiB target immediately after cherry-pick (physical free
78.83 to 84.09 GiB). D8a target was already absent. Both registered source worktrees
stay until the full wave boundary. Integration targets will be retained only for the
immediate remaining wave-0 gates, with the final gate/cleanup trigger recorded in
the pair evidence. Shared compiler settings/cache and others' artifacts
were untouched. At pair-gate handoff, integration targets measured core 5.5 GiB and
console 1.7 GiB, with 77.17 GiB physical free (next width 2 budget: strictly over 60 GiB).
Retain only for immediate D6b/D1a/D1b and D8b/remaining wave-0 integration gates; remove
once those builds finish or earlier if headroom requires it. Push receipts for the
validated integration branches belong to the pair evidence. No PR, deployment, release,
publication, tag or merge is authorized at this boundary.

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
landed — implementation, unit acceptance and fresh review complete; integrated on
`simplify/2026-09`. Reviewed worker: `5741a42817c78b3ee4d8d779c51686f5dfbd8f69`;
integrated self identity `Simplify-Unit: D6a`; evidence: workspace
`.validation/simplify-2026-09/D6a/repair-report.md`. Owner-approved oversized-frame fixture
repair preserves every existing assertion; all scoped gates and required real cross-UID
acceptance passed. IPC GID derives from the broker-owned parent (no new config field).
Private credential/config/provider/store boundaries and owner-only clients are preserved.
Discoveries: broad deployment/architecture prose and chart init proof remain D6b-owned.

### D6b
pending — chart/init ownership and deployment proof/docs, after D6a; commit: —; discoveries: —.

### D1a
pending — Slack single 429 retry; commit: —; discoveries: —.

### D1b
pending — policy and transport aggregate refusals; commit: —; discoveries: —.

### D8a
landed — console chat mode, freshly re-reviewed and integrated in the console repo;
commit: `97e8625617aa44dbc4b62a58d6f005b98d0c7af6`; reviewed worker:
`b5aa18afbe862ff48c85ac9a6c209b85fc4a6873`; evidence: workspace
`.validation/simplify-2026-09/D8a/rereview.md`; discoveries: D8b remains separate.

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
