// Syntax/topology test only. Does not launch agents, workflows, or experiments.
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const file = path.join(__dirname, '../workflows/campaign.js');
const code = fs.readFileSync(file, 'utf8');
assert.ok(code.startsWith('export const meta = {'));
assert.ok(!/Math\.random|Date\.now|require\(|import /.test(code));
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
const run = new AsyncFunction('args', 'agent', 'parallel', 'phase', 'log', code.replace('export const meta', 'const meta'));
(async () => {
  for (const ready of [true, false]) {
    for (const parallelLanes of [true, false]) {
      const labels = []; const phases = [];
      const result = await run({studyRoot:'/synthetic/study',campaignId:'test',parallelLanes}, async (prompt, options) => {
        assert.ok(options.schema);
        assert.ok(!labels.includes(options.label)); labels.push(options.label);
        if (options.label === 'ledger-gate' || options.label === 'round-controller') {
          assert.equal(options.thread, 'memory-ledger-controller'); return {ready,blockers:[]};
        }
        return {attemptIds:[],blockers:[],status:'blocked'};
      }, jobs => Promise.all(jobs.map(fn=>fn())), name=>phases.push(name), () => {});
      assert.equal(labels.length,ready ? 7 : 2);
      assert.equal(new Set(phases).size,5);
      assert.equal(result.sourceOfTruth,'canonical SQLite only');
      assert.ok(result.ledger.every(item=>item && item.id));
    }
  }
  console.log('workflow syntax, live resume barriers, two lane bounds and blocked paths: PASS');
})().catch(error => { console.error(error); process.exitCode=1; });
