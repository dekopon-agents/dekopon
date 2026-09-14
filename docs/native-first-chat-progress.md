# Native-first chat progress proposal

**Status: Exploration — design proposal, not implemented.** This document proposes a change to
[the current progress design](chat-progress.md). Merging this document does not change runtime
behavior, configuration defaults, Slack installation settings, or a deployed gateway. Code shapes
and configuration below are proposed contracts, not currently accepted syntax.

## Problem and outcome

A Slack Agent session can display both its native Working/Stop UI and a gateway-authored
`Working on it…` message. The latter is an ordinary `chat.postMessage`: Slack can notify the
recipient about the placeholder. Editing it into the answer with `chat.update` avoids transcript
clutter but does not retract the earlier notification or ensure a new answer notification.

The desired default for an enabled Slack Agent liveness experience is native status while working,
then a fresh answer message. Message-based progress remains an explicit option. Actual answer
streaming is independent of whether the operator wants status prose in the transcript.

This is gateway presentation policy, not model prompting or provider execution. It supports the
existing extensible agent runtime without creating a new crate, dependency, authority path, or
model-facing command. It preserves complete traced execution even when less is shown in chat.

## Current implementation and the missing decision

The relevant implementation is entirely inside `crates/dekopond/src/`:

- `transport.rs`: `ChatDriver` exposes optional `NativeStatus`, `TypingLease`, `InboundReaction`,
  `ProgressMessage`, `TextStream`, and `CancelButton` objects. `Status` is Working/Idle.
- `transport/slack.rs`: native status maps to `agents.sessions.setStatus` processing/active.
  A permanent installation refusal disables that capability and exposes the configured reaction
  fallback. Progress posting uses `chat.postMessage`; ordinary text finalization uses `chat.update`.
- `progress/policy.rs`: the session task owns rendering and terminal delivery. Native indicators
  and ordinary progress are additive. Both `streams()` and `writes_progress()` depend on route
  detail not being Off.
- `config.rs`: transport liveness and per-conversation-kind overrides are resolved separately
  from the route's `progressDetail`.

The traits already express what a transport implements. The missing decision is which supported
presentation to use. Do not add a parallel capability bitmap that can disagree with the objects.

## Separate capabilities, choices, and established state

Keep the small capability traits. Native status must not implement `ProgressMessage` or manufacture
an empty `MessageRef`: there is no transcript message to finalize into an answer.

Use gateway-private enums for operator choices, with exhaustive matches:

```rust
// Proposed internal policy types; not new public crate APIs.
enum ProgressPolicy {
    Off,     // no progress prose; ambient indicators remain eligible
    Auto,    // prefer an ambient indicator; message fallback only if none works
    Message, // explicitly opt into transcript-visible progress
}

enum AnswerPolicy {
    Complete,
    Stream,
}
```

The existing master liveness Off setting still disables all intermediate presentation. Progress
Off is narrower: it disables placeholder prose, not native status, typing, reaction, or an explicitly
requested answer stream. Route detail controls prose verbosity, not answer eligibility.

Separate selected intent from successfully established state:

```rust
// Conceptual state shapes; variants carrying a handle must retain their exact session target.
enum ActiveIndicator {
    None,
    Native,
    Typing,
    Reaction,
}

enum MessageSurface {
    None,
    Progress(MessageRef),
    AnswerStream(MessageRef),
}
```

A session may have one ambient indicator and one message surface. Only explicit Message policy
permits progress prose beside a working native indicator. A stream never contains a synthetic
working line. The policy task remains the sole writer and terminal owner; adapters retain API
mapping, target validation, limits, availability, and service-specific error classification.

```text
authorized session + effective settings + available capability objects
                              |
                              v
                  per-session presentation policy
                  /                           \
       one ambient indicator            one message surface
       native > typing > reaction       none | progress | answer stream
                  \                           /
                   terminal delivery and cleanup
                              |
                      transport acceptance
```

## Selection and transitions

Resolve effective transport settings and conversation-kind overrides once, then apply the route's
prose detail. Select only capabilities eligible for this authenticated target and installation.
Availability may change on service refusal; successful establishment, not `Some`, decides whether
the session has an indicator.

| Choice and available surface | While running | Answer delivery |
|---|---|---|
| Auto, Complete, native status accepted | Working/Stop only | Fresh reply |
| Auto, Complete, typing or reaction accepted | One ambient indicator | Fresh reply |
| Auto, Complete, no ambient surface accepted | Delayed progress message if supported | Finalize that message |
| Message, Complete | Explicit editable progress plus selected ambient indicator | Finalize progress |
| Off, Complete | Selected ambient indicator only | Fresh reply |
| Any prose policy, Stream supported | Ambient indicator, then actual model text in a stream | Finalize stream |
| Stream requested but unavailable for the target | Record degradation and use Complete rules | Complete answer |

