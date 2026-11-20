# Skeleton: gateway (wakes, durable store, notices, tick runner)

Step `06-designs/skeleton-gateway/design`, xhigh revision 3. The coordinator ruled that
skeleton-reload (#982) owns `daemon/supervisor` and is not widened. Branch `design/skeleton-gateway`, PR
dekopon-agents/dekopon#983. Code head `ad7712f`. Base: main `af0dcf4`, merged rather than
rebased so stacked unit branches keep their base.

This file sits at the branch root for the coordinator to fetch, and it is removed once fetched.
While it is here, the `docs` word-budget job counts it.

This revision could not read campaign-trail: the session's classifier refused `add_repo`. Units and
acceptance stubs come from the medium draft (`955ccd3`); the rest was re-derived from the code.
The `S-gateway-NN` ids are this revision's, so reconcile them with the draft's.

## How units use it

- Each unit branches from this head and fills only its own bodies. Every body is `todo!()` under a
  site `#[expect(clippy::todo, reason = "skeleton: <unit>")]`. Each new module expects `dead_code`
  until its unit wires it; rustc fails an expectation that stops firing.
- Every lifecycle stub is `#[ignore = "skeleton: <unit>"]` with a `todo!()` body. A unit turns its
  stubs into real tests. It does not delete or rename one without saying why.
- Registration in existing files only:
  - `mod` lines (including `mod restart`), `pub use store::{ConversationKey, StoreError}` and
    `GatewaydError::Store` in `lib.rs`;
  - `mod cron` and `mod delivery` in `wake.rs`;
  - the test modules in `tests.rs`;
  - one match arm each in `gateway.rs` (three) and `tests/gateway/fauxlack.rs`;
  - `Deserialize, Serialize` on `transport::ThreadClaim`.
- New dependencies:
  - `rusqlite` 0.43 with `bundled`;
  - `croner` 2.2.0, because 4.x pulls duplicate `darling` and `strum`, which `cargo deny` bans.
- Merged with `design/skeleton-reload` at `779e17b`, the tree has no textual conflict and passes
  `cargo clippy -p dekopon-gatewayd --all-targets --all-features -- -D warnings`.

## Dependency on skeleton-reload

`daemon/supervisor` is not a unit here. Every unit whose files meet supervisor's footprint
(`gateway.rs`, `lib.rs`) depends on S-reload-01..05 and S-reload-129, and builds after that unit
lands:

- S-reload-01: `ServeOutcome` carries its fatal cause.
- S-reload-02: `GatewaydError::ReloadTask`.
- S-reload-03: `reload_ended` over `serve`'s `JoinSet<GatewaydError>`.
- S-reload-04, S-reload-05 and S-reload-129: the broker's reload task and `BrokerStop`.

The draft also had three gateway behaviours that S-reload-01..05 and S-reload-129 do not cover.
The coordinator assigned them, and their stubs stay in this skeleton:

1. **Store-fatal exit → `state/durable-store`** (S-gateway-09).
   - A durable-store write failure on a serving path ends serving with its cause.
   - It is one more `ServeOutcome` cause arm beside S-reload-01's, added after supervisor lands.
   - Today the wake arm logs `wakes = "stopped"` and serves on.
   - `state/spend-slice1`'s `a_failed_settlement_write_fails_fast_like_a_wake_store_failure` relies
     on it.
   - Its four stubs are in `tests/durable_store.rs`.
2. **Reader restarts → `rpi/tick-runner`** (S-gateway-10).
   - A reader task that `RecoveringTransport` gives up on restarts over
     `transport::reconnect_backoff()` while the gateway serves.
   - It stops for a non-retryable error.
   - Today a failed reader ends the gateway (`supervise_transports`).
   - A restart needs the transport back from `read_transport`.
   - Its four stubs are in `tests/tick_runner.rs`.
3. **Tick-panic ends → `rpi/tick-runner`** (S-gateway-11).
   - A panicking wake tick emits one cause-bearing event and serving continues.
   - It reads `TickEnd::Panicked` (S-gateway-07), which `daemon/notice-channel` also counts.
   - Its four stubs are in `tests/tick_runner.rs`.

## Items

| Id | Unit | Item (`crates/dekopon-gatewayd/src/…`) |
|---|---|---|
| S-gateway-01 | durable-store | `store.rs`: `StoreError` (+ `category`), `GatewaydError::Store` |
| S-gateway-02 | durable-store | `store.rs`: `DurableStore::{acquire, restore, commit}`, `Restored`, `BootReport`, `Migrated` |
| S-gateway-03 | durable-store | `store/lock.rs`: `StoreOwner::acquire` |
| S-gateway-04 | durable-store | `store/schema.rs`: `SchemaVersion::{CURRENT, read, migrate}` |
| S-gateway-05 | durable-store | `store/record.rs`: `ConversationKey::{of, transport}`, `Snapshot`, `AnchorRow`, `WakeRow`, `WakeRowKind`, `WatchRow`, `PendingDelivery`, `FollowRow`, `JobRow`, `Change`, `Record`/`RecordVersion` |
| S-gateway-06 | durable-store | `store/convert.rs`: `LegacyInputs::convert`, `Conversion` |
| S-gateway-07 | durable-store | `wake/delivery.rs`: `DeliveryTicket::{issue, wake}`, `Delivery`, `TickEnd`, `WakeStore::{restored, acknowledge}` |
| S-gateway-08 | cron-wakes | `wake/cron.rs`: `CronSchedule::{parse, next_after, missed_between}` + serde, `MissedTicks`, `CronRegistration`, `WakeStore::register_cron` |
| S-gateway-09 | durable-store | `lib.rs`/`gateway.rs`: the store-fatal `ServeOutcome` cause arm, added on S-reload-01 (no stub item at this head) |
| S-gateway-10 | tick-runner | `restart.rs`: `ReaderRestarts::{new, failed, connected}`, `ReaderVerdict` |
| S-gateway-11 | tick-runner | `restart.rs`: `record_tick_panic` |
| S-gateway-12 | notice-channel | `notice.rs`: `Destination`, `NoticeChannel::post` |
| S-gateway-13 | notice-channel | `notice.rs`: `FatalLabel`, `MarkerFile::{in_dir, record_exit, claim_notice, close_after_stability}`, `ExitMarker`, `MarkerNotice`, `MarkerError` |
| S-gateway-14 | notice-channel | `notice.rs`: `WakePanics::{panicked, succeeded}`, `NoticeLine` |
| S-gateway-15 | spend-slice1 | `store/ledger.rs`: `DurableStore::ledger`, `Ledger::{open_hold, settle, charge_crashed_holds, spent}`, `Hold`, `HoldId`, `HoldOutcome`, `SpendMicros` |
| S-gateway-16 | tick-runner | `runner.rs`: `RunnerEntry::run_cycle`, `GitSha::parse`, `EpochSlug::parse`, `IntentId`, `Effect`, `TickPlan`, `ParkReason`, `CycleOutcome` |
| S-gateway-17 | tick-runner | `runner/receipt.rs`: `ReceiptOutput::of`, `ReceiptBatches::{push, finish}`, `Receipt`, `ReceiptOutcome` |

`state/grant-settle` owns no item of its own. It settles S-gateway-15 holds:
`HoldOutcome::Spent` refunds the rest of the estimate, and `Unreturned` charges all of it.

Lifecycle stubs: 63 in `src/tests/`:

| File | Stubs |
|---|---|
| `durable_store.rs` | 27 |
| `notice_channel.rs` | 9 |
| `cron_wakes.rs` | 6 |
| `tick_runner.rs` | 14 |
| `spend_ledger.rs` | 4 |
| `grant_holds.rs` | 3 |

## Footprint

`depends` lists cross-skeleton items a unit builds after. The key name and the `09-<unit>/build`
step ids are this revision's guesses; correct them to the kickoff's.

```footprint
{"unit": "state/durable-store", "step": "09-state/durable-store/build", "files": ["crates/dekopon-gatewayd/src/store.rs", "crates/dekopon-gatewayd/src/store/lock.rs", "crates/dekopon-gatewayd/src/store/schema.rs", "crates/dekopon-gatewayd/src/store/record.rs", "crates/dekopon-gatewayd/src/store/convert.rs", "crates/dekopon-gatewayd/src/wake.rs", "crates/dekopon-gatewayd/src/wake/store.rs", "crates/dekopon-gatewayd/src/wake/delivery.rs", "crates/dekopon-gatewayd/src/gateway.rs", "crates/dekopon-gatewayd/src/lib.rs", "crates/dekopon-gatewayd/src/session.rs", "crates/dekopon-gatewayd/src/follow.rs", "crates/dekopon-gatewayd/src/memory/journal.rs", "crates/dekopon-gatewayd/src/jobs.rs", "crates/dekopon-gatewayd/src/transport/slack.rs", "crates/dekopon-gatewayd/src/config.rs", "crates/dekopon-gatewayd/src/tests.rs", "crates/dekopon-gatewayd/src/tests/wakes.rs", "crates/dekopon-gatewayd/src/tests/attribution.rs", "crates/dekopon-gatewayd/src/tests/digests.rs", "crates/dekopon-gatewayd/src/tests/memory.rs", "crates/dekopon-gatewayd/src/tests/wake_reason.rs", "crates/dekopon-gatewayd/src/tests/durable_store.rs"], "items": ["S-gateway-01", "S-gateway-02", "S-gateway-03", "S-gateway-04", "S-gateway-05", "S-gateway-06", "S-gateway-07", "S-gateway-09"], "rank": 1, "depends": ["S-reload-01", "S-reload-02", "S-reload-03", "S-reload-04", "S-reload-05", "S-reload-129"]}
{"unit": "state/cron-wakes", "step": "09-state/cron-wakes/build", "files": ["crates/dekopon-gatewayd/src/wake/cron.rs", "crates/dekopon-gatewayd/src/wake/store.rs", "crates/dekopon-agent/src/wake.rs", "crates/dekopon-gatewayd/src/tests/cron_wakes.rs"], "items": ["S-gateway-08"], "rank": 2}
{"unit": "state/spend-slice1", "step": "09-state/spend-slice1/build", "files": ["crates/dekopon-gatewayd/src/store/ledger.rs", "crates/dekopon-gatewayd/src/store/schema.rs", "crates/dekopon-gatewayd/src/metering.rs", "crates/dekopon-gatewayd/src/session.rs", "crates/dekopon-gatewayd/src/tests/spend_ledger.rs"], "items": ["S-gateway-15"], "rank": 2}
{"unit": "daemon/notice-channel", "step": "09-daemon/notice-channel/build", "files": ["crates/dekopon-gatewayd/src/notice.rs", "crates/dekopon-gatewayd/src/lib.rs", "crates/dekopon-gatewayd/src/gateway.rs", "crates/dekopon-gatewayd/src/config.rs", "crates/dekopon-gatewayd/src/tests/notice_channel.rs"], "items": ["S-gateway-12", "S-gateway-13", "S-gateway-14"], "rank": 2, "depends": ["S-reload-01", "S-reload-02", "S-reload-03", "S-reload-04", "S-reload-05", "S-reload-129"]}
{"unit": "state/grant-settle", "step": "09-state/grant-settle/build", "files": ["crates/dekopon-gatewayd/src/proxy.rs", "crates/dekopon-gatewayd/src/tests/grant_holds.rs"], "items": [], "rank": 3}
{"unit": "rpi/tick-runner", "step": "09-rpi/tick-runner/build", "files": ["crates/dekopon-gatewayd/src/runner.rs", "crates/dekopon-gatewayd/src/runner/receipt.rs", "crates/dekopon-gatewayd/src/restart.rs", "crates/dekopon-gatewayd/src/config.rs", "crates/dekopon-gatewayd/src/gateway.rs", "crates/dekopon-gatewayd/src/lib.rs", "crates/dekopon-gatewayd/src/wake/store.rs", "crates/dekopon-gatewayd/src/tests/tick_runner.rs"], "items": ["S-gateway-10", "S-gateway-11", "S-gateway-16", "S-gateway-17"], "rank": 3, "depends": ["S-reload-01", "S-reload-02", "S-reload-03", "S-reload-04", "S-reload-05", "S-reload-129"]}
```

Order:

1. `state/durable-store` builds first.
2. Then `state/cron-wakes`, `state/spend-slice1` and `daemon/notice-channel`.
3. Then `state/grant-settle`, after spend-slice1, and `rpi/tick-runner`, after cron-wakes.

`daemon/slack-mentions` landed in #973.

Beyond the supervisor, these files also appear in reload's later units:

| File | Gateway units | Reload units |
|---|---|---|
| `gateway.rs` | durable-store, notice-channel, tick-runner | watcher-core, broker-self-reload, reload-consumers |
| `lib.rs` | durable-store, notice-channel, tick-runner | broker-self-reload, reload-consumers |
| `config.rs` | durable-store, notice-channel, tick-runner | watcher-core, reload-consumers |
| `session.rs` | durable-store, spend-slice1 | reload-consumers |
| `proxy.rs` | grant-settle | reload-consumers |
| `tests.rs` | durable-store | broker-self-reload |

Those overlaps carry no dependency here. Order them if `footprint-edges.py` flags them.

## What each unit must hold

### state/durable-store

- **Boot order.** `DurableStore::acquire` runs under `offload` before any transport is built or
  agent activated. A second gateway gets `StoreError::Owned` at once, with no busy wait.
- **One writer.**
  - One `rusqlite::Connection` sits under a `parking_lot::Mutex`.
  - Every method blocks, so it runs under `offload`.
  - Owner: the holders of `Arc<DurableStore>`. Bound: one transaction per call.
  - A `commit` is one transaction over all its changes.
- **One owner of "due".** `WakeStore` keeps its in-memory rows and decides what is due. It writes
  through `commit` instead of rewriting `wakes.jsonl`. The durable store has no scheduling API.
- **Delivery tickets.**
  - A wake fires from `take_due`, `Tick::resolve` or a cron tick. The commit that fires it also
    sets the row's `pending` to the rendered text and notice.
  - A pending row is never due again, so a wake holds at most one pending delivery.
  - The session or orphan-notice task holds the `DeliveryTicket`. When that task returns,
    `WakeStore::acknowledge` commits the wake's change (remove or rearm), and that commit is the
    acknowledgement.
  - `WakeStore::restored` replays every pending row as a `Delivery`, and the duplicate is accepted.
  - The tick `JoinSet` yields `TickEnd`, and a panicking tick keeps its wake id.
- **Keys.** `ConversationKey::of` is the only constructor. It is injective over transport,
  container, id and thread. A `TransportId` has no `:`, so `transport()` parses the prefix back,
  and each restored `ThreadClaim` returns to its transport's `ThreadOwnership`.
- **Records.** Each conversation has one versioned JSON `Snapshot`, and the commit that empties a
  snapshot deletes it. A wait expires after thirty days; cron wakes are exempt.
- **Schema.**
  - One backup is written per old schema version, before migrating.
  - A failed migration is `MigrationFailed`, naming the backup.
  - A newer schema is `SchemaTooNew`.
  - Every record that cannot be upgraded is listed in one `Unupgradable` error.
- **Conversion.**
  - It runs as one transaction. Afterwards `wakes.jsonl` is renamed aside and never read again.
  - Follows come from the journal's markers.
  - A Slack thread ownership that left no persisted evidence is counted lost.
  - A corrupt legacy line is `LegacyWakes`.
  - `WakeStoreError` goes once nothing returns it.
- **Jobs.**
  - `JobStarted`, carrying the job's anchor, at admission; `JobEnded` at the end.
  - At boot, each interrupted job is reported once to its anchor with its deadline.
- **Fatal (S-gateway-09).** With supervisor landed, a store write failure on a serving path ends
  serving with its cause through a `ServeOutcome` cause arm. Owned tasks join within the shutdown
  grace, and the next boot resumes from the store.

### state/cron-wakes

- A cron wake is `WakeRowKind::Cron` and delivers its `note`.
- Missed ticks merge into one late delivery.
- It uses the delivery ticket, so a tick interrupted by a restart fires once after it.
- Left to the kickoff:
  - the wake tool's cron fields (`WakeRequest`);
  - the refusal for a bad expression (`WakeRefusal`);
  - the time zone (this skeleton assumes UTC).

### daemon/notice-channel

- It is opt-in. With no channel configured, nothing posts.
- **Exit marker.**
  - The marker records the `FatalLabel` of each fatal `ServeOutcome` (S-reload-01, plus note
    item 1).
  - It lives in the journal directory, outside the store, because the store can be the fatal
    cause.
  - `Owed` changes to `Posted` before the post, so a crash mid-post cannot loop.
  - Later exits in a streak only update the marker.
  - After the stability window the marker is deleted and one `Recovered` line posts.
- **Wake panics.** `WakePanics` lives in memory and holds only open streaks, fed by
  `TickEnd::Panicked`.
- Left to the kickoff: the config keys, the marker's file name, and the line texts.

### state/spend-slice1 (provisional until `designs/spend-holds-revision`)

- The ledger tables live in the same SQLite file and share its `SchemaVersion`.
- A hold opens before a model stream and settles after it.
- At boot, every hold still open is charged its full estimate.
- `spent` reads only in-window rows, indexed by agent and time.
- A settlement write failure is fatal (note item 1).

### state/grant-settle (provisional, same revision)

- Each VM exec grant opens one hold, so holds are bounded by concurrent VM exec admission.
- A tally that returns settles as `Spent`; one that never returns settles as `Unreturned`.

### rpi/tick-runner

- `RunnerEntry` is static config (11-D2): no credential and no broker policy.
- Its `CronSchedule` comes from cron-wakes.
- `run_cycle` applies effects through the broker as `principal`.
- Receipts go in batches of at most 20 KiB. An output of 8 KiB or more is recorded as `Oversize`
  with its SHA-256.
- A truncated or unparseable plan applies nothing and parks.
- A refused effect stops the cycle, keeping the receipts so far.
- A failed batch parks with its intent ids.
- Wake registration refuses the runner's principal.
- **Reader restarts (S-gateway-10).**
  - There is one `ReaderRestarts` per transport, owned by the reader-supervision loop in `lib.rs`.
  - It runs over `transport::reconnect_backoff()`, and a reconnect resets the backoff.
  - It returns `Stop` for a non-retryable error, such as a credential problem or
    `IdentityChanged`.
  - `read_transport` returns the transport with its problem, so the reader can restart.
  - A panicked reader is either rebuilt from config or fatal; the unit decides and says which.
- **Tick panics (S-gateway-11).**
  - `spawn_tick` maps `threads::Failure::Task(TaskOutcome::Panicked)` to `TickEnd::Panicked { wake }`.
  - The serve loop calls `record_tick_panic` once and keeps serving.
  - Ticks stay bounded by the session gate's `admit_probe`.

## Choices I made

Each line names the draft text it overrides.

1. Deleted `DurableStore::{load, next_due, take_due, acknowledge, expire}`. The wake store stays
   the one owner of what is due. Draft: "deliveries with acknowledgement tickets" in the store.
2. The pending delivery moved onto `WakeRow.pending`, replacing `Snapshot.deliveries`,
   `DeliveryId` and `Change::{Deliver, Acknowledged}`. It keeps the rendered text and notice, not a
   copy of the row.
3. `DeliveryAck` became `DeliveryTicket`, with private fields, issued only by the wake store. Draft:
   public fields and `PendingDelivery::handled(conversation)`.
4. Rows reuse `WakeId` and `JobId` through local serde adapters, plus `TransportId`, `ThreadClaim`
   (now serde, replacing `OwnershipRow`) and `FollowMarker`. Draft: `u32`, `u64`, `String`,
   `OwnershipRow`, `at_ms`/`until_ms`.
5. `JobRow` gained `anchor`, so an interrupted job can be reported.
6. Watch and digest wakes share `WatchRow`.
7. Cron `message` became the wake's `note`.
8. Deleted `SyncMode` and `StoreLocation`. Nothing checks them, and they imply an unnamed config
   key.
9. Deleted `ConversionInterrupted`, because conversion is one transaction. Added
   `MigrationFailed` and `LegacyWakes`. `SchemaTooNew` carries `SchemaVersion`s.
10. `BootReport.{migrated_at, backup}` became `migration: Option<Migrated>`.
11. Both `LegacyInputs` paths are optional.
12. `ConversationKey` gained `of` and `transport()`, and `Restored.ownership` carries the key.
13. `ReaderRestarts` dropped `consecutive`, because nothing checks it.
14. Exit marker:
    - `cause` plus `causes` became `causes`;
    - `notified: bool` became `MarkerNotice`;
    - `first_exit_ms` was deleted;
    - it has its own `MarkerError`;
    - `in_dir(&Path)` replaces `in_journal`.
15. `WakePanics` holds a `HashSet<WakeId>`.
16. `Destination` is shared by `NoticeChannel` and `RunnerEntry.packets`.
17. Spend:
    - amounts are a `SpendMicros` newtype;
    - `Hold` is a consumed `#[must_use]` ticket without `Clone`;
    - `HoldOutcome::{Settled, Crashed}` became `{Spent, Unreturned}`;
    - `charge_crashed_holds` and `spent(agent, since: SystemTime)` replace the draft's
      `settle_crashed_at_boot` and `spent_micros`.
18. Added `EpochSlug::parse`.
19. Merged main rather than rebasing.
20. Per the rulings, `supervise.rs` and `tests/supervisor.rs` are gone.
    - `FatalCause` is deleted. A fatal cause is S-reload-01's `ServeOutcome` plus durable-store's
      arm.
    - `FatalLabel` moved to `notice.rs`, which persists it.
    - `TickEnd` moved to `wake/delivery.rs`, which reshapes the tick.
    - `ReaderRestarts`, `ReaderVerdict` and `record_tick_panic` moved to a new `restart.rs` under
      tick-runner. I avoided the name `supervise` so it is not confused with reload's unit.
    - The 12 stubs moved: four to `tests/durable_store.rs` and eight to `tests/tick_runner.rs`.
21. Added a `depends` key to the footprint lines. The reload skeleton shows no syntax for a
    cross-skeleton dependency.

## Gates on `ad7712f`

Passed:

- `git diff --check` and `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- rustdoc `-D warnings` for `dekopon-gatewayd`
- `cargo deny --locked check` and `cargo machete`
- both dependency-boundary greps and `test_daemon_dependency_gates.py`
- the gatewayd clippy on this head merged with skeleton-reload `779e17b`
- `cargo check --release -p dekopon-gatewayd`, on the prior head `63e2ab5` only. It warns that
  `LivenessTarget::Fauxlack` is never constructed; that warning is already on main.

`cargo test -p dekopon-gatewayd --lib --all-features`, filtered to wakes and the six stub modules,
gave 30 passed, 63 ignored and 1 failed. The failing test is
`tests::wakes::a_failed_write_fires_nothing_and_leaves_the_wake_pending`: the container runs as
uid 0, which ignores the read-only mode the test sets. That test is untouched.

Not run:

- `cargo test --workspace`
- the provider Wasm lanes
- the ARM64 Wasmtime lane
- `footprint-edges.py`, which lives in campaign-trail, which this session cannot read
