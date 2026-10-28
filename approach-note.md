# Turns, jobs, wake ticks and memory generation on ShellThreads

Status: approach only; no implementation or Rust build before the coordinator-launched Astra note review answers.

Steering: `xrl/agents` at `7a5ceaf641b3516be8cfe7b2ff0769ad6690f598`, first line: “# Steering for GPT executors”.
Base: `dekopon-agents/dekopon` main at `2e0a398620c6ceaf77b0b0cd3353d1d9fa66c0fc`.
Branch: `codex/turns-off-blocking-1`.

## Dispatch checks

Fetched origin with tags, checked out the exact base, then created the branch. All five prerequisite merge commits are ancestors of the base:

| Prerequisite | Merge commit |
|---|---|
| ShellThreads #803 | `6514c913be305fd5914ecedccc46a92ecdaf6da0` |
| Memory #802 | `00982c7791d29d2742d004c481219993fcebb112` |
| Memory #805 | `48a07b5cb06355c10cd32c38f1ba035a03b99185` |
| Memory #806 | `b3751aa5c6b571ac3eadfad90da6bae01f21dc97` |
| Runtime-sizing #814 | `46d54e8f7d36ffde50c607cc3ff178c67a2c552d` |

The base also descends from `v0.64.0`. Read the pinned steering, AGENTS.md, the constitution, the development/change maps and validation, CONTRIBUTING.md, the thread audit, PLAN §11-lean/threads Decisions 2–3, SEQUENCE row 37 and D7, and campaign-trail `tools/cloud-env/HELPER-RULES.md`.

## Conflicts at the base and the reading I will follow

1. The brief's `memory/generation.rs` `prepare` site no longer exists. D15 moved the model-holding operation to `memory/engine.rs:266`, `run` calling `summarize`. Migrate that operation; leave `generation.rs:119,201` and the engine's short overflow/panic journal writes on Tokio's blocking pool.
2. The brief's `SHELL_THREADS` constant no longer exists. `lib.rs:332` constructs the shared budget from the flag; `cli.rs:23–29` defaults it to 32. Preserve that value and share that exact instance with every new holder and existing shell stage.
3. `ThreadPermit::spawn` in `dekopon-process/src/threads.rs:91` only accepts a `Scope`. Add the explicitly authorized owned-thread method on the same type, retaining the scoped method and its callers. The brief's explicit API instruction takes precedence over the steering's general restriction on new public API; add no other public types or alternative spawner.
4. `JobRefusal` in `dekopon-shell/src/job.rs:9` has no thread-limit variant. Add a variant carrying `LimitExceeded`, used for the permitted `LimitExceeded::ShellThreads` refusal. Preserve existing refusal text and precedence for disabled/full jobs, and preserve the successful job id/notice contract.
5. `TaskFailure` in `session.rs:4156` only converts Tokio `JoinError`. Keep both variants, categories and existing conversion; map native panic payloads into the same `Panicked { cause }`. Admission and OS spawn errors remain distinct from join cancellation and panic.
6. The referenced existing “panic/cancel test” is only `a_panicking_session_task_surfaces_as_panicked_with_its_cause` at `tests/asset_resolve.rs:475`; there is no `TaskFailure::Cancelled` assertion. Preserve that test and add the cancellation assertion and coverage of the new native join.
7. `gateway.rs:564` detaches ticks after the serving grace, and `lib.rs:382` aborts resident async owners, including the summary dispatcher. These mechanisms cannot own native handles safely. Keep cancellation of the callers, but retain a separate gateway join owner until every admitted native thread has been joined. Retain tick results through shutdown so a completed `Fired` still starts its wake.
8. Gatewayd's blocking default is 512 (`main.rs:136–138`), its help says 512, and `docs/gatewayd.md:39–48` still describes the transitional Pi `B=16` requirement. Set gatewayd to 8, update help and the ceiling, and remove the obsolete transition. Brokerd and console retain their defaults. Update the affected `docs/inference.md` execution sentences and gateway shutdown paragraph too; their blocking-pool ownership claims would become stale.

No dispatch precondition fails, and this approach requires no wire, WIT, configuration-key or script-result change beyond the authorized typed thread-limit refusal.

## Spawn API and native join ownership

