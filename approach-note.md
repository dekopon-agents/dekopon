# Approach: quiet re-review on push through PR subscriptions

Status: proposal for the coordinator's Astra note review. No implementation has started.
Base: `9089a6e1808cbac4d5ab4fb9969765c0394b2fa3` (typed-events-pr3, #837).
Branch: `codex/s5-rereview-1`.
Stamped steering: `xrl/agents@7a5ceaf641b3516be8cfe7b2ff0769ad6690f598:steering/GPT.md`.
Its first line is **# Steering for GPT executors**.

## Inputs and conflicts before implementation

The dispatch supplies the design's required behavior and stop conditions. The promised
full DESIGN.md §1, §4, §7, contract table and D4, PLAN.md lines 233–234, and
HELPER-RULES.md excerpts have not arrived in this thread. I requested them without
reading campaign-trail. This note is reviewable against the supplied brief; a complete
design-versus-base reconciliation remains pending those excerpts, before implementation.

These are the conflicts found at the stamped base and the readings proposed:

| Base behavior or instruction | Reading to follow |
| --- | --- |
| `crates/dekopon-broker/src/lib.rs:2828` refuses every direct space subject. `evaluate` at line 2191 always evaluates an asker and optionally a space. | The approved unattended-turn rule authorizes a narrowly validated space-only wake. Reuse existing subject, conversation and `Trigger::Wake`; change broker resolution/evaluation internally. A new attestation/wire field would be a stop. |
| `wake.rs:34` stores the original person in `Anchor`; `claim` reuses it. | A PR subscription anchors the authenticated channel/thread with its canonical space subject. Never borrow or retain the poster's executable leg for the fired turn. Ordinary personal wakes keep their existing meaning. |
| `SessionWakes::list/cancel` (`wake.rs:375`, `:379`) and the existing `wake` tool manage only personal wakes, including in DMs. | Add subscription management to the same tool with explicit subscription actions; authorize from a mapped channel under the current asker ∩ space leg. Refuse subscription actions in DMs without removing personal wake actions. |
| `prompt.rs:1100` allows decline only before capability work, and gateway optional replies currently cover follow/continuation turns (`session.rs:2690`). | The quiet re-review contract requires a narrowly selected internal reply mode allowing `decline_chat_reply` after review work on a subscription wake. Ordinary continuation turns retain their current rule. The model judges whether blocking findings changed. |
| `gateway.rs:858` dispatches GitHub deliveries only into repository sessions; `docs/security-model.md:243` says they cannot resume watched chats. | Add the approved subscription branch after the existing GitHub route/ids and broker repository-admission gates. An unmatched delivery keeps current handling. Update only the affected security/gateway descriptions. |
| `wake/store.rs:69` and `:357` support only once/watch records with strict authored fields. | Add a subscription line kind while continuing to read today's exact once/watch shapes. Old binaries rejecting the new shape makes the carrying release `revert-safe: no`. |
| `config.rs:921` already carries `maxPerSubject`. | Per the brief's conditional rule, add sibling subscription bound/expiry configuration, with defaults of 64 per agent and 14 days, rather than independent enforcing constants. |
| `pull_request_links.rs` accepts repository names containing dots; `dekopon_core::github::RepoFullName` in `github.rs:205` excludes dots. | Use the existing typed repository/event/thread vocabulary for subscriptions and matching; do not silently invent a second grammar or broaden typed GitHub facts. This inherited mismatch needs disposition in the note review before claiming all parsed link targets are subscribable. |
| `gateway.rs:935` sends an orphaned timed wake's note directly to chat; subject-filtered routing can also differ between a person and the canonical space subject. | Revalidate a subscription's anchored transport/channel/thread and agent against current routing for the space turn. If it no longer answers, refuse/retire as appropriate with a structured cause; do not apply the personal reminder's direct-post fallback to a quiet subscription. Resolve the route before starting a turn. |
| Generic steering asks for a PR and full report; this dispatch explicitly requires a pushed note and end of turn first. | Push only this approach note, report its head, and stop for the coordinator-launched note review. Open the single implementation PR after that gate passes. The dispatch's 300-word parent report takes precedence over the generic full-diff report. |
| Repository instructions allow releases inside some approved contracts. | This dispatch authorizes only the named feature branches and one PR; no merge, tag, release, deploy, main push or live-system change. |

No change is proposed to `ChatScopeClaim`, `context.github`, or `WakeReasonView`.
The typed-events-pr3 dispatch gate is present at the base. Full design excerpts and the
inherited repository-name mismatch remain unresolved inputs, not implied approvals.

## Store and lifecycle

Extend the existing store, mutex, atomic rewrite and ID allocator. Add
`WakeKind::Subscription` with a typed repository identity, `GithubNumber`, and fixed
expiry. Keep the common persisted anchor and use its canonical space subject, agent,
transport, conversation and reply target. A subscription is unique per
(agent, repository, number, anchored thread); successful repeat reviews in that same
thread reuse it rather than consuming another slot. Distinct anchored threads may
subscribe independently.

Preserve today's `Line` representation for once/watch rows. Proposed new representation:
an optional `subscription` object on the strict common line, mutually exclusive with
`watch`; its presence identifies the new kind. The object carries repository and
number; `nextAtMs` is expiry. Keep `deny_unknown_fields` on common and nested records.
Reject ambiguous shapes. Reading an unchanged old row must remain successful, and
existing unknown-field/corruption refusals must remain refusals.

Enforce per-agent subscription count inside store mutation; the 65th distinct entry
under defaults returns a typed subscription-full refusal before a write. Proposed
sibling config names: `maxSubscriptionsPerAgent` and `subscriptionExpiryMs`, with
defaults 64 and 1209600000. Validate alongside existing wake bounds and report
configuration conflicts together.

Expiry is measured from creation, not extended on every push or re-review. Remove
expired subscriptions during ordinary store scheduling/matching operations; their
expiry contributes to the existing next-deadline loop. Expiry removes a subscription
without waking the model. A routed, admitted PR `closed` delivery removes its matching
subscriptions, covering merged PRs as well. Cancellation removes exactly the
authorized subscription. No extra task, lock or persistence file is needed.

A subscription survives a synchronize fire until close, expiry or cancellation.
The current `Fired` type documents consumption of a timed wake row: distinguish a
persistent subscription fire internally rather than accidentally deleting the
subscription or pretending it is an already-consumed timed row.

## First successful review signal

Use the gateway's existing invocation-observer seam in `FollowInvoker::mark`, which
already observes the broker-selected capability, actual proposal input and terminal
result for both command-word `run` and direct `invoke`.

For a PR-link turn only, observe the first successful `gh.pull-request.comment`
invocation for each structured link target, matching actual owner/repo/number.
Register after confirmed success, before relying on any Slack summary. Do not infer
success from model prose, a requested script or an arbitrary review URL.

The pinned gh fixture is v0.11.1 at source
`f207a05b35a1080165389d9a46af3bb3dbd7a52e`
(`ci/fetch-external-provider-components.sh:99`).
Its `src/reviews.rs:188` submission path pre-reads the PR, pins its head, requires
the expected returned review state and commit, and only then succeeds.
A refusal or uncertain result creates no subscription. This is an observation of
an effect already authorized by the broker, never a new gateway authorization path.

## Webhook to anchored space turn

1. Keep raw-body HMAC verification, bounded enqueue and current admission ownership.
   Match typed `GithubEvent`/`GithubThread::Number`; only `PullRequest` with
   action `synchronize` fires. Use `closed` only for lifecycle removal.
2. First require the delivery's current GitHub route, including its `ids`, and fresh
   broker resolution/admission of its authenticated repository subject. Use the existing
   model-free broker session gate so an unmapped repository cannot reach subscription
   dispatch. Close that admission leg before starting the anchored turn.
3. Match only subscriptions for the routed agent and exact typed repository/PR.
   Unmatched deliveries take the current repository-session path. Mapping/admission
   refusal fires nothing. Recheck current routing when queued work starts.
4. Produce one normal wake admission per matching anchor, through the existing session
   gate/mailbox/concurrency owner, into the original Slack thread. A lost anchored route must not post an orphan reminder. Preserve causal
   linkage from the delivery to the new receipt; do not add another worker or dedupe layer.
   Exactly one fire in a test means one delivered synchronize and one matching subscription,
   not a new crash-durable exactly-once guarantee.
5. Mint the wake's existing chat attestation with the canonical channel subject and
   `Trigger::Wake`, no participant and no GitHub claim. In broker resolution, allow this
   form only when the subject resolves to the same mapped space derived from the attested
   channel/thread and the attestor namespace permits it. Refuse mismatched channel,
   DM, unmapped space and person-subject shortcuts. Evaluate all effects as that space
   alone; no repository or poster grants enter the execution leg.
6. Keep participants disabled for the fired turn. Existing `turn_started_by` already
   yields `wake` from the trigger; assert the principal is the space and no person
   principal/handle appears. Use existing `WakeReasonView::Wake` with no person in
   `scheduledBy`; the internal wake prompt identifies the PR and asks for a fresh,
   head-pinned COMMENT review and a Slack reply only if blocking findings changed.

The two broker legs serve separate purposes: repository admission proves F11 before
dispatch; the resumed space leg grants every operation afresh. Never copy a repository
grant into space authority or attach GitHub claims to a Slack scope.

## Quiet reply and management

The re-review model reads the prior review and current head through its granted
surface, refreshes the GitHub COMMENT review and decides whether blocking findings
changed. Offer the existing zero-argument `decline_chat_reply` for this specific
turn mode after work; a decline suppresses Slack answer, streaming/progress messages,
attachments and durable delivered-chat receipt. Retain the turn trace and bounded
conversation state needed by subsequent reviews. The gateway does not implement a
second findings parser or a text/JSON equality policy.

Extend `wake` with actions `listSubscriptions` and `cancelSubscription` (existing
integer `id`). Filter by current agent, transport, workspace and channel, not the
original poster, so any freshly authorized mapped participant in the channel can
manage the channel's thread subscriptions. Require a fresh mapped asker ∩ space
surface; refuse DM/group-DM, unmapped and unauthorized calls with typed causes.
Ordinary scheduled wakes remain person-scoped.

## Telemetry and docs

Use attributes on existing `gateway.admission` and wake records for subscription
created/fired/ignored/expired/cancelled outcomes, including wake ID, repository, PR and
low-cardinality cause where applicable. Add no span kind or event name. Tests inspect
structured fields, not formatted log strings.

Edit only the affected behavior descriptions in `docs/security-model.md` and
`docs/gatewayd.md`: gated GitHub resumption, space-only wake authority, subscription
lifecycle/config/list/cancel, and the existing person-only “by typing” sentence.
Distinguish ordinary personal wakes from these unattended space turns.

## Validation after note approval

Reuse `tests/gateway.rs`'s real gateway/broker fixture and
`tests/gateway/fauxlack.rs`'s mapped-space setup; add subscription coverage beside
that harness and Slack loopback delivery assertions beside the current PR-link tests.
Use built provider components and signed typed webhook fixtures. Do not use a stub
broker to claim space authority. Reuse `Audit`/`CaptureLayer` structured records.

Name tests for: one matching synchronize firing once into the anchored thread as
space alone; unmapped repository; repository outside route ids; closed removal;
14-day expiry; typed refusal of the 65th subscription; another mapped participant
listing/cancelling from the channel; DM refusal; unchanged findings producing no
Slack writes after real review work; legacy store-line readability and strict shape
refusal; mismatched space-subject refusal. Drive time using existing explicit store
timestamps and paused Tokio time where applicable, without a wall-clock expiry wait.

For this note only, the documented duplicate-entry gate passed (49 Markdown files),
and the exact CI audit-event step passed (45 event names). Run staged
`git diff --check` before commit; the unstaged check also passed. Implementation gates deferred until the note passes:
fmt; clippy all targets/all features with `-D warnings`; release check of both daemons
(and console per CI); machete; both dependency-boundary gates and their checker tests;
the no-exporter step; rustdoc `-D warnings`; provider components; workspace
`--lib --bins --tests` and `--doc`; cargo deny; documentation duplicate and audit gates.
Also run CI's applicable SDK/provider-workspace and shell steps. Record every
unrun gate with its reason in the final PR validation table.

## Ship order and stop

The carrying s5/s6 release is **revert-safe: no**: older gateways reject the new
strict wake-store line. Stage the broker supporting space-only wakes before enabling
the new gateway; ship both as the coordinated release, with s6 built on this branch
at pr-open. Rollback requires the coordinator/operator to remove the wakes file,
losing all pending wakes/subscriptions, before an older gateway starts.
Verify actual consumer versions/artifacts before implementation's final ship report;
this note makes no assertion about a live deployment.

After this note is pushed, stop. The coordinator launches the Astra note review.
Only after it passes, implement; delete this note in the final PR commit; open the
single ready PR with the requested title and one physical Added changelog trailer.
The coordinator launches fresh Astra and Claude head-building/harness-test reviews.
The later passive deployed check belongs to the coordinator and remains unresolved
if no qualifying admission is seen; do not poll it.

Stop if implementation needs a new attestation/wire field, changes
`ChatScopeClaim`, `context.github` or `WakeReasonView`, breaks reading today's wake
rows, fires from an unmapped/out-of-ids repository, or retains a person's grants.

## Choices I made

- Preserve old common JSONL rows; identify a subscription by a strict, optional, mutually exclusive subscription object.
- Make the subscription key include the anchor thread; repeated successful reviews in that thread reuse the subscription.
- Set expiry from creation; pushes and re-reviews do not refresh it.
- Use sibling config defaults of 64 per agent and 14 days because maxPerSubject already exists.
- Observe the successful broker-selected gh.pull-request.comment result in the existing FollowInvoker seam.
- Admit the repository through the existing broker session gate before a separate space-only execution leg.
- Represent space-only authority using the existing canonical channel subject and Trigger::Wake, with exact broker validation.
- Reuse WakeReasonView::Wake with no person in scheduledBy.
- Let the model judge changed blocking findings and select decline_chat_reply under a subscription-specific internal reply mode.
- Extend the wake tool with listSubscriptions and cancelSubscription actions scoped to the current mapped channel.
- Refuse a lost subscription route without the timed reminder's direct-post fallback.
- Place authority tests in the real gateway/broker harness and actual Slack-write assertions in the existing loopback transport tests.
- Treat the push-note-and-stop instruction as the current delivery milestone; defer the single PR and implementation gates to the approved build phase.
