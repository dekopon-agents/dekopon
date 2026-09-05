export const meta = {
  name: "dekopon_memory_initial_campaign",
  description: "Build a relational memory-study ledger, run two bounded adaptive experiment rounds with two global slots, and publish evidence-linked findings",
  phases: [
    { title: "Map" },
    { title: "Build" },
    { title: "Execute" },
    { title: "Synthesize" },
    { title: "Audit" }
  ]
};

// Invoke with concurrency: 2, maxAgents: 12, agentRetries: 0.
// The executor's canonical SQLite ledger must enforce two live experiments globally,
// including across overlapping workflow invocations. This outer limit is not enough.
const workspace = args && args.workspace;
const repoWorktree = args && args.repoWorktree;
const briefPath = args && args.briefPath;
if (![workspace, repoWorktree, briefPath].every(value => typeof value === "string" && value.startsWith("/"))) {
  throw new Error("workspace, repoWorktree and briefPath must be explicit absolute paths");
}
const cellsPerWorker = args && Number.isInteger(args.cellsPerWorker)
  ? Math.max(1, Math.min(args.cellsPerWorker, 6)) : 6;
const ledger = [];
const common = `Study workspace: ${workspace}. Dedicated source/research worktree: ${repoWorktree}. Read the owner brief at ${briefPath} and applicable AGENTS.md/design/development instructions. The brief supplies established paths, versions, measurements, safety rules and delivery requirements: do not repeat broad environment discovery. Production is read-only. Isolated experiments use synthetic data and no real credentials, chat content or external model/provider calls. No nested agents/workflows. Preserve other worktrees. Return concise structured results, not transcripts.`;

async function call(label, prompt, schema) {
  try {
    const result = await agent(`${common}\n\n${prompt}`, { label, schema });
    ledger.push({ id: label, status: result === null ? "missing" : "returned", result });
    return result;
  } catch (error) {
    ledger.push({ id: label, status: "error", error: String(error).slice(0, 1500) });
    return null;
  }
}

const mapSchema = {
  type: "object",
  properties: {
    id: { type: "string" },
    hypotheses: {
      type: "array", maxItems: 10,
      items: {
        type: "object",
        properties: {
          id: { type: "string" }, question: { type: "string" }, subsystem: { type: "string" },
          factors: { type: "array", items: { type: "string" }, maxItems: 8 },
          recipe: { type: "string" }, metrics: { type: "array", items: { type: "string" }, maxItems: 10 },
          prerequisites: { type: "array", items: { type: "string" }, maxItems: 6 },
          evidenceRefs: { type: "array", items: { type: "string" }, maxItems: 6 },
          limitation: { type: "string" }
        },
        required: ["id", "question", "subsystem", "factors", "recipe", "metrics", "prerequisites", "evidenceRefs", "limitation"]
      }
    },
    implementationHints: { type: "array", items: { type: "string" }, maxItems: 10 },
    risks: { type: "array", items: { type: "string" }, maxItems: 8 }
  },
  required: ["id", "hypotheses", "implementationHints", "risks"]
};

phase("Map");
const maps = await parallel([
  () => call("map-runtime", `Decision supported: design the native-memory/Wasmtime half of the matrix and identify experiments that can actually attribute the observed broker baseline. READ-ONLY; write no files, run no benchmarks. Inspect at most 12 relevant implementation files (beyond required docs), restricted to broker-host, provider-sdk host, brokerd startup/runtime, allocator/build configuration and relevant fixture APIs. Production release is 0.12.0; distinguish it from current main using git show when needed. Produce at most 10 stable hypotheses, prioritize 6 cheap discriminating cells for the first round. Address allocator live vs retained memory (e.g. isolated mallinfo2/malloc_info/trim observations), provider count/size, compiled code vs compiler scratch, compilation cache, store lifecycle/guest limits and admission concurrency. Prefer verified release artifacts plus tiny instrumentation over unnecessary full Rust rebuilds. Identify profiler overhead and portability limits. Do not map gateway/Cedar/storage internals; the other mapper owns those. No broad web research; at most two installed dependency/API references if a recipe needs confirmation. Stop at actionable recipes with source references or explicitly mark unknown.`, mapSchema),
  () => call("map-state", `Decision supported: design the application-state/policy/storage half of the matrix and an evidence-driven future-control taxonomy. READ-ONLY; write no files, run no benchmarks. Inspect at most 12 relevant implementation files (beyond required docs), restricted to Cedar/policy, broker replay/audit metadata, gateway/session/history, storage-host and telemetry queues. Do not remap Wasmtime/allocator internals. Produce at most 10 stable hypotheses; prioritize 6 feasible synthetic/no-network cells for the first round, state exact seams/configuration and observability needed. Cover retention/bounds/admission/cancellation, CPU and p50/p95 latency, disk-backed state/spill/quotas/durability/security, and portability; do not assume a proposed control is implemented. Where no executable seam exists propose a minimal isolated harness and mark the difference from a real service measurement. No broad web research; at most two installed dependency/API references if needed. Stop at actionable recipes and explicit deferrals.`, mapSchema)
]);
if (maps.every(value => value === null)) {
  return { status: "blocked-planning", workspace, ledger };
}

