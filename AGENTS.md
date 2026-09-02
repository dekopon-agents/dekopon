# Guidance for coding agents

## What this repository is

Dekopon is early-stage security infrastructure for AI agents: a model may *propose* an invocation, only the separate privileged broker may *authorize* and execute it, and providers are import-free WebAssembly components. It is one Rust workspace (`Cargo.toml`, edition 2024, shared version `0.12.0`) of 26 crates under `crates/`, 25 of them published, with four binaries: `dekopon` (operator CLI), `dekopon-run` (direct read-only runner), `dekopon-brokerd` (the broker), and `dekopond` (the unprivileged chat gateway). Skills, `read_skill`, `suggest_improvement`, and `dekopon-run session list|show|replay` are post-0.12.0 work recorded under `[Unreleased]` in `CHANGELOG.md`; do not describe them as released.

## Required reading

**Always read [`docs/design.md`](docs/design.md) first.** It is the canonical overview of Dekopon's product model, vocabulary, authority transition, component ownership, and the distinction between current behavior and committed direction. Then read [`docs/development.md`](docs/development.md): it maps source and tests, records generated and mirrored files, explains the separate provider workspaces under `examples/providers/`, and gives scope-specific validation commands. [`docs/README.md`](docs/README.md) is the complete documentation map; [`CONTRIBUTING.md`](CONTRIBUTING.md) states validation and pull-request expectations.

Where things live:

