# Approach note: chat-history retention (22-memory-unify/memory/history-retention), r3

Temporary review note, deleted before the ready-for-review push. Base `4d1a4d5` (v0.87.0).
Settled by the coordinator: `storage.maxNamespaces` stays for non-chat namespaces; defaults are
20,000,000,000 bytes and 90 days. Settled by r2: F1, F2, F5, F6, F8, and shared-scope chats share
lifetime but not storage identity.

## What changed from r2, and why

r2 put the budget in the gateway and settled broker bytes from the broker's responses. Most r2
findings (1, 4, 5, 6, 7) came from that split. r3 moves the budget, the ledger and admission into
the **broker**, for three reasons:

- The broker already enforces storage reservations at each mutation.
- The broker is the common recording boundary that every writer crosses, including the TUI smoke
  recorder.
- The broker owns the root-byte and entry headroom that the budget has to respect.

The gateway becomes a client for its own store. It reserves before it writes a journal record,
settles afterwards, and runs a whole-chat reclaim only when the broker names one.

## Conflicts with the code at base, and the reading followed

1. Wakes skip the broker record (`session.rs:4098`). Reading: wake activity reaches the broker as
   part of journal settlement, below.
2. Broker recording comes before journal persistence (`session.rs:3749`, then
   `memory/generation.rs:154`). Reading: one reservation, taken before the broker record, covers both
   writes.
3. The gateway's whole-file sweep runs on file mtime (`journal.rs:873`), and record eviction renames
   the file, resetting that mtime. Summary slots append in place. Reading: whole-chat expiry uses
   activity; record-level eviction is unchanged.
4. The broker namespace identity carries `channel = conversation.id` and
   `conversation = id[:thread]`, but no workspace (`dekopon-broker/src/lib.rs:3292`). The retention
   key includes the workspace, so stored legacy identity cannot be mapped back to a key. Migration
   exception M1 below covers this.

## Retention key (accepted in r2)

The key is the digest of `ConversationKey::absorb` (`conversation.rs:91`), moved to `dekopon-core`.
Its inputs are agent, transport, workspace, channel, thread, and the canonical subject when the audience
is private. The broker computes it from the authenticated attestation plus the gateway's `audience`
bit. A `chat` file in the namespace base links each chat-memory namespace to its key.

## Config (stated once, in the broker)

```yaml
history:
  maxBytes: 20000000000   # gateway journals + summaries + broker chat memory
  maxIdleMs: 7776000000   # whole chat expires this long after its latest committed append
```

The gateway gets no budget key. The keys live at the broker's top level, so a journal-only
deployment without provider storage still has a budget.

## The ledger (broker, in memory, rebuilt at boot)

- **Per key:** `{ broker bytes, gateway bytes, broker activity, gateway activity, gateway revision }`.
  Total = the sum of both stores' bytes plus outstanding reservations.
- **Broker side, rebuilt from the root scan `StorageHost::open` already runs:**
  - bytes come from the existing mutation accounting (`StorageHandle::after_mutation`,
    `Reservation::observe_direct`, `account_sweep`);
  - activity comes from a per-namespace `activity` file, set at handle finalization only after a
    committed append (accepted in r2).
- **Gateway side:** a mirror, synchronized at gateway boot and whenever the broker's boot epoch
  changes.
  - The sync is `SyncHistory` in pages of 1,024 `{ key, bytes, newestAtMs }`. Each page carries the
    gateway's boot epoch, and the final page closes the sync.
  - Before the sync completes, a gateway reservation is refused with `Unsynced`, and the gateway
    syncs, then retries.
  - The gateway index stays bounded by the r2 F5 rules: a 4 KiB per-chat charge, a cap of 65,536
    chats, and a bounded boot scan.
- **Ordered settlement (r2-6):**
  - Each gateway settle carries `(epoch, per-key revision, absolute bytes, newest at_ms)`. The broker
    applies it only if the revision is newer, so a delayed settle cannot overwrite a later one.
  - A reservation whose settle never arrives stays debited in full until a later settle for that key,
    or the next sync, supersedes it. An indeterminate write is never released early.

## Admission (r2-4, r2-5, r2-7)

**Gateway turn.** Before the broker record (or before the journal append, when the route has no chat
memory), the gateway sends:

