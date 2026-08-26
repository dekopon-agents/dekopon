# Development guide

Read [`design.md`](design.md) before this guide. The design defines authority; this document maps common changes to source, tests, generated artifacts, and validation commands.

## Start here

From the repository root (`Cargo.toml`, `AGENTS.md`, and `docs/` should be present):

1. Run `git status --short --branch` and preserve unrelated work.
2. Classify the change as **Current**, **Committed direction**, or **Exploration**.
3. Read the area document selected by [`../AGENTS.md`](../AGENTS.md).
4. Find the implementation and its nearest tests before editing.
5. Check whether the change crosses the root workspace, a separate provider workspace, a generated artifact, or a mirrored contract.

Prefer targeted tests while iterating, then run the scope-appropriate checks below. Do not claim a command or remote check passed unless it was actually observed.

## Repository map

| Area | Primary implementation | Behavior tests and fixtures |
|---|---|---|
| Domain identifiers and enums | `crates/dekopon-core/src/lib.rs` | Inline unit and compile-fail tests |
| Proposal/authorization typestate | `crates/dekopon-capability/src/lib.rs` | Inline unit tests |
| Resource wire types | `crates/dekopon-protocol/src/lib.rs` | Inline schema and round-trip tests |
| Config discovery and validation | `crates/dekopon-config/src/` | `crates/dekopon-config/src/tests.rs`; `crates/dekopon-config/tests/examples.rs` loads `examples/local/dekopon.yaml` and `examples/conditional-write/dekopon.yaml` |
| OTLP exporter settings and subscriber wiring | `crates/dekopon-telemetry/src/` | Inline endpoint, transport, and environment-credential tests |
| Operator CLI and model auth commands | `crates/dekopon/src/` | `crates/dekopon/tests/cli.rs` |
| Interactive console: state machine, session driving, observation decorators, redaction, panes | `crates/dekopon-tui/src/` | Inline state-machine, key-handling, decorator-ordering, transcript-folding, and redaction tests, plus `crates/dekopon-tui/tests/render.rs` over a `TestBackend` |
| Model clients, bounded OpenAI image generation, and ChatGPT auth | `crates/dekopon-model/src/` | Inline mock HTTP/OAuth/SSE/base64/byte-bound tests |
| Provider guest API and adapter | `crates/dekopon-provider-sdk/src/lib.rs`, `crates/dekopon-provider-sdk/wit/` | Inline adapter tests |
| Buffered HTTP WIT and guest facade | `wit/http/`, `crates/dekopon-provider-http/` | Guest validation and mirrored-contract tests plus WIT package workflow |
| Provider storage WIT and guest facade | `wit/storage/`, `crates/dekopon-provider-storage/` | Feature/import inspection, mirror comparisons, package workflow |
| Native provider storage | `crates/dekopon-storage-host/src/{config,key,layout,namespace,quota,transaction,jsonl,vfs,gc,metrics}.rs` | Path/key/quota/transaction/restart/continuity tests plus broker-host component integration |
| Bounded native HTTP host | `crates/dekopon-http-host/src/` | Inline destination, method, DNS, header, bound, and loopback mock-server tests |
| Broker async component host | `crates/dekopon-broker-host/src/`, `crates/dekopon-broker-host/wit/` | Inline adapter tests plus `crates/dekopon-broker-host/tests/host.rs` authorization-boundary, Wasmtime, and loopback tests |
| Cedar policy adapter | `crates/dekopon-policy/src/lib.rs` | `crates/dekopon-policy/src/tests.rs` validation-refusal, deny-by-default, context-matching, explanation, and digest-stability tests |
| Broker authorization, evidence, and audit core | `crates/dekopon-broker/src/lib.rs` | Inline context/hash-chain/durable-file tests, `crates/dekopon-broker/tests/broker.rs` constraint-validation, redaction, and replay-restart tests, and `crates/dekopon-broker/tests/policy_decisions.rs` for the workflow decision table |
| Broker local protocol/client | `crates/dekopon-broker-protocol/src/lib.rs` | Inline strict framing, deadline, authority-omission, socket-metadata, and peer-UID tests |
| Authenticated Unix broker service, private secret sources, and offline provider manager | `crates/dekopon-brokerd/src/`; strict public-DRN/private-map adapters in `secrets.rs`; provider set/lock, bounded OCI transport, content store, and lifecycle commands in `provider_manager.rs` | Inline strict-config/socket/CLI tests; secret-map aggregate validation, strict JSON/YAML projection, secure-file and mock-backed 1Password/Vault/AWS/GCP/Azure/Kubernetes adapters; local mock-registry resolution, locked-sync, offline list/verify, atomic-activation, and blob-hygiene tests; plus `crates/dekopon-brokerd/tests/server.rs` mapped/unmapped-peer, informational reporting, real HTTP listener, end-to-end invocation, clean-shutdown, and restart-replay tests, and `crates/dekopon-brokerd/tests/examples.rs` pinning `examples/conditional-write/` against the loaded `http-probe` manifest and Cedar grammar |
| Broker operational web UI | `crates/dekopon-webui/src/`; Wasmtime observations in `crates/dekopon-broker-host/src/{metrics,metadata}.rs` | Router/rendering, escaping/security-header, provider-detail, live-counter, artifact/interface, GET-only, and listener-ceiling tests in `crates/dekopon-webui/tests/dashboard.rs`, request-tracing coverage in `crates/dekopon-webui/tests/request_tracing.rs` (both use the exact fetched standalone echo fixture outside the published package); real bind/redirect coverage in `crates/dekopon-brokerd/tests/server.rs` |
| Immediate Wasmtime host | `crates/dekopon-provider-host/src/lib.rs`, `crates/dekopon-provider-host/wit/` | `crates/dekopon-provider-host/tests/host.rs` |
| Sandboxed script language | `crates/dekopon-shell/src/` | Per-module unit tests plus the kept-versus-dropped grammar corpus in `crates/dekopon-shell/src/interp/tests.rs` |
| Shared prompt loop, safe agent-configuration/image-generation meta views, and session capability dispatch | `crates/dekopon-agent/src/` | Inline prompt/meta-tool, one-attempt byte-free image output, bounded redaction-shape, composite-dispatch, and stub-broker-socket leg tests |
| Direct runner, shell subcommand, broker client, local/OTLP tracing and lifecycle logs | `crates/dekopon-run/src/` | `crates/dekopon-run/tests/cli.rs`, including authenticated broker subprocess exchange and shell limit/rejection coverage; `examples/otel-traces/smoke-test.sh` for OpenObserve delivery/redaction |
| Chat gateway configuration, text/image transports, routing, bounded agent sessions, credential-free self-inspection, conversation history, and prompt cache keys | `crates/dekopond/src/` | `crates/dekopond/src/tests.rs` for strict configuration, routing, admission, effective config introspection, conversation replay and eviction, cache-key minting/rotation, generated-image delivery, and loopback Slack/Discord/Telegram transports; `crates/dekopond/tests/gateway.rs` for a real `dekopon-brokerd` end to end; `crates/dekopond/tests/examples.rs` for the checked-in walkthrough configuration |
| Chat gateway configuration, text/image transports, routing, bounded agent sessions, credential-free self-inspection, conversation history, and prompt cache keys | `crates/dekopond/src/` | `crates/dekopond/src/tests.rs` for strict configuration, routing, admission, effective config introspection, conversation replay and eviction, cache-key minting/rotation, generated-image delivery, and loopback Slack/Discord/Telegram/WhatsApp transports; `crates/dekopond/src/transport/whatsapp.rs` for webhook signature, refusal, saturation, listener, and reply-splitting tests; `crates/dekopond/tests/gateway.rs` for a real `dekopon-brokerd` end to end; `crates/dekopond/tests/examples.rs` for the checked-in walkthrough configuration |
| Provider component test harness | `crates/dekopon-provider-sdk-testkit/src/lib.rs` | `crates/dekopon-provider-sdk-testkit/tests/harness.rs`, driving exact fetched `echo`/`memory-chat` releases plus the checked `storage-probe` fixture |
| Rust provider fixtures | `examples/providers/http-probe/`, `memory-reservation-probe/`, `provider-v0-1-compat/`, and `storage-probe/` | Separate-workspace tests, checked-component import inspection, host/runner rejection, loopback mocks, and broker/VFS tests; exact standalone echo/JSONPlaceholder/memory-chat fixtures are fetched by `ci/fetch-external-provider-components.sh` |
| End-to-end deployment example | `examples/conditional-write/` | `crates/dekopon-brokerd/tests/examples.rs`, `crates/dekopon-config/tests/examples.rs`, `crates/dekopond/tests/examples.rs` |
| CI, dependency policy, release | `.github/workflows/`, `deny.toml`, `release.toml` | Required GitHub checks and `cargo package` |
| Container image | `Dockerfile`, `ci/stage-image-context.sh`, `.github/workflows/container-image.yml` | Assembled from a published release into a constructed context, verified against it on pull requests; see [`container-image.md`](container-image.md) |