Add `ThreadPermit::spawn_owned(name: ThreadName, run: impl FnOnce() -> T + Send + 'static)`. It returns an `io::Result` containing an async native-join operation with output `std::thread::Result<T>`; use an opaque future rather than a new exported handle type. It uses the existing 2 MiB stack size and `thread::Builder`, moves the existing semaphore permit into the native closure, and enters its Tokio runtime exactly as scoped `spawn` does. A completion guard wakes the async join on normal return and unwind, after the workload and runtime/permit guards have been dropped. The join operation awaits completion, then joins the native handle and returns its value or panic payload. It never waits for the workload in `spawn_blocking`.

A crate-private gateway owner holds a `JoinSet` of these join operations. A caller receives a `oneshot` result after its native join; dropping or aborting that caller drops only the receiver. The join operation stays owned and is never aborted. This is also the detached job's new owner. Preserve each site's subscriber and span explicitly across the native boundary.

The join owner's bookkeeping uses the gateway's existing `parking_lot` dependency, never a lock held across an await or a native join. It reaps completed receipts during normal operation and before admission. Bound outstanding join operations and reservations to `S`, from the same flag value used to construct `ShellThreads`; this bounds completed-but-unreaped receipts as well as live work. Reserve an owner slot and one ShellThreads permit before spawning; a failed reservation drops both, returns typed `LimitExceeded::ShellThreads { maximum: S }`, and schedules no work. There is no waiting admission queue, new resident OS thread, new setting or dependency. This second lifetime bound exists because the ShellThreads permit ends with the native closure while the owner must retain the handle until joined.

Put the result receiver into the existing session/tick/summary flow. Keep a job's join observation in the owner, including `Panicked`, rather than dropping a task handle. Typed internal startup errors distinguish limit refusal from `io::ErrorKind`; do not relabel OS thread-creation failures as cancellation or a reached limit. An already-admitted job whose OS spawn fails takes its existing abandoned-job failure/notice path and records the errno once, without introducing another script-visible refusal shape.

## Each migrated site

All references below are relative to `crates/dekopon-gatewayd/src/` at the stamped base. Native handle ownership is the gateway join owner above; existing owners still own delivery and cancellation.

| Site and native name | Existing bound and delivery owner | Refusal and shutdown |
|---|---|---|
| Turn, `session.rs:2819`, `sh-turn` | `SessionGate`, the session task and its cancellation/watch/mailbox; one additional shared thread permit | A typed admission failure reaches the existing session-failure terminal and trace, without a model request. Native join errors retain `TaskFailure` categories. Aborting a session triggers its existing `CancellationOnDrop`; the join owner continues to join its native thread. |
| Job, `jobs.rs:778`, `sh-job` | `Jobs` maximum, row and cancel signal; the new join owner observes termination | Integrate thread admission before inserting a running job row, after the existing off/full checks. A thread-limit refusal returns `JobRefusal` carrying the typed limit, with no row, notice or held job/thread permit. Keep `Started::run`'s normal/panic completion and notice behavior. `Jobs::seal` cancels jobs; the join owner joins them. |
| Tick, `lib.rs:921`, `sh-tick` | Existing `admit_probe` shares SessionGate's permits; `ticks` retains result delivery | Return the typed refusal internally and emit exactly one `gateway_wake_tick_skipped` record, with `reason = shell-threads`, numeric maximum and wake id. Release the probe admission. `take_due` has already advanced the watch lease, so refusal leaves it eligible at its next scheduled tick; do not manufacture a fired wake. Admitted tick results remain drained at shutdown, including starting completed fires. |
| Summary, `memory/engine.rs:266`, `sh-memory` | Existing bounded summary queue, concurrency and dispatcher cancellation watch | The dispatcher uses the same process budget. Refuse before calling `summarize`, emit one `gateway_memory_summary_refused` record with record/part and typed limit fields, return `Next::Stop`, and hold no permit. No model call, invented summary, new persistent error variant or retry queue. The untouched queued journal slot remains restart-eligible, as the existing recovery path defines. Dispatcher abortion still sets its cancellation watch; the join owner outlives it and joins the native summarizer. |

Use `ThreadName::new` constants for all four names (7, 6, 7 and 9 bytes respectively). Preserve model request bytes, tree/call budgets, cancel signals, job notices, span parenting, wake verdict resolution and admitted summary output.

## Shutdown order

