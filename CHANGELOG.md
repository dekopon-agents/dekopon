# Changelog

All notable changes to Dekopon are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Application headings map to
`vX.Y.Z` Git tags; independently versioned Helm releases retain their full
`dekopon-chart-X.Y.Z` tag name. Release dates are the annotated tagger dates.

## [Unreleased]

### Added

- Harness-owned execution-aware history, scoped generation leases and bounded versioned memory
  checkpoints. Request-one bootstrap carries fresh descriptions and complete input schemas;
  execution evidence and consumed budgets survive inference failure and Stop, while generated
  text, accepted delivery and optional durable-memory recording remain separate.

- Unreleased structured harness activity at actual nested capability submissions, bounded public
  `activityLabels`, and opt-in Slack `activity.progressMessage`: one owned plain-text/hourglass
  post, coalesced updates and generation-safe best-effort cleanup, separate from replies/history.
  Existing Agent status/Stop/reaction and Discord/Telegram typing remain; local/WhatsApp are no-ops.

- Add harness-owned checkpointed token accounting across inference attempts, model/effort segments and terminal delivery outcomes; preserve unknown usage and replace success-only token observers.

- Unreleased configured model/effort transitions in `dekopon-harness`, opt-in gateway route
  `controls`, reused allowlisted clients, sole-tool batch preflight, bounded refusal-inclusive
  attempts, fresh broker admission per application, checkpointed portable-context rebuilds and
  replay/cache invalidation without resetting work budgets. Explicit model effort is encoded as
  Chat Completions `reasoning_effort` or Responses `reasoning.effort`; default omits the setting.
  Direct/replay runners remain fail-closed without a control authorizer.

- Unreleased core model/effort admission through authenticated `authorizeControl`, fresh Cedar
  `agent.prompt` plus reserved `agent.model.select`/`agent.effort.set` decisions, bounded broker
  `controlTargets`, request/agent/job bindings, startup epochs, and durable replay-consuming
  admission audit. Protocol `v1alpha3` requires lockstep client/broker migration; admission is not
  application or reusable provider authority.

- Added cooperative cancellation to `dekopon-process`: `CancelSignal::pair` yields a cloneable
  `CancelHandle` whose idempotent `cancel` makes the supervisor abort a
  `ProcessMetadata::cancellable` node at its next await and then still join it, surfacing
  `ProcessOutcome::TaskFailed` with `is_cancelled()`; a node that already returned keeps its real
  result, dropping every handle (or `CancelSignal::never`) never cancels, and the `process.node`
  span records `process.interruptibility` as `cancellable` and a requested cancellation as
  `process.outcome` `cancelled`. The broker leg in `dekopon-harness` is the one cancellable
  consumer — a gateway session's Stop abandons an in-flight command run through it — and the
  runner's `legacy-shell` and `direct-command` nodes stay non-interruptible.
- Added `dekopon:provider@0.3.0`, whose new `provider-cli` world exports `run-command`: a command
  word receives its argv and the value piped into it and answers with a capability proposal, text
  it rendered itself (stdout, stderr, and an exit status), or a decline. On the SDK side that is
  `Provider::run_command` (defaulting to the legacy rewrite), `CommandRun`, `CommandRunOutcome`,
  and `export_provider_with_cli!`; `dekopon_provider_sdk::host` carries the plumbing both hosts
  share to read which command export a component offers, gate a manifest declaring `commandWords`
  without one, count argv plus the piped value against the input bound, and decode either
  export's answer into one `CommandRunOutcome`. Both hosts serve the export —
  `ProviderRegistry::run_command` and `command_words_by_provider` on the import-free immediate
  host, `BrokerProviderRegistry::run_command` on the broker host — refuse an oversized input
  before a store exists (`CommandInputTooLarge`), call `run-command` when a component exports
  both, and keep loading components built against `0.1.0` or `0.2.0`, whose legacy export never
  receives a piped value. The `provider` and `provider-commands` worlds are unchanged, every WIT
  mirror moved together, the `cli-probe` and frozen `provider-v0-2-compat` fixtures joined the
  tree, and every repository-owned component was rebuilt with the pinned toolchain.
- Added an optional `clap` feature to `dekopon-provider-sdk`: `cli::run_command` parses a
  command word's argv against a declared `clap::Command` tree and answers as the upstream tool's
  `main` would — `--help`, `--version`, and the `help` subcommand rendered on stdout at status 0,
  an unknown subcommand or a missing argument rendered as clap's usage error on stderr at status
  2, and a well-formed argv handed with the piped value to a dispatch closure whose proposal is
  authorized as any other. The SDK re-exports `clap` so a guest builds its tree (or derives it)
  against the SDK's exact version, compiled without `env` or `color`: no argument can read a
  process environment and no escape sequence reaches the model. Hand-rolled argv handling stays
  the baseline contract; the `cli-probe` fixture now uses the layer and `memory-reservation-probe`
  is the hand-rolled `run-command` guest. `dekopon-provider-sdk-testkit` gained
  `FakeBroker::run_command`, which drives a component's command word through the broker host.
- `dekopon-run` serves provider command words in direct mode: `shell`, `prompt`, and `session
  replay --provider` answer every word the loaded components declare through
  `ProviderRegistry::run_command`, each run one nested non-interruptible `direct-command` process
  node inside the `legacy-shell` node, so `probe --help` renders the component's page,
  `echo hello | probe upper -` hands the piped value to the guest, and a proposal is invoked
  exactly as a bare capability word would be; `--max-input-bytes` bounds argv plus the piped value
  there. The broker leg in `dekopon-harness` runs each word as a cancellable `broker-command` node:
  `BrokerLeg::with_cancel_signal` accepts a `CancelSignal`, `dekopond` supplies one per session,
  and a native Stop abandons an in-flight command run instead of waiting for the broker's answer.
  `dekopon-run --broker` supplies none, so its nodes are cancellable in contract only.
- `dekopon-shell`'s `CommandRun` gains `Errored` and `Denied` for a run that never reached the
  provider's answer: a broker transport failure, a host refusal or trap, or a task that did not
  complete is reported like a capability that ran and errored (`<word>: failed: <cause>`, exit
  `1`), and a run cancelled underneath its session like a refused capability
  (`<word>: denied: session-cancelled`, exit `126`), so neither reads to the model as a usage
  error it should fix.
- Added the `agent.command.unobserved` audit record, emitted by `dekopon-harness`'s
  `report_unobserved_command_run` when either command leg's process node finishes after its
  caller was dropped: `command.leg` (`broker` or `direct`), a fixed `outcome`, and a fixed
  `error.type`, never the word, the argv, or any text; the cause goes out as an ordinary error
  event beside it. `command_run_from_outcome` maps a wire `CommandRunOutcome` onto the shell's
  `CommandRun` for both legs.
- Added skills, operator-authored reference material an agent reads on demand. `spec.skills` lists
  directories in the Agent Skills `SKILL.md` layout: YAML front matter carrying `name` (equal to
  the directory name; `[a-z0-9-]`, at most 64 bytes) and `description` (at most 1024 bytes), the
  optional `license`, `compatibility`, `metadata`, and `allowed-tools`, and no other key; then a
  Markdown body; every other regular UTF-8 file in the tree is a resource addressed by its
  `/`-separated relative path. `dekopon-config` reads each skill whole at catalog load — paths
  resolved against the catalog file's directory, hidden entries skipped, and a symbolic link, a
  non-regular file, nesting past 4 levels, more than 64 resources, a `SKILL.md` over 64 KiB, a
  resource over 256 KiB, or more than 1 MiB of resources refused — and reports every unloadable
  directory and every repeated name in the one catalog refusal, so a session never touches the
  filesystem. `dekopond` mounts the agent's skills on every session of a bound route, and
  `dekopon-run prompt --skill <DIRECTORY>` (repeatable) mounts them for one session, exiting 1
  before a session starts when a directory does not load or two carry one name. A mounted set adds
  one system message after the instructions listing each skill's name and description only; the
  model reads a body or one resource with `read_skill`, a repeat is answered with a one-line pointer
  at the earlier result, and an unknown name or path is a tool result naming what does exist rather
  than the end of the session. Each read is one `agent.skill.read` record (`skill.name`,
  `skill.resource`, `skill.bytes`, `skill.repeated`) and each refusal one `agent.skill.refused`
  record (`reason` `unknown-skill` or `unknown-resource`), in either payload mode.
  `inspect_agent_config` gains `skills` — names, descriptions, and resource paths, never the text.
  A skill is untrusted model text exactly as `instructions` is: the model reads it in full, so
  nothing secret goes in it, and it grants nothing. `examples/local` mounts a
  `pull-request-review` skill on its `reviewer` agent.
- Added `suggest_improvement`, the tool an agent taps the glass with: at most three structured
  notes per session on how its operator could improve it, each a `category` (`instructions`,
  `skill`, `capability`, `tool`, `limits`, `other`), a `target` (at most 128 bytes), a `summary`
  (512), an `evidence` and a `proposal` (2048 each), and a `confidence` (`low`, `medium`, `high`).
  It is off everywhere by default: `dekopon-run prompt --suggestions` and `session replay
  --suggestions` offer it and print each note to standard error, keeping standard output for the
  answer, and `routes[].improvementSuggestions: true` offers it on a gateway route, where a note
  reaches telemetry and never the chat. An accepted note is one `agent.improvement.suggested`
  record carrying every field; a note outside an enum, blank, past a bound, or past the session
  limit is answered with the reason and one `agent.improvement.refused` record
  (`invalid-category`, `invalid-confidence`, `empty-field`, `field-too-long`, `session-limit`), and
  the session continues either way. Both records fire whether or not payload telemetry is on,
  because offering the tool is the consent to put model-authored text in the log. A suggestion
  changes nothing: no instruction, skill, limit, or grant moves because a model asked. Embedders
  read them back from `SessionExit.suggestions`.
- Added `dekopon-run session list|show|replay`, which read sessions back from the OpenObserve log
  stream the runner and gateway export to. The receiver is `--openobserve-url`
  (`DEKOPON_OPENOBSERVE_URL`), the organization base the OTLP exporter posts to, with
  `--openobserve-stream` (`DEKOPON_OPENOBSERVE_STREAM`, default `dekopon`) and
  `--openobserve-auth-env` (default `DEKOPON_OPENOBSERVE_AUTHORIZATION`), the name of the variable
  holding the complete `Authorization` header value, so no credential value appears in an
  argument; the client follows no redirect, uses no ambient proxy, reads at most 20 pages of 500
  records and warns when it stops there, bounds a response at 32 MiB, and validates a trace
  identifier before interpolating it into SQL. `list` groups `accounting.model.call` records by
  trace within `--since` (default `7d`; a count followed by `s`, `m`, `h`, or `d`), newest first,
  so it also lists sessions recorded metadata-only; `show` reconstructs one session — system
  messages, earlier exchanges, prompt, every turn's scripts and their outputs, the answer — from
  `agent.model.prompt`, `agent.model.answer`, and the accounting records, and `--json` prints the
  exact shape `replay --from-file` reads back, so a recording can be kept, edited, and replayed
  with no backend in the loop. A session recorded with payload telemetry off is reported as
  accounted turns with no transcript rather than guessed at. Under the runner's root span the
  command is `session.list`, `session.show`, or `session.replay`.
- `session replay` puts a recorded conversation to a model again — the recorded instructions
  unless `--system` or `--system-file` replaces them, the recorded skills listing unless `--skill`
  replaces it, and whichever `--model` the operator names — and answers every script the model
  writes from the recording, so by default no capability runs and no effect happens. The first
  script the recording never ran is the divergence: the replay stops there and exits 0 unless
  `--provider` components were supplied, in which case that script runs live in direct mode and
  the report says so. The report compares recorded and replayed scripts index by index (`same`,
  `differs`, `recorded only`, `replayed only`) beside both answers and token totals, `--json`
  prints it whole, and the exit code is 1 only when the replayed session failed for a reason other
  than a divergence stop. Turns before the divergence are a faithful comparison and turns after a
  live one are a new session; replay never invents tool output. There is deliberately no durable
  store, no automatic rewriting, and no grader: the loop is `list`, `show`, edit, `replay`, commit.
