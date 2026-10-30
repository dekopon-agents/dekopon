# Gateway wakes and detached jobs

Turns the gateway starts without a new message: wakes, the daily digest, PR subscriptions and detached jobs.

## Detached jobs

A route opts into jobs with `limits.jobTimeoutMs`; without it, `&`, `jobs`, `wait` and
`kill` report that jobs are off. Every job shares its starter's capability-call budget.
`jobTimeoutMs` is the independent deadline of each job a turn starts; a job started by a probe
or by another job shares its starter's deadline and other tree budgets. `cmd &` starts a
job that outlives its script and turn, prints `[N]`, and sets `$!`. `jobs` lists the person's jobs in that conversation;
`wait %N` waits without a completion notice, and `kill %N` stops one. A finished job
not waited for sends a bounded notice as a separate turn or a steer; a busy gateway
may drop it, so `jobs` is the fallback. `sessions.maxJobs` (default 2) bounds running
jobs and retained rows across the process. Jobs and their rows are lost at restart.

A job inherits its starting scope, but its variables and buffers die with it; only its
printed output reaches the notice (at most 16 KiB of inbound text). Jobs have no chat
asset inputs or reply attachment slot and cannot send files or images. A job started
by a probe remains read-only; the broker refuses write capabilities under trigger
`probe`. A notice turn carries trigger `job` and may itself start jobs: on a route
with `jobTimeoutMs`, an agent can keep itself alive through successive notices.
The route key is the explicit opt-in; the person's stop word cancels their running
jobs in that conversation, including when no turn is active.

## Wakes

A route with `wakes: true` offers the model a `wake` tool, so an agent can come back to a
conversation later without anyone writing to it. A wake is a synthesized message: it carries the
conversation and reply target of the authenticated message that scheduled it; the model chooses
only when, its probe and the note it will read. Every new wake created in a mapped space carries
`run_as: space` and the canonical channel subject. Creation is authorized as asker ∩ space;
firing runs as that space alone. Wakes created outside a mapped space, including DMs, stay
personal. Legacy once/watch rows without `run_as` retain their personal ownership.

- `schedule` wakes the agent once, `afterSeconds` from now.
- `watch` runs a probe script every `everySeconds` (at least `minIntervalMs`, at most `forSeconds`)
  for up to `forSeconds`, with no model in the
  loop. Exit 0 wakes the agent with the probe's output; exit 1 keeps waiting; any other exit, or
  output over 8 KiB, wakes it with the failure and retires the watch. `$PREV` holds the previous
  run's output. The first run happens while scheduling, with `$PREV` unset: it never wakes the
  agent, and a probe that exits 2 or more there is refused rather than stored.
- `digest` uses the watch arguments for a daily digest anchored to this message's thread.
  It requires a mapped channel, refuses a second digest in the same space, and checks at least
  24 hours apart. Ordinary schedules/watches keep their existing configured bounds.
- `list` and `cancel` include the current mapped channel's space schedules from any freshly
  authorized participant, matching the anchor's conversation container rather than its thread
  or original asker. Legacy personal entries remain subject-scoped; a DM sees only its own
  personal wakes. Another channel or DM cannot cancel a space wake by its ID.

Every probe run opens its own broker leg attested with `trigger: probe`. The broker neither lists
nor authorizes a capability that is not read-only on that leg (`probe-write`), whatever policy
says. A daily digest's woken session also attests `trigger: probe` and stays read-only; other
woken sessions attest `trigger: wake`. Each is authorized afresh, so a personal wake can do only
what its scheduling person could do by typing. Every wake newly
created in a mapped space, whether schedule, watch, digest or PR subscription, runs as the space
alone and can do only what that space may do, without a participant leg or the original person's
grants. Space-watch probes also run as the space alone under the read-only ceiling.
Owner Cedar policy can narrow either through `context.trigger`.