1. Keep the serving grace for draining admitted ticks/sessions and the current early `Jobs::seal`. Retain tick result delivery instead of `detach_all`; a tick that has retired its row must still deliver its fire. After the grace, request existing cooperative cancellation for sessions, retaining native join ownership independently of their result waiters.
2. Stop the summary dispatcher and other resident tasks through the current shutdown path, which drops the dispatcher's cancellation guard. Finish tick fires/session admission before sealing the native join owner; do not close it while a required shutdown wake still needs to start.
3. Seal the native owner, then await every join operation while the Tokio runtime and assets remain available. Drain join failures, including job `Panicked`, before closing assets and returning to runtime teardown. Neither `JoinSet::shutdown` nor dropped caller receivers may discard native handles.

The serving grace remains a serving deadline; native cleanup must finish its joins afterward. The existing five-second runtime shutdown timeout then applies to residual short blocking work. Update the old documentation that relied on runtime teardown to own long blocking holders. No new shutdown timeout or configuration key is proposed.

## Permits per turn and the Pi's 32

A turn now holds **one** permit throughout the model loop. An N-stage plain pipeline adds **N−1** producer threads because its last stage runs on the turn thread (`dekopon-shell/src/interp.rs:1527–1595`), so the total is **N**. The default 32 MiB retained-value budget and 2 MiB producer stacks admit at most sixteen producers before other retained charges, yielding a turn-plus-pipeline peak of **17 permits** for that case.

That is not an absolute peak for arbitrary scripts: each streamed provider call adds one call thread and, with piped input, one feeder; jq pumps and child upcalls also share the budget. The absolute admitted process total, including these helpers and all long holders, stays 32. Nested or simultaneous wide pipelines can refuse; do not promise four maximum-width pipelines fit.

The reference concurrency is four sessions/probes combined (ticks use the same gate), two jobs and one summary, hence at most **7 long holders**, leaving **25 permits** for stages and helpers. One sixteen-producer pipeline alongside those holders uses **23**, leaving 9 for other helpers. This supports the Pi's normal small chat workload without increasing its cap; D7's passive measurement, not this arithmetic, verifies the real workload.

Gatewayd's ceiling remains `W + 4 + B + S`, with `B=8` by default. The Pi settings `W=2, B=8, S=32` give 6 idle and at most **46** threads with both exporters. No additional native joiner thread changes this formula.

## Acceptance and planned evidence

Tests are planned here, not claimed to pass. Reuse `runner`, `runner_with_jobs`, `stub_broker`, the wake fixtures, the engine's `Script`/`Factory` and `held_listener`, process `threads()`, and `dekopon_test_support::CaptureLayer`; no second fake per seam.

| Invariant/test | Required observation and regression it detects |
|---|---|
| `a_turn_at_the_shell_thread_cap_is_refused_without_holding_a_permit` | Assert the typed ShellThreads admission error, failed session delivery, zero model requests and full reacquisition after releasing the deliberately held permits. Fails if turn admission is bypassed or leaks. |
| `a_job_at_the_shell_thread_cap_is_refused_without_a_row_or_permit` | Assert the nested typed JobRefusal, unchanged rows/notices and job slots, and full thread-budget recovery. Fails if job admission happens after row insertion/spawn. |
| `a_tick_at_the_shell_thread_cap_is_refused_without_holding_admission` | Assert the internal typed limit, one structured skipped record, no probe, released gate and thread permits, and preserved next watch scheduling. Fails if a refused tick runs, fires or leaks admission. |
| `a_summary_at_the_shell_thread_cap_is_refused_before_calling_the_model` | Assert one structured refusal with its limit/record/part, no model request, unchanged queued journal slot and full thread-budget recovery. Fails if summarization starts or silently queues/retries. |
| `a_panicking_job_returns_its_thread_permit_and_its_owner_observes_panicked` | Exercise the actual job/native-owner path; assert the owner's typed `Panicked`, existing failed job/one notice, full permit recovery and no outstanding joins. Fails if a panic detaches, is misclassified or leaks. |
| Existing session panic test; new cancelled-join and native-panic cases | Keep the existing panic cause/category assertion; use an actually aborted Tokio task for `Cancelled`, and the new native join for `Panicked`. Fails if either conversion loses its category/cause. |
| `shutdown_joins_admitted_threads_even_after_their_callers_are_cancelled` | Use controlled work and completion barriers for all four sites, abort result waiters, then assert shutdown observes every exit and drains the native owner; retain the existing sleeping-job and summary cancellation regressions. Fails if shutdown drops a native handle or its cleanup still needs a stopped runtime. |
| Owned-thread name/runtime test; short-I/O progress test | Real `ThreadName`, native thread and runtime timer; prove short `spawn_blocking` work completes while long holders are deliberately parked with a small blocking cap. Fails if runtime entry is omitted or long joins/work return to the blocking pool. |

