# Dekopon memory study

**Status: verified narrow pilot complete (2026-09-05); no further execution authorized.**
The canonical database is schema v4. After the independently accepted migration/build/Linux
owner-loss/History gates, **one real RT-X02 proof passed before the nine-trial screen**.
[E2E_PROOF.md](E2E_PROOF.md) joins raw evidence, hashes, numerical reconstruction, sanitized
export/analysis-only restore and deletion/slot release. The screen completed three fresh trials
each of RT-X01, RT-X02 and ST-X01-ARM64, sequentially, without repairs or retries.
[FINDINGS.md](FINDINGS.md) reports the actual observations and gaps, not a full memory-attribution study.
Read [DESIGN_DIGEST.md](DESIGN_DIGEST.md) for the compact accepted questions and exploratory controls.
The reusable lesson is [experimental-campaign](skill/experimental-campaign/SKILL.md).
Production was not accessed or changed; no proposed memory control was implemented.

> **Keep this active worktree.** `private/ledger.sqlite3` and immutable `private/raw/` currently
> contain local evidence. Do not remove the worktree, raw evidence, or sole database copy.
> The public SQL snapshot is sanitized and analysis-only, **not an executable backup**.

## Start / continue

Run from this directory (the owner's study symlink resolves here). Python 3.11+ and its standard
library suffice for the CLI/tests. Workload execution requires the **local Unix Docker Engine,
Linux cgroup v2 and locally available digest-pinned Linux arm64 images**. No automatic pulls,
installs, remote execution, credentials, provider/model calls, or inherited workload environment.

```sh
python3 study.py check
python3 study.py status --campaign approved-v4-pilot-01-screen
python3 analyze.py --campaign approved-v4-pilot-01-screen
```

The approved continuation applied migrations through v4, registered current sources, and built
only the two tiny tools in `approved-v4-pilot-01-tools`. Their exact binary products and producer
IDs are recorded in E2E_PROOF/FINDINGS. Older incompatible binaries remain immutable evidence,
not prerequisites. No build was needed during the proof or screen; no study targets remain.
Do **not** run init/migrate/controller-sync or rebuild as routine startup. Future changed sources
need fresh approval, quiescence, a backup, reviewed condition/driver registration and new identity.

`approved-v4-pilot-01-infra` passed collector and synchronized History checks. The independent
safety gate also accepted all four real killed/SIGSTOPped-owner records, each holding its slot
until PID1 deadline exit, `State.Pid=0`, recovery and deletion. GNU timeout yields the expected
non-retryable137; collector SIGALRM yields124. These are infra outcomes, not benchmark failures
to retry. The opt-in test is `python3 tests/linux_owner_loss.py --run-synthetic-faults`; it is
**not** part of routine inexpensive checks and was not rerun by the proof/screen continuation.

The exact completed screen plan and lane commands were:

```sh
python3 study.py plan --campaign approved-v4-pilot-01-screen --seed 20260905 --replicates 3 --cells RT-X01 RT-X02 ST-X01-ARM64
# Six sequential calls, inspecting each returned runtime attempt before the next:
python3 study.py run --campaign approved-v4-pilot-01-screen --lane runtime --max-cells 1
# Only after all six runtime trials, hash/absence/disk checks and eligible cleanup:
python3 study.py run --campaign approved-v4-pilot-01-screen --lane state --max-cells 1
```

The state call completed all three replicates of its single cell. All ten proof/screen attempts
succeeded at number1; the proof is excluded from screen summaries. These execution identities are
now fenced by a publication-only privacy update (see E2E_PROOF); do not rerun the historical plan/run
commands. Same-identity idempotence applies only with identical driver inputs, not a new run.
Any future campaign needs explicit approval; stop failed cells, do not fill gaps
with additional experiments or silently retry changed conditions.

Keep these lanes **sequential for CPU/latency comparisons**. Overlapping CLI workers/campaigns still
share the same two-slot database gate, but the gate does not remove interference. Each worker runs
at most six distinct ready cells and 18 attempts, sequentially; fewer is valid. No automatic retries.
The same plan ID is idempotent only for identical cells/replicates/seed/driver. Three independent fresh
replicates are the default; one/two are exploratory, never comparative confidence.

CLI exits: `0` means the ledger command completed, **not that all trials succeeded**; inspect returned
attempt statuses. `2` means a refusal/error. `attention` always means a lease remains held.

## What is executable, and what is not

| Cells | Classification and actual scope |
|---|---|
| RT-X01 / RT-X02 | Release-replica **variants**, P4 versus echo, cache off, synthetic policy, fresh tiny audit/checkpoint, no telemetry/TCP. A static observer in the same capped container spawns the release broker, observes socket readiness and 60 seconds of idle, then joins shutdown. No provider invocation. |
| ST-X01-ARM64 | **Source-kernel**, not gateway end-to-end. Byte-identical fenced `prompt/history.rs`; 128 windows ×12 turns of 2048B user/4096B answer, 64KiB/12-turn limits, four held clones, drop phases. Uncalled model serialization types are stubs; actual History retention/trim/clone methods are not reimplemented. No ConversationStore, lazy-expiry, grants, transport or model client measurement. |
| BUILD-COLLECTOR-ARM64 / BUILD-HISTORY-ARM64 | **Infra**; tiny static C/Rust standalone builds, no Cargo dependency graph. Pinned locally available Rust 1.86 arm64 container; actual compiler output/ELF identity retained. This is NOT the repository/release toolchain. |
| INFRA-SMOKE / INFRA-HISTORY / INFRA-WATCHDOG / INFRA-DEADLINE | **Infra**; collector counter validation, source fixture assertions, hard timeout/tree cleanup. Excluded from benchmark analysis. |
| RT-C01…06 | Original mapper screen remains **unsupported**: external observer parity, empty/populated cache pairing and matched glibc observer/trim are not implemented. X variants do not silently satisfy those cells. |
| C-STATE-001…006 | Original whole-library/private-seam screens remain **unsupported**; do not substitute Python allocation loops. |
| RT-D04/06/07/08/09; D-STATE-007…009 | Explicitly deferred ownership barriers, parallelism, stores/multi-memory/admission, gateway backpressure, exact replay-index prototype, storage recovery. Prerequisites/dependencies and follow-up questions are retained. |

Legacy `BUILD-COLLECTOR`, `BUILD-HISTORY` (unavailable local arm64 Rust 1.89 image) and `ST-X01`
(planned 1.89 kernel build) were blocked locally; use the explicit `-ARM64` conditions above. The
static matrix retains those historical planned identities; `disable` receipts explain local status.
The v3 backup preserves **31 staged cells**; the current v4 ledger has 32, including INFRA-DEADLINE.
Only three non-infra cells have executed successfully; 20 remain unsupported and three locally
blocked conditions are retained. This is not a completed Cartesian product.

Release identity: `0.12.0`, source `b9b9533a…`, image digest ending `…feb2e768`; source-kernel fence:
`5a03a296…`, History SHA-256 `c892c88bd9a25110677b7652d535c846fd9c67c584a29336e63d2c49daff6ff2`.
P4 is a representative baked set, not the known deployed configuration. Runtime inventory retains the
exact broker ELF and all baked component bytes/hashes before launch. The source kernel retains its
full original source and build inputs. Debug symbols are stripped/unavailable, not implicitly matched.

## Hard safety gates

- There is **no `--db`, arbitrary command, env, mount, image or target-path execution option**.
  The canonical database is resolved next to this code. A private UUID/device/inode marker rejects
  copied/replaced databases. Do not clone the harness to evade the global gate. All writers use
  `foreign_keys=ON`, FULL synchronization, WAL, a 10-second busy timeout and `BEGIN IMMEDIATE`.
- Two fixed rows (`slot` IDs1/2) atomically bind attempts/tokens. Every build, smoke and benchmark uses
  the same gate. A lease heartbeat is observation only; it never expires into permission to launch.
- A process-held flock protects each attempt. The controller lock is shared for execution, exclusive
  for migrations, matrix additions, driver updates, restore and publication. Update only at quiescence;
  campaign/attempt hashes pin recipes, schema, driver and input artifacts. No worker edits shared code.
- Each slot owns one named private Docker container/PID namespace/cgroup and one private volume.
  Before create, the DB records the resource name/token and daemon ID. Immutable container ID,
  creation timestamp and observed start timestamp are then retained. Known IDs, not PID numbers or
  names alone, are checked. The actual `/info` daemon identity is re-read before and after API
  requests, drain, deletion and absence observations; a replacement endpoint returning 404 holds the lease. Rename/restart/daemon mismatch or unavailable inspection blocks recovery.
- Containers have **1GiB memory, zero swap, one CPU, 64 pids**, no network, no capabilities, no new
  privileges, UID65532, readonly root, bounded file descriptors/file size/logs and no restart policy.
  No host source/credentials/socket/PVC mounts; inputs are bounded tar uploads into the private volume.
  Docker HostConfig is verified before start; the C/shell preflight reads back actual cgroup v2 caps.
- Host disk must have **15GiB** physically free before each trial/build; the private Docker build/data
  disk must have **8GiB** before compilation/child launch. Pre/post local free bytes and Docker's
  in-container free-space observation are retained. Disk is not globally reserved: unrelated writers
  can consume it. Fixed tiny recipes, per-file/output limits, bounded archive reads and deadlines
  bound this harness; this is not a general filesystem-quota facility. No full Cargo builds/spill tests.
- Wall deadlines are 2–180 seconds depending on recipe. Builds/watchdog run GNU `timeout` as
  PID1 with `--foreground --signal=KILL`; its exit tears down the namespace including `setsid`
  descendants. The static collector requires PID1 and installs a SIGALRM `_exit(124)` deadline
  **before** preflight/fork. No Docker init wrapper is permitted. These timers do not depend on Python
  polling, owner survival or cooperative descendants. The CLI is an additional timer. Cancellation kills the owned container,
  checks stopped state and captures bounded evidence. **A stopped snapshot alone never releases the
  slot**: confirmed container deletion/absence is the final barrier against a delayed start RPC.
  The daemon owns all descendants, including `setsid` children. Tool/daemon failure keeps the slot.
- A trusted local daemon/kernel and non-malicious same-UID operator are assumptions. Docker API access
  itself is host-privileged; never expose it to workloads. A same-UID actor deliberately rewriting code,
  DB, locks or containers is outside this harness's boundary. Unsupported platforms fail closed.

Pi execution, macOS-native RSS benchmarks, alternative allocators, profiler injection and package
builds are **not implemented backends**. Do not weaken containment or install host/Pi packages to
unblock them. Host sccache, Cargo wrapper/incremental settings and other worktrees remain untouched.
Container standalone `cc`/`rustc` does not use or claim compatibility with the host compiler cache.

## Recovery, retry and cleanup

```sh
python3 study.py status --campaign screen-01
python3 study.py cancel --attempt HEX_ID     # request; DOES NOT free the slot
python3 study.py recover --attempt HEX_ID    # only after owner is dead/unlocked
python3 study.py retry --attempt HEX_ID      # transient/interrupted first attempt only
python3 study.py continue --campaign screen-01 # retry prerequisite checks, no previous attempt
python3 study.py run --campaign screen-01 --lane runtime --max-cells 6
python3 study.py cleanup --attempt HEX_ID    # exact owned stopped volume only
```

Recovery takes the owner lock, verifies the same daemon/container/create/start identity, kills if
necessary, retains evidence and removes the container before releasing. A pre-create crash can be
recovered only after verified absence. Unknown identities, interrupted evidence writes, missing raw
archives or failure to prove deletion require owner attention; do not clear SQL slot rows by hand.
Already-written artifacts are immutable; partial/orphan files are preserved rather than overwritten.

Pre-claim failures keep their causes in `trial_incident` without inventing an attempt. A primary
outcome/error is committed before drain, evidence collection or deletion; secondary cleanup errors
cannot change its retry classification. Recovery preserves known OOM/nonzero/deadline results rather
than relabelling them interrupted. An unknown owner-lost outcome remains explicitly interrupted.

Retries have a predecessor FK, maximum attempt2 and the identical condition/driver hash. Changed
compiler/image/platform/recipe/artifacts need a new condition ID or newly recorded driver condition
and campaign, not a retry. Resource exhaustion, unsafe prerequisites, parser/fixture bugs and
benchmark failures are not silently retried. Earlier failures remain visible.

Cleanup only removes the exact **study-owned Docker volume**, including its inactive
`/work/run/target`, after hashes and a complete work archive have been retained; builds have an
additional standalone binary receipt before becoming prerequisites. Unknown/foreign labels, live
containers, symlinks/out-of-root evidence, missing archives and checksum conflicts refuse cleanup.
Container deletion can complete while volume deletion refuses; a stopped/absent process tree is
independent of retained disk evidence. No host `target/`, source, raw file, database, shared sccache,
registered worktree or another agent's files are deletion candidates. No `cargo clean` or host `rm -rf`.
Consult the owner's cleanup skill before any future target/worktree cleanup outside this closed path.

## Measurements and limitations

`collector.c` reads counters, never process memory contents. Sampling is approximately 100ms during
broker startup, 1s idle; smaps-derived deep aggregates at observed ready and +1/+10/+20/+30/+60s.
The History fixture uses 20ms process sampling and 150ms hold barriers. Raw evidence is bounded
numeric JSONL and a private work archive, **not full raw smaps mapping dumps**. Collector source/hash
is necessary to interpret those mapping categories; the original external-observer cells remain gaps.

- RSS/HWM/virtual bytes come from child `/proc/status`; PSS/anonymous from smaps_rollup; unnamed
  executable mapping RSS is JIT-consistent only. Mappings do not identify allocation ownership.
- cgroup memory.current/peak, anon/file/kernel and working set include the collector. Its own process
  RSS is separately observed, not subtracted from unlike cgroup accounting. Kernel high-water and
  sampled maxima remain distinct. Sampled peaks are lower bounds; missed spikes are not zero.
- Socket existence is a sampling-delayed readiness proxy. CPU/fault totals use joined-child wait4;
  proc I/O counts block bytes, not logical read/write lengths. Rust operation latencies exclude fixture
  construction but the driver/JSON emission still perturbs the process.
- New History builds use a two-pipe, acknowledged phase protocol (`history-pipe-v1`). The child
  waits for an observer deep snapshot at each named phase. Drop transitions first suspend sampling,
  then drop and acknowledge a post-drop snapshot, so a sample cannot straddle a destructive boundary.
  Periodic load/clone-phase samples include the operations, not just steady holds. Pipe/barrier overhead
  is instrumentation, not a production control. Collector and child elapsed clocks remain separate
  (`sample.clock_origin`); never align them by subtraction.
- Strict History parsing requires all six synchronized phases with RSS/PSS/cgroup snapshots. Legacy
  or incomplete non-strict reparses place observer metrics in `whole-run` and record `parse_scope` as
  `whole-run-only`. Existing v3 evidence is whole-run-only regardless of its old startup labels.
  The real v4 History smoke and three screen trials passed synchronized reconstruction. The pilot
  shows logical text counts can reach zero without RSS returning to startup; it does not identify
  which allocations remain resident or establish a production memory saving.
- The execution driver is archived per attempt. A post-screen export-denylist privacy cleanup
  registered a new publication driver; it changed no workload/parser/build inputs and does not
  relabel the proof or screen. The old plan/run identities must not execute under it.
- `environment.libc` is explicitly the **static collector's libc**, not proof of the broker's loaded
  malloc implementation. Broker ELF/provider hashes and image identity are retained; exact loaded
  broker libc/API interposition and live/retained allocator accounting remain missing prerequisites.
- Missing metrics get `missing_metric` rows, not zeros. `retained_text_bytes` is a logical count, not
  allocator capacity/RSS. Collector overhead has not been causally calibrated against a plain control.
  Host load averages/other active attempts are private metadata; Docker guest interference is unknown.
- Local Linux arm64 smoke observations report **4KiB pages**. That is not evidence for the 16KiB Pi,
  macOS, production provider/config parity, CPU performance, retention savings or absence of leaks.

## Actual schema and analysis

[`migrations/`](migrations/) is authoritative (canonical live version4; pre-migration v3 backup retained). Primary identity/null guards,
foreign keys, typed level checks, unit definitions and immutable attempt/resource/evidence constraints
are enforced, not JSON-only conventions. The relational families are:

- `study`, `subsystem`, `hypothesis`, `reference`, `hypothesis_reference`;
- `source_revision`, `build`, `provider`, `provider_set`, ordered `provider_member`;
- `factor`, typed `factor_level`, `experiment`, `assignment`, `recipe`, comparison groups/members;
- prerequisites/evidence and experiment dependencies; campaign, trial/replicate/order/pair, retry attempt;
- `binary_version`, `binary_requirement`, immutable `binary_product` and `attempt_input`: consumed
  binary → successful producer → compiler build/image → source revision, exact input/source hash,
  retained source/work archive and compiler log. Prerequisite evidence now has an artifact FK.
  History 1.89 cannot consume a 1.86 product, even when hashes match. Legacy unversioned products
  remain evidence, not executable prerequisites; no provenance is invented for old attempts;
- `trial_incident`, primary `attempt_outcome`, separately tagged cleanup errors;
- environment, executor identity, slot, resource, error, recovery and cleanup ledgers;
- artifact, metric, append-only parse_run/parse_scope, sample/clock_origin, phase_summary, missing_metric;
- `execution_sequence`: global atomic claim order, start-RPC dispatch order and start-observation time.
  Comparison members share their group's minimum scheduling priority, then use shuffled trial order;
  ready prerequisites still win. Dispatch order is not a claim of perfectly simultaneous daemon start
  order; immutable Docker start timestamps are retained separately. Legacy sequence is unknown;
- finding/evidence links, proposed_control, followup/dependency.

Only exact bounded configuration/command/tool snapshots are JSON. Joins reconstruct the matrix.
`matrix`, `coverage`, `unresolved`, `latest_parse`, `results` are useful views. Use read-only queries:

```sh
python3 analyze.py --campaign screen-01   # bounded results; excludes infra and failed attempts
python3 - <<'PY'
import sqlite3
c = sqlite3.connect('file:private/ledger.sqlite3?mode=ro', uri=True)
for row in c.execute('SELECT id,lane,cell_status,trials,succeeded FROM coverage ORDER BY id'):
    print(row)
for row in c.execute('SELECT id,lane,prerequisite,satisfied FROM unresolved WHERE satisfied=0 LIMIT 64'):
    print(row)
PY
python3 study.py reparse --attempt HEX_ID
python3 study.py record-findings --file findings.json # only where referenced evidence exists
```

A changed parser creates a new `parse_run` with input artifact hash, parser SHA and supersedes link;
raw evidence and previous samples remain. New attempts retain the allowlisted harness source tar,
and new parses retain their exact parser source, so future analysis does not require rebuilding. Unchanged reparsing is idempotent. Do not edit old migrations
or raw files. Analysis uses only the latest parse, groups by campaign/cell/architecture/pages/driver,
and reports independent replicates; matching numbers are not causal ownership attribution.

## Backup, export, restore and relocation

```sh
python3 study.py backup --name before-round2
# Copy private/raw/, private/controller/ and private/backups/before-round2/ to independent
# private storage; the manifest lists every required immutable evidence file and checksum.
python3 study.py export                      # reproducible exports/snapshot.sql, quiescent
python3 study.py restore --name before-round2 # same canonical DB only, retains a pre-restore backup
python3 study.py check
```

Backups use SQLite's backup API, not a copy of a live WAL file. The DB backup alone is insufficient.
Restore validates identity, integrity, no active leases and every manifest checksum; it preserves
current rows in a separate pre-restore backup and leaves all raw files intact. Restoring into the
same file preserves its canonical inode. Restore tests run only in disposable test databases.

Reconstruct the public, sanitized relational export for **analysis only**:

```sh
python3 - <<'PY'
import sqlite3
from pathlib import Path
p = Path('private/published-analysis.sqlite3')
if p.exists(): raise SystemExit('refusing overwrite')
c = sqlite3.connect(p)
c.executescript(Path('exports/snapshot.sql').read_text())
c.execute('PRAGMA foreign_keys=ON')
assert not c.execute('PRAGMA foreign_key_check').fetchall()
assert c.execute('SELECT count(*) FROM executor_identity').fetchone()[0] == 0
print(c.execute('SELECT count(*) FROM experiment').fetchone()[0])
PY
```

Exports preserve relational IDs, conditions' hashes, metrics/units, failures, evidence hashes,
coverage, input/producer/build/source/tool-version links and execution sequence. Raw contents/commands, daemon/container identities and free-form runtime diagnostics are
redacted or pseudonymized. No private brief, hostnames, personal paths or payloads are published.
Inspect the export diff before publication; generic automatic redaction is not a promise about
arbitrary future authored findings. Repeating export without DB changes is byte-identical.

All artifact references are relative. A same-filesystem move that preserves the sole canonical
inode is possible only with the owner, no leases/workers, a complete backup and proper Git worktree
relocation/symlink handling. **Cross-filesystem copies are intentionally non-executable**; rebinding
is not automated because two runnable copies would violate the global gate. Do not bypass the
identity marker. No retention expiry or raw-data deletion is automatic; synthetic/private evidence
stays until the owner approves an independently backed-up retention plan.

## Resumable Dynamic Workflows campaign

[`workflows/campaign.js`](workflows/campaign.js) uses the documented runtime (raw JavaScript, literal
meta, phases, `agent`, thunk-based `parallel`, schemas and explicit JSON return). It does not import
filesystem modules or launch nested agents/workflows. `initial-campaign.js` is unchanged.

After approval, use the Dynamic Workflows **tool** with `script` equal to the file's raw contents
(not its filename), `maxAgents: 7`, `concurrency: 2`, `agentRetries: 0`, and:

```json
{"studyRoot":"<absolute canonical study directory>","campaignId":"screen-01","seed":20260905,"parallelLanes":false}
```

At most: one live preflight/controller, two workers round1, one adaptive controller, two workers
round2, one live read-only audit. Each worker runs only its lane's CLI, at most six cells. The adaptive
boundary may unblock prerequisites or schedule a permitted identical retry; unsupported follow-ups
require a future single-writer change, not new speculative work. No default whole-campaign rebuild.
Named controller/audit threads are documented non-journaled resume barriers: they re-read the DB,
so cached workflow results never imply completion. To resume an incomplete run use `resumeFromRunId`
with the same raw `script`/args and bounds (no simultaneous `name`); the database remains authoritative.
The workflow syntax/topology has been stub-tested, **not launched by this bootstrap task**.