Tests intentionally live beside the crate that owns the behavior. The top-level `tests/` directory remains a map to package-owned suites; the repository-level observability smoke test lives with its runnable example under `examples/otel-traces/`.

## Change maps

### Catalog resources or validation

Update protocol types first, then config validation, CLI rendering, examples, schemas, and docs as applicable. Authored fields are strict: unknown fields fail rather than being silently ignored. Parse config once; command handlers should consume typed resources, not YAML values.

### CLI behavior

Keep Clap syntax in `cli.rs`, execution separate from rendering, and process exits documented. Add parser tests and black-box tests. Machine-readable JSON/YAML shapes and exit codes need compatibility consideration even when table output can evolve.

`dekopon auth` does not load the catalog. `dekopon-run` consumes model credentials but does not own account-lifecycle commands. Its explicit broker subcommands must remain identity-free clients; do not add principal, actor, policy, constraints, credentials, or authorization arguments.

### Model clients or prompt tools

Generic model types and transports belong in `dekopon-model`; the immediate bounded tool loop belongs in `dekopon-run`. Gateway image generation is also a model client: its fixed public endpoint and credential remain in `dekopon-model`, while the shared prompt loop carries generated bytes through a request-local output slot rather than a model message. Keep credentials and generated bytes inside their typed boundaries and out of providers, broker protocol, history, and traces. Mock network protocols in tests; never read or import another application's credential store.