const buildSchema = {
  type: "object",
  properties: {
    ready: { type: "boolean" }, cliPath: { type: "string" }, databasePath: { type: "string" },
    testsRun: { type: "array", items: { type: "string" }, maxItems: 12 },
    workerInstructions: { type: "string", maxLength: 3500 },
    blockers: { type: "array", items: { type: "string" }, maxItems: 10 }
  },
  required: ["ready", "cliPath", "databasePath", "testsRun", "workerInstructions", "blockers"]
};
const reviewSchema = {
  type: "object",
  properties: {
    verdict: { type: "string", enum: ["pass", "fix", "blocked"] },
    findings: { type: "array", maxItems: 12, items: { type: "object", properties: {
      id: { type: "string" }, severity: { type: "string" }, location: { type: "string" }, remedy: { type: "string" }
    }, required: ["id", "severity", "location", "remedy"] } },
    checksRun: { type: "array", items: { type: "string" }, maxItems: 12 }
  },
  required: ["verdict", "findings", "checksRun"]
};

phase("Build");
let built = await call("build-ledger", `You are the ONLY shared code writer. Turn the brief into a working study, not just a proposal. You alone consume these raw mapper results: ${JSON.stringify(maps.map((result, i) => ({ id: i === 0 ? "runtime" : "state", result })))}. Compact their accepted/rejected hypotheses and references into one on-disk design digest; downstream workers will read it instead of these reports. Implement only under studies/memory/ in the dedicated worktree. Create the actual normalized SQLite DB, migrations/constraints, seeded staged matrix, canonical transactional two-slot executor, CLI, bounded recipes/measurement collectors, failed-attempt/evidence/cleanup ledgers, relevant tests, and a practical README with exact execution/analysis/continuation commands. Ensure the two lanes named runtime and state have executable prioritized synthetic recipes where feasible; do not replace real experiments with arbitrary toy microbenchmarks without explicit classification. Enforce process-tree ownership, timeout/cancellation, safe lease recovery, foreign keys, metric units, provenance and conservative disk guards. Make raw/private data ignored and a sanitized relational export reproducible. Preserve the existing private brief exclusion. Do not run unbounded full builds or a whole campaign during bootstrap; small schema/runner/instrumentation smoke tests are appropriate and any actual benchmark must claim a slot. Save workflows/campaign.js for future resumable execution through the Dynamic Workflows tool, using the documented runtime and global DB gate. Do not overwrite workflows/initial-campaign.js. Read the workflow-authoring skill before writing workflow code and the cleanup skill before cleanup. The implementation should be dependency-light, easy to test and truly executable. Use at most the mapper-selected code seams plus required docs, no new broad research. Run deterministic tests including concurrent claim contention, interrupted process/lease safety, foreign-key failures, retry provenance and safe cleanup refusal. Do not commit/push yet. Stop when infrastructure is tested, worker commands are documented, and remaining unsupported experiments are represented honestly. If an essential safety gate cannot be made reliable, return ready=false rather than launch.`, buildSchema);
if (!built || !built.ready) return { status: "blocked-build", workspace, ledger };
const gate = await call("review-infrastructure", `READ-ONLY independent review of ${workspace}. Inspect README, schema, runner, safety tests and saved workflow. No benchmarks, installations, editing or broad source research. Execute the documented inexpensive tests against temporary test databases and inspect a live DB status only. Fixed rubric: referential matrix reconstruction and units; exact version/platform provenance; genuine bounded recipes; no credential/production mutation; global <=2 live process trees across workflows; no unsafe lease reclamation or orphan children; cleanup ownership/path/process guards; failure/attempt evidence; reproducible exports; tests exercising these invariants. Return at most 12 actionable findings. A missing essential gate is blocked/fix, not pass.`, reviewSchema);
if (!gate) return { status: "blocked-review-missing", workspace, ledger };
if (gate.verdict !== "pass") {
  built = await call("repair-infrastructure", `You are the ONLY shared code writer. Apply this bounded infrastructure review to existing files: ${JSON.stringify(gate)}. Read current README/digest and inspect only the implicated code. Do not redo planning. Repair at most these 12 findings plus directly necessary tests; run all relevant safety tests. Do not start the experiment campaign, commit or publish. Return ready=true only with evidence the essential gates now pass; otherwise preserve the database and return blockers.`, buildSchema);
  if (!built || !built.ready) return { status: "blocked-repair", workspace, ledger };
}

