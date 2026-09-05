# One real end-to-end proof

**PASS, 2026-09-05.** One fresh RT-X02 domain trial completed before the tiny screen was
allowed to start. This proves the pipeline, **not a comparison, production parity, allocator
attribution, or a full memory study**. No repair/retry was needed. The prior campaign's failed
attempts and v3 backup remain intact; earlier infrastructure tests are not this proof.

## Stable evidence chain

All IDs resolve in the canonical local SQLite ledger and the sanitized SQL snapshot.
Artifacts' full hashes/relative locations are in `artifact`; their contents remain private.

| Record | Identity |
|---|---|
| Campaign / experiment | `approved-v4-pilot-01-proof` / `RT-X02` |
| Trial | `approved-v4-pilot-01-proof:RT-X02:r1` |
| Attempt | `6233f5550d8643e796e3575a84d89040` (number1, exit0, succeeded) |
| Parse | `c43bac2278a844398e2c395ea5631b20` (`recipe-phases`) |
| Raw log | `2a429c42810b4efa8e6eb35125c9d784` |
| Harness / parser source | `bb17630909294c5eb798d7d874e73e38` / `eb8b851323004110997dfdcb4bed64a2` |
| Work archive / final container state | `c48894b61cca4f248b57582b88242863` / `e2c1a6cb0cbe4335bcb03014a0c625bd` |
| Image / exact inputs / host-load metadata | `d9a7cdc0c0c4418eb27cc7e3215e84c1` / `d1e55bc8ed7449ca84cdadf25e3060bf` / `f09415d38a724970b086ce8700b22413` |
| Release inventory | `b6a2d75f545743eda5c7907f88ee9da2` |
| Broker ELF archive / all baked providers | `b07d0976faf145348961fda12dc04ff0` / `b8cf41a6740443e09462e85fdc1e97fb` |
| Collector binary / producing attempt | `ac1a7ab0a290444ab1f7c97e16eb0d97` / `e33ddcbda87e42339896379cfa44f269` |
| Producer work archive / compiler log | `f68124c8ca1b417e98a5f844e862eb17` / `d0ee6cc69fe24bc2b1c21d5b55a5d8b2` |
| Preflight / plan / execution receipts | `0f05cf1cae824c4b9cab41d8f96aee04` / `1db219bfc14942318096b4347aef0b0f` / `7cb45fd81b0d411799f89c5928d9ee9e` |
| Read-only verifier source / result | `fedfbf9c4f8144bf88d7e90510c3b34c` / `d4e873332c1945c6b2d0421a79d7722b` |
| Analysis / export-restore receipt | `c1966514747246cd9edae5f77c6900de` / `82cfe57cc8f04ab58648c9881149dbad` |
| Exact proof-time sanitized snapshot | `f75c1a5a9ea241088b2df34e95b04c7f` |

Key SHA-256 values (not inferred from version labels):

```text
raw log    8d9cf67ac90eea8ac703255d50cf7772a84b183c283174750809abbeb66956ac
driver     002f2fc97781ff099ee1b0f5384d2ad0cdff3e0fc336893658ceb4619dde5189
condition  7ea89ae6a58cd7aa76d4cb4783226c7c5ff8ecea87b11140b813b7c5ff4e81ee
parser     15e9ad015f1af979d38aaebbdaa40bce56b81fcc90dabb0ef38f9fa412862f0a
collector  aa18ce61f9b0d52150f8e497cca83cb1a46f7b74772ff3fd22dd5ffe088190fb
broker ELF 4eb748df6607fad7e98b93c41b6a7a6413b527c275efd354181a71a1116251d7
echo Wasm  c15e88cf50726e8a80d1f73f8167563242d59ea80c1af026014e30054ac786b1
config     0143ec4f2f6b24d5a98525ee764740e7666aabeb5e483c8098e96018890e7e75
policy     1cb6159cd1c9ca42de6feb314cbfe9ba57562dbd521a5b625590b5b45a57141c
snapshot   a5bed2a220e91d8d291b743a4427896df4fa3fda3d82db212fcf176311d1bc61
```

