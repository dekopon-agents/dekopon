export const meta = {
  name: "dekopon_memory_campaign",
  description: "Resume bounded synthetic memory experiments from the canonical SQLite ledger, never from workflow cache",
  phases: [{ title: "Preflight" }, { title: "Round one" }, { title: "Controller" }, { title: "Round two" }, { title: "Audit" }]
};

// Invoke through Dynamic Workflows with script = this file's raw contents,
// maxAgents: 7, concurrency: 2, agentRetries: 0. No nested workflows or agents.
// Default lane execution is sequential to avoid compiler/CPU measurement interference.
if (!args || typeof args.studyRoot !== "string" || !args.studyRoot.startsWith("/")) {
  throw new Error("args.studyRoot must name the existing canonical study directory");
}
if (typeof args.campaignId !== "string" || !/^[A-Za-z0-9_-]{1,64}$/.test(args.campaignId)) {
  throw new Error("args.campaignId must be a portable ledger ID");
}
const seed = args.seed === undefined ? 20260905 : args.seed;
if (!Number.isSafeInteger(seed)) throw new Error("seed must be an integer");
const location = JSON.stringify(args.studyRoot);
const campaign = args.campaignId;
const common = `Study directory: ${location}. Read README.md and DESIGN_DIGEST.md here only.
Production is read-only; no production calls, broad discovery, external model/provider calls,
new agents/workflows, downloads, arbitrary commands, source edits or independent databases.
Use the existing study.py CLI and its canonical DB gate. Do not modify the compiler wrapper.
Never release a lease manually. Unavailable prerequisites are missing coverage, not success.
Return concise IDs/status, no raw logs/private paths. Stop after the specified bounded work.`;
const gateSchema = {
  type: "object", properties: { ready: { type: "boolean" }, blockers: { type: "array", items: { type: "string" }, maxItems: 8 } },
  required: ["ready", "blockers"], additionalProperties: false
};
const workerSchema = {
  type: "object", properties: {
    attemptIds: { type: "array", items: { type: "string" }, maxItems: 18 },
    blockers: { type: "array", items: { type: "string" }, maxItems: 8 },
    status: { type: "string", enum: ["executed", "blocked", "incomplete"] }
  }, required: ["attemptIds", "blockers", "status"], additionalProperties: false
};
phase("Preflight");
// Threaded calls are documented live resume barriers, not journaled results. Consequently
// every resume rechecks the actual DB before any worker could be replayed as completed.
const preflight = await agent(`${common}
Run python3 study.py check and status. Require empty global slots and the collector/history
artifact prerequisites already satisfied; do not build prerequisites here. On any unsafe gate
return ready=false. Otherwise idempotently plan exactly:
python3 study.py plan --campaign ${campaign} --seed ${seed} --replicates 3 --cells RT-X01 RT-X02 ST-X01-ARM64
An existing different campaign definition is a blocker, not permission to reset it.
No more than four CLI commands; no edits.`, {
  label: "ledger-gate", thread: "memory-ledger-controller", schema: gateSchema
});
const ledger = [{ id: "preflight", result: preflight, missing: preflight === null }];
async function workers(round) {
  const lanes = ["runtime", "state"];
  const jobs = lanes.map(lane => async () => {
    const result = await agent(`${common}
Execution lane ${lane}, round ${round}, existing campaign ${campaign}. Recheck the live DB.
Run once: python3 study.py run --campaign ${campaign} --lane ${lane} --max-cells 6
Then: python3 study.py status --campaign ${campaign} --lane ${lane}
At most six ready cells, 18 fresh processes per worker, sequential within the CLI, no retries here.
Do not edit common source or construct alternative recipes. If attention/lease held, stop and
report the attempt ID. Never rerun completed trials merely because this workflow resumed.`, {
      label: `${lane}-r${round}`, schema: workerSchema
    });
    return { id: `${lane}-r${round}`, result, missing: result === null };
  });
  if (args.parallelLanes === true) {
    const values = await parallel(jobs);
    return lanes.map((lane, i) => values[i] || { id: `${lane}-r${round}`, result: null, missing: true });
  }
  const results = [];
  for (const job of jobs) results.push(await job());
  return results;
}
phase("Round one");
if (preflight && preflight.ready) ledger.push(...await workers(1));
else ledger.push({ id: "round-one", skipped: true, reason: "preflight missing/blocked" });
phase("Controller");
let second = null;
if (preflight && preflight.ready) {
  second = await agent(`${common}
Round-one bounded reports (not evidence): ${JSON.stringify(ledger.filter(x => /-r1$/.test(x.id)))}
At a quiescent DB boundary inspect this campaign's trial/error/prerequisite records with the
README analysis commands. At most six CLI commands. You may use continue --campaign ${campaign}
and retry --attempt ID only for a transient/interrupted FIRST attempt with identical condition,
a verified absent process tree, and no safety/fixture/resource-exhaustion error. At most two
attempts per trial. Never repair source here or open an independent/new comparison campaign.
Return ready=true only if supported pending work remains in this campaign and every global
lease is clear. Otherwise stop honestly; record unsupported future questions in your blockers.
This is the only adaptive boundary; no new research loop or unrun Cartesian-product claims.`, {
    label: "round-controller", thread: "memory-ledger-controller", schema: gateSchema
  });
  ledger.push({ id: "controller", result: second, missing: second === null });
}
phase("Round two");
if (second && second.ready) ledger.push(...await workers(2));
else ledger.push({ id: "round-two", skipped: true, reason: "no supported pending work or controller blocked" });
phase("Audit");
const audit = await agent(`${common}
Final READ-ONLY live audit of campaign ${campaign}; prior reports are hints only.
Run check, status --campaign ${campaign}, then the README results/coverage query once (at most
three CLI/query commands). Do not export, publish, commit, repair, delete or change the DB.
Report surviving leases, failed/missing cells, measured versus unsupported coverage, and whether
there are three independent successful replicates per comparative cell. Infra is never benchmark
evidence. No numerical broker allocation attribution without actual causal comparisons.`, {
  label: "live-ledger-audit", thread: "memory-ledger-audit", schema: workerSchema
});
ledger.push({ id: "audit", result: audit, missing: audit === null });
log("Campaign stopped. Canonical SQLite trials/evidence, not workflow reports, are authoritative.");
return { campaign, database: "private/ledger.sqlite3", sourceOfTruth: "canonical SQLite only", ledger };
