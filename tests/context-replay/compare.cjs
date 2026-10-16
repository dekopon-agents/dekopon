const assert = require('node:assert/strict');
const fs = require('node:fs');
const { spawnSync } = require('node:child_process');
const [fixturePath, nativePath, bindingPath] = process.argv.slice(2);
const input = JSON.parse(fs.readFileSync(fixturePath, 'utf8'));
const { run_replay } = require(bindingPath);
function native(value) {
  const process = spawnSync(nativePath, { input: JSON.stringify(value), encoding: 'utf8' });
  return process;
}
const result = native(input);
assert.equal(result.status, 0, result.stderr);
const nativeSteps = JSON.parse(result.stdout);
const wasmSteps = JSON.parse(run_replay(JSON.stringify(input)));
assert.equal(nativeSteps.length, input.steps.length);
for (let i = 0; i < nativeSteps.length; i++) {
  assert.deepStrictEqual(wasmSteps[i], nativeSteps[i], `step ${i}`);
  console.log(`step ${i}: parity; retained=${nativeSteps[i].retained.length}, visible=${nativeSteps[i].modelVisible.map(x => x.recordId).join(',')}, omitted=${nativeSteps[i].omittedRecordIds.join(',')}`);
}
assert.equal(nativeSteps[0].retained[0].id, '9007199254740993');
assert.equal(nativeSteps[0].modelVisible[0].recordId, '9007199254740993');
assert.equal(nativeSteps[0].modelVisible[0].answer, 'Remembered.');
assert.equal(nativeSteps[0].modelVisible[0].user.includes('"recordId":9007199254740993'), true);
for (const step of nativeSteps) {
  const bytes = step.modelVisible.reduce((sum, turn) => sum + Buffer.byteLength(turn.user) + Buffer.byteLength(turn.answer ?? ''), 0);
  assert.ok(bytes <= input.maxBytes);
}
assert.equal(nativeSteps[3].modelVisible.some(x => x.recordId === '9007199254740993'), false);
assert.equal(nativeSteps[4].retained.length, 5);
assert.equal(nativeSteps[4].modelVisible.some(x => x.recordId === '9007199254740997'), false);
assert.equal(nativeSteps[2].modelVisible.at(-1).user.includes('🌿 ca'), true);
for (const mutation of [
  x => x.cassette.shift(),
  x => { x.cassette[0].request.configId = 'other'; },
  x => { x.cassette[0].request.input.user = 'changed'; },
  x => { x.cassette[0].request.version = 'different-version'; },
  x => { [x.cassette[0], x.cassette[1]] = [x.cassette[1], x.cassette[0]]; },
]) {
  const bad = structuredClone(input);
  mutation(bad);
  assert.notEqual(native(bad).status, 0);
  assert.throws(() => run_replay(JSON.stringify(bad)));
}
console.log('five missing/mismatched/out-of-order cassette refusals: native and WASM');
