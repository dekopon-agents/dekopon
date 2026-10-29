# Approach: a daily digest wake that runs as the space

Status: proposed for coordinator-launched Astra note review; no implementation has started.
Base: `42c4ed85041f55b64351b8b4bd2cf6dbd00a41c7` on `codex/s5-rereview-1` (#839, open and ready).
Branch: `codex/s6-digest-2`.
Stamped steering: `xrl/agents@7a5ceaf641b3516be8cfe7b2ff0769ad6690f598:steering/GPT.md`.
Its first line is **# Steering for GPT executors**.

## Inputs and conflicts before implementation

The binding inputs are the coordinator-supplied DESIGN sections 1, 5 and 7, decisions
7 and 10, the owner-approved contract table, defaults D3/D4, PLAN line 234, s5's
accepted approach note and its repository-grammar ruling. Campaign-trail is unavailable
to this executor; those supplied excerpts are the design authority. The supplied
helper rules require a generic User-Agent, no personal identifier in requests, stopping
only a process's own PID and deletion of any credential file written. No credential
was read or written during this note work.

The fetched s5 branch and GitHub PR metadata both match the stamped base.
The remote already contained codex/s6-digest-1; preserve it and use the next
authorized branch number, codex/s6-digest-2, rooted at the same stamped base. The
following conflicts and readings precede any implementation:

| Base behavior or instruction | Reading to follow |
| --- | --- |
| Strict `wake/store.rs:588` `Line` has once/watch and s5 subscription shapes, but no execution selector. | Add only an optional persisted `run_as` field, explicitly spelled with the underscore despite the surrounding camelCase convention. Its only authored value is `space`. Preserve old once/watch and s5 subscription rows, including rows without this field; keep strict unknown-field and ambiguous-shape refusal. |
| `SessionWakes::schedule` at `wake.rs:453` stores the asker's original anchor for every timed/watch request. | New schedules created through a freshly authorized mapped-space surface use the canonical space anchor and `run_as: space`. Creation, including the initial probe, remains asker intersected with space; later execution retains no executable person leg. Unmapped chats and DMs retain personal wakes. |
| `Tick::resolve` at `wake/store.rs:205` removes a watch after Fire, Failed or a final Wait; `take_due` leases ordinary watches. | A space-run Watch is the bounded recurring digest-on-change representation. Keep it after a changed successful check, advance its existing `prev`, and retain the existing lease/deadline owner. Ordinary legacy/personal watches still retire on fire. Quietly retire a space watch at its fixed horizon, without a final empty or failure post. |
| s5's `resolve_context` guard at `dekopon-broker/src/lib.rs:2856` accepts a direct space subject only with `Trigger::Wake`; the existing probe helper uses `Trigger::Probe`. | Extend that same validated space-only branch to Probe, keeping exact-channel, namespace, no participant, no delivery and no GitHub-claim requirements. Existing `probe_permits` keeps every probe read-only before policy. Do not add a second authority path. |
| `SessionWakes::list/cancel` at `wake.rs:516` and store operations at `wake/store.rs:495` match the person's subject. | From a freshly authorized mapped participant, select space schedules by the anchor's transport, agent, workspace and channel container, ignoring thread and original asker. A different channel or DM cannot reach them. Retain ownership of legacy personal rows; a mapped-channel turn may manage only its own legacy personal rows anchored in that channel. DM/unmapped personal management stays person-scoped. |
| `WakeBounds` at `config.rs:956` has general wake and subscription limits but no digest bounds. | Put the fixed one-per-space and minimum-24-hour bounds on the enforcing digest schedule/store type, rather than introduce config keys. Preserve `maxPerSubject`, `minIntervalMs` and `maxHorizonMs` as additional ceilings. Return typed refusals for a second live digest or an interval below the effective minimum. |
| s5's `session.rs:576` and subscription-named local variables treat every space wake as a PR subscription; `dekopon-agent/src/prompt.rs:635` tells every quiet review turn to refresh a PR review. | Generalize the existing quiet mode's common instruction to unattended space work, retaining task-specific instructions in the subscription/digest wake prompt. Carry a typed origin through the crate-private wake admission metadata for telemetry. Keep WakeReasonView, ChatScopeClaim and all broker wire frames unchanged. |
| `gateway.rs:1036` suppresses the direct orphan reminder only for subscriptions. | Suppress it for every space-run wake and revalidate the current route/agent/wakes opt-in, including queued work. A space digest never posts outside a freshly authorized model turn. |
| Decision 7 permits tagging a clear owner, while D3 limits the first version's tagging. | Render GitHub logins as text, without Slack mention syntax or a login-to-person lookup. |
| s5 already rewrote the person-only authority sentence in `docs/gatewayd.md`, while `docs/security-model.md:63` names PR subscriptions specifically. | Extend the existing sentences narrowly to space schedule wakes and read-only probes; do not add another security description or undo s5's wording. |
| Generic steering requests a main-based PR and full report now; the dispatch requires a stacked note and stop first. | Push only this approach note on the stamped s5 base and end the turn. Open the one ready implementation PR against s5 only after note approval and implementation gates. The dispatch's 300-word parent report takes precedence over the generic full-diff report. |
| Repository guidance permits releases within some approved execution contracts and normally starts work from main. | This dispatch authorizes only s6 feature branches and one PR stacked on s5. No main/s5 push, merge, tag, release, deployment or live-system change is authorized. Never rebase or amend pushed commits. |

The mapped-space Watch reading above is deliberate: without another persisted marker,
new space watches are the daily recurring form. Their minimum interval and uniqueness
bound also apply when a model directly asks for a watch in a mapped channel. Old rows
without `run_as` remain personal and keep their old lifecycle. Space Once schedules
remain single-shot and do not consume the digest slot. No new line kind is proposed.

## Creation and the model-facing tool

Extend the existing `wake` tool rather than create a scheduler or digest service.
Keep action spellings `schedule`, `watch`, `list` and `cancel`; preserve s5's
`listSubscriptions` and `cancelSubscription` actions.

A daily-digest request uses `watch`, its existing `script`, `everySeconds`,
`forSeconds` and `note` arguments, and an optional model-facing request
`runAs: "space"`. The gateway chooses space execution automatically for new
mapped-channel schedules; an explicit space request in a DM, group DM or unmapped
channel returns the existing typed `MappedChannelRequired` refusal. The selector is
a request to a gateway meta tool, not a new broker wire/attestation field and never
an authority assertion. Personal execution is not a selectable bypass in a mapped
channel. Tool guidance requires an explicit space request for a digest so that a DM
cannot silently become a personal daily watch.

The current person leg must already expose the broker's mapped-space surface,
which means the normal admission/capability gate evaluated asker intersected with
space. Reuse s5's `with_space_access`/`space_anchor` seam. The initial read-only
baseline runs under the authenticated asker's probe leg intersected with space.
Only after a successful baseline is the canonical space anchor stored. The anchor
retains the transport-derived root thread/reply target; the model never supplies a
subject, conversation, channel or thread.

Use a default 24-hour interval, subject to a larger configured `minIntervalMs`.
The model supplies the existing watch duration, capped by `maxHorizonMs`; tool
guidance chooses that horizon for a daily digest. The default configured horizon is
30 days. The first check is one full interval after creation; creation records a
baseline and posts no digest. A configured horizon shorter than the required
interval refuses creation through typed horizon/interval errors.

## Storage, recurrence and the daily bound

Add a crate-internal typed execution selector to the wake/store path. Persist only
`run_as: "space"` on new space rows. No `digest`, last-post-day, originator,
renewal, recurrence discriminator or new nested object/line kind is added.

Decoding an absent selector means the existing behavior: a once/watch row is personal;
an s5 subscription row is space-run, as proved by s5's existing canonical-space
validation. New space rows require the same canonical subject/conversation validation.
Keep `deny_unknown_fields`, refuse invalid selector values and retain the existing
watch/subscription mutual exclusion. Do not rewrite an old person's row into a space
wake merely because its channel is now mapped.

A space-run Watch is a digest. Under the existing store mutex, refuse a second live
space Watch with a dedicated typed `DigestAlreadyScheduled` cause before writing.
The uniqueness key is the canonical conversation space subject (service, workspace,
channel), independent of thread, agent or transport alias: two routes must not create
two daily digests for the same channel. PR subscriptions and space Once reminders do
not consume this slot. Drop expired digest rows during registration/scheduling so an
expired row does not block a new explicit request.

Validate the interval inside the enforcing type against both 24 hours and the
configured minimum. Registration's first deadline and each lease's next deadline are
at least that interval away. This supplies a rolling 24-hour bound, which also gives
at most one digest in any UTC day, without a second persisted timestamp. Cancellation
and a new explicit request start a new baseline and wait a full interval again.

Reuse the current `take_due` atomic rewrite before the probe starts. On a successful
space check, update the existing `watch.prev` for both Wait and Fire. An unchanged
successful output or empty successful output is a decline and starts no model turn,
even if the script returns exit 0. A changed nonempty Fire produces one existing wake
admission and retains the recurring row. An explicit Wait produces no admission.
Failed probes retire the digest with structured cause attributes on existing records
and no orphan/failure chat post. At the horizon, remove it quietly before probing.

The digest lives only until the existing `untilMs` deadline: 30 days with default
guidance/config, less if requested or configured. Successful checks, firings and
declines do not renew it. After expiry, another explicit channel request creates a
new baseline and horizon; while it is live a repeat request gets the typed duplicate
refusal and can use list/cancel to replace it. No background renewal task is added.

## Probe content and the quiet digest

The model composes a bounded read-only script over repositories it can read through
the existing granted gh surfaces. The pinned component is gh v0.11.1 at
`f207a05b35a1080165389d9a46af3bb3dbd7a52e`; its command/schema/source support is
checked before implementation chooses exact query flags. No arbitrary GitHub API
escape hatch, provider upgrade or new capability is proposed.

Use these defaults in the model-facing instructions:

- Stale PRs are open, non-draft PRs with no update for seven days.
- Outstanding review requests identify requested GitHub reviewers whose review is
  still awaited, checked against current review state rather than guessed from prose.
- Unowned issues are open issues with no assignee.
- Emit a stable, sorted projection of repository/item number, URL, category, title,
  head/review state and relevant GitHub logins. Omit clocks, result ordering and other
  incidental volatile fields from the comparison projection. Empty results emit an
  empty output. Use the existing bounded output ceiling and propagate read failures.

The gateway compares the bounded probe output with the existing previous output; it
does not add a GitHub findings parser or another equality store. The probe script and
note are model text, not authority. The fired prompt asks the model to verify current
items through granted reads, produce at most one summary in the anchored thread,
use GitHub logins without Slack pings, and call `decline_chat_reply` for an empty
or unchanged day. Reuse s5's existing space-wake quiet mode, generalizing its shared
PR-specific system sentence to unattended space work; each subscription/digest wake prompt
supplies the actual task. Preserve suppression of progress, streaming, attachments
and delivered-chat receipts on decline.
`WakeReasonView::Wake` continues to use `scheduledBy: null`.

Every later probe and firing gets a new space-only broker leg. Extend s5's one
validated resolution path to existing Trigger::Probe while keeping its read-only
ceiling; firing uses Trigger::Wake unchanged. Broker decisions must name the space,
have no person principal/participant/asker leg and report wake/probe origin rather
than person origin. The scheduling person's grants are never retained or copied.

## Management and telemetry

Extend current list/cancel selection using s5's canonical anchor/channel seam.
A freshly authorized mapped participant in another thread of the same channel can
list/cancel its space schedules. The original asker and anchored thread are not
authorization keys. Require exact agent, transport, workspace and channel for
management; the uniqueness bound is intentionally wider across agent/transport
aliases. A different channel or DM sees no space schedule and receives NotFound
when cancelling its ID. A DM continues to list/cancel only its own personal wakes.
s5's subscription-specific actions keep their existing behavior.

Add digest created, fired, declined and cancelled attributes, wake ID, run-as mode
and low-cardinality causes to existing `gateway.wake.scheduled`, wake lifecycle,
`gateway.admission` and existing terminal admission/session records as appropriate.
Do not add an audit event name or span kind. Emit persisted lifecycle success only
after successful writes. Reuse structured `Audit`/`CaptureLayer` assertions.
A digest must not inherit s5's `pr-subscription` telemetry label merely because both
are space wakes. Carry a small typed origin from Fired through the existing
crate-private MessageId::Wake metadata, which has no serialization or wire contract;
it grants nothing and does not enter the attestation or WakeReasonView. Do not infer
origin from a prompt string or require a later store lookup after cancellation.

Edit only `docs/gatewayd.md`'s affected wake descriptions and
`docs/security-model.md`'s subscription-specific authority sentences where they
need to cover schedule/probe execution. State the fixed horizon and explicit renewal,
daily bound, run_as compatibility, channel management and space-only authority.

## Validation after note approval

Reuse the real gateway/broker harness in `tests/gateway.rs`, its
`tests/gateway/fauxlack.rs` mapped-space setup and s5's neighboring subscription
helpers. Add a digest suite there, not a second fake broker or a public production
API solely to drive time. Use explicit store timestamps in gateway primitive tests,
paused Tokio time where compatible, and due JSONL fixtures written before fixture
boot for real scheduler/probe/firing runs. Never wait a wall-clock day.
Use built pinned provider components and a loopback GitHub peer to exercise reads.

Name tests for the invariants they observe:

- A digest created through an authorized mapped-channel model turn stores
  `run_as: space`; its later real broker decision names the space, no person, no
  participant and `turn.started_by = wake`, and it posts once in the anchor thread.
- A digest requested in a DM is refused with MappedChannelRequired and persists nothing.
- A second digest in the same space, including another thread/agent route, gets the
  typed DigestAlreadyScheduled cause and leaves exactly the first row.
- A space digest interval below 24 hours is refused; a successful fire remains stored
  but cannot be leased again inside the next 24 hours.
- An unchanged probe starts no model turn, and an empty day produces zero chat writes;
  separately exercise a real quiet model decline after granted reads.
- Another mapped participant lists/cancels from a different thread in the same channel;
  another channel and a DM see neither the row nor cancellation authority.
- A pre-change once/watch file and s5 subscription file without run_as both load;
  subscription behavior and strict malformed/unknown-field refusal remain pinned.
- A space probe uses only the space and cannot write; wrong channel, DM and participant
  shortcuts remain refused by the same broker resolution guard.
- Expiry, cancellation during a leased tick, and a lost route never emit an empty,
  stale orphan or duplicate digest.

For every new test, record the mutation that would make it fail. Reuse the neighboring
helpers before writing setup. Assert structured telemetry and typed causes, not
formatted error/log strings. Actual Slack loopback assertions cover the thread target,
one final digest post and zero progress/streaming requests on decline; fauxlack
covers authenticated multi-participant/container authority through the real broker.

Run the dispatch's full s5 gate set after implementation: fmt; clippy workspace/all
targets/all features with -D warnings; release check of brokerd/gatewayd/console;
machete; both daemon dependency boundaries and checker tests; the no-exporter
telemetry step and HTTP-host/agent check; rustdoc -D warnings; pinned provider
components and validation; SDK Wasm bindings; all separate in-tree provider-workspace
fmt/clippy/native-test/Wasm checks; CI ShellCheck; workspace --no-run then
--lib --bins --tests and --doc; cargo deny; documentation duplicates and the exact CI
audit-event step. Use locked Cargo commands. Name every unrun gate and its reason
in the implementation PR's validation table. No Rust build/test is appropriate for
this approach-note-only commit.

For this note only, the documentation duplicate-entry gate passed (49 Markdown
files), and the exact audit-event script extracted from CI passed (45 event names).
Working-tree and staged diff checks passed. Every Rust/provider/shell/remote-CI gate listed above is deferred
until the note passes, because this commit changes only this proposal. Coordinator
Astra note review and both implementation reviews have not run.

## Ship order and review stop

The carrying release remains s5's coordinated release, **revert-safe: no**.
The broker accepting validated space probes and wakes must precede the gateway that
uses them; ship s5 and s6 together in one coordinator-owned heartbeat. Older strict
gateways reject run_as/subscription rows. Rollback requires the coordinator/operator
to remove the wakes file, losing all pending wakes/subscriptions, before starting
the older gateway. No deployed versions, artifacts or live data were inspected here;
the coordinator verifies the actual deployed consumer inventory before shipping.

If s5 moves in a way that changes the space-wake seam, report and wait for a new
coordinator stamp. Otherwise merge its new head into this branch; never rebase pushed
commits. After s5 merges, merge main and retarget the single PR to main.

After this note is pushed, end the turn for the coordinator's fresh Astra note review.
Only build after it passes; delete this note in the implementation PR's final commit.
The later PR is ready for review, titled
`feat(gateway): a daily digest wake that runs as the space`, with one physical
`Changelog: Added: Mapped channels can request a daily threaded digest that runs as the space and list or cancel it from the channel.`
trailer. The final PR body ends with Choices I made, one meaningful choice per line.

The coordinator launches fresh Astra and Claude reviews before merge, each building
the head and running digest tests through the gateway harness. No self-review is
claimed. The coordinator's post-deploy watch stays passive until a live Slack space;
a qualifying gateway.admission joined to a broker.decision at the carrying version
must have no person principal. None observed leaves it unresolved; do not poll it.

Stop for a new persisted field/line kind beyond run_as and s5 subscription, any
ChatScopeClaim/context.github/WakeReasonView change, new wire/attestation field,
legacy-row decoding break, person authority on a digest fire, cross-channel/DM
management access, or a changed unstamped s5 authority seam.

## Choices I made

- Read the contract-table run_as rule as applying to new mapped-space schedules; preserve old personal rows without silently converting them.
- Use the existing Watch kind for recurring digest-on-change behavior, with run_as plus Watch as its typed discriminator and no extra persisted marker.
- Treat new mapped-space watches as the daily recurring form, including direct watch requests, so D4 cannot be bypassed by alternate tool spelling.
- Keep watch/list/cancel action spellings and add only an optional model-facing runAs space request so digest intent is refused in DMs.
- Persist the field exactly as run_as despite Line's camelCase default; absent legacy subscription selectors infer s5's existing space execution.
- Use a rolling minimum 24-hour interval, one live space watch across agents/transport aliases, and the existing maxHorizonMs lifetime with explicit re-request after expiry.
- Put fixed digest bounds on the enforcing internal type/store instead of adding config keys.
- Run the baseline under asker intersected with space, then use canonical-space-only probe and wake legs through s5's single resolution path.
- Decline unchanged/empty probe outputs before inference, persist prev after successful checks, and retain the row after a changed fire until expiry/cancel.
- Retire failed or expired digests quietly and extend s5's no-orphan-post rule to all space schedules.
- Define stale as seven days without a PR update; project stable waiting items and GitHub logins while excluding incidental clocks/order.
- Reuse s5's quiet space-turn mode with a generic shared instruction and task-specific wake prompts, and WakeReasonView with no scheduling person.
- Scope channel management by the canonical container rather than asker/thread, preserving ownership and channel confinement for legacy personal rows.
- Carry typed digest/subscription origin in existing crate-private wake message metadata for telemetry, without a serialized, attestation or broker wire field.
- Place authority/creation/effect tests in the real gateway harness, store-time checks beside existing wake primitives, and Slack-write checks in the loopback transport harness.
- Treat the note push and coordinator review as this turn's milestone; defer implementation, the one PR and Rust gates until note approval.
- Preserve the pre-existing remote s6-digest-1 branch and publish this note from s6-digest-2 at the same stamped base.