- Added the telemetry this round's refusals needed, all of it category-only and all of it recorded
  in `docs/observability.md`: warn-level `gateway_reply_rate_limited` (`transport`, `method`,
  `cause_type`, `retry_after_seconds` for the refused sender and `channel_parked_seconds` for the
  shared slot, plus the `channel` only under the payload gate) for a channel-creating post refused
  because the channel is parked; the
  `gateway_activity_failed` cause types `activity-quarantine-full` and `activity-cleanup-abandoned`;
  the warn-level `conflicting-usage-observation` and `accounting-field-unreported` records naming
  the usage fields a tracker stopped trusting and the job they belong to; the error-level
  `live-checkpoint-lock`, emitted where a poisoned checkpoint lock is recovered or fences the
  lease; and the error-level `control-surface`, which names every conflict in a route's `controls:`
  block at construction. No new `audit.event` name.

### Changed

- Replace `dekopon-agent` with `dekopon-harness` and migrate all in-tree embedders to
  `SessionEngine`/`SessionBootstrap`; no compatibility facade. Model adapters now require an
  inference-attempt recorder. New call/transition/job accounting supersedes turn/image emitters.
  See `docs/upgrading.md` for API, protocol and telemetry migration and `docs/harness.md` for
  the current integration limitations. New-crate publication bootstrap is a separate release task.

- `dekopon-shell`'s `CapabilityInvoker::run_command` replaces `resolve_command`: it receives the
  piped value rendered as text (strings verbatim, other values as compact JSON) and answers with a
  `CommandRun` — a capability proposal authorized and charged like a direct call, text the provider
  rendered itself (help, a version, a usage error) written to the shell's stdout and diagnostic
  streams at the provider's own exit status and charging no capability call, or a decline
  reported as a usage error at exit `2`. The scripting tool's description now tells the model to
  run `<word> --help` for a provider command word's subcommands and flags. `dekopon-harness`,
  `dekopond`, and `dekopon-run` forward the new method, and the broker leg carries it over the
  new `runCommand` operation with the piped value.
- The broker protocol gains `runCommand` (`BrokerRequest::RunCommand`, with an optional `stdin`),
  answered by `BrokerResponse::CommandRun` carrying the guest's own `CommandRunOutcome` — a
  proposal, rendered text with its exit status, or a decline with the provider's stable code and
  message, which now rides the wire beside the message. `BrokerClient::run_command` and
  `RequestEnvelope::run_command` replace their `resolve_command` forms, which are gone,
  `Broker::run_command` replaces `Broker::resolve_command` and threads the piped value to the
  guest, and `dekopon-brokerd` answers both operations: `runCommand` with the
  outcome intact and the legacy `resolveCommand` with rendered text degraded to a decline carrying
  the text. Legacy operation handling does not admit an older envelope: the controls migration
  requires `v1alpha3` on both sides and rejects older binaries before dispatch. The piped value is
  bounded by the frame ceiling on the client and by the host's `maxInputBytes` on the broker.
- `dekopon-broker-host` renamed its command-word errors around the new export:
  `MissingResolveCommand` is `MissingCommandExport`, `ResolveCommandSignature` is
  `CommandExportSignature`, `ResolveCommand` is `RunCommand`, `InvalidCommandResolution` is
  `InvalidCommandRun`, and `ResolveCommandUsedHostImport` is `RunCommandUsedHostImport`, beside
  the new `CommandInputTooLarge`; `dekopon-provider-host` gains the same set plus
  `UnknownCommandWord`. Their messages say a command word was run, not rewritten; an exhaustive
  match downstream must name them.
- The scripting tool's description now tells the model when to reach for the tool and to write a
  job as one script; the exact JSON a `--kebab-case` flag becomes (a value reading as a number,
  `true`, `false`, or `null` is sent typed, anything else as a string, a bare flag as `true`);
  what each exit code means and what to do next (127 is `cap --list`, 126 a refusal to report, 2
  names its cause, 124 the deadline); that truncated output keeps its head and tail with a marker
  giving the total line count; that scripts share nothing but the conversation; and that skills,
  attachments, and configuration are not files. The refusal list and the four differences from
  bash are unchanged, and a test pins every promised exit code and message to the interpreter.
- `dekopon describe agent` always prints a `Skills:` section, `(none)` when nothing is mounted, its
  `--output json` carries each loaded skill whole, and the wide `get agent` table gains a `SKILLS`
  column between `PROVIDERS` and `MODEL`.
- `dekopon-harness`'s `SessionExit` gains `suggestions`, and `PromptError` gains
  `MissingSkillName`, `UnexpectedSkillArguments`, and `InvalidSuggestion` (telemetry kinds
  `missing-skill-name`, `unexpected-skill-arguments`, `invalid-suggestion`), which end a session
  as every other malformed tool call does; an exhaustive match downstream must name them.
- `dekopon-harness` now depends on `dekopon-config`, for the loaded `Skill` a session shows a model,
  and `dekopon-run` on `dekopon-config` (the same loader behind `--skill`), `ureq` (the OpenObserve
  client, on the HTTP stack the model clients already use), and `time` (RFC 3339 timestamps in
  `session list`). `dekopon-run` still reaches no broker crate; the CI `cargo tree` gate checks it.
  `dekopon-harness` and `dekopond` now also depend on `dekopon-process`, for the node each broker
  command run executes in and the cancel signal a gateway session hands it; it is not a broker
  crate, and the same gate covers `dekopond`.
  `dekopon-core` gains `SkillId`, `SkillIdError`, and `MAX_SKILL_NAME_LENGTH`, and
  `dekopon-protocol`'s `AgentSpec` gains `skills`, absent from serialized output when empty.
  `dekopon-harness` also gains `sha2`, for the per-component surface digests a session's freshness
  check compares, and `dekopon-broker` gains `getrandom`, for the startup epoch every control
  admission is bound to.
- `sessions.maxConcurrent` is validated at startup against `dekopon_harness::checkpoint::MAX_JOBS`
  (128), the checkpoint-lease ceiling every live session holds one of, and the refusal names the
  field, the value and the constant. A configured model whose `name` is not a configured-model
  identifier is refused with `models[].name` and the offending value, whether or not the deployment
  configures `controls:`; `docs/upgrading.md` carries the migration.
- `CheckpointStore::compare_and_save` takes the encoded length of the document its caller built, so
  a store checks its ceiling against that measurement instead of re-encoding the snapshot; an
  out-of-tree implementation must accept the added argument, and `Checkpoint::measure` is public so
  one that enforces the ceiling itself measures the document the same way the in-tree store does.
- `dekopon_shell::CapabilityInvoker::check_freshness` returns `Result<(), FreshnessError>` instead
  of `Result<(), String>`. `FreshnessError` is `Unavailable` or `Changed(SurfaceChange)`, and
  `SurfaceChange` names which half of the surface moved (`Epoch`, `Descriptions`, `EffectiveViews`,
  `CommandWords`, `ChatMemory`), so a log site records a stable token rather than a sentence; an
  out-of-tree implementor changes its signature and returns the typed value.
- A session compares its capability surface against the broker's at exactly two points in a turn —
  before each model request, and after a completion before it is disclosed — where it also compared
  before every capability invocation and every tool call. The broker authorizes each `invoke` and
  `runCommand` at dispatch, under the live epoch and the policy loaded then, so the client-side
  comparison guards disclosure rather than authority; dropping it from the inner loops removes one
  broker round trip per tool call and per invocation without moving where a decision is made.
  `docs/security-model.md` and `docs/harness.md` name the two checks that remain.
- The harness checkpoint store's byte ceiling is `MAX_JOBS * MAX_CHECKPOINT_BYTES`, so its lease
  ceiling and its byte ceiling agree at 128 rather than exhausting the second at 32 reservations —
  which is what silently capped a deployment's concurrent sessions at a quarter of the leases it
  advertised. A store already holding `MAX_JOBS` leases refuses the next one with `Capacity` before
  it evicts anything, so reaching the ceiling no longer destroys every stored checkpoint on the way
  to an error, and the refusal names the ceiling. `MAX_JOBS` is public and `docs/harness.md` states
  the relationship.
- A conversation's idle timeout runs from its last message rather than from its last committed
  turn: `begin` touches the entry the way `commit` already did. Without it a session that answered
  slowly left its own conversation the least recently touched candidate at the moment it finished,
  which is the eviction the **Fixed** entry below closes.
- `routes[].activityLabels` reports every offending entry in one refusal, each named with the rule
  it broke, instead of stopping at the first. A label the renderer would truncate past
  `MAX_ACTIVITY_LABEL_BYTES` (80 UTF-8 bytes) or leave blank once control characters and
  directional marks are stripped is refused at startup rather than shown clipped or empty; the
  gate calls the renderer's own `label_is_renderable` rather than mirroring its constant.
- A quarantined activity target — one an uncertain native write may still own — is tracked apart
  from the live leases, under its own 128-entry ceiling, and ages out after fifteen minutes. A full
  quarantine used to consume the lease ceiling and disable activity process-wide behind a debug
  log; it now costs one warn-level `gateway_activity_failed` when it fills and live leases keep
  acquiring. That event's category is `busy`, `quarantined`, or `capacity`, where one combined
  token could not tell a contended thread from an exhausted ceiling.
- `select_model`'s `effort` enum offers only the efforts the candidate list actually carries,
  mirroring `set_effort`. The gateway still offers all four, because it cannot see the broker's
  `controlTargets`; `docs/dekopond.md` states that a route whose baseline effort is absent from
  that list is answered `target-denied` on every proposal while still spending an attempt.
- `policy_digest` hashes the two reserved control actions unconditionally, so every deployment's
  digest changes on upgrade even where the policy set is byte-identical. `docs/upgrading.md`
  records that as expected rather than as evidence that a policy moved.
- A recording carries `version` at its top level and refuses an unknown top-level key. A file
  written before the field is read as version 1, and a version this build does not read is refused
  naming it; a hand-written recording carrying an extra key that used to be ignored is now a
  read-side break. `ReplayReport` gains `droppedHistoryTurns`, and a replay whose recorded exchanges
  did not fit `HistoryLimits` says how many turns it dropped rather than replaying a short history
  silently.
- An `agent.model.answer` row claiming more than `MAX_TOOL_CALLS_PER_TURN` tool calls is refused
  before the reconstruction iterates it, naming the turn, the claimed count, and the limit; the
  writer never produces such a row, so it is a corrupt or hostile one.
- Public API, for anyone rebasing on this branch: `dekopon-shell` gains `SurfaceChange` and
  `FreshnessError`; `dekopon-broker-protocol` gains `ClientErrorKind` and `ClientError::kind()` and
  drops the unconsumed `ERROR_CONTROL_DENIED`; `dekopon-model`'s `usage` module gains
  `USAGE_FIELD_NAMES`, `ObservationPrecedence`, `LoggedAttempt`, `AttemptLog`, `conflicting_fields`,
  and a defaulted `AttemptRecorder::observe_ranked`, and `ChatGptError` gains `Accounting`;
  `dekopon-harness` gains `control::ControlFailureKind` and `ControlError::Surface`, makes
  `TransitionOutcome::AuthorizationFailed` a struct variant carrying `cause` (a checkpoint JSON
  shape change, unreleased), adds `precedence` to `AttemptRecord`, and publishes
  `HistoryLimits::MAX_TURNS`/`MAX_BYTES`, `checkpoint::MAX_JOBS`, `CONVERSATION_CACHE_PREFIX`,
  `MAX_ACTIVITY_LABEL_BYTES`, `MAX_ACTIVITY_LABELS`, and `label_is_renderable`;
  `PolicyBuildError::{ReservedAction, DuplicateCapability}` and `BootstrapError::{Identifier,
  InvalidSchema}` carry every collision rather than one; and `dekopond`'s `cache_key::for_conversation`
  is deleted in favor of the harness-owned prefix constant both minting sites now share.

### Removed

- Removed `dekopon-agent`. `dekopon-harness` replaces it outright — no compatibility crate, no
  alias, no `run_prompt_session` facade — so there is no newer `dekopon-agent` version to move a
  pin to; an out-of-tree embedder migrates its dependency, its imports, and its
  `BrokerLeg::connect_attested` call sites, as `docs/upgrading.md` records.

### Fixed

- Reconstruct persistent portable tool history and model-switch/full context revisions in session
  show/replay; reject conflicting revisions and preserve independent failure/image usage without
  recounting remembered calls or restoring opaque provider continuation. Accept byte-free asset
  summaries interleaved within tool batches and count failed chat calls in replay comparisons.
