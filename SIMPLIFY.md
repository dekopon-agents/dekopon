# Simplification execution ledger

Authority: [SIMPLIFY-BRIEF.md](SIMPLIFY-BRIEF.md), including the owner-authorized §5
repair rules committed in `6537a0823567ba6e9e0dd6cb1c515c3651ca29c1`.
Core: `simplify/2026-09`, original base `542430e`; accepted pre-batch head `6537a08`.
Evidence paths below are relative to workspace `.validation/simplify-2026-09/`.
No releases, tags, crate publication/yanks, merges, live deployment or core PR yet.

## Current milestone — wave1-pair1-resumed integration

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

Final-only core MSRV/release/feature-off/provider variants, daemon help/install, rewritten
OTLP smoke, final metrics and exact-head core CI remain outstanding. Full workspace gates
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
  Integrated self identity `Simplify-Unit: D3g` (exact SHA in batch evidence).
  Bounded owner-authenticated probe/chart command, 98 tests/full Helm bodies pass;
  no existing assertions/boundaries changed. Evidence: `D3g/report.md`,
  `D3g/wave1-pair1-resumed-review.md`; discovery: nine raw hits are brief/still-live runner
  instructions and D7a-owned broker-http history, not chart residue.
- **D3b — pending.** Two-daemon OTLP driver/stub/queries; commit: —; discoveries: —.
- **D2a — pending.** Direct storage handle, remove transactions/GC; commit: —; discoveries: —.
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