`attempt_input → binary_product → producer attempt → build` proves the compatible static
ARM64 collector was built with the retained source/input archive and compiler output, not
silently reused from the incompatible old tool. Producing input-version hash is
`d6dba478a218c67e059a3cc2c5902264909f0d4874e820a6c49f7c06a939596a`.
Runtime release is 0.12.0 at R, with the pinned image in README/DESIGN_DIGEST; the standalone
builder is the separate local Rust1.86 image. Its static glibc2.36 is **collector** identity,
not loaded broker allocator identity. Broker compiler and matching debug symbols remain unknown.

## Actual completion and measurements

- The raw log records echo loaded, `broker_started`, sampled socket readiness at
  **208,773,178 ns**, 61 idle observations spanning **60.150 seconds after ready**,
  `broker_stopped`, joined-child `drained`, and exactly one `complete` with exit0.
  The shutdown signal is the retained collector's SIGINT path; no deadline or OOM fired.
- The archived audit is empty, checkpoint has records0/head-null, and no broker socket remains.
  This is startup + idle, not a provider invocation, permission test, or transport round trip.
- There are two startup observations, six deep idle snapshots (ready,+1/+10/+20/+30/+60s),
  and 61 idle RSS samples. All 841 sample values were matched to their raw JSONL ordinals, and all
  33 phase summaries (count/min/max/mean/nearest-rank p50/p95) were recomputed independently.
- Idle process RSS mean **26,340,368.786885247 B = 25.120133 MiB**; sampled maximum
  **26,361,856 B = 25.140625 MiB**. Joined-child CPU **112,665,000 ns**; faults 2717 minor,
  16 major. These are observations of this one run, not population estimates.
- Linux aarch64, 4096-byte pages, 1GiB cgroup memory, swap0, one CPU quota, pids64. Collector
  idle RSS is 655,360 B. Cgroup charge includes it; process RSS does not. No subtraction
  identifies live heap. Host load was captured privately; Docker guest interference is unknown.
- The first startup sample can precede child `exec` (collector fork image). Startup PSS was
  not sampled; readiness is socket-existence polling, not a request. Sampling can miss peaks;
  process/cgroup high-water counters are different metrics. Some summary/final timestamps carry
  the legacy-unspecified clock label; do not align clocks by subtraction.
- Seven `missing_metric` rows explicitly cover malloc accounting and six History-only metrics.
  Missing is not zero. No allocator/live-retained accounting, throughput or invoke latency was
  measured. Missing rows are attempt-level, not proof of every metric in every phase.

## Exact commands and numerical reproduction

Run analysis from the canonical study directory. These are the historical direct sequential CLI
calls, not a workflow. Do not rerun plan/run: the publication-only driver update below intentionally
fences the executed identities. It is not permission for another experiment.

```sh
python3 study.py check
python3 study.py plan --campaign approved-v4-pilot-01-proof --seed 20260905 --replicates 1 --cells RT-X02
python3 study.py run --campaign approved-v4-pilot-01-proof --lane runtime --max-cells 1
python3 analyze.py --campaign approved-v4-pilot-01-proof
python3 study.py export
```

The following query joins the entire measurement/evidence chain. Use a read-only connection
to `private/ledger.sqlite3`, or restore `exports/snapshot.sql` into an analysis-only database:

```sql
SELECT e.id, t.id, a.id, p.id, f.id, f.sha256,
       count(s.value), avg(s.value), max(s.value)
FROM experiment e JOIN trial t ON t.experiment_id=e.id
JOIN attempt a ON a.trial_id=t.id JOIN latest_parse p ON p.attempt_id=a.id
JOIN sample s ON s.parse_id=p.id JOIN artifact f ON f.id=p.input_artifact_id
WHERE t.campaign_id='approved-v4-pilot-01-proof'
  AND s.phase='idle' AND s.metric_id='process_rss_bytes'
GROUP BY e.id,t.id,a.id,p.id,f.id,f.sha256;
```

