# Skeleton: gateway (wakes, durable store, notices, grants, tick runner)

Step `06-designs/skeleton-gateway/design`, check round-1 repair. Branch `design/skeleton-gateway`,
PR dekopon-agents/dekopon#983, code head `f8ea7ee` (supersedes the kickoff's `477f866` and the
reviewed `053d9fa`). Base main `af0dcf4`, merged rather than rebased. This file is removed from the
branch once the coordinator fetches it.

The ids keep the draft catalog (`a31e3c29`, code `955ccd3`), which accepted sibling evidence cites.
Retired ids name their replacement; new items start at S-gateway-104. The step ids are the plan's
real ones.

## How units use it

- **Bodies and stubs.** Every body is `todo!()` under a site
  `#[expect(clippy::todo, reason = "skeleton: <unit>")]`, and each new module expects `dead_code`.
  Every lifecycle stub is `#[ignore = "skeleton: <unit>"]`. A unit fills its bodies, deletes the
  expectations that stop firing, and turns its stubs into tests.
- **Edits to existing files** are registrations only:
  - `mod` lines, `pub use store::{ConversationKey, StoreError}` and `GatewaydError::Store`;
  - `mod cron` and `mod delivery` in `wake.rs`;
  - test modules in `tests.rs`;
  - match arms in `gateway.rs` and `tests/gateway/fauxlack.rs`;
  - serde derives on `transport::ThreadClaim`;
  - in `dekopon-agent`, `pub mod grant` and the `BrokerLeg::with_grants` stub.
- **Dependencies:** `rusqlite` 0.43 (`bundled`) and `croner` 2.2.0.
- **With skeleton-reload.** Merged with `design/skeleton-reload` at `779e17b`, the tree merges
  without conflict and the gateway passes clippy.

## Order

Real After rows decide the order. The footprint block below is ranked so that `footprint-edges.py`
prints exactly those rows plus edges on shared hot files. The tool reads neither `depends` nor line
order, so this prose states the cross-skeleton order.

- `daemon/supervisor` (skeleton-reload, S-reload-01..05 and S-reload-129) lands first.
  `durable-store` builds after it.
- **Printed chain:** `grant-settle` (rank 1) → `durable-store` (2) → `cron-wakes` (3) →
  `tick-runner` (4) → `notice-channel` (5) → `spend-slice1` (6).

