# Harness sessions (Unreleased)

An **agent** is configured identity, instructions, skills and proposal surface. The
**harness** is its unprivileged runtime. `dekopon-harness` replaces `dekopon-agent` outright;
there is no compatibility crate, alias or `run_prompt_session` facade. This is post-0.12.0 work,
not a published crate or permission to bootstrap publication.

## Ownership and API

`SessionEngine::new(model, runtime).run(bootstrap, &mut history)` is the concrete driver.
`SessionBootstrap` supplies host context, instructions, services and optional controls/activity.
`ScriptRuntime` and `ShellRuntime` adapt direct read-only execution or the identity-free broker
protocol client. `ContextPolicy`, `CheckpointStore` and `ModelRegistry` are narrow replaceable
seams, not a framework-wide agent trait. Runner, replay and gateway consume the engine.

The gateway retains authenticated ingress, routing coordinates, admission, cached clients, Stop,
reply delivery and transport receipts. It supplies harness-owned conversation storage and retains
`JobAccounting` through delivery. Only broker/policy code authorizes provider effects or admits
model/effort transitions; neither Cedar nor provider credentials enter the harness or gateway.
The direct runner remains import-free and read-only, including live replay divergence.

## Request one and portable evidence

Before inference, one scoped snapshot supplies sorted capability IDs, descriptions, complete
input schemas and filtered command words. The same source backs inspection and fallback
`cap --list`/`cap --describe`. Duplicate IDs, more than 256 capabilities or more than 128 KiB of
encoded metadata refuse before inference. Schemas are never truncated. There is no discovery
model request, synthetic tool-result bootstrap or global catalog disclosure. Metadata remains
untrusted reference material, not permission.

`JobRecord` separates the user message, generated answer, delivery disposition and execution
observations. Accepted delivery retains the exact accepted text separately. A generated answer
is not proof of delivery. Whole assistant tool batches and correlated results form `ToolGroup`s;
incomplete groups are labelled summaries, not orphan tool messages or invented successes.
Actual dispatch observations cover nested calls in one script, including a lone broker leg.
Builtins and provider-rendered help are not capability execution.

`ExecutionRecord` includes job/call/tool/sequence linkage, direct-read-only, broker-observed or
recorded-replay provenance, invocation/evidence references, outcome and a bounded UTF-8 excerpt
with original byte length, digest and truncation. Denied, not-executed, succeeded, failed and
unknown are distinct. A failed operation can have effects. Stop or a later inference failure does
not erase earlier observations. Unknown effects fence further dispatch and resume; no automatic
rerun or exactly-once effect claim exists.

Retained history defaults to 12 jobs/64 KiB, independently of the 1 MiB model-context and 512 KiB
whole-group ceilings; execution excerpts are at most 4 KiB. Retention can discard old bounded
records and is not an audit archive. Pending-work warnings survive selection. Reasoning, binary
attachments, generated PNGs and opaque provider continuation are excluded from history/checkpoints.
Trimming or switching discards incompatible continuation and repairs skill/inspection repeat
pointers so missing text can be read again.

`BoundedConversationStore` keys agent/route/transport/channel/conversation/sender from routing
only, bounds entries and total bytes, and appends through random generation leases rather than
writing back cloned windows. Refusal, idle/capacity eviction or changed full metadata/startup epoch
fences late appends and rotates cache keys, including A→B→A. `oneShot` imports no previous jobs.
These comparisons do not cache authorization; every provider invocation still reaches the broker.

Seeding a session touches the conversation, so the idle timeout runs from the last *message*, and
the conversation a session is answering is never that session's own eviction victim. A conversation
a *concurrent* session is still answering under is passed over too: the store records the
outstanding generation, and eviction takes a conversation nobody is answering first, so one
session's arrival does not rotate another's cache key and turn its delivered answer into a refused
append. Only when every candidate has a session in flight does the least recently touched go
anyway — the ceiling is a bound before it is a courtesy — and a generation that was never committed
stops protecting its conversation once it is older than the idle timeout. Both ceilings are read from running byte totals maintained where turns are appended and
dropped — `History::bytes()` and the store's own total are O(1) — so neither a lookup nor an
eviction step ever encodes the retained corpus. `HistoryLimits::MAX_TURNS` and
`HistoryLimits::MAX_BYTES` are the hard clamps any configured window is reduced to, published so a
reader reconstructing a recorded history can report what the clamp dropped.

## Checkpoints, accounting and controls

All engines consume supplied bounded memory checkpoints by default. Version 2 snapshots contain
position/revision/scope/surface, model/effort, portable history/evidence, pending work, spent budgets,
one-attempt flags, skill state and the mandatory token tracker including sequences/report cursor
and terminal flags. Storage uses exclusive live-job leases and compare-and-save receipts; limits
are `MAX_JOBS` = 128 jobs, 2 MiB per snapshot, and a store ceiling of exactly
`MAX_JOBS * MAX_CHECKPOINT_BYTES` = 256 MiB so the two agree. Active jobs reserve worst-case space
and are not evicted, which makes `MAX_JOBS` a **concurrency ceiling**: every live session holds one
lease, so `dekopond` refuses a `sessions.maxConcurrent` above it at startup rather than converting
the surplus into capacity refusals under load. A store already holding `MAX_JOBS` leases refuses
the next one before evicting anything, so a refusal never destroys the snapshots the other
in-flight messages still need. Capacity failure precedes work, and the refusal names the ceiling.
Each stored snapshot's encoded size is measured once by the save that stored it, so eviction reads
cached sizes rather than re-encoding every stored checkpoint on every step. A mutation encodes the
snapshot exactly once as well: the model-facing group ceiling, the per-checkpoint byte ceiling and
the save all share that single measurement, because a mutation runs several times per tool call and
holds the live lock while it does. Saves surround dispatch
observations, transitions and terminalization; failed persistence fences old copies and retains
live observations.