- Repair gateway test compilation and parallel accounting trace capture; pin interpreter job-span
  ancestry and make oversized-frame refusal tests independent of socket write buffering.
- Fence retained-context reuse at authenticated broker freshness boundaries; validate execution IDs
  before checkpoint reservation and bound eviction of inactive fenced jobs. Preserve batch-local
  results, restored history, failed/nullable response usage and terminal host delivery accounting.
- Bound Slack cleanup metadata, retain native-write uncertainty through fallback, reject duplicate
  authenticated installations, and coordinate final/progress channel posts with definitive-429-only
  recovery. Recheck physical post slots after response arrival to prevent concurrent retries from
  colliding; preserve cleanup uncertainty and forward gateway safe-yield authorization checks.
- A Slack 429 now makes later senders in that channel wait rather than turning their paid-for
  answers into an instant `post-capacity`. The wait each sender can afford is measured from the
  moment it observes the slot, not from when it entered, so a sender already queued behind another
  when the 429 lands waits the whole park out instead of inheriting somebody else's backoff as a
  refusal; a sender waits at most two minutes in total, a stated `Retry-After` parks the channel
  for at most sixty seconds, and a missing or unparsable one parks it for five. A sender that is
  refused — because it spent that total, or because it drew the 429 itself — is told how many
  seconds of backoff are left, uncapped for the sender the service told to come back later, and
  `gateway_reply_failed` names the refusal rather than the generic `service`. Image answers take
  the same channel slot as text ones, because `files.completeUploadExternal` creates a channel
  message exactly as `chat.postMessage` does.
- Gateway shutdown drains the activity workers after the sessions and inside the same
  `shutdownGraceMs`, so an ordinary SIGTERM no longer strands a ⌛ progress message in a channel.
  A grace that expires before the removals land reports what it abandoned: one warn-level
  `gateway_activity_failed` with `cause_type="activity-cleanup-abandoned"` counting the artifacts
  left behind, separate from `gateway_sessions_abandoned`, which now means what it says. The
  workers are owned by the gateway rather than by the process, so two gateways in one process
  cannot drain or abandon each other's.
- Conversation eviction no longer takes a conversation another in-flight session is answering
  under: the store records the outstanding generation and evicts one nobody is answering first, so
  one sender's arrival cannot rotate another's cache key and turn a delivered answer into a
  refused append. When every candidate is in flight the least recently touched still goes, and a
  generation nobody committed stops protecting its conversation once it is idle.
- A checkpoint mutation encodes the snapshot once, not twice: the model-facing group ceiling, the
  per-checkpoint byte ceiling and the save share one measurement, where the size check and the save
  each used to traverse up to 2 MiB of JSON under the live lock five to eight times per tool call.
  Resuming an existing dormant entry at the lease ceiling is admitted rather than refused for a
  slot it already occupies.
- The gateway builds one capability snapshot per message. The broker leg keeps the projection it
  validated when it connected, and the fingerprint behind a conversation's surface is computed
  once, where each was built and encoded twice per inbound message.
- An operator-authored `activityLabels` value is accepted exactly when the renderer keeps it whole:
  the gate bounded the trimmed text while the renderer bounded the untrimmed one, so surrounding
  whitespace bought a label that passed startup validation and then lost its last characters in the
  channel.
- A usage field the tracker cannot trust is now reported as unreported calls for that field alone,
  instead of blanking the whole delta. `take_report` decides every field before it advances its
  cursor, so a `provider_total` that does not equal `input + output` no longer discards the input
  and output the same attempt reported, and the calls it covered are no longer skipped for good;
  the field and the job are named in a warn-level `accounting-field-unreported` record.
- A second, differing usage observation on one attempt marks that attempt's usage unknown instead
  of fencing the job and its checkpoint. Duplicate `"usage"` keys in one JSON object and a
  non-terminal SSE usage that disagrees with the terminal one are both handled that way, and a
  terminal `response.completed` usage wins over a non-terminal one when they differ.
- `modelTurns` is unknown rather than zero for a recording whose call list names no chat call at
  all — image calls only, which a truncated page set produces — and the transcript query is ordered
  like the accounting one, so a truncated fetch keeps the start of a session rather than an
  arbitrary slice. A reconstruction always states its call list, empty included, so a current trace
  whose accounting rows the receiver did not return reads as unknown rather than falling back to
  its answered turns — a different quantity — the way a file written before call accounting does.
- The cosmetic ⌛ progress post honors the same channel-slot park bound the answer path does. It is
  a `chat.postMessage` on the same physical channel, so a 429 there used to park the shared slot
  for the stated `Retry-After` — up to a day — and drop every later answer in that channel,
  including the failure fallback, as `post-capacity`.
- `gateway_session_failed` carries a stable `cause` token rather than the error chain. It is the
  terminal catch-all for every session failure, including a model-selected tool name, and
  `docs/observability.md` keeps untrusted model, provider and transport text out of events; a
  control failure still reports which client failure it was.
- Session reconstruction names the offender in every conflict it reports: a conflicting accounting
  call record names its job and call sequence and a conflicting prompt job ID names both IDs, where
  identical bare sentences collapsed into a single line naming none of them.
- `gateway_stopped` reports how many conversations were still resident at exit, the denominator for
  the `gateway_conversation_evicted` churn an operator sizing `sessions.maxConversations` watches.
- A whitespace-only completion is no longer stored as the job's generated answer before it is
  rejected, so a job resumed from that checkpoint can no longer deliver an empty answer with a
  `Send` outcome. `SessionBootstrap::with_resume` is `pub(crate)`, and `docs/harness.md` says
  plainly that no shipped binary resumes a checkpoint today.
- A control authorization failure carries a typed `ControlFailureKind` instead of a discarded
  `ClientError`: the kind reaches the checkpointed transition record, the `accounting.model.transition`
  event, and the gateway's `gateway_session_failed` through `cause`, so a `ControlBinding` refusal
  and a `ConnectTimeout` are no longer the same line in the log.
- A ledger refusal is reported as what it is. `ChatGptRequestError` and `ChatGptError` gain
  `Accounting`, which maps to `ModelError::Accounting` and to a `PromptError` whose
  `telemetry_kind()` is `accounting` rather than a retryable transport `Request`, on the image path
  as well as the chat one — a fenced tracker is an operator problem, and dashboards counting model
  transport errors were counting it.
- A broken policy stays visible: once a proposal's reason is `policy-error`, a later dimension's
  ordinary `policy-denied` does not overwrite it, so the operator signal that a policy failed to
  evaluate survives a denial that happened to follow it in the same proposal.
- The 30-per-minute cosmetic budget is reserved after the local gates rather than before them, so a
  cosmetic call refused because the route sends nothing, or because the channel's post rate is
  already spent, no longer spends an installation's budget on work that never left the process.
- A replay group claims `RecordedReplay` provenance only when every result in the batch was answered
  from the recording. A batch mixing recorded answers with a live dispatch carries the live
  provenance and shows no banner, where the group label used to promise that no new capability
  execution was claimed while one had just happened.
- `list_sessions` deduplicates by job and call coordinate *after* it filters on `audit.event`, so a
  non-accounting row sharing a coordinate with a real accounting row no longer suppresses it and
  drops the session from the listing. `docs/run.md` states that sessions recorded before the
  accounting rename appear in `show` but not `list`.
- Reconstruction, context validation, and recording validation each report every conflict they find
  before failing, instead of stopping at the first, and a `duration_ms` disagreement between
  duplicate exports is a conflict rather than a last-wins overwrite. `PolicyWorld::new` reports
  every reserved-action and duplicate-capability collision at once, bootstrap reports every
  malformed identifier and every non-object schema at once, `SessionControls::new` collects all
  three surface conflicts, and gateway startup names every transport that could not connect —
  `DekopondError::TransportConnect` carries the whole list — where each of these used to make an
  operator fix one problem per run.
- The replay validator and the live enforcer agree on what a message group's bytes are: both reset
  the count on any non-`tool` message, including the attachment summary, and an equality test feeds
  the same message sequence to both so the two cannot drift apart again.
- `begin` and `commit` no longer serialize the whole conversation corpus. `History::bytes()` is
  O(1) against a size maintained in `record`, trim, and eviction, and the store keeps a running
  byte total, where enforcing the ceiling used to re-encode every resident conversation on every
  message. Checkpoint sizes are likewise measured once per save through a counting writer and cached
  on the stored entry, so eviction reads cached sizes instead of re-encoding every stored checkpoint
  per step, and `CapabilitySnapshot::from_invoker` bounds itself with a running count rather than
  re-encoding the accumulated vector per capability.
- The scripting tool's description no longer tells the model to open with a `cap --list` discovery
  call. Request-one bootstrap already carries fresh descriptions and complete input schemas, so that
  sentence bought nothing and spent a paid model turn; a test pins that the description instructs no
  discovery call.
- `examples/conditional-write/dekopond.yaml`'s `http-probe` activity labels describe what that
  example's capabilities actually do, rather than naming work it never performs.

## [0.12.0] - 2026-08-29

### Added

- Added `dekopon-process`, a small unprivileged Tokio lifecycle seam that runs one typed async
  operation as one payload-free traced task under a self-contained supervisor that keeps joining,
  recording, and delivering it to a required abandonment observer if the outer caller is dropped
  while the owning Tokio runtime remains alive. Normal runner execution keeps that runtime alive.
  `dekopon-run shell` now moves provider loading and the unchanged synchronous interpreter off
  runtime workers as one opaque non-interruptible blocking node. Structured scopes/ports,
  cooperative cancellation, per-stage shell lowering, and provider stdin remain follow-ups
  requiring a real consumer.
- Added public inert secret DRNs and an owner-only private secret map. A broker-backed model may
  propose exact Basic/Bearer use without placing a reference or value in provider JSON/WIT; the
  broker requires ordinary capability policy, a separate exact Cedar `secret.use` grant, and a
  private authority/method/canonical-path/query binding before resolving one invocation-pinned
  snapshot. Current adapters cover strict secure files, Kubernetes projections/API Secret and
  acknowledged ConfigMap values, 1Password Connect, Vault KV v1/v2, AWS Secrets Manager/SSM, GCP
  Secret Manager, and Azure Key Vault. Native rendering, binding-swap refusal, injection bounds,
  and raw/rendered response-reflection refusal keep values out of models, providers, protocol,
  evidence, audit, traces, and errors. Existing implicit per-capability/per-agent credentials remain
  compatible.
- Added an offline provider manager to `dekopon-brokerd`. `provider sync`, `sync --locked`, `list`,
  and `verify` resolve fully qualified exact OCI tag or manifest-digest references, stream the one
  bounded `application/wasm` layer into a content-addressed store, validate the complete proposed
  provider set, and atomically activate a deterministic strict lock. A managed `providerSet` broker
  configuration is mutually exclusive with legacy `providers`; daemon startup remains network-free
  and compares lock digest, length, and provider identity against the exact buffer handed to
  Wasmtime. Registry provenance verification, private registry credentials, SemVer requirements,
  update/remove/prune commands, and container-image staging migration remain follow-ups.
