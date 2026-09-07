# Simplification execution ledger

Authority: [SIMPLIFY-BRIEF.md](SIMPLIFY-BRIEF.md), including the owner-authorized §5
repair rules committed in `6537a0823567ba6e9e0dd6cb1c515c3651ca29c1`.
Core: `simplify/2026-09`, original base `542430e`; accepted pre-batch head `6537a08`.
Evidence paths below are relative to workspace `.validation/simplify-2026-09/`.
No releases, tags, crate publication/yanks, merges, live deployment or core PR yet.

## Current milestone — wave-1 pair-2 combined local acceptance complete

Accepted pre-pair base `0074ee38e8a5bbf5af56a33d9f7ddc575c86fb0b`; exact D2a/D3b reviews
pass. Integrated D2a is `0a18d30a7ea7ad65847d01bafa3f9f88615fb78e`; final integrated
D3b is this commit (`Simplify-Unit: D3b`), with that exact parent and two unit commits.
Independent changelog/doc edits are composed; this closing amendment changes ledger
metadata only. Actual nativeRustHead and smokeHead both remain
`fefa4364acf739f33bea73bcf27391cb68b7cb09`, preserved by the create-only recovery ref
`recovery/pair2-validated-before-ledger-close-202609`. All 393 non-ledger source paths
retain aggregate `f6bb24c63930145cb7b72eb8b220a366114299070eac4a4d0b1f0ab81465c3f8`.

Current-source nine-package locked check/test/no-fail-fast, all-target/all-feature
Clippy with warnings denied, warnings-denied rustdoc and separate doctests pass:
496 distinct native tests plus 9 doctests, zero failed/ignored; raw 499 native successes
exclude three IPC child repeats. Actual docs/audit, scripts/actionlint, metadata/release,
dependency/privilege, nine chart CI bodies, mechanical and current-residue gates pass.
Nine actual package LIST/content receipts pass, not archive compilation. Six prior
actual deterministic provider comparisons/variant receipts are verified by exact
input/log/artifact identity; no new provider builds. Saved fresh rendered HTML and
static Schemars description metadata proof close the consumers; the latter is not a
generated-schema byte-identity claim.

Fresh real two-daemon smoke passes: 16 spans across all five required families,
15 local/shipped = 15 ingested = 15 retrieved rows, no rejected/partial/truncated rows,
both actual native daemon correlations, all ten controls and complete payload/dummy-
credential redaction. Production logs remain stdout-only. Current original D2a 340 /
active 339 literal and D3b eight-literal inventories remain explicit: pre-amendment raw
statuses 1, hits 7401/7400/6, zero unknowns, not zero raw residue or a blanket waiver.
The restored private native_loaded_bytes has four live producer/reader/init/declaration
occurrences; it is not retired transaction state. Final ledger line classifications
and affected non-build checks are recorded externally at their actual final head.

Prior auxiliary lookup used provider-storage's mirror-test filename under broker-host;
actual mirror tests and byte proof passed. This was inspection incompleteness, not a
failed product or missing required gate (`pair2-core-resume/parent-close-cause-proof.json`).
Prior stops/raw receipts remain historical. This closing call runs no native/provider
builds, Cargo test/doc/package reruns or smoke executions. Safe prior CORE target cleanup
reclaimed 9.513 GiB physical (68.554 → 78.068 GiB); no duplicate closing-call credit.
CORE/D2a/D3b root/provider targets and owned smoke resources were already absent; final
absence/free-space verification and any guarded metadata-only cleanup are external.
Shared cache/settings, worker sources and all previous recovery refs remain preserved.

No push is claimed inside this amendment: normal fast-forward push requires all affected
final non-build/source/residue gates green and live remote still at the accepted base.
Exact final head, push/remote equality and cleanup receipts live in
`pair2-core-resume/close-result.md` and `pair2-consumers-accepted.md`. Earlier integration
history remains in `pair2-integrate/` and `pair2-review-tail-integrated.md`. Final-source
full-wave workspace/archive/MSRV and exact-head core CI remain downstream; this scoped
pair acceptance is not final-wave or final-PR acceptance.

