# Approach note: chat-history retention (22-memory-unify/memory/history-retention), r2

Temporary review note, deleted before the ready-for-review push. Base `4d1a4d5` (v0.87.0).
Coordinator decisions applied: `storage.maxNamespaces` stays; defaults are 20,000,000,000 bytes and 90 days.

## Conflicts with the code at base, and the reading followed

1. **Wakes never reach the broker store** (`session.rs:4098`). A broker copy therefore cannot
   advance its own clock on a wake. Reading: the gateway propagates wake activity to the broker
   (see the activity flow below).
2. **Broker recording precedes journal persistence.** `session.rs:3749` records the turn, then
   `memory/generation.rs:154` appends it to the journal. Reading: one admission covers both writes
   and runs before the first of them.
3. **The gateway's whole-file sweep runs on file mtime** at the largest `forgetAfterMs`
   (`journal.rs:873`). Record-age and agent-cap eviction rename the file and so reset that mtime;
   summary slots append in place. Reading: whole-chat expiry uses the newest record's `at_ms`
   instead. Record-level eviction (`maxAgeMs`, `maxBytesPerAgent`, `forgetAfterMs` recall) is unchanged.
4. **`maxNamespaces` counts every provider** (`quota.rs:81`). Reading: chat-memory namespaces leave
   its count and admission. Non-chat providers keep the ceiling. Root bytes and entries still cover
   both classes.

## Identity: the retention key

A chat's **retention key** is the digest the gateway already uses for its journal stem,
`ConversationKey::absorb` (`conversation.rs:91`). Its inputs are the agent, transport, workspace,
channel and thread, plus the subject's canonical DRN for `privateConversation` and nothing for
`sharedConversation`. The function moves to `dekopon-core` beside `SpaceKey`, so both daemons compute
the same key.

- The broker derives the key from the request's authenticated attestation (subject, agent, claim
  conversation). From the gateway it takes only the route's audience, private or shared, as a new
  `audience` field on `DeliveredTurnRequest` and on the activity request.
- The key groups storage for lifecycle only. Each broker namespace keeps its own identity, token,
  scope and grants; nothing is merged or widened. In a private chat the key includes the subject,
  so one person's activity never keeps another person's copy alive. In a shared chat the gateway
  already replays one history to every participant, and its per-subject broker namespaces share
  that chat's lifetime.
- On a chat-memory grant the broker writes a `chat` file holding the key into the namespace base
  (rewritten if the audience changes; last writer wins).

## Clocks (persisted; reads, scans and failed writes never advance them)

- **Gateway:** the newest record's `at_ms` in the chat's journal file. Wakes append records like any
  turn. Evicting old records leaves the newest in place. Summaries, pins and choices do not change
  `at_ms`. No marker file is added.
- **Broker:** a new `activity` file per chat-memory namespace, set only when a history append
  succeeds. `StorageHandle` notes a committed `jsonl_append`/write and sets `activity` at
  finalization, under the base lease it already holds. `last-used` and every non-chat provider's
  `idle-ttl` keep today's semantics.
- **Activity flow:**
  - Every successful record advances `activity`.
  - For a wake, the gateway sends `ChatActivity { attestation, audience }` after the journal
    append. The broker advances `activity` on each namespace linked to that key, under each one's
    lease, and creates nothing.
  - A failed propagation leaves the broker copy older until the next activity reaches it.

## Accounting and bounds

- **Budget owner:** one gateway `HistoryBudget`. A `parking_lot` mutex guards
  {gateway bytes, broker bytes, reserved}. A `Semaphore(1)` serializes reclamation across awaits.
- **Gateway index:** stem → (journal-file bytes plus a fixed per-chat charge, newest `at_ms`). It is
  kept under the journal `lock`, and only journal files count. Seals and follow markers are excluded
  and keep their own lifecycle owners. Assets leave through the existing `sync_assets`.
