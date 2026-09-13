# Development guide

Read [`design.md`](design.md) before this guide. The design defines authority; this document maps common changes to source, tests, generated artifacts, and validation commands.

## Start here

From the repository root (`Cargo.toml`, `AGENTS.md`, and `docs/` should be present):

1. Run `git status --short --branch` and preserve unrelated work.
2. Classify the change as **Current**, **Committed direction**, or **Exploration** ([`design.md`](design.md#constitution)).
3. Read the area document selected by [`../AGENTS.md`](../AGENTS.md).
4. Find the implementation and its nearest tests before editing.
5. Check whether the change crosses the root workspace, a separate provider workspace, a generated artifact, or a mirrored contract.

Prefer targeted tests while iterating, then run the scope-appropriate checks below. Do not claim a command or remote check passed unless it was actually observed.

## Repository map

| Area | Primary implementation | Behavior tests and fixtures |
|---|---|---|
| Domain identifiers and enums | `crates/dekopon-core/src/lib.rs`; the skill-name grammar (`SkillId`) in `crates/dekopon-core/src/skill.rs` | Inline unit and compile-fail tests |
| Proposal/authorization typestate | `crates/dekopon-capability/src/lib.rs` | Inline unit tests |
| Resource wire types | `crates/dekopon-protocol/src/lib.rs` | Inline schema and round-trip tests |
| Config discovery and validation | `crates/dekopon-config/src/`; skill-directory loading (`SKILL.md` front matter, resources, and size/depth/count bounds) in `crates/dekopon-config/src/skill.rs` | `crates/dekopon-config/src/tests.rs`, including catalog-relative skill loading and one-refusal reporting of every unmountable skill; inline front-matter, bound, and symlink-refusal tests in `skill.rs`; `crates/dekopon-config/tests/examples.rs` loads `examples/catalog/dekopon.yaml`, which mounts `examples/catalog/skills/pull-request-review/`, and `examples/conditional-write/dekopon.yaml` |
| OTLP exporter settings and subscriber wiring | `crates/dekopon-telemetry/src/` | Inline endpoint, transport, environment-credential, and OTLP-filter tests |
| Isolated model auth commands | `crates/dekopond/src/{cli,auth,auth_result,auth_render,auth_output}.rs` | `crates/dekopond/tests/auth.rs` and inline parser/renderer/export-guard tests |
| Model clients and ChatGPT auth | `crates/dekopon-model/src/` | Inline mock HTTP/OAuth/SSE/base64/byte-bound tests |
| Provider guest API and adapter | `crates/dekopon-provider-sdk/src/lib.rs`, `crates/dekopon-provider-sdk/wit/` | Inline adapter tests |
| Buffered HTTP WIT and guest facade | `wit/http/`, `crates/dekopon-provider-http/` | Guest validation and mirrored-contract tests plus WIT package workflow |
| Provider storage WIT and guest facade | `wit/storage/`, `crates/dekopon-provider-storage/` | Feature/import inspection, mirror comparisons, package workflow |
| Host wall clock WIT and guest facade | `wit/clock/`, `crates/dekopon-provider-clock/` | Mirror comparison, `clock-probe` import inspection, package workflow |
| Native provider storage | `crates/dekopon-storage-host/src/{config,key,layout,namespace,quota,handle,jsonl,vfs,metrics}.rs` | Path/key/quota/direct-write/startup/continuity tests plus broker-host component integration |
| Bounded native HTTP host | `crates/dekopon-http-host/src/` | Inline destination, method, DNS, header, bound, and loopback mock-server tests |
| Broker async component host | `crates/dekopon-broker-host/src/`, `crates/dekopon-broker-host/wit/` | Inline adapter tests plus `crates/dekopon-broker-host/tests/host.rs` authorization-boundary, Wasmtime, clock-refusal, and loopback tests, and `crates/dekopon-broker-host/tests/spans.rs` for store, instantiation, fuel, and `provider_clock_read` telemetry |
| Cedar policy adapter | `crates/dekopon-policy/src/lib.rs` | `crates/dekopon-policy/src/tests.rs` validation-refusal, deny-by-default, context-matching, explanation, and digest-stability tests |
| Broker authorization, evidence, and audit core | `crates/dekopon-broker/src/lib.rs` | Inline context and bounded in-memory audit tests, `crates/dekopon-broker/tests/broker.rs` constraint-validation and redaction tests, `crates/dekopon-broker/tests/audit_log_events.rs` for the audit log record, and `crates/dekopon-broker/tests/policy_decisions.rs` for the workflow decision table |
| Broker local protocol/client | `crates/dekopon-broker-protocol/src/lib.rs` | Inline strict framing, deadline, authority-omission, socket-metadata, and peer-UID tests |
| Authenticated Unix broker service, private secret sources, and offline provider manager | `crates/dekopon-brokerd/src/`; strict public-DRN/private-map adapters in `secrets.rs`; provider set/lock, bounded OCI transport, content store, and lifecycle commands in `provider_manager.rs` | Inline strict-config/socket/CLI tests; secret-map aggregate validation, strict JSON/YAML projection, secure-file and mock-backed 1Password/Vault/AWS/GCP/Azure/Kubernetes adapters; local mock-registry resolution, locked-sync, offline list/verify, atomic-activation, and blob-hygiene tests; plus `crates/dekopon-brokerd/tests/server.rs` mapped/unmapped-peer, end-to-end invocation, and clean-shutdown tests, `audit_stdout.rs` and `audit_trace.rs` for the audit record on stdout and in the caller's trace, and `crates/dekopon-brokerd/tests/examples.rs` pinning `examples/conditional-write/` against the loaded `http-probe` manifest and Cedar grammar |
| Sandboxed script language | `crates/dekopon-shell/src/` | Per-module unit tests plus the kept-versus-dropped grammar corpus in `crates/dekopon-shell/src/interp/tests.rs` |
| Tokio process lifecycle | `crates/dekopon-process/src/` | Inline typed-result, Tokio task-panic, cooperative-cancellation, and payload-free tracing tests plus a public doctest; consumed by `dekopon-agent`'s cancellable `broker-command` node |
| Shared prompt loop, the safe agent-configuration meta view, session capability dispatch, the two byte-carrying attachment conventions, mounted skills, and opt-in improvement suggestions | `crates/dekopon-agent/src/`; the result-attachment and `chat-asset:` marker conventions in `attachment.rs`, the skills listing and `read_skill` tool in `skills.rs`, the `suggest_improvement` tool and its bounds in `improvement.rs` | Inline prompt/meta-tool, byte-free attachment-slot and marker-expansion, bounded redaction-shape, composite-dispatch, and stub-broker-socket leg tests, plus inline listing/argument and suggestion-validation tests in those two modules |
| Chat gateway configuration, text/attachment transports, routing, bounded agent sessions, credential-free self-inspection, conversation history, and prompt cache keys | `crates/dekopond/src/` | `crates/dekopond/src/tests.rs` for strict configuration, routing, admission, effective config introspection, conversation replay and eviction, cache-key minting/rotation, provider-attachment delivery and chat-asset input expansion, route-mounted skills read on demand, the per-route `improvementSuggestions` opt-in, and loopback Slack/Discord/Telegram/WhatsApp transports; `crates/dekopond/src/transport/whatsapp.rs` for webhook signature, refusal, saturation, listener, and reply-splitting tests; `crates/dekopond/tests/gateway.rs` for a real `dekopon-brokerd` end to end; `crates/dekopond/tests/examples.rs` for the checked-in walkthrough configuration |
| Provider component test harness | `crates/dekopon-provider-sdk-testkit/src/lib.rs` | `crates/dekopon-provider-sdk-testkit/tests/harness.rs`, driving exact fetched `echo`/`memory-chat` releases plus the checked `storage-probe` and `cli-probe` fixtures |
| Rust provider fixtures | `examples/providers/cli-probe/`, `clock-probe/`, `http-probe/`, `memory-reservation-probe/`, `provider-v0-1-compat/`, and `storage-probe/` | Separate-workspace tests, checked-component import inspection, broker-host validation, loopback mocks, and broker/VFS tests; exact standalone echo/JSONPlaceholder/memory-chat fixtures are fetched by `ci/fetch-external-provider-components.sh` |
| End-to-end deployment example | `examples/conditional-write/` | `crates/dekopon-brokerd/tests/examples.rs`, `crates/dekopon-config/tests/examples.rs`, `crates/dekopond/tests/examples.rs` |
| Agent skill example | `examples/catalog/skills/pull-request-review/` (`SKILL.md` plus `references/risk-checklist.md`), mounted by the `reviewer` agent in `examples/catalog/dekopon.yaml` | Loaded with the catalog by `crates/dekopon-config/tests/examples.rs` |
| Shared test scaffolding | `crates/dekopon-test-support/src/` | Not published and never a normal dependency: `provider_fixture`, the `LoopbackServer` builder, `content_length`, one `tracing` `CaptureLayer`, `snapshot_tree`, and `shutdown_on`, reached only as a path `[dev-dependencies]` entry |
| CI, dependency policy, release | `.github/workflows/`, `deny.toml`, `release.toml` | Required GitHub checks and `cargo package` |
| Container image | `Dockerfile`, `ci/stage-image-context.sh`, `.github/workflows/container-image.yml` | Assembled from a published release into a constructed context, verified against it on pull requests; see [`container-image.md`](container-image.md) |

Tests intentionally live beside the crate that owns the behavior. The top-level `tests/` directory remains a map to package-owned suites; the repository-level observability smoke test lives with its runnable example under `examples/otel-traces/`.

Scaffolding that more than one suite needs lives in `dekopon-test-support` instead of being copied: the provider-fixture path, loopback HTTP servers, the `tracing` capture layer, and the directory-tree snapshot. It is `publish = false`, depends on no other crate in this workspace, and is added only under `[dev-dependencies]` as a path entry with no version — Cargo strips that from a published manifest, so it stays out of `.github/release-crates.txt`, out of every `cargo package` archive, and out of the dependency tree CI checks for broker crates inside `dekopond`. Anything that is genuinely one suite's own — a bespoke subscriber keyed by span id, a transport mock driven by a handler closure — stays with that suite.

## Change maps

### Catalog resources or validation

Update protocol types first, then config validation, surviving typed gateway/agent consumers, examples, schemas, and docs as applicable. Authored fields are strict: unknown fields fail rather than being silently ignored. Parse config once; command handlers should consume typed resources, not YAML values.

Skills are catalog resources too. `Agent.spec.skills` (`dekopon-protocol`) names directories, resolved relative to the catalog file unless absolute; `SkillId` in `crates/dekopon-core/src/skill.rs` owns the name grammar; `crates/dekopon-config/src/skill.rs` reads each directory into memory at load time under its size, depth, and count bounds, so no session touches the filesystem; and the catalog loader reports every unmountable or same-named skill in one refusal (`CatalogProblem::Skill`, `CatalogProblem::DuplicateSkill`) and serves the loaded set through `LocalCatalog::agent_skills`.

### CLI behavior

Keep Clap syntax in `cli.rs`, execution separate from rendering, and process exits documented. Add parser tests and black-box tests. Machine-readable JSON/YAML shapes and exit codes need compatibility consideration even when table output can evolve.

`dekopond auth` does not load the catalog. Broker protocol clients remain identity-free proposal clients; do not add principal, actor, policy, constraints, credentials, or authorization arguments.

### Model clients or prompt tools

Generic model types and transports belong in `dekopon-model`; the shared bounded prompt/tool loop belongs in `dekopon-agent` (`prompt.rs`), which `dekopond` and external clients embed. `dekopon-model` holds no image client: producing an image is a provider effect, so the credential lives behind the broker and the gateway only delivers the bytes. Keep provider credentials inside the broker. Attachment bytes travel in provider input/results over the broker protocol, but the gateway's byte-carrying conventions keep them out of the shell, model transcript, and conversation history. Mock network protocols in tests; never read or import another application's credential store.

**Two result/input keys are reserved across every capability**, and `crates/dekopon-agent/src/attachment.rs` owns the gateway half of both. A successful result's top-level `attachments` key is the delivery convention, an array of `{mediaType, base64}` objects: an embedding gateway strips it, validates each entry, and writes back `attached` plus, on a refusal, `attachmentNote`. So `attachments`, `attached`, and `attachmentNote` must not mean anything else in a capability result. The string `chat-asset:<N>`, matched exactly, is the input convention: on the capabilities a route lists, the gateway replaces it with a `data:` URL before proposing. A provider that wants neither simply never writes the key, and a provider that receives an unexpanded marker should reject it as invalid input rather than guess.

Both are **documented JSON shapes, not a shared Rust type**. A provider ships from its own repository against its own pinned `dekopon-provider-sdk` version, so a compile-time type would not be shared in the way that matters, and putting one in the SDK would compile the SDK's source into both this tree's provider fixtures and the gateway's dependency graph — for two string fields. The schema lives in [`crates/dekopon-provider-sdk/README.md`](../crates/dekopon-provider-sdk/README.md#attachments-out-of-band) for the authoring side; the gateway's reader is a private `ResultAttachment` in `attachment.rs` with `deny_unknown_fields`, which is what keeps the two honest about the shape.

Skills and the `suggest_improvement` tool live beside the prompt loop in `dekopon-agent` (`skills.rs`, `improvement.rs`), which is why that crate depends on `dekopon-config` for `Skill`. A skill body reaches the model only through `read_skill`, never the prompt prefix; `suggest_improvement` is offered only where the embedder opted in (`improvementSuggestions`) because its record carries model-authored text whether or not payload telemetry is on. The contract is in [`improvement.md`](improvement.md).

Provider JSON Schemas are exposed to the model, but there is no general JSON Schema validator in the host. The host requires an object-shaped schema and object invocation input; each provider must validate its own capability-specific fields and constraints.

### Provider contract or host

The SDK and host provider WIT files are mirrored and must remain byte-identical:

- `crates/dekopon-provider-sdk/wit/provider.wit`

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

The wall clock package is mirrored byte-for-byte at:

- `wit/clock/clock.wit`
- `crates/dekopon-provider-clock/wit/deps/clock.wit`
- `crates/dekopon-broker-host/wit/deps/clock.wit`
- `examples/providers/clock-probe/wit/deps/clock.wit`

The broker host and imported guests also mirror the provider package:

- `crates/dekopon-broker-host/wit/deps/provider.wit`
- `examples/providers/cli-probe/wit/deps/provider.wit`
- `examples/providers/clock-probe/wit/deps/provider.wit`
- `examples/providers/http-probe/wit/deps/provider.wit`
- `examples/providers/memory-reservation-probe/wit/deps/provider.wit`
- `examples/providers/storage-probe/wit/deps/provider.wit`

Update all copies together and keep their equality checks passing. The SDK copy is the publication source for the `dekopon:provider@0.3.0` WIT package. That package contains the same `provider` world—exactly the `describe` and `invoke` exports and zero imports—plus a `provider-cli` world adding `run-command` and a `provider-commands` world adding `resolve-command`, and is stored at `ghcr.io/dekopon-agents/dekopon/provider:0.3.0`. Published package versions are immutable, so the `provider-commands` world stays in that text even though no host calls `resolve-command` any more and the SDK generates no such export; nothing in this repository builds against it. The `0.1.0` and `0.2.0` packages also remain published, and a component built against either still loads as long as it exports nothing under a command name the host cannot call: the host reads whether a component's type offers `run-command` at load and looks it up by name at instantiation rather than requiring it of the bound world. `provider-v0-1-compat/wit/deps/provider.wit` freezes the `0.1.0` text and is not a mirror; the WIT package workflow fails if a later package version appears under that fixture's `wit/`. Packaging this existing contract adds distribution, not guest authority: the broker authorizes every effect.

WIT package versions and Rust crate versions are independent. Providers depend on
the WIT interface versions they import; a broker host crate may register adapters
for multiple supported WIT versions. Compatible native HTTP-library upgrades do not
require provider rebuilds.

The root [`wkg.toml`](../wkg.toml) and [`wkg.lock`](../wkg.lock) retain the immutable provider package metadata and dependencies. [`../wit/http/wkg.toml`](../wit/http/wkg.toml) plus [`../wit/http/wkg.lock`](../wit/http/wkg.lock), [`../wit/storage/wkg.toml`](../wit/storage/wkg.toml) plus [`../wit/storage/wkg.lock`](../wit/storage/wkg.lock), and [`../wit/clock/wkg.toml`](../wit/clock/wkg.toml) plus [`../wit/clock/wkg.lock`](../wit/clock/wkg.lock), independently define the HTTP, storage, and clock packages. The shared [`wkg/config.toml`](../wkg/config.toml) maps the namespace to GHCR. The workflow publishes the import-free `dekopon:provider@0.3.0` worlds and the interface-only `dekopon:http@1.0.0`, `dekopon:storage@0.1.0`, and `dekopon:clock@1.0.0` packages independently. The broker host's own `dekopon:broker-provider@0.3.0` world is not published, so widening it with an import leaves its version unchanged. Published package versions are immutable. Change every mirror and increment the affected WIT package version before publishing a changed contract; the publication workflow rebuilds generated components, byte-compares them with the checked artifacts, and rejects different bytes for an existing package version.

The exact fetched echo v0.1.0 component decodes to zero imports even though its standalone
source compiles `dekopon-provider-storage` with the empty default feature set: depending on
the facade grants and imports nothing. `dekopon-broker-host` links only project-owned HTTP,
storage, and wall clock interfaces, consumes `AuthorizedInvocation` and an exact optional storage
grant, and maps WIT values to native engines enforcing exact grants beneath independent ceilings.
The clock is readable only in an invocation's store; a read during a description or command run
traps.
The host does not authenticate callers, evaluate policy, or construct authorization.

The SDK's optional `host` feature retains manifest validation, complete conflicting-provider-set
reports, store bounds, engine construction, and the seven shared `DEFAULT_MAX_*` constants, which
live only there. These SDK APIs also serve external embeddings. The feature is off by default and
pulls in Wasmtime, so guest builds must not
enable it. Check wasm32 both with default features and with `--features clap`; the optional
`cli::run_command` adapter is built without `env` or `color`. The broker owns its linker and
yields on fuel so a Tokio deadline can cancel a call.

The repository-owned checked components are generated:

| Source | Build script | Artifact |
|---|---|---|
| `examples/providers/cli-probe/src/lib.rs` | `examples/providers/cli-probe/build.sh` | `examples/providers/cli-probe-provider.wasm` |
| `examples/providers/clock-probe/src/lib.rs` | `examples/providers/clock-probe/build.sh` | `examples/providers/clock-probe-provider.wasm` |
| `examples/providers/http-probe/src/lib.rs` | `examples/providers/http-probe/build.sh` | `examples/providers/http-probe-provider.wasm` |
| `examples/providers/memory-reservation-probe/src/lib.rs` | `examples/providers/memory-reservation-probe/build.sh` | `examples/providers/memory-reservation-probe-provider.wasm` |
| `examples/providers/provider-v0-1-compat/src/lib.rs` | `examples/providers/provider-v0-1-compat/build.sh` | `examples/providers/provider-v0-1-compat-provider.wasm` |
| `examples/providers/storage-probe/src/lib.rs` | `examples/providers/storage-probe/build.sh` | `examples/providers/storage-probe-provider.wasm` |

Never edit `.wasm` files directly. Each in-tree source directory is a separate Cargo workspace with its own lockfile, so root workspace format, lint, and test commands do **not** cover it. Echo, JSONPlaceholder, and memory-chat source and Wasm are not tracked here: `ci/fetch-external-provider-components.sh examples/providers` installs their exact ignored fixtures — the release tag, checksum, and size pinned per provider in that script — after verifying core-pinned release checksums. Publication CI rebuilds every repository-owned checked component with the pinned provider artifact toolchain (the [`rust-toolchain.toml`](../rust-toolchain.toml) compiler and the [`ci/toolchain.env`](../ci/toolchain.env) wasm-tools) and byte-compares it before inspection; it separately fetches and inspects the standalone releases. `http-probe` and fetched JSONPlaceholder each decode to exactly one HTTP import. `clock-probe` (a hand-rolled `run-command` guest) decodes to exactly the wall clock import and three provider exports. Fetched memory-chat decodes to JSONL only and three provider exports; `cli-probe` (the `clap`-layer guest: three provider exports including `run-command`), `memory-reservation-probe` (the hand-rolled `run-command` guest, same three exports), and the provider-v0.1 compatibility fixture are import-free; `storage-probe` decodes to durable-files only and the same three exports. None may import WASI. Broker-host tests enforce the exact supported imports and reject WASI.

### Dependencies, crates, CI, or releases

Declare shared versions and path dependencies in the root `Cargo.toml`; commit `Cargo.lock`. Changing a dependency closure changes those workspaces' `Cargo.lock` files, which are committed. New publishable crates also require a meaningful tested responsibility, packaging validation, architecture/roadmap updates, and an entry in the dependency-ordered plan in `.github/release-crates.txt`. Pull-request CI and release validation compare that plan with Cargo metadata and reject omissions, private or unknown entries, duplicates, and any normal, build, or dev dependency published after its consumer—`cargo package` resolves all three while verifying an archive.

[`../CHANGELOG.md`](../CHANGELOG.md) is required release metadata. Keep pending work under `[Unreleased]`; an application release must promote completed bullets into a dated `[VERSION]` section, while an independently versioned chart release uses `[dekopon-chart-<VERSION>]`. `.github/scripts/verify_changelog.py` requires exactly one Unreleased heading and a non-placeholder bullet under a Keep a Changelog category. Pull-request CI compares both the workspace and chart versions with those headings, and the corresponding tag workflow repeats the check before publication.

GitHub Actions are pinned by full commit SHA. Required check names such as `test (Rust)` are branch-protection contexts: renaming a job without coordinating the repository setting leaves a permanently pending required check. Validate workflow and shell-script edits with `actionlint .github/workflows/*.yml` and `shellcheck <SCRIPT>` when those tools are available. Do not change branch protection, publish crates, create a release, or add credentials without explicit maintainer authorization.

Expensive validation runs on pull requests only. The classifier selects Rust, OTLP smoke-test, documentation, dependency, release-metadata, chart, package-archive, and CLI-install lanes independently; missing classifier output runs every lane. Workspace tests run in their own Cargo lane concurrently with formatting, linting, rustdoc, provider-workspace, shell, release-profile, and privilege-boundary checks, while the toolchain-free documentation lane runs the duplicate-entry and audit-event gates beside them and a native `ubuntu-24.04-arm` lane runs only `cargo test -p dekopon-broker-host --test host`; the required `quality (stable)` context aggregates all four lanes and requires each only under the gate that selected it. Any Markdown change selects the documentation lane, and so does any Rust change, because its audit-event gate reads `crates/**/*.rs`. The workspace-test lane is also the required `test (Rust)` context: `rust-toolchain.toml` pins the MSRV, so no second compiler runs the suite. Full `cargo package --workspace` verification runs when manifests, build scripts, explicit package inputs, WIT, or publication machinery change, while release metadata validation runs for ordinary Rust and changelog changes.

Pull-request compiler and Cargo-registry caches are restore-only. `.github/workflows/cache-warm.yml` writes a default-branch registry cache capped at 512 MiB plus granular sccache compiler objects after relevant changes reach `main`; its independent warmer jobs compile lint/test targets but execute no tests and are not a second validation gate. CI job summaries record cache selection, network byte deltas, and target/registry growth so cache usefulness is measured rather than inferred from lookup hits. The tag-triggered release performs only the release-specific tag/version, changelog, and publication-plan checks before building and attesting three platform archives, creating the GitHub release, and publishing every public crate in dependency order. The authorized tag push is the single publication gate: the `crates-io` environment remains part of the short-lived trusted-publisher OIDC identity but has no required-reviewer rule. A manual dispatch against an existing tag is only recovery; it packages and publishes crates while skipping platform builds, the existing GitHub release, and immutable crate versions already present. Every public crate needs a crates.io GitHub trusted-publisher entry for `dekopon-agents/dekopon`, `release.yml`, and that environment; bootstrap a brand-new crate name only under explicit authorization, then register it and revoke the bootstrap credential. Published versions and tags remain immutable. The complete operator checklist lives in the root [`README.md`](../README.md#maintainer-release-process).

Publishing a release additionally runs `.github/workflows/homebrew-tap.yml`, which renders `dekopon-agents/homebrew-tap`'s formula with `.github/scripts/render-homebrew-formula.py`. That script reads the release's asset list and its published `.sha256` sidecars, so the formula's platform blocks follow whatever a release shipped and never a list held in the workflow; a target it cannot map to a Homebrew `on_macos`/`on_linux` block fails the job rather than disappearing from the formula. Its hand-maintained lists are `EXECUTABLES`, the binaries every archive carries, and `RETIRED`, naming targets a past release shipped that the tap must stop offering, so an immutable older release cannot reintroduce a platform the project has dropped. Pushing to another repository needs a credential `GITHUB_TOKEN` cannot provide: the job mints a short-lived installation token from a GitHub App via the `TAP_APP_ID` and `TAP_APP_PRIVATE_KEY` repository secrets, and skips with a warning when either is absent or when the App is not installed on the tap, since both are the same unfinished operator setup. The one-time App setup is in the root [`README.md`](../README.md#homebrew-tap-automation).

Neither that workflow nor `.github/workflows/container-image.yml` triggers on `release: published`. `release.yml` publishes with `GITHUB_TOKEN`, and GitHub does not create workflow runs from events raised by that token, so the event is dispatched to nothing: neither workflow produces a run at all, rather than a failed or skipped one. Both are `workflow_call` reusable workflows that `release.yml` invokes as jobs with `needs: github-release`, which is what guarantees they see a release with its assets attached; both keep a `workflow_dispatch` with a `tag` input as the manual recovery path. A reusable workflow reads `github.event_name` and `github.ref` from its caller, so neither may branch on its own event name; each branches on whether its `tag` input is set.

## Runtime facts that are easy to miss

Shared orchestration and shell behavior is documented in
[`dekopon-agent`](../crates/dekopon-agent/README.md) and
[`dekopon-shell`](../crates/dekopon-shell/README.md): session-wide capability budgets,
per-script limits, no environment access, bounded text builtins, and payload-free command spans.
The production OTLP smoke drives both daemons and five daemon/provider span families.

Privileged broker path:

- `BufferedHttpClient` accepts a broker-produced `HttpConstraints` grant but performs no authorization transition itself.
- Grants can narrow but never widen native ceilings for HTTP call count, request bytes, response bytes, and headers.
- Native HTTP disables redirects, ambient proxies, and decompression; DNS results are checked and pinned before connection.
- `BrokerProviderRegistry` retains one async Wasmtime engine and compiled components, then creates a fresh bounded store and component instance for each description or invocation. The `provider.describe`, `provider.run_command`, and `provider.invoke` spans record `stores` and `instantiations`, which is where the one-store-one-instance shape of an operation is read.
- Description uses a disabled HTTP context; any attempted host call rejects loading even if the guest catches the WIT error.
- Public execution consumes `AuthorizedInvocation`; policy rejections remain terminal after guest code returns.
- `dekopon-broker` validates owner-authored constraint sets against loaded routes, host ceilings, and the credential store. A typed DRN proposal additionally passes separate `secret.use` policy and a private binding before one brokerd resolver snapshot is rendered by the native host. It audits only metadata/digests plus policy IDs/digest and the selected symbolic name/DRN.
- A constraint set may name a default `credential` and `credentialByAgent` overrides. Validation covers every credential the set can select, not only the default: each must exist in the store and its destinations must cover every `allowedHosts` entry of that set. Selection happens in `Broker::execute` from the trusted `AuthenticatedContext`, so it can never read a request payload. *Committed direction:* these bindings will be replaced by public DRNs ([migration requirements](design.md#legacy-credential-bindings)).
- `dekopon-policy` is startup-fixed: policies parse once, the schema is generated from declared principals/providers/capabilities and private-map `Secret` entities, and strict validation runs before the first request. Nothing is parsed per decision. Any evaluation error denies, and policy text never reaches a runtime path — not an error, not an audit field, not `Debug`.
- Every capability a policy references needs a constraint set, or the broker refuses to start; a capability with no constraint set is denied `unconstrained-capability` before Cedar is consulted.
- `AuthenticatedContext` construction alone is not authentication. Every audit record is a `tracing` event on target `dekopon_broker::audit`, emitted before the `AuditLog` sink is offered it; `TraceOnlyAuditLog` keeps nothing, and a full `InMemoryAuditLog` is the only sink that refuses.
- `dekopon-broker-protocol` frames strict JSON under a hard byte ceiling and complete-operation deadline; its invocation type cannot carry identity, policy, constraints, credentials, or authorization, its client authenticates the configured server UID, and its normal dependency graph contains no broker host or native HTTP engine.
- `dekopon-brokerd` derives context from connected Unix peer UID and exact owner-controlled mapping, owns secure socket lifecycle, maps distinct configured peer UIDs, bounds concurrent connections, and serves with `TraceOnlyAuditLog`, so its audit is the log record alone.
- `dekopon-brokerd provider` is a separate operator mode. Exact-reference `sync` and `sync --locked` are the only network-capable lifecycle commands; `list`, `verify`, and daemon startup construct no registry request. A managed lock passes expected component length, SHA-256, and provider ID into the host so its one artifact read is both verified and compiled. The daemon embeds a narrow strict OCI-reference parser and bounded distribution path over `http-auth` and the rustls `reqwest` client.
- The service enforces the [current local process boundary](security-model.md#current-local-process-boundary). Auth does not invoke it. CI rejects `dekopon-broker`, `dekopon-broker-host`, `dekopon-brokerd`, `dekopon-http-host`, `dekopon-storage-host`, or `dekopon-policy` in the normal dependency tree of `dekopond`. In the reverse direction, CI rejects `dekopon-agent`, `dekopon-shell`, `dekopon-process`, or `dekopon-config` in the normal dependency tree of `dekopon-brokerd`; `dekopon-model` is allowed for the shared `chatgpt::CredentialFile` refresh implementation. Both gates allow the shared `dekopon-broker-protocol` client.
- `dekopond` is the unprivileged agent daemon on the other side of that boundary: strict owner-controlled configuration naming environment variables rather than secrets, chat transports, first-match routing to catalog agents, admission-bounded sessions, optional bounded conversation history in process memory (private per authenticated subject by default, explicitly shareable only within one agent/transport/conversation), and attested on-behalf-of proposals. Its attested `capabilities` gate refuses an unauthorized subject before any model call; the broker answers it only when policy permits `agent.prompt` for that principal and agent. See [`dekopond.md`](dekopond.md).

See [`dekopond.md`](dekopond.md) for the user-facing contract, [`observability.md`](observability.md) for OTLP signal and redaction behavior, and [`security-model.md`](security-model.md) for the trust boundary.

## Validation

Use `--locked` for reproducible validation. Start with `git diff --check`. Targeted checks are encouraged during development; run every relevant group before opening a PR.

### Root workspace

This is the complete list of root-workspace commands behind the required `quality (stable)` context and the `dependency policy` job, in each lane's order in [`../.github/workflows/ci.yml`](../.github/workflows/ci.yml); the comment above each command is what it gates.

```console
# quality checks (stable): formatting.
cargo fmt --all --check
# Lint every target with warnings denied.
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
# Release-profile compile of the two daemons; tag workflows perform the final linked builds.
cargo check --release --locked -p dekopon-brokerd -p dekopond
# Unused dependencies; CI pins cargo-machete 0.9.2.
cargo machete
# Opposite-direction normal-dependency gates; broker-protocol is allowed in both.
# Run in Bash; fail on Cargo errors as well as forbidden exact package names.
(
  set -euo pipefail
  tree=$(cargo tree --locked -p dekopond --edges normal --prefix none)
  if grep -Eq '^dekopon-(broker|broker-host|brokerd|http-host|storage-host|policy) v' <<<"$tree"; then
    printf '%s\n' "$tree" >&2
    exit 1
  fi
  # dekopon-model is absent: the broker's `kind: chatgptSubscription` credential
  # resolves through `dekopon_model::chatgpt::CredentialFile`, the one implementation of it.
  tree=$(cargo tree --locked -p dekopon-brokerd --edges normal --prefix none)
  if grep -Eq '^dekopon-(agent|shell|process|config) v' <<<"$tree"; then
    printf '%s\n' "$tree" >&2
    exit 1
  fi
)
python3 .github/scripts/test_daemon_dependency_gates.py
# The guest host-interface bindings must compile for Wasm, each storage feature on its own.
rustup target add wasm32-unknown-unknown
cargo check --locked -p dekopon-provider-sdk --target wasm32-unknown-unknown
cargo check --locked -p dekopon-provider-sdk --features clap --target wasm32-unknown-unknown
cargo check --locked -p dekopon-provider-clock --target wasm32-unknown-unknown
cargo check --locked -p dekopon-provider-http --target wasm32-unknown-unknown
cargo check --locked -p dekopon-provider-storage --no-default-features --target wasm32-unknown-unknown
cargo check --locked -p dekopon-provider-storage --no-default-features --features jsonl --target wasm32-unknown-unknown
cargo check --locked -p dekopon-provider-storage --no-default-features --features durable-files --target wasm32-unknown-unknown
# The repository shell scripts.
shellcheck ci/fetch-external-provider-components.sh \
  examples/otel-traces/smoke-test.sh examples/providers/build-component.sh examples/providers/*/build.sh
# Rustdoc with warnings denied.
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
# test (Rust): the exact standalone fixtures the tests read, then the suite, then its doctests.
ci/fetch-external-provider-components.sh examples/providers
cargo test --workspace --all-features --locked
cargo test --workspace --all-features --locked --doc
# broker-host components (Linux ARM64): on an aarch64 Linux host only, the component host's own tests.
cargo test -p dekopon-broker-host --test host --all-features --locked
# dependency policy: advisories, licenses, bans, and sources over the full feature graph.
cargo deny --all-features check
```

The same quality lane also runs the [Provider example workspaces](#provider-example-workspaces) commands for every `examples/providers/*/Cargo.toml`, and the context also requires the toolchain-free [Documentation gates](#documentation-gates).

`rust-toolchain.toml` is also the MSRV, so the test commands above are the MSRV check. Moving it, or anything else in the component toolchain, is the lockstep its leading comment lists.

For package metadata, include lists, or dependency-boundary changes, run from a clean tree:

```console
cargo package --workspace --locked
```

Only `dekopond` packages `tests/**`; every other published crate's `include` list omits it, so Cargo may warn that an integration file such as `tests/storage.rs`, `tests/host.rs`, `tests/broker.rs`, `tests/memory.rs`, `tests/policy_decisions.rs`, `tests/refusal_logging.rs`, `tests/span_parenting.rs`, `tests/server.rs`, `tests/examples.rs`, `tests/failure_logging.rs`, `tests/span_redaction.rs`, `tests/wit_mirror.rs`, or `tests/harness.rs` is not included in the published package. Release packaging runs `.github/scripts/prepare-package-cache.sh` before its target-cache save to remove unpacked test-source directories from `target/package`; they are not compiler artifacts, and leaving them there makes `rust-cache` misclassify them as nested target directories and emit false `ENOENT` annotations.

### Documentation gates

Both gates run without a Rust toolchain in the `documentation checks` job that the required
`quality (stable)` context aggregates, so a Markdown-only pull request is gated even though it
selects no Rust lane. The duplicate-entry check covers `docs/`, the root `README.md`, `AGENTS.md`,
and every `crates/*/README.md`:

```console
python3 .github/scripts/check_docs_duplicates.py docs README.md AGENTS.md crates/*/README.md
```

The other gate is inline in [`../.github/workflows/ci.yml`](../.github/workflows/ci.yml): every
`audit.event` name emitted under `crates/` must appear in [`observability.md`](observability.md),
and an extraction that reads nothing fails rather than passing silently. Emitting a new name
therefore means adding it, backticked, to that document in the same change, which is why
`agent.skill.read`, `agent.skill.refused`, `agent.improvement.suggested`, and
`agent.improvement.refused` appear there.

### OpenObserve OTLP end-to-end test

For daemon telemetry, OpenObserve example, or observability CI changes, run:

```console
examples/otel-traces/smoke-test.sh
```

The script builds `dekopon-brokerd` and `dekopond`, starts one pinned OpenObserve container with an isolated Docker volume, and drives a private local-transport turn using a Python standard-library model stub and a real authorized echo provider. It requires `transport.receive`, `gateway.message`, `gateway.session`, `broker.invocation`, `provider.compile`, and `provider.invoke`, with invocation trace continuity from the transport's receipt onward (startup compilation may use a separate trace). A smoke-only shipper ingests actual daemon JSON stdout; complete bounded queries and ingestion counts independently prove native trace/span correlation for both daemons. Local, shipped, and remote records must exclude payload and fake credential sentinels. Production logs remain stdout-only. The script unconditionally cleans up owned processes, container, volume, and temporary configuration. Run `python3 examples/otel-traces/test-smoke.py` for the correlation, retrieval, ingestion, and redaction failure controls.

Run `shellcheck examples/otel-traces/smoke-test.sh` before submission. Validating the Compose file needs the same credentials the stack does, because `compose.yaml` declares them with `:?` so a missing value fails loudly rather than starting an unauthenticated instance:

```console
OPENOBSERVE_ROOT_EMAIL=dev@example.com OPENOBSERVE_ROOT_PASSWORD=devpassword \
  docker compose -f examples/otel-traces/compose.yaml config
```

### Provider example workspaces

Run these commands for each affected in-tree fixture manifest (`cli-probe`, `clock-probe`, `http-probe`, `memory-reservation-probe`, `provider-v0-1-compat`, and `storage-probe`). Standalone provider repositories own their own source gates:

```console
cargo fmt --manifest-path examples/providers/<PROVIDER>/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/<PROVIDER>/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/<PROVIDER>/Cargo.toml
cargo check --locked --manifest-path examples/providers/<PROVIDER>/Cargo.toml --target wasm32-unknown-unknown
```

For `cli-probe`, `memory-reservation-probe`, and `storage-probe`, additionally run their
`build.sh`, `wasm-tools validate`, and `wasm-tools component wit --json`; assert zero imports
(`cli-probe` and `memory-reservation-probe`) or durable-files (`storage-probe`), `run-command` as
the third export of each, and no WASI. `clock-probe` gets the same three steps and must decode to
exactly `dekopon:clock/wall@1.0.0` with `run-command` as its third export. Fetch standalone
memory-chat and assert its exact pinned component is JSONL-only with no WASI:

```console
ci/fetch-external-provider-components.sh examples/providers memory-chat
wasm-tools component wit --json examples/providers/memory-chat-provider.wasm
```

If in-tree fixture source, SDK exports, WIT, or tool manifests change, install the pinned component tool, regenerate repository-owned fixtures, fetch standalone fixtures, and exercise each affected artifact:

```console
. ci/toolchain.env
cargo install wasm-tools --version "$WASM_TOOLS_VERSION" --locked
examples/providers/http-probe/build.sh
wasm-tools validate examples/providers/http-probe-provider.wasm
wasm-tools component wit examples/providers/http-probe-provider.wasm
ci/fetch-external-provider-components.sh examples/providers
wasm-tools validate examples/providers/echo-provider.wasm
wasm-tools validate examples/providers/jsonplaceholder-provider.wasm
wasm-tools validate examples/providers/memory-chat-provider.wasm
cargo test -p dekopon-broker-host --locked
cargo test -p dekopon-broker --locked
cargo test -p dekopon-broker-protocol --locked
cargo test -p dekopon-brokerd --locked
```

A deterministic rebuild should leave the artifact unchanged when the source and toolchain are unchanged.

### Published WIT packages

Install the pinned package and component tools, then build and inspect the package from the repository root:

```console
. ci/toolchain.env
cargo install wkg --version "$WKG_VERSION" --locked
cargo install wasm-tools --version "$WASM_TOOLS_VERSION" --locked
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
(
  cd wit/clock
  wkg build --wit-dir . --output ../../target/wit-package/dekopon-clock.wasm \
    --config ../../wkg/config.toml
)
wasm-tools validate target/wit-package/dekopon-clock.wasm
wasm-tools component wit target/wit-package/dekopon-provider.wasm
wasm-tools component wit target/wit-package/dekopon-http.wasm
wasm-tools component wit target/wit-package/dekopon-storage.wasm
wasm-tools component wit target/wit-package/dekopon-clock.wasm
```

The builds must leave all four `wkg.lock` files unchanged. The decoded provider package must identify `dekopon:provider@0.3.0` with three import-free worlds: `provider` with two exports, the retired `provider-commands` adding `resolve-command` (`argv: list<string>`), and `provider-cli` adding `run-command` (`argv: list<string>`, `stdin: option<string>`), every function returning `string`. That package version is published and immutable, so its text does not change when a world stops being used. The HTTP package must identify `dekopon:http@1.0.0`, one `client` interface with a single buffered `send` function, and no worlds. The storage package must identify `dekopon:storage@0.1.0`, the complete pinned JSONL and durable-files signatures/types, and no worlds. The clock package must identify `dekopon:clock@1.0.0`, one `wall` interface with a single `now-unix-millis` function returning `u64`, and no worlds. Exercise the configured fetch path with:

```console
wkg get \
  --config wkg/config.toml \
  --output target/wit-package/fetched-provider.wasm \
  dekopon:provider@0.3.0
wkg get \
  --config wkg/config.toml \
  --output target/wit-package/fetched-http.wasm \
  dekopon:http@1.0.0
wkg get \
  --config wkg/config.toml \
  --output target/wit-package/fetched-storage.wasm \
  dekopon:storage@0.1.0
wkg get \
  --config wkg/config.toml \
  --output target/wit-package/fetched-clock.wasm \
  dekopon:clock@1.0.0
```

`.github/workflows/wit-package.yml` performs local publish/fetch round trips for all three packages on pull requests. When the relevant files reach `main`, it publishes the immutable packages to GHCR and verifies that fetching each package returns identical bytes.

### Secret references and private source adapters

No provider or HTTP WIT file changes for this feature: the DRN is a typed top-level proposal field
and the native HTTP host keeps injection broker-owned. Validate the domain, dual policy, shell,
path/credential-echo host, broker swap refusal, strict private map, and mock adapters with:

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
endpoints require HTTPS. Gateway dependency-boundary checks must stay green.

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

For dependency changes, also run `cargo deny --all-features check`. A manual
public-GHCR smoke test is useful but is not a substitute for the loopback tests and must not be made
a CI dependency. The container staging path is independent of the provider manager:
`ci/stage-image-context.sh` fetches release archives and runs its `gh attestation verify`
provenance check, and must not be replaced with digest-only OCI fetching; see
[`container-image.md`](container-image.md).

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
docker run --rm dekopon:local dekopond --help
ci/verify-image-broker.sh dekopon:local
docker export "$(docker create dekopon:local unused)" | tar -tvf - opt/dekopon/providers
```

The script prints the twelve files it staged and the digest of each executable, then the build
context is exactly those files: there is no `.dockerignore` denylist to keep correct as the
repository grows. The repository root cannot be used as a context and fails in about a second if
someone tries.

Neither build needs emulation: every instruction is a `COPY`, so a foreign-architecture image can
be assembled and its filesystem inspected anywhere. Only *running* one needs QEMU, so run the
image that matches the machine.

Do not add a compile stage to the Dockerfile: the point of the image is that its binaries are the
release's binaries, verifiable with `sha256sum` against the published archive. The workflow checks
exactly that for all four before it pushes anything, and the staging script refuses to stage a
binary that needs a glibc newer than the runtime base provides.

`ci/verify-image-broker.sh` starts the real broker with the baked echo component and waits
under a deadline for its post-load socket. Releases exposing `probe` exercise that command;
older ones use version-appropriate configuration and prove component-load/startup only. This validates released image bytes, not a native build of source
HEAD. The `docker export` listing is how ownership and mode are read: the image has no shell. The four
default components and optional memory component must be regular single-link files owned by `65532` under a `65532`-owned directory that is not
group- or world-writable, or `dekopon-brokerd` refuses to start.

## Before opening a pull request

- Rebase or branch from current `main`; do not stack accidentally on an already merged feature branch.
- Keep the diff scoped and preserve generated/source consistency.
- Update current-behavior docs in the same change; do not edit the roadmap as proof of implementation.
- Describe user-visible behavior, security implications, validation run, and known limitations.
- Use a conventional commit subject where practical.
- Push the branch, open the PR, and verify the required checks rather than assuming local success implies remote success.