For Auto + Stream, reserve the one message surface for the answer from session start: do not post
a placeholder while waiting for the first text delta, even if no ambient indicator exists. This
accepts silence before the first delta rather than creating a second message or requiring a native
append-only stream to replace prose. Message + Stream follows the same stream precedence; document
it explicitly rather than introducing a second message.

Complete-mode message creation retains the current delayed event/keep-alive eligibility and
fast-answer suppression. A text delta with Complete selected never exposes partial answer text.
After an ambient indicator definitively fails, Auto can activate the delayed message fallback if
no eligible ambient fallback succeeds and no stream owns the surface. Once a message exists, do
not oscillate back to another message strategy during that session.

### Native refusal and fallback

Have the native adapter return a typed establishment outcome distinguishing an accepted status from
a definitive installation/target unavailability; retain the underlying cause. Transient service
errors, rate limiting, and timeout remain errors, not fabricated permanent unavailability. The
policy must not match Slack strings such as `feature_disabled` or infer Slack experience from the
success of a cosmetic API call. Classic/Agent conversation semantics remain explicitly configured.

On definitive native refusal, the adapter disables the affected capability at its existing scope,
and the policy selects and attempts the configured fallback in the same session. Do not defer that
reaction until the next user message. A failed typing renewal similarly permits bounded fallback
once its breaker opens. Never re-enable a failed rung within the session or introduce retry loops.

A deadline is ambiguous: the service may have accepted the request. Track cleanup obligations for
an attempted durable native status separately from `ActiveIndicator`, so an uncertain Working call
still receives a bounded Idle cleanup attempt. Do not disable the capability transport-wide on a
transient error. Temporary overlap with a fallback after ambiguous acceptance is an accepted limit,
not a claim of exactly-once presentation. Reactions retain the adapter's existing ownership rule:
remove only a reaction that this generation knows it added.

Selection state must not be reconstructed from capability accessors at cleanup time: another
session may have changed installation availability. Retain the target and required cleanup action.
Preserve the current two-second cosmetic deadlines, cooldowns, breakers, edit/keep-alive budgets,
and no repeat message creation after an ambiguous creation deadline. Preserve terminal finalize
ambiguity handling; do not delete a potentially accepted answer after a finalization timeout.

## Completion, cancellation, and notification contract

For native-only Complete sessions, call `ChatDriver::reply` for the answer and then perform bounded
indicator cleanup, including on delivery failure. Native status acceptance is never answer delivery
acceptance. Attachments, partial delivery, conversation history, and durable recording retain their
existing acceptance rules.

Stop remains authenticated through the transport envelope and the initiating subject's existing
atomic terminal decision. Keep native Stop, stop words, budgets, stale-answer suppression, and
cooperative cancellation unchanged. No-reply completion clears indicators without posting anything.
Failure and cancellation retain the existing fixed terminal replies and partial-stream handling;
operator shutdown retains its no-new-message behavior. None of these paths gains broker authority.

On classic transports a Stop button needs a real message. An explicit `cancelButton: true` with
Auto therefore opts into a message-backed control when the delayed surface opens, even if typing
or a reaction works. With progress Off and Complete, reject that configuration rather than silently
hiding the button. With Stream it may attach to the first answer stream render where supported.
Slack Agent mode continues to reject a separate cancel button because Slack owns native Stop.
Stop words remain available before any message-backed button exists.

Do not promise that every platform supports silent messages, nor that users will receive exactly
one notification. Auto with native status avoids submitting placeholder chat messages; a fresh
answer is eligible for normal service notifications. Explicit message progress and streaming may
notify on initial creation and may not notify on final edits. Their trade-off must be documented.

Native Working/Idle is not rich tool progress. Arbitrary phase text or provider-call descriptions
would require a separately demonstrated native API capability, not a new Status string or a fake
message adapter. No such capability is proposed here.

## Proposed configuration and migration

Prefer extending existing configuration over adding a second presentation block:

```yaml
# Proposed syntax; progress: auto is NOT accepted by current releases.
experience: agent
liveness:
  mode: native
  classicFallback: reaction
  progress: auto
  stream: false
```

Map `progress` to ProgressPolicy and the existing `stream` boolean to AnswerPolicy. Keep route
`progressDetail` off/plain/detailed; Off suppresses prose only, while plain/detailed style whichever
message the resolved policy permits. Keep conversation-kind override precedence unchanged.

Proposed release behavior:

1. An absent liveness block remains disabled: do not silently opt previously quiet transports in.
2. Inside enabled liveness, omitted progress becomes Auto. Explicit message/off retain their
   meanings; an existing explicit message configuration must be changed to auto to solve the
   duplicate-native-status experience.
3. Explicit stream remains opt-in. `progressDetail: off` no longer suppresses it. Operators relying
   on the old coupling must set `stream: false` before upgrading.
4. Resolve defaults and overrides into one typed effective configuration. Reject unknown fields
   and all incompatible combinations together, including master Off with active presentation and
   message-backed cancel controls without a surface. Do not add permissive YAML aliases.
5. The implementation PR must update `docs/dekopond.md`, this proposal's status, `chat-progress.md`,
   `docs/upgrading.md`, the gateway README, affected examples, config introspection, and an
   Unreleased changelog entry together. No current operator examples are changed by this proposal.

## Independently verifiable implementation milestones

One gateway owner should implement the coupled policy/state changes. Each boundary needs a fresh
read-only review; do not advance with unresolved material findings or failing required tests.

| Milestone | Dependencies and scope | Acceptance gate | Stop condition |
|---|---|---|---|
| Typed resolution | Existing config parser and driver traits; enums, defaults, overrides, detail/stream independence | Strict parser, aggregate-conflict, effective-config and policy matrix tests | Ambiguous legacy migration or an enum with no runtime consumer |
| Session presentation | First gate passed; selection, establishment outcome, fallback, cleanup and terminal writer | Fake-driver lifecycle tests plus loopback Slack HTTP assertions | Duplicate Auto placeholder, lost cleanup, unbounded fallback, or altered delivery acceptance |
| Release contract | Second gate passed; operator docs/examples and focused cross-transport regression coverage | Package gates, exact-head required CI, fresh review, authorized live smoke before deployment | Mock success used as notification proof, formatting regression, or unresolved cancellation failure |

Required behavior cases for implementation:

- Authorized Slack Auto + Complete, slow first turn and many tool calls: processing is accepted,
  zero placeholder `chat.postMessage` calls, one complete text answer post, then active cleanup.
- Unauthorized input: no progress API calls and no model execution.
- Native permanent refusal: configured reaction appears in that same session; absent/failed
  reaction permits bounded Auto message fallback; future sessions skip permanently disabled native.
- Native timeout and reaction failure: cause is observable, cleanup remains bounded, answer still
  delivers, and no repeated ambiguous message creation occurs.
- Native-only success, failure, declined reply, Stop, shutdown, and failed answer delivery all
  exercise cleanup; concurrent sessions cannot erase one another's cleanup obligations.
- Stop from another subject is ignored; initiating Stop wins once and suppresses stale answer and
  history. Message-free Auto mode must not require a `MessageRef` to cancel.
- Stream with progress/detail Off emits real text without a working placeholder. A stream selected
  but unavailable for the target degrades observably; an ambiguous first-stream deadline follows
  the existing no-second-creation rule.
- Explicit message mode still edits/finalizes one surface; image and oversized-answer fallbacks
  preserve complete/partial transport acceptance semantics.
- Classic button + Auto selects a message-backed control; Off + Complete + button refuses startup.
  Discord/Telegram typing expiry, reaction ownership, local output, and no-edit transports are
  covered by capability-combination and adapter tests.
- Config tests pin absent versus enabled defaults, explicit legacy values, conversation overrides,
  route detail, introspection, and multiple simultaneous conflicts.

Run package-scoped validation first (`cargo test -p dekopond --locked` and relevant gateway
integration tests), then the repository's required scope gates. Loopback HTTP assertions prove API
selection, not Slack notification delivery. A controlled live Slack check must separately inspect
native Working/Stop, absence of placeholder notifications, final-answer rendering/notification under
known client settings, and cancellation. Streaming table-formatting work is a separate prerequisite
for deployments that currently disable streaming for that reason; this proposal does not fix it.

Before builds and between milestones, measure worktree target size and physical free space. After
exact-head validation and with no builds or executables using it, remove only that worktree's
ignored rebuildable target. A design-only PR needs documentation checks and no Cargo target.

## Non-goals and decision requested

No model prompt change, Slack manifest reinstall, new native rich-status API, broker/protocol
change, durability, notification delivery guarantee, automatic operational rollout, or generic
transport plugin framework. No runtime code is added solely to illustrate these types.

Reviewers are asked to accept native-first Auto, independent answer streaming, explicit message
opt-in, same-session bounded fallback, and the migration above as the direction for a subsequent
implementation PR. Until then, the existing configuration and behavior remain authoritative.