const workerSchema = {
  type: "object",
  properties: {
    lane: { type: "string" }, completed: { type: "array", items: { type: "string" }, maxItems: 6 },
    failed: { type: "array", items: { type: "string" }, maxItems: 12 },
    blocked: { type: "array", items: { type: "string" }, maxItems: 20 },
    readyRemaining: { type: "integer", minimum: 0 }, safeToContinue: { type: "boolean" },
    proposals: { type: "array", items: { type: "string" }, maxItems: 6 },
    artifactRefs: { type: "array", items: { type: "string" }, maxItems: 10 }
  },
  required: ["lane", "completed", "failed", "blocked", "readyRemaining", "safeToContinue", "proposals", "artifactRefs"]
};
const adaptSchema = {
  type: "object",
  properties: {
    safeToContinue: { type: "boolean" }, readyRemaining: { type: "integer", minimum: 0 },
    decisions: { type: "array", items: { type: "string" }, maxItems: 12 },
    addedExperimentIds: { type: "array", items: { type: "string" }, maxItems: 8 },
    blockers: { type: "array", items: { type: "string" }, maxItems: 12 }
  },
  required: ["safeToContinue", "readyRemaining", "decisions", "addedExperimentIds", "blockers"]
};
phase("Execute");
const rounds = [];
let termination = "round-bound";
for (let round = 1; round <= 2; round++) {
  log(`Experiment round ${round}: two lanes, at most ${cellsPerWorker} cells per lane, global DB slots <= 2`);
  const results = await parallel(["runtime", "state"].map(lane => () => call(`run-${round}-${lane}`, `You are experiment worker ${lane}, round ${round}. Read the README, design digest, lane's queued matrix rows and exact CLI help. Bootstrap worker handoff: ${built.workerInstructions}. Do not edit shared source/schema/README/workflows. Claim at most ${cellsPerWorker} ready cells for YOUR lane through the canonical database CLI; execute each cell's default three comparative replicates sequentially, never more than one live experiment tree per slot. Runtime owns allocator/Wasmtime/native footprint; state owns policy/replay/gateway/history/buffers/storage/telemetry. Prefer same-environment controlled comparisons and previously retained binaries. Put private raw artifacts in disjoint lane/attempt directories. Every actual trial/compile/profile must obtain the global slot; no detached or nested work and no bypass database. Observe resource/disk thresholds and promptly clean exact inactive study-owned targets after preserving binaries/symbols and evidence. Do not delete another lane's artifacts or shared compiler cache. Work around errors only through documented bounded alternative recipes with new condition/provenance records; never silently change the independent variable or weaken safety. Missing tool/unsafe prerequisite is a blocked cell, not permission to install on production or inspect private data. If a driver bug requires common-code changes, record a targeted repair task and continue an independent ready cell. Record novel hypotheses as proposed tasks, not concurrent source edits. Inspect final process/container and slot status before returning; do not release a slot while its child might still run. Stop at the cell bound, a safety blocker, or no ready cells. Report IDs/counts/paths only; the DB is authoritative.`, workerSchema)));
  rounds.push({ round, lanes: results.map((result, i) => ({ id: i === 0 ? "runtime" : "state", result })) });
  if (round === 2) break;
  const adaptive = await call("adapt-matrix", `You are the ONLY shared code writer/controller between experiment rounds. Workers have returned: ${JSON.stringify(rounds[rounds.length - 1])}. Read the canonical DB, failed-attempt evidence, digest and named targeted repair tasks, not all raw logs. First verify there are zero live experiment trees and acquire the documented controller/update guard; expired leases alone are NOT sufficient. If any ownership is uncertain, record an incident and return safeToContinue=false without launching or patching. Otherwise adjudicate findings once into a compact evidence digest. Repair at most three concrete fixture/parser/executor defects and add at most eight high-information follow-up cells, preserving original failed attempts and driver versions. Tests must pass before requeueing; at most two attempts per trial, and changed conditions create new experiments. Do not expand into a general product implementation. Keep claims with missing evidence explicitly unknown, ensure each lane has distinct ready work where feasible, export status and checkpoint/backup the database. Recheck disk/cleanup. No benchmark execution in this controller call. Return whether round two is safe and has useful ready work.`, adaptSchema);
  if (!adaptive || !adaptive.safeToContinue) { termination = "blocked-controller"; break; }
  if (adaptive.readyRemaining === 0) { termination = "no-ready-cells"; break; }
}

