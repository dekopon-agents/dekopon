# Accepted study design (mapper compaction)

This is the single downstream input; do not repeat the two raw mapper investigations.
No mapper ran experiments. **All memory-control changes below are Exploration** unless
explicitly called Current. Production remains read-only; no production access is needed.

## Identity and interpretation fence

- R: release 0.12.0, `b9b9533a9050a5bfe5096bffc0de40ee1ffd8f42`, Wasmtime 36.0.13.
  Image `ghcr.io/dekopon-agents/dekopon@sha256:2639863b542ef202fd4efb65ae68e9d5f5395391df7d5f13f8b8dbfbfeb2e768`.
- M: source `5a03a296ff4877cef4549ad83db46e52f6cbe231`, Wasmtime 36.0.14.
  Prompt/command cancellation/WIT drift exists. Moving stable release Rust does not establish
  the release compiler. Thin LTO, stripped, four codegen units are source-supported flags.
- P4 means the four baked defaults (echo, http-probe, JSONPlaceholder, gh), not the known
  deployed provider configuration. Optional memory-chat is excluded. Production cache,
  policy/world size and limits are not established.
- The owner's historical ~77.09 MiB broker working set and 2.625 MiB anonymous executable
  RSS are context without retained original raw evidence, **not trials**. Neither the JIT-consistent
  mappings nor the remaining anonymous RSS identify allocation ownership or live heap.
- Working set = cgroup charge minus inactive file, not process RSS/PSS. Virtual Wasm reservations,
  per-memory requested limits, malloc accounting, kernel charge and resident pages are distinct.
  Docker Linux results are not Pi evidence (Pi pages 16 KiB; actual container pages must be measured).

## Accepted questions and dispositions

