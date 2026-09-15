# Guidance for coding agents

Dekopon is an extensible runtime for self-hosted AI agents: providers are WebAssembly components, the model proposes while a separate broker authorizes and executes, a model may reference a secret but can read none, and one complete trace covers every run.
Read the [constitution](docs/design.md#constitution) first; its goals and non-goals decide what belongs in this tree.

## Start here

Read [`docs/design.md`](docs/design.md), then [`docs/development.md`](docs/development.md) and [`CONTRIBUTING.md`](CONTRIBUTING.md).
The [repository map](docs/development.md#repository-map) owns source/test locations, separate provider workspaces, generated artifacts, and shared scaffolding; do not recreate that inventory here.
[`docs/README.md`](docs/README.md) is the complete documentation map, including per-crate contracts and the roadmap (sequencing, never proof of implementation).

Choose the relevant reading before editing:

- **Architecture and security:** [`architecture.md`](docs/architecture.md), [`security-model.md`](docs/security-model.md), and [`secrets.md`](docs/secrets.md) for ownership, authenticated identity, capabilities, policy, credentials, and effects.
- **Runtime, gateway, and memory:** [`dekopond.md`](docs/dekopond.md) distinguishes RAM conversation replay from durable provider memory;
  [`inference.md`](docs/inference.md) covers model requests and prompt-cache guarantees.
  Use [`catalog.md`](docs/catalog.md), [`cli.md`](docs/cli.md), and [`improvement.md`](docs/improvement.md) for authored fields, operator commands, and skills;
  the [`agent`](crates/dekopon-agent/README.md) and [`shell`](crates/dekopon-shell/README.md) READMEs own orchestration limits.
- **Providers, WIT, and HTTP:** [provider change map](docs/development.md#provider-contract-or-host), [`brokerd` boundaries](crates/dekopon-brokerd/README.md#boundaries), and [`HTTP host`](crates/dekopon-http-host/README.md) for mirrored interfaces,
  generated components, and native enforcement.
- **Observability:** [`observability.md`](docs/observability.md), [`HTTP host`](crates/dekopon-http-host/README.md), and [`telemetry`](crates/dekopon-telemetry/README.md) for request evidence, traces, audit events,
  and the existing OpenTelemetry log bridge; inspect these before adding instrumentation.
- **Validation and toolchains:** [change maps](docs/development.md#change-maps), [validation groups](docs/development.md#validation), and [PR checklist](docs/development.md#before-opening-a-pull-request).
  Pins and lockstep requirements live in [`rust-toolchain.toml`](rust-toolchain.toml) and [`ci/toolchain.env`](ci/toolchain.env);
  lint definitions live in [`Cargo.toml`](Cargo.toml) and [`clippy.toml`](clippy.toml).
- **Release and deployment:** [maintainer release process](README.md#maintainer-release-process), [dependency/publication mechanics](docs/development.md#dependencies-crates-ci-or-releases), [`operations.md`](docs/operations.md), [`upgrading.md`](docs/upgrading.md), and [`container-image.md`](docs/container-image.md).
  Credential delivery has separate guides: [`chatgpt-credential.md`](docs/chatgpt-credential.md) and [`1password-eso.md`](docs/1password-eso.md).

## Critical rules

- Only the broker authorizes and executes effects; a capability declaration permits proposals, not ambient authority.
  Read authority never implies write authority: external writes require explicit narrow capabilities. Identity comes from an authenticated envelope, never model, repository, skill, or payload text.
- Keep `dekopond` and `dekopon-brokerd` separate processes. The gateway gains no policy, provider credentials, or authorization path; the broker gains no model orchestration.
  Preserve the [opposite-direction dependency gates](docs/development.md#root-workspace).
- Provider secret bytes stay inside the broker boundary, never in prompts, provider memory, protocol, evidence, or logs. Model credentials stay inside the selected model client.
  Configuration names credentials, never embeds values; carry secrets in `dekopon_core::Redacted` and minimize `expose`/`into_inner` sites. Instructions and skills are untrusted model text, not secret stores.
- One complete W3C trace covers each message's prompts, scripts, command words and arguments, decisions, and egress; only secret bytes and daemon credentials stay out.
  Bound attribute size, never span count. Never hold `Entered`/`EnteredSpan` across `.await`; use `.instrument(span)` or `in_scope`.
- Parse configuration once into typed resources; unknown authored fields fail. Report every validation conflict together, never last-wins duplicate keys.
  Provider schemas are model-facing metadata, not complete host validation: providers validate capability-specific input.
- Keep every [WIT mirror](docs/development.md#provider-contract-or-host) byte-identical; change all copies and bump the affected published WIT version before changing its contract.
  Never hand-edit generated `.wasm` or lockfiles; rebuild components from source using their pinned `build.sh` and regenerate lockfiles with Cargo.
- Preserve error causes in a returned error or a tracing event at the discard site, including multi-cause bool/Option checks. Emit each refusal/failure cause once.
  Classify retryable versus permanent and executed versus not-executed accurately; never exit successfully with daemon work dead.
- Bound everything that grows or waits and assign an owner: enforce peer lengths rather than preallocating from them, deduplicate/evict retained state, and give threads, connections, and reads deadlines and exit observers.
  Reuse expensive clients, engines, linkers, compiled components, and workers at process/session scope.
- Every new public item, dependency, config field, or error variant needs a non-test consumer now.
  Keep one definition per fact; a validator or constant mirroring an authority must share its definition or have an equality-pinning test. Do not weaken lints; any justified allowance is site-scoped with a reason.
- Publishing crates, creating releases, pushing/moving tags, weakening branch protection, or adding credentials requires explicit human authorization for that action and named release version, never standing permission.
  Do not commit credentials, private endpoints, local paths, coverage artifacts, or fetched provider fixtures.

## Working and verifying

1. Confirm the repository root and inspect `git status --short --branch`; preserve unrelated work and start follow-ups from current `main`. Classify behavior as **Current**, **Committed direction**, or **Exploration**; identify data and authority owners. Stop for human decision if design/security constraints conflict.
2. Inspect implementation, nearest tests, and mirrored/generated contracts. Make the smallest coherent change; follow the [change maps](docs/development.md#change-maps) for companion docs, examples, tests, and behavior changelog entries.
3. Start with `git diff --check`; scope Cargo checks with `--locked` and validate each affected separate provider workspace. Markdown-only changes use the [toolchain-free documentation gates](docs/development.md#documentation-gates), not Rust builds. Check disk/target growth and follow the [artifact lifecycle](docs/development.md#validation); never delete active or another owner's artifacts.
4. For deployment diagnosis, inspect the **deployed** runtime version/ref and provider component digest, not checkout HEAD or an old report. Distinguish browser, fixture, native-host, provider-loading, and live-provider-request acceptance: none proves the others.
   State precisely which boundary and bytes were exercised and which verification gaps remain.
5. Follow the PR template and verify required checks on the exact submitted head. Never claim an unobserved command, remote operation, or future behavior succeeded.
   Automated agents do not approve their own changes; required CI and fresh human review precede merge.
