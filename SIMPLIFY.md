# Simplification execution ledger

Authority: [SIMPLIFY-BRIEF.md](SIMPLIFY-BRIEF.md). Core integration branch:
`simplify/2026-09`, base `542430e`. Console base `ef0bf3f`, separate repository and branch.
No releases, crate publication, yanks, PR merges, or live deployments are authorized.

## Current milestone — wave 0 accepted

All six planned wave-0 units are freshly reviewed and integrated. Stable full-workspace
fmt, warnings-denied all-target/all-feature Clippy, all-feature tests with no-fail-fast,
separate doctests, warnings-denied rustdoc, machete, dependency policy, both privilege
boundaries, docs/audit/release-metadata/regressions, corrected mechanical checks and
selected workspace archive verification passed at the integrated D1b head
`124ff2cd6bcb8c27070af53b413dfa7f23d0fa37`. This final D1b amendment changes only this
ledger; source/archive-input equivalence is recorded, not a relabeled fresh build.
The full stable suite's existing ignored SDK doctest example remains explicitly reported.

Helm's nine actual CI run bodies passed, including render/schema/archive parity and
actual rendered init commands on amd64 and arm64. All 224 inputs match batch 3's exact-head
real Linux cross-UID daemon proof: mapped/owner success, unmapped/pin/live-peer/GID/private-
file refusals. No redundant provider rebuild: all defined provider/WIT/source inputs remain
byte-identical to the pinned base. Raw removal residue is D6a's two brief hits and D1b's
five brief/history hits; other wave-0 inventories have no hits. No suppression or surviving
assertion weakening was introduced.

Known integrated work identities: D6a `302cff4922c189a7783248ce159764a29765c522`,
D6b `020f0bd72b7ad5ff1b2a14e857c703cbac6a052f`,
D1a `88bc2d6da32ebdf901fe03f159a8ec0794b14816`,
D1b self `Simplify-Unit: D1b` (prior validated integrated SHA above),
console D8a `97e8625617aa44dbc4b62a58d6f005b98d0c7af6` and
D8b `e3896db08d675921070b5e17d021ee70c5293904`.

Console final housekeeping removed only its ledger at
`18c3d890c52bea72fc45ed5bad320788b2ef6ecb`. Complete actual CI/local scope passed on
stable and Rust 1.89.0 (97 distinct tests per run, no failures/ignores); published pins
remain exactly `=0.11.1`. Exact-head Linux CI run `34058770775` passed. Its one PR is open,
ready for human review, not approved or merged:
https://github.com/dekopon-agents/dekopon-console/pull/1 .
No physical-TTY or deployed/live-credential acceptance is claimed.

The six clean inactive integrated unit worktrees were normally removed and pruned after
patch/source-equivalence checks; all branches and evidence remain. Console's inactive
ignored target was removed after exact-head CI; no console/provider targets remain.
Physical free space is 53.43 GiB after cleanup (serial width 1; below two-worker budget).
Core target is retained only for immediate wave-1 integration gates; reclaim after those
gates, before handing off without immediate local builds, or before free space falls below
the serial >30 GiB budget. Shared sccache/wrapper/cache/incremental settings are untouched.

Acceptance/evidence and remaining exact §8 final-only coverage:
workspace `.validation/simplify-2026-09/wave0-final.md` and `w0-full-gate/`.
Final core MSRV/release/feature-off/provider variants, daemon help/install and rewritten
OTLP smoke, final metrics and exact-head core CI remain outstanding. No core PR yet;
core integration stays open for wave 1. No new feature work or stop trigger at this gate.

## Prior milestone receipts (historical)

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
D6a integrated at `302cff4922c189a7783248ce159764a29765c522`. Pair results and exact heads are recorded in workspace
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
integrated commit `302cff4922c189a7783248ce159764a29765c522`; evidence: workspace
`.validation/simplify-2026-09/D6a/repair-report.md`. Owner-approved oversized-frame fixture
repair preserves every existing assertion; all scoped gates and required real cross-UID
acceptance passed. IPC GID derives from the broker-owned parent (no new config field).
Private credential/config/provider/store boundaries and owner-only clients are preserved.
Discoveries: broad deployment/architecture prose and chart init proof remain D6b-owned.