Provider JSON Schemas are exposed to the model, but there is no general JSON Schema validator in the host. The host requires an object-shaped schema and object invocation input; each provider must still validate its capability-specific fields and constraints.

### Provider contract or host

The SDK and host provider WIT files are mirrored and must remain byte-identical:

- `crates/dekopon-provider-sdk/wit/provider.wit`
- `crates/dekopon-provider-host/wit/provider.wit`

The buffered HTTP WIT package and guest/host copies are also mirrored:

- `wit/http/http.wit`
- `crates/dekopon-provider-http/wit/deps/http.wit`
- `crates/dekopon-broker-host/wit/deps/http.wit`
- `examples/providers/http-probe/wit/deps/http.wit`

The storage package is mirrored byte-for-byte at:

- `wit/storage/storage.wit`
- `crates/dekopon-provider-storage/wit/deps/storage.wit`
- `crates/dekopon-broker-host/wit/deps/storage.wit`
- `examples/providers/storage-probe/wit/deps/storage.wit`

The broker host and imported guests also mirror the provider package:

- `crates/dekopon-broker-host/wit/deps/provider.wit`
- `examples/providers/http-probe/wit/deps/provider.wit`
- `examples/providers/memory-reservation-probe/wit/deps/provider.wit`
- `examples/providers/storage-probe/wit/deps/provider.wit`

Update all copies together and keep their equality checks passing. The SDK copy is the publication source for the `dekopon:provider@0.2.0` WIT package. That package contains the same `provider` world—exactly the `describe` and `invoke` exports and zero imports—plus a `provider-commands` world adding `resolve-command`, and is stored at `ghcr.io/dekopon-agents/dekopon/provider:0.2.0`. The `0.1.0` package remains published and its components remain loadable: `resolve-command` is looked up by name at instantiation rather than required by the bound world. Packaging this existing contract adds distribution, not guest authority: the immediate linker remains empty.

The root [`wkg.toml`](../wkg.toml) and [`wkg.lock`](../wkg.lock) retain the immutable provider package metadata and dependencies. [`../wit/http/wkg.toml`](../wit/http/wkg.toml) plus [`../wit/http/wkg.lock`](../wit/http/wkg.lock), and [`../wit/storage/wkg.toml`](../wit/storage/wkg.toml) plus [`../wit/storage/wkg.lock`](../wit/storage/wkg.lock), independently define the HTTP and storage packages. The shared [`wkg/config.toml`](../wkg/config.toml) maps the namespace to GHCR. The workflow publishes the import-free `dekopon:provider@0.2.0` worlds and the interface-only `dekopon:http@1.0.0` and `dekopon:storage@0.1.0` packages independently. Published package versions are immutable. Change every mirror and increment the affected WIT package version before publishing a changed contract; the publication workflow rebuilds generated components, byte-compares them with the checked artifacts, and rejects different bytes for an existing package version.

Immediate providers must remain read-only and import-free; adding WASI or a host import there is an authority change, not a convenience refactor. The exact fetched echo v0.1.0 component decodes to zero imports even though its standalone source compiles `dekopon-provider-storage` with the empty default feature set, proving that merely depending on the facade grants and imports nothing. `dekopon-broker-host` is the separate privileged adapter: it links only the project-owned HTTP and storage interfaces, consumes `AuthorizedInvocation` plus an exact optional storage grant, and maps WIT values to `dekopon-http-host` or `dekopon-storage-host`. The native engines consume exact grants beneath independent host ceilings. Neither host authenticates callers, evaluates policy, constructs authorization, injects credentials, or writes audit records.

The repository-owned checked components are generated:

| Source | Build script | Artifact |
|---|---|---|
| `examples/providers/http-probe/src/lib.rs` | `examples/providers/http-probe/build.sh` | `examples/providers/http-probe-provider.wasm` |
| `examples/providers/memory-reservation-probe/src/lib.rs` | `examples/providers/memory-reservation-probe/build.sh` | `examples/providers/memory-reservation-probe-provider.wasm` |
| `examples/providers/provider-v0-1-compat/src/lib.rs` | `examples/providers/provider-v0-1-compat/build.sh` | `examples/providers/provider-v0-1-compat-provider.wasm` |
| `examples/providers/storage-probe/src/lib.rs` | `examples/providers/storage-probe/build.sh` | `examples/providers/storage-probe-provider.wasm` |

