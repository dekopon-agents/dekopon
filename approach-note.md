# Approach: a daily digest wake that runs as the space

Status: revised for coordinator-launched Astra note review round 2. No implementation has started.
Base: `42c4ed85041f55b64351b8b4bd2cf6dbd00a41c7` (`codex/s5-rereview-1`, #839).
Branch: `codex/s6-digest-1`.
Stamped steering: `xrl/agents@7a5ceaf641b3516be8cfe7b2ff0769ad6690f598:steering/GPT.md`.
Its first line is **# Steering for GPT executors**.

## Inputs and conflicts before implementation

The binding inputs are the dispatch's supplied DESIGN sections 1, 5 and 7, decisions 7 and
10, the owner-approved contract table, D3/D4, PLAN line 234, s5's accepted approach and the
coordinator's dotted-repository ruling and binding 2026-10-29 08:35Z s6 ownership ruling.
Campaign-trail is unavailable to this executor; these supplied excerpts are the design input,
not an independently fetched copy.
The fetched s5 branch head exactly matches the stamp. The working tree was clean before
branching from that commit. The stamped steering was already available locally; no fetch
of `/workspace/agents` was needed.

| Conflict at the stamped base | Reading to follow |
| --- | --- |
| `wake.rs:453` creates personal once/watch wakes, while the approved contract table and coordinator's 08:35Z ruling require every newly created mapped-space wake to run as the space. | New `digest`, ordinary `schedule` and ordinary `watch` creation all use s5's freshly authorized asker-intersected-with-space surface and canonical space anchor, persist `run_as: space`, and fire through the one space-only path without a participant leg. Any mapped participant can manage them from the same channel. Legacy once/watch rows lacking the field and new non-space schedule/watch creation stay personal. |
| Strict `Line` at `wake/store.rs:588` has no `run_as`; it does have s5's optional subscription. | Add only optional persisted `run_as`, spelled exactly that, with sole explicit value `space`. Preserve absent-field decoding of once/watch rows and s5 subscription rows. Keep strict common/nested decoding and reject inconsistent new shapes. |
| `Tick::resolve` at `wake/store.rs:205` removes a watch on fire; `take_due` at line 538 leases it by advancing `next_at`. | Reuse `WakeKind::Watch` for recurrence, distinguishing the digest recipe through a reserved gateway-authored header in its existing script text, decoded into a closed internal purpose type. `run_as` chooses ownership only: an ordinary space watch remains a one-shot watch. Keep only digest rows and updated previous output after fire. No new field or line kind. |
| Ordinary watch expiry/failure wakes the model with a reminder or error. | Digest empty/unchanged results rearm quietly; expiry or broken probes retire quietly with a cause on existing records. Ordinary watch lifecycle remains as documented for personal and space ownership; every new space-owned firing still uses only space authority. |
| `resolve_context` at `dekopon-broker/src/lib.rs:2856` admits a direct space only with `Trigger::Wake`, while `Anchor::probe_leg` uses `Trigger::Probe`. | Extend that same guarded space-only resolution branch to the existing Probe trigger. Retain exact subject-to-conversation mapping, attestor namespace and absent participant/GitHub/delivery checks. The existing `probe_permits` read-only restriction remains mandatory. No parallel authority path. |
| `WakesConfig` defaults allow 20 wakes per subject and five-minute intervals. | Apply D4's one-per-space and 24-hour bounds only to digests, on the store's enforcing type. Ordinary space schedule/watch requests keep `maxPerSubject` and existing interval/horizon bounds under the canonical space subject; they neither consume the digest slot nor acquire its daily minimum. A second digest returns a typed refusal before a write. No new config key is needed. |
| `SessionWakes::list/cancel` at `wake.rs:516` match only the original person subject. | On a freshly authorized mapped channel, include every new space-owned digest, schedule and watch using s5's canonical anchor/channel comparison, excluding original asker and thread. Keep legacy personal once/watch entries and new non-space personal entries subject-scoped. A DM has only its own personal entries; another channel cannot list or cancel any of these space wakes. |
| s5's `space_wake` is used as an `is_subscription` flag throughout `session.rs`, including admission labels and quiet reply selection. | Generalize internal wake classification so digest records are identified as digests while reusing the existing space-only session behavior. Use only internal gateway data; keep ChatScopeClaim, context.github and WakeReasonView unchanged. |
| `gateway.rs:1036` suppresses orphan fallback only when `Fired::is_subscription` is true. | Every space-owned digest, schedule and watch suppresses direct orphan notices and revalidates the anchored route/agent with wakes enabled, including after queueing. No space wake bypasses admission by posting its note directly. |
| `docs/gatewayd.md:1554` describes the original person anchor and person-only list/cancel; its authority sentence now singles out subscriptions. `docs/security-model.md:53,63,237` also name subscriptions specifically. | Extend the by-typing sentence and ownership/list/cancel descriptions to every newly space-created digest, schedule and watch. State that legacy personal rows and non-space creation stay personal, and D4 applies only to digests. Update security-model wording only where s5 does not already cover schedule/watch wakes and space probes. Preserve the broker boundary. |
| D3 names GitHub logins; ordinary model reply text can contain Slack mention markup. | The digest instruction uses plain GitHub logins, and digest reply rendering neutralizes Slack user/group/broadcast mention syntax. No login-to-Slack mapping or ping capability is added. |
| Generic steering requests an immediate PR against main and a full body/diff report; repository guidance ordinarily starts from main. | This dispatch explicitly requires a pushed approach note and stop first, then one ready PR stacked on s5. Its 300-word parent report overrides the generic full report. Never rebase or amend pushed commits. |
| Repository guidance permits some plan-authorized release actions. | This dispatch authorizes only the s6 branch and one PR. No main/s5 push, merge, tag, release, deploy or live-system action. The carrying release remains s5's, revert-safe: no. |

The ownership reading follows the coordinator's binding 2026-10-29 08:35Z ruling: every
wake created in a mapped space, regardless of action, is space-owned. DESIGN section 1's
unattended-turn examples are not an exhaustive set. D4 applies only to digests. These are
proposals for implementation, not claims that the behavior exists. The existing frozen GitHub
subject/RepoFullName grammar and s5's dotted-repository refusal are unchanged.

## Creation and stored representation

Extend the existing `wake` tool and `WakeRequest` dispatch with `digest`; do not add a tool,
provider, scheduler, worker, lock or persistence file. Reuse the watch arguments `script`,
`everySeconds`, `forSeconds` and `note`. The model-facing description directs a daily digest
request to this action, with `everySeconds: 86400` and a bounded lifetime. Creation in a DM,
group DM, unmapped channel or turn lacking s5's fresh mapped-space surface returns the existing
typed mapped-channel refusal, with wording expanded beyond subscriptions.

Ownership is selected by the authenticated creation context, not by the tool action. A new
ordinary `schedule` or `watch` in a mapped space also requires s5's fresh asker ∩ space
surface, stores the canonical channel anchor with `run_as: space`, and later fires as
the space alone without a participant leg. Ordinary schedule/watch creation outside a mapped
space, including DMs, stays personal and omits the field. The `digest` action continues to
refuse non-space creation; it does not define the boundary of space ownership.

Digest and ordinary watch creation use the requesting participant's authenticated probe leg,
restricted to read-only and evaluated as asker intersected with space when mapped. Their
baseline must succeed before registration; an ordinary once schedule needs no probe. For all
new mapped-space wakes only the canonical channel anchor is persisted for execution, with no
participant leg or executable person identity retained. The digest creation confirmation
states the expiry and how to stop it.

Represent execution ownership internally as a closed type and encode only `run_as: "space"`
when space-owned. Because the common line uses camelCase, explicitly rename this one field to
`run_as`. An absent field preserves a personal once/watch row. An old subscription without the
field still implies space execution, as s5 already implements; a rewritten subscription may
carry the explicit space value. Unknown values, unknown fields, watch-plus-subscription and a
new space-owned row with a noncanonical/person/DM anchor remain corruption refusals. Existing
valid legacy rows still load unchanged. No new field is added to WatchLine or the subscription.

Decode ownership and lifecycle separately:

| Stored shape | Ownership and lifecycle |
| --- | --- |
| Once/watch without `run_as` | Legacy personal ownership and existing once/watch lifecycle, even if the channel is now mapped. Never infer space ownership or a digest from its script. |
| New non-space once/watch without `run_as` | Personal ownership, original subject filtering and existing bounds/lifecycle. |
| `run_as: space`, once | Canonical space ownership; ordinary single-shot schedule. |
| `run_as: space`, watch without the reserved digest script header | Canonical space ownership; ordinary watch with its existing fire/expiry/failure lifecycle and configured bounds. |
| `run_as: space`, watch with the reserved digest script header | Canonical space ownership; recurring digest with D4 bounds and quiet digest lifecycle. |
| s5 subscription with absent `run_as` or explicit `space` | Existing validated space-only subscription execution and lifecycle; not a legacy personal once/watch. |

`run_as: space` plus Watch therefore does not by itself imply a digest. The digest constructor
prefixes the existing `script` text with the reserved comment `# dekopon: daily-digest` and a
newline; parse that header once into a closed internal ordinary/digest purpose type. Ordinary
watch registration escapes that leading header with a newline, preserving script execution
without allowing ordinary watch text to opt into recurrence or adding an input restriction.
Legacy personal scripts retain their exact interpretation.
The header selects lifecycle and bounds only; canonical ownership/authority validation remains
independent. This uses the existing string field and Watch kind, not another persisted field,
WatchLine member or line kind.

Use the existing `script`, `prev`, `everyMs`, `untilMs` and `nextAtMs`. The mutex, atomic rename
and ID allocator remain the sole store owner. Check digest uniqueness there by canonical space
subject, across anchored threads and agent/transport aliases, before allocating or writing.
Count only digest-purpose watches for `DigestExists`; ordinary space schedules/watches neither
occupy that slot nor receive D4's 24-hour minimum. An expired digest does not block a fresh
explicit request. Validate the digest minimum in the enforcing digest/store construction path
and tool help; ordinary watches keep `minIntervalMs`, and all schedules/watches retain their
applicable `maxPerSubject` and `maxHorizonMs` bounds. Do not duplicate a caller-only bound or add
a second quota mechanism.

## Recurrence, content and quiet behavior

A digest lives for the requested `forSeconds`, capped by `sessions.wakes.maxHorizonMs`:
30 days under defaults. Its first scheduled check is one interval after creation; the immediate
baseline only sets the previous output, as today's watch does. Each lease advances the existing
next check by at least 24 hours from that lease, so delayed checks do not catch up in a burst.
Keep the deadline at the fixed `untilMs`; a terminal deadline retires without a model turn.
Firing does not extend expiry. Only a new authorized mention after expiry or cancellation creates
a new digest; neither the digest nor a background task renews it automatically.

The probe is a bounded model-authored script using the currently granted read-only GitHub
surface. Its recipe prints a deterministic, sorted candidate snapshot, containing stable item
URLs/IDs, titles, the waiting state and GitHub logins. Exclude observation timestamps and changing
age counters from the comparison output. Categories are open non-draft PRs with no activity for
seven days, outstanding review requests with no subsequent answering review by that requested
login, and open issues with no assignee. Clearly attribute an item only when the source identifies
its owner or requested reviewer. Do not invent owners. Repositories come from the request and
granted surface, never an extra token, model-selected principal or broader query authority.
Existing output and script/call limits apply. Failure to read is an error, never an empty snapshot.

Reuse Probe's stdout and previous-output mechanism. For a digest, a successful probe whose
output is empty/whitespace or exactly unchanged cannot produce a model wake, even if it exits
zero. Exit one also declines normally. Update `prev` for completed empty/wait/fire checks, retain
the recurrent row on changed nonempty fire, and keep ordinary watch lifecycle unchanged for
both personal and space ownership. No second findings parser or persisted last-post ledger
is introduced. A failed probe retires the
digest with a structured cause and no orphan/error post. The D4 cadence is an enforced rolling
24-hour schedule bound, not an added crash-durable calendar-day/exactly-once guarantee.

A changed nonempty digest probe starts a normal admitted wake in the original message's
thread. Every newly space-created digest, ordinary schedule and ordinary watch fires through
the same s5 space-only resolution/evaluation path: its existing WakeReasonView has
`scheduledBy: null`, `turn.started_by` is `wake`, and no participant, GitHub claim or original
person's grants enter the turn. Fresh space authority grants its prompt and each effect.
Digest and ordinary space-watch ticks use that resolution with Probe, including the existing
broker read-only check. Disable participant joins for all space-fired turns as s5 does. Legacy
personal wakes and non-space-created wakes retain their original personal execution identity.

Reuse s5's quiet reply mode so the model can inspect the candidates and decline an empty or
unchanged result after read work. The digest instruction asks for one concise threaded answer,
covering what waits on whom, or `decline_chat_reply`. Keep streaming and progress off; a decline
produces no Slack answer, attachment or durable delivered-chat receipt. Bound the digest answer
to a single transport message rather than splitting it into multiple digest posts. Render GitHub
logins as plain text and neutralize native Slack mentions before delivery. Reuse existing trace,
resident-history and journal behavior. A lost route never emits the personal reminder fallback.

## Management and telemetry

Keep s5's `listSubscriptions` and `cancelSubscription` actions and their exact subscription
scope. Extend ordinary `list` and `cancel` to include run-as-space schedules from the current
mapped channel using `space_anchor` and `same_channel`, which include current agent, transport,
workspace and channel, but not original poster or root thread. This includes newly created
ordinary schedule/watch wakes as well as digests. Legacy personal once/watch rows lacking
run_as and new non-space personal entries retain their subject filter and can be managed only
by that person; they are never adopted by other channel participants. A DM lists/cancels only
its own personal wakes and cannot reach any space-created schedule/watch/digest, even by known
ID. Do not substitute a space subject when the current turn lacks fresh space access. Test
negative cancellation using actual space wake IDs, not unknown IDs.

Add digest created/fired/declined/cancelled outcomes as structured attributes on the existing
wake scheduling and gateway.admission records. Include wake ID, execution ownership and bounded
reason where applicable. Distinguish a probe decline from a model reply decline without a new
event name or span kind. Carry a private gateway wake classification through dispatch for prompt
mode and telemetry; it is not a new attestation, broker-protocol field or WakeReasonView variant.
Record only after the corresponding store mutation succeeds. Test captured structured fields,
never formatted-log substring matches. Serving retains best-effort recording.

## Verification after note approval

Reuse `tests/gateway.rs::boot_with`, the mapped Fauxlack setup in `tests/gateway/fauxlack.rs`,
its team manifest, the real broker daemon/provider components, and Audit/CaptureLayer.
Put digest behavior alongside s5's `tests/gateway/subscriptions.rs`, extracting only genuinely
shared fixture setup into the existing harness. Store-shape/lifecycle cases belong beside
`src/tests/wakes.rs`; Slack HTTP write/mention assertions belong beside s5's existing loopback
quiet-reply tests. No second fake broker or authorization stand-in.

Name tests for these invariants and make each assert the behavior that removing its fix breaks:

- A mapped-channel digest creation persists `run_as: space`; its fired broker decision names
  the space, kind space, started-by wake, with no participant/person/asker leg, and replies once
  in the creation thread. Observe a real granted read under the fired leg, not just model prose.
- Ordinary `schedule` and ordinary `watch` created by a mapped-channel participant each
  persist `run_as: space` and the canonical space subject, then fire through the real broker
  as the space with no person/participant leg. Another mapped participant in a different thread
  of that channel lists and cancels each actual wake; another channel and a DM cannot see or
  cancel either known ID. Assert a real granted read on the fired leg.
- Ordinary space schedules/watches retain configured bounds: a watch below 24 hours but above
  `minIntervalMs` is accepted, ordinary wakes coexist with a digest without consuming its
  unique slot, and the ordinary watch still retires on fire rather than recurring.
- Digest creation in a DM is refused and persists nothing. Ordinary DM schedule/watch creation
  stays personal, omits run_as, fires as that person and is manageable only by that person.
- A second digest in the same space, including another thread, receives `DigestExists` before
  persistence; a sub-day digest interval is refused.
- An unchanged probe starts no model turn; an empty day posts no Slack message. Also cover
  a model decline after real read work with zero Slack HTTP writes.
- Another mapped participant in a different thread of the same channel lists and cancels the
  actual digest; another channel and a DM cannot see it or cancel its known ID. A DM still sees
  only its own personal wakes. Exercise the current asker/space authorization gate.
- A changed probe rearms, a subsequent check cannot run within 24 hours, and expiry retires
  without a model wake or renewal. Cancellation during a leased tick cannot resurrect it.
- Files authored before s6, including today's once/watch shapes and s5 subscription lines,
  reopen successfully; inconsistent new ownership/anchor shapes are refused without weakening
  existing strict decoding. Legacy once/watch rows without run_as remain personal after the
  channel becomes mapped and another participant cannot adopt/cancel them; test the original
  owner's listing/cancellation. Include old subscription rows lacking run_as, and distinguish
  an ordinary space watch from a digest after reopening the same file.
- Space probes retain read-only authority and refuse DM/mismatched-space/person shortcuts;
  a removed anchored route produces no orphan chat write. Digest replies cannot ping Slack.

Drive store time through its existing explicit SystemTime arguments and reuse paused Tokio time
where applicable. Through the gateway harness, use controlled existing due timestamps at fixture
setup, including rows produced by actual creation, so no test sleeps a day or lowers the real
24-hour bound. Assert probe and fire broker decisions through the real primitives.

Before the implementation PR, run the dispatch's full s5 gate list with locked dependencies:
fmt; all-target/all-feature clippy with warnings denied; release check of both daemons and
console; machete; both daemon dependency-boundary gates and checker tests; CI's no-exporter
step; rustdoc with warnings denied; built/fetched provider components before workspace
`--lib --bins --tests` and `--doc`; cargo deny; documentation duplicate and exact CI audit-event
gates. Include applicable provider-workspace, SDK/Wasm and shell steps, and the documented OTLP
smoke gate for changed daemon telemetry. Record every unrun gate and its reason individually.
Fresh Astra and Claude head-building/harness-test reviews are coordinator-launched gates.

For this note-only milestone, Rust builds/tests and implementation reviews are deferred until
the note passes. The duplicate-entry gate passed for 50 Markdown files, including this note;
the exact audit-event shell step extracted from CI passed for 45 event names. Both unstaged
and staged whitespace checks passed and are repeated immediately before commit. The available
toolchain is rustc 1.98.1 (48a229cea 2026-09-01); no Rust code has been built in this milestone.
All implementation gates listed above, ARM64 component testing and remote PR checks remain
unrun because this is the required note-only phase. No credential file was written.

## Ship order and stops

The carrying s5/s6 release is **revert-safe: no**. Older strict readers reject these new wake
rows. Rollback requires the coordinator/operator to remove the wakes file, losing all pending
wakes and subscriptions, before starting an older gateway. Stage the space-capable broker before
enabling the gateway, and carry both features in the same coordinated release heartbeat.
This executor neither releases nor deploys. Verify actual consumer versions/artifacts during
implementation reporting; this note makes no assertion about live versions.

Recheck s5's head before implementation and final push. If it moves, inspect the change; clean
unrelated advancement can be merged, never rebased. A change to the space-wake seam stops for a
coordinator re-stamp. Stop for any persisted field/line kind beyond run_as and s5 subscription,
a ChatScopeClaim/context.github/WakeReasonView change, new attestation/wire field, broken legacy
row reading, inherited person grants on firing, or cross-channel/DM management authority.

After pushing this note, end the turn for the coordinator-launched Astra note review. Do not
implement or open the single PR before that passes. Delete this note in the final PR commit.
The later ready PR uses the dispatched title and one physical `Changelog: Added: ...` trailer;
it targets s5 until s5 merges, then merges main and retargets without rewriting history.
The passive deployed admission watch belongs to the coordinator; no qualifying event leaves
it unresolved and this executor does not poll it.

## Round 2

The coordinator-launched Astra note review found that digest-only ownership contradicted the
approved contract table. The binding 2026-10-29 08:35Z ruling applies space ownership to every
new mapped-space digest, schedule and watch. This revision updates the conflict readings,
creation/firing, channel list/cancel, decoding, acceptance cases and by-typing documentation
plan accordingly. Legacy personal rows and non-space creation stay personal; s5 subscriptions
retain their existing space-only behavior. D4 remains digest-only, so ownership no longer acts
as the digest discriminator; use the reserved header in the existing script representation.
Ordinary space-watch bounds and lifecycle remain unchanged. No implementation has started;
a second coordinator-launched Astra note review must pass before building.

## Choices I made

- Use explicit `wake` action `digest` for digest intent, while every new mapped-space digest/schedule/watch uses canonical space ownership, space-only firing and channel-based management under the coordinator's ruling; preserve personal ownership only for legacy personal rows and non-space creation.
- Spell the sole new persisted field `run_as`, overriding common-line camelCase only for that approved contract field.
- Infer old subscription ownership from its existing kind when run_as is absent; write explicit space ownership on new/rewritten space rows.
- Decode ownership independently of purpose; identify recurrent digests by a reserved gateway-authored header in existing Watch script text, parsed into a closed internal purpose type, adding no field, line kind or post ledger.
- Enforce one digest by canonical space subject across threads/agent aliases in the existing store mutation and return typed DigestExists before writing.
- Apply one-per-space and 24-hour D4 limits only to digests; ordinary space schedules/watches keep maxPerSubject and existing interval/horizon bounds, with no new configuration key.
- Keep fixed requested expiry, default-capped at 30 days; only an explicit authorized mention after expiry/cancellation renews it.
- Use seven inactive days for stale open non-draft PRs and a stable sorted candidate snapshot without changing age/observation timestamps.
- Compare digest stdout with stored prev, suppress empty/unchanged output in the enforcing probe path, retain rows after fire, and retire failed/expired digests quietly.
- Extend s5's one guarded space-only resolution branch to the existing read-only Probe trigger rather than borrow a person's leg or create another authority path.
- Extend list/cancel for every new space-created digest/schedule/watch through s5's mapped-channel anchor matching, preserving legacy/non-space personal subject filtering and subscription actions.
- Carry private gateway wake classification for quiet reply and digest attributes without changing any approved broker/wake-reason wire shape.
- Reuse quiet-after-work reply behavior, disable progress/streaming, fit one transport message and neutralize Slack mention syntax for digest replies.
- Reuse the real gateway/broker harness for authority and lifecycle acceptance, existing store helpers for shape tests, and Slack loopback fixtures for actual write assertions.
- Treat the pushed approach note and stop as the entire current delivery milestone; defer the one stacked PR and implementation gates until note approval.