## Accepted milestone — wave1-pair1-resumed integration

D3a and D3g are freshly reviewed and serially integrated. Only ledger metadata and
the independent Unreleased changelog entries required conflict resolution; source is
unchanged from the reviewed unit patches. Batch scoped gates pass; validated push is
recorded externally after this same-unit ledger amendment. One writer, serial
builds with strictly >30 GiB free (two need >60 GiB; maximum two). D3a's exact inactive,
ignored 4.8 GiB target was removed immediately after cherry-pick (physical free 76.84 →
81.46 GiB); D3g target is absent. Registered worker sources stay until the wave boundary.
Integration target grew 0 → 1.1 → 7.2 → 7.7 GiB across check/test/Clippy-doc gates and
was safely reclaimed after the final named local gate (physical free 73.71 → 81.00 GiB).
No local builds remain scheduled for this pair; no speculative target retention.
Shared sccache/wrapper/cache/incremental settings remain unchanged.

Mechanical consequences are repaired inside their owning unit, recorded, re-gated and
freshly reviewed. D3a Prompt.model boxing is explicitly authorized. Unapproved surviving
behavior/meaningful API/credential-boundary changes stop; a required gate still failing
after bounded in-scope repair stops. No suppressions, weakened assertions or hidden residue.
Integration may resolve independent mechanical conflicts and ledger metadata only.
Evidence: `wave1-pair1-resumed-integrated.md`; detailed receipts in `wave1-pair1-resumed/`.

Pair gate source head `72db95f540b6d507a871ccfc8afd9583b6163b27`: scoped locked
check/test (481 distinct tests, zero failed/ignored, plus three IPC child reruns),
all-target/all-feature warnings-denied Clippy, fmt, doctests/rustdoc, machete/deny,
docs/audit/release metadata/regressions, scripts/actionlint, all nine Helm CI bodies
including rendered init, cleanup controls, both privilege directions and mechanical
checks pass. The final D3g amendment changes only this ledger; source equivalence and
exact final non-build checks are recorded, not relabeled fresh builds. Raw residue
remains D3a seven lines (eight expanded hits) and D3g nine hits, fully classified.

## Accepted receipts and remaining gates

Wave 0's six units passed fresh review and integration. Stable full-workspace fmt,
all-target/all-feature warnings-denied Clippy, all-feature tests/no-fail-fast, separate
doctests, warnings-denied rustdoc, machete, deny, both privilege directions, docs/audit,
release metadata/regressions, mechanical and selected archive gates passed at
`124ff2cd6bcb8c27070af53b413dfa7f23d0fa37`; final D1b ledger-only amendment has identical
source/archive inputs. Existing ignored SDK doctest remains explicitly reported.
All nine Helm CI bodies passed, including schema/package parity and amd64/arm64 actual
rendered init execution. All 224 inputs matched batch 3's exact-head Linux real-daemon
cross-UID proof (mapped/owner success; unmapped/pin/live-peer/GID/private-file refusal).
Provider/WIT inputs were byte-identical to the pinned base; no redundant rebuild claimed.
Raw wave-0 residue: D6a two brief hits, D1b five brief/history hits; others zero.
Evidence: `wave0-final.md`, `w0-full-gate/`, `wave0-pair1-integrated.md`,
`wave0-batch-2.md`, `wave0-batch-3.md`.

Console base `ef0bf3f`, published pins exactly `=0.11.1`. Final ledger-only housekeeping
head `18c3d890c52bea72fc45ed5bad320788b2ef6ecb`; local stable/MSRV 1.89.0 gates passed
(97 distinct tests each, zero failures/ignores), exact-head Linux CI `34058770775` passed.
Delivered PR https://github.com/dekopon-agents/dekopon-console/pull/1 is open and ready
for human review, not approved/merged; do not edit it. No physical-TTY or live-credential
acceptance claimed. Six integrated inactive wave-0 worktrees were normally removed/pruned;
branches/evidence preserved. Console target removed after exact-head CI; no provider targets.

