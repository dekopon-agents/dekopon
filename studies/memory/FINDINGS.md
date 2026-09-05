# Verified narrow pilot, 2026-09-05

**Exploration, not a full memory-attribution study.** One real end-to-end proof passed, then
exactly nine fresh screening trials succeeded (three each of RT-X01, RT-X02, ST-X01-ARM64).
No repair or retry was needed. Production, credentials, chat/provider/model services and other
worktrees were untouched. No production memory-control change or new backend was implemented.

## Proof before screen

[E2E_PROOF.md](E2E_PROOF.md) records campaign `approved-v4-pilot-01-proof`, trial RT-X02:r1,
attempt `6233f5550d8643e796e3575a84d89040`, and the complete provenance, raw/derived numerical
agreement, sanitized export/analysis-only restore and verified deletion/slot release.
Its one trial is **excluded** from the screen averages below. Source/unit/synthetic gates alone
had previously left the pipeline unproved; this was the first real broker vertical slice.

## Runtime screen: P4 versus echo, not deployed parity

Campaign `approved-v4-pilot-01-screen`, seed20260905; release0.12.0, Linux aarch64, 4096-byte pages,
cache off, synthetic echo policy/identity, empty audit, no TCP/telemetry, no invokes. Each fresh
container has the same one-CPU/1GiB/zero-swap/64-pid caps and static in-container collector.
P4 loads echo/http-probe/JSONPlaceholder/gh; it is **not** a known production provider set.

Values below are the unweighted means of three trial means. Brackets show the **range of trial
maxima**, not confidence intervals or the range of every observation. MiB = 1,048,576 bytes.

| Metric | RT-X01 (P4) | RT-X02 (echo) |
|---|---:|---:|
| Idle process RSS, MiB | 61.187842 [59.742188–62.003906] | 24.850090 [24.976562–25.191406] |
| Idle process PSS, MiB | 61.162760 [59.738281–62.000000] | 24.883681 [24.972656–25.187500] |
| Idle cgroup working set, MiB | 45.055520 [43.679688–45.960938] | 8.405716 [8.730469–9.023438] |
| Anonymous executable RSS, MiB | 1.812500 [1.812500–1.812500] | 0.250000 [0.250000–0.250000] |
| Socket-ready proxy, ms | 746.737603 [706.468276–823.358186] | 203.370553 [202.016559–204.668488] |
| Joined-child total CPU, ms | 722.766333 [707.975–748.620] | 105.912000 [103.028–110.678] |

**Observation:** these P4 variants had larger idle RSS and longer ready/CPU observations than
echo-only. This is a small descriptive screen, not an isolated per-provider cost or an allocator
explanation. Provider code, manifests/metadata, generated policy world and compilation change
together. Anonymous executable mappings are JIT-consistent, not exclusive allocation-owner tags.

Each runtime raw log contains actual loaded-provider records, `broker_started`, readiness,
61 idle samples, six deep snapshots, `broker_stopped`, joined-child counters and complete0.
All six archives have empty audit/checkpoint records0 and no remaining socket. Collector idle
RSS was 655,360 B; cgroup accounting includes it and other charges. RSS and cgroup working set
must not be subtracted into ownership. PSS uses six deep samples while RSS uses61: their
unlike averages can cross. The first startup observation can precede `exec`; startup sampled
maxima can miss compilation peaks. Ready is a ~100ms polling proxy, not a successful request.

These are fresh processes/volumes, **not cold filesystem-cache trials**. All six screen runs
reported zero block read bytes/major faults, unlike the proof (2,220,032 B reads/16 major faults).
No page cache was flushed. Each screen broker wrote4096 block bytes for startup/checkpoint;
that is not logical audit volume. Order was shuffled within lanes, not alternating by force:
P4:r2, P4:r1, echo:r3, P4:r3, echo:r1, echo:r2, then History:r1,r3,r2. No study attempts overlapped;
host load was captured privately but guest/background interference and affinity were not controlled.
No invoke throughput, latency under contention, leak duration, allocator trim or production saving
is established. Three replicates do not by themselves confer statistical confidence.

## History source-kernel screen

ST-X01-ARM64 executes byte-identical fenced `History` retention/trim/clone/drop methods, using
the separate static Rust1.86 product. Uncalled model-serialization types are stubs. This is **not**
ConversationStore, the gateway, lazy expiry, authorization/grants, transport or a model request.