Never edit `.wasm` files directly. Each in-tree source directory is a separate Cargo workspace with its own lockfile, so root workspace format, lint, and test commands do **not** cover it. Echo, JSONPlaceholder, and memory-chat source and Wasm are not tracked here: `ci/fetch-external-provider-components.sh examples/providers` installs their exact ignored v0.1.0 fixtures after verifying core-pinned release checksums. Publication CI rebuilds every repository-owned checked component with the pinned provider artifact toolchain (`rustc 1.97.0`, `wasm-tools 1.236.1`) and byte-compares it before inspection; it separately fetches and inspects the standalone releases. `http-probe` and fetched JSONPlaceholder each decode to exactly one HTTP import. Fetched memory-chat decodes to JSONL only and three provider exports; `memory-reservation-probe` and the provider-v0.1 compatibility fixture are import-free; `storage-probe` decodes to durable-files only and three provider exports. None may import WASI. Direct-host and `dekopon-run inspect` tests reject every imported component.

### Dependencies, crates, CI, or releases

Declare shared versions and path dependencies in the root `Cargo.toml`; commit `Cargo.lock`. `dekopon-core` and `dekopon-capability` are inherited with `default-features = false`, so their default-on `schemars` feature reaches a build only where a crate asks for it: `dekopon-capability` and `dekopon-protocol` forward it through their own `schemars` features, and the guest SDK does not, which keeps `schemars`, `schemars_derive`, and `syn` out of every `examples/providers/*` wasm build. Changing that closure changes those workspaces' `Cargo.lock` files, which are committed. New publishable crates also require a meaningful tested responsibility, packaging validation, architecture/roadmap updates, and an entry in the dependency-ordered plan in `.github/release-crates.txt`. Pull-request CI and release validation compare that plan with Cargo metadata and reject omissions, private or unknown entries, duplicates, and any normal, build, or dev dependency published after its consumer—`cargo package` resolves all three while verifying an archive.

[`../CHANGELOG.md`](../CHANGELOG.md) is required release metadata. Keep pending work under `[Unreleased]`; an application release must promote completed bullets into a dated `[VERSION]` section, while an independently versioned chart release uses `[dekopon-chart-<VERSION>]`. `.github/scripts/verify_changelog.py` requires exactly one Unreleased heading and a non-placeholder bullet under a Keep a Changelog category. Pull-request CI compares both the workspace and chart versions with those headings, and the corresponding tag workflow repeats the check before publication. Only immutable application tags v0.2.0 through v0.7.0 may omit the file during manual recovery because they predate its introduction.

GitHub Actions are pinned by full commit SHA. Required check names such as `test (Rust 1.89.0)` are branch-protection contexts: renaming a job without coordinating the repository setting leaves a permanently pending required check. Validate workflow and shell-script edits with `actionlint .github/workflows/*.yml` and `shellcheck <SCRIPT>` when those tools are available. Do not change branch protection, publish crates, create a release, or add credentials without explicit maintainer authorization.

Expensive validation runs on pull requests only. The classifier selects Rust, dependency, release-metadata, chart, package-archive, and CLI-install lanes independently; missing classifier output still runs every lane. Stable workspace tests run concurrently under `cargo-nextest` while doctests retain `cargo test`, and the required `quality (stable)` context aggregates that test lane with formatting, linting, documentation, provider-workspace, shell, and privilege-boundary checks. The required `test (Rust 1.89.0)` context compiles and links every binary test target on the MSRV with `--no-run` without executing that suite; its small doctest set still executes because Cargo cannot compile doctests under `--no-run`. Full `cargo package --workspace` verification runs when manifests, build scripts, explicit package inputs, WIT, or publication machinery change, while release metadata validation still runs for ordinary Rust and changelog changes.

