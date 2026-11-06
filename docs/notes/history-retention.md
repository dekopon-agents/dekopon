# Approach note: chat-history retention (22-memory-unify/memory/history-retention)

Temporary review note; it is deleted before the ready-for-review push. Base `4d1a4d5` (v0.87.0).

## Conflicts between the brief and the code at base, with the reading I follow

1. **Reads reset the broker clock.** `NamespacePlan::apply` touches `last-used` on every grant
   (`dekopon-storage-host/src/namespace.rs:367`), including the read-only
   `chatMemoryRecent`/`chatMemorySearch` grants. Reading: touch only on write-access grants. This also
   changes `idle-ttl` for other providers to "idle since last write grant"; no deployed provider uses
   `idle-ttl` with read-only grants.
2. **Wakes never reach the broker store.** `record_delivered_turn` skips wakes
   (`dekopon-gatewayd/src/session.rs:4098`), so a broker copy has no wake entries. Reading: each store's
   copy keeps its own last-append clock. The gateway copy's clock includes wakes. Both copies sit under
   the same budget and the same 90 days, so deleting one never puts the other outside the policy.
   I do not link the two copies: the identities differ (the gateway stem drops or keeps the sender per
   route `scope`; the broker namespace hashes the canonical subject), and the brief forbids merging them.
3. **The gateway's whole-file expiry runs on file mtime** at the largest route `forgetAfterMs`
   (`memory/journal.rs:873`). Every record-age or agent-cap rewrite and every summary-slot write
   renames the file, which resets that mtime. Reading: the new chat-idle clock replaces this whole-file
   sweep. Record-level eviction (`maxAgeMs`, `maxBytesPerAgent`, `forgetAfterMs` recall horizon)
   stays as is.
4. **Record-age eviction empties a journal long before 90 days** with the 7-day defaults, and that
   would lose the chat clock. Reading: the clock lives in its own marker, not in the records.
5. **`storage.maxNamespaces` is the chat admission ceiling** (`quota.rs:104`) and counts every
   provider's namespaces. Reading: delete the key. `maxRootBytes` and `startupMaxEntries` already
   bound the ledger and the restart scan, since each namespace costs at least seven entries.
   **This is a config-key deletion**, the one contract decision below.

## Design

**One budget, stated once, owned by the gateway.** The gateway owns the chat: it appends both copies'
source turns and is the only caller of `chatMemoryRecord`. New keys sit beside the existing record-level keys:

```yaml
memory:
  retention:
    maxAgeMs: 604800000          # existing, record-level
    maxBytesPerAgent: 67108864   # existing, record-level
    maxHistoryBytes: 20000000000 # new: journal + summaries + broker chat memory, all agents
    maxChatIdleMs: 7776000000    # new: whole-chat expiry after the latest append
```

The broker gets no budget key. Its storage limits stay resource bounds, and the chart and homelab
must size `maxRootBytes` and the PVC to hold the broker's share plus other providers.

**Clocks.**
- Gateway: an empty `<agent>/<stem>.active` file whose mtime is set at each successful `append`,
  including wakes. Rewrites, summary slots, reads and sweeps leave it alone. At first boot after
  rollout, a chat without the file takes its newest record's `at_ms`, so existing history enters the
  policy rather than being purged.
- Broker: the existing `last-used` marker, touched only on write grants. Existing namespaces keep
  their current mtime.

**Accounting.** The gateway keeps an in-memory index, rebuilt at boot from one directory scan:
stem → (bytes of its journal, seal and follow files; last append). It is kept current under the
journal's existing `lock`. The broker keeps the same for chat-memory namespaces only. A namespace
counts as chat memory when its identity file names the configured `chatMemory` provider with a
conversation scope. The index is built by the root scan `StorageHost::open` already runs and updated
by `account_finish`/`account_sweep`. Turso, python and every other provider namespace are never in
this index, so they are never candidates. Assets keep their existing lifetime: they leave with their
record through `sync_assets` and are not counted.