```text
ReserveHistory { key, journalBytes, record: bool } -> Reserved { ticket } | Reclaim { pass, key, activityMs } | Refused(TooLarge | Exhausted | Unsynced)
```

- `journalBytes` is the encoded record size plus the entry charge, an upper bound computed by the
  same encoder that writes the record.
- With `record`, the broker adds its own upper bound for the chat-memory grant:
  `maxWriteBytesPerInvocation` plus `maxFilesPerNamespace × ENTRY_CHARGE`. The existing
  `charge_write` and file limits enforce that bound at mutation, so retained growth cannot exceed it.
- `TooLarge` (need > `maxBytes`) is returned before any reclamation.
- Fitting means the aggregate budget **and** the broker root-byte and entry headroom both hold. A
  shortfall in either starts reclamation of chat history only. Non-chat namespaces and their quotas
  are never candidates.
- **Settlement:**
  - `RecordDeliveredTurn` carries the ticket. The record grant draws its portion; settlement at
    handle finalization charges the actual growth and releases the rest.
  - The journal portion settles through `SettleHistory`.
  - Summary, pin and choice writes reserve and settle the same way, in async context before their
    blocking write.

**Any other recorder** (the TUI smoke recorder, `server.rs:876`). `RecordDeliveredTurn` without a
ticket reserves inline at the same boundary. It can reclaim only chats with no gateway copy, and
otherwise returns the typed `Exhausted`.

## Reclamation: one bounded pass, whole chats, both stores (r2-1, r2-3, r2-8, r2-9)

- **Snapshot.** A pass is created on the first shortfall. It holds a fixed snapshot of the 64
  candidate keys with the least recent activity. Activity per key is the maximum over both stores,
  and keys with outstanding reservations are excluded. Each candidate is examined at most once.
  When the snapshot is spent, the pass ends with `Exhausted`, however activity has moved in the
  meantime.
- **Candidate with no gateway copy.** The broker reclaims it inline.
- **Candidate with a gateway copy.** The broker returns `Reclaim { pass, key, activityMs }`, and the
  gateway then:
  1. Takes the key's existing conversation lease (`runner.conversations`). Busy means it sends
     `Skip`. **This lease is moved to before journal recall** (`session.rs:1369`) and held through
     deferred persistence, so a running invocation's key is busy from its first history read.
  2. Writes and fsyncs `<stem>.reclaim` (an intent), then reads its journal's real newest `at_ms`.
  3. Sends `ReclaimChat { pass, key, gatewayActivityMs }`.
- **Broker revalidation.** The broker `try_lock`s the namespace mutex and `base.lock` of **every**
  namespace linked to the key, before any destructive step. If any is busy, or if
  max(its committed activity, `gatewayActivityMs`) is newer than the snapshot value (or, for expiry,
  newer than the cutoff), it deletes nothing and answers `Skipped`. Otherwise it deletes all of them
  and answers `Reclaimed { bytes }`.
- **Gateway finish.** On `Reclaimed` the gateway unlinks the journal file, then the intent, settles
  0 bytes for the key, and releases the lease.
  - Stale mirror data can only reorder candidates. Deletion is authorized by both stores' real
    activity, read under exclusion in both stores.
  - On `Skipped` the gateway removes the intent and releases the lease.
- **Retry.** The gateway then repeats `ReserveHistory` with the same `pass`.
- **Idle expiry** uses the same pass shape with the cutoff now − `maxIdleMs`, at gateway boot and in
  `maintain`, with no new scheduler.

**Interrupted deletion (r2-8).**
- **Broker:** before touching a namespace, it writes `reclaiming/<token>` in the namespaces root and
  syncs that parent, under the namespace mutex and `base.lock`.
  - Removal order: data, `current`, `activity`, `chat`, `last-used`, `identity`, `base.lock`, the
    base directory, and last the intent file.
  - `StorageHost::open` and `NamespacePlan::prepare` finish any pending intent before admitting the
    token. A residue with no `base.lock` is owned through the in-process namespace mutex plus the
    intent, and no peer process opens storage.
- **Gateway:** at boot, before the first reservation, every `<stem>.reclaim` intent is finished
  regardless of pressure: `ReclaimChat` again (idempotent; an empty key answers `Reclaimed { 0 }`),
  then the journal unlink, then the intent.

