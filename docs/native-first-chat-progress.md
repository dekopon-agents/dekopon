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
or an explicitly requested message-backed Stop control permits progress prose beside a working
ambient indicator. A stream never contains a synthetic working line. Proposed presentation and
terminal ownership stays with the policy task; readers acknowledge interactions before queueing,
but must not mutate shared text or controls before cancellation authorization. Discord currently
violates that separation (see below). Adapters retain API mapping, target validation, limits,
availability, and service-specific error classification.

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
the session has an indicator. Distinguish accepted, unavailable, and skipped/cooldown outcomes:
Discord's current `TypingLease::renew` can return `Ok(())` without sending, which cannot establish
or renew visible state. A skipped call honors the bounded cooldown; it is neither success nor a
rapid-retry trigger.

| Choice and available surface | While running | Answer delivery |
|---|---|---|
| Auto, Complete, native status accepted | Working/Stop only | Fresh reply |
| Auto, Complete, typing or reaction accepted | One ambient indicator | Fresh reply |
| Auto, Complete, no ambient surface accepted | Delayed progress if supported and detail permits | Finalize if created; otherwise fresh reply |
| Message, Complete, editable surface available | Explicit delayed prose if detail permits, plus ambient indicator | Finalize if created; otherwise fresh reply |
| Message, Complete, editable surface absent | Record degradation; ambient indicator only | Fresh reply |
| Off, Complete | Selected ambient indicator only | Fresh reply |
| Any prose policy, Stream supported | Ambient indicator, then actual model text in a stream | Finalize stream |
| Stream supported by transport but unavailable for target | Record degradation and use Complete rules | Complete answer |
| Stream or button unsupported by transport (WhatsApp) | Aggregate startup refusal, including overrides | No session |

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
not a claim of exactly-once presentation. Remove only a reaction this generation knows it added.
This is a proposed eligibility requirement, not existing Telegram/Discord ownership: their set/remove
APIs and `InboundReaction` implementations do not distinguish a preexisting bot reaction. Select
reaction fallback only when safe ownership and cleanup can be established; otherwise record the
missing eligibility and try the next permitted surface. Do not infer ownership from HTTP success
or clear another generation's reaction. Preserve Slack's existing ownership rule.

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
cooperative cancellation unchanged. Seal before terminal delivery and cease typing renewals;
`TypingLease` has no clear operation. No-reply completion attempts owned durable-indicator cleanup
without posting anything; typing may linger until service expiry. Failure and cancellation retain
the existing fixed terminal replies and partial-stream handling;
operator shutdown retains its no-new-message behavior. None of these paths gains broker authority.

On classic transports a Stop button needs a real message. An explicit `cancelButton: true` with
Auto therefore opts into a message-backed control when the delayed surface opens, even if typing
or a reaction works. With progress Off and Complete, reject that configuration rather than silently
hiding the button. With Stream it may attach to the first answer stream render where supported.
Slack Agent mode continues to reject a separate cancel button because Slack owns native Stop.
Stop words remain available before any message-backed button exists. Also reject Complete plus a
message-backed button when effective route detail is Off, including Auto and Message: do not
silently override Off or invent a control-only message. Validate these effective route/transport
combinations together. If target-level Stream degradation would leave a requested button without
an eligible Complete surface, report that degradation too; stop words remain, not a hidden post.

Do not promise that every platform supports silent messages, nor that users will receive exactly
one notification. Auto with native status avoids submitting placeholder chat messages; a fresh
answer is eligible for normal service notifications. Explicit message progress and streaming may
notify on initial creation and may not notify on final edits. Their trade-off must be documented.

Native Working/Idle is not rich tool progress. Arbitrary phase text or provider-call descriptions
would require a separately demonstrated native API capability, not a new Status string or a fake
message adapter. No such capability is proposed here.

## Telegram, Discord, and WhatsApp styles