**One new broker-protocol operation**, gateway → broker over the existing daemon connection:

```text
ReclaimChatHistory { throughMs: Option<u64> }
  -> ChatHistory { bytes: u64, oldestMs: Option<u64>, reclaimedBytes: u64 }
```

With `throughMs`, the broker deletes its single least-recently-active chat-memory namespace whose
`last-used` ≤ `throughMs`, skipping any it cannot lease. Without it, the call only reports. The
gateway cannot name a namespace. Deletion reuses `sweep_one`'s sequence: namespace mutex and
`base.lock` `try_lock` (busy means skip), the bounded tree removal and `account_sweep`.

**Admission** (gateway, before `Journal::append`, serialized by one async owner of the budget):
1. Take `need` as the encoded record bytes plus the delivered-turn bytes the broker will append.
   If `need > maxHistoryBytes`, fail with `HistoryBudget::TooLarge` and delete nothing.
2. While `journal + broker.bytes + need > maxHistoryBytes`, reclaim whichever store holds the older
   least-recently-active eligible chat, comparing the gateway's oldest against the broker's
   `oldestMs`. On the gateway, eligible excludes the appending stem and stems with a session in
   flight.
3. With nothing eligible, fail with `HistoryBudget::Exhausted`. Both failures surface through the
   existing append-failure path: the turn is answered and a WARN carries the category. No new
   refusal reaches the chat.

**Inactivity expiry** runs on the gateway's existing triggers, boot and `maintain` after each append,
with no new scheduler. It removes gateway chats idle past `maxChatIdleMs`, then calls
`ReclaimChatHistory { throughMs: now - maxChatIdleMs }` until `oldestMs` is newer.

**Crash and restart.** A gateway chat is removed journal, seal and follow files first, `.active` last,
so an interrupted delete is finished by the next pass. A broker namespace keeps the existing
`sweep_one` order, adjusted so `last-used` goes after the data: a crash leaves a marker-bearing
residue that the next request reclaims, never a markerless base that `prepare` reports as corrupt.
Both indexes are rebuilt from disk at boot, so no ledger file is added.

**Evidence.** Each reclaim emits one record: `gateway.history.reclaimed` /
`storage.history.reclaimed`, with `reason` = `idle` | `budget`, bytes, record or file counts and
the cutoff in ms. No bodies, no namespace values.

## Ship order

The wire change is one new `BrokerRequest` variant and one response. Gateway and broker run from one
image (`dekopon.image`, `deployment.yaml:378,405`), so they ship in one release. Deleting
`maxNamespaces` means homelab `host.yaml` must drop the key in the same rollout that pins this
release, because `deny_unknown_fields` refuses it. The rest is the coordinator's to schedule:
continuity fix (shipped as #937), then capacity reconciliation, then enabling.

## Tests (synthetic; the crate's existing journal and storage harnesses)

- Gateway: 90-day expiry; a late wake resets the clock; a recall does not; the clock survives
  reopen; record-age eviction still trims an active chat.
- Budget: the least-recently-active chat goes first even when younger than 90 days; a broker chat
  older than every gateway chat is reclaimed first; 600 small chats are admitted under the budget;
  a turso namespace survives; an oversized record returns `TooLarge` with nothing deleted.
- Concurrency: a leased namespace is skipped and the next one taken; a reclaim racing an append
  under real threads; a crash at each removal step, then reopen, finishes the delete.

## Decisions for the owner

1. **Delete `storage.maxNamespaces`** (recommended) rather than keep a count ceiling beside the
   byte budget. The alternative keeps the key and exempts chat-memory namespaces from it, which is
   a second meaning for one key.
2. **Defaults** for the two new keys: the owner's 20 GB / 90 days (recommended). Below that, the
   budget does not bind until the PVCs are sized, and the homelab writer reconciles capacity
   before enabling.
