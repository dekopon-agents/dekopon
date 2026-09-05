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

## Checkpoints, accounting and controls

All engines consume supplied bounded memory checkpoints by default. Version 2 snapshots contain
position/revision/scope/surface, model/effort, portable history/evidence, pending work, spent budgets,
one-attempt flags, skill state and the mandatory token tracker including sequences/report cursor
and terminal flags. Storage uses exclusive live-job leases and compare-and-save receipts; limits
are 128 jobs, 64 MiB total and 2 MiB per snapshot. Active jobs reserve worst-case space and are not
evicted. Capacity failure precedes work. Saves surround dispatch observations, transitions and
terminalization; failed persistence fences old copies and retains live observations.

These are **process-local storage receipts**, not crash durability, broker audit checkpoints,
automatic recovery of binary assets or exactly-once execution. Resume requires matching scope and
fresh surface, preserves consumed limits and refuses unresolved work. A noninitial model selection
requires fresh admission from the configured baseline; stored decisions grant nothing.

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

Ordinary safe boundaries now revalidate the broker's authenticated surface/epoch before inference,
after completion before disclosure, and before tool/capability dispatch. Changed or uncertain
responses fence the checkpoint and retain live observations privately; they do not authorize effects.
Inactive fenced entries can be evicted under the same bounded store policy; restore then fails, never
selects an older snapshot. Model-selected capability identifiers are validated before reservation.
Portable context normalizes tool IDs by host job/call/batch coordinates.

Slack startup refuses two authenticated configurations for the same endpoint/team/bot installation.
Final and cosmetic posts share bounded channel slots; pending finals shed new cosmetics. Only an
explicit HTTP 429 may retry a final post once, honoring a bounded Retry-After; uncertain creations
never retry. Physical platform pacing can delay acceptance. Runner prompt/replay finalizers now live
through output write/flush, distinguishing acceptance, failed/partial writes and unknown flushes.

The recording migration remains incomplete: prior portable tool history and later full context
revisions can still be refused by the legacy history decoder. Independent call accounting now retains
failed/no-answer and image calls; an absent transcript answer never proves zero spend. Broader gateway
fixture migration, parallel tracing isolation and full platform-rate delivery-equivalence coverage
remain validation gaps, not successful claims.