### D6b
landed — freshly re-reviewed and integrated into `simplify/2026-09`.
Reviewed worker: `c4e471a09f85da7ced8018dc11a50afad8e2dbf7`; review:
workspace `.validation/simplify-2026-09/D6b/rereview.md`. Only integration ledger metadata
differs from the reviewed tree. Batch exact-head gates and cleanup receipts are recorded
in workspace `.validation/simplify-2026-09/wave0-batch-2.md`. Worker target was absent
immediately after cherry-pick; registered source stays until the wave boundary. Integration
target stays only for immediate D1a/D1b and full wave-0 gates, then is reclaimed.
Self identity: `Simplify-Unit: D6b`, branch `simplify/unit-D6b` (one amended work commit).
Base/D6a integrated SHA: `302cff4922c189a7783248ce159764a29765c522`; prior console D8a
integrated SHA: `97e8625617aa44dbc4b62a58d6f005b98d0c7af6`. Evidence: workspace
`.validation/simplify-2026-09/D6b/report.md`.

Chart pod/broker stays 65532:65532; gateway container is 65533:65533; supplementary
IPC group 65534 reaches the broker-owned 0710 parent / 0660 socket. Gateway pins
serverUid 65532 and realistic broker configuration maps UID 65533 separately from
owner probes. Config tmpfs, temporary volumes and state subdirectory mounts are separate;
private files stay 0600, directories 0700, with no fsGroup. The claim root stays root-owned.
Rendered init commands and distinct-UID Linux OS processes prove mutual private-file
denials, group connection and socket replacement refusal. Real-daemon mapped/unmapped,
server pin/live-peer acceptance remains D6a's unchanged Linux proof, not the Python layout
fixture: 223 of its 224 source/fixture hashes match; only the gateway README changed.
Full Helm CI lane (render variants, schema, package parity, refusal and init tests),
arm64 init acceptance, docs/audit-event, format, metadata and corrected mechanical gates
passed. No Rust source or assertion changed; seven literal removed resource/anchor names
have zero raw non-Rust residue. No Cargo target was created; owned disposable Docker
containers/volumes were cleaned. Physical free space stayed above the two-worker 60 GiB gate.

Focused review repair of `53b198be21a09d63967b9c8f7e5f5a03fdb939f0`: register each
non-preexisting volume before interruptible creation. The actual allocation/cleanup prefix
now has bounded Docker-stub controls for allocation-then-failure (42), TERM (143), preserved
preexisting-object refusal, and successful cleanup. The original prefix fails the negative
control with an allocated but untracked volume; the repaired prefix cleans every allocation.
These are cleanup-control tests, not substitutes for real Docker layout or daemon proof.
Restored the released 0.4 roadmap socket fact without changing current D6 documentation.
Full Helm lane and existing boundary/seed/refusal assertions pass after this focused repair;
no other implementation or assertion changed. Evidence: workspace
`.validation/simplify-2026-09/D6b/review-repair.md` (amended exact identity resolved there).

Discoveries for following owners: D2b must also remove checkpoint names from the new
old-layout refusal loop; D2d still owns broad durability prose, and D7a owns retirement of
broker-http.md. Existing claim upgrades require offline broker/ subdirectory and gateway
credential ownership migration; init refuses unmigrated default broker files without
changing their bytes. This unreleased chart requires a matching image containing D6a;
no image publication/deployment, D3g probe change or D7 auth move was performed.

### D1a
landed — freshly re-reviewed and integrated into `simplify/2026-09`.
Reviewed worker: `96b345057010cbc9f30444ed69be5e844d058522`; fresh review:
workspace `.validation/simplify-2026-09/D1a/rereview.md`.
Batch gates and exact integrated identities are recorded in workspace
`.validation/simplify-2026-09/wave0-batch-3.md`. Only ledger integration metadata
differs from the reviewed tree. Worker target was verified inactive/ignored and
removed immediately after cherry-pick; registered source stays until wave boundary.
Self identity: `Simplify-Unit: D1a`, branch `simplify/unit-D1a` (one work commit).
Base/D6b integrated SHA: `020f0bd72b7ad5ff1b2a14e857c703cbac6a052f`;
D6a: `302cff4922c189a7783248ce159764a29765c522`; console D8a:
`97e8625617aa44dbc4b62a58d6f005b98d0c7af6`; console D8b:
`e3896db08d675921070b5e17d021ee70c5293904`.
Ported only text-post 429 backoff/retry, with unchanged receipt validation and a
payload-free warning. The bounded loopback session test proves identical posts,
a one-second wait, one accepted delivery and answered rather than reply-failed.
No identifiers, APIs, paths or tests removed; no surviving assertions changed.
Existing shared capture support and tracing-subscriber are test-only dependencies;
the generated lock adds only those two edges. Check/test (206 tests), scoped Clippy,
fmt, doc/event, metadata, dependency and mechanical gates pass. Evidence: workspace
`.validation/simplify-2026-09/D1a/report.md`. Worker target reclaimed at integration. Integration target retained only for the
immediate full wave-0 gate, then reclaimed. Discoveries: none.
Focused review repair: moved only the Slack retry bullet from released 0.12.0 to
Unreleased / Fixed; released history now matches the integrated base byte-for-byte.
Original reviewed identity: `4bf567dde3d64b113f05883a1a1a880e61cf2910`;
same unit amended under `Simplify-Unit: D1a`. Rust source and tests unchanged.
Repair validation: workspace `.validation/simplify-2026-09/D1a/review-repair.md`.