Final-source core MSRV/release/feature-off/provider variants, daemon help/install, final
OTLP smoke, final metrics and exact-head core CI remain downstream despite the current
pair's actual smoke and verified prior provider receipts. Full workspace gates
run at a complete wave boundary, not for each pair. Core publication/console re-pin are
unauthorized follow-ups. No final-PR acceptance is implied by a scoped batch pass.

Historical execution facts, not active blockers: failed workflow
`e8209c58-a289-47fd-89b7-1402eef7fb73` / attempt `06cff1ef-0486-4a77-b678-650402a1c5af`
failed before children at review-foundation (missing pi-server/pi-client modules); clean
core `2010f55`, console `9b6c068` were preserved. Native smoke
`19a218a8-f0d8-472a-8465-2fe3d73c9151` superseded it (`wave0-preflight.md`). Execution/review
uses Dynamic Workflows; pi-subagents was used only for that native smoke. Cost receipts
are separate from the workflow cost view. Original accepted ledger and detailed historical
cleanup/validation facts are preserved at `wave1-pair1-resumed/accepted-ledger.md`, with
prior stop at `wave1-stop.md`; historical free-space figures are not current headroom.

## Units — reviewed / integrated identities and discoveries

- **D6a — landed.** Reviewed `5741a42817c78b3ee4d8d779c51686f5dfbd8f69`;
  integrated `302cff4922c189a7783248ce159764a29765c522`. Owner-approved oversized-frame
  prefix fixture and equivalent Clippy let-chain repairs preserve every assertion;
  IPC GID derives from parent, no config field; real cross-UID/private-file proof passed.
  Evidence: `D6a/repair-report.md`, `D6a/review.md`; discovery: deployment prose/init owned D6b.
- **D6b — landed.** Reviewed `c4e471a09f85da7ced8018dc11a50afad8e2dbf7`;
  integrated `020f0bd72b7ad5ff1b2a14e857c703cbac6a052f`. Broker/pod 65532, gateway 65533,
  IPC group 65534, 0710 parent/0660 socket; private 0700/0600 paths and no fsGroup;
  actual init/layout proof, allocation/failure/TERM/preexisting/success cleanup controls pass.
  Focused repair of `53b198be21a09d63967b9c8f7e5f5a03fdb939f0` registers volumes before creation;
  released 0.4 roadmap fact restored. Evidence: `D6b/{report,rereview,review-repair}.md`.
  Discoveries: D2b removes checkpoint names in old-layout refusal loop; D2d broad prose;
  D7a broker-http retirement. Offline broker/ subdirectory and gateway credential ownership
  migration required; init refuses unchanged old files. Matching D6a image required, not deployed.
- **D1a — landed.** Reviewed `96b345057010cbc9f30444ed69be5e844d058522`;
  integrated `88bc2d6da32ebdf901fe03f159a8ec0794b14816`. Identical text-post retry once,
  one-second loopback proof, payload-free warn, unchanged receipt/206 test assertions.
  Test-only capture/subscriber edges; focused repair moved retry bullet from released history
  to Unreleased (original `4bf567dde3d64b113f05883a1a1a880e61cf2910`), no Rust change.
  Evidence: `D1a/{report,rereview,review-repair}.md`; discoveries: none.
- **D1b — landed.** Reviewed `7a846d9be351bc130d49209cfa17f0b8cdc715c5`;
  integrated `2e5d7a892602ea672579f0e54e1889eea724b8b6`. Both conflict classes and every
  transport failure aggregate; named public API/assertion migrations verified. Focused R1
  repair of `6a8e866d43d5f8afd684f9dcc99fc6d2f4ccb1b6` strips Telegram reqwest URLs while
  preserving typed causes; two-token send/body/Display/Debug/exit-chain negative control passed.
  Disjoint D1a changelog/tests merged mechanically, no semantic resolution.
  Evidence: `D1b/{report,rereview,review-repair}.md`; discovery: R1 resolved.
