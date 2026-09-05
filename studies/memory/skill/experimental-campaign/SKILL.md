---
name: experimental-campaign
description: Plan and validate a bounded empirical software study with durable evidence. Use for benchmark or experimental campaigns, especially before expanding a matrix or orchestrating trials. Not for ordinary implementation, production stress testing, or permission to launch workflows.
compatibility: A project-reviewed driver and local SQLite evidence ledger; execution backends and safety limits are project-specific.
---

# Experimental campaign: prove one real slice first

Use the project's existing tools. This skill is a small decision/checklist layer, not an
executor, schema, benchmark suite or authorization to build infrastructure.

## Establish the boundary

1. Read applicable project instructions and the current study README. Confirm the real Git
   root, approved scope, one code writer, canonical database and process-tree owner.
2. Identify the question, baseline/variant, scientific unit, units, independent replicate
   count, interference controls and observable acceptance/failure conditions **before running**.
3. Separate current implementation, source-supported hypotheses and proposed controls. Do not
   replace a domain workload with toy allocations or call an infrastructure smoke a result.
4. Inspect physical disk, active owners and retained assets. If isolation, termination evidence,
   input compatibility or capacity is missing, STOP with the exact prerequisite; never weaken it.

## Make SQLite the evidence authority

Keep durable relationships, not only JSON summaries or chat/workflow completion:

- experiment/condition → trial/replicate/order → attempt/outcome/error;
- source/build/toolchain/image and factors → exact consumed binary/input hashes;
- attempt → environment/limits/interference, command/driver identity and timestamps;
- attempt → immutable raw artifact/checksum → parser version → samples/summaries/units;
- finding → named evidence, plus missing coverage and cleanup/recovery receipts.

Use constraints, FKs, transactions and a single executable authority. Keep bounded configuration
snapshots as JSON where useful, not as a substitute for joins. A copied analysis DB must not
become a second executable gate. Preserve old attempts, parses, raw files and failures.

## First gate: exactly one real vertical slice

Before matrix or framework expansion, plan one approved domain trial and run it through the
actual driver and isolation boundary. Then inspect, rather than trusting a zero CLI exit:

- real workload completion, readiness if applicable, successful shutdown and outcome;
- expected phases, sample counts, missing metrics and units; no missing-as-zero conversion;
- environment and exact build/input/driver/parser identities, including producing sources;
- the experiment-to-measurement/artifact join and retained checksums;
- at least one numerical result independently reproduced from raw/SQL analysis;
- sanitized export plus analysis-only restore, integrity/FKs and the same numerical result;
- process-tree termination, resource deletion, lease release and eligible target cleanup.

Record stable IDs, exact **project-specific** commands, hashes and caveats in a short proof
report. One trial proves the pipeline, not a comparison, production parity or attribution.
Unit/synthetic/mock tests remain important safety gates, but are not this domain evidence.

If the slice fails, allow at most **one small fixture/parser repair and focused regression**
when already authorized. Preserve the old attempt; register changed driver/condition and use a
new campaign identity for one renewed proof. A second failure means blocker, not more machinery.
Any containment/ownership ambiguity stops immediately, without spending the repair allowance.

## Only then: a bounded scientific screen

- Run only approved cells/replicates. Independent fresh replicates are not retries; retries are
  bounded attempts of the same trial/condition for explicitly retryable causes. Changed compiler,
  allocator, inputs or configuration require a new condition, never an invisible retry.
- Preserve randomized order and dependencies. Record actual claim/start order and overlap.
  Serialize CPU/latency-sensitive lanes unless contention is the explicit condition. Slots cap
  concurrency; they do not eliminate interference or make three replicates statistical confidence.
- Stop failed cells, expose unsupported/blocked/missing coverage and exclude failed/infra attempts
  from successful benchmark summaries. Do not run follow-ups merely because results are interesting.
- Report memory with CPU, latency/throughput, faults and disk/I/O where meaningful. Distinguish
  logical bytes, RSS/PSS, cgroup charges, virtual reservations, sampled peaks and high-water marks.
  None alone identifies live heap, allocator retention or ownership; instrumentation needs controls.

## Executor and asset lifecycle are scientific preconditions

The executor—not agent count—must atomically enforce the shared slot ceiling for all workloads
and builds. Each lease needs a process-held owner and bounded whole-tree lifetime independent of
the controller. A stale heartbeat, dead parent or stopped snapshot alone is not a release barrier.
Require identity-checked termination/deletion evidence; unavailable inspection keeps the lease.
Never clear slot rows manually or fork the executable database to bypass the gate.

Check documented host/container watermarks before builds and between batches. Build only what the
next approved consumer needs. Retain exact binaries, available symbols (or explicit absence),
compiler/source/input hashes and raw work receipts **before** cleanup. Immediately remove the
exact inactive study-owned reproducible target/volume when eligible; recheck disk and record the
receipt. Preserve sources, live DB/raw evidence, active worktrees and shared compiler caches.
No general pruning, cache policy changes or deletion based only on a directory name.
Every retained asset needs a named consumer or an explicit evidence/recovery/safety reason.

## Delivery and orchestration

Recheck the exact final head cheaply: tests, DB integrity/FKs, numerical/provenance links,
export/restore, privacy exclusions, no active owners/slots/containers and cleanup receipts.
Publish only sanitized claims with gaps; keep raw identities, private paths and delivery receipts
local. Reuse an existing note/install receipt rather than duplicating delivery.

Workflows require explicit user opt-in and a separate bounded plan. Read-only auditors can check
milestone gates; cached workflow results never outrank the live ledger. Prefer direct sequential
execution for a tiny screen. Port this checklist, **not hardcoded universal shell commands**:
each project owns its reviewed recipes, isolation, metrics and repair/stop contract. Do not copy
an executor/schema or add a backend to satisfy this skill.

See [the verified narrow pilot](references/pilot.md) for an example, not a portable recipe.