Pending wakes live in one JSONL file (`0600`, rewritten through a rename, no fsync). A due
single-shot wake's row is removed before its session starts, so a restart at that moment loses
it. A recurring digest retains its row after a changed firing; there is no crash-durable
exactly-once guarantee. A due watch is leased by moving its next check forward in the same write, so one
watch never runs two probes at once. Ticks take a `sessions.maxConcurrent` permit and are skipped,
not queued, when none is free. A line that does not parse refuses startup; deleting the file
resets every pending wake. On WhatsApp a wake cannot be scheduled past 24 hours after the message
that asked for it, because Meta refuses free-form messages outside that window.

A wake bypasses stop words, the addressed filter and media collection. Behind a running
conversation it queues as a follow-up if there is mailbox room, never as a steer; an idle conversation
starts its turn directly if a permit is free. Wakes receive no acknowledgment. The same mailbox
and saturation refusals apply. Only when its route no longer answers there as
the same agent with `wakes: true` does the gateway post a personal wake's note and any probe
output without a session. Space wakes never use this orphan fallback. The local development
transport cannot schedule wakes: its reply target is a connection number a restart reuses. Provider chat memory does not record wake turns;
the resident window and the journal do. Platform recall for a wake reads the history up to now.

### Daily digest

A mention such as “post a daily digest here” uses `wake` action `digest` with a read-only
probe, `everySeconds: 86400` and `forSeconds` within `sessions.wakes.maxHorizonMs` (30 days by
default). The first probe records a baseline without posting. Later changed, nonempty checks
wake the space to verify stale open non-draft PRs (seven inactive days), unanswered review
requests and unassigned open issues through granted GitHub reads. Stable probe output excludes
observation timestamps and changing age counters. Read failures retire the digest with a
structured cause; they are not treated as empty results.

Unchanged or empty checks produce no model turn or chat post. The model may also select
`decline_chat_reply` after reading when there is nothing new to report. A digest posts at most
one concise message in the anchored thread, with GitHub logins as plain text and Slack mention
syntax neutralized; it produces no progress or streaming messages. Successful checks update the
previous output and retain the recurring watch until cancellation or its fixed expiry.
Expiry is quiet and never renews automatically: request a new digest after expiry or cancel
and replace it. Both creation and replacement wait a full interval before their first check.

Digests use the existing strict watch shape and a gateway-authored header in its script;
`run_as` selects ownership independently of ordinary watch versus digest lifecycle. Old
once/watch rows and s5 subscription rows still load. Older gateways reject the new strict
store shapes: the coordinated s5/s6 release is **revert-safe: no**, and rollback requires
removing the wakes file, losing all pending wakes and subscriptions.

### PR subscriptions

A successful PR-link COMMENT review on a mapped Slack channel route with `wakes: true` leaves
one subscription per repository, PR and anchored thread. Repeating the review in the same
thread reuses it. The store defaults to 64 subscriptions per agent and a 14-day lifetime from
creation, configured by `sessions.wakes.maxSubscriptionsPerAgent` and `subscriptionExpiryMs`.
The next distinct subscription above the limit is refused; pushes do not extend expiry.

A later routed `pull_request.synchronize` delivery fires only when its repository subject is
mapped by the broker and falls inside the GitHub route's `ids`. The wake resumes the anchored
thread as the space alone, reads the current head, and refreshes its head-pinned COMMENT review.
When every capability invocation in the turn successfully refreshes the subscribed PR's review,
the model replies to Slack only when blocking findings changed; otherwise it may select
`decline_chat_reply`, producing no chat post or progress message. Any other invocation, including
a failed review or a review of another PR, requires a visible Slack report even after a successful
refresh, or the fixed audit-before-retry warning when no reporting turn remains.
Unmatched deliveries retain their ordinary repository-session handling.

PR close, including merge, expiry and cancellation remove the subscription. From any mapped
participant's freshly authorized turn in the channel, `wake` action `listSubscriptions` lists
that channel's subscriptions and `cancelSubscription` cancels one by `id`; both require
asker ∩ space authorization and are refused in DMs. These subscription actions remain separate
from scheduled-wake `list` and `cancel`.
Subscriptions share the strict JSONL wake store. Older gateways cannot read the new line kind;
rollback requires removing the wakes file, resetting every pending wake and subscription.
A lost subscription route is ignored silently rather than posted as an orphan reminder.