const deliverySchema = {
  type: "object",
  properties: {
    status: { type: "string" }, databasePath: { type: "string" }, readmePath: { type: "string" },
    findingsPath: { type: "string" }, notePath: { type: "string" }, branch: { type: "string" }, prUrl: { type: "string" },
    experimentCounts: { type: "object", properties: { successful: { type: "integer" }, failed: { type: "integer" }, blocked: { type: "integer" }, pending: { type: "integer" } }, required: ["successful", "failed", "blocked", "pending"] },
    remainingGaps: { type: "array", items: { type: "string" }, maxItems: 12 },
    validation: { type: "array", items: { type: "string" }, maxItems: 12 }
  },
  required: ["status", "databasePath", "readmePath", "findingsPath", "notePath", "branch", "prUrl", "experimentCounts", "remainingGaps", "validation"]
};
phase("Synthesize");
const delivery = await call("synthesize-publish", `You are the ONLY shared code writer/finalizer. The campaign termination was ${termination}. Read the canonical DB, compact digest and existing study artifacts. Do not rerun research or benchmarks. Before changes verify no active experimental work/controller conflict; if unsafe record and return blocked. Produce evidence-linked FINDINGS.md and a cohesive MEMORY_CONTROL_DESIGN.md grounded in actual results: subsystem owners/budgets, aggregate admission/backpressure, hard/soft bounds, retention/eviction/trim/spill policies, CPU/latency/I/O/durability tradeoffs, telemetry and portable/security-preserving behavior. Proposed config fields must be explicitly Exploration, not production features. State what remains unmeasured and whether any allocation attribution is still only a hypothesis; a few measurements cannot justify full understanding. Refresh the README with actual schema, representative SQL joining full matrix including failures/missing cells, safe continuation and cleanup commands, run IDs and artifact provenance. Create a sanitized reproducible relational export of published evidence and test restoring it. Private DB/raw evidence and owner brief remain ignored; review staged paths/content for private hostnames/personal paths/identities/credentials. Ensure dependency/source/runtime version differences are visible. Run inexpensive documented validation and project documentation gates as applicable; no workspace-wide Rust rebuild for study docs/Python unless required. Read the create-note skill and create/index one substantive Vagus note with findings, evidence IDs, uncertainty and future-control framework, using vagus add-note --print-path (no hand-written frontmatter), and record its returned path locally. Commit scoped study work on research/memory-study, push and open a PR against main, then include the PR link in the note where practical without creating duplicates. Report errors honestly; no releases or production changes. Do not delete the active study worktree/database.`, deliverySchema);

phase("Audit");
const audit = await call("audit-delivery", `READ-ONLY final audit of ${workspace}. Delivery reference: ${JSON.stringify(delivery)}. Read final README/findings/control design, DB coverage views and sanitized export, not the entire research corpus. Spot-check at most five central quantitative claims back to trial/metric/artifact IDs and hashes; check that comparative sample counts and version/platform caveats match. Run only inexpensive documented tests, PR/git status, foreign_key_check/integrity checks, and experiment lease/process/cleanup status. Verify local raw evidence is retained, exports restore, <=2 invariant has test evidence, no study-owned target remains without a recorded consumer, private brief/raw paths are not staged/published, and the Vagus note path exists. Check remaining gaps and exact continuation command are explicit. No editing, benchmarking or additional agents. Return at most 12 actionable findings and pass/fix/blocked.`, reviewSchema);
return { status: delivery && audit && audit.verdict === "pass" ? "initial-campaign-delivered" : "initial-campaign-incomplete", workspace, termination, rounds, delivery, audit, ledger };