## Legacy chats (r2-2): bounded one-time migration, before reclamation is enabled

- **Scope.** The migration runs once at broker open over existing chat-memory-provider namespaces
  with no `chat` file. It is bounded by `startupMaxEntries`. Tokens, generations and authority
  pointers are untouched. Reclamation stays disabled until it completes.
- **Association.** The retention key needs the workspace, which legacy identity lacks (conflict 4).
  A legacy namespace becomes associated lazily: the first grant that carries an authenticated
  attestation for it writes its `chat` file.
- **Migration exception M1, for your decision.**
  - Until a legacy namespace is associated, it is its own retention unit, keyed by its namespace
    token.
  - Its activity is the newest mtime among its current generation's data files. Appends and
    record-time compaction are the only writes, and reads never touch those files. If it has no data
    file, its activity is the migration time.
  - Pre-DRN-cutover namespaces that are never written again stay in this state. They expire at the
    earliest 90 days after their last real append, or 90 days after rollout.
  - `last-used` is never used.

## Authorization (accepted in r2)

`ReserveHistory`, `SettleHistory`, `SyncHistory`, `ReclaimChat` and `Skip` are restricted to the
reconciliation owner (`server.rs:526`, `dekopon-gatewayd`). A ticketless `RecordDeliveredTurn` keeps
its current peer rules.

## Ship order and consumers

- **Wire changes:**
  - `DeliveredTurnRequest` gains `audience` and an optional `ticket`.
  - Five owner-only operations are added.
- **Consumers at their deployed version:** gateway and broker, which ship in one image
  (`_helpers.tpl:828`), and `dekopon-tui`'s `smoke_record`, which is updated in the same PR.
- Homelab keys are unchanged. The writer reconciles PVC capacity before enabling.

## Evidence

One record per reclaim: `history.reclaimed`, with `reason` (`idle` | `budget` | `capacity`), bytes,
namespace and record counts, skips, and the snapshot or cutoff time in ms. Each pass logs one
`history.pass.exhausted` with its examined count. No bodies, keys or values are logged.

## Tests (synthetic; existing journal, storage-host and brokerd harnesses)

- **Clocks:** a 90-day expiry; a day-89 wake whose propagation failed still keeps the day-89 journal
  through revalidation; reads and failed appends never advance the clock; ledger and intents
  survive restart; record-age eviction still trims an active chat.
- **Pressure:**
  - The least-recently-active chat is reclaimed in both stores, even when younger than 90 days.
  - Root-byte pressure reclaims chat history and leaves a turso namespace and its `maxNamespaces`
    count intact.
  - 600 small chats are admitted.
  - `TooLarge` deletes nothing.
  - The TUI recorder is admitted, and refused with `Exhausted` when it cannot be.
- **Ordering:** concurrent reservations never exceed `maxBytes`, and a delayed settle is ignored.
- **Concurrency and recovery:**
  - A busy lease in either store skips the chat, and an all-busy snapshot ends with `Exhausted`.
  - A crash at each deletion step, in either store, finishes after reopen.
  - A non-owner peer is refused.

## r2 resolution

| r2 finding | Change |
|---|---|
| 1 | Candidates are ordered by the maximum activity across copies. Deletion revalidates both stores' real activity under exclusion; propagation runs through ordered settlement. |
| 2 | One-time bounded migration before reclamation is enabled; lazy association from an authenticated grant; migration exception M1 for your decision. |
| 3 | The conversation lease moves before journal recall and is held through deferred persistence. Both stores are excluded before the first destructive step. |
| 4 | Enforceable upper bounds, enforced at mutation by existing write and file limits; aggregate reservations; unused allowance released at settlement. |
| 5 | Admission sits at the broker recording boundary; the TUI recorder is admitted there or refused with a typed error. |
| 6 | Per-key revisions with epoch; indeterminate writes stay debited until superseded. |
| 7 | Admission includes root-byte and entry headroom; a capacity shortfall reclaims chat history only. |
| 8 | Broker intent is kept outside the removed directory and finished at open and at prepare; gateway intent covers the cross-store unit and is finished at boot. |
| 9 | A fixed 64-candidate snapshot per pass; `Exhausted` at the bound. |
| taste | Hooks are named. |
| guideline | `dekopon-tui` is in the consumer inventory. |