Pull-request compiler and Cargo-registry caches are restore-only. `.github/workflows/cache-warm.yml` writes a default-branch registry cache capped at 512 MiB plus granular sccache compiler objects after relevant changes reach `main`; its independent warmer jobs compile lint/test targets but execute no tests and are not a second validation gate. CI job summaries record cache selection, network byte deltas, and target/registry growth so cache usefulness is measured rather than inferred from lookup hits. The tag-triggered release performs only the release-specific tag/version, changelog, and publication-plan checks before building and attesting three platform archives, creating the GitHub release, and publishing every public crate in dependency order. The authorized tag push is the single publication gate: the `crates-io` environment remains part of the short-lived trusted-publisher OIDC identity but has no required-reviewer rule. A manual dispatch against an existing tag is only recovery; it packages and publishes crates while skipping platform builds, the existing GitHub release, and immutable crate versions already present. Every public crate needs a crates.io GitHub trusted-publisher entry for `dekopon-agents/dekopon`, `release.yml`, and that environment; bootstrap a brand-new crate name only under explicit authorization, then register it and revoke the bootstrap credential. Published versions and tags remain immutable. The complete operator checklist lives in the root [`README.md`](../README.md#maintainer-release-process).

Publishing a release additionally runs `.github/workflows/homebrew-tap.yml`, which renders `dekopon-agents/homebrew-tap`'s formula with `.github/scripts/render-homebrew-formula.py`. That script reads the release's asset list and its published `.sha256` sidecars, so the formula's platform blocks follow whatever a release shipped and never a list held in the workflow; a target it cannot map to a Homebrew `on_macos`/`on_linux` block fails the job rather than disappearing from the formula. Its one hand-maintained list is `RETIRED`, naming targets a past release shipped that the tap must stop offering, so an immutable older release cannot reintroduce a platform the project no longer builds. Pushing to another repository needs a credential `GITHUB_TOKEN` cannot provide: the job mints a short-lived installation token from a GitHub App via the `TAP_APP_ID` and `TAP_APP_PRIVATE_KEY` repository secrets, and skips with a warning when either is absent or when the App is not installed on the tap, since both are the same unfinished operator setup. The one-time App setup is in the root [`README.md`](../README.md#homebrew-tap-automation).

Neither that workflow nor `.github/workflows/container-image.yml` triggers on `release: published`. `release.yml` publishes with `GITHUB_TOKEN`, and GitHub does not create workflow runs from events raised by that token, so the event is dispatched to nothing—at v0.4.0 both workflows produced no run at all rather than a failed or skipped one. Both are `workflow_call` reusable workflows that `release.yml` invokes as jobs with `needs: github-release`, which is what guarantees they see a release with its assets attached; both keep a `workflow_dispatch` with a `tag` input as the manual recovery path. A reusable workflow reads `github.event_name` and `github.ref` from its caller, so neither may branch on its own event name; each branches on whether its `tag` input is set.

## Runtime facts that are easy to miss

Immediate host:

- A `ProviderRegistry` retains compiled Wasmtime `Component` values for its lifetime. `HostOptions::compile_cache_dir`, exposed as `dekopon-run --compile-cache`, is the only cross-process cache and is off unless a directory is named.
- Every describe or invoke operation creates a fresh bounded store and component instance.
- One shared runtime mutex serializes immediate component execution; current calls are not parallel.
- The linker is empty: no WASI, filesystem, network, environment, clock, random, or credential imports reach a component.
- The host validates bounds, routing, read-only manifests, object-shaped inputs, and typed wire responses. Capability-specific argument validation remains provider-owned.
- Immediate provider output is raw JSON. It is not broker evidence, an `InvocationResult`, or an authorization receipt.
- Prompt mode offers exactly one model tool, `bash`, whose `script` argument runs on `dekopon-shell`. Model tool selection and arguments remain untrusted, and a call carrying no string `script` ends the session.
- The prompt loop is bounded by `--max-steps`, at most ten tool calls per model turn, and a whole-session `--shell-max-capability-calls` ceiling spent across every script rather than refreshed per script.
- `prompt --broker` adds a second dispatch leg for capabilities direct mode cannot serve. Direct capabilities are always preferred; the broker stays the sole authority, so its denials reach the script as exit code `126`.
- `dekopon-run shell` runs `dekopon-shell` over the same registry. Its bounds are independent of the Wasm ones: Wasm fuel bounds one component call, while the interpreter's step, recursion, output, deadline, and capability-call ceilings bound how many such calls a script can drive. The interpreter never reads the host process environment.
- The one exception to "no clock, no environment" inside a script is the off-by-default `--shell-allow-clock`, which grants the `date` builtin a UTC wall-clock reading and nothing else. Unset, `date` is "command not found"; it never consults an environment variable, so `TZ` stays unobservable either way. This bounds the interpreter only — the Wasm linker's clock import stays absent regardless.
- Each command word a script runs emits one `shell.command` span carrying its kind, argument count, exit code, and outcome—never argument values. Started/completed log mirrors were deliberately removed; logs are reserved for accounting, refusals, errors, and opt-in payloads. The shape is pinned by the in-process Chrome-trace tests in `crates/dekopon-run/tests/cli.rs` and `crates/dekopon-run/tests/prompt_tracing.rs`. `examples/otel-traces/smoke-test.sh` runs `invoke`, not `shell` or `prompt`, so it covers the direct-invocation spans rather than per-command shell spans.

Privileged broker path:

- `BufferedHttpClient` accepts a broker-produced `HttpConstraints` grant but performs no authorization transition itself.
- Grants can narrow but never widen native ceilings for HTTP call count, request bytes, response bytes, and headers.
- Native HTTP disables redirects, ambient proxies, and decompression; DNS results are checked and pinned before connection.
- `BrokerProviderRegistry` retains one async Wasmtime engine and compiled components, then creates a fresh bounded store and component instance for each description or invocation. Its cloneable metrics handle observes compilation/store/instantiation/invocation/fuel, limiter memory/table requests, and sanitized HTTP byte/count totals; Wasmtime exposes no allocator-wide resident-memory or JIT-cache statistic through this embedding API.
- Description uses a disabled HTTP context; any attempted host call rejects loading even if the guest catches the WIT error.
- Public execution consumes `AuthorizedInvocation`; policy rejections remain terminal after guest code returns.
- `dekopon-broker` validates owner-authored constraint sets against loaded routes, host ceilings, and the legacy credential store. A typed DRN proposal additionally passes separate `secret.use` policy and a private binding before one brokerd resolver snapshot is rendered by the native host. It audits only metadata/digests plus policy IDs/digest and the selected symbolic name/DRN.
- A constraint set may name a default credential and per-agent overrides. Validation covers every credential the set can select, not only the default: each must exist in the store and its destinations must cover every `allowedHosts` entry of that set. Selection happens in `Broker::execute` from the trusted `AuthenticatedContext`, so it can never read a request payload.
- `dekopon-policy` is startup-fixed: policies parse once, the schema is generated from declared principals/providers/capabilities and private-map `Secret` entities, and strict validation runs before the first request. Nothing is parsed per decision. Any evaluation error denies, and policy text never reaches a runtime path — not an error, not an audit field, not `Debug`.
- Every capability a policy references needs a constraint set, or the broker refuses to start; a capability with no constraint set is denied `unconstrained-capability` before Cedar is consulted.
- `AuthenticatedContext` construction alone is not authentication. `FileAuditLog` exclusively locks, verifies, and synchronizes bounded owner-only JSONL, exposes exact chain-prefix checks, and restores replay IDs across restart. `dekopon-brokerd` synchronizes a separately locked atomic checkpoint after each append and requires it to match a verified audit prefix at startup.
- `dekopon-broker-protocol` frames strict JSON under a hard byte ceiling and complete-operation deadline; its invocation type cannot carry identity, policy, constraints, credentials, or authorization, its client authenticates the configured server UID, and its normal dependency graph contains no broker host or native HTTP engine.
- `dekopon-brokerd` derives context from connected Unix peer UID and exact owner-controlled mapping, owns secure socket lifecycle, rejects unreachable UID mappings, bounds concurrent connections, verifies/reconciles its durable audit checkpoint, and restores audit/replay state before listening. `--http-bind` separately opens the unauthenticated GET-only `dekopon-webui`; absent means no TCP listener.
- `dekopon-brokerd provider` is a separate operator mode. Exact-reference `sync` and `sync --locked` are the only network-capable lifecycle commands; `list`, `verify`, and daemon startup construct no registry request. A managed lock passes expected component length, SHA-256, and provider ID into the host so its one artifact read is both verified and compiled. The incompatible standard-Wasm-package assumptions in `wasm-pkg-client` are not used; the daemon embeds a narrow strict OCI-reference parser and bounded distribution path over `http-auth` and the existing rustls `reqwest` client.
- The service currently treats one owner UID as a trust domain, has no independently retained, signed, or remote checkpoint anchor, and is not integrated with the operator CLI. Explicit `dekopon-run broker` commands are unprivileged fresh-connection clients; direct runner subcommands remain on the independent empty-linker host. CI rejects `dekopon-broker`, `dekopon-broker-host`, `dekopon-http-host`, or `dekopon-brokerd` in the normal dependency tree of both `dekopon-run` and `dekopond`.
- `dekopond` is the unprivileged agent daemon on the other side of that boundary: strict owner-controlled configuration naming environment variables rather than secrets, chat transports, first-match routing to catalog agents, admission-bounded sessions, optional bounded per-sender conversation history in process memory, and attested on-behalf-of proposals. Its `capabilitiesFor` gate refuses an unauthorized subject before any model call; the broker answers it only when policy permits `agent.prompt` for that principal and agent. It also best-effort reports a content-free normalized agent inventory and provider-reported model usage for the web UI; those values are informational and never feed authorization. See [`dekopond.md`](dekopond.md).

See [`run.md`](run.md) for the user-facing contract, [`observability.md`](observability.md) for OTLP signal and redaction behavior, and [`security-model.md`](security-model.md) for the trust boundary.

## Validation

Use `--locked` for reproducible validation. Start with `git diff --check`. Targeted checks are encouraged during development; run every relevant group before opening a PR.

### Root workspace

```console
ci/fetch-external-provider-components.sh examples/providers
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
cargo deny check
```

For MSRV-sensitive code or dependency changes, compile and link the binary test targets without executing the stable suite twice, then retain compile-fail and ordinary doctest coverage on the minimum toolchain:

```console
cargo +1.89.0 test --workspace --all-features --locked --no-run
cargo +1.89.0 test --workspace --all-features --locked --doc
```

For package metadata, include lists, or dependency-boundary changes, run from a clean tree:

```console
cargo package --workspace --locked
```

The storage host, immediate host, broker host, broker-core, and broker-service packages intentionally exclude repository-only integration fixtures, so Cargo may warn that `tests/storage.rs`, `tests/host.rs`, `tests/broker.rs`, `tests/memory.rs`, `tests/policy_decisions.rs`, or `tests/server.rs` is not included in the published package. Release packaging runs `.github/scripts/prepare-package-cache.sh` before its target-cache save to remove unpacked test-source directories from `target/package`; they are not compiler artifacts, and leaving them there makes `rust-cache` misclassify them as nested target directories and emit false `ENOENT` annotations.

### OpenObserve OTLP end-to-end test

For runner telemetry, OpenObserve example, or observability CI changes, run:

```console
examples/otel-traces/smoke-test.sh
```

The script builds the runner, starts one pinned OpenObserve container with an isolated Docker volume, executes a direct provider invocation, and searches both streams: the trace stream for required spans and absence of a sentinel provider input, and the log stream for a lifecycle record carrying the same `trace_id`, which is what makes a log result pivot to its trace. It removes the container and volume afterward.

Run `shellcheck examples/otel-traces/smoke-test.sh` before submission. Validating the Compose file needs the same credentials the stack does, because `compose.yaml` declares them with `:?` so a missing value fails loudly rather than starting an unauthenticated instance:

```console
OPENOBSERVE_ROOT_EMAIL=dev@example.com OPENOBSERVE_ROOT_PASSWORD=devpassword \
  docker compose -f examples/otel-traces/compose.yaml config
```

### Provider example workspaces

Run these commands for each affected in-tree fixture manifest (`http-probe`, `memory-reservation-probe`, `provider-v0-1-compat`, and `storage-probe`). Standalone provider repositories own their own source gates:

```console
cargo fmt --manifest-path examples/providers/<PROVIDER>/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/<PROVIDER>/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/<PROVIDER>/Cargo.toml
cargo check --locked --manifest-path examples/providers/<PROVIDER>/Cargo.toml --target wasm32-unknown-unknown
```

For `memory-reservation-probe` and `storage-probe`, additionally run their `build.sh`,
`wasm-tools validate`, and `wasm-tools component wit --json`; assert zero imports or durable-files
respectively and no WASI. Fetch standalone memory-chat and assert its exact v0.1.0 component is
JSONL-only with no WASI:

```console
ci/fetch-external-provider-components.sh examples/providers memory-chat
wasm-tools component wit --json examples/providers/memory-chat-provider.wasm
```

If in-tree fixture source, SDK exports, WIT, or tool manifests change, install the pinned component tool, regenerate repository-owned fixtures, fetch standalone fixtures, and exercise each affected artifact:

```console
cargo install wasm-tools --version 1.236.1 --locked
examples/providers/http-probe/build.sh
wasm-tools validate examples/providers/http-probe-provider.wasm
wasm-tools component wit examples/providers/http-probe-provider.wasm
ci/fetch-external-provider-components.sh examples/providers
wasm-tools validate examples/providers/echo-provider.wasm
wasm-tools validate examples/providers/jsonplaceholder-provider.wasm
wasm-tools validate examples/providers/memory-chat-provider.wasm
cargo test -p dekopon-provider-host --test host --locked
cargo test -p dekopon-broker-host --locked
cargo test -p dekopon-broker --locked
cargo test -p dekopon-broker-protocol --locked
cargo test -p dekopon-brokerd --locked
cargo test -p dekopon-run --test cli --locked
```

A deterministic rebuild should leave the artifact unchanged when the source and toolchain are unchanged.

### Published WIT packages

Install the pinned package and component tools, then build and inspect the package from the repository root:

```console
cargo install wkg --version 0.16.0 --locked
cargo install wasm-tools --version 1.236.1 --locked
mkdir -p target/wit-package
wkg build \
  --wit-dir crates/dekopon-provider-sdk/wit \
  --output target/wit-package/dekopon-provider.wasm \
  --config wkg/config.toml
(
  cd wit/http
  wkg build \
    --wit-dir . \
    --output ../../target/wit-package/dekopon-http.wasm \
    --config ../../wkg/config.toml
)
wasm-tools validate target/wit-package/dekopon-provider.wasm
wasm-tools validate target/wit-package/dekopon-http.wasm
(
  cd wit/storage
  wkg build --wit-dir . --output ../../target/wit-package/dekopon-storage.wasm \
    --config ../../wkg/config.toml
)
wasm-tools validate target/wit-package/dekopon-storage.wasm
wasm-tools component wit target/wit-package/dekopon-provider.wasm
wasm-tools component wit target/wit-package/dekopon-http.wasm
wasm-tools component wit target/wit-package/dekopon-storage.wasm
```

The builds must leave all three `wkg.lock` files unchanged. The decoded provider package must identify `dekopon:provider@0.2.0`, a `provider` world with two exports and zero imports, and a `provider-commands` world with three exports and zero imports. The HTTP package must identify `dekopon:http@1.0.0`, one `client` interface with a single buffered `send` function, and no worlds. The storage package must identify `dekopon:storage@0.1.0`, the complete pinned JSONL and durable-files signatures/types, and no worlds. Exercise the configured fetch path with:

```console
wkg get \
  --config wkg/config.toml \
  --output target/wit-package/fetched-provider.wasm \
  dekopon:provider@0.2.0
wkg get \
  --config wkg/config.toml \
  --output target/wit-package/fetched-http.wasm \
  dekopon:http@1.0.0
wkg get \
  --config wkg/config.toml \
  --output target/wit-package/fetched-storage.wasm \
  dekopon:storage@0.1.0
```

`.github/workflows/wit-package.yml` performs local publish/fetch round trips for all three packages on pull requests. When the relevant files reach `main`, it publishes the immutable packages to GHCR and verifies that fetching each package returns identical bytes.

### Secret references and private source adapters

No provider or HTTP WIT file changes for this feature: the DRN is a typed top-level proposal field
and the native HTTP host keeps injection broker-owned. Validate the domain, dual policy, shell,
path/reflection host, broker swap refusal, strict private map, and mock adapters with:

```console
cargo test -p dekopon-core --locked
cargo test -p dekopon-policy --locked
cargo test -p dekopon-shell --locked
cargo test -p dekopon-broker-protocol --locked
cargo test -p dekopon-http-host --locked
cargo test -p dekopon-broker-host --locked
cargo test -p dekopon-broker --locked
cargo test -p dekopon-brokerd --locked
```

No test contacts a public secret manager. Remote adapters use literal-loopback mocks; production
endpoints require HTTPS. Direct runner/gateway dependency-boundary checks must stay green.

### Provider manager

The provider manager is covered by the broker-service package. Its mock registry uses literal
loopback HTTP only through the same explicit opt-in the CLI exposes; no test contacts a public
registry.

```console
cargo test -p dekopon-broker-host --test host --locked
cargo test -p dekopon-brokerd --locked
cargo clippy -p dekopon-broker-host -p dekopon-broker -p dekopon-brokerd \
  --all-targets --all-features --locked -- -D warnings
```

For dependency or MSRV changes, also run the workspace MSRV command and `cargo deny check`. A manual
public-GHCR smoke test is useful but is not a substitute for the loopback tests and must not be made
a CI dependency. The existing container staging path remains unchanged until a published release
contains the manager; do not replace its `gh attestation verify` provenance check with digest-only
OCI fetching.

### Container image

The image is assembled from the executables a release already published, into a context that is
constructed rather than filtered. One script does the whole fetch-verify-stage path, and CI runs
the same one. Contract and deployment details are in [`container-image.md`](container-image.md).

```console
actionlint .github/workflows/container-image.yml
shellcheck ci/stage-image-context.sh
work=$(mktemp -d)
ci/stage-image-context.sh v0.3.0 "$work"
docker buildx build --platform linux/arm64 --load -t dekopon:local "$work/context"
docker buildx build --platform linux/amd64 --load -t dekopon:local-amd64 "$work/context"
docker run --rm dekopon:local dekopon version
docker run --rm dekopon:local dekopon-run invoke \
  --provider /opt/dekopon/providers/echo-provider.wasm echo.echo --input '{}'
docker export "$(docker create dekopon:local unused)" | tar -tvf - opt/dekopon/providers
```

The script prints the sixteen files it staged and the digest of each executable, then the build
context is exactly those files: there is no `.dockerignore` denylist to keep correct as the
repository grows. The repository root cannot be used as a context and fails in about a second if
someone tries.

Neither build needs emulation: every instruction is a `COPY`, so a foreign-architecture image can
be assembled and its filesystem inspected anywhere. Only *running* one needs QEMU, so run the
image that matches the machine.

Do not add a compile stage to the Dockerfile: the point of the image is that its binaries are the
release's binaries, verifiable with `sha256sum` against the published archive. The workflow checks
exactly that for all eight before it pushes anything, and the staging script refuses to stage a
binary that needs a glibc newer than the runtime base provides.

`echo` is the only baked component the direct runner can load — the other default components
import HTTP and optional memory imports JSONL, while the immediate linker is empty — and loading
one matters because components compile lazily through Cranelift, so a clean startup proves nothing.
The `docker export` listing is how ownership and mode are read: the image has no shell. The four
default components and optional memory component must be regular single-link files owned by `65532` under a `65532`-owned directory that is not
group- or world-writable, or `dekopon-brokerd` refuses to start.

## Before opening a pull request

- Rebase or branch from current `main`; do not stack accidentally on an already merged feature branch.
- Keep the diff scoped and preserve generated/source consistency.
- Update current-behavior docs in the same change; do not edit the roadmap as proof of implementation.
- Describe user-visible behavior, security implications, validation run, and known limitations.
- Use a conventional commit subject where practical.
- Push the branch, open the PR, and verify the required checks rather than assuming local success implies remote success.