| ID | Question / recipe / disposition | Primary source references and caveat |
|---|---|---|
| RT-H01 | Accept six-cell release screen; RT-C01 P4/cache-off baseline. | R brokerd lib:219–247,355–391; release.yml:91–146. Exact platform/config parity missing. |
| RT-H02 | Accept observer control RT-C05 versus trim RT-C06, deferred until compatible interposable glibc is proved. | R broker-host lib:599–721,1198–1231; SDK host:427–438. mallinfo2 is not application-live bytes; trim success proves some retention, failure proves nothing. Never call allocator APIs from signal handlers. |
| RT-H03 | Accept RT-C02 echo-only against P4; later largest/leave-one-out. | R broker-host lib:555–582,724–808,1198–1242; SDK host:338–376; brokerd config:531–535. Composition changes metadata/policy world and code together. |
| RT-H04 | Defer engine/linker, compile/describe, providers-dropped, runtime-dropped source barriers. | R broker-host lib:416–465,555–582,679–721,1198–1242. Hold/drop every clone explicitly, join compilation. CLI lacks barriers. |
| RT-H05 | Accept paired RT-C03 empty cache → RT-C04 populated cache (three pairs). | R SDK host:427–438; broker-host:683–705; brokerd lib:142–147; host tests:874–919; Wasmtime cache 36.0.13 lib:29–59,110–127,204–217,263–279. Populated is not a verified hit; ordinary compilation counters include cache paths. |
| RT-H06 | Defer default/1/2 glibc arenas, then independent outer-job and Wasmtime parallel compilation controls. | R broker-host lib:1202–1228; brokerd main:121–123; Wasmtime 36.0.13 config:1864–1874. Tokio workers do not bound blocking jobs. CPU/throughput cost required. |
| RT-H07 | Defer echo one/64 invokes plus success/trap/timeout/cancellation and growth fixture. | R broker-host lib:468–504,951–984,1026–1055,1529–1574; metrics:293–305,324–364; SDK host:31–89; tests:495–516,790–872. Largest requested single memory includes denied requests, not aggregate residency. |
| RT-H08 | High-priority deferred two-memory fixture (2 × touched 3 MiB; 4 MiB per-memory/token). | R SDK host:31–40,51–89; broker-host:376–413,468–495; brokerd config:792–805. Source concern: one per-store budget token vs default four allowed memories. Not an executed bypass; tables/stacks/native allocations also outside token. |
| RT-H09 | Defer small loopback delayed HTTP: connections 1/2/4 vs total-store budget, overlap barriers and timeout drain. | R brokerd server:99–174; config:299–320; broker-host:386–413,468–504; tests:790–872. Excess connections reject, not queue. Native HTTP confounds guest bytes; count all refusals. |
| H-STATE-001 | Accept Cedar world/engine construction/drop and 10,000 half-allow authorizations; driver pending. 256 principals,32 capabilities,4 providers,128 policies,100 warmups. | M policy lib:103–202,514–707,1028–1075; policy README. Library cost, not full broker latency. |
| H-STATE-002 | Accept actual private ReplayLedger + FileAuditLog 2,048 durable denied decisions/reopen/transfer; same-crate driver pending. | M broker lib:1439–1459,2217–2452,2790–2810; brokerd README audit/replay. Current head-only metadata and one-time replay-ID transfer already exist. Brokerd's extra checkpoint sync excluded. |
| H-STATE-003 | Accept 129 keys into capacity128,12 turns × 6144 text bytes,64KiB window; 4 held clones; lazy expiry. Full store driver pending. | M gateway conversation:130–303; agent prompt/history:184–288; gateway docs, security-model. Current byte count omits capacities, grants, keys, clones; expiry prevents replay, not timed erasure. ST-X01 implements only the real History kernel with explicit stubbed unused model types. |
| H-STATE-004 | Accept non-recording lazy mocks of prompt loop:7 tool turns ×256KiB,8-step cap,16 calls,100 sessions/concurrency4; cancel each tenth during call4. Driver pending. | M agent prompt:93–109,184–211,496–566,637–774,935–974,1844–1913. Existing recording mock would confound memory. Cooperative cancellation may not interrupt a synchronous call. |
| H-STATE-005 | Accept real native JSONL:2MiB initial,100 chunk reads/100 1024B appends + LF,abort/refusal/reopen. Driver pending. | M storage-host lib:109–174,376–432,689–719; config:14–291; jsonl:15–149; transaction:360–435,456–515,646–740. Metadata/read streaming differs from whole-file Vec write overlays; logical quotas != physical disk. |
| H-STATE-006 | Accept count/discard fake trace/log exporters:10,000 each,512B attrs,queue256,batch64,100ms delay,1s block,10s deadline. Driver pending. | M telemetry lib:226–283,340–360; installed OTel SDK0.32.1 trace/span_processor:586–702,829–939; logs/batch_log_processor:152–281,623–710. Installed API not deployed lock identity. No transport or InMemory exporter. |
| H-STATE-007 | Defer gateway admission harness:4 sessions,channel64,burst256,slow busy replies,task/permit counters. | M gateway lib:83–99,588–738; session:216–300,624–688,875–980. Spawn-before-admission source evidence, not observed production leakage. |
| H-STATE-008 | Defer exact SQLite replay index prototype behind H-STATE-002/recovery invariants. | M broker lib:2333–2347,2790–2810,2971–2995; brokerd README replay. SQLite WAL FULL,cache1MiB,mmap0,100k IDs; watermark crash recovery. Python set comparison is algorithmic, not Rust/broker savings. WAL needs separate cap. |
| H-STATE-009 | Defer actual storage post-marker fault/timeout and leased bounded GC harness. | M transaction:576–619,646–860,1947–2007,2013,2285; storage lib:704–719. Fault injection != power loss; retain unknown-outcome evidence and quota. |

## Rejected shortcuts (not rejected research questions)

- No generic malloc benchmark substituted for RT-C05/06; no live-heap attribution by subtraction.
- No duplicate component to simulate count (duplicate identity rejected); zero-provider CLI refuses.
- `memory-reservation-probe` tests capability namespace reservation, **not memory.grow**.
- No bloom-only/TTL replay index, security-ledger eviction, deleting audit history, credential/chat fixtures,
  global page-cache flushing, production injection, live target sharing or detached cancellation.
