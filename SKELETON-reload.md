# skeleton-reload: design skeleton (round 4)

Step `06-designs/skeleton-reload/design`. Branch `design/skeleton-reload`, PR dekopon-agents/dekopon#982,
on main `af0dcf4`. The PR is never merged as is: units copy from it or stack on it.

Inputs: the medium draft, the dekopon code, the round 1 and round 2 adjudications, and the template
formats and PLAN.md:811 text the coordinator relayed. campaign-trail itself was not readable in this session,
so the kickoff and IDEA-0154 were read only through that relay.

## What the mechanism replaces

| Today | Replaced by |
| --- | --- |
| gatewayd `ReloadState::run`: a 2 s interval re-reading config, catalog and CA bundles | `Watcher<GatewayGroup>`, `GatewayGroup::Config` |
| gatewayd driving brokerd through `PrepareGeneration`/`CommitGeneration` on every tick | brokerd's own `Watcher<BrokerGroup>`; the broker publishes its digest at connect and each commit (D1) |
| brokerd `lock_watch::watch`: a 2 s poll of `providers.lock.yaml` | `BrokerGroup::Lock`, verified with the existing `Observer` |
| model-proxy `Reloading::acceptor`: three `stat`s under a mutex on every accept | `TlsReload` prepare and commit through `GatewayGroup::ProxyTls` |
| refresh scheduler noticing a new generation at its next 5 min tick | `serve_on_commits`, woken by the commit signal |
| a panicking gateway reload task, never joined, so reloads silently stop | `ServeOutcome::{ReloadStopped, ReloadPanicked}`, joined by `serve` |

## Owner decisions

**D1, the generation exchange. Recommend: delete prepare, commit and abort; keep the ruled
publication.** PLAN.md:811 (IDEA-0154 R4): each daemon reloads its own inputs; the broker publishes
its digest on the existing socket at connect and on each commit; the gateway folds it into its
serving digest without calling prepare; a brief mismatch fails closed at Cedar.

Before (gateway reload, every 2 s):
```
gateway: load candidate -> PrepareGeneration{active} -> broker loads, holds `prepared`
gateway: CommitGeneration{next} -> broker swaps -> gateway swaps routes
```
After:
```
broker:  notify(own inputs) -> load -> swap -> CommitSignal -> Generation{digest} frame to the observer
gateway: ObserveGeneration held open -> fold each digest; notify(own inputs) -> load -> swap
```
Stays, because the publication needs it: `BrokerRequest::ObserveGeneration`,
`BrokerResponse::Generation`, `GenerationOutcome::{Active, Refused}`, `ProtocolVersion::V1Alpha3`
(the version that carries those frames), `RECONCILIATION_OWNER` and
`MappedPeer::is_reconciliation_owner` (only the gateway may observe). C1, the wire shape, is
drawn as: `BrokerClient::observe_generations` sends `ObserveGeneration` and keeps the connection
open (`GenerationFeed::next`); the broker's `serve_observer` writes one `Generation` frame at
connect and one per commit; no new frame. Observers live in their own set and `drain_observers`
ends them at admission close, so a broker stop with a gateway attached does not end in
`ShutdownTimeout`. A lost feed does not stop the gateway: `follow` reconnects with bounded backoff,
as the `Unsettled::Monitor` retry does today, and serving continues on the last published digest
until a mismatch fails closed at Cedar.