After note approval, run `git diff --check` first; targeted process, shell and gateway tests with `--locked`; then applicable docs/development.md and AGENTS.md gates: workspace fmt, clippy with `-D warnings`, release checks, dependency boundaries, machete, SDK Wasm check, workspace tests/doctests and fixtures, rustdoc with `RUSTDOCFLAGS=-D warnings`, cargo deny, release-metadata checks, documentation duplicates and CI's audit-event extraction. Run the documented OTLP smoke check for changed daemon telemetry where the environment permits it. Use the pinned toolchain, and report every unavailable/failed gate and exact tested head. No WIT/SDK/component change is intended; separate provider-workspace rebuild gates apply only if that scope changes. Native ARM64 checks and D7 are coordinator/CI evidence, not claims from this checkout.

For this note-only handoff, run the documentation gates and diff check, commit/push only this file, and stop. Open the one ready PR only after implementation gates pass, using the required title and validation table. Preserve pushed history; delete this note in the final implementation commit. That commit alone carries `Changelog: Changed: Gateway turns, jobs, wake checks and memory summaries share the shell thread budget; the gateway blocking-thread default is 8.` No CHANGELOG.md edit.

## Note-only handoff validation

- Whitespace: `git diff --no-index --check /dev/null approach-note.md` passed; the staged diff is checked again before commit.
- Documentation duplicates: `python3 .github/scripts/check_docs_duplicates.py docs README.md AGENTS.md crates/*/README.md` passed over 49 files.
- Audit documentation: CI's extraction found 45 audit names. The first shell invocation exited 1 without a missing-event diagnostic; rerunning explicitly under `/bin/bash` with task-specific variable names passed (exit 0, all 45 documented). No source/event changes are in this note.
- Rust formatting/builds/tests, clippy, rustdoc, cargo deny, dependency/release gates and OTLP smoke: not run in the note phase; implementation is gated on the coordinator's Astra response.
- Acceptance 1's behavioral tests and Acceptance 3's implementation/Changelog commit remain planned above. Acceptance 4's D7 measurement remains the coordinator's post-deploy check. No PR has been opened in this phase.

## Choices I made

- Follow D15's actual `memory/engine.rs` summarizer rather than restoring the obsolete `generation.rs` prepare path.
- Extend `ThreadPermit` with `spawn_owned` and an opaque async join result; preserve the scoped API and avoid additional public types.
- Give all four native thread kinds one gateway-owned JoinSet; result receivers may be cancelled independently, while native join operations are retained.
- Bound ownership receipts/reservations separately to the same flag-derived S, because native permit lifetime alone does not bound completed join receipts.
- Reserve both ownership and thread capacity before job-row insertion; preserve existing job off/full precedence and notices.
- Map OS spawn failures separately from limit refusal; an already-admitted failed job uses its existing failure/notice path.
- Retain TaskFailure's two variants and existing Tokio conversion, add native panic conversion and missing cancellation coverage.
- Use sh-turn, sh-job, sh-tick and sh-memory as ThreadName constants.
- Record a refused tick through the existing skipped event and leave its advanced watch lease intact.
- Record a refused summary once, stop that dispatch without a model request, and retain its queued journal slot for existing restart recovery.
- Read the serving grace as ending delivery's grace, with native joins still mandatory during cleanup; keep the runtime available through every native join and tick fire.
- Change only gatewayd's blocking default to 8; preserve its shell default of 32 and the other binaries' defaults.
- Update the adjacent execution/shutdown documentation as well as the numeric ceiling because their blocking-pool ownership claims would otherwise be stale.
- Budget approximately +300/−60 production lines and +200 test lines, versus the plan's +60/−40; cancellation-safe ownership, finite receipt retention and the requested site-level regressions account for the increase. These are estimates; record measured production/test totals in the final PR body.
- Make the approach note a documentation-only commit without a Changelog trailer; reserve the single requested Changed line for the implementation commit that deletes this note.