- No recording model/exporter mocks or `audit.records()` snapshots in memory workloads.
- No hidden retry changes to allocator/compiler/platform/config; no full workspace build for a screen.

## Build selection and honest gaps

The twelve requested initial cells remain in the relational matrix even where unsupported.
`RT-X01/02` are executable release-broker P4/echo variants using a small **in-container** static
collector: process RSS/PSS and cgroup accounting include a declared observer (its RSS is measured
separately). They are NOT substituted for RT-C01/02's external-observer condition.
`ST-X01-ARM64` executes unchanged source History retention/trim/clone methods in a standalone Rust driver;
unused model serialization types are stubs. This is source-kernel evidence, not the whole gateway.
`BUILD-COLLECTOR-ARM64`, `BUILD-HISTORY-ARM64` and `INFRA-SMOKE` are slot-consuming infrastructure cells;
their memory/timing is never published as a Dekopon benchmark. Missing APIs/seams remain blocked.

The canonical v4 migration, compatible ARM64 tools and real Linux owner-loss/synchronized History
gates passed independent review before the pilot. The v3 backup and old failed evidence remain
preserved; old History observer data are whole-run-only. The v4 runner checks compatible producing
versions, condition/driver identities, global slots and container-owned deadlines. Scientific
coverage still comes from domain trials, not infrastructure or synthetic gates.

**Verified pilot, not full attribution:** [E2E_PROOF.md](E2E_PROOF.md) records one real RT-X02
vertical slice, including analysis-only restore and safe cleanup, before any screen execution.
Then `approved-v4-pilot-01-screen` (seed20260905) completed three fresh replicates of each X variant,
sequential runtime then state, with no repair/retry. Mean idle RSS was61.187842MiB for P4 versus
24.850090MiB for echo; mean sampled-ready latency746.738ms versus203.371ms. These describe
composition variants, not per-provider/allocator costs or deployed parity. History logical text
fell from7,864,320B to0 after drop while RSS remained8,957,952–8,962,048B. That is not allocation-owner
attribution, a leak diagnosis or a trim recommendation. Exact IDs, CPU/I/O, sample gaps and caveats
are in [FINDINGS.md](FINDINGS.md); no study targets or active slots remain.

Future priorities remain H-STATE-001/005 public-library drivers, then full H-STATE-003/004 seams
and matched runtime controls. These are **not authorized follow-ups**. Any changed project driver
needs one writer, quiescence, explicit approval, a new recorded condition and a real end-to-end
slice before expanding a matrix. Use bounded fresh trials, preserve missing/failed coverage, and
isolate CPU/latency lanes; three replicates alone are not statistical confidence.

## Budget ownership framework

| Owner | Current bound | Exploration and required tradeoff |
|---|---|---|
| broker | Policy source/policy counts; permanent replay IDs/audit count (defaults100k vs200k mismatch) | World bytes/preflight and exhaustion lead-time; exact disk replay index only with fail-closed recovery, disk/WAL caps and latency/I/O evidence. |
| broker-host/startup | Per-memory bytes/count, per-store admission token; fresh stores | Correct aggregate guest bound, independent outer compilation jobs; process/cgroup headroom still mandatory. |
| gateway/session | History turns/bytes/count, admitted sessions; inbound and usage queues64 | Total resident-history/transcript-byte admission, expiry sweeper, separate bounded busy/Stop reply lane, cancellation through joined worker. Reject/evict text, never cache authorization. |
| storage-host | Root/namespace/file/logical-entry/transaction/GC ceilings | Aggregate native-overlay bytes or spill; independent physical quota, key isolation, crash recovery, retention lifecycle. No encryption/secure deletion claim. |
| each telemetry process | Count-bounded lossy per-signal queues | Byte/age/in-flight/drop observability and HTTP response cap; avoid recursive exporter diagnostics. Durable audit never becomes lossy telemetry. |
| operator/native allocator | External process/cgroup hard containment; private compiled cache | Arena/trim are soft Linux-specific optimizations; measure faults/contention/latency after reclaim. Cache trust, quota and binary/engine identities required. |