- **D8a — landed (console).** Reviewed `b5aa18afbe862ff48c85ac9a6c209b85fc4a6873`;
  integrated `97e8625617aa44dbc4b62a58d6f005b98d0c7af6`. Bounded development-only local
  chat UI; evidence `D8a/rereview.md`; discovery: shell credential-free startup owned D8b.
- **D8b — landed (console).** Reviewed `943636e6c60bd3635d4447404d6f5616709f7551`;
  integrated `e3896db08d675921070b5e17d021ee70c5293904`. Existing shell avoids model
  credential access, normal turn/chat and prior assertions preserved; seven published pins
  unchanged. Evidence: `D8b/review.md`; discoveries: none.
- **D3a — landed; batch gates pass.** Reviewed worker
  `ea3d0e0a56dacaf9590e2134506172be4e64bea4`, base `6537a0823567ba6e9e0dd6cb1c515c3651ca29c1`;
  integrated `8635680ccb7b0d16501237b992204c7cdb1c8461`.
  Authorized boxing/equivalent guard plus unused import and replay-only helper removal;
  175 scoped tests/Clippy/check/docs/dependency gates pass; only integration ledger differs.
  Recovery refs preserve `fb4a007a8aac69029b18cb822c28f692ae9d9104` and `045d643`.
  Evidence: `D3a/wave1-pair1-resumed-gate-repair.md`, `D3a/wave1-pair1-resumed-review.md`.
  Discoveries: seven classified raw brief/history/lexical lines (expanded reviewer eight hits);
  inert recorded-session test comment remains above cli_probe_path, not an executable consumer.
- **D3g — landed; batch gates pass.** Worker/reviewed
  `e79eb2c999b2d202263c70bfd6131a0f8276728a`, base `2e5d7a892602ea672579f0e54e1889eea724b8b6`.
  Integrated `86802b1d72de7ba09af48667529f85d18624901f`.
  Bounded owner-authenticated probe/chart command, 98 tests/full Helm bodies pass;
  no existing assertions/boundaries changed. Evidence: `D3g/report.md`,
  `D3g/wave1-pair1-resumed-review.md`; discovery: nine raw hits are brief/still-live runner
  instructions and D7a-owned broker-http history, not chart residue.
- **D3b — integrated; fresh review and combined local gates pass.** Reviewed
  `bb552bc85dd5ba053978c4d0f1d3d7f3d255641a`, base `0074ee38e8a5bbf5af56a33d9f7ddc575c86fb0b`.
  Integrated identity: this commit (`Simplify-Unit: D3b`). Real daemon/local0600/stub/provider
  smoke, native stdout IDs and independent startup/invocation correlation; complete bounded
  retrieval/ingestion counts. Current execution at `fefa4364acf739f33bea73bcf27391cb68b7cb09`
  proves 16 spans/five families, 15 complete rows, both native daemon pairs, ten controls
  and redaction; no smoke rerun in ledger closure. No product capability removed by D3b;
  the smoke can no longer pass on a direct in-process runner or retain owned resources.
  Evidence: `D3b/review-tail-review.md`, `pair2-core-resume/smoke-proof.json` and
  `pair2-core-resume/close-result.md` (actual final head/push/cleanup).
  Discovery: `examples/otel-traces/drive-turn.py` checkpoint keys are D2b-owned.