All three trials passed the six acknowledged phase barriers and actual `workload-complete`
1536-operation marker; raw values reproduce all summaries. The fixed recipe records12 exchanges
of 6144 text bytes in each of128 windows, bounded to64KiB and12 turns. Thus ten exchanges/window
remain: **1280 turns / 7,864,320 logical bytes**. Four held clones report **245,760 logical bytes**.
After clone drop, held bytes are0; after window drop, retained bytes are0.

| Phase observation | Across three fresh trials |
|---|---|
| Retained RSS (seven snapshots/trial) | 8,880,128–8,884,224 B |
| Clones phase sampled RSS maximum | 9,125,888–9,129,984 B |
| Clones-dropped RSS (seven snapshots/trial) | 9,011,200–9,015,296 B |
| All-windows-dropped RSS (seven snapshots/trial) | 8,957,952–8,962,048 B, despite logical retained bytes0 |
| Load phase RSS snapshot | 860,160–864,256 B, at entry; not a sampled allocation peak |

**Observation:** logical deletion does not imply immediate RSS reclamation. The remaining pages
could include allocator retention and instrumentation/other allocations; this does not identify
live versus freed heap, establish a leak, or validate trimming as a remedy. Synchronized drop
snapshots prevent phase mixing but do not make the instrument free of overhead.

The record-only operation mean varied **87.38–168.22 ns** across trials; largest clone latency
was **2,057,513 ns**. The reported operations/second divides1536 by the sum of `History::record`
times only; it excludes text construction, JSON emission, holds/barriers and whole-driver work.
Do not call it gateway throughput. Load's brief work finished between periodic samples; only its
entry deep snapshot exists. These data motivate controlled follow-ups, not performance claims.

## Trial and evidence index

All attempts are number1, exit0, succeeded; prefix each trial with `approved-v4-pilot-01-screen:`.
The table is actual launch order (global sequences11–19); state was intentionally a later lane.

| Trial suffix | Attempt | Latest parse |
|---|---|---|
| RT-X01:r2 | `f5e72f1135b74480b50ba8af715d1200` | `71ad4b38a7ab4f6694b441630ab79bef` |
| RT-X01:r1 | `680968a1caec4a638c6de8584083cf4f` | `d6d14c382d1c4086bf89516df21bc0d7` |
| RT-X02:r3 | `1f5cc050c9594ed0822a47b53c80e87f` | `10c1be3a353d4701a832a9c11ac4c420` |
| RT-X01:r3 | `36009cd386d84ef5a19a88d05481bbb2` | `f27057cbcc514ea4ab39ce438d8d65e4` |
| RT-X02:r1 | `4d13c7f99f8047638d6ed69c750fea84` | `44a6553b73ac4509aefe27a6f78880d3` |
| RT-X02:r2 | `5fffa9f1ba7045b99571e681f4e5fd65` | `90fb46c9c79a4b8abb78b5b5cac7e4a7` |
| ST-X01-ARM64:r1 | `1a6b8f09275945a5bacf277c1816834e` | `1d398ba0676e41ffa20b0a6d7b39d75e` |
| ST-X01-ARM64:r3 | `be86990f502a4ca0808cae7fb74ba0ea` | `2d3acd5e3c034036a24bcf6d57cf9983` |
| ST-X01-ARM64:r2 | `2308e248fc7545eb866cf1c7422a9451` | `fbff4b13fde3430fae455366957de93d` |

Screen verification artifact `2044658e6c484741b7ee35db299daf4a` rechecks287 artifact hashes,
11,709 sample values and567 summaries, completion, compatible producer/input/harness/parser hashes,
final PID0 state, all deleted containers/volumes and free slots. Its read-only source is artifact
`fedfbf9c4f8144bf88d7e90510c3b34c`. Analysis artifact: `e81c1138097f4c9b8665aa3eb26da2e6`.
Between-lane deletion/disk receipt: `3f9e4a3d3e7145ef99e6c428c9a5f9d5`. Exact cleanup rows55–72
cover all nine screen attempts. Current History product `d7504375e15c48509ca920d49d05ad4f` was
produced by `be750a38780e432dbc96cd23915bb32f`, SHA-256
`186259d310d00fc8e847db4a4142c2d8da081fd996058d9d440acaf2b8eaea78`.