| Edge | Basis |
|---|---|
| grant-settle → durable-store | `session.rs`. Not a reversal: grant-settle has no After row on durable-store (C2: grant-settle first, against today's meter). |
| durable-store → cron-wakes | real row; `wake.rs`, `wake/store.rs` |
| cron-wakes → tick-runner | real row; `wake/store.rs` |
| tick-runner → notice-channel | `config.rs`, `gateway.rs` |
| notice-channel → spend-slice1 | `config.rs` |

- spend-slice1's other real rows hold transitively: after durable-store, and after grant-settle.
- tick-runner never waits on spend-slice1.
- **Merge candidates** (three or more shared hot files): durable-store with spend-slice1 (4) and
  with tick-runner (3). Both stay separate units, because they are different steps with different
  After rows.
- **Merged with skeleton-reload,** the single group's chain runs:
  1. supervisor
  2. grant-settle
  3. durable-store
  4. watcher-core
  5. broker-self-reload
  6. cron-wakes
  7. reload-consumers
  8. tick-runner
  9. notice-channel
  10. signer-reload
  11. spend-slice1

  This reverses none of the real After rows above, and reload's own train keeps its order. Links
  printed between units with no shared hot file are tool ordering, not dependencies.

I checked both runs with a local copy of the rule the coordinator stated. The real
`footprint-edges.py` was not run here.

```footprint
{"unit": "state/grant-settle", "step": "10-state/grant-settle/build", "files": ["crates/dekopon-agent/src/grant.rs", "crates/dekopon-agent/src/lib.rs", "crates/dekopon-gatewayd/src/session.rs", "crates/dekopon-gatewayd/src/tests/grant_holds.rs"], "rank": 1, "items": ["S-gateway-91", "S-gateway-92", "S-gateway-93", "S-gateway-111", "S-gateway-129", "S-gateway-138"]}
{"unit": "state/durable-store", "step": "10-state/durable-store/build", "files": ["crates/dekopon-gatewayd/src/store.rs", "crates/dekopon-gatewayd/src/store/lock.rs", "crates/dekopon-gatewayd/src/store/schema.rs", "crates/dekopon-gatewayd/src/store/record.rs", "crates/dekopon-gatewayd/src/store/convert.rs", "crates/dekopon-gatewayd/src/wake.rs", "crates/dekopon-gatewayd/src/wake/store.rs", "crates/dekopon-gatewayd/src/wake/delivery.rs", "crates/dekopon-gatewayd/src/gateway.rs", "crates/dekopon-gatewayd/src/lib.rs", "crates/dekopon-gatewayd/src/session.rs", "crates/dekopon-gatewayd/src/collection.rs", "crates/dekopon-gatewayd/src/follow.rs", "crates/dekopon-gatewayd/src/memory/journal.rs", "crates/dekopon-gatewayd/src/jobs.rs", "crates/dekopon-gatewayd/src/transport/slack.rs", "crates/dekopon-gatewayd/src/config.rs", "crates/dekopon-gatewayd/src/tests.rs", "crates/dekopon-gatewayd/src/tests/wakes.rs", "crates/dekopon-gatewayd/src/tests/attribution.rs", "crates/dekopon-gatewayd/src/tests/digests.rs", "crates/dekopon-gatewayd/src/tests/memory.rs", "crates/dekopon-gatewayd/src/tests/wake_reason.rs", "crates/dekopon-gatewayd/src/tests/jobs_e2e.rs", "crates/dekopon-gatewayd/src/tests/thread_budget.rs", "crates/dekopon-gatewayd/src/tests/subscriptions.rs", "crates/dekopon-gatewayd/src/tests/github_burst.rs", "crates/dekopon-gatewayd/src/tests/durable_store.rs", "crates/dekopon-gatewayd/tests/gateway.rs", "crates/dekopon-gatewayd/tests/gateway/fauxlack.rs", "crates/dekopon-gatewayd/tests/gateway/subscriptions.rs"], "rank": 2, "items": ["S-gateway-01", "S-gateway-02", "S-gateway-03", "S-gateway-04", "S-gateway-05", "S-gateway-10", "S-gateway-11", "S-gateway-12", "S-gateway-13", "S-gateway-14", "S-gateway-15", "S-gateway-16", "S-gateway-17", "S-gateway-18", "S-gateway-19", "S-gateway-20", "S-gateway-23", "S-gateway-25", "S-gateway-41", "S-gateway-42", "S-gateway-43", "S-gateway-44", "S-gateway-45", "S-gateway-46", "S-gateway-47", "S-gateway-48", "S-gateway-49", "S-gateway-50", "S-gateway-51", "S-gateway-52", "S-gateway-53", "S-gateway-54", "S-gateway-55", "S-gateway-56", "S-gateway-57", "S-gateway-58", "S-gateway-59", "S-gateway-60", "S-gateway-61", "S-gateway-62", "S-gateway-63", "S-gateway-72", "S-gateway-73", "S-gateway-74", "S-gateway-75", "S-gateway-104", "S-gateway-105", "S-gateway-106", "S-gateway-107", "S-gateway-108", "S-gateway-109", "S-gateway-113", "S-gateway-114", "S-gateway-115", "S-gateway-116", "S-gateway-117", "S-gateway-118", "S-gateway-119", "S-gateway-120", "S-gateway-121", "S-gateway-122", "S-gateway-123", "S-gateway-124", "S-gateway-125", "S-gateway-126"]}
{"unit": "state/cron-wakes", "step": "10-state/cron-wakes/build", "files": ["crates/dekopon-gatewayd/src/wake.rs", "crates/dekopon-gatewayd/src/wake/cron.rs", "crates/dekopon-gatewayd/src/wake/store.rs", "crates/dekopon-agent/src/wake.rs", "crates/dekopon-gatewayd/src/tests/cron_wakes.rs"], "rank": 3, "items": ["S-gateway-31", "S-gateway-32", "S-gateway-85", "S-gateway-86", "S-gateway-87", "S-gateway-88", "S-gateway-89", "S-gateway-90", "S-gateway-110"]}
{"unit": "rpi/tick-runner", "step": "11-rpi/tick-runner/build", "files": ["crates/dekopon-gatewayd/src/runner.rs", "crates/dekopon-gatewayd/src/runner/receipt.rs", "crates/dekopon-gatewayd/src/restart.rs", "crates/dekopon-gatewayd/src/transport.rs", "crates/dekopon-gatewayd/src/config.rs", "crates/dekopon-gatewayd/src/gateway.rs", "crates/dekopon-gatewayd/src/lib.rs", "crates/dekopon-gatewayd/src/wake/store.rs", "crates/dekopon-gatewayd/src/tests/tick_runner.rs"], "rank": 4, "items": ["S-gateway-22", "S-gateway-24", "S-gateway-34", "S-gateway-35", "S-gateway-36", "S-gateway-37", "S-gateway-38", "S-gateway-39", "S-gateway-40", "S-gateway-64", "S-gateway-65", "S-gateway-66", "S-gateway-67", "S-gateway-68", "S-gateway-69", "S-gateway-70", "S-gateway-71", "S-gateway-98", "S-gateway-99", "S-gateway-100", "S-gateway-101", "S-gateway-102", "S-gateway-103", "S-gateway-112", "S-gateway-131", "S-gateway-132", "S-gateway-133", "S-gateway-134", "S-gateway-135", "S-gateway-136"]}
{"unit": "daemon/notice-channel", "step": "09-daemon/notice-channel/build", "files": ["crates/dekopon-gatewayd/src/notice.rs", "crates/dekopon-gatewayd/src/gateway.rs", "crates/dekopon-gatewayd/src/lib.rs", "crates/dekopon-gatewayd/src/config.rs", "crates/dekopon-gatewayd/src/tests/notice_channel.rs"], "rank": 5, "items": ["S-gateway-21", "S-gateway-26", "S-gateway-27", "S-gateway-28", "S-gateway-29", "S-gateway-30", "S-gateway-76", "S-gateway-77", "S-gateway-78", "S-gateway-79", "S-gateway-80", "S-gateway-81", "S-gateway-82", "S-gateway-83", "S-gateway-84", "S-gateway-127", "S-gateway-128"]}
{"unit": "state/spend-slice1", "step": "10-state/spend-slice1/build", "files": ["crates/dekopon-agent/src/grant.rs", "crates/dekopon-gatewayd/src/store.rs", "crates/dekopon-gatewayd/src/store/ledger.rs", "crates/dekopon-gatewayd/src/store/schema.rs", "crates/dekopon-gatewayd/src/metering.rs", "crates/dekopon-gatewayd/src/meter_restore.rs", "crates/dekopon-gatewayd/src/session.rs", "crates/dekopon-gatewayd/src/lib.rs", "crates/dekopon-gatewayd/src/config.rs", "crates/dekopon-gatewayd/src/tests/spend_ledger.rs"], "rank": 6, "items": ["S-gateway-33", "S-gateway-94", "S-gateway-95", "S-gateway-96", "S-gateway-97", "S-gateway-130", "S-gateway-137", "S-gateway-139"]}
```

## Items

The Unit column is the owning unit. "kept" means the id names the same item, possibly reshaped.

| Id | Unit | Item (`crates/dekopon-gatewayd/src/…` unless named) | Status |
|---|---|---|---|
| S-gateway-01 | durable-store | `store.rs`: `StoreError` with `category`. Adds `MigrationFailed`, `LegacyWakes` and `SchemaTooNew(SchemaVersion)`; drops `ConversionInterrupted`. | kept |
| S-gateway-02 | durable-store | `store.rs`: `DurableStore { owner, connection, sync, fatal }` | kept |
| S-gateway-03 | durable-store | `DurableStore::acquire(&DurableSettings, &LegacyInputs) -> Acquired` | kept, reshaped |
| S-gateway-04 | durable-store | `DurableStore::restore` | kept |
| S-gateway-05 | durable-store | `DurableStore::commit -> Result<(), StoreFailed>` | kept, reshaped |
| S-gateway-06 | — | `load` | retired, replaced by 04 |
| S-gateway-07 | — | `next_due`/`take_due` | retired: the existing `WakeStore::{next_at, take_due}` owns what is due; see 108 |
| S-gateway-08 | — | `acknowledge`/`DeliveryAck` | retired, replaced by 107, 108 |
| S-gateway-09 | — | `expire` | retired: expiry runs in the wake store's due pass and in 04 |
| S-gateway-10 | durable-store | `store.rs`: `DurableSettings { path, sync }`, `SyncMode { Off, Normal, Full }` (default `Normal`; owner-ruled, IDEA-0140:62) | kept, `StoreLocation` renamed |
| S-gateway-11 | durable-store | `store/lock.rs`: `StoreOwner` | kept |
| S-gateway-12 | durable-store | `store/schema.rs`: `SchemaVersion::{CURRENT, read, migrate}` | kept |
| S-gateway-13 | durable-store | `store/convert.rs`: `LegacyInputs`, `Conversion`, `FollowConversion`, `convert` | kept |
| S-gateway-14 | durable-store | `store/record.rs`: `ConversationKey::{of, transport}`, `RecordVersion`, `Record`, `Snapshot` | kept |
| S-gateway-15 | durable-store | `store/record.rs`: `WakeRow`, `AnchorRow`, `WakeRowKind`, `WatchRow` | kept |
| S-gateway-16 | durable-store | `store/record.rs`: `FollowRow`, `JobRow`. `OwnershipRow` is replaced by `ThreadClaim`. | kept |
| S-gateway-17 | durable-store | `store/record.rs`: `Change` | kept |
| S-gateway-18 | durable-store | `store/record.rs`: `PendingDelivery` (on `WakeRow.pending`). `DeliveryId` retired, replaced by 107. | kept |
| S-gateway-19 | durable-store | `store.rs`: `Restored`, `BootReport`, `Migrated { from, backup, at }` | kept |
| S-gateway-20 | durable-store | the store's I/O task: see "Owners, bounds and shutdown" | kept |
| S-gateway-21 | notice-channel | `notice.rs`: `FatalLabel { Store, Reload, Transports }`. `FatalCause` retired, replaced by 105 and S-reload-01. | kept, moved |
| S-gateway-22 | tick-runner | `restart.rs`: `ReaderRestarts::{new, failed, connected}`, `ReaderVerdict` | kept, moved (ruling) |
| S-gateway-23 | durable-store | `wake/delivery.rs`: `TickEnd { wake, result }`, `TickResult`. `record_tick_panic` is now 112. | kept, split |
| S-gateway-24 | tick-runner | transport readers task | kept (ruling) |
| S-gateway-25 | durable-store | wake ticks task (`JoinSet<TickEnd>`) | kept |
| S-gateway-26 | notice-channel | `notice.rs`: `NoticeChannel::post`, `Destination` | kept |
| S-gateway-27 | notice-channel | `notice.rs`: `ExitMarker { streak, first_exit_ms, notice }`, `MarkerFile`, `MarkerNotice`, `MarkerError` | kept, reshaped |
| S-gateway-28 | notice-channel | `notice.rs`: `NoticeLine` | kept |
| S-gateway-29 | notice-channel | `notice.rs`: `WakePanics::{observe, retire}` | kept |
| S-gateway-30 | notice-channel | notice stability window | kept |
| S-gateway-31 | cron-wakes | `wake/cron.rs`: `CronSchedule`, `MissedTicks` | kept |
| S-gateway-32 | cron-wakes | `wake/cron.rs`: `CronRegistration`, `WakeStore::register_cron` | kept |
| S-gateway-33 | spend-slice1 | `store/ledger.rs`: `Ledger::{open_hold, settle, charge_crashed_holds, history}`, `Hold`, `HoldId`. `HoldOutcome` becomes `Settlement`, `Provenance` and `ChargeRow`. | kept, reshaped |
| S-gateway-34 | tick-runner | `runner.rs`: `GitSha`, `EpochSlug::parse`, `IntentId` | kept |
| S-gateway-35 | tick-runner | `runner.rs`: `RunnerEntry { principal: PrincipalId, agent, schedule, epoch, approved, packets, deadline }`, `RunnerEntry::attestation` | kept, reshaped |
| S-gateway-36 | tick-runner | `runner.rs`: `Effect`, `TickPlan` | kept |
| S-gateway-37 | tick-runner | `runner.rs`: `ParkReason`, `CycleOutcome`, `RunnerState` | kept |
| S-gateway-38 | tick-runner | `RunnerEntry::run_cycle` | kept |
| S-gateway-39 | tick-runner | `runner/receipt.rs`: `Receipt`, `ReceiptOutcome`, `ReceiptOutput` (adds `Binary`) | kept |
| S-gateway-40 | tick-runner | `runner/receipt.rs`: `ReceiptBatches::push -> Result<(), ReceiptError>`, `ReceiptError` | kept |
| S-gateway-41 | durable-store | `lib.rs`: `GatewaydError::Store` | kept |
| S-gateway-42..103 | per stub | the draft's lifecycle stubs; see Stubs | kept |
| S-gateway-104 | durable-store | `store.rs`: `Persistence::{at_boot, warning}`, `StoreWarning` | new (A4, A5) |
| S-gateway-105 | durable-store | `store.rs`: `FatalWatch::failed`, `StoreFailed`, `Raised`, `DurableStore::raise` | new (A10) |
| S-gateway-106 | durable-store | `DurableStore::first_served` | new (A6) |
| S-gateway-107 | durable-store | `wake/delivery.rs`: `DeliveryTicket::{issue, delivered}`, `Delivered { Wake, Job }` | new (A7, A9) |
| S-gateway-108 | durable-store | `WakeStore::settle`, `DeliveryDisposition`, `Dispositions::{with_delivery, take_delivery}` | new (A7) |
| S-gateway-109 | durable-store | `wake/delivery.rs`: `Delivery`, `JobNotice`, `Replay`, `WakeStore::restored` | new (A7, A9) |
| S-gateway-110 | cron-wakes | `wake/cron.rs`: `CronTick::resolve`, `CronResolution` | new (A18) |
| S-gateway-111 | grant-settle | `dekopon-agent/src/grant.rs`: `SessionGrants::{new, carve, tally, release, release_all}`, `GrantRefusal`; `BrokerLeg::with_grants` | new (A2) |
| S-gateway-112 | tick-runner | `restart.rs`: `record_tick_panic` | new, split from 23 |
| S-gateway-138 | grant-settle | `dekopon-agent/src/grant.rs`: `GrantLedger` (`carved`, `tallied`, `released`), `GrantRelease`, `GrantLedgerFailed`, `GrantRefusal::Ledger`. The carrier calls the hook when one is attached. | new (round-2 F1) |
| S-gateway-139 | spend-slice1 | `SessionGrants::with_ledger`; `store/ledger.rs`: `GrantRecorder` implements `GrantLedger` over the durable store | new (round-2 F1) |

## Stubs

| Id | Unit | File | Stub | Note |
|---|---|---|---|---|
| S-gateway-42 | state/durable-store | `durable_store.rs` | `a_crash_between_the_due_commit_and_handling_replays_the_delivery_at_boot` |  |
| S-gateway-43 | state/durable-store | `durable_store.rs` | `a_store_moved_to_a_new_path_restores_every_conversation` |  |
| S-gateway-44 | state/durable-store | `durable_store.rs` | `a_record_with_nothing_pending_is_deleted_and_a_wait_expires_after_thirty_days` |  |
| S-gateway-45 | state/durable-store | `durable_store.rs` | `a_wake_registered_before_a_restart_fires_once_from_its_row` |  |
| S-gateway-46 | state/durable-store | `durable_store.rs` | `a_contender_fails_at_once_and_activates_no_agent` |  |
| S-gateway-47 | state/durable-store | `durable_store.rs` | `after_the_owner_exits_a_new_instance_acquires_the_store_and_restores` |  |
| S-gateway-48 | state/durable-store | `durable_store.rs` | `one_connection_serializes_every_write_so_a_row_has_one_writer` |  |
| S-gateway-49 | state/durable-store | `durable_store.rs` | `ownership_is_observed_before_activation_on_a_store_that_needs_no_migration` |  |
| S-gateway-50 | state/durable-store | `durable_store.rs` | `an_interrupted_conversion_retries_and_converts_each_wake_once` |  |
| S-gateway-51 | state/durable-store | `durable_store.rs` | `a_converted_store_keeps_wakes_jsonl_as_converted_and_never_reads_it_again` |  |
| S-gateway-52 | state/durable-store | `durable_store.rs` | `conversion_streams_the_legacy_wake_file_line_by_line` |  |
| S-gateway-53 | state/durable-store | `durable_store.rs` | `thread_ownership_without_persisted_evidence_is_marked_lost_at_cutover` |  |
| S-gateway-113 | state/durable-store | `durable_store.rs` | `a_corrupt_legacy_wake_line_fails_boot_naming_the_line` | new |
| S-gateway-54 | state/durable-store | `durable_store.rs` | `a_failed_migration_keeps_the_backup_and_names_the_one_line_restore` |  |
| S-gateway-55 | state/durable-store | `durable_store.rs` | `a_newer_schema_or_an_unupgradable_record_fails_boot_listing_every_one` |  |
| S-gateway-56 | state/durable-store | `durable_store.rs` | `a_migration_writes_one_backup_per_old_schema_version` |  |
| S-gateway-57 | state/durable-store | `durable_store.rs` | `every_record_is_migrated_forward_at_boot` |  |
| S-gateway-58 | state/durable-store | `durable_store.rs` | `an_unacknowledged_delivery_replays_and_a_duplicate_is_accepted` |  |
| S-gateway-59 | state/durable-store | `durable_store.rs` | `an_acknowledgement_commits_in_the_same_transaction_as_its_wake_change` |  |
| S-gateway-60 | state/durable-store | `durable_store.rs` | `a_recurring_wake_holds_at_most_one_pending_delivery` |  |
| S-gateway-61 | state/durable-store | `durable_store.rs` | `an_interrupted_job_is_reported_with_its_deadline_within_one_boot` |  |
| S-gateway-62 | state/durable-store | `durable_store.rs` | `a_leave_drops_the_follow_and_the_thread_ownership_in_one_transaction` |  |
| S-gateway-63 | state/durable-store | `durable_store.rs` | `follows_and_thread_ownership_survive_a_restart` |  |
| S-gateway-72 | state/durable-store | `durable_store.rs` | `a_denied_wake_store_write_yields_one_cause_bearing_exit_and_a_nonzero_status` | moved from `supervisor.rs` (ruling) |
| S-gateway-73 | state/durable-store | `durable_store.rs` | `a_store_that_is_writable_again_after_the_restart_recovers` | moved from `supervisor.rs` (ruling) |
| S-gateway-74 | state/durable-store | `durable_store.rs` | `a_fatal_exit_joins_every_owned_task_within_the_shutdown_grace` | moved from `supervisor.rs` (ruling) |
| S-gateway-75 | state/durable-store | `durable_store.rs` | `a_restart_after_a_fatal_exit_resumes_from_the_store` | moved from `supervisor.rs` (ruling) |
| S-gateway-114 | state/durable-store | `durable_store.rs` | `the_store_is_acquired_once_at_initial_boot_and_never_by_check_or_reload` | new |
| S-gateway-115 | state/durable-store | `durable_store.rs` | `a_second_gateway_process_on_the_same_store_fails_at_once` | new |
| S-gateway-116 | state/durable-store | `durable_store.rs` | `conversion_reads_legacy_inputs_through_a_projected_symlink` | new |
| S-gateway-117 | state/durable-store | `durable_store.rs` | `legacy_follow_markers_are_retained_and_counted_never_keyed_by_guess` | new |
| S-gateway-118 | state/durable-store | `durable_store.rs` | `a_durable_off_gateway_keeps_waits_in_memory_and_warns_they_are_lost_on_restart` | new |
| S-gateway-119 | state/durable-store | `durable_store.rs` | `first_served_is_recorded_once_after_a_migration` | new |
| S-gateway-120 | state/durable-store | `durable_store.rs` | `every_serving_write_failure_raises_one_first_cause_fatal` | new |
| S-gateway-121 | state/durable-store | `durable_store.rs` | `a_queued_wake_turn_settles_its_ticket_only_when_the_turn_completes` | new |
| S-gateway-122 | state/durable-store | `durable_store.rs` | `an_orphaned_or_rejected_wake_delivery_settles_by_its_disposition` | new |
| S-gateway-123 | state/durable-store | `durable_store.rs` | `a_restored_delivery_is_admitted_when_serving_starts` | new |
| S-gateway-124 | state/durable-store | `durable_store.rs` | `a_subscription_synchronize_while_pending_keeps_the_pending_delivery` | new |
| S-gateway-125 | state/durable-store | `durable_store.rs` | `an_interrupted_job_notice_replays_after_a_crash_before_its_acknowledgement` | new |
| S-gateway-126 | state/durable-store | `durable_store.rs` | `a_tick_result_keeps_its_wake_id_on_every_outcome` | new |
| S-gateway-76 | daemon/notice-channel | `notice_channel.rs` | `a_crash_mid_post_cannot_loop_because_notified_commits_before_the_post` |  |
| S-gateway-77 | daemon/notice-channel | `notice_channel.rs` | `the_marker_is_deleted_after_the_stability_window_with_one_recovery_line` |  |
| S-gateway-78 | daemon/notice-channel | `notice_channel.rs` | `further_fatal_exits_in_a_streak_only_update_the_marker` |  |
| S-gateway-79 | daemon/notice-channel | `notice_channel.rs` | `several_fatal_exits_across_restarts_post_once_and_recover_once` |  |
| S-gateway-80 | daemon/notice-channel | `notice_channel.rs` | `a_wake_panicking_on_every_tick_posts_once` |  |
| S-gateway-81 | daemon/notice-channel | `notice_channel.rs` | `a_wake_s_next_success_after_a_panic_posts_one_recovery_line` |  |
| S-gateway-82 | daemon/notice-channel | `notice_channel.rs` | `wake_panic_state_holds_only_open_streaks` |  |
| S-gateway-83 | daemon/notice-channel | `notice_channel.rs` | `wake_panic_state_is_in_memory_so_a_restart_can_repeat_one_post` |  |
| S-gateway-84 | daemon/notice-channel | `notice_channel.rs` | `a_gateway_without_a_notice_channel_posts_nothing` |  |
| S-gateway-127 | daemon/notice-channel | `notice_channel.rs` | `a_retired_wake_leaves_no_panic_streak` | new |
| S-gateway-128 | daemon/notice-channel | `notice_channel.rs` | `a_lost_transport_exit_writes_the_marker` | new |
| S-gateway-85 | state/cron-wakes | `cron_wakes.rs` | `a_cron_tick_interrupted_before_acknowledgement_fires_once_after_restart` |  |
| S-gateway-86 | state/cron-wakes | `cron_wakes.rs` | `cancelling_a_cron_registration_ends_the_recurring_wake` |  |
| S-gateway-87 | state/cron-wakes | `cron_wakes.rs` | `cron_ticks_missed_while_down_merge_into_one_late_delivery` |  |
| S-gateway-88 | state/cron-wakes | `cron_wakes.rs` | `a_registered_cron_wake_survives_a_restart_and_fires_with_its_message` |  |
| S-gateway-89 | state/cron-wakes | `cron_wakes.rs` | `an_empty_or_rule_resolved_cron_tick_invokes_no_model` |  |
| S-gateway-90 | state/cron-wakes | `cron_wakes.rs` | `a_cron_wake_is_exempt_from_the_thirty_day_expiry` |  |
| S-gateway-91 | state/grant-settle | `grant_holds.rs` | `a_grant_whose_tally_never_returns_is_charged_in_full` |  |
| S-gateway-92 | state/grant-settle | `grant_holds.rs` | `a_returned_tally_settles_the_hold_and_refunds_the_rest` |  |
| S-gateway-93 | state/grant-settle | `grant_holds.rs` | `a_grant_is_kept_across_calls_and_released_with_its_turn` | renamed from `open_holds_are_bounded_by_concurrent_vm_exec_calls` |
| S-gateway-129 | state/grant-settle | `grant_holds.rs` | `a_carve_the_budget_cannot_cover_is_refused_before_the_vm_runs` | new |
| S-gateway-94 | state/spend-slice1 | `spend_ledger.rs` | `a_sigkill_mid_stream_charges_the_open_hold_as_crashed_at_boot` |  |
| S-gateway-95 | state/spend-slice1 | `spend_ledger.rs` | `a_failed_settlement_write_fails_fast_like_a_wake_store_failure` |  |
| S-gateway-96 | state/spend-slice1 | `spend_ledger.rs` | `spend_outside_the_budget_window_is_not_rescanned_per_call` |  |
| S-gateway-97 | state/spend-slice1 | `spend_ledger.rs` | `a_budget_survives_a_restart_and_refuses_an_over_budget_call_with_its_reset` |  |
| S-gateway-130 | state/spend-slice1 | `spend_ledger.rs` | `the_ledger_restores_timed_model_charges_including_crashed_holds` | new |
| S-gateway-98 | rpi/tick-runner | `tick_runner.rs` | `a_refused_effect_stops_the_cycle_and_records_the_receipts_so_far` |  |
| S-gateway-99 | rpi/tick-runner | `tick_runner.rs` | `a_truncated_tick_plan_applies_nothing_and_parks` |  |
| S-gateway-100 | rpi/tick-runner | `tick_runner.rs` | `serialized_receipt_batches_stay_within_twenty_kib_and_outputs_past_the_threshold_are_oversize` | renamed from `receipt_batches_stay_within_twenty_kib_and_outputs_of_eight_kib_are_oversize`; the boundary waits on stop 3 |
| S-gateway-101 | rpi/tick-runner | `tick_runner.rs` | `a_runner_cycle_resumes_on_schedule_across_a_gateway_restart` |  |
| S-gateway-102 | rpi/tick-runner | `tick_runner.rs` | `a_failed_receipt_batch_parks_with_its_intent_ids` |  |
| S-gateway-103 | rpi/tick-runner | `tick_runner.rs` | `an_agent_cannot_register_a_wake_for_the_rpi_coordinator_principal` |  |
| S-gateway-64 | rpi/tick-runner | `tick_runner.rs` | `a_reader_that_fails_mid_frame_restarts_after_its_backoff` | moved from `supervisor.rs` (ruling) |
| S-gateway-65 | rpi/tick-runner | `tick_runner.rs` | `a_reconnected_reader_resets_its_backoff` | moved from `supervisor.rs` (ruling) |
| S-gateway-66 | rpi/tick-runner | `tick_runner.rs` | `a_reader_restart_delay_never_exceeds_the_backoff_cap` | moved from `supervisor.rs` (ruling) |
| S-gateway-67 | rpi/tick-runner | `tick_runner.rs` | `readers_restart_through_the_backoff_while_the_gateway_serves` | moved from `supervisor.rs` (ruling) |
| S-gateway-68 | rpi/tick-runner | `tick_runner.rs` | `a_wake_tick_panic_emits_one_cause_bearing_event_and_the_gateway_serves_on` | moved from `supervisor.rs` (ruling) |
| S-gateway-69 | rpi/tick-runner | `tick_runner.rs` | `a_reload_during_a_tick_keeps_the_tick_and_its_wake` | moved from `supervisor.rs` (ruling) |
| S-gateway-70 | rpi/tick-runner | `tick_runner.rs` | `ticks_in_flight_are_bounded_by_the_session_gate` | moved from `supervisor.rs` (ruling) |
| S-gateway-71 | rpi/tick-runner | `tick_runner.rs` | `a_tick_that_panicked_before_a_restart_runs_again_from_its_row` | moved from `supervisor.rs` (ruling) |
| S-gateway-131 | rpi/tick-runner | `tick_runner.rs` | `a_non_utf8_output_is_recorded_by_digest_unaltered` | new |
| S-gateway-132 | rpi/tick-runner | `tick_runner.rs` | `a_receipt_that_cannot_fit_a_batch_parks_the_cycle` | new |
| S-gateway-133 | rpi/tick-runner | `tick_runner.rs` | `a_cycle_interrupted_mid_batch_parks_with_its_receipts_so_far` | new |
| S-gateway-134 | rpi/tick-runner | `tick_runner.rs` | `a_cron_tick_while_a_cycle_runs_is_skipped_and_logged` | new |
| S-gateway-135 | rpi/tick-runner | `tick_runner.rs` | `a_cycle_past_its_deadline_parks` | new |
| S-gateway-136 | rpi/tick-runner | `tick_runner.rs` | `the_runner_attests_as_its_configured_principal_and_agent` | new |
| S-gateway-137 | state/spend-slice1 | `spend_ledger.rs` | `grant_carves_tallies_and_releases_are_recorded_in_the_gateway_ledger` | new (round-2 F1) |

There are 87 stubs: the 62 draft stubs (two renamed, twelve moved by ruling) and 25 new ones.

## Owners, bounds and shutdown

| Primitive | Owner | Bound and full policy | Shutdown |
|---|---|---|---|
| Store connection (S-gateway-20) | Holders of `Arc<DurableStore>`: the wake store, follow table, ownership write-through, jobs, ledger and serve. | One connection under a `parking_lot::Mutex`, one transaction per call. Every call runs under `offload`, inside the caller's existing admission (session gate permit or tick admission). | The last `Arc` drops after serve joins ticks and sessions. After a raise, no commit succeeds. |
| Fatal signal (105) | Created by the store; serve's `select!` holds the `FatalWatch`. | The first cause wins (`Raised::First`); later failures return `StoreFailed` only. | Serve ends with the cause through a `ServeOutcome` cause arm beside S-reload-01's, then the existing grace-bounded join. |
| Delivery tickets (107–109) | The message's `Dispositions`, carried through `Admitted`, `FollowUp` (queued), `Full` and `Saturated` to the turn's end. | One pending delivery per wake or job row. Restored deliveries are admitted at serve start through the same gate. | A busy result settles as `Rejected` and is offered again on the next due pass. An aborted turn leaves the row pending, and it replays at the next boot. |
| Wake ticks (25) | Serve's `JoinSet<TickEnd>`. | `admit_probe` plus `thread_owner.reserve`. A busy tick is skipped and logged, as today. | Joined within the grace. A late tick's delivery stays pending. |
| Readers (24, 22) | `run_with_service`'s reader `JoinSet`, one task per transport. | Restart delay comes from `reconnect_backoff()`, capped. A non-retryable error stops the reader. | `abort_all`, then join (existing). |
| Notice stability window (30) | One more `Option<Instant>` deadline in serve's `select!`; no task. | One window per boot. Posts go into serve's owned `sessions` set, at most one per streak transition, best effort. | Dropped with serve. A post cut off at the grace is abandoned; the marker's `notice` was already written. |
| Exit marker write (27) | `run_with_service`, after serve returns a fatal outcome. | One offloaded write. | Done before the process exits. |
| Runner cycles (38) | Serve; one slot per entry in a `JoinSet`. | One cycle in flight per entry: a cron tick while `Running` is skipped and logged. Each cycle runs under `timeout(entry.deadline)`, and a timeout parks with `DeadlineExceeded`. | Joined within the grace, else aborted. Shutdown parks the entry with `Interrupted { intents }` and keeps the receipts so far. |
| Parked entries (37) | Serve's in-memory `RunnerState`. | Park posts one packet line, then the entry skips ticks. | Cleared by a restart or by a reload that changes `approved`. No durable runner row. |
| Grants (111) | The turn's `SessionGrants`, attached to its `BrokerLeg`. | One grant per VM session, up to a per-turn maximum (`GrantRefusal::Full`). | `release_all` at the turn's end. A tally that never returned is charged in full. |
| Ledger holds (33) | The model call's admission. | One hold per admitted stream. | A crash leaves the hold open; `charge_crashed_holds` charges it at the next boot. |

## What each unit must hold

### state/grant-settle (10-state/grant-settle/build; rank 1)

- **Seam.** The carve runs at the provider leg: `BrokerLeg::run_call` (`dekopon-agent/src/lib.rs`).
  The turn's `SessionGrants` is built where the leg is built (`session.rs`, at `leg.with_progress`)
  and attached with `with_grants`.
- **Carve.** It reserves against today's token meter (`Metering`/`Admission`) for one `VmSession`.
  The grant stays open across that session's later calls. Tallies settle it, and `release`/
  `release_all` close it. A tally that never returns is charged in full. A carve the budget cannot
  cover is refused before the VM runs.
- **Ledger adoption** of these holds is spend-slice1's.
- **Declared missing dependency (stop):** the broker's grant-required response and tally return.
  `RunOutcome` has no such variant. skeleton-broker owns that surface; do not invent it here.

### state/durable-store (10-state/durable-store/build; after daemon/supervisor)

- **Boot.**
  - `Persistence::at_boot` runs after config resolution and before preparation (`credentials`,
    then `prepare`), under `offload`.
  - It runs only at initial boot: `active.is_none()` and `LoadMode::Boot`. `check` and reload
    never acquire.
  - The `Acquired` value and its `FatalWatch` are carried into serving.
- **Keys.**
  - The new store path key is `sessions.durable.path`, with the ruled `sync` beside it.
  - Legacy inputs are derived from `sessions.wakes.path` and the journal directory.
  - The wake bounds stay under `sessions.wakes`.
- **Contention.** A second process gets `StoreError::Owned` at once (`StoreOwner`, exclusive lock).
- **Restore window (IDEA-0140:105-106).**
  - The boot report states the migration time (`Migrated.at`).
  - `first_served` records the first served call, not process start.
  - Restore applies only before that point; after it, repair is forward.
- **Delivery.**
  - A firing commits `pending` in the same transaction that computes it. Firings come from a once
    wake, a watch resolve, a cron tick and a subscription synchronize.
  - A pending row is never due again.
  - A synchronize while pending keeps the pending delivery.
  - The ticket rides in `Dispositions` to the turn's actual end, and settles as `Handled`,
    `Orphaned` (after its notice) or `Rejected` (offered again).
- **Ticks.** Every `TickEnd` carries its wake id, and durable-store produces `TickResult::Panicked`
  in `spawn_tick` (F4). tick-runner only observes it.
- **Jobs (A9).** `JobRow` stays until its interrupted-job notice is settled; that settlement
  commits `JobEnded`. A crash before settlement replays the notice.
- **Durable-off (owner-ruled, IDEA-0140:72).** Without `sessions.durable`, `Persistence::Memory`
  holds waits in memory, and `StoreWarning::WaitsLostOnRestart` warns that a restart loses them.
- **Retention (owner-ruled, IDEA-0140:86).**
  - A wait expires after thirty days and the agent is told.
  - A record is deleted once nothing in it is pending.
  - A recurring wake ends when its registration is removed, so the thirty days do not apply to it.
- **Fatal (A10).** Every serving write failure raises through the store's `FatalWatch`.

### state/cron-wakes (10-state/cron-wakes/build; after durable-store)

- A cron wake is `WakeRowKind::Cron` and delivers its `note`.
- Missed ticks merge into one late delivery.
- `CronTick::resolve` settles an empty or rule-resolved tick in code before any model runs (A18).
- The exhaustive `WakeRequest` dispatch is in `wake.rs`.
- The request and refusal spelling and the time zone stay with the kickoff.

### rpi/tick-runner (11-rpi/tick-runner/build; after cron-wakes)

- **Entry.** `RunnerEntry` is static config (11-D2), resolved to `principal: PrincipalId` plus
  `agent: AgentId`.
- **Attestation.** `attestation()` is stubbed. The broker-side attestation for principal
  `rpi-coordinator` is a declared missing stop owned by the broker design: broker-protocol has no
  attestation path for a service principal today.
- **Verify against 11-D2..D4:** the configured approved SHA.
- **Threshold:** owner stop 3. The skeleton does not decide whether an output of exactly
  8,192 bytes is inline or oversize.
- **Receipts.**
  - Size is measured on the serialized form, and a batch stays within 20 KiB.
  - Output that is not UTF-8 is recorded by digest, never altered.
  - A receipt that cannot fit parks the cycle with `Unrepresentable`.
- **Readers (22, 24).** Reader restarts run over `transport::reconnect_backoff()`, which this unit
  makes `pub(crate)` in `transport.rs`.
- **Tick panics (112).** The tick-panic event is observed and logged once from `TickEnd`, and
  serving continues.

### daemon/notice-channel (09-daemon/notice-channel/build; after standin-streak and supervisor)

- **Streak state.**
  - `ExitMarker` keeps the streak's state in the marker file, with `first_exit_ms` and the
    `Owed → Posted` notice (IDEA-0155:111).
  - `WakePanics` keeps one state per wake in memory, fed by `TickEnd`, and `retire` drops a
    cancelled, expired or gone wake.
- **The streak type is declared missing.** One reusable core type does open/repeat/close (the
  `dekopon_core::standin` placeholder), and `daemon/standin-streak` defines it with its own tests.
  `StandinState` is an uninhabited placeholder until then, so neither caller can grow a second
  streak algorithm.
- **Stability window.** It starts after Ready (10 minutes) and posts one recovery line with the
  restart count and causes.

### state/spend-slice1 (10-state/spend-slice1/build; after durable-store, grant-settle, spend-holds-revision, durable-deploy/pin)

- **Units.** The ledger counts tokens, with the call's agent and model and a typed `Provenance`.
- **History.**
  - `history(since)` returns timed, model-aware rows, crashed holds included.
  - The budget restore reads it instead of `meter_restore.rs`.
  - Out-of-window rows are never rescanned.
- **Adoption.** It adopts grant-settle's meter-backed holds through the `GrantLedger` hook
  (S-gateway-138): `SessionGrants::with_ledger(GrantRecorder)` records each carve, tally and
  release in the gateway ledger. The storage stays in `gatewayd`; `dekopon-agent` names only the
  trait.
- **Fatal.** A settlement write failure is fatal through 105.
- **Out of scope:** no slice2 prices or caps.

## Owner stops

Each names its question; none is decided here.

1. **Legacy follows and ownership (A8, W1-05).** `.followed` markers hold only a route-and-
   conversation hash and two timestamps, so neither a `ConversationKey` nor a `ThreadClaim` can
   be recovered. Conversion counts and keeps them (`FollowConversion::Retained`); nothing is
   guessed or dropped. Should the store keep follows by hash (lossless), or adopt a loss policy?
   Thread ownership was never persisted, so it starts empty at cutover. Approve?
2. **Fatal-exit scope (A19, G2-10, F16).** IDEA-0155:111 says a fatal exit writes the marker.
   The skeleton carries `Store`, `Reload` and `Transports` (lost transports and terminally stopped
   readers both exit non-zero today). Does every non-zero exit write it?
3. **Receipt threshold (A16, round-2 F2).** PLAN.md:847 says outputs of "8 KiB or more" are
   oversize; 11-rpi/DECISIONS.md:29 says "over" 8 KiB. Is an output of exactly 8,192 bytes inline
   or oversize? The stub (S-gateway-100) and `ReceiptOutput::of` stay neutral until this is
   answered.

## Declared missing dependencies

| Need | Owner |
|---|---|
| The broker's grant-required response and tally return | skeleton-broker |
| A broker attestation for principal `rpi-coordinator` | broker design |
| The `standin` type | 09-daemon/standin-streak |
| 11-D2..D4 check: the approved SHA config | to verify |

## Not real

The adjudicator found W1-04, W1-06, W1-07/F14, G2-5 and F23 not real; no work was spent on them.

## Choices I made

1. Kept the draft ids 01–103, with retirements naming their replacement. New ids start at 104.
2. Ranks are G1, D2, C3, T4, N5, S6. Durable-store after grant-settle is a `session.rs` edge, not
   a reversal. Notice-channel after tick-runner (`config.rs`, `gateway.rs`) keeps tick-runner off
   spend-slice1.
3. Grant-settle's carrier `SessionGrants` is a concrete type in `dekopon-agent`, next to
   `BrokerLeg`, which already depends on the token governor. Only its persistence is a trait,
   `GrantLedger` (round-2 F1), because `dekopon-agent` cannot name the gateway's store. The trait
   follows the existing cross-crate seams `ProgressSink` and `JobControl`, which the gateway also
   implements.
4. One `DeliveryTicket` type covers wakes and interrupted jobs. It rides in the existing
   `Dispositions`, the per-message completion carrier, instead of a new channel.
5. A serving write failure returns the `Copy` marker `StoreFailed` to its caller. The cause
   travels once, through `FatalWatch`, so every write path feeds the fatal signal by construction.
6. Follows stay in the per-conversation `Snapshot` at run time; legacy conversion is the owner
   stop.
7. `ReceiptOutput::Binary` adds `{"binary": true, "bytes", "sha256"}` beside `Oversize`. That is a
   receipt wire shape for the kickoff to confirm.
8. `FatalLabel::Transports` is carried so the broad reading of IDEA-0155 compiles. The scope is
   owner stop 2.
9. `StandinState` is an uninhabited enum, so the notice callers cannot be filled before the core
   type lands.
10. Two draft stubs were renamed, with their ids kept: `open_holds_are_bounded_by_concurrent_vm_exec_calls`,
    because a grant is retained across calls (A2), and the receipt-batch stub, which now reads
    "serialized" and "past the threshold" so it picks no side of stop 3 (A16, round-2 F2).
11. Durable-off is no longer a stop: IDEA-0140:72 rules it, and the round-1 carriers already
    implement it (round-2 N3).

## Gates on `f8ea7ee`

Passed:

- `git diff --check` and `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`
- rustdoc `-D warnings` for `dekopon-gatewayd` and `dekopon-agent`
- `cargo deny --locked check` and `cargo machete`
- `test_daemon_dependency_gates.py`
- `cargo test -p dekopon-agent --lib`: 203 passed

`cargo test -p dekopon-gatewayd --lib --all-features`, filtered to wakes and the six stub modules,
gave 30 passed, 87 ignored and 1 failed. The failure is
`tests::wakes::a_failed_write_fires_nothing_and_leaves_the_wake_pending`; the container runs as
uid 0, which ignores the read-only mode the test sets.

Not run: `cargo test --workspace`, the Wasm and ARM64 lanes, and the real `footprint-edges.py`
(emulated from the coordinator's stated rule; the chain above is unchanged by round 2).