Deleted in `daemon/broker-self-reload`: `BrokerRequest::{Prepare,Commit,Abort}Generation`,
`GenerationOutcome::{Prepared, Applied, Aborted}`, `ProtocolError::InvalidGenerationDigest` and
`valid_generation_digest` (only those requests carry a digest), `BrokerClient::{prepare,commit,abort}_generation`;
brokerd `Coordination::{prepare, commit, abort}`, `GenerationState.prepared` and the `loading`
semaphore (the watcher's `&mut` serializes reloads); gatewayd `ReloadState.pending` and
`Unsettled::{Monitor, Refused}` (the Monitor retry moves into `follow`). The 28 prepare/commit cases in `brokerd/src/tests.rs` are ported to
`Coordination::reload` and the publication, not dropped.

Skew: a gateway route can name an agent the broker's active generation lacks for as long as the
broker refuses its candidate, not only one debounce; each such turn fails closed at Cedar
(S-reload test `a_route_naming_an_agent_the_broker_refused_is_refused_until_the_broker_applies`).
In `mode: signaled`, send SIGHUP to the broker first, then the gateway. Under the chart the broker's
files are copies `prepare-files` writes into an `emptyDir`, so the broker's config only changes at
pod start; the gateway is the side that reloads live there.

**D2, signer reload. Coordinator default: deferred, owner's call (A8).** In the chart the
`signedJwt` key (#979) is a copied private file, so a renewed key arrives only by restart and the
JWKS gate never runs. `daemon/signer-reload` stays blocked on #979 and D2 and carries its dormant
stubs. If the owner wants live gated renewal: the key comes from a projected Secret read in place,
it leaves the config fingerprint so exactly one group owns it, and `kid` becomes the RFC 7638
thumbprint (deleting `signedJwt.kid`), all before #979 merges.

## Ruled contract

`reload:` in both daemons' config: `mode: aggressive | signaled` (any case), `debounceMs`,
`activationTimeoutMs` (`NonZeroU32` milliseconds; the ruled activation bound is 30 s), unknown keys
refused, a restart category in both daemons. The skeleton leaves defaults `todo!()` and adds the key
to neither config struct: a `#[serde(default)]` field would call a `todo!()` `Default` in every
existing config test. No chart value or mount changes: the gateway's config, catalog and skills are
ConfigMap projections and the proxy TLS is a Secret volume, all watchable as `Projection`s.

## Units and train

Every body is `todo!()` under `#[expect(clippy::todo, …, reason = "skeleton: <unit>")]`; unused
stubs carry `#[expect(dead_code, …)]`; every test is `#[ignore = "skeleton: <unit>"]`. Every `mod`
line, `pub use`, dependency and test module is already in this PR, so units never edit them.

Train: supervisor → watcher-core → broker-self-reload → reload-consumers → signer-reload.
broker-self-reload and reload-consumers may build in parallel and land in rank order; signer-reload
waits for #979 and D2.

- **`daemon/supervisor`** (rank 1, 6 items, 5 tests). Lands before any watcher can exit
  fatally. Gateway: `serve`'s `reloading` set becomes `JoinSet<GatewaydError>` and joins the
  `select!` through `reload_ended`; an `Exit` is `ServeOutcome::ReloadStopped`, a panic
  `ReloadPanicked`; `run_with_service` already maps both to `Err` at this head. Broker: `serve` owns
  one `JoinSet<BrokerStop>` holding the existing lock watch, joined through `reload_ended`; a lock
  change stays `BrokerStop::LockChanged`, a clean exit, so `brokerd/src/tests.rs`'s lock tests keep
  expecting `Ok`; broker-self-reload later replaces that task with the watcher. Shutdown joins the
  task in both.

  `daemon/supervisor` is owned by this skeleton: it spans both daemons and carries the reload task's
  join that every reload unit needs first. A skeleton that also needs it (skeleton-gateway) drops its
  footprint line and declares a dependency on S-reload-01..05 and S-reload-129 instead.
- **`daemon/watcher-core`** (rank 2, 17 items, 38 tests). The core module, the shared
  projection predicate (and its two moved callers), and the gateway's Config group replacing the 2 s
  tick.
- **`daemon/broker-self-reload`** (rank 3, 14 items, 29 tests). D1 (including the
  deleted variants' match arms in `dekopon-agent` and gatewayd's tests), the broker's groups, the
  held-open publication on both sides, `serve_with_watcher` (the watcher factory that replaces
  `lock_poll`, so daemon tests get an `Injector`), the commit-woken refresh scheduler, the lock group, the
  secret map's key paths in the Config watch set, and `docs/deploy.md` ("While the gateway is
  connected, the broker reloads…" loses its condition).
- **`daemon/reload-consumers`** (rank 4, 8 items, 15 tests). Restart categories as
  `Category`, the swappable follow table, the proxy TLS group.
- **`daemon/signer-reload`** (rank 5, 1 item, 5 tests). Dormant.

## Must-holds

1. One owner per watched path. The gateway never watches a broker-private path (its 0700 directory
   also refuses `inotify_add_watch`); the broker never watches the credential files it writes; a
   `File` entry registers its directory but covers only its own name.
2. A group changes what it serves only after its last await (the comment on `Reconcile::reconcile`).
   TLS: `TlsSource::prepare` is offloaded and mutates nothing; `TlsReload::commit` runs after the
   final await.
3. The watcher registers the union of every slot's registrations, canonical directories, and matches
   an event after canonicalizing its parent (FSEvents reports `/private/var/...` for `/var/...`).
4. One projection predicate: `dekopon_core::projection::root` (unix, outside the `reload` feature).
   `WatchedPath`, `dekopon_config::Projection::is_claimed` and brokerd `secrets.rs` call it;
   watcher-core moves both callers.
5. Fatal state is out of band: a backend failure sets `Shared::failed`, a full queue sets
   `overflowed`, and both wake the loop, so a full queue never hides a failure or strands a rescan.
6. Recovery is level-triggered: `Rescan` and SIGHUP re-read every group's full state. There is no
   safety poll; a change no event reports (a `ReadWriteMany` volume written from another node) is
   recovered by SIGHUP or restart. The chart's state volume is `ReadWriteOnce`, written from the
   same node.
7. A restart-only candidate exits only after the boot-only checks pass (transport credentials,
   storage roots), so the restart it asks for can boot; otherwise it is refused and last good serves.
8. Moving the broker's commit keeps credential retirement, `policy_loaded`, refusal dedup by raw
   candidate, and the shutdown drain of retired credentials.
9. Every stateful type has apply, refuse, abort and restart cases: Config groups, `LockGroup`,
   `TlsReload`, `SignerGroup`.
10. A lost publication feed never stops the gateway; a failed TLS offload does, as
   `GatewaydError::ReloadTask`.
11. Telemetry: the existing `*.reload.applied` / `*.reload.refused` records gain a `cause` label; an
   exit is one error record naming the category and the resident it binds. Serving never waits on
   the watcher.

## Items

| Id | Item | Kind | File | Invariant |
| --- | --- | --- | --- | --- |
| S-reload-01 | `enum ServeOutcome { …, ReloadStopped(Box<GatewaydError>), ReloadPanicked(TaskOutcome) }` | type | `crates/dekopon-gatewayd/src/lib.rs` | a stopped or panicked reload task ends serving with its cause; `run_with_service` returns it as `Err` |
| S-reload-02 | `GatewaydError::ReloadTask(TaskOutcome)` | error enum | `crates/dekopon-gatewayd/src/lib.rs` | a panicked or cancelled gateway reload task is a failure exit, never a clean one |
| S-reload-03 | `async fn reload_ended(task: &mut JoinSet<GatewaydError>) -> ServeOutcome` | async signature | `crates/dekopon-gatewayd/src/gateway/reload.rs` | task owner: `Gateway::serve` owns the one reload task in its `reloading` set and joins it at shutdown |
| S-reload-04 | `BrokerdError::ReloadTask(TaskOutcome)` | error enum | `crates/dekopon-brokerd/src/lib.rs` | a panicked or cancelled broker reload task is a failure exit |
| S-reload-05 | `pub(crate) async fn reload_ended(task: &mut JoinSet<BrokerStop>) -> Result<(), BrokerdError>` | async signature | `crates/dekopon-brokerd/src/self_reload.rs` | task owner: brokerd `serve` owns the one reload task (first the lock watch, then the watcher); `LockChanged` drains and exits `Ok` as today, `Failed` and a panic exit with the cause |
| S-reload-06 | `pub trait Reconcile { type Exit: From<ReloadError> + Send; fn input; fn watch_set; fn reconcile }` | trait | `crates/dekopon-core/src/reload/group.rs` | a group changes what it serves only after its last await; `run` returns the daemon's own stop, which may be a clean one |
| S-reload-07 | `pub enum Cause { Startup, Changed, Rescan, Hangup }` | type | `crates/dekopon-core/src/reload/group.rs` | every reconcile carries why it ran, and the record labels it |
| S-reload-08 | `pub enum Input { Operator, Machine }` | type | `crates/dekopon-core/src/reload/group.rs` | signaled mode holds only Operator groups |
| S-reload-09 | `pub enum Reconciled { Applied, Unchanged, Refused }` | type | `crates/dekopon-core/src/reload/group.rs` | only Applied re-derives registrations |
| S-reload-10 | `pub enum ReloadError` | error enum | `crates/dekopon-core/src/reload/error.rs` | each variant ends the task; exhaustive `category()` |
| S-reload-11 | `pub enum WatchedPath { File, Directory, Tree, Projection } + fn file/directory/tree` | type | `crates/dekopon-core/src/reload/path.rs` | classified once at group load; a projected entry becomes its `..data` directory; a mounted single file is `Unwatchable`; a symlinked file is watched at its target directory |
| S-reload-12 | `pub struct WatchSet` | type | `crates/dekopon-core/src/reload/path.rs` | covers an event by exact file name or watched directory, after canonicalizing the event's parent |
| S-reload-13 | `struct Registration { directory, depth: Depth }` | type | `crates/dekopon-core/src/reload/path.rs` | the watcher registers the union of every slot's registrations, so one group dropping a directory never unwatches another's |
| S-reload-14 | `pub struct ReloadSettings { mode: ReloadMode, debounce: Debounce, activation_timeout: ActivationTimeout }` | type | `crates/dekopon-core/src/reload/settings.rs` | `deny_unknown_fields`; mode parses in any case; both durations are `NonZeroU32` milliseconds |
| S-reload-15 | `pub struct Watcher<G: Reconcile> + fn subscribe, fn injected` | type | `crates/dekopon-core/src/reload/watcher.rs` | one per daemon; registers before its Startup reconcile so no change between boot load and registration is lost |
| S-reload-16 | `pub async fn Watcher::run(self) -> G::Exit` | async signature | `crates/dekopon-core/src/reload/watcher.rs` | returns only a fatal cause; each reconcile runs under the activation bound; recovery is level-triggered (Rescan, Hangup re-read full state) with no safety poll |
| S-reload-17 | `struct Shared { overflowed: AtomicBool, failed: OnceLock<io::ErrorKind>, wake: Notify }` | type | `crates/dekopon-core/src/reload/watcher.rs` | notify's thread never blocks; a backend failure and an overflow are out of band and wake the loop, so a full queue can neither hide a failure nor strand an overflow |
| S-reload-18 | `pub struct Injector` | type | `crates/dekopon-core/src/reload/watcher.rs` | the test seam drives the same queue and shared state as notify |
| S-reload-19 | `pub mod projection { pub const DATA_LINK; pub fn root(entry: &Path) -> Option<&Path> }` | type | `crates/dekopon-core/src/projection.rs` | the one projection predicate, outside the `reload` feature; `WatchedPath`, `dekopon_config::Projection::is_claimed` and brokerd `secrets.rs` call it (watcher-core moves both callers) |
| S-reload-20 | `GatewaydError::Reload(ReloadError)` | error enum | `crates/dekopon-gatewayd/src/lib.rs` | a mechanism failure ends the gateway with its cause |
| S-reload-21 | `pub(crate) enum GatewayGroup { Config(Box<ReloadState>), ProxyTls(TlsReload) } (impl Reconcile)` | type | `crates/dekopon-gatewayd/src/gateway/reload.rs` | the gateway's groups; Config is Operator, ProxyTls is Machine |
| S-reload-22 | `ReloadState::watch_set, async ReloadState::reconcile_cause` | async signature | `crates/dekopon-gatewayd/src/gateway/reload.rs` | the watch set is every path `GatewaySources` read; a restart-only candidate that passes boot-only validation exits with `RestartRequired`, otherwise it is refused once and last good serves |
| S-reload-23 | `pub struct GenerationDigest, CommitSignal, CommitReceiver (+ async changed)` | type | `crates/dekopon-core/src/reload/commit.rs` | latest-only; a new subscriber starts from the active generation; a dropped signal ends every subscriber |
| S-reload-24 | `async fn broker_generation::follow(client, commits, active) -> Infallible` | async signature | `crates/dekopon-gatewayd/src/gateway/broker_generation.rs` | task owner: the gateway's reload set; reads the digest the broker publishes at connect and each commit; a lost feed reconnects with bounded backoff (the `Unsettled::Monitor` retry today) while serving continues on the last digest, and a mismatch fails closed at Cedar; it never ends, so it joins the `JoinSet<GatewaydError>` of S-reload-03 as `async move { match follow(..).await {} }` |
| S-reload-25 | `ServingGeneration::with_broker(&GenerationDigest)` | type | `crates/dekopon-gatewayd/src/gateway/broker_generation.rs` | folds the published digest into the serving digest without calling prepare |
| S-reload-26 | `pub(crate) enum BrokerGroup { Config(ConfigGroup), Lock(LockGroup), Signer(SignerGroup) } (impl Reconcile)` | type | `crates/dekopon-brokerd/src/self_reload.rs` | the broker's groups; Config is Operator, Lock and Signer are Machine; `Exit = BrokerStop` through `impl From<ReloadError> for BrokerStop` |
| S-reload-27 | `async Coordination::reload(&self, Cause); Coordination::watch_set; Coordination::commits() -> CommitReceiver` | async signature | `crates/dekopon-brokerd/src/self_reload.rs` | swaps after its last await, keeping credential retirement, `policy_loaded`, refusal dedup and shutdown drain; publishes each commit; the watch set includes every private source (secret map, policies, GitHub App keys) |
| S-reload-28 | `pub(crate) struct LockGroup + fn watch_set, async fn verify -> Result<Reconciled, BrokerStop>` | async signature | `crates/dekopon-brokerd/src/lock_watch.rs` | Machine input; re-reads with `Observer`; a stable change is `BrokerStop::LockChanged`, a clean exit as today, an unsettled one `Failed(ProviderLockUnsettled)` |
| S-reload-29 | `async refresh_scheduler::serve_on_commits(server, catalog, commits, deadline)` | async signature | `crates/dekopon-brokerd/src/refresh_scheduler.rs` | replaces `serve`; a commit wakes it at once instead of at the next 5 min tick |
| S-reload-30 | `BrokerdError::Reload(ReloadError)` | error enum | `crates/dekopon-brokerd/src/lib.rs` | a mechanism failure ends the broker with its cause |
| S-reload-31 | `pub(crate) enum Category + fn from_key, const fn disposition, const fn label` | type | `crates/dekopon-gatewayd/src/gateway/consumers.rs` | parsed once; exhaustive disposition replaces the `&'static str` category lists |
| S-reload-32 | `pub(crate) enum Disposition { Swap, Restart(Resident) }, enum Resident` | type | `crates/dekopon-gatewayd/src/gateway/consumers.rs` | every restart category names the resource it binds |
| S-reload-33 | `CurrentGeneration::swap_follow` | type | `crates/dekopon-gatewayd/src/gateway/consumers.rs` | one owner holds the follow table; a swap keeps in-flight tickets |
| S-reload-34 | `async fn reconcile_tls(tls: &mut TlsReload, cause: Cause) -> Result<Reconciled, GatewaydError>` | async signature | `crates/dekopon-gatewayd/src/gateway/reload.rs` | offloads `TlsSource::prepare`, then commits after the last await; a refusal never ends the task; a failed offload is `GatewaydError::ReloadTask`, fatal |
| S-reload-35 | `pub struct TlsReload + fn source, fn commit, fn first_refusal; Listener::tls_reload` | type | `crates/dekopon-model-proxy/src/tls/reload.rs` | `applied` is seeded at bind; `commit` is the only mutation; a refused bundle is reported once |
| S-reload-36 | `pub struct TlsSource + fn prepare(self) -> Result<Prepared, TlsRefusal>` | type | `crates/dekopon-model-proxy/src/tls/reload.rs` | blocking reads only, offloaded; mutates nothing, so a cancelled reconcile changes nothing |
| S-reload-37 | `pub enum Prepared { Unchanged, Ready(PreparedTls) }, #[must_use] pub struct PreparedTls, pub struct BundleDigest` | type | `crates/dekopon-model-proxy/src/tls/reload.rs` | a bundle equal to `applied` is Unchanged |
| S-reload-38 | `pub struct TlsRefusal` | error enum | `crates/dekopon-model-proxy/src/tls/reload.rs` | names the refused bundle so the report is once per change |
| S-reload-39 | `pub(crate) struct SignerGroup, KeyId, StagedKey + async reconcile_key, async jwks_serves` | async signature | `crates/dekopon-brokerd/src/signer_reload.rs` | dormant until #979 and D2; if live, the key leaves the config fingerprint and promotion waits for the JWKS kid |
| S-reload-129 | `pub(crate) enum BrokerStop { LockChanged, Failed(BrokerdError) }` | type | `crates/dekopon-brokerd/src/self_reload.rs` | the broker reload task's stop; only `LockChanged` is clean |
| S-reload-130 | `pub struct GenerationFeed + async fn next; async BrokerClient::observe_generations` | async signature | `crates/dekopon-broker-protocol/src/lib.rs` | the held-open `ObserveGeneration`: one digest at connect, one per commit, on the existing socket |
| S-reload-131 | `async fn serve_observer(stream, commits, limits) -> Result<(), ConnectionError>` | async signature | `crates/dekopon-brokerd/src/server.rs` | task owner: the server's observer set, separate from connection tasks; writes one `Generation` frame per commit |
| S-reload-132 | `async fn drain_observers(observers: &mut ConnectionTasks)` | async signature | `crates/dekopon-brokerd/src/server.rs` | observers end at admission close, before the connection drain, so a broker stop with a gateway attached never ends in `ShutdownTimeout` |
| S-reload-133 | `LoadedMap::key_paths(&self) -> Vec<PathBuf>` | type | `crates/dekopon-brokerd/src/secrets.rs` | every key file the secret map names (GitHub App keys) joins the Config group's watch set |
| S-reload-137 | `async fn serve_with_watcher(config_path, shutdown, deadline, watcher: W) where W: FnOnce(ReloadSettings, Vec<BrokerGroup>) -> Result<Watcher<BrokerGroup>, ReloadError>` | async signature | `crates/dekopon-brokerd/src/lib.rs` | replaces `serve` and its `lock_poll`: `run` passes `Watcher::subscribe`; the factory receives the loaded settings and groups and returns the watcher |
| S-reload-138 | `#[cfg(test)] fn injected_watcher(sender: oneshot::Sender<Injector>) -> impl FnOnce(..)` | type | `crates/dekopon-brokerd/src/self_reload.rs` | the test factory: builds `Watcher::injected` and sends its `Injector` to the test; the callers at `tests.rs:2978` and `ManagedBroker::serve` (`tests.rs:3841`) move onto it |

## Tests

| Id | Test | File | Unit |
| --- | --- | --- | --- |
| S-reload-40 | a_panicking_broker_reload_task_stops_the_broker_with_its_cause | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/supervisor |
| S-reload-41 | the_broker_reload_task_is_joined_at_shutdown | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/supervisor |
| S-reload-42 | a_panicking_reload_task_stops_the_gateway_with_its_cause | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/supervisor |
| S-reload-43 | the_reload_task_is_joined_at_shutdown | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/supervisor |
| S-reload-44 | the_root_is_the_directory_that_holds_the_data_link | `crates/dekopon-core/src/projection.rs` | daemon/watcher-core |
| S-reload-45 | two_successive_projection_swaps_each_reconcile | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-46 | a_projected_file_is_watched_through_its_data_link | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-47 | an_event_through_a_symlinked_parent_is_covered | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-48 | a_burst_inside_the_debounce_runs_one_reconcile | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-49 | a_change_during_a_running_reconcile_runs_exactly_one_more | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-50 | signaled_mode_holds_an_operator_change_until_hangup | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-51 | a_machine_input_reconciles_in_signaled_mode | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-52 | a_full_event_queue_becomes_one_rescan_of_every_group | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-53 | a_change_between_subscribe_and_startup_still_reconciles | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-54 | a_backend_failure_ends_the_task_with_its_cause | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-55 | a_closed_event_queue_ends_the_task_with_its_cause | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-56 | an_activation_that_overruns_its_bound_ends_the_task | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-57 | an_applied_reload_that_names_a_new_directory_watches_it | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-58 | a_directory_two_groups_watch_stays_registered_when_one_drops_it | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-59 | a_failed_registration_keeps_the_previous_registrations | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-60 | a_single_mounted_file_is_refused_as_unwatchable | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-61 | mixed_case_reload_modes_parse_and_unknown_keys_are_refused | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-62 | a_backend_failure_behind_a_full_queue_still_ends_the_task | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-63 | an_overflow_wakes_an_idle_loop | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-64 | a_hangup_before_any_event_reconciles_every_operator_group | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-65 | a_rescan_rereads_every_group_without_a_safety_poll | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-66 | a_recreated_watched_directory_is_registered_again | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-67 | a_symlinked_file_entry_is_watched_at_its_target_directory | `crates/dekopon-core/src/reload/tests.rs` | daemon/watcher-core |
| S-reload-68 | an_aborted_gateway_reload_keeps_the_last_good_generation_serving | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/watcher-core |
| S-reload-69 | a_projected_gateway_config_update_applies_twice_without_a_restart | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/watcher-core |
| S-reload-70 | an_invalid_gateway_change_is_refused_while_last_good_serves | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/watcher-core |
| S-reload-71 | a_valid_restart_category_change_exits_with_its_cause | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/watcher-core |
| S-reload-72 | a_catalog_only_change_swaps_the_catalog | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/watcher-core |
| S-reload-73 | a_burst_of_catalog_events_runs_one_reconcile | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/watcher-core |
| S-reload-74 | a_restarted_gateway_serves_the_files_on_disk | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/watcher-core |
| S-reload-75 | an_activation_that_cannot_settle_exits_after_its_bound | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/watcher-core |
| S-reload-76 | a_skill_directory_named_by_a_reload_is_watched_from_then_on | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/watcher-core |
| S-reload-77 | the_gateway_never_watches_a_broker_private_path | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/watcher-core |
| S-reload-78 | a_reload_block_change_is_a_restart_category | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/watcher-core |
| S-reload-79 | a_restart_only_change_that_cannot_boot_is_refused_not_exited | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/watcher-core |
| S-reload-80 | the_gateway_watch_set_classifies_each_source_path | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/watcher-core |
| S-reload-81 | a_config_change_during_boot_is_served_after_startup | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/watcher-core |
| S-reload-82 | a_broker_config_edit_applies_in_process | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-83 | an_invalid_broker_edit_is_refused_while_the_active_generation_serves | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-84 | a_valid_broker_restart_category_change_exits_with_its_cause | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-85 | an_aborted_broker_reload_leaves_the_active_generation_serving | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-86 | a_burst_of_broker_config_events_runs_one_reload | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-87 | a_restarted_broker_loads_its_config_from_disk | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-88 | the_refresh_scheduler_sees_a_new_generation_at_its_commit | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-89 | a_replaced_provider_lock_ends_in_a_bounded_broker_exit | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-90 | an_unchanged_lock_digest_stands_the_check_down | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-91 | three_unreadable_lock_reads_stop_the_broker_with_its_cause | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-92 | the_lock_is_watched_in_signaled_mode | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-93 | the_brokers_own_credential_writes_trigger_no_reload | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-94 | a_prepare_frame_from_an_older_gateway_is_refused | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-95 | each_commit_publishes_the_broker_digest | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-96 | a_restart_only_broker_change_that_cannot_boot_is_refused_not_exited | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-97 | a_rotated_github_app_key_reloads_the_broker | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-98 | an_aborted_lock_check_leaves_the_broker_serving | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-99 | a_restarted_broker_verifies_the_lock_on_disk | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-100 | a_commit_subscriber_sees_only_the_latest_generation | `crates/dekopon-core/src/reload/tests.rs` | daemon/broker-self-reload |
| S-reload-101 | a_dropped_commit_signal_ends_every_subscriber | `crates/dekopon-core/src/reload/tests.rs` | daemon/broker-self-reload |
| S-reload-102 | a_new_commit_subscriber_starts_from_the_active_generation | `crates/dekopon-core/src/reload/tests.rs` | daemon/broker-self-reload |
| S-reload-103 | a_gateway_reload_sends_no_prepare_or_commit | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/broker-self-reload |
| S-reload-104 | the_gateway_folds_a_published_broker_digest_without_calling_prepare | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/broker-self-reload |
| S-reload-105 | a_gateway_reload_racing_a_broker_commit_serves_the_newest_broker_digest | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/broker-self-reload |
| S-reload-106 | the_broker_generation_feed_holds_only_the_latest_digest | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/broker-self-reload |
| S-reload-107 | a_reconnected_gateway_learns_the_broker_digest_at_connect | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/broker-self-reload |
| S-reload-108 | a_lost_broker_feed_reconnects_and_serves_the_last_digest_meanwhile | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/broker-self-reload |
| S-reload-109 | a_route_naming_an_agent_the_broker_refused_is_refused_until_the_broker_applies | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/broker-self-reload |
| S-reload-110 | a_reload_that_adds_the_first_follow_route_enables_follow | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/reload-consumers |
| S-reload-111 | a_follow_table_swap_keeps_in_flight_follow_tickets | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/reload-consumers |
| S-reload-112 | a_swapped_follow_table_retires_the_previous_table | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/reload-consumers |
| S-reload-113 | follow_state_survives_a_restart_through_the_durable_store | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/reload-consumers |
| S-reload-114 | every_restart_category_names_the_resource_it_binds | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/reload-consumers |
| S-reload-115 | a_proxy_tls_volume_change_swaps_the_acceptor_through_the_gateway_watcher | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/reload-consumers |
| S-reload-116 | a_proxy_tls_change_applies_in_signaled_mode | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/reload-consumers |
| S-reload-117 | a_refused_tls_bundle_is_reported_once_per_change | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/reload-consumers |
| S-reload-118 | a_reload_swaps_the_acceptor_and_accepting_stats_nothing | `crates/dekopon-model-proxy/src/tls/reload.rs` | daemon/reload-consumers |
| S-reload-119 | a_half_swapped_bundle_is_refused_and_the_old_acceptor_serves | `crates/dekopon-model-proxy/src/tls/reload.rs` | daemon/reload-consumers |
| S-reload-120 | an_aborted_tls_reload_keeps_the_old_acceptor | `crates/dekopon-model-proxy/src/tls/reload.rs` | daemon/reload-consumers |
| S-reload-121 | a_restarted_proxy_serves_the_mounted_bundle | `crates/dekopon-model-proxy/src/tls/reload.rs` | daemon/reload-consumers |
| S-reload-122 | the_bound_bundle_reads_as_unchanged_and_a_refusal_reports_once | `crates/dekopon-model-proxy/src/tls/reload.rs` | daemon/reload-consumers |
| S-reload-123 | a_retired_acceptor_drains_with_its_connections | `crates/dekopon-model-proxy/src/tls/reload.rs` | daemon/reload-consumers |
| S-reload-124 | a_renewed_signing_key_leaves_the_config_generation_unchanged | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/signer-reload |
| S-reload-125 | a_renewed_signing_key_is_promoted_only_after_the_jwks_serves_its_kid | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/signer-reload |
| S-reload-126 | a_jwks_that_never_serves_the_kid_keeps_the_active_key | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/signer-reload |
| S-reload-127 | a_second_renewal_replaces_the_staged_key | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/signer-reload |
| S-reload-128 | a_restarted_broker_signs_with_the_mounted_key | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/signer-reload |
| S-reload-134 | a_held_open_observer_ends_at_shutdown_without_a_timeout | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/broker-self-reload |
| S-reload-135 | a_lock_change_through_the_reload_task_ends_the_broker_cleanly | `crates/dekopon-brokerd/src/reload_tests.rs` | daemon/supervisor |
| S-reload-136 | a_panicking_tls_prepare_stops_the_gateway_with_its_cause | `crates/dekopon-gatewayd/src/tests/reload_watch.rs` | daemon/reload-consumers |

## Footprint

```footprint
{"unit": "daemon/supervisor", "step": "09-daemon/supervisor/build", "files": ["crates/dekopon-gatewayd/src/gateway.rs", "crates/dekopon-gatewayd/src/gateway/reload.rs", "crates/dekopon-gatewayd/src/lib.rs", "crates/dekopon-gatewayd/src/tests/reload_watch.rs", "crates/dekopon-brokerd/src/lib.rs", "crates/dekopon-brokerd/src/self_reload.rs", "crates/dekopon-brokerd/src/lock_watch.rs", "crates/dekopon-brokerd/src/reload_tests.rs", "crates/dekopon-brokerd/src/tests.rs"], "items": ["S-reload-01", "S-reload-02", "S-reload-03", "S-reload-04", "S-reload-05", "S-reload-129", "S-reload-40", "S-reload-41", "S-reload-42", "S-reload-43", "S-reload-135"], "rank": 1}
{"unit": "daemon/watcher-core", "step": "09-daemon/watcher-core/build", "files": ["crates/dekopon-core/src/reload/error.rs", "crates/dekopon-core/src/reload/group.rs", "crates/dekopon-core/src/reload/path.rs", "crates/dekopon-core/src/reload/settings.rs", "crates/dekopon-core/src/reload/watcher.rs", "crates/dekopon-core/src/reload/tests.rs", "crates/dekopon-core/src/projection.rs", "crates/dekopon-config/src/projection.rs", "crates/dekopon-brokerd/src/secrets.rs", "crates/dekopon-gatewayd/src/gateway.rs", "crates/dekopon-gatewayd/src/gateway/reload.rs", "crates/dekopon-gatewayd/src/config.rs", "crates/dekopon-gatewayd/src/tests/reload_watch.rs"], "items": ["S-reload-06", "S-reload-07", "S-reload-08", "S-reload-09", "S-reload-10", "S-reload-11", "S-reload-12", "S-reload-13", "S-reload-14", "S-reload-15", "S-reload-16", "S-reload-17", "S-reload-18", "S-reload-19", "S-reload-20", "S-reload-21", "S-reload-22", "S-reload-44", "S-reload-45", "S-reload-46", "S-reload-47", "S-reload-48", "S-reload-49", "S-reload-50", "S-reload-51", "S-reload-52", "S-reload-53", "S-reload-54", "S-reload-55", "S-reload-56", "S-reload-57", "S-reload-58", "S-reload-59", "S-reload-60", "S-reload-61", "S-reload-62", "S-reload-63", "S-reload-64", "S-reload-65", "S-reload-66", "S-reload-67", "S-reload-68", "S-reload-69", "S-reload-70", "S-reload-71", "S-reload-72", "S-reload-73", "S-reload-74", "S-reload-75", "S-reload-76", "S-reload-77", "S-reload-78", "S-reload-79", "S-reload-80", "S-reload-81"], "rank": 2}
{"unit": "daemon/broker-self-reload", "step": "09-daemon/broker-self-reload/build", "files": ["crates/dekopon-core/src/reload/commit.rs", "crates/dekopon-core/src/reload/tests.rs", "crates/dekopon-brokerd/src/self_reload.rs", "crates/dekopon-brokerd/src/lock_watch.rs", "crates/dekopon-brokerd/src/refresh_scheduler.rs", "crates/dekopon-brokerd/src/lib.rs", "crates/dekopon-brokerd/src/server.rs", "crates/dekopon-brokerd/src/config.rs", "crates/dekopon-brokerd/src/reload_tests.rs", "crates/dekopon-brokerd/src/tests.rs", "crates/dekopon-broker-protocol/src/lib.rs", "crates/dekopon-broker-protocol/src/tests.rs", "crates/dekopon-gatewayd/src/gateway.rs", "crates/dekopon-gatewayd/src/gateway/broker_generation.rs", "crates/dekopon-gatewayd/src/gateway/reload.rs", "crates/dekopon-gatewayd/src/lib.rs", "crates/dekopon-gatewayd/src/tests/reload_watch.rs", "docs/deploy.md", "crates/dekopon-agent/src/lib.rs", "crates/dekopon-gatewayd/src/tests.rs", "crates/dekopon-gatewayd/src/tests/github_burst.rs", "crates/dekopon-gatewayd/src/tests/jobs_e2e.rs", "crates/dekopon-brokerd/src/secrets.rs"], "items": ["S-reload-23", "S-reload-24", "S-reload-25", "S-reload-26", "S-reload-27", "S-reload-28", "S-reload-29", "S-reload-30", "S-reload-82", "S-reload-83", "S-reload-84", "S-reload-85", "S-reload-86", "S-reload-87", "S-reload-88", "S-reload-89", "S-reload-90", "S-reload-91", "S-reload-92", "S-reload-93", "S-reload-94", "S-reload-95", "S-reload-96", "S-reload-97", "S-reload-98", "S-reload-99", "S-reload-100", "S-reload-101", "S-reload-102", "S-reload-103", "S-reload-104", "S-reload-105", "S-reload-106", "S-reload-107", "S-reload-108", "S-reload-109", "S-reload-130", "S-reload-131", "S-reload-132", "S-reload-133", "S-reload-134", "S-reload-137", "S-reload-138"], "rank": 3}
{"unit": "daemon/reload-consumers", "step": "09-daemon/reload-consumers/build", "files": ["crates/dekopon-gatewayd/src/gateway/consumers.rs", "crates/dekopon-gatewayd/src/gateway/reload.rs", "crates/dekopon-gatewayd/src/gateway.rs", "crates/dekopon-gatewayd/src/config.rs", "crates/dekopon-gatewayd/src/proxy.rs", "crates/dekopon-gatewayd/src/session.rs", "crates/dekopon-gatewayd/src/lib.rs", "crates/dekopon-gatewayd/src/tests/reload_watch.rs", "crates/dekopon-model-proxy/src/tls.rs", "crates/dekopon-model-proxy/src/tls/reload.rs"], "items": ["S-reload-31", "S-reload-32", "S-reload-33", "S-reload-34", "S-reload-35", "S-reload-36", "S-reload-37", "S-reload-38", "S-reload-110", "S-reload-111", "S-reload-112", "S-reload-113", "S-reload-114", "S-reload-115", "S-reload-116", "S-reload-117", "S-reload-118", "S-reload-119", "S-reload-120", "S-reload-121", "S-reload-122", "S-reload-123", "S-reload-136"], "rank": 4}
{"unit": "daemon/signer-reload", "step": "09-daemon/signer-reload/build", "files": ["crates/dekopon-brokerd/src/signer_reload.rs", "crates/dekopon-brokerd/src/token_issuer.rs", "crates/dekopon-brokerd/src/secrets.rs", "crates/dekopon-brokerd/src/self_reload.rs", "crates/dekopon-brokerd/src/reload_tests.rs"], "items": ["S-reload-39", "S-reload-124", "S-reload-125", "S-reload-126", "S-reload-127", "S-reload-128"], "rank": 5}
```

## Gates (this head)

Passed: `git diff --check`; `cargo fmt --all --check`; `cargo clippy --workspace --all-targets
--all-features --locked -- -D warnings`; `test_daemon_dependency_gates.py`; `cargo machete`;
`cargo deny --locked check`; `RUSTDOCFLAGS=-D warnings cargo doc --workspace --all-features
--no-deps`; `cargo check --release` for the three daemons. Test filters: brokerd `reload_tests` 27
ignored, `lock_watch` 6 passed; gatewayd `reload` 20 passed, 32 ignored. Round 3 ran the wasm32
checks, both boundary greps and the core, model-proxy and broker-protocol filters; no dependency
or core change since. Not run: the full `cargo test --workspace` and the provider-example
workspaces. The `docs` word-budget job counts `SKELETON-reload.md` while it sits at the branch root.

## Choices I made

- Supervisor first: `ServeOutcome` loses `Copy` and `PartialEq` to carry `Box<GatewaydError>` and
  `TaskOutcome`, so the seven existing `assert_eq!` lines on a `ServeOutcome` became
  `assert!(matches!(…))`; the two new arms in `run_with_service` are wiring, not stubs.
- D1 keeps `ObserveGeneration`, `V1Alpha3` and the reconciliation owner for the ruled publication,
  and recommends the open `ObserveGeneration` connection as C1's shape rather than a new frame.
- The commit feed is core's `CommitSignal` again (both daemons use it); `Reconciled::Applied` still
  carries no digest, since the watcher never reads one.
- `TlsReload` splits into `TlsSource::prepare` (owned, offloadable) and `TlsReload::commit`;
  `BundleDigest` gives the bind-seeded applied and refused bookkeeping.
- `projection::root` is a stub in core, not yet called; watcher-core moves the two existing callers.
- Kept from round 1: no `Stop<E>`/`GroupId`, union registrations, `file`/`directory`/`tree`,
  brokerd-local verification with `Observer`, no `notify` in model-proxy, `Category` over
  `resident(&str)`, no `ProviderLockChanged`, no fragment loader (skeleton-broker lists it).
- Not changed (recorded): the 30 s activation bound against a slow probe, the lock-change exit
  status, a task-only join. N1, the default-feature clippy warning at `transport.rs:230`, is
  inherited and unchanged.
- Round 4: `follow` returns `Infallible` rather than the set taking a unit-or-error output, so the
  type states it never ends and `match follow(..).await {}` adapts it to the error-typed set; the
  test factory is a closure-returning stub because a `todo!()` body cannot stand for `impl FnOnce`.
- Round 3: `Reconcile::Exit` drops its `Error` bound so the broker's stop can be the clean
  `BrokerStop::LockChanged`; `follow` returns `()` because it never ends the gateway; observers get
  their own drain rather than a shorter grace.
- P1–P8 from the round 2 adjudication and N7–N13 are recorded, not widened.
- Merged main and added commits instead of force-pushing, so the draft stays in the PR history.