- **Gateway bounds:**
  - The per-chat charge (4 KiB) prices metadata even when the transcript holds 0 bytes.
  - A constant cap of 65,536 journal chats (the broker's `maxNamespaces` ceiling) bounds the index
    and the boot scan. Creating a chat at the cap reclaims the least-recently-active eligible chat
    instead of refusing.
  - A boot scan that finds more than the cap fails fast, naming the directory.
  - A chat whose records are all evicted loses its file, so its index entry goes with it.
- **Broker index:** retention key → {namespaces, bytes, newest `activity`}, for chat-class
  namespaces only. A namespace is chat-class when it has a `chat` file. Chat-class namespaces skip
  `reserve_namespace`'s count but still reserve root bytes and entries.
  - `StorageHost::open` takes the chat-memory provider id. On first boot it adopts legacy
    namespaces of that provider in one move: each gets `chat` set to its own namespace token
    (a group of one) and `activity` set to its `last-used` mtime. They then fall under the policy
    with no purge.
  - The index is rebuilt by the root scan `open` already performs, which `startupMaxEntries` bounds.

## Admission (all history growth goes through the budget owner)

1. Before the broker record (or the journal append, if the route has no chat memory), the session
   computes `need` = estimated journal record bytes + the encoded `DeliveredTurnRequest` bytes the
   broker will append.
   - If `need > maxHistoryBytes`, the result is `HistoryBudget::TooLarge`, decided before any
     reclamation or write.
   - Otherwise the session takes a `Reservation` (RAII) that also marks its stem busy.
2. Summary slots, pins and choices take their own reservation, in async context, before the
   blocking journal write. Broker-only routes reserve the broker estimate only.
3. **Settling:**
   - The journal write settles to the file's actual size change.
   - Every broker response in this family carries the broker's total chat-class bytes, taken from
     the existing `account_finish`/`account_sweep` hooks. Partial failures are therefore counted as
     retained.
   - A broker overrun above the estimate is counted when it settles and reclaimed before the next
     admission. It is bounded by `maxWriteBytesPerInvocation`.
4. If no candidate in either store can be reclaimed, admission returns `HistoryBudget::Exhausted`.
   The turn is still answered; the existing record/append failure WARN carries the category.

## Reclamation: whole chats across both stores, LRA

```text
ChatHistory { after: Option<Cursor>, reclaim: Option<{ key, notAfterMs }> }
  -> ChatHistoryReport { bytes, reclaimed: { bytes, namespaces, leased }, next: Option<{ key, activityMs }> }
```

- **One loop, bounded per pass:**
  - The gateway compares its oldest eligible chat with the broker's `next` and picks the older key.
  - It reclaims that key in both stores: its journal file, if any, then `reclaim` for the key at
    the broker.
  - The cursor (activity time, key) only moves forward, so the pass terminates.
  - A pass also stops when a reclaim frees nothing and neither store has a further candidate.
- **Pressure** reclaims until the reservation fits, even for chats younger than 90 days.
- **Idle expiry** (cutoff = now − `maxChatIdleMs`) uses the same loop with "activity < cutoff" as its
  stop. It runs at boot and in `maintain` after each append, with no new scheduler.
- **Active use is skipped in both modes:**
  - The gateway skips busy stems.
  - The broker revalidates each namespace under its namespace mutex and `base.lock` `try_lock`. A
    leased namespace, or one whose `activity` is newer than `notAfterMs`, is skipped and counted in
    `leased`.
  - A partly reclaimed chat stays in the index with its remaining namespaces.

## Interrupted deletion

- **Broker:** before any destructive step, and under the existing locks, write and fsync a
  `reclaiming` file in the base.
  - Removal order: data, then `current`, `activity`, `chat`, `last-used`, then `identity`, then
    `base.lock`, then `reclaiming`, then the directory.
  - The `StorageHost::open` scan and `NamespacePlan::prepare` both finish any base holding
    `reclaiming` before admitting use, whatever its age or the current pressure.
  - Until the final step the `reclaiming` file keeps the residue identifiable and lockable, so
    `prepare` never sees the markerless state it rejects (`namespace.rs:230`).
- **Gateway:** a chat is removed by one atomic unlink of its journal file, so it is either present or
  gone. The existing boot-time `attach_assets` → `sync_assets` reclaims its derived assets.

## Authorization

`ChatHistory` and `ChatActivity` are accepted only from the gateway peer that passes the existing
reconciliation-owner check (`server.rs:526`: mapped peer, attestor grant, `RECONCILIATION_OWNER`,
coordination `owns(uid)`). Any other mapped peer gets `ERROR_UNAUTHENTICATED` before the history index
is touched. These are daemon-level requests, unreachable from provider invocations, guests or upcalls.
Namespace classification and selection stay broker-side. A test covers the denied peer.

## Config, ship order, evidence

- **Config:** `memory.retention.maxHistoryBytes` (default 20,000,000,000) and
  `memory.retention.maxChatIdleMs` (default 7,776,000,000) in the gateway config, stated once.
  The broker gets no budget key.
- **Ship order:**
  - The wire changes are one field on `DeliveredTurnRequest` and two daemon operations. Gateway and
    broker share one image (`_helpers.tpl:828`), so they ship in one release.
  - Homelab config changes nothing in its keys. The writer reconciles PVC capacity with 20 GB
    before enabling.
  - The continuity fix (#937) is already in base.
- **Evidence:** one record per reclaim, `history.reclaimed`, with `reason` (`idle` | `budget` |
  `capacity`), bytes, namespace/record counts, leased skips and the cutoff in ms. No bodies, keys
  or values.

## Tests (synthetic, on the existing journal, storage-host and brokerd harnesses)

- **Expiry:** a chat expires after 90 days; a day-89 wake keeps both copies; a recall or search
  resets nothing; a failed append resets nothing; the clocks survive restart; record-age eviction
  still trims an active chat.
- **Pressure:**
  - The least-recently-active chat is reclaimed in both stores even when younger than 90 days.
  - A broker-only chat older than every journal chat goes first.
  - 600 small chats are admitted under the budget.
  - A turso namespace survives and still counts against `maxNamespaces`.
  - `TooLarge` deletes nothing.
- **Concurrency and recovery:**
  - A leased namespace is skipped and the pass ends when only leased chats remain.
  - A reclaim races an append on real threads.
  - A crash after each removal step, followed by reopen or prepare, finishes the deletion.
  - A non-owner peer is refused.

## r1 resolution

| Finding | Change |
|---|---|
| F1 | `maxNamespaces` kept for non-chat namespaces; chat-class namespaces skip only its count; no homelab key change. |
| F2 | New `activity` marker set only on a committed chat append at handle finalization; `last-used` and `idle-ttl` unchanged. |
| F3 | Shared retention key derived from authenticated identity, with an audience bit; `chat` link; wake activity propagated; whole-chat reclaim by key in both stores. |
| F4 | One reservation before the first write of either store, including broker-only routes and later summary/pin/choice growth; settled from actual sizes; `TooLarge` decided first. |
| F5 | Fixed per-chat charge, a constant chat cap with LRA reclaim at the cap, a bounded boot scan, and no zero-byte markers. |
| F6 | Seals and follow markers excluded; only journal files and their records and summaries are reclaimed. |
| F7 | Persistent `reclaiming` state finished at open and at prepare; atomic unlink on the gateway. |
| F8 | Reconciliation-owner check on both new operations; denied-peer test. |
| F9 | Forward-only cursor, a reclaim result that counts leased skips, a bounded pass, `Exhausted` on no progress, and revalidation under lease. |
| Taste | Summary slots append in place (conflict 3); image reference is `_helpers.tpl:828`. |
