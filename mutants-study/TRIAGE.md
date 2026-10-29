# Mutation baseline study: stopped before measurement

> # Steering for GPT executors

Epoch: diaphanous-dilettantes. Step: `04-test-time/mutants/measure`. Lane: codex. IDEA-0040.
Base: `9089a6e1808cbac4d5ab4fb9969765c0394b2fa3` (typed-events-pr3, #837).
Branch: `study/mutants-measure`. No source, test, manifest, lockfile, or CI changes; no PR.

## Verdict and real stop

The study is **blocked by the brief's network stop**. `Cargo.toml:151-152` patches
`tracing-core` to `https://github.com/dekopon-agents/tracing` at revision
`2b696a18454a04df9c9a60569b6fb73a22415296`; `Cargo.lock:4296` locks that Git source.
The inherited Cargo home had no Git checkout or Git database. The three offline baseline
attempts all returned exit 101 before compilation, with:

```text
error: failed to load source for dependency `tracing-core`
...
can't checkout from 'https://github.com/dekopon-agents/tracing': you are in the offline mode (--offline)
```

A network-enabled baseline would need to fetch this GitHub dependency, beyond the allowed
crates.io and fixture-fetch measurement traffic. That fetch was not attempted. No credential
was read or written, no repository setting changed, and no failing test was observed.
Resume requires an explicitly authorized fetch of that exact dependency, then fresh passing
unmutated baselines at this base before mutation. Existing repository read/push access does
not override the brief's separate measurement-network stop.

## Inputs and setup observed

- Read `xrl/agents` steering at `7a5ceaf641b3516be8cfe7b2ff0769ad6690f598` before other work.
- Read the base's `AGENTS.md` (including Tests and Proportionate remedies), constitution,
  contribution guidance, repository map, and development Validation sections.
- Read campaign-trail's `tools/cloud-env/HELPER-RULES.md` through the GitHub API at current main;
  that path is absent at the plan stamp. Followed generic HTTP User-Agents and external scratch logs.
- Recovered the plan at `88236969aa6f015750a7d75b3a25be571fd22ecd`,
  `store/designs/diaphanous-plan-draft/plan/PLAN.md`, line 140. The dispatch adds typed-events-pr3
  to the older plan's prerequisites; followed the dispatch's exact base.
- Installed with `CARGO_HTTP_USER_AGENT=codex-study cargo install cargo-mutants --version 27.1.0 --locked`.
  Verified `cargo mutants --version`: `cargo-mutants 27.1.0`.
- Toolchain: rustc 1.98.1 (`48a229cea`), cargo 1.98.1 (`797e8a9bc`).
- Preserved inherited `CARGO_HOME=/workspace/.onboarding/cargo` and `CARGO_BUILD_JOBS=3`.
- `ci/fetch-external-provider-components.sh examples/providers` passed: JSONPlaceholder v0.3.0,
  memory-chat v0.5.0, gh v0.11.1; every pinned digest and size verified. No fixtures committed.

## Crate results

Mutation wall time is zero because mutation never started. Baseline wall times are the captured
offline dependency-resolution attempts, not test runtimes. Counts and scores are unavailable,
not zero-valued measurements.

| Crate | Baseline wall seconds | Baseline | Mutation wall | Caught | Missed | Timeout | Unviable | Score |
|---|---:|---|---:|---:|---:|---:|---:|---:|
| `dekopon-policy` | 0.077 | blocked, exit 101 | 0 s | N/A | N/A | N/A | N/A | N/A |
| `dekopon-broker` | 0.072 | blocked, exit 101 | 0 s | N/A | N/A | N/A | N/A | N/A |
| `dekopon-broker-protocol` | 0.065 | blocked, exit 101 | 0 s | N/A | N/A | N/A | N/A | N/A |

Score, when measurable, is caught / (caught + missed). No crate's baseline passed; no crate
was mutated. All mutants in all production files of the three requested crates remain untested;
no enumeration was possible without resolving the Git dependency, so their exact count is unknown.
No other crate was measured. No baseline or mutation timeout was raised. The 4-hour cap was not reached.

## Missed-mutant triage

| Function / mutant | Tag | Reason / invariant test |
|---|---|---|

There are no missed-mutant records because there was no mutation run. This empty table does
not claim complete coverage or equivalence. Tag counts: `kill` 0, `equivalent` 0,
`covered-elsewhere` 0, `skip` 0 (zero classified records, not measured survivor counts).
Dependent-test confirmation is inapplicable until survivors exist. No item is tagged `kill`.

## Highest-value kill items

Unavailable: no evidence supports ranking ten survivors. `mutants/kill-survivors` should remain
blocked on a completed measurement; this stop supplies no test-change brief or score receipt.

## Artifacts

Each crate directory contains the captured `baseline.log`; `baseline-results.json` records the
commands, exit codes, and wall times. `STUDY.json` records machine-readable status and scope.
None of `missed.txt`, `caught.txt`, `timeout.txt`, `unviable.txt`, or `outcomes.json` exists:
cargo-mutants produced no output, so none was synthesized or represented as a successful run.
All baseline logs were copied with `cp` from external scratch. Study evidence is pushed once
because no mutation run finished; no per-crate raw-result push milestone was reached.

## Conflicts and readings followed

1. Cached checkout predates dispatch: fetched the exact base and branched from it.
2. General steering says open a draft when blocked: this study forbids PRs, so report the stop.
3. General contribution guidance excludes generated coverage artifacts: this brief authorizes
   generated study artifacts on this branch; only study evidence is committed.
4. General validation describes workspace gates: use the study's three crate baselines and
   mutation gates; do not expand into unrelated crates.
5. The plan's `broker-protocol` shorthand means package `dekopon-broker-protocol`.
6. The base requires a non-crates.io Git dependency: obey the explicit network stop; do not
   replace the patch, edit the lockfile, or fetch it under assumed authorization.
7. HELPER-RULES asks for a report of at most 100 words; the explicit deliverable requires
   multiple result fields and permits 600 words plus tables. Follow the task-specific report.
8. The older plan omits the typed-events-series prerequisite: follow the later stamped dispatch.

## Choices I made

- Measured no later main: used the dispatched SHA even though fetching a later main was allowed.
- Fetched only the missing exact dekopon base, without tags; the steering's tag-fetch rule applies
  to PR briefs and this is expressly a study. The steering commit was present, so agents was not fetched.
- Selected exact cargo-mutants 27.1.0, the registry's stable release at setup, installed locked.
- Fetched all three repository-pinned external fixtures before any viable broker baseline.
- Kept both inherited Cargo home and build-job limit; set generic HTTP User-Agents for setup calls.
- Detected the Git patch before running a network-enabled baseline; confirmed the blocker offline
  in order policy, broker, broker-protocol, sequentially, so every requested crate has direct evidence.
- Used Python wall-clock capture after an initial timing-wrapper launch found `/usr/bin/time`
  absent (exit 127, no Cargo invocation). The three recorded baseline attempts are the subsequent runs.
- No `-j`/`--in-place`, sharding, file subset, or per-mutant order was selected: the prerequisite
  baselines stopped before those choices could affect a measurement. No timeout was applied because
  dependency resolution failed immediately. Required mutation timeout remains 120 seconds on resume.
- Read campaign files through the GitHub API when raw URLs returned 404; found the nested plan path
  from its stamped tree. No campaign files were edited. These were instruction reads, not measurement traffic.
- Kept logs in external scratch and copied only brief, nonsecret evidence into the study branch;
  omitted nonexistent raw mutation files instead of fabricating empty outcomes.
- Ran `git diff --check` and the documentation duplicate gate for the study documentation. Did not
  run mutation gates, dependent survivor reruns, workspace tests, clippy, rustdoc, cargo deny, formatting,
  packaging, provider rebuilds, daemon boundaries, ARM64/Wasmtime lanes, or CI: no source changed and
  the required crate baselines were blocked before compilation. Remote CI was not claimed.
- Retained the installed mutation tool for an authorized near-term restart; no workspace target
  was created, and no active or shared artifact was deleted. No credential file was created.