Expected numeric suffix: `61 | 26340368.786885247 | 26361856.0`; IDs/hash match the table above.
`analyze.py` independently selects successful non-infra latest parses and reports the same mean
and peak with `independent_replicates=1`. The proof-time snapshot was exported twice without DB
changes and matched byte-for-byte. An actual separate SQLite analysis restore passed integrity/FKs,
had **zero executor identities**, and returned exactly the same query result. The retained snapshot
hash above is stable; `exports/snapshot.sql` later includes the screen and delivery evidence.
See README's generic analysis-only restore command. Never restore a public snapshot as an executor.

## Containment, checksums and lifecycle

Preflight rechecked the canonical v4 identity/migrations/current driver, both empty slots, all
25 owner locks, unchanged daemon, no study containers/host targets, and all 168 pre-existing
artifact checksums. Host free space was 73.86GiB, above 15GiB. The collector verified
41,766,875,136 B of private Docker disk free, above 8GiB, **before launching the broker**.

The final-state artifact proves exit0, `Pid=0`, not running and not OOM-killed. Cleanup receipts
**53 (drained) and 54 (removed)** are followed by attempt completion and released slot. The
read-only verifier independently observed both immutable-ID and name absence on the same daemon,
volume absence, all owner locks available, both slots empty and no host targets. Every retained
artifact checksum, uploaded input, inventory member, parser source and numerical summary matched.
No fixture/parser correction or ownership exception was needed.

The collector producer's exact volume/target was already removed (receipts35/36), after retaining
its standalone binary and full work/source archive; runtime consumes that binary, not its target.
The History product is retained for the approved screen. No build was needed for this proof.
The one prior pre-create-failure volume remains safety-retained: without an archive endpoint the
cleanup refuses rather than inventing ownership/evidence. It holds no slot and no known target.

Immutable raw/source/compiler/work archives are evidence, including obsolete failures/products;
current binaries have explicit screen consumers. The live database/worktree/identity are safety
assets; backup and proof-time restored DB are recovery/analysis evidence. No raw deletion, shared
cache/target change, general Docker prune, package install, nested agent or production access occurred.

## Publication-only source fence

After all ten domain attempts, publication privacy cleanup generalized the export denylist from a
private local-DNS hostname to the `.lan` suffix. This strengthens redaction; no recipe, parser,
executor, schema, build input or workload was changed. Receipt `3d4c66333f464ff298a83a5d8c04ee6e`
pins both source/driver hashes. The final publication receipt is `e44b710ed14d45c6b95e90af9b743fbb`; the current registered driver is
`0b63b71a82dc21a750ad7f3f9d6a2e0d0e93ca507fed9a8078b84b5df7aee149`. The **executed** driver remains
the archived `002f2fc9…` above. No new domain trial was run or claimed under the publication driver.
The first generic substring check also matched SQL `e.lane` and failed three export unit checks;
that failed test receipt (`67cb52e7b10f4872b715fe8b83b6e130`) is preserved. One focused suffix-regex
correction cleared the gate: all35 tests, including refusal without overwriting an existing export,
passed (`116dc019ae4044abaf788986f7d3ed67`); stubbed workflow passed
(`1b21cc4bf9d4419a8ef5fcc602897686`). Export/restore was revalidated separately. No scientific
condition was relabelled and this publication fix did not consume or trigger a domain retry.
The local one-shot workflow prompt with owner delivery coordinates is ignored and retained only
as provenance, not a public reusable workflow.

Final evidence receipt `ba41abd50a2f4365a73ae4884fa8bb91` reverified all296 preceding artifacts,
all168 pre-proof artifact records unchanged, 35 owner locks available, no study targets/containers,
and both slots empty. The final export SHA-256 is
`0106358f89476fbb8ce89c551afe5b8d8f08755c31748e5321d7ae1e79120e3d` (3,284,878 bytes).
Two unchanged exports matched; a second actual analysis-only SQLite restore reproduced **all600
proof/screen summaries**, passed integrity/FKs, and had zero executor identities. Private
UUID/daemon/container/token values and personal-path/hostname sentinels were absent. Restore/local
delivery coordinates and their checksums are deliberately in ignored delivery metadata only.