- **D2a — integrated; fresh review and combined local gates pass.**
  Reviewed/integrated parent `0a18d30a7ea7ad65847d01bafa3f9f88615fb78e`;
  fresh review PASS: `pair2-buffer-triage/buffer-review.md`.
  Current combined execution at `fefa4364acf739f33bea73bcf27391cb68b7cb09` passes
  nine-package 496 distinct native tests + 9 doctests, real two-daemon smoke, nine
  package LIST/content and all prepared secondary gates. Six prior actual deterministic
  comparisons are verified; no new provider builds. Original340/active339 inventories
  preserve the explicit live private counter reconciliation. D2a cannot roll back an
  invocation, atomically commit multiple calls, recover transaction manifests or
  automatically collect old generations; authorized completed writes can remain visible.
  Current evidence: `pair2-core-resume/parent-close-cause-proof.json` and
  `pair2-core-resume/close-result.md`.
  Full-wave archive/MSRV/workspace/exact-head CI remain downstream.
  All following owner-stage statements through this entry's final evidence line are
  explicitly historical, including their then-pending review/integration/smoke/push
  and earlier counts; they are not current outstanding gates or combined acceptance.
  B1 repair: real pinned memory-chat RED reproduced storage-quota at the
  failed-success assertion with unchanged 512 KiB read bound (exit 101).
  Restored `native_loaded_bytes` as LIVE private original-load containment state,
  initialized at begin, checked/read before loading, updated only after successful
  bounded size-verified reads; no refund or repeated-load charge. Historical raw
  removed inventory remains preserved: this one counter is no longer retired.
  Removed only the replacement's grown-candidate/read-ceiling comparison; independent
  write/read requests and all file/root/namespace/grant bounds remain. No transaction
  machinery restored, no new rollback/atomicity/eviction or RSS guarantee.
  Added native positive/negative boundaries and real pinned-provider B1 regression;
  all previous assertions survive. Clean native execution checkpoint
  `a3a82d1` passed locked seven-package all-feature check/tests, all-target Clippy
  with warnings denied, warnings-denied rustdoc and separate doctests:
  275 distinct native tests + 9 doctests (focused/child/reruns excluded).
  Real B1 GREEN uses identical failing fixture limits and proves dedup append,
  compaction and readback. Native exact-boundary tests preserve bytes/tree on
  load, read-request and per-call/per-invocation write refusals.
  All 388 outside-repair mode/type/blobs equal 0a94e0c, including guest inputs.
  Six prior ACTUAL consumer provider comparisons verified by receipt hashes and
  current immutable bytes; ZERO provider builds this pass. Every prior test body
  is byte-preserved. No new assertion suppression. Raw 340-literal inventory/history
  retained; active inventory removes only the explicitly restored live counter
  (339 literals), not an alias or scanner waiver. Current hits remain classified.
  Fresh fmt/diff/docs/audit/scanner/release metadata and seven package LIST/content
  proof pass; LIST is not archive compilation. Safe inactive 9.2 GiB target removal
  reclaimed 8.97 GiB physical; free space 69.14 to 78.11 GiB, all seven targets absent.
  Final ledger-only amendment is source-equivalent to the executed native checkpoint.
  At that historical owner handoff, HIGH review, combined native integration and real
  two-daemon OpenObserve smoke were still unperformed; they have since passed above.
  Archive/CI remains downstream; that owner handoff performed no push or integration.
  Evidence: `pair2-buffer-triage/buffer-worker.md` and `buffer-*` receipts.
  Unit ref `Simplify-Unit: D2a`; base `0074ee38e8a5bbf5af56a33d9f7ddc575c86fb0b`.
  Direct per-call namespace/VFS writes replace transaction/GC state; private-key/path,
  authority/continuity, quota and drain bounds remain. Four-package final-source
  locked check/tests/Clippy/doc/doctests, metadata/dependency/docs/script/privilege
  and actual chart-container gates pass. Full inventories classify historical and
  unrelated lexical residue openly. The namespace housekeeping refusal fixture now
  uses one byte below the three-entry live peak; every refusal/no-mutation assertion
  remains. Approved mixed rollback migration preserves exact visible bytes and drain
  assertions. R1–R4 review repair corrects empty positional growth, rejects retired
  poison entries at both layout levels, removes the no-op scan callback, and corrects
  canonical WIT prose. Two added regressions preserve size/stat/zero/quota/reopen and
  quarantine/data/neighbor assertions; the reopened test handle is closed before finish.
  Fresh invalidated four-package and doc/metadata/mechanical gates pass. Evidence:
  `D2a/review-tail-worker.md`; prior closure receipts reused only by input equality.
  Rustdoc consumer closure corrects the facade JSONL per-call promise only; fresh
  locked facade check/test (3 tests, zero failed/ignored), all-target/all-feature Clippy,
  warnings-denied rustdoc/doctest (0 doctests), three wasm feature checks and docs/fmt pass.
  All six pinned provider rebuilds validate and byte-match original fixtures; each target
  is reclaimed serially. Runtime/tests/WIT/locks and all other tracked source equal
  reviewed cbde6b6; prior native 477-test evidence is reused, not rerun.
  Evidence: `pair2-doc-closure/worker.md`; final root target cleanup follows validation.
  Those four-package/477-test and six-build receipts are historical, not current-input
  acceptance. The classified ten-consumer repair corrects per-call descriptions and
  the broker namespace formula only: threshold + append + live dedup + 32 entry charges.
  The named mixed sizing test replaces the retired 30 MiB staged-copy rejection with
  a below-direct-peak 16 MiB rejection, explicitly accepts 30 MiB, and preserves exact/
  one-below and all other quota/budget assertions. Real generated-provider compaction
  now also runs at direct peak; the default-limit run and its assertions remain.
  No new authority, credential, WIT or guest-memory budget change. Negative capability:
  invocation failure cannot roll back completed calls; file sizing no longer reserves
  removed staged JSONL copies. That historical seven-package execution and six new
  comparisons ran: 273 distinct native tests plus 9 doctests and six byte-equal comparisons.
  Historical release invocation omitted arguments (exit 2), preserved unchanged.
  Authorized completion ran fresh metadata and explicit release arguments successfully.
  Static Schemars description-only and saved rendered HTML proof complete; seven scanner
  regressions pass with current literal classifications. Native inputs equal all 391
  saved non-ledger paths at dirty execution HEAD 9676f67, not clean old HEAD.
  Manifest-aware Cargo LIST/content inclusion is complete; integration tests are
  intentionally excluded by unchanged include patterns, not missing package source.
  No archive compilation claim; full-wave archive acceptance remains later work.
  Saved rendered pages preceded safe removal of the inactive ignored 581 MiB doc
  target (physical free space rounded 78 GiB before/after); six provider targets absent.
  Final source maps exactly to the dirty-input proof; no native/provider gate rerun.
  That historical completion performed no independent review, integration or push;
  current reviewed/combined acceptance is above. Historical completion evidence below.
  Evidence: `pair2-storage-docs/consumer-worker.md`, `pair2-consumer-finish/worker-completion.md`.