**Exploration, not implemented.** These are applications of the same policy, not three new
configuration dialects. `classicFallback` remains Slack-specific. None of these drivers exposes
`NativeStatus` or native Working/Stop; native typing is not editable prose, rich tool status, or
answer streaming. All choices below assume enabled liveness; absent/master Off stays reply-only.
Current `config.rs::ProgressSurface` accepts only Off/Message (default Off), stream/button default
false, and `policy.rs::{streams,writes_progress}` still couples both message modes to detail.

### Telegram

**Current evidence:** [`transport/telegram.rs`](../crates/dekopond/src/transport/telegram.rs),
`ChatDriver`, `TypingLease::renew`, `InboundReaction::set`, `post_text`, `edit_text`,
`finalize_in_place`, and `reply_markup` implement these surfaces:

| Surface | Current adapter contract |
|---|---|
| Ambient | `sendChatAction(action=typing)`, four-second renewal; fixed 👀 via `setMessageReaction`, empty reaction list on removal |
| Editable prose | `sendMessage` / `editMessageText` / `deleteMessage`; 4,096 UTF-16 units, three-second edit floor; unchanged-message refusal counts as success |
| Answer stream | Cumulative replacement of one message, not native append; 4,000-scalar policy ceiling plus the UTF-16 wire bound |
| Stop | Inline keyboard with conversation-bound callback; `answerCallbackQuery` attempted before queueing, without removing the keyboard; successful terminal edit sends an empty keyboard |

**Proposed style:** Auto + Complete selects typing, then safely owned reaction if typing's breaker
opens, then delayed editable prose only if no ambient rung works and detail permits. Healthy typing
means no placeholder and a fresh complete answer. Off + Complete uses the same ambient selection
but never prose. Message + Complete explicitly allows delayed working/tool/keep-alive prose beside
the selected indicator. Each of Auto/Off/Message + Stream instead reserves the message for the
first nonempty actual answer text, even with detail Off; no synthetic line precedes it. Apply the
shared button-forcing/refusal rules, including no button before the first stream render.

Retain authenticated chat/topic coordinates: `pressed` and inbound routing accept a positive
`message_thread_id` only with `is_topic_message`. New messages/photos/actions carry that topic;
edits and cleanup use the retained reference, never a reconstructed or invented topic. Only the
initiating subject may cancel. Current callback acknowledgment shares cosmetic cooldown and can
fail or be suppressed; queueing cancellation must still proceed, not claim acknowledgment receipt.

Preserve the three-second coalescing and two-second cosmetic deadline. The repository records a
five-second typing expiry, not a live visibility measurement; latency and serial cosmetic calls
can consume the one-second renewal margin. Terminal cleanup stops renewals and removes only an
owned reaction. Fitting text finalizes in place with controls removed; oversize/images explicitly
refuse in-place finalization and use ordinary split text/photo delivery, with partial acceptance
preserved. Telegram's UTF-16-only stream cut can currently be unmarked; visibly marking every cut
is an implementation acceptance requirement, not a claim that the scalar headroom solves it.

### Discord

**Current evidence:** [`transport/discord.rs`](../crates/dekopond/src/transport/discord.rs),
`ChatDriver`, `TypingLease::renew`, `InboundReaction::set`, `liveness_body`, `TextStream::show`,
`finalize_in_place`, and `CancelButton::ack` own the mappings:

| Surface | Current adapter contract |
|---|---|
| Ambient | POST channel `/typing`, eight-second renewal; fixed 🍊 PUT/DELETE on inbound message reaction `/@me`, without generation ownership |
| Editable prose | POST/PATCH/DELETE one reply-referenced message; 2,000 UTF-16 units and two-second edit floor |
| Answer stream | Cumulative PATCH of actual text; 1,900-scalar policy ceiling, then UTF-16 splitting; final single-message text removes components |
| Stop | Danger-style component carrying `stop:<conversation-key>`; reader acknowledges before queueing even when the eventual subject check rejects the press |

