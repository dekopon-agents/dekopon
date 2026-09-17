# Repository tooling

## CI timing extraction

`ci_timings.py` is an opt-in, read-only study tool (Python 3.10+ standard library
and authenticated `gh`; tested with gh 2.100). It does not edit or rerun workflows.
Keep datasets outside the repository and do not publish fetched evidence by default.

From the repository root, choose a study directory, then freeze the sample before
collection. Completed includes success, failure, cancellation and other terminal
conclusions; it does not mean successful. Selection uses the GitHub run-list API's
creation-recency order, not mutable update timestamps or a completion-time sort.
Existing manifests keep their frozen IDs.

```sh
STUDY=../timing-study
python3 .github/scripts/ci_timings.py collect \
  --repo dekopon-agents/dekopon --workflow ci.yml --limit 50 \
  --output "$STUDY/data" --freeze-only
# Use one ID from data/manifest.json for a vertical slice before the batch.
python3 .github/scripts/ci_timings.py run \
  https://github.com/OWNER/REPO/actions/runs/RUN_ID --output "$STUDY/slice"
# A numeric run ID also works with --repo OWNER/REPO.
python3 .github/scripts/ci_timings.py collect \
  --repo dekopon-agents/dekopon --workflow ci.yml --limit 50 \
  --output "$STUDY/data"
python3 .github/scripts/ci_timings.py analyze "$STUDY/data"
PYTHONDONTWRITEBYTECODE=1 python3 .github/scripts/test_ci_timings.py
```

Use a new output directory for a new sample. Collection re-extracts an existing
frozen sample, rather than incrementally resuming it. It stops if a frozen run's
attempt count/status changed. A URL containing `/attempts/N` selects the run,
**including all its attempts**, not just N. `analyze` is entirely offline.
Exit 0 means the command completed; exit 1 reports a sanitized extraction/schema
error; argparse usage errors exit 2. A failed collection leaves a failed manifest,
partial tables and an explicit failing run ID; never present those as complete.

### Dataset and joins

CSV empty fields are null, not zero. All durations are seconds. Timestamps are UTC
GitHub REST API timestamps; missing, negative, sentinel and skipped intervals have
null durations. Completed zero-second intervals remain zero, and a cancelled step
with both valid timestamps retains its measured interval.

| File | Identity and purpose |
| --- | --- |
| `manifest.json` | Frozen run IDs/attempt counts, collection timestamps/state, counts, sanitized errors, extractor SHA-256, retained-file sizes/hashes and received stdout bytes |
| `runs.csv` | `run_id`; event, conclusion, run number, API head SHA and nullable PR head/number |
| `attempts.csv` | `(run_id, attempt)`; each attempt independently fetched, source provenance, elapsed interval |
| `jobs.csv` | `job_id`, with run/attempt foreign key; status, runner/labels, timings, source job, log and checkout coverage |
| `steps.csv` | `(job_id, number)`; status, API timings, historical source line and mapping status |
| `metrics.csv` | `(job_id, step_number, metric)`; numeric count/sum/min/max from allowlisted log lines; null step means timestamp attribution was ambiguous/unavailable |
| `sources.csv` | Revision/path references to content IDs, with immutable GitHub source URLs |
| `workflow_sources.json` | Historical workflow text, deduplicated by content ID (first 16 hex characters of SHA-256) |
| `analysis.json` | Deterministic nearest-rank p90/median/sum, elapsed/work, tails/gaps, controlled cohorts, cache maxima, retry/cancellation totals, reconciliation and analysis-code hashes |
| `execution_index.csv` | Observed job ID → canonical evidence ID, earliest observed attempt, carryover/uncertain/unmeasured classification |
| `source_references.json` | Historical content ID → job dependencies and named-step line ranges; joins into `workflow_sources.json`, not today's checkout |

The jobs endpoint is **attempt-specific** (`runs/ID/attempts/N/jobs`); every returned
job's attempt and unique ID are checked. However, failed-job reruns can expose new
IDs with earlier execution timestamps even through these endpoints. Offline analysis
matches carryovers only across attempts of the same run, using source job ID, full
display name (including matrix discriminators), platform labels, exact non-null valid
start/end times and conclusion. The earliest observed attempt is canonical evidence,
not an assertion about an unobserved original attempt. Raw tables stay unchanged;
carryovers are excluded from job/step/metric statistics and retry/growth totals.
Unmatched jobs starting before their attempt are classified uncertain and their work
reported separately, not counted as confirmed fresh retry work. Null/sentinel/skipped
intervals cannot establish duplicate execution. Jobs and steps do not repeat long workflow commands: join the attempt's
`source_id`, job's `source_job` and step's `source_line` into the retained historical
text. Local scripts/action references remain in that text, revision-linked; their
contents and action-internal behavior are not fetched or separately mapped.