### D1b
landed — freshly re-reviewed and integrated into `simplify/2026-09`.
Reviewed worker: `7a846d9be351bc130d49209cfa17f0b8cdc715c5`; fresh review:
workspace `.validation/simplify-2026-09/D1b/rereview.md`.
D1a integrated SHA: `88bc2d6da32ebdf901fe03f159a8ec0794b14816`.
Batch gates, reviewed-to-integrated differences and cleanup receipts are recorded in
workspace `.validation/simplify-2026-09/wave0-batch-3.md`. Auto-merged disjoint
changelog/test/ledger additions; no semantic resolution. Integration amendments
change only ledger metadata; D1a source additions are preserved.
Self identity: `Simplify-Unit: D1b`, branch `simplify/unit-D1b` (one work commit).
Base/prior D6b integrated SHA: `020f0bd72b7ad5ff1b2a14e857c703cbac6a052f`;
D6a: `302cff4922c189a7783248ce159764a29765c522`; console D8a:
`97e8625617aa44dbc4b62a58d6f005b98d0c7af6`; D8b:
`e3896db08d675921070b5e17d021ee70c5293904`.
Both conflict classes and every transport connect failure are reported together, without
changing credential resolution, protocol inventory validation or authority boundaries.
The two authorized policy assertions are re-expressed more precisely; the mixed four-name
case and two real local-transport refusals pass. Package check/test, scoped Clippy,
format, docs/audit-event, metadata and corrected mechanical gates pass. Raw non-Rust
removal residue is confined to the canonical execution brief and a historical CHANGELOG
entry (reported, not suppressed).
Evidence: workspace `.validation/simplify-2026-09/D1b/report.md`; focused R1 repair:
`.validation/simplify-2026-09/D1b/review-repair.md`.
Review of `6a8e866d43d5f8afd684f9dcc99fc6d2f4ccb1b6` identified credential-bearing
Telegram request URLs in aggregate diagnostics. The same amended unit strips reqwest URLs
at getMe send and shared response-body error construction, preserving typed causes and
underlying sources. A bounded two-synthetic-token loopback regression covers send failure,
truncated body, Display, Debug and the gateway exit-chain renderer. It fails before the
repair and passes after; all prior assertions and scoped package/mechanical/boundary gates
remain green. No unrelated transport paths changed. Fresh re-review passed at the identity above.
Discoveries: R1 resolved by this focused repair. Worker target was verified inactive,
ignored and rebuildable and removed immediately after cherry-pick. Registered source
stays until wave boundary; integration target stays only for the immediate full wave-0
gate, then is reclaimed.

### D8a
landed — console chat mode, freshly re-reviewed and integrated in the console repo;
commit: `97e8625617aa44dbc4b62a58d6f005b98d0c7af6`; reviewed worker:
`b5aa18afbe862ff48c85ac9a6c209b85fc4a6873`; evidence: workspace
`.validation/simplify-2026-09/D8a/rereview.md`; discoveries: D8b remains separate.

### D8b
landed — existing console shell without model credential, freshly reviewed and integrated
in the console repository; commit: `e3896db08d675921070b5e17d021ee70c5293904`; reviewed worker:
`943636e6c60bd3635d4447404d6f5616709f7551`; evidence: workspace
`.validation/simplify-2026-09/D8b/review.md`. Normal turn/chat modes and all prior
assertions remain; seven published requirements stay `=0.11.1`. Discoveries: none.

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