**Proposed style:** The same six Telegram combinations apply: Auto/Complete prefers typing then
eligible owned reaction then delayed prose; Off/Complete forbids prose; Message/Complete opts in;
all three Stream combinations show actual answer text only. Auto + button permits a delayed
message even while typing works; Off/Complete + button and Complete/detail-Off + button refuse.
There is no native Stop to substitute. DM, channel, and thread use the same policy; the current
reader retains `parent:thread` as the thread conversation key and the thread channel as REST
destination. Use that source contract, not conflicting parent/thread wording in older prose.

**Current exception and proposed gate:** `CancelButton::ack` uses a type-7 update to replace content
with `Stopping…` and clear components *before* subject authorization. Thus a foreign press cannot
cancel but can erase displayed partial text/controls today. It bypasses cosmetic cooldown, uses the
interaction token without bot authorization, and has a two-second deadline. Preserve fast reader
acknowledgment before a potentially blocked queue, not the destructive side effect: the proposal
requires non-mutating pre-authorization acknowledgment, followed by session-owned authorized
mutation under the atomic terminal decision. Demonstrating that acknowledgment mapping and a
foreign-press partial-stream-preservation test is a gate before shipping the proposed button handling.
Acknowledgment failure must still route cancellation; it grants no cancellation authority.

The repository records a ten-second typing lease; the eight-second cadence does not prove continuous
visibility or client dismissal. Preserve cooldowns, breakers, and two-second coalescing, with skipped
typing explicitly distinguished from acceptance. Stop renewal on every terminal path. Fitting text
finalizes with empty components; images/oversize use ordinary lossless split delivery and its
partial-delivery classification. `shown_text` marks scalar cuts, but `one_message` can cut again at
the UTF-16 boundary and lose that marker: test visible truncation for astral text, not only ASCII.

### WhatsApp Cloud

**Current evidence:** [`transport/whatsapp.rs`](../crates/dekopond/src/transport/whatsapp.rs),
`TypingLease::renew`, `ChatDriver`, and tests `typing_is_the_read_receipt_and_the_indicator_in_one_call`,
`a_running_session_re_posts_the_same_indicator_request`, and `whatsapp_offers_typing_and_nothing_else`.
Only typing is exposed: one messages-endpoint request couples `status: read`, the inbound message ID,
and `typing_indicator: {type: text}`, requiring HTTP success and JSON `success: true`. There is no
implemented reaction, native status, editable prose, stream, or button. `config.rs` rejects stream
and cancelButton in base settings and overrides, but accepts Message as a no-op.

**Proposed style:** Auto/Complete and Off/Complete attempt that coupled typing/read request, then
stay silent until a fresh complete answer; exhausted typing cannot fall back to a nonexistent
surface. Progress Off does not suppress the gateway's read-receipt request; master Off does.
Message/Complete remains accepted for compatibility, but reports missing editable-surface degradation
once and behaves ambient-only. Never manufacture append-only working/keep-alive posts or dummy
references. Auto/Off/Message + Stream and any cancelButton remain aggregate startup refusals, not
runtime Complete degradation. Authenticated initiating-subject stop words remain available.

Retain the twenty-second renewal cadence and bounded failures. Source records Meta's twenty-five-
second/reply-arrival dismissal but explicitly leaves renewal unverified: the repeat-request test
proves neither scheduling nor renewed client visibility. Seal and stop renewals on answer, silence,
failure, cancellation, delivery failure, and shutdown; cleanup cannot retract the read receipt or
send Idle. Silence/shutdown submits no cleanup message; fixed failure/stopped replies remain.
Complete delivery is text-only, split at 4,096 scalars with no blind send retry and explicit partial
delivery. The repository records service-window restrictions and no template fallback; neither
read/typing activity extending that window nor API-wide edit/interactive impossibility is established
by this review. The implemented capability absence, not such universal claims, justifies rejection.

### Notifications and retained surfaces

**Current requests, not observed client behavior:** Telegram `post_text`/`send_text` omit
`disable_notification`. Discord `liveness_body` and ordinary replies suppress parsed/user/role/reply
mentions but send no silent-notification flag; mention suppression is not notification suppression.
WhatsApp implements no silent-progress send. None exposes a notification-policy setting.