Source matching is intentionally conservative, not a general YAML parser. Exact
static job/step names, job identity and increasing step order are required.
Dynamic matrix job names, unnamed/generated steps and action internals remain
unresolved. Source files are evidence of text at a revision, not evidence that the
workflow engine executed that exact definition:

- `head_sha` is the run API field; `pr_head_sha` exists only when the API supplies
  exactly one associated PR. They need not establish the workflow execution SHA.
- `checkout_sha` is a strict 40-hex line immediately following checkout's identified
  `git log -1 --format=%H` command, within that checkout step's API timestamp window.
  Checkout-only attribution allows the completion second because API boundaries
  have whole-second precision. Multiple distinct SHAs are marked ambiguous.
- `workflow_execution_sha` remains null; the API/log evidence used here does not
  independently prove it. `checkout_source_not_execution_proof` maps source at a
  unique observed checkout SHA; otherwise `head_source_unverified` is explicit.
- Skipped/no-checkout jobs may share an attempt's checkout-source mapping; this is
  a source reference, not proof that the job checked out that revision.

### Log safety and coverage

Only the script contacts the Actions run, attempt, job and job-log endpoints via
`gh api`; workflow contents use the same authenticated API. Requests are serial,
with 100 items/page and at most 100 pages per listing. Each request has a 90-second
wall-clock bound and at most three attempts (only timeout/429/500/502/503/504 retry,
with 1/2-second backoff). Responses are capped at 8 MiB for JSON and 64 MiB per job
log; stderr is capped at 64 KiB; log-line accumulation uses a 64 KiB threshold with
16 KiB read chunks. CLI sample size is
limited to 1–1000 runs. Tables are held in memory and checkpointed after each run;
these bounds are not a promised constant-memory collector for arbitrarily large
histories. No log archive is fetched or retained.

Recent gh versions refuse terminal escapes even when piped. For **only the private
log parser pipe**, the tool passes `--allow-escape-sequences`; parsed numeric values,
not those bytes, reach output. HTTP error bodies are discarded, stderr is reduced
to fixed cause categories, and neither raw logs nor errors are written or printed.
A log 404/410 or size cap becomes explicit missing coverage without blocking API
timings. Other transport errors stop the collection. Overlong lines are discarded
and counted; capped/failed logs contribute no partial metrics.

Allowed metrics: Cargo `Finished` profile seconds, Rust test-result execution
seconds, specific sccache integer counters, and cache restore/miss message counts.
No cache keys, package/test names, arbitrary messages or failure excerpts are saved.
Log attribution uses unambiguous half-open API step windows; second-precision
boundaries can leave metrics unassigned. Cargo/test timings overlap API step time
and each other. Cache counters can be repeated snapshots: do **not** sum them into
a purported global hit rate. Extended analysis takes each counter's maximum per
canonical job, avoiding repeated snapshots without asserting synchronized counters,
monotonicity or a true final hit rate. Registry restore message counts are per job,
not a proof of exact-key hits. Missing logs are not cache misses. API timestamps
have whole-second precision: a line can be temporally assigned to a neighboring
step rather than its semantic producer. Log-derived metrics are secondary evidence,
not exact phase decompositions (nested Cargo durations can exceed step wall time).

### Interpretation

Attempt elapsed time runs from API `run_started_at` to the latest non-skipped job
completion; `api_updated_at` is retained but is not a completion timestamp. Summed
parallel job durations are execution time, not elapsed time or billed cost. The
last-finishing observed job is not a proven dependency-DAG critical path. Start
offsets include dependencies and scheduling, not just runner queue time. Extended
analysis uses historical `needs` edges for gaps from the later of attempt start and
last observed non-skipped dependency completion to job start. Carried dependencies
can establish readiness but do not count again as work. Unknown edges/invalid times
remain unmeasured. Branch tails compare last and second-last non-classifier,
non-aggregator canonical jobs, and are not promised savings. Run
`created_at` to final job completion can additionally include waits between reruns.

Global rankings mix conclusions and workflow versions; use the source/job/platform/
conclusion cohorts for early/recent comparisons and report sample sizes. Prefer
`extended.controlled_cohorts`: successful jobs in successful latest attempts only,
split chronologically within historical workflow content ID, source job, platform
and executed named-step signature (conditional lane proxy). Zero-second executed
steps are included; skipped steps are not. Ordered job IDs and normalized lane
names make every half reproducible. Older top-level cohorts remain exploratory
mixed-attempt summaries. Even controlled cohorts do not control test/workload size,
compiler changes outside workflow text or hidden cache warmth. Retry and
cancelled-run execution totals overlap and are not necessarily avoidable waste.
The sample measures this workflow, not all PR-required workflows or PR-to-green.

Retained bytes are separate from transfer. Manifest `received_bytes` counts gh
stdout consumed by that collection, including HTTP headers; it excludes separate
freeze/slice/diagnostic calls, stderr and HTTP/TLS/redirect overhead. It is not an
exact network-interface measurement. Raw-log transfer can greatly exceed the
compact retained dataset.