Reproduce the table with `python3 analyze.py --campaign approved-v4-pilot-01-screen`;
select its named metric/phase groups, divide byte values by1048576 or nanoseconds by1000000.
For per-trial values join `phase_summary → latest_parse → attempt → trial`, filter this campaign,
and order by experiment/replicate. The sanitized snapshot supports the same queries without raw
access. [findings.json](findings.json) imports typed relational evidence links for the claims.

## Validation, gaps and asset lifecycle

The canonical ledger passes v4 identity/migration/driver, integrity and FK checks. The accepted
pre-proof gate built both compatible ARM64 tools, checked the real History smoke's six phases,
and independently tested killed/stopped owners for both PID1 deadlines. Those are **infrastructure**
gates only. Prior unit tests and stubbed workflow passes were not counted as domain trials.
Final **35 unit tests** and the **stubbed workflow check** passed under the canonical exclusive
quiescent controller gate (simulated engines only), with receipts `116dc019ae4044abaf788986f7d3ed67`
and `1b21cc4bf9d4419a8ef5fcc602897686`. Documentation duplicate checks also passed. A publication-only
privacy cleanup generalized a local-DNS sentinel; its initial overbroad match failed export tests,
then one focused regex correction passed. E2E_PROOF preserves both driver identities and the failed
test receipt; no domain workload/parser/recipe was changed or retried. Public export/restore is
rechecked before publication. No Cargo build or additional synthetic fault workload was authorized
during this proof/screen phase.

All ten new domain containers/volumes were removed after archive/binary/input hash retention;
both slots are empty and no study targets remain. Host disk gates passed15GiB before/after every
attempt; each child preflight passed8GiB Docker free. No build occurred between proof and screen.
Every retained asset has this consumer or safety reason:

- Canonical DB, identity, owner locks and active worktree: sole executable evidence authority;
  keep them. Public SQL has no executor identity and is analysis-only.
- All immutable raw logs, work/harness/parser/release archives, current and obsolete binary
  products, recovery snapshots, compiler logs and validation/command receipts: provenance and
  failure/containment evidence. Current collector was consumed by all ten new domain trials;
  current History by three. Now both remain exact-input evidence, not a reason to retain targets.
- Pre-migration backup and proof-time analysis restore/snapshot: recovery and independent
  numerical reconstruction evidence; no backup is claimed to be off-machine.
- Prior pre-create-failure volume (attempt `36440045e0644df2ababb6414a98f531`): no archive endpoint,
  so safety-retained; no slot and no known target. Do not force cleanup or prune globally.
- Local note/global-skill delivery metadata: idempotence and installed-byte verification only;
  ignored and not exported. The skill source and its small example are version-controlled.
  The local one-shot orchestration prompt is ignored too: it contains owner delivery coordinates
  and is retained as execution-provenance context, not a public reusable workflow.

Coverage is not erased: 20 unsupported cells and three blocked local legacy conditions remain.
Malloc accounting is missing in all nine screen attempts; six runtime attempts lack the six
History-only metrics, three state attempts lack broker readiness. Attempt-level missing rows do
not establish phase-level completeness. Original RT-C01…06 and C-STATE-001…006 are **unrun**.
Older bootstrap infra failures, watchdog137/deadline124 outcomes, F-INFRA-001…003 and F-001 remain
historical evidence, never upgraded into screen successes. Old History observer data remain
whole-run-only, unlike the new synchronized parses.

## Future decisions (not authorized follow-up experiments)

The compact owner/budget/tradeoff table in [DESIGN_DIGEST.md](DESIGN_DIGEST.md) remains the control
framework. Next questions require new explicit scope: compatible external observer/no-op/trim and
loaded-libc controls, matched cache state, Wasmtime ownership barriers/multi-memory admission,
real public-library Cedar/storage, and full gateway History/prompt seams. Measure CPU/latency,
I/O, hard-cap headroom, refusals and cancellation alongside any memory reduction. Linux4KiB is
not Pi16KiB or macOS evidence; no secure deletion, spill durability or production savings claim
follows from this pilot. Start the next project with one real vertical slice, not more machinery.
