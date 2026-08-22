# Changelog

All notable changes to Dekopon are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Application headings map to
`vX.Y.Z` Git tags; independently versioned Helm releases retain their full
`dekopon-chart-X.Y.Z` tag name. Release dates are the annotated tagger dates.

## [Unreleased]

### Fixed

- Serialized ChatGPT subscription refreshes across processes on an advisory lock beside the
  credential file, adopting a record another process rotated instead of presenting a refresh token
  the provider has already invalidated, and kept a turn alive on the freshly rotated in-memory
  credential when persisting it fails.
- Reported the endpoint's own error body on a failed chat completion, device authorization, or token
  request instead of a bare `http status: <code>`, including the OAuth `error` code that
  distinguishes an expired credential from a transient rejection.
- Kept a device login polling through a transient network failure until its fifteen-minute deadline
  rather than discarding the user code on one dropped packet.

### Security

- Swept abandoned `chatgpt-auth.tmp-*` staging files, which hold access and refresh tokens in the
  clear, on every credential save and on `dekopon auth chatgpt logout`, and `fsync`ed the credential
  directory after the rename so a rotated credential cannot be lost to a power failure.
### Added

- Added a first text-only Meta WhatsApp Cloud API gateway transport with a signed bounded webhook,
  process-local message-ID deduplication, canonical `whatsapp.<wa_id>` subjects, and pinned Graph
  API text replies.
- Added an opt-in chart-managed ClusterIP Service and readiness-gated gateway port for an
  operator-owned exact-path webhook ingress.
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

### Added

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

### Added

- `broker.authorize` now records `policy.errors_present`, and a Cedar evaluation error denies with
  the distinct reason `policy-error` instead of being indistinguishable from `policy-denied`.
  `broker.execute` records `outcome` and the classified `error`.

### Fixed

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

### Added

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

### Fixed

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
- `broker.authorize` no longer stays entered on its worker thread while the authorizing task is
  suspended. The section awaits the replay ledger and, on every denial, a durable audit append that
  fsyncs, so whatever the runtime polled next on that thread was exported as a child of another
  request's authorization while that request's own later events lost it. The span instruments the
  section instead of being held across the awaits; its fields and values are unchanged.

### Changed

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

### Fixed

- Fixed `http.request` spans mis-parenting concurrent trace events: the span is now attached with
  `Instrument` rather than an entered guard held across DNS, connection, and body awaits.
- Fixed IPv6 literal destinations, which could never match a grant or be resolved because the URL
  host was carried bracketed into the canonical authority and the resolver lookup.
- Native HTTP client-builder failures are now reported as `internal` rather than as a wire-level
  protocol failure.
- HTTP call evidence now records a status-less entry for a request the credential binding refuses,
  so evidence counts reconcile with the request budget the attempt consumed.
- The Agent Slack manifest now requests public/private channel history events required to observe
  continuations; ambient traffic is discarded inside the transport before routing or inference.
- `docs/broker-http.md` now documents the `provider-error` failure code, the deliberately ungated
  `resolveCommand` operation, and a version-and-compatibility section stating that all four
  executables upgrade together; its startup-validation section no longer claims every entity literal
  is proved, since agent names are the deliberate exception.
- `docs/run.md` no longer describes the gateway chat client as stateless: `--subject` plus
  `--conversation` selects a persistent history on a `persistent` route, including one a chat-service
  sender created.
- `charts/dekopon/README.md` separates "never installed on a cluster" from "not published"; chart
  `0.1.0` and the container image it pulls are both published.
- Corrected the `dekopon-model` attachment-rendering example, added `resolveCommand` to the
  `dekopon-broker-protocol` README, fixed the `dekopon-provider-sdk` WIT-package description and
  documented `export_provider_with_commands!`, dropped the stale `0.1.x` and `0.1.0` version pins
  from `docs/cli.md` and `dekopon-capability`, and gave `Broker::capabilities` its own rustdoc.

### Security

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