**Proposed:** retain those request defaults; no silent-send feature is added here. Telegram's
`disable_notification` is a candidate API option, not a verified silent-delivery contract; Discord
silent-flag support and WhatsApp silent-send support likewise remain externally unverified by these
bounded source reports. API request support, service acceptance, and observed push/sound behavior
are distinct. Omitting placeholders reduces submitted messages, not necessarily notifications;
final edits may not notify, and a long complete answer may require multiple sends. Reaction
eligibility, recipient settings, typing dismissal, and actual notification behavior require
separately authorized live checks, not loopback assertions.

Preserve `policy.rs::discard`: ordinary progress can be deleted on fallback, but partial streams
are retained even on Telegram/Discord, despite their message-delete APIs. Success removes controls
when finalization succeeds; failure/cancellation preserve existing partial-text terminal treatment.
Shutdown does not rewrite the last surface. Failed or ambiguous cleanup can leave controls or text
behind, and finalization timeout must never delete a possibly accepted answer. This proposal adds
no durable reconciliation or universal immediate-clear guarantee.

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
| Session presentation | First gate passed; selection, establishment outcome, fallback, cleanup and terminal writer | Fake-driver lifecycle tests plus Slack/Telegram/Discord/WhatsApp loopback assertions below | Duplicate Auto placeholder, unsafe reaction ownership or acknowledgment mutation, lost cleanup, unbounded fallback, or altered delivery acceptance |
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

Additional transport acceptance cases for the implementation PR (not tests added or run here):

| Test boundary | Required observable result |
|---|---|
| Telegram/Discord policy matrix | All six progress/answer combinations, all detail levels, absent/enabled defaults and per-kind overrides; healthy Auto/Complete sends typing but zero reaction/placeholder requests; Stream sends only cumulative model text |
| Fallback and ownership | Paused-time typing errors/cooldowns open only bounded fallback; skipped calls never establish a lease; unsafe/preexisting reaction is neither selected nor removed; owned cleanup cannot erase another generation |
| Telegram addressing and limits | Topic/non-topic actions, photos, edits and Stop keys stay exact; spoofed/nonpositive topic refused; unchanged edits succeed; astral text cuts visibly and never exceeds UTF-16 bounds |
| Discord addressing and Stop | DM/channel/thread REST targets and cancellation keys stay exact; non-mutating ack occurs before blocked queue; foreign press preserves partial text/components; initiating press wins once; ack failure still routes cancellation |
| Message/control resolution | Auto/button forces delayed prose only where permitted; Off/Complete/button and Auto/Message + Complete/detail-Off/button report all conflicts; successful terminal edits remove keyboard/components; Stream attaches only at first text |
| WhatsApp restrictions and lease | Exact coupled read/typing body; master Off sends neither; Message degrades once with zero placeholder calls; all Stream/button conflicts in base/overrides aggregate; paused time schedules twenty-second renewals without claiming remote visibility |
| Terminal and failure paths | Success, failed model, declined reply, cancellation, shutdown, failed final send and partial delivery cease renewal; retained streams versus deletable progress follow existing rules; stale answer/history suppressed after Stop |
| Refusal and ambiguity | HTTP refusal, malformed success and timeout retain cause; no repeated ambiguous creation, no deletion after ambiguous finalization; oversized/image fallback preserves every supported part and partial-delivery accounting |

Run package-scoped validation first (`cargo test -p dekopond --locked` and relevant gateway
integration tests), then the repository's required scope gates. Loopback HTTP assertions prove API
selection, not notification delivery. Separately authorized live checks must inspect Slack native
Working/Stop, Telegram/Discord typing and reaction eligibility, WhatsApp renewal, absence of
placeholder posts, final-answer rendering/notification under known client settings, and cancellation.
Unverified public silent-send API options must not be advertised as supported gateway features.
Streaming table-formatting work is a separate prerequisite for deployments that currently disable
streaming for that reason; this proposal does not fix it.

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
