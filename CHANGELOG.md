# Changelog

All notable changes to Dekopon are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Application headings map to
`vX.Y.Z` Git tags; independently versioned Helm releases retain their full
`dekopon-chart-X.Y.Z` tag name. Release dates are the annotated tagger dates.

## [Unreleased]

### Added

- Added the opt-in Exploration-only `skylight-private` broker provider proof of concept with two
  unofficial, private, unsupported, mock-only account/frame reads. It is absent from default
  catalogs, images, policies, and deployments.
- Added opt-in native in-flight activity for Slack, Discord, and Telegram chat sessions: Slack Agent
  `processing`/Stop lifecycle with classic/free `:tangerine:` reaction fallback, Discord typing,
  Telegram topic-aware chat actions, and separate classic/Agent Slack manifests.

### Changed

- Application release tags now publish all crates.io packages automatically through trusted
  publishing; manual workflow dispatch remains an idempotent recovery path.

### Security

- Bound each Skylight read to one fixed HTTPS GET and a static short-lived destination-bound broker
  bearer, while keeping authorization and OAuth out of the guest and projecting only bounded IDs
  and optional frame names. The pinned pyskylight MIT notice is adjacent to source and artifact.
- Derive activity and Stop targets only from authenticated chat envelopes, start activity only after
  fresh broker authorization, prevent model-controlled status content, and cooperatively suppress
  later model/tool work, stale answers, and history commits after a Slack Agent Stop event.

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

[Unreleased]: https://github.com/dekopon-agents/dekopon/compare/v0.8.1...HEAD
[0.8.1]: https://github.com/dekopon-agents/dekopon/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/dekopon-agents/dekopon/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/dekopon-agents/dekopon/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/dekopon-agents/dekopon/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/dekopon-agents/dekopon/compare/v0.4.0...v0.5.0
[dekopon-chart-0.1.0]: https://github.com/dekopon-agents/dekopon/releases/tag/dekopon-chart-0.1.0
[0.4.0]: https://github.com/dekopon-agents/dekopon/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/dekopon-agents/dekopon/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/dekopon-agents/dekopon/releases/tag/v0.2.0
