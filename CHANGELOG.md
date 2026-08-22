# Changelog

All notable changes to Dekopon are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Application headings map to
`vX.Y.Z` Git tags; independently versioned Helm releases retain their full
`dekopon-chart-X.Y.Z` tag name. Release dates are the annotated tagger dates.

## [Unreleased]

### Added

- Added `dekopon-provider-sdk-testkit`, an in-process fake broker that loads a provider component
  and runs it against a real `StorageHost` over a temporary root, so storage-backed providers can
  be tested end to end. It skips Cedar and the constraint catalog by minting authorization through
  `AuthorizationGate`, and defaults its per-invocation ceilings from the host limits in force
  rather than restating them. Storage grants are minted per invocation from constant scope
  material, so successive calls reach one durable namespace.
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

### Changed

- A whole right-hand side keeps its value rather than collapsing to text for a bare `$NAME` as well
  as a bare `$(cmd)`, so `copy=$obj` followed by `${copy[key]}` works. Previously only the
  substitution spelling survived, and `copy=$obj` silently flattened the object into its JSON text.
- The shell no longer rejects file-descriptor redirection wholesale. Descriptors other than 1 and 2
  (`3>`, `<&`) are still refused by name, as is `2>&1` written *before* the redirection it copies:
  bash duplicates the file description there and leaves stderr on the terminal, this interpreter
  has destinations rather than descriptions, and that spelling is the one a script writes when it
  believes it captured output that went elsewhere.

### Fixed

- Held the `test (Rust 1.89.0)` job to the MSRV it names. `rust-toolchain.toml` pins `channel =
  "stable"`, which outranks the `rustup default` the toolchain action sets, so the job installed
  1.89.0, used it only for a cache key, and then compiled on current stable. It now exports
  `RUSTUP_TOOLCHAIN` and fails loudly if the effective `rustc` or the workspace `rust-version`
  drifts from the pin.

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

[Unreleased]: https://github.com/dekopon-agents/dekopon/compare/v0.9.0...HEAD
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