- **Area, source, and tests:** the [Repository map](docs/development.md#repository-map) is the authoritative table; it is not duplicated here.
- **Per-crate operational and API contracts:** `crates/*/README.md` (every published crate has one), indexed by operator question in [`docs/operations.md`](docs/operations.md).
- **Tests:** integration tests live in `crates/*/tests`; shared scaffolding is `crates/dekopon-test-support` (`publish = false`, reached only as a path dev-dependency, never a normal dependency). `tests/README.md` is only a pointer. The observability smoke test is `examples/otel-traces/smoke-test.sh`.
- **Provider fixtures:** `examples/providers/{cli-probe,http-probe,memory-reservation-probe,provider-v0-1-compat,provider-v0-2-compat,storage-probe}` are separate Cargo workspaces with their own `Cargo.lock`; root workspace commands do not cover them. Their checked `*-provider.wasm` files are generated; echo, JSONPlaceholder, and memory-chat are gitignored fixtures fetched by script.
- **WIT sources and mirrors:** listed under [Provider contract or host](docs/development.md#provider-contract-or-host); `wkg.toml` and `wkg.lock` hold the package metadata.
- **CI and release machinery:** `.github/workflows/`, `.github/scripts/`, `.github/release-crates.txt`, `deny.toml`, `release.toml`, `ci/`.
- **Experiments:** `docs/experiments/` records experiments (currently Turso on wasm32) outside the documentation map.

Then read the documents selected by the work:

| If the change touches… | Read… |
|---|---|
| Any product behavior or architecture | [`docs/design.md`](docs/design.md): the non-negotiable invariants and accepted design decisions |
| Capabilities, actors, identity, policy, credentials, providers, evidence, audit, or external effects | [`docs/security-model.md`](docs/security-model.md): trusted inputs, untrusted content, threats, and current limitations |
| Crate boundaries, dependencies, protocols, daemon/broker separation, async, Wasmtime, or Cedar | [`docs/architecture.md`](docs/architecture.md): design responsibilities mapped to implementation boundaries |
| Operator auth, catalog CLI parsing, config discovery, resource reads, output, or exit codes | [`docs/cli.md`](docs/cli.md): the current operator contract |
| `AgentSpec`, `CapabilitySpec`, or `ProviderSpec` fields, skills directories, or what a catalog value is consumed by | [`docs/catalog.md`](docs/catalog.md): every `v1alpha1` field, its actual consumer, and the reserved fields read by nothing |
| Operating a running deployment: startup refusals, audit checkpoint recovery, draining, or socket and directory hygiene | [`docs/operations.md`](docs/operations.md): the index into `crates/*/README.md`, chiefly [`crates/dekopon-brokerd/README.md`](crates/dekopon-brokerd/README.md) |
| A breaking configuration change, a protocol change, or anything an operator must do between releases | [`docs/upgrading.md`](docs/upgrading.md): the migrations `CHANGELOG.md` only names, the lockstep rule, and the restart order |
| Exporting, storing, or deploying a ChatGPT subscription credential | [`docs/chatgpt-credential.md`](docs/chatgpt-credential.md): the rotating-refresh-token constraints |
| Model requests, prompt caching, cache retention, conversation memory, or long-lived agent memory | [`docs/inference.md`](docs/inference.md): current wire behavior versus provider guarantees and future memory design |
| Immediate providers, Wasm components, prompt tools, model endpoints, or limits | [`docs/run.md`](docs/run.md): the experimental runner and the privileges it must not gain |
| Runner traces, OTLP logs, telemetry redaction, audit event names, or OpenObserve | [`docs/observability.md`](docs/observability.md): signal contents, configuration, and the CI-gated audit-event list |
| Skills, `read_skill`, `suggest_improvement`, session replay, or evaluating a changed instruction before it ships | [`docs/improvement.md`](docs/improvement.md): the operator-driven improvement loop and what is deliberately absent |
| Provider source, WIT, generated Wasm, tests, CI, dependencies, packaging, or releases | [`docs/development.md`](docs/development.md): repository mechanics and validation traps root commands do not cover |
| The `Dockerfile`, the container image workflow, or a container deployment | [`docs/container-image.md`](docs/container-image.md): release-archive reuse, the numeric runtime UID, baked provider paths, and the ownership the broker refuses to start without |
| Broker-mediated provider HTTP, host imports, or broker client mode | [`docs/broker-http.md`](docs/broker-http.md): the process boundary, buffered HTTP contract, authorization inputs, and staged delivery |
| Chat transports, gateway configuration, routing, agent sessions, or attested proposals | [`docs/dekopond.md`](docs/dekopond.md): the unprivileged daemon's contract and the authority it does not hold |
| Public DRNs, private secret maps, secret-use bindings, source adapters, or native Basic/Bearer sinks | [`docs/secrets.md`](docs/secrets.md): the typed proposal, dual authorization, projection, rotation, and non-disclosure contract |
| Deployment secrets, 1Password, External Secrets, or delivering a credential file into a cluster | [`docs/1password-eso.md`](docs/1password-eso.md): the deployed secret-store configuration and the bootstrap a human owns |
| Scope, priority, package names, or a proposed new crate | [`docs/roadmap.md`](docs/roadmap.md): sequencing and non-goals, never evidence that something exists |

## Rules that must survive every change

- A model may propose an invocation, but only the broker may authorize it. Capability declarations permit proposals; they do not grant ambient process authority.
- Read authority never implies write authority. External writes require explicit narrow capabilities.
- Trusted actor identity comes from an authenticated envelope, never from model or repository content. A skill body is untrusted model text exactly as `instructions` is.
- Provider credentials remain inside the broker boundary and out of prompts, config, evidence, and logs; model credentials stay inside the selected model client and never enter provider components. Configuration names credentials by environment variable, never by value; secret values travel as `dekopon_core::Redacted`, whose `Debug`, `Display`, and `Serialize` print a marker, and leave it only through `expose` or `into_inner`, so keep those call sites few.
- `dekopond` and `dekopon-brokerd` remain separate processes. External-write authority exists now, so this is a live invariant: the gateway must never gain policy, provider credentials, or an authorization path of its own. CI rejects `dekopon-broker`, `dekopon-broker-host`, `dekopon-brokerd`, `dekopon-http-host`, `dekopon-storage-host`, or `dekopon-policy` in the normal dependency tree of `dekopond`, `dekopon`, and `dekopon-run`; the unprivileged `dekopon-broker-protocol` client is the one broker-named crate they may carry.
- The direct `dekopon-run` provider path remains read-only and import-free; do not add provider credentials, WASI, host I/O, local writes, external writes, or authorization claims to immediate mode, and keep its `broker` subcommands identity-free clients. A broker-backed mode may submit proposals, but only the separate broker may resolve privileged imports or execute effects.
- Do not describe unimplemented daemons, policy, privileged provider interfaces, or external effects as available.
- Parse config once into typed resources; unknown authored fields fail. Do not spread YAML handling through command execution.
- Provider schemas are model-facing metadata, not complete host validation; providers must validate capability-specific input.
- The SDK and host `provider.wit` files, the canonical `dekopon:http` and `dekopon:storage` WIT files, and every checked-in guest, example-provider, or broker-host mirror remain byte-identical; the full list is under [Provider contract or host](docs/development.md#provider-contract-or-host), and equality tests in `dekopon-broker-host`, `dekopon-provider-http`, and `dekopon-provider-storage` fail when any copy drifts. The SDK copy is the publication source for `dekopon:provider`; published package versions are immutable, so change every mirror and bump the WIT version before changing a published contract. Never hand-edit a generated `.wasm`; rebuild it from its Rust source with its `build.sh`.
- Root workspace commands do not cover the separate workspaces under `examples/providers/`; validate each affected provider workspace explicitly.
- Do not publish crates, create releases, move tags, weaken branch protection, or add credentials without explicit human authorization.

## Known failure patterns to check your diff against

These are the classes a deep review actually found, repeatedly. Each is checkable.

- Never discard an error's cause. `map_err(|_| …)`, `let _ = fallible()`, and bool/Option returns from multi-cause checks are bugs: emit a tracing event carrying the reason (the `cause_type` kind, an errno) at the discard site, or return an error naming which check failed. The first two are deny-level workspace clippy lints (`map_err_ignore`, `let_underscore_must_use`), so they fail CI.
- Classify errors on the axis the caller acts on: retryable vs permanent, executed vs not-executed. Never report a permanent exhaustion as transient, a completed result as timed out, or exit 0 with the daemon's work dead.
- Validation reports every conflict, then fails. Never stop at the first error; never last-wins on duplicate keys.
- Never hold a span guard (`Entered`/`EnteredSpan`) across `.await`; use `.instrument(span)` or `in_scope`. `await_holding_invalid_type` names those guard types in `clippy.toml`.
- Everything that grows or blocks needs a bound and an owner: a peer-claimed length is a limit to enforce, never a size to preallocate; state retained across model turns needs dedup/eviction; every spawned thread, connection, and network read needs a deadline and something that observes its exit.
- INFO-level telemetry volume must not scale with model turns, script words, or repeat iterations, but every refusal/failure path must emit its cause once.
- Construct expensive resources once: HTTP/model clients, Wasmtime engines, linkers, compiled components, and worker threads live at process or session scope, never per request/message/invocation.
- Every new pub item, dependency, config field, and error variant needs a non-test consumer in the same PR; otherwise make it private or delete it. Parsed-but-unread config and unreachable variants are bugs, not future-proofing.
- One definition per fact. A pre-validator mirroring an enforcing layer (CLI vs API server, gate vs broker, `Display` vs serde, copied constant) must share the definition or carry an equality-pinning test; a mirror that accepts what the authority rejects is worse than no mirror.

## What makes CI red

Branch protection runs [`.github/workflows/ci.yml`](.github/workflows/ci.yml) on pull requests only; a path classifier selects lanes, and any Rust change also selects the documentation lane. The complete, ordered command list behind the required `quality (stable)` context is [Root workspace](docs/development.md#root-workspace); this section only names what the PR template's three lines (fmt, clippy `-D warnings`, test) leave out:

- The release-profile `cargo check` of all four binaries, and the feature-off check `cargo check -p dekopon-core -p dekopon-capability -p dekopon-protocol --locked`, which is the only gate that compiles those crates without their opt-in `schemars` feature.
- `cargo machete` (CI pins 0.9.2): any unused dependency fails.
- Three `cargo tree` privilege greps over the normal dependency trees of `dekopon-run`, `dekopon`, and `dekopond`; any hit fails.
- Format, lint, test, and `wasm32-unknown-unknown` check of every `examples/providers/*/Cargo.toml` ([Provider example workspaces](docs/development.md#provider-example-workspaces)), plus wasm32 checks of `dekopon-provider-sdk`, `dekopon-provider-http`, and `dekopon-provider-storage` with each storage feature on its own.
- `shellcheck` over the repository scripts and rustdoc with warnings denied; the workspace tests and their `--doc` run separately in their own lane.
- The toolchain-free `documentation checks` job, which gates Markdown-only PRs too: the duplicate-entry check `python3 .github/scripts/check_docs_duplicates.py docs README.md AGENTS.md crates/*/README.md` (no repeated bold or code bullet key, and no repeated table first cell, within one section), and the audit-event gate, which fails when any `audit.event = "…"` literal under `crates/**/*.rs` is not present backticked in `docs/observability.md` ([Documentation gates](docs/development.md#documentation-gates)).
- `test (Rust 1.89.0)`: `cargo +1.89.0 test --workspace --all-features --locked --no-run`, then the same with `--doc`, after asserting that `rust-version` in `Cargo.toml` still equals the job's pin.
- `package crates`: `.github/scripts/verify-release-metadata.py` checks the shared version, the `CHANGELOG.md` shape (exactly one `## [Unreleased]` heading, which may be empty, plus exactly one dated `## [<version>] - YYYY-MM-DD` section for the current workspace version carrying a non-placeholder bullet under a Keep a Changelog category), and `.github/release-crates.txt` (no omission, no private or unknown crate, no duplicate, no dependent before its dependency); it runs for PRs touching Rust, manifests, crate READMEs, or `CHANGELOG.md`, and `cargo package --workspace --locked` runs when manifests, build scripts, crate READMEs, include lists, WIT, or publication machinery change.
- `dependency policy`: `cargo deny --all-features check` against `deny.toml`; a new advisory on a transitive dependency reddens the tree until the lockfile moves.
- `CLI smoke tests`: builds the four binaries, runs their help and example commands, and path-installs each with `cargo install --debug --locked --path`.
- Separate jobs cover the OpenObserve OTLP smoke test and the Helm chart. `actionlint .github/workflows/*.yml` and `shellcheck <SCRIPT>` are the local checks for workflow and script edits. `quality (stable)` and `test (Rust 1.89.0)` are branch-protection contexts: renaming a job leaves a permanently pending required check.

## Toolchain facts

- `rust-toolchain.toml` pins `channel = "stable"` with clippy and rustfmt, so a lint new in the latest stable can redden a previously green tree with no code change; `Cargo.toml` declares `rust-version = "1.89.0"`, and the MSRV job forces `RUSTUP_TOOLCHAIN=1.89.0` because the toolchain file outranks `rustup default`. Move both values together.
- Every gate passes `--locked`. After any dependency change, including a new edge between workspace crates, `--locked` fails with "cannot update the lock file": refresh `Cargo.lock`, and each affected `examples/providers/*/Cargo.lock`, commit them, and return to `--locked`. CI sets `CARGO_INCREMENTAL=0`.
- Workspace lints: `unsafe_code = "forbid"`, `rustdoc::broken_intra_doc_links = "deny"`, and clippy `dbg_macro`, `todo`, `unimplemented`, `await_holding_invalid_type`, `await_holding_lock`, `await_holding_refcell_ref`, `map_err_ignore`, and `let_underscore_must_use` at `deny`. So `let _ = writeln!(buf, ...)` is an error. A site that legitimately drops a value takes a scoped `#[allow(clippy::<lint>, reason = "…")]` whose reason says why the drop is safe; never widen an allow to a module or crate.
- Rebuilding a repository-owned provider `.wasm` needs exactly `rustc 1.97.0` and `wasm-tools 1.236.1` through the fixture's `build.sh`; `examples/providers/build-component.sh` refuses any other version, and the `WIT package` workflow (`.github/workflows/wit-package.yml`), not `ci.yml`, rebuilds every checked component and byte-compares it on pull requests and pushes to `main` that touch provider or WIT paths. A deterministic rebuild from unchanged source leaves the artifact unchanged.

## Environment gotchas

- Run `ci/fetch-external-provider-components.sh examples/providers` before `cargo test --workspace`; it installs the checksum-pinned echo, JSONPlaceholder, and memory-chat fixtures (gitignored, never committed), without which many tests fail with `NotFound` on `examples/providers/*-provider.wasm`. The CLI smoke and OTLP lanes fetch only `echo`.
- `target/` grows past 25 GB with test binaries, and a full disk surfaces as LLVM or linker "IO failure" errors rather than test failures. Recover by deleting `target/debug/incremental` and the large test executables under `target/debug/deps`; they regenerate. `CARGO_INCREMENTAL=0` keeps it from regrowing.
- The `dekopon-brokerd` unit test `a_secret_file_that_cannot_be_opened_still_names_its_errno` chmods a file to `0o000` and expects an open errno; it assumes a non-root user and fails under root (containers). It is not a regression signal there.
- OpenObserve, WIT package round trips, and the container image have their own command groups under [Validation](docs/development.md#validation); run them only when those areas change.

## When adding X, also update Y

- **Audit or telemetry event:** add the backticked name to `docs/observability.md` in the same change (CI-gated); grep `docs/` and `crates/*/README.md` for every identifier you rename or remove.
- **Crate:** `Cargo.toml` members and `[workspace.dependencies]`; `.github/release-crates.txt` in dependency order (omit only `publish = false` crates; adding an edge between existing crates can also require reordering it); `docs/architecture.md`, `docs/design.md` component ownership, `docs/roadmap.md`, `CHANGELOG.md`; the [Repository map](docs/development.md#repository-map); a `crates/<name>/README.md`; for a brand-new crate name, an explicitly authorized scoped-credential bootstrap, then the crates.io trusted-publisher entry (`dekopon-agents/dekopon`, `release.yml`, environment `crates-io`) registered immediately and the bootstrap credential revoked, as [Maintainer release process](README.md#maintainer-release-process) step 4 and [Dependencies, crates, CI, or releases](docs/development.md#dependencies-crates-ci-or-releases) describe.
- **Config field or CLI flag:** the owning document (`docs/run.md`, `docs/dekopond.md`, `docs/catalog.md`, `docs/cli.md`), the crate README, `CHANGELOG.md`, the example under `examples/`, plus parser tests and black-box CLI tests; JSON/YAML output shapes and exit codes are compatibility surfaces.
- **Capability or provider:** the catalog example, the constraint set and Cedar policy where the broker serves it, `docs/catalog.md` and `docs/security-model.md`, the fixture and its `build.sh` if repository-owned, and the import inspection tests that reject WASI or unexpected imports.
- **WIT change:** every mirror listed in [Provider contract or host](docs/development.md#provider-contract-or-host), the package version bump, fixtures rebuilt with the pinned toolchain, all three `wkg.lock` files unchanged, and the [Published WIT packages](docs/development.md#published-wit-packages) checks.
- **Dependency:** the root `Cargo.toml` `[workspace.dependencies]` entry, refreshed lockfiles, `cargo deny --all-features check`, `cargo machete`, and the MSRV `--no-run` build when the crate raises its floor.

## Conventions

- Tests are named for the behavior they pin (`conflicting_providers_are_all_reported_in_one_failure`), live beside the owning crate, and cover failure paths: a failure-path test asserts the surfaced error or log carries the underlying cause; a validation test constructs at least two simultaneous conflicts and asserts both are reported. Mock network peers on loopback; never read another application's credential store.
- Commit subjects follow conventional commits where practical (`feat(config): detect duplicate agents`, `fix: …`, `test: …`, `docs: …`, `chore(release): …`). Commits keep the `Co-Authored-By:` and `Claude-Session:` trailers the agent harness adds; model and session identifiers appear nowhere else, not in code, comments, or documentation.
- Pull requests follow `.github/pull_request_template.md`: **Summary**, **Security impact** (trust boundaries, capabilities, credentials, external effects, or "None"), the **Validation** checklist, and **Limitations / follow-ups**. Describe only checks that actually ran; automated agents never approve their own changes.
- Documentation, examples, and a `CHANGELOG.md` `[Unreleased]` bullet in a Keep a Changelog category ship in the same PR as the behavior; the roadmap is never edited as proof of implementation.
- Prefer targeted `cargo test -p <crate> --locked` while iterating, then the scope group under [Validation](docs/development.md#validation) and the checklist in [Before opening a pull request](docs/development.md#before-opening-a-pull-request).

## Release and publication discipline

Releases require explicit human authorization for the named version only, never standing permission. Follow [Maintainer release process](README.md#maintainer-release-process) and [Dependencies, crates, CI, or releases](docs/development.md#dependencies-crates-ci-or-releases) exactly; CI validates the changelog headings, the crates publication list, and package metadata on every pull request that touches Rust, manifests, or `CHANGELOG.md`, so do not improvise around a red check. `cargo release` only prepares the shared-version commit and tag and pushes nothing; the pushed `v<VERSION>` tag is the single publication gate, and the `Release` workflow builds and attests three platform archives, creates the GitHub release, publishes the container image, updates the Homebrew tap, and publishes every public crate in `.github/release-crates.txt` order. Crate publication can stop partway (the v0.12.0 tag's crates.io job failed while its other jobs succeeded); the recovery is a `workflow_dispatch` of `Release` against the existing tag with `publish_to_crates=true`, which skips versions already present and rebuilds nothing else. Crate versions and Git tags are immutable; fix on `main` and cut a patch. Never print or expose credentials while diagnosing publication. After publishing, verify crates.io and fresh version-pinned installs of `dekopon`, `dekopon-run`, `dekopon-brokerd`, and `dekopond`; an upload command is not verification.

## Working method

1. Inspect `git status --short --branch`; preserve unrelated work and start follow-ups from current `main`.
2. Classify the requested behavior as **Current**, **Committed direction**, or **Exploration** using `docs/design.md`.
3. Identify the process that owns the data and the process that owns the authority.
4. Inspect the relevant implementation, nearest tests, generated files, and mirrored contracts; do not infer current behavior from roadmap prose.
5. Make the smallest coherent change that preserves the authority boundary.
6. Add failure-path, serialization, CLI, or compile-fail tests appropriate to the change, and the "also update" items above, in the same pull request.
7. Run the scope-specific checks in `docs/development.md`, starting with `git diff --check`; never report a check or remote operation as successful unless it was observed.

If the requested implementation conflicts with the design or security model, stop and surface the conflict for human decision rather than silently choosing a new architecture.

## Never

- Publish crates, push tags, create releases, weaken branch protection, or add credentials without explicit human authorization for that action.
- Commit credentials, private endpoints, local paths, generated coverage data, fetched provider fixtures, or a hand-edited `.wasm` or `Cargo.lock`.
- Add WASI, host imports, credentials, or authorization arguments to `dekopon-run`'s immediate mode or its broker client subcommands, or a broker crate to `dekopon`, `dekopon-run`, or `dekopond`.
- Edit one WIT mirror without the others, or change a published WIT package without a version bump.
- Use `unsafe`, `todo!`, `unimplemented!`, `dbg!`, panics on user input, `anyhow` in public APIs, or `map_err(|_| …)`.
- Claim a command or remote check passed without observing it, or describe roadmap items as shipped.