- **D2b — pending.** Checkpoint removal/config/fixtures plus D6b refusal-loop names;
  commit: —; temporary checkpoint methods explicitly D2c-owned.
- **D7a — pending.** Webui embedding and broker-http retirement; commit: —;
  preserve surviving HTTP/storage security invariants at their canonical home.
- **D7b — pending.** Move ChatGPT auth to gateway before CLI deletion; commit: —;
  examples/local has surviving config/skill readers requiring authorized fixture migration.
- **D2c — pending.** Append-only JSONL; requires D2b; commit: —; discoveries: —.
- **D3c — pending.** Atomic runner/provider-host/manifests/lock/release/packaging retirement
  includes D3d; needs D8a/D8b/D3a/D3b/D3g; commit: —; no orphan interval.
- **D3e — pending.** Both daemon dependency directions after D3c; commit: —; discoveries: —.
- **D2d — pending.** Remaining durability prose after D2c; commit: —;
  named future-owned hits must remain explicitly reported, never silently excluded.
- **D3f — pending.** Final changelog/component ownership after deletions; commit: —;
  no packaging deferral, publication or yanks. D1 has only D1a/D1b, no D1c slot.

A unit commit cannot contain its own SHA: resolve its trailer at the next same-commit
integration update; exact current head lives in external evidence. Remove this ledger
only in final housekeeping before the core PR; keep the amended brief committed.