These are **process-local storage receipts**, not crash durability, broker audit checkpoints,
automatic recovery of binary assets or exactly-once execution. Resume requires matching scope and
fresh surface, preserves consumed limits and refuses unresolved work. A noninitial model selection
requires fresh admission from the configured baseline; stored decisions grant nothing.
**No shipped binary resumes a checkpoint today.** Neither `dekopond` nor `dekopon-run` calls the
resume path: a checkpoint is what a failing session leaves behind for inspection and what fences a
retry, not a queue something drains. `SessionBootstrap::with_resume` is crate-visible for that
reason, and the engine's own tests are what exercise the path.

`TokenTracker` owns logical chat/image calls and bounded inference-attempt observations across
model segments and resume. `ChatModel`/`ImageGenerator` require `AttemptRecorder`: reserve before
each inference transmission and observe usage before decoding content. The single explicit-401
subscription retry is a second inference attempt; authentication refresh is not. Missing or invalid
usage is unknown, never zero; cached input and reasoning output are subsets, never additive spend.
Observed usage survives cancellation, tool errors, failed persistence and failed delivery.
`JobAccounting` finalizes once with the host disposition; last-owner drop records unknown delivery
after workers settle. Hard process exit can leave an unterminated job. See the strict
[event/field contract](observability.md#accounting), including aggregation levels and UI deltas.
No prices or subscription-dollar estimates are inferred.

Optional `select_model` and `set_effort` tools select only configured clients. A control must be its
sole tool in a batch; mixed batches execute nothing. Local refusals and broker denials consume the
maximum four attempts per job. Fresh broker `agent.prompt` plus each changed-dimension Cedar action
is required. Safe application checkpoints, preserves budgets/evidence, rotates cache/continuation
and rebuilds portable context. Missing controls disables the tools, including in direct/replay
runners with a provider broker leg. See [gateway configuration](dekopond.md#configured-model-and-effort-controls-unreleased)
and [lockstep migration](upgrading.md#0120--next-unreleased--core-controls-and-v1alpha3).

## Cosmetic activity and current limitations

The runtime activity seam reports actual submissions with bounded public configured labels, never
arguments, private titles, results or reasoning. Submission is not execution evidence. Slack may
opt into one owned ordinary progress post plus its existing native status/reaction; Discord and
Telegram use their existing expiring typing actions, local is a no-op and WhatsApp has no activity.
Progress is coalesced/bounded, separate from history/checkpoints/receipts, and holds no execution
or final-delivery I/O lock. Physical channel post pacing can delay acceptance.
Posts may notify or remain platform-retained after failed removal. See
[activity lifecycle and platform bounds](dekopond.md#structured-activity-and-slack-progress-unreleased).

Freshness is a disclosure gate, not an authorization one. It runs at exactly two places in a turn:
before each model request, and after a completion and before any of it is disclosed. It does not run
per capability invocation, because the broker authorizes every `invoke` at dispatch against its live
policy and epoch — a client-side refetch immediately in front of one decides nothing the broker is
not about to decide, and cost a full round trip per capability a script drove. It does not run per
provider command word either, for a different reason: `runCommand` is deliberately ungated and
grants nothing, so there is no authorization for a freshness check to anticipate — the broker runs
the declaring component's import-free argv handling and authorizes only the proposal that comes
back, on the `invoke` path. A failed check names which part of the surface moved: the epoch, the
descriptions, the effective views, the command words, or the chat-memory surface. The comparison is
five per-component digests taken once when the session's broker leg is built, so a check costs the
round trip and nothing else. Changed or uncertain
responses fence the checkpoint and retain live observations privately; they do not authorize effects.
Inactive fenced entries can be evicted under the same bounded store policy; restore then fails, never
selects an older snapshot. Model-selected capability identifiers are validated before reservation.
Portable context normalizes tool IDs by host job/call/batch coordinates.

Slack startup refuses two authenticated configurations for the same endpoint/team/bot installation.
Final and cosmetic posts share bounded channel slots; pending finals shed new cosmetics. Only an
explicit HTTP 429 may retry a final post once, honoring a bounded Retry-After; uncertain creations
never retry. Physical platform pacing can delay acceptance. Runner prompt/replay finalizers now live
through output write/flush, distinguishing acceptance, failed/partial writes and unknown flushes.

Recording reconstruction preserves portable tool history and ordered full/delta context revisions,
including model-switch rebuilds and byte-free attachment summaries between correlated tool results,
independently of answered turns and call accounting. Replay seeds
the initial bounded portable context without executing remembered calls or restoring opaque state.
Failed/no-answer and image calls remain accounted; an absent answer never proves zero spend.
Replay comparisons count independent chat calls, including failed inference, rather than answers;
image calls contribute spend but not chat-turn counts. Historical files without independent call
accounting retain their transcript-turn fallback.
Focused regressions cover portable reconstruction, parallel accounting capture, gateway safe-yield
fixtures, real broker restart/revocation during inference, terminal receipts and tiny-window Stop.
A loopback Slack fixture enforces one accepted post per channel per second with progress on/off,
including concurrent finals and bounded rate-limit refusal. This does not certify live-platform
availability or zero quota latency; workspace/MSRV and release gates remain separate requirements.