- `dekopon-brokerd audit verify --audit-path <PATH>` verifies a durable audit chain offline, beside
  `provider list|verify`. (#34)
- `grep -E` and `sed -E` accept real regular expressions, compiled by the engine `jq` already links,
  with bounded pattern size and nesting; without `-E` patterns stay literal and an unescaped
  metacharacter is still rejected by name. (#23)
- Durable chat memory is identified by a typed `route:` on each constraint set instead of by
  capability and provider names, so renaming the shipped provider no longer drops the reservation
  and naming a capability `memory.chat.export` no longer gains one. (#12)
- `dekopon-provider-sdk` gained an optional `host` feature carrying the manifest validation,
  conflict reporting, store bounds, and engine construction both Wasmtime hosts share; the seven
  `DEFAULT_MAX_*` constants are deprecated at their old paths. (#14)

### Changed

- Reworked pull-request CI around measured bottlenecks: stable tests now run concurrently beside
  quality checks, the MSRV compiles test targets without executing the stable suite twice, debug
  path installs reuse the smoke-test build while one unified release check preserves profile
  coverage, and package/install/dependency/chart work is selected from tested path classes. Default-branch registry and sccache warmers feed restore-only PR jobs
  instead of uploading multi-gigabyte PR-scoped target archives, while job summaries report cache,
  network, and target-growth measurements for follow-up tuning.
- The local broker protocol is now `dekopon.dev/broker/v1alpha2`: attestation is one optional
  field on one operation per verb (`capabilities`, `resolveCommand`, `invoke`,
  `recordDeliveredTurn`) instead of eleven shape-specific operations, and a mixed broker/client
  pair now fails at the envelope in both directions. The broker answers an `apiVersion` it does not
  know with `invalid-request` on the first request frame, before anything is authorized, accounted,
  or audited; a client never emits that code, so an older client against a newer broker cannot
  decode the refusal and reports the outcome as unknown rather than as refused. Broker and gateway
  upgrade in lockstep; see `docs/upgrading.md`. (#20)
- The interactive console moved out of this repository to `dekopon-agents/dekopon-console`, taking
  `ratatui`, `crossterm`, five duplicate-version `deny.toml` exemptions, the `dev.<surface>.<name>`
  subject service, and the broker's `allowDevelopmentSubjects` field with it. (#22, #32)
- Route-scoped image generation collapsed to one `imageGenerator:` block with a boolean route
  opt-in; see `docs/upgrading.md`. (#27)
- `brokerLimits` and `hostLimits` default field by field, so naming one field no longer drops the
  defaults of every other field in the block. (#10)
- The `schemars` feature of `dekopon-core`, `dekopon-capability`, and `dekopon-protocol` is opt-in,
  so a default dependency no longer pulls `schemars`, `schemars_derive`, and `syn`. (#24)
- The provider manager's 73 error variants collapsed into ten a caller can branch on, each naming
  the exact check that refused. (#21)
- The abandoned `fs2` dependency is replaced by `std::fs::File` advisory locking on the audit,
  checkpoint, storage-lease, and provider-store paths. (#30)
- The trusted-file predicate — no symlink, regular, owner-owned, single-link, byte-capped — has one
  definition in `dekopon-core`, with its two permission tiers named. (#13)
- Every exporting process installs its subscriber and flushes its exporters through one
  `dekopon-telemetry` builder, and both daemons' exit records carry the same single `error` field. (#29)
- The Slack, Discord, Telegram, and WhatsApp transports share one reconnect delay, one dedup ring,
  one `retry_after` parser, and one message splitter, and WhatsApp reports its service codes as
  `http-<status>` like the others. (#31)
- The broker's web UI classifies accept failures with the same table the control socket uses and
  logs the errno chain instead of a bare event. (#7)
- Model clients no longer follow an ambient `HTTPS_PROXY`/`ALL_PROXY`; every `dekopon-model`
  transport is built from one agent that disables proxies and redirects. (#2)
- The scripting tool's description no longer tells the model that `[[ ]]` and `set -e` are errors,
  and a test pins the refusal list to what the interpreter rejects. (#28)
- Documentation gates run in their own toolchain-free CI lane covering `README.md`, `AGENTS.md`, and
  `crates/*/README.md`, so a documentation-only pull request can no longer skip them. (#9)

### Removed

- Removed the in-tree Echo, JSONPlaceholder, and memory-chat source workspaces and checked
  components after their standalone public v0.1.0 releases. Core tests, release archives, and image
  staging now fetch exact assets from
  [dekopon-provider-echo](https://github.com/dekopon-agents/dekopon-provider-echo),
  [dekopon-provider-jsonplaceholder](https://github.com/dekopon-agents/dekopon-provider-jsonplaceholder),
  and [dekopon-provider-memory-chat](https://github.com/dekopon-agents/dekopon-provider-memory-chat),
  require checksums pinned in `ci/fetch-external-provider-components.sh`, and keep downloaded Wasm
  ignored. Generic in-tree probes retain host, WIT, authority, resource, and redaction coverage.
- Removed the in-tree `skylight-private` source workspace and checked component. Its public
  standalone source is
  [dekopon-agents/dekopon-provider-skylight-private](https://github.com/dekopon-agents/dekopon-provider-skylight-private);
  no provider release is claimed, and it remains absent from default catalogs, images, policies,
  and deployments.
- Removed the shape-specific broker seam. `dekopon-broker-protocol` drops the six
  `RequestEnvelope` constructors `capabilities_for`, `capabilities_for_chat`, `invoke_for`,
  `invoke_for_chat`, `resolve_command_for_chat`, and `record_delivered_turn_for_chat`, and the six
  `BrokerClient` methods `session_surface_for`, `session_surface_for_chat`, `invoke_for`,
  `invoke_for_chat`, `resolve_command_for_chat`, and `record_delivered_turn_for_chat`;
  `dekopon-broker` drops the six `Broker` entry points `capabilities_for`, `capabilities_for_chat`,
  `invoke_for`, `invoke_for_chat`, `resolve_command_for_chat`, and
  `record_delivered_turn_for_chat`; and `dekopon-agent` drops `BrokerLeg::connect_attested` and
  `BrokerLeg::connect_chat`. The three claim types `ChatAttestation`, `ChatSessionClaim`, and
  `SubjectAttestation` are replaced by one `Attestation`, which the surviving calls take as an
  argument where the shape used to pick the method. (#20)
- `dekopon-brokerd`'s public `ProviderManagerError` lost 63 of its 73 variants. The ten that remain
  — `Configuration`, `StateConflicts`, `FileSecurity`, `Registry`, `DigestMismatch`,
  `LockMismatch`, `StoreFull`, `OperationInProgress`, `Host`, and `Io` — each name the check that
  refused, so a caller branches on the ten instead of matching a variant per message. (#21)
- `dekopon-broker` no longer exports `MEMORY_PROVIDER`, `MEMORY_WORD`, `MEMORY_RECENT`, or
  `MEMORY_SEARCH`, and `MEMORY_RECORD` is private. The chat-memory surface is identified by the
  typed `route:` on a constraint set, so there is nothing left for an outside caller to match
  against. (#12)
- `ProviderConflicts` is no longer defined in `dekopon-broker-host` or `dekopon-provider-host`; it
  lives in `dekopon_provider_sdk::host`, and both hosts re-export it at the old paths. It gained a
  public `wording: ConflictWording` field and is not `#[non_exhaustive]`, so a downstream
  struct-literal construction must name the new field. (#14)
- `CapabilityInvoker::invoke_with_secret_use` is gone from `dekopon-shell`.
  `CapabilityInvoker::invoke` takes `(capability, input, secret_use)` itself, so an implementor can
  no longer forward an invocation while silently dropping the secret binding. (#8)
- A batch of unreferenced public items (`script_outcome_label`, `provider_for`, `header_values`,
  testkit `storage_evidence`/`temporary_dir`, the `_assert_private_path` stub), and `truthy`,
  `is_bare_command_substitution`, and `History::from_turns`, which had only test callers and are
  no longer public. (#34)

### Fixed

- `dekopond` reports every problem in a gateway configuration, every unsatisfiable route, and every
  unusable credential in a single startup refusal, and resolves all chat, model, and
  image-generator credentials before any transport authenticates. (#11)
- A chat refusal is audited with its real class (`attestation-denied`, `unmapped-subject`,
  `agent-denied`, `policy-error`) and the policies that determined it, instead of one flattened
  `chat-attestation-denied`, and each chat message evaluates the `agent.prompt` policy once instead
  of twice. (#5)
- An unaudited storage outcome names the class of the failure that caused it (`quota`, `timeout`,
  `corrupt`, `denied`, `io`) in the error chain and in a new `broker_storage_outcome_unaudited`
  log. (#6)
- A model's `apiKeyEnv` naming an unset, blank, or non-UTF-8 variable is a startup refusal naming
  the variable, instead of a tokenless client that answered every message with a 401. (#4)
- A `${drn:…}` secret reference in a script reaches the broker from a `dekopond` chat session
  instead of being refused inside the process that built the proposal. (#8)
- ChatGPT credential discovery ignores an empty `DEKOPON_CHATGPT_AUTH_FILE`/`XDG_CONFIG_HOME`/
  `HOME`/`APPDATA` export and refuses a relative discovered path instead of writing the rotating
  refresh token into the current directory. (#3)
- `.gitignore` ignores `examples/conditional-write/broker-credentials.yaml`, the path the surviving
  walkthrough tells you to paste a live token into. (#1)
- `docs/dekopond.md` cites the chart's real 270 s pod grace, and `docs/catalog.md` agrees with its
  own four-row reserved-fields table. (#33)

## [dekopon-chart-0.3.0] - 2026-08-29

### Added

- The chart can enable the read-only web UI through `broker.httpBind`. It is empty by default, so
  the rendered deployment passes no `--http-bind` argument at all and neither a Service nor an
  Ingress is created for it. (#25)

### Changed

- `appVersion` names `0.12.0`, and the chart authors `brokerLimits.maxReplayIds: 200000` and
  `hostLimits.maxTotalMemoryBytes: 268435456` explicitly. `0.12.0` defaults both blocks field by
  field, so a chart that names one field no longer silently drops the rest. (#10)

## [0.11.1] - 2026-08-23

### Fixed

- Moved the container image's runtime base from `gcr.io/distroless/cc-debian12` (glibc 2.36) to
  `cc-debian13` (glibc 2.41). `dekopon`'s console gained two weak-linked symbols at `GLIBC_2.39`,
  `pidfd_spawnp`/`pidfd_getpid` — Rust's std probes both at runtime and falls back cleanly when
  either is absent, but glibc's dynamic linker refuses to load a binary naming a version node the
  runtime library lacks at all, weak reference or not, so v0.11.0's container-image publish failed
  outright. Pure infrastructure: no crate's source changed.

## [0.11.0] - 2026-08-23

### Added

- Added `dekopon-provider-sdk-testkit`, an in-process fake broker that loads a provider component
  and runs it against a real `StorageHost` over a temporary root, so storage-backed providers can
  be tested end to end. It skips Cedar and the constraint catalog by minting authorization through
  `AuthorizationGate`, and defaults its per-invocation ceilings from the host limits in force
  rather than restating them. Storage grants are minted per invocation from constant scope
  material, so successive calls reach one durable namespace.
- Added `dekopon console`, an interactive terminal view over a running `dekopon-brokerd`. It opens
  an attested session for one external subject through one catalog agent, shows that agent's
  declared capabilities beside the surface policy actually grants it, runs turns, and draws each
  turn's scripts and capability calls with their exact JSON input and result. A bare `dekopon` opens
  it when standard input and output are both terminals; anywhere else a missing subcommand remains
  a usage error exiting `2`.
- Added `dekopon-tui`, the crate behind that console. It observes the agent loop through decorators
  on `dekopon-agent`'s `ScriptRuntime` and `dekopon-shell`'s `CapabilityInvoker` seams, which is the
  only place a tool call's arguments and results exist: conversation history keeps prompts and
  answers, `shell.command` spans keep argument counts, and audit records keep digests. Neither
  decorator can influence a session. Its local dispatch leg is empty by construction, so every
  capability reaches the broker and no Wasmtime enters the operator CLI's dependency tree — checked
  in CI by the same `cargo tree` gate already applied to `dekopon-run` and `dekopond`.
- Added a `dev.<surface>.<name>` subject service and a `dekopon-brokerd` `allowDevelopmentSubjects`
  opt-in that admits it. It is the only subject service no external service authenticated — a name
  a local caller typed on an owner-only socket rather than one Slack or a carrier verified — so it
  is off by default, and a broker whose configuration names development identities without it lists
  every offending mapping and attestor namespace at once and refuses to start. The opt-in is the
  whole enforcement: configuration is immutable for a process, so a broker started without it holds
  no `dev.*` mapping and an attested development subject resolves to nothing through the ordinary
  unmapped-subject refusal. It exists so `dekopon console` need not borrow `tel.15550100000`, which
  would put a value in `identityMappings`, in policy, and in the audit chain that reads like a phone
  number and is not one. The surface segment scopes a grant to `dev.console` without admitting
  `dev.ci`.
- Added `dekopon_model::chatgpt::resolve_auth_path_named` and `DEFAULT_AUTH_FILE_NAME`, so a second
  consumer can resolve a credential under the documented precedence with a different file name.
  `dekopon console` uses it for `chatgpt-auth.console.json` and refuses to start if discovery lands
  on the shared `chatgpt-auth.json` instead, because the refresh token rotates and a shared file
  means whichever process refreshes invalidates the gateway's copy. An explicit `--auth-file`
  accepts that deliberately.
- Added `read` and `getopts` to the sandboxed shell. `read [-r] NAME...` is what makes
  `cmd | while read line; do ...; done` terminate: it consumes one line per call through a cursor
  on the enclosing pipeline stage and reports end of input as a status rather than a diagnostic,
  which would otherwise be one message per loop iteration. Several names split the line on
  whitespace runs with the remainder in the last, a rule local to `read` rather than a return of
  IFS word splitting. `getopts` parses a shell function's own flags with `OPTIND` and `OPTARG`, and
  is scoped to a function because that is the only place positional parameters exist here.
- Made `set -e`, `set -u`, and `set -o pipefail` real in the sandboxed shell, with their `+` forms,
  and added `${PIPESTATUS[@]}`. `set` was refused outright before on the grounds that an option
  changing nothing while looking like it had is the exact silent wrongness this shell refuses;
  that argument stops applying once the option is enforced and still holds for every option that
  is not, so `set -x`, `set -o noclobber`, and `set --` now end the script by name rather than
  being ignored. `errexit` exempts the three positions bash exempts, and they compose. `pipefail`
  matters more here than in bash: `some.capability x | jq .` succeeded by default even when the
  capability never ran, because `jq` was handed nothing and had no complaint.
- Accepted `[[ ... ]]` in the sandboxed shell. It runs the same tests `[` and `test` run — one
  function, so the two spellings cannot disagree — and adds bash's connective grammar (`&&`, `||`,
  `!`, parentheses, short-circuiting) plus the promise that an unquoted expansion is one operand.
  The right operand of `==` is a glob in bash; every pattern here is literal text, so a
  metacharacter there is refused by name rather than compared literally, and `=~` is refused
  outright. Grammar keywords are now mirrored into `dekopon_core::RESERVED_COMMAND_WORDS`, closing
  a hole where a provider could declare a command word like `do` or `then`, load successfully, and
  never be reachable because the parser consumed the word first.
- Made compound commands — `if`, `for`, `while`, `until`, `case`, and the newly accepted `{ ...; }`
  group — usable as pipeline stages, so `cmd | while read-shaped loop` and
  `cmd || { echo failed; exit 1; }` parse, and a compound stage carries its own redirections. A
  piped compound runs in the current scope rather than a subshell, so a `while` loop feeding off a
  pipe keeps the variables it assigns; bash discards them with the subshell, which is the single
  most notorious trap in the language. A stage feeding a pipe or a redirection has its emissions
  collected into one value, the same collection a command substitution already performed.
- Gave the sandboxed shell real parameter expansion: `${NAME:-w}`, `${NAME:=w}`, `${NAME:?w}`,
  `${NAME:+w}` and their colon-free forms, `${#NAME}`, `${NAME[@]}`/`${NAME[*]}`, and the literal
  `${NAME#p}`, `${NAME%p}`, `${NAME/p/r}`. Two answer differently than bash because values are real
  JSON: `${#NAME}` counts elements of an array and keys of an object, and `${NAME[@]}` selects the
  elements of a JSON array rather than emulating a bash array. `${NAME:?w}` ends the script, which
  is what the construct is for. Expansion patterns are literal text like every other pattern here,
  with quoting as the escape hatch while the parser can still see it.
- Gave the sandboxed shell two script-addressable streams. `2>`, `2>>`, `&>`, `&>>`, `2>&1`, `>&2`,
  and `> /dev/null` now redirect a command's diagnostics or its value into a named in-memory
  buffer, and a command may carry more than one redirection. The stdout/stderr split already
  governed behaviour — `$( )` captured the value while diagnostics escaped to the terminal — and
  this makes it something a script can address. `x=$(cmd 2>&1)` therefore captures *why* a
  capability failed rather than only that it did; a quiet command's value, and its type, are left
  untouched. `ScriptOutcome::output` is still the one combined transcript a terminal would show.
- SQL is now available to providers, out of this tree. `turso-sql` is a SQLite-compatible engine
  (`turso_core`, pure Rust) compiled to `wasm32-unknown-unknown`, importing
  `dekopon:storage/durable-files@0.1.0` and nothing else — no WASI, no JS interop, no C in the
  artifact. It lives at
  [dekopon-agents/dekopon-provider-turso-sql](https://github.com/dekopon-agents/dekopon-provider-turso-sql)
  with its own tags and release cadence, because its component is 11 MB and its dependency graph is
  an order of magnitude heavier than any example in this repository. Nothing here depends on it and
  no shipped memory path uses it; an operator who wants it fetches the release asset.

### Changed

- Broker socket discovery now has one definition, `dekopon_broker_protocol::BrokerSocketDiscovery`,
  consumed by `dekopon-run`, `dekopond`, and the console. The precedence and the refusal to probe a
  candidate for existence are unchanged; each caller keeps its own error wording, since "no tier
  applied" is a usage failure to one and a configuration failure to another.
- `dekopon --help` now renders `Usage: dekopon [OPTIONS] [COMMAND]`, because the subcommand is
  genuinely optional on a terminal.
- A whole right-hand side keeps its value rather than collapsing to text for a bare `$NAME` as well
  as a bare `$(cmd)`, so `copy=$obj` followed by `${copy[key]}` works. Previously only the
  substitution spelling survived, and `copy=$obj` silently flattened the object into its JSON text.
- The shell no longer rejects file-descriptor redirection wholesale. Descriptors other than 1 and 2
  (`3>`, `<&`) are still refused by name, as is `2>&1` written *before* the redirection it copies:
  bash duplicates the file description there and leaves stderr on the terminal, this interpreter
  has destinations rather than descriptions, and that spelling is the one a script writes when it
  believes it captured output that went elsewhere.
- Corrected the durable-files documentation: the five-level lock ladder constrains the shape of a
  lock sequence and is consulted by no I/O path, so a guest may read and write at `none` and an
  adapter that never locks is equally correct. The SHM and multiprocess-database disclaimers stand;
  the "no WAL claim" wording did not, since a single-instance WAL engine needs neither and runs on
  these primitives unchanged.
- Rescoped the 2026-08-20 Turso gate result in the roadmap, security model, and experiment log. It
  tested the crates.io `turso` wrapper, whose SDK-kit dependencies are not reachable from the
  engine; it was not a finding about `turso_core`.

### Removed

- The `gh` shell builtin and its six `gh.*` capabilities left this repository. `gh` now ships from
  [dekopon-agents/dekopon-provider-gh](https://github.com/dekopon-agents/dekopon-provider-gh) with
  its own tags, issues, and release cadence — `gh-provider.wasm`, its `.sha256`, and a SLSA
  provenance attestation, the same standard the release archives here are held to.
  `examples/pr-summarizer-linter/`, the only in-tree example carrying a broker configuration, a
  Cedar policy, and a credentials template, moved with it. `examples/conditional-write/` replaces it
  as the walkthrough exercising two authorized calls in one invocation — `http-probe.conditional-write`
  in place of `gh.pull-request.approve`. The container image still ships `gh`, fetched at a pinned
  tag with its digest and build provenance verified before staging.

### Fixed

- Held the `test (Rust 1.89.0)` job to the MSRV it names. `rust-toolchain.toml` pins `channel =
  "stable"`, which outranks the `rustup default` the toolchain action sets, so the job installed
  1.89.0, used it only for a cache key, and then compiled on current stable. It now exports
  `RUSTUP_TOOLCHAIN` and fails loudly if the effective `rustc` or the workspace `rust-version`
  drifts from the pin.

## [dekopon-chart-0.2.1] - 2026-08-23

### Fixed

- The Helm chart's `appVersion` now names `0.11.0`, the current application release, so
  `app.kubernetes.io/version` and a default `image.tag` stop reporting `0.10.0` on pods running a
  later one.
- `charts/dekopon/values-pr-summarizer-linter.yaml` and `charts/dekopon/README.md` now point at
  [dekopon-agents/dekopon-provider-gh](https://github.com/dekopon-agents/dekopon-provider-gh), where
  the pr-summarizer-linter walkthrough moved with the `gh` provider. No rendered manifest changed.

## [0.10.0] - 2026-08-22

### Added

- Added a first text-only Meta WhatsApp Cloud API gateway transport with a signed bounded webhook,
  process-local message-ID deduplication, canonical `whatsapp.<wa_id>` subjects, and pinned Graph
  API text replies.
- Added opt-in route-scoped OpenAI image generation and bounded generated-PNG replies across Slack,
  Discord, Telegram, and the local development transport.
- Added broker-owned, namespace-bound provider storage with strict quotas, transactional JSONL,
  engine-neutral durable files, feature-gated Rust guest bindings, and content-free evidence.
- Added the optional generated `memory-chat` provider and on-demand `memory recent` / `memory
  search` commands, with stable or non-reusing authority-bound continuity across restarts.
- Added chat-scope grants, invocation-bound chat attestations, and dedicated post-transport
  `RecordDeliveredTurnForChat` recording.
- Added opt-in broker-only provider-storage PVC/key mounts and optional container packaging for the
  memory provider.
- Documented the nine refusal, error, and outcome audit events `docs/observability.md` had never
  named, and made an emitted `audit.event` name absent from that file a CI failure.
- Added an optional broker `compileCachePath` for Wasmtime's persistent compilation cache, so a
  restart reads compiled provider code back instead of running Cranelift again.
- Added an optional `dekopon-run --compile-cache <DIRECTORY>` (`DEKOPON_RUN_COMPILE_CACHE`) backed by
  `dekopon-provider-host`'s `HostOptions::compile_cache_dir`, so repeated `inspect`, `invoke`,
  `shell`, and `prompt` processes read Wasmtime's compiled provider code back instead of running
  Cranelift again.
- Added an optional broker `hostLimits.maxTotalMemoryBytes` aggregate guest-memory ceiling, so
  concurrent invocations past the budget are refused instead of being OOM-killed. The broker also
  states the `maxConnections` × `maxMemoryBytes` worst case in one startup line.
- Broker connection, framing, audit-append, and checkpoint failures now log their cause: the
  protocol failure kind, the provider host error, the audit failure category, and the full source
  chain reach `broker_request_frame_invalid`, `broker_audit_append_failed`,
  `broker_checkpoint_poisoned`, `broker_connection_failed`, and `broker_outcome_unaudited`. Wire
  responses are unchanged and stay generic.
- A refused `capabilitiesFor`/`capabilitiesForChat` now emits `broker_capabilities_refused` naming
  the refusal class and the canonical subject on the broker side, while the wire answer stays
  opaque.
- `broker.authorize` now records `policy.errors_present`, and a Cedar evaluation error denies with
  the distinct reason `policy-error` instead of being indistinguishable from `policy-denied`.
  `broker.execute` records `outcome` and the classified `error`.
- `broker.authorize` now records `policy.errors_present`, and a Cedar evaluation error denies with
  the distinct reason `policy-error` instead of being indistinguishable from `policy-denied`.
  `broker.execute` records `outcome` and the classified `error`.
- A finished connection task is now observed as soon as it completes rather than on the next accept,
  so `broker_outcome_unaudited` no longer waits for unrelated traffic on a quiet broker.
- `broker.authorize` no longer stays entered on its worker thread while the authorizing task is
  suspended. The section awaits the replay ledger and, on every denial, a durable audit append that
  fsyncs, so whatever the runtime polled next on that thread was exported as a child of another
  request's authorization while that request's own later events lost it. The span instruments the
  section instead of being held across the awaits; its fields and values are unchanged.

### Changed

- `dekopond` now builds one model client per configured model instead of one per message, so a
  routed message no longer pays a fresh TCP and TLS handshake to the model endpoint before its
  first token. Prompt cache keys and completion options remain per-request.
- `dekopon-protocol` resources now carry a per-resource single-variant `kind`, so a document whose
  `kind` names another resource fails to decode in the crate itself rather than only in
  `dekopon-config`. `model_class`, `policy_profile`, `credential_ref`, `CapabilityStatus`, and
  `ProviderStatus` documentation now matches what 0.9.0 actually does with them.
- `JsonSchema` derives in `dekopon-core`, `dekopon-capability`, and `dekopon-protocol` moved behind
  a default-on `schemars` feature, and `dekopon-provider-sdk` no longer enables it, so a wasm
  provider build drops `schemars`, `schemars_derive`, and `syn`.
- Broker provider components now compile concurrently through Wasmtime's parallel Cranelift backend
  instead of one at a time on a single core, while conflict reporting and the first reported
  failure stay in configured order.
- Broker provider imports are now linked once per component into a cached `InstancePre`, so an
  invocation, description, or command rewrite no longer rebuilds a linker and re-resolves imports.
- The `provider.compile` span now carries the component path, artifact bytes, and elapsed
  milliseconds; each loaded provider emits one info event; `resolve_command` runs inside a
  `provider.resolve_command` span carrying the provider and the command word.
- Added Slack Agent owned-thread continuation: after one explicitly addressed message is freshly
  authorized, that authenticated sender can continue in the exact thread without another mention.
- Added the request-scoped `decline_chat_reply` model tool for unaddressed owned-thread follow-ups,
  allowing the agent to post nothing instead of always taking the last word.
- Added `docs/catalog.md`, the field-by-field `v1alpha1` resource reference, naming what consumes
  each `AgentSpec`/`CapabilitySpec`/`ProviderSpec` field and stating that `policyProfile`,
  `credentialRef`, `status`, and `labels` are read by nothing.
- Added `docs/upgrading.md`, covering the 0.3.0 `rules` → `policiesPath`/`constraintSets` migration,
  the 0.5.0 broker-protocol lockstep, later operator-visible changes, and the restart order.
- Added `docs/operations.md`, an operator index into the per-crate operational contracts, so audit
  checkpoint recovery is reachable from `docs/` instead of only from a crate README.
- An exhausted replay ledger or audit log now answers the new stable failure code
  `capacity-exhausted` and logs `broker_capacity_exhausted`, instead of the retriable
  `broker-unavailable`. Neither bound evicts, rotates, or clears on restart, so a client was being
  invited to retry forever. `maxReplayIds` must be sized against `auditMaxRecords`, which is now
  documented in `docs/broker-http.md` and the chart README.
- A broker socket cleanup failure at shutdown no longer masks the serve or web UI failure that ended
  service; it is logged as `broker_socket_cleanup_failed` and returned only when nothing more
  significant failed.
- The `--http-bind` web UI now serves at most sixteen concurrent connections, refusing rather than
  queuing further ones, and drops any connection that exceeds a thirty-second budget from accept to
  close; both ceilings are configurable through `dekopon_webui::serve_with_limits`.
- The web UI emits one `debug` tracing event per request with method, path, status, and response
  bytes, and no query string or body.
- Web UI provider pages are rendered once at broker startup instead of per request, the dashboard's
  agent inventory is shared by reference rather than deep-copied per render, and the provider page's
  "Manifest API" row now shows the manifest's own `apiVersion` value instead of the Rust variant
  name.
- The web UI's "Fuel yield interval" row now reports the interval `dekopon-broker-host` actually
  configures rather than re-deriving it.
- A route that names an image generator on the text-only WhatsApp transport is now a startup
  failure, rather than paying a model for a PNG that has no delivery path.
- A WhatsApp answer longer than one 4,096-scalar text message is now split at a line boundary and
  sent as consecutive messages instead of truncated, matching the Discord transport; a failure after
  the first chunk reports `partial-delivery` and records no delivered turn.
- A transport endpoint override must now be a literal loopback address (`127.0.0.1`, `::1`); the
  name `localhost` is no longer accepted, because what it resolves to is the resolver's decision.
- Chat replies now produce opaque receipts only after complete service/kernel transport acceptance;
  durable recording uses the exact bounded answer once and is never retried automatically.
- Storage-backed audit and telemetry omit raw identity/scope/provider fields and exact payload byte
  totals, using domain-separated keyed commitments and coarse counters instead.
- Storage now uses retained descriptor-relative tree operations, base→generation lease ordering,
  exact manifest/entry reservations, bounded finalization, strict recovery/quarantine, and
  canonical effective-authority generations independent of configuration ordering.
- Chat recording now uses service-typed scope-bound delivery identities and requires successful
  Slack/Telegram HTTP status; legacy subject-only attestors retain ordinary non-memory chat access.
- Memory composition now validates complete compaction/read/write/host-call/file/input/result/Wasm
  memory and fuel headroom, so every accepted default store can advance and query at its bounds.
- Catalog validation now scans the whole file and reports every duplicate, invalid name,
  unsupported API version, and missing reference in one list, instead of stopping at the first.
- `agent.spec.providers` is now held to the providers the agent's own capabilities route to, in
  both directions, so a rendered provider inventory can no longer drift from the capability list.
- The HTTP grant entry grammar — exact authorities, exact method tokens, and the entry-count cap —
  now lives once on `HttpConstraints::validate` and is enforced by `AuthorizationGate`,
  `dekopon-broker`, and `dekopon-http-host` alike. The broker's accepted set is unchanged; the gate
  and HTTP host no longer accept entries the broker refuses.
- The capability-shaped command-word refusal now explains the real mechanism: the shell resolves
  provider command words before capability fallback, so such a word would shadow the capability of
  that name.

### Removed

- Removed `dekopon-testkit`, which no workspace crate depended on, the `dekopon-capability`
  dependency of `dekopon`, `thiserror` from `dekopon-provider-sdk`, four unreferenced dependencies
  of `dekopon-run`, and three of `dekopon-telemetry`.
- Removed `CapabilityDescriptor` and `ProposedInvocation`'s unused `Deserialize` derive from
  `dekopon-capability`, `ResourceReader` from `dekopon`, and the single-implementation `Provider`
  trait from `dekopon-provider-host`, whose methods are now inherent on `WasmProvider`.
- A Cedar literal outside the Dekopon identifier grammar now returns `UnknownAction` or
  `UnknownProvider` in tolerant startup mode instead of an opaque validation error, and a policy
  that cannot be canonicalized for the policy digest refuses startup instead of silently degrading
  to source text.

### Fixed

- Bounded every Slack Socket Mode read, and opening a socket, with a 90-second liveness deadline,
  so a half-open connection is abandoned and reconnected instead of wedging every Slack route
  silently and indefinitely.
- Bounded `dekopond`'s exit after the shutdown grace expires, so abandoned blocking session work can
  no longer overshoot the pod's termination grace by a further full model timeout.
- `dekopond` now exits non-zero when every chat transport has ended without a requested shutdown,
  and re-announces individually dead transports as `gateway_transports_degraded` on an interval
  instead of logging them once.
- Telegram answers longer than 4,096 UTF-16 characters are now split losslessly across sequential
  messages instead of being rejected whole, which left the sender with no reply at all.
- Discord no longer holds its REST lock across a rate-limit wait, so one throttled reply stops
  delaying other sessions' answers and mid-session attachment refreshes.
- Gateway broker report failures now log a stable `category`, with `timeout` distinguished from a
  client failure, on both `gateway_agent_inventory_report_failed` and `gateway_usage_report_failed`.
- Serialized ChatGPT subscription refreshes across processes on an advisory lock beside the
  credential file, adopting a record another process rotated instead of presenting a refresh token
  the provider has already invalidated, and kept a turn alive on the freshly rotated in-memory
  credential when persisting it fails.
- Reported the endpoint's own error body on a failed chat completion, device authorization, or token
  request instead of a bare `http status: <code>`, including the OAuth `error` code that
  distinguishes an expired credential from a transient rejection.
- Kept a device login polling through a transient network failure until its fifteen-minute deadline
  rather than discarding the user code on one dropped packet.
- A transient `accept` failure on the broker's Unix listener — `EMFILE`, `ENFILE`, `ENOBUFS`,
  `ENOMEM`, `ECONNABORTED`, `ECONNRESET`, or `EINTR` — no longer exits the privileged daemon. It is
  logged as `broker_accept_retried` with its errno and retried after a bounded backoff; every other
  accept failure stays fatal.
- Broker shutdown now drains the Unix listener, the provider-storage GC, and the web UI concurrently
  against one shared deadline. They previously ran in sequence, each under its own full
  `shutdownGraceMs`, so a process with `--http-bind` could take two or three graces to exit against a
  `terminationGracePeriodSeconds` that budgets one.
- Startup frame validation now also bounds the capability response an attested session receives, not
  only each direct peer's. In a gateway deployment the peer holds almost nothing while the mapped
  principals hold the real capability sets, so an oversized response passed startup and then failed
  `write_frame` on every session open — the exact failure the check exists to prevent.
- A finished connection task is now observed as soon as it completes rather than on the next accept,
  so `broker_outcome_unaudited` no longer waits for unrelated traffic on a quiet broker.
- `--secret-name` is now validated per DNS-1123 label, so names such as `a.-b.c` are refused before
  the credential is printed rather than by `kubectl apply` afterwards.
- Configuration discovery no longer treats an unexaminable default candidate as absent; anything
  other than "not found" fails and names the candidate instead of loading a lower-precedence file.
- A single provider declaring one command word twice is now reported as exactly that, rather than
  as a collision between more than one provider.
- Telegram subjects are now required to be numeric in both the constructor and the parser, so an
  `identityMappings` typo such as `telegram.alice` is refused at broker startup instead of being
  accepted as an unmatchable subject.
- The Agent Slack manifest now requests public/private channel history events required to observe
  continuations; ambient traffic is discarded inside the transport before routing or inference.
- Storage reservations now size the candidate manifest with a fixed-width commitment placeholder
  instead of recomputing a content HMAC over every dirty file on every positional write, so
  appending frames costs bytes hashed linear in the change set rather than quadratic in file size.
  Reservations are byte-identical and durable manifests still carry real commitments over real
  bytes.
- `dekopon-run invoke --repeat N` emits one lifecycle log record for the first iteration plus one
  summary rather than one per iteration; failures still report individually and the JSON report is
  unchanged.
- OTLP over HTTP builds one blocking HTTP client per process instead of one per signal.
- OTLP export failures now report themselves. The OpenTelemetry `internal-logs` feature is enabled,
  so a rejected token, a missing `organization` header, or an unreachable receiver reaches the
  runner's stderr and the daemons' stdout instead of being discarded in silence; every binary keeps
  the `opentelemetry` target off its own OTLP layers so a failure can never be re-exported.
- `grpc` transport reaches an `https://` endpoint. The OTLP exporter now enables WebPKI roots for
  tonic, matching the workspace's TLS stance and the documented promise that both transports are
  first-class.
- The `service.version` resource attribute now carries the exporting executable's own version
  rather than `dekopon-telemetry`'s.
- `--otel-telemetry-payloads` is honored with no OTLP endpoint configured, so the flag can no
  longer be silently ignored; a `--trace` file is a sink like any other and follows the opt-in.
- `dekopon-run --max-frame-bytes` and `--io-timeout-ms` are now refused without `--broker`, like
  every other broker connection flag, instead of parsing and configuring nothing.
- The runner's broker-leg connect record carries `audit.event = "broker.leg.connected"`, the
  resolved socket tier, and the session trace, so it is queryable like every sibling audit event.
- Payload telemetry now emits `agent.model.prompt` whole on a session's first model turn and only
  the messages appended since the previous turn thereafter, ending the quadratic re-ship of one
  conversation's transcript.
- A textual chat asset is clamped to 256 KiB on the way into the prompt with a trailer naming the
  cut, instead of reaching the model whole and ending the session with a context-length rejection.
- A repeated `inspect_agent_config` call is answered with a short pointer at the copy already in
  the conversation rather than a second full serialization retained for the rest of the session.
- `IdSequence::new` now rejects a prefix whose derived invocation identifiers would exceed the
  identifier length bound, instead of constructing a session whose every capability call fails.
- A broker capability snapshot naming the same capability twice is refused, listing every repeat,
  rather than silently last-winning into disagreeing `cap --list` and `inspect_agent_config` views.
- Removed a run of spaces from the model-facing scripting tool description.
- Direct `dekopon-run` provider loads now report every duplicate provider, duplicate capability, and
  command-word conflict in one failure instead of stopping at the first, and refuse the same
  command-word conflicts `dekopon-brokerd` refuses at startup.
- Immediate-mode `provider.invoke` spans now carry input/output byte counts and remaining fuel, and
  a deadline, output-ceiling, or component failure emits a `WARN` naming the wall it hit.
- One long-lived deadline worker per runtime now arms each direct provider call instead of a thread
  spawned and joined per describe and invoke.
- A direct provider call that completes at its deadline boundary now returns its output; only a
  failed call is reported as a timeout, and it keeps the Wasmtime error as the timeout's source.
- A broker provider artifact is now read once and its recorded SHA-256 is of the exact buffer
  Cranelift compiled, replacing a before/after comparison that a change-and-revert could pass. The
  unreachable `ArtifactChanged`, `DuplicateProvider`, and `DuplicateCapability` broker-host error
  variants are gone.
- A provider exporting `resolve-command` with the wrong signature is now reported as a type
  mismatch instead of as an absent export, and the export is proven from the component's own type
  rather than by instantiating it once at startup.
- A provider exporting `resolve-command` with the wrong signature is now reported as a type
  mismatch instead of as an absent export, and the export is proven from the component's own type
  rather than by instantiating it once at startup.
- A provider whose manifest cannot be serialized now describes itself with the serialization error
  instead of an empty description a host misdiagnoses.
- `HttpError` renders the `dekopon:http@1.0.0` kebab-case error names instead of Rust variant
  spelling.
- A Cedar literal outside the Dekopon identifier grammar now returns `UnknownAction` or
  `UnknownProvider` in tolerant startup mode instead of an opaque validation error, and a policy
  that cannot be canonicalized for the policy digest refuses startup instead of silently degrading
  to source text.
- An abandoned `jq` filter is now logged with its elapsed time and counted; `jq` refuses to start a
  new filter once too many non-terminating workers are still running in the process, and
  `dekopon_shell::abandoned_filter_workers` reports the live count.
- Script output no longer loses an oversized line entirely: a line too large for the retained tail
  is kept as a clamped prefix, so a script's large final result survives truncation.
- A `grep` or `sed` pattern ending in an escaped `\$` now matches a literal dollar sign instead of
  being read as an end anchor plus a stray backslash.
- A command substitution now honors a suppressed trailing newline, so `v=$(printf '%s' a; printf
  '%s' b)` is `ab` rather than `a\nb`.
- A command that produces no value no longer contributes a blank line to a command substitution, so
  `$(true; echo a)` and `$(echo a; true)` are both `a` rather than gaining a leading or trailing
  newline.
- A capabilities answer now costs one Cedar pass instead of two. The capability listing and the
  command words are derived from a single authorized constraint-set filter, `capability_view`, used
  by the readiness probe's `capabilities`, `capabilitiesFor`, `capabilitiesForChat`, and the startup
  frame check. Both halves still come from the one evaluation an invocation would receive, so a
  listing can no more disagree with a decision than before.
- The durable audit log no longer retains every record hash and a second copy of every restored
  invocation identifier for the life of the process — roughly 30 MB at the production caps, for
  state read only at startup. It keeps the one-record reconcile window `contains_checkpoint`
  actually needs, and `FileAuditLog::replay_ids` becomes the consuming `take_replay_ids`, because
  the broker's own replay ledger owns those identifiers from then on. Records on disk are
  byte-identical, and an audit file written by an earlier build still verifies.
- Hot-path allocations are gone from the paths every chat message crosses: an audit event is
  serialized once for both its hash material and its durable line, evidence digests stream into the
  hasher instead of copying an entire provider response to prefix a label, `ExternalSubject`
  namespace checks and identity resolution compare segments instead of building a canonical string,
  identifier deserialization reuses its buffer, and `dekopon-policy` parses its constant Cedar
  entity type names once at construction rather than three times per authorization.
- `ClientError::Protocol` now carries the exchange phase and renders its bounded framing detail, and
  `ClientError::may_have_executed` covers both a lost response and `outcome-unaudited`. A script
  reaching that state receives `denied` (exit `126`) instead of a retryable failure. Invalid frame
  bounds now surface as the separate `ClientError::Limits`, and `capabilities_for` is removed in
  favor of `session_surface_for`.
- Frame payloads are now read incrementally, so a peer-claimed in-bound length no longer allocates
  before any payload byte arrives, and one frame is written with one `write_all` instead of two.
- `AgentInventory::validate` and `ModelUsageReport::validate` name the offending agent and the exact
  bound; `dekopon-brokerd` logs that reason server-side while the wire message stays generic.
- Native HTTP telemetry now records the failure class and its static sanitized message, emits the
  `accounting.http.request` record for every attempt including one refused before a destination was
  resolved, omits status rather than reporting `0` when no response arrived, and publishes accounted
  bytes under `dekopon.http.*.accounted_bytes` instead of the OTel payload-size names.
- Broker-mediated HTTP now reuses one resolution and one pinned client per execution context while
  the `(host, addresses)` pin set is unchanged, so a multi-call capability shares a warm connection
  instead of paying a fresh lookup and TLS handshake per request.
- Destinations resolving to more addresses than the native pin set holds are now deduplicated and
  truncated, with every retained address still validated, instead of failing the whole request.
- Fixed `http.request` spans mis-parenting concurrent trace events: the span is now attached with
  `Instrument` rather than an entered guard held across DNS, connection, and body awaits.
- Fixed IPv6 literal destinations, which could never match a grant or be resolved because the URL
  host was carried bracketed into the canonical authority and the resolver lookup.
- Native HTTP client-builder failures are now reported as `internal` rather than as a wire-level
  protocol failure.
- HTTP call evidence now records a status-less entry for a request the credential binding refuses,
  so evidence counts reconcile with the request budget the attempt consumed.
- `docs/broker-http.md` now documents the `provider-error` failure code, the deliberately ungated
  `resolveCommand` operation, and a version-and-compatibility section stating that all four
  executables upgrade together; its startup-validation section no longer claims every entity literal
  is proved, since agent names are the deliberate exception.
- `docs/run.md` no longer describes the gateway chat client as stateless: `--subject` plus
  `--conversation` selects a persistent history on a `persistent` route, including one a chat-service
  sender created.
- Corrected the `dekopon-model` attachment-rendering example, added `resolveCommand` to the
  `dekopon-broker-protocol` README, fixed the `dekopon-provider-sdk` WIT-package description and
  documented `export_provider_with_commands!`, dropped the stale `0.1.x` and `0.1.0` version pins
  from `docs/cli.md` and `dekopon-capability`, and gave `Broker::capabilities` its own rustdoc.
- A chat-service reply that is not valid JSON is now reported as `malformed-response` carrying the
  parse position, distinct from the well-formed-but-missing-field `response` class it previously
  shared, so an interposed proxy's HTML error page and a renamed API field no longer look alike.
- A broker `policy-denied` the policy engine never evaluated — a request the Cedar schema does not
  admit — now emits `policy.request.refused` naming the reason. The wire result and audit reason
  stay `policy-denied`, so the taxonomy callers act on is unchanged.
- A retained storage document that fails to decode now emits `storage_document_decode_failed` with
  the decode class, line, and column, and nothing else; a corruption error previously named a scope
  and left no description of what was actually wrong with the retained state.
- Broker daemon configuration failures now name what refused them: an invalid storage path, storage
  limit, or frame limit reports the offending field and its underlying cause rather than collapsing
  into one shared "invalid limits" message.
- Native HTTP malformed-URI failures now carry the parser's reason, and a failed DNS lookup is
  traced inside the `http.request` span rather than being reduced to `outcome=failed`.
- `runner.command` no longer holds a `tracing` span guard across suspension points, so a concurrent
  task's events could parent under the wrong span and the holder's own events lost their parent on
  resume. The span instruments the section instead, matching `http.request` and `broker.authorize`.
- The workspace now denies `await_holding_invalid_type` for `tracing`'s `Entered` and `EnteredSpan`,
  so a span guard held across an `.await` is a compile error rather than a review catch.

### Security

- Swept abandoned `chatgpt-auth.tmp-*` staging files, which hold access and refresh tokens in the
  clear, on every credential save and on `dekopon auth chatgpt logout`, and `fsync`ed the credential
  directory after the rename so a rotated credential cannot be lost to a power failure.
- Direct `dekopon-run` provider stores now bound table elements, tables, linear memories, and core
  instances as well as linear-memory size, closing a `table.grow` path that could allocate far past
  `--max-memory-bytes`.
- Script traces now open one `shell.script` span per run carrying the whole run's command totals,
  and emit only the first 256 `shell.command` spans at `INFO` so a loop-heavy script cannot export
  one span per step.
- A piped value now moves from one pipeline stage to the next and is shared with a function body's
  statements rather than deep-copied for each of them, and `grep` no longer copies every input line
  it tests.
- Command-word and namespace resolution now ask `CapabilityInvoker` membership questions
  (`has_command_word`, `grants_namespace`) instead of materializing, sorting, and deduplicating both
  session legs' capability and command-word lists for every command a script runs. Resolution order
  is unchanged.
- `jq` reuses one filter worker per thread instead of spawning and joining a thread per call, and
  values cross that boundary directly rather than through JSON text on each side. A filter output
  JSON cannot represent — `nan`, `infinite`, a byte string, a non-string object key, or nesting
  past 128 containers — is still refused, now by name rather than as a parse error, and a float no
  longer loses its last bit to the round trip.
- WhatsApp webhook HMAC is checked over exact raw bytes before parsing; WABA/phone scope and sender
  come only from the signed envelope, transport secrets remain gateway-only, and outcome-unknown
  Graph sends are never blindly retried.
- Every refused WhatsApp webhook request now names its reason in telemetry, rate-limited to one
  content-free line per reason per minute carrying the count it stands for, so a wrong app secret is
  visible without letting an unauthenticated caller drive log volume.
- A transport credential environment variable that is exported but blank is now a startup failure
  naming the variable: an empty HMAC key verifies signatures anyone can compute, and an empty bearer
  token is still sent as a header.
- A failed WhatsApp `accept()` is now classified instead of ending the listener: a dead connection is
  ignored and descriptor or buffer exhaustion is retried after a short pause, so one transient
  failure can no longer silently take the only inbound transport off the air for the process's life.
- Generated images use one fixed public model endpoint, one attempt and one 8 MiB PNG per session,
  gateway-owned filenames and authenticated reply targets, and never enter prompts, conversation
  memory, durable chat records, telemetry payloads, provider components, or broker protocol.
- Direct `dekopon-run`, legacy broker operations, and generic chat invocation cannot discover or
  execute hidden memory recording. Storage imports receive only an exact interface/access grant.
- Documented finite JSONL dedup capacity, no automatic replay/deletion/export, no encryption-at-rest
  claim, native-I/O timeout and same-UID filesystem limitations, and no database/WAL/SHM claim.
- Every web UI response now carries the closed no-store/nosniff/no-referrer/CSP header set,
  including the 405 axum produces for a mutating method, which previously left the router unprotected.
- Bound Slack continuation ownership to an exact transport-authenticated workspace/channel/thread/
  sender tuple, claimed only after fresh authorization, revoked on refusal, capped in memory, and
  cleared on restart. A decline selected alongside work runs nothing, and capability work requires
  a visible reply—or a fixed audit-before-retry warning when the turn budget is exhausted—rather
  than permitting the model to hide an effect.

## [dekopon-chart-0.2.0] - 2026-08-22

### Added

- Added an opt-in chart-managed ClusterIP Service and readiness-gated gateway port for an
  operator-owned exact-path webhook ingress.

### Changed

- The Helm chart's `terminationGracePeriodSeconds` now covers both drains in sequence — the
  gateway's and then the broker's `shutdownGraceMs` plus `drainBudget.bufferSeconds`, 270 s at the
  shipped defaults — and `helm template` refuses a shorter budget instead of letting the kubelet
  SIGKILL a draining broker mid-invocation and mid-audit-append.

### Fixed

- The Helm chart's `appVersion` now names the current application release, so
  `app.kubernetes.io/version` and a default `image.tag` stop reporting `0.4.0` on pods running a
  later one.
- `charts/dekopon/README.md` separates "never installed on a cluster" from "not published"; chart
  `0.1.0` and the container image it pulls are both published.
- Retained Helm chart claims now also carry `argocd.argoproj.io/sync-options:
  Prune=false,Delete=false`, so a GitOps prune cannot delete the audit chain, its checkpoint, the
  live ChatGPT credential, or durable provider data that only a Helm-uninstall annotation protected.

## [0.9.0] - 2026-08-20

### Added

- Added the opt-in Exploration-only `skylight-private` broker provider proof of concept with two
  unofficial, private, unsupported, mock-only account/frame reads. It is absent from default
  catalogs, images, policies, and deployments (#120).
- Added opt-in native in-flight activity for Slack, Discord, and Telegram chat sessions: Slack Agent
  `processing`/Stop lifecycle with classic/free `:tangerine:` reaction fallback, Discord typing,
  Telegram topic-aware chat actions, separate classic/Agent Slack manifests, and Slack's required
  Agent View App Home event subscription (#122, #124).

### Changed

- Application release tags now publish all crates.io packages automatically through trusted
  publishing; manual workflow dispatch remains an idempotent recovery path (#121).

### Security

- Bound each Skylight read to one fixed HTTPS GET and a static short-lived destination-bound broker
  bearer, while keeping authorization and OAuth out of the guest and projecting only bounded IDs
  and optional frame names. The pinned pyskylight MIT notice is adjacent to source and artifact
  (#120).
- Derive activity and Stop targets only from authenticated chat envelopes, start activity only after
  fresh broker authorization, prevent model-controlled status content, and cooperatively suppress
  later model/tool work, stale answers, and history commits after a Slack Agent Stop event (#122).

## [0.8.1] - 2026-08-20

### Fixed

- Raised the per-model-turn tool-call ceiling from four to ten so bounded multi-attachment requests
  reach the attachment-specific limit instead of failing the entire session as runaway fan-out
  (#118).

## [0.8.0] - 2026-08-20

### Added

- Added Discord Gateway v10 transport support for direct messages, explicit guild mentions,
  resumable sessions, no-ping replies, and bounded lazy photo/file attachments with signed-URL
  refresh (#114).

### Changed

- Backfilled release history and made pull-request and tag validation require a dated, non-empty
  changelog entry for future application and Helm chart releases (#113).

### Security

- Discord uses only non-privileged message intents, derives `discord.<user id>` from authenticated
  Gateway payloads, sends bot credentials only to pinned Discord REST/Gateway origins, and sends no
  token to host-validated CDN downloads. Model-authored replies cannot trigger Discord mentions.

## [0.7.0] - 2026-08-19

### Added

- Added authorized `inspect_agent_config` gateway introspection, exposing a bounded view of the
  agent's exact standing instructions, model class, session limits, conversation settings, and the
  sender's freshly authorized capability metadata (#109).
- Added typed, opt-in agent-configuration views to `dekopon-agent` and an
  `agent.config.inspected` telemetry event; `dekopon-run prompt` does not enable this tool.

### Changed

- Clarified Slack onboarding for the separate `xapp-…` Socket Mode and `xoxb-…` bot tokens,
  including scopes, environment variables, revocation, and reinstall behavior (#108).

### Fixed

- Made agent configuration inspection repeatable under the prompt loop's shared per-turn tool-call
  and model-step limits (#110).

### Security

- Configuration inspection reuses the sender's fresh effective capability snapshot, grants no
  authority, and omits policy source, constraints, identities, endpoints, denied or merely declared
  capabilities, and credential references or values.
- Required an empty argument object and limited each result to 128 KiB. Inspection consumes no
  capability budget, makes no broker invocation, and creates no durable broker audit record.
- Authorized users can retrieve standing instructions verbatim; operators must not place secrets
  in system prompts.

## [0.6.0] - 2026-08-19

### Added

- Added the opt-in `dekopon-webui` dashboard to `dekopon-brokerd`, showing provider metadata,
  bounded runtime metrics, gateway-reported agent inventory, and model-token totals (#103).
- Added bounded, best-effort gateway status reporting over the local broker protocol. Reports are
  process-local and informational, not authorization, audit, or billing records.

### Changed

- Replaced the approval-oriented rubber-stamper example with a comment-only PR summarizer/linter
  using existing narrow GitHub capabilities and a head-pinned review (#105).
- Centralized and validated the crates.io publication order while reducing duplicated release
  builds and improving recovery caches (#104).

### Fixed

- Forwarded `expectedHeadSha` for comment and request-changes reviews as well as approvals (#105).

### Security

- The dashboard opens no port unless `--http-bind` is supplied. It is deliberately unauthenticated
  and read-only, so the selected bind address and surrounding network are its access boundary;
  displayed gateway reports never influence policy or execution.
- Rejected userinfo in OTLP endpoint URLs and failed broker startup if a provider artifact changed
  while being compiled.

## [0.5.0] - 2026-08-19

### Added

- Added bounded, on-demand Slack and Telegram attachment access, stable per-conversation asset
  references, and multimodal image/file model messages (#94–#98).
- Added provider-declared shell command words and the optional `provider-commands@0.2.0` resolver
  surface; no bundled provider declared a new command word in this release (#89–#90).
- Added deterministic, non-recursive provider-directory loading for `dekopon-brokerd` and
  `dekopon-run`, with ownership, mode, and provider-count checks (#85).

### Changed

- Made broker startup tolerant of policy references to unloaded providers by default, with
  structured warnings and a `strict: true` compatibility mode; unrouted or unconstrained
  invocations remain denied (#84).
- Changed the alpha broker protocol for policy-filtered command words and command resolution,
  requiring lockstep deployment of broker and clients (#90).
- Rewired release automation to invoke container and Homebrew delivery directly after GitHub
  release creation instead of relying on an event that `GITHUB_TOKEN` could not trigger (#82).
- Reported guessed commands in an authorized namespace as `not-granted` while withholding the
  guessed word unless payload telemetry is enabled (#86).

### Fixed

- Restored Slack and Telegram upload routing, captions, attachment continuity after follow-up turns
  and history trimming, and CommonMark rendering in Slack responses (#81, #83, #96–#98).

### Security

- Constrained attachment reads by media type, size, attempt count, inventory size, and validated
  Slack redirect hosts; retained state and default telemetry contain references and metadata rather
  than attachment bytes.
- Updated `h2` to address RUSTSEC-2026-0258 denial-of-service risks from empty DATA frames (#92).

## [dekopon-chart-0.1.0] - 2026-08-18

### Added

- Released the initial independent Helm chart for application 0.4.0. The default render creates
  configuration, retained state, and a singleton broker deployment with deny-by-default sample
  policy (#71).
- Added an optional co-located `dekopond` gateway, broker-socket startup gating, inline or
  existing-object configuration sources, and seed-once persistent ChatGPT credentials with
  explicit destructive reseeding.

### Changed

- Enforced one replica with `Recreate`, a retained RWO state claim, and real broker-socket startup
  and readiness probes; the chart creates no Service or Ingress.
- Published the chart independently as
  `oci://ghcr.io/dekopon-agents/charts/dekopon:0.1.0`.

### Security

- Defined the pod and UID/GID 65532 as one deliberate trust domain. Daemons run non-root with
  read-only roots, dropped capabilities, RuntimeDefault seccomp, and no service-account token.
- Used a narrowly privileged root init container to copy projected secrets into owner-only regular
  files. Inline credentials remain visible in Helm release values, so existing Secrets are
  preferred; the init requirement targets Pod Security `baseline`, not `restricted`.

## [0.4.0] - 2026-08-18

### Added

- Added `dekopon auth chatgpt export` for rendering an existing local ChatGPT login as canonical JSON
  or a Kubernetes Secret (#72).
- Added the first multi-architecture container image for Linux AMD64/ARM64 and Homebrew installation
  for all four Dekopon executables (#70, #75).
- Added an operational 1Password/External Secrets guide without claiming a built-in
  `ExternalSecret` or secret-store integration (#73).

### Changed

- Reduced release archives to macOS ARM64 and Linux ARM64/x86-64, retiring Intel macOS artifacts
  (#74).
- Kept application authority and process boundaries otherwise unchanged from 0.3.0; the Helm chart
  is documented in its separate `dekopon-chart-0.1.0` entry.

### Security

- Made credential export an explicit cleartext escape hatch requiring `--expose-credential`,
  rejecting quiet mode and terminal output without another acknowledgement, and performing no
  network request. Exported credentials are rotation-sensitive seeds, not backups.

## [0.3.0] - 2026-08-17

### Added

- Added the unprivileged `dekopond` gateway with bounded Slack Socket Mode, Telegram long polling,
  and owner-only local development transports (#55).
- Added opt-in in-memory persistent conversations, opaque prompt-cache routing, and
  `dekopon-run chat`; one-shot routing remains the default (#58–#63).
- Added shared bounded prompt/session handling in `dekopon-agent`, including catalog standing
  instructions.
- Added a broker-only GitHub provider and constrained `gh` shell surface for 19 separately
  authorized read and write capabilities, without an API or GraphQL passthrough.

### Changed

- Replaced exact-match broker rules with bounded Cedar authorization. This is a breaking
  configuration migration from `rules` to `policiesPath` and `constraintSets` despite retaining the
  `v1alpha1` API version.
- Expanded release packaging and privilege-boundary checks to all four executables and 20 public
  crates, and raised the Rust MSRV to 1.89.0 (#69).

### Fixed

- Corrected Slack thread identity, strict direct-message decoding, bounded conversation-history
  construction, and packaging of `dekopond` and provider examples (#57–#59, #67, #69).

### Security

- Added broker-held destination-bound bearer credentials, canonical external subjects,
  owner-controlled mappings, attestor namespaces, and a separate `agent.prompt` authorization
  gate. Persistent conversations cache text, not authorization; each message obtains fresh broker
  authority.

## [0.2.0] - 2026-08-16

_This is the first application tag represented in repository history. The last 0.1-versioned
snapshot is only a comparison marker; no authenticated `v0.1.0` tag exists._

### Added

- Added the Unix-only privileged broker daemon and unprivileged runner clients with peer-UID
  authentication, bounded framing and concurrency, deny-by-default authorization, replay-resistant
  invocations, and graceful draining (#20–#27).
- Added policy-constrained native HTTP provider execution and a JSONPlaceholder demonstration with
  separately authorized read and external-write capabilities (#20–#26).
- Added the bounded `dekopon-shell` interpreter and a model prompt mode exposing one bash-style
  scripting tool with direct-first, broker-fallback capability dispatch (#34–#35).
- Added correlated runner/broker OTLP tracing, structured broker logs, sanitized accounting events,
  and an OpenObserve smoke-test deployment (#36, #47–#54).

### Changed

- Configured tag releases to validate the workspace and assemble provenance-attested Linux and
  macOS archives containing the three executables and provider fixtures.

### Fixed

- Preserved sanitized HTTP evidence when an external effect succeeded before guest failure,
  rejected disguised plaintext bearer-token hosts, and distinguished broker-unavailable from
  potentially effected but unaudited outcomes (#29–#31).
- Hardened shell parsing, resource bounds, environment isolation, deadlines, and telemetry exporter
  validation.

### Security

- Kept direct execution structurally separate from privileged broker and native-HTTP crates, and
  enforced destination, method, TLS, DNS, header, redirect, and resource constraints in the broker
  HTTP path.
- Added owner-only hash-linked audit records with checkpoint recovery and payload-redacted
  telemetry, and updated Wasmtime to 36.0.13 for RUSTSEC-2026-0222 (#23, #27, #32).

[Unreleased]: https://github.com/dekopon-agents/dekopon/compare/v0.11.1...HEAD
[0.11.1]: https://github.com/dekopon-agents/dekopon/compare/v0.11.0...v0.11.1
[0.11.0]: https://github.com/dekopon-agents/dekopon/compare/v0.10.0...v0.11.0
[dekopon-chart-0.2.1]: https://github.com/dekopon-agents/dekopon/compare/dekopon-chart-0.2.0...dekopon-chart-0.2.1
[0.10.0]: https://github.com/dekopon-agents/dekopon/compare/v0.9.0...v0.10.0
[dekopon-chart-0.2.0]: https://github.com/dekopon-agents/dekopon/compare/dekopon-chart-0.1.0...dekopon-chart-0.2.0
[0.9.0]: https://github.com/dekopon-agents/dekopon/compare/v0.8.1...v0.9.0
[0.8.1]: https://github.com/dekopon-agents/dekopon/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/dekopon-agents/dekopon/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/dekopon-agents/dekopon/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/dekopon-agents/dekopon/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/dekopon-agents/dekopon/compare/v0.4.0...v0.5.0
[dekopon-chart-0.1.0]: https://github.com/dekopon-agents/dekopon/releases/tag/dekopon-chart-0.1.0
[0.4.0]: https://github.com/dekopon-agents/dekopon/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/dekopon-agents/dekopon/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/dekopon-agents/dekopon/releases/tag/v0.2.0
