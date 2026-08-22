# Guidance for coding agents

This file applies to the entire repository. Humans can use the same reading guide; agents are expected to follow it before editing code or documentation.

## Required reading

**Always read [`docs/design.md`](docs/design.md) first.** It is the canonical overview of Dekopon's product model, vocabulary, authority transition, component ownership, and the distinction between current behavior and committed direction. Reading only the roadmap is not enough: roadmap entries are sequencing ideas, not proof that a feature exists or permission to weaken a boundary.

Then read [`docs/development.md`](docs/development.md). It maps source and tests, records generated and mirrored files, explains the separate provider workspace, and gives scope-specific validation commands.

Finally, read the documents selected by the work:

| If the change touches… | Read… | Because… |
|---|---|---|
| Any product behavior or architecture | [`docs/design.md`](docs/design.md) | It defines the non-negotiable invariants and accepted design decisions. |
| Capabilities, actors, identity, policy, credentials, providers, evidence, audit, or external effects | [`docs/security-model.md`](docs/security-model.md) | It defines trusted inputs, untrusted content, threats, and current limitations. |
| Crate boundaries, dependencies, protocols, daemon/broker separation, async, Wasmtime, or Cedar | [`docs/architecture.md`](docs/architecture.md) | It maps design responsibilities to current and future implementation boundaries. |
| Operator auth, catalog CLI parsing, config discovery, resource reads, output, or exit codes | [`docs/cli.md`](docs/cli.md) | It is the current operator contract. |
| `AgentSpec`, `CapabilitySpec`, or `ProviderSpec` fields, or what a catalog value is consumed by | [`docs/catalog.md`](docs/catalog.md) | It records every `v1alpha1` field, its actual consumer, and which fields are reserved and read by nothing. |
| Operating a running deployment: startup refusals, audit checkpoint recovery, draining, or socket and directory hygiene | [`docs/operations.md`](docs/operations.md) | It indexes the per-crate operational contracts, chiefly [`crates/dekopon-brokerd/README.md`](crates/dekopon-brokerd/README.md), by operator question. |
| A breaking configuration change, a protocol change, or anything an operator must do between releases | [`docs/upgrading.md`](docs/upgrading.md) | It records the migrations `CHANGELOG.md` only names, the lockstep rule, and the restart order. |
| Exporting, storing, or deploying a ChatGPT subscription credential | [`docs/chatgpt-credential.md`](docs/chatgpt-credential.md) | It records the rotating-refresh-token constraints that decide how a credential may reach a pod. |
| Model requests, prompt caching, cache retention, conversation memory, or long-lived agent memory | [`docs/inference.md`](docs/inference.md) | It distinguishes current wire behavior and bounded chat history from provider guarantees and future memory design. |
| Immediate providers, Wasm components, prompt tools, model endpoints, or limits | [`docs/run.md`](docs/run.md) | It defines the experimental current runner and the privileges it must not gain. |
| Runner traces, OTLP logs, telemetry redaction, or OpenObserve | [`docs/observability.md`](docs/observability.md) | It defines signal contents, configuration, audit limitations, and end-to-end coverage. |
| Provider source, WIT, generated Wasm, tests, CI, dependencies, packaging, or releases | [`docs/development.md`](docs/development.md) | It records repository mechanics and validation traps that root workspace commands do not cover. |
| The `Dockerfile`, the container image workflow, or a container deployment | [`docs/container-image.md`](docs/container-image.md) | It records that the image reuses the release archives rather than compiling, the numeric runtime UID, the baked provider paths, and the file ownership the broker refuses to start without. |
| Broker-mediated provider HTTP, host imports, or broker client mode | [`docs/broker-http.md`](docs/broker-http.md) | It defines the accepted process boundary, buffered HTTP contract, authorization inputs, and staged delivery. |
| Chat transports, gateway configuration, routing, agent sessions, or attested proposals | [`docs/dekopond.md`](docs/dekopond.md) | It defines the unprivileged daemon's contract and the authority it deliberately does not hold. |
| Deployment secrets, 1Password, External Secrets, or delivering a credential file into a cluster | [`docs/1password-eso.md`](docs/1password-eso.md) | It records the deployed secret-store configuration, the bootstrap a human owns, and the file hygiene no Kubernetes volume satisfies. |
| Scope, priority, package names, or a proposed new crate | [`docs/roadmap.md`](docs/roadmap.md) | It records sequencing and explicit non-goals; it does not make future components current. |

[`docs/README.md`](docs/README.md) is the complete documentation map. Read [`CONTRIBUTING.md`](CONTRIBUTING.md) for validation and pull-request expectations.

## Rules that must survive every change

- A model may propose an invocation, but only the broker may authorize it.
- Capability declarations permit proposals; they do not grant ambient process authority.
- Read authority never implies write authority. External writes require explicit narrow capabilities.
- Trusted actor identity comes from an authenticated envelope, never from model or repository content.
- Provider credentials remain inside the broker boundary and out of prompts, config, evidence, and logs; model credentials stay inside the selected model client and never enter provider components.
- `dekopond` and `dekopon-brokerd` remain separate processes. External-write authority exists now, so this is a live invariant rather than a future one: the gateway must never gain policy, provider credentials, or an authorization path of its own, and CI rejects any broker crate appearing in its normal dependency tree.
- The direct `dekopon-run` provider path remains read-only and import-free; do not add provider credentials, WASI, host I/O, local writes, external writes, or authorization claims to immediate mode. A broker-backed mode may submit proposals, but only the separate broker may resolve privileged imports or execute effects.
- Do not describe unimplemented daemons, policy, privileged provider interfaces, or external effects as available.
- Do not add empty crates or heavy future dependencies without meaningful, tested behavior.
- Parse config once into typed resources; do not spread YAML handling through command execution.
- Provider schemas are model-facing metadata, not complete host validation; providers must validate capability-specific input.
- The SDK and host provider WIT files must remain identical. The SDK copy is also the source for the published `dekopon:provider` WIT package; preserve its import-free boundary and bump its WIT version before changing an already-published contract. The canonical `dekopon:http` WIT file and every checked-in guest or broker-host mirror must also remain identical. Never hand-edit generated provider `.wasm` files; rebuild them from their Rust source.
- Root workspace commands do not cover the separate workspaces under `examples/providers/`; validate each affected provider workspace explicitly.
- Do not publish crates, create releases, weaken branch protection, or add credentials without explicit human authorization.

## Release and publication discipline

Read the maintainer procedure in [`README.md`](README.md#maintainer-release-process) and the validation details in [`docs/development.md`](docs/development.md#dependencies-crates-ci-or-releases) before changing versions, tags, package metadata, or release automation.

- Explicit authorization applies only to the release the human named; it is not standing permission for later versions.
- Prepare from a clean, current `main`. Every public workspace package must share the release version, and the tag must be exactly `v<VERSION>`.
- Every release must update [`CHANGELOG.md`](CHANGELOG.md): application tag `v<VERSION>` requires a dated, non-empty `[VERSION]` section, and chart tag `dekopon-chart-<VERSION>` requires `[dekopon-chart-<VERSION>]`. Keep pending work under `[Unreleased]`. Pull-request CI checks both current versions, and each tag workflow rechecks its own entry before publishing.
- `cargo release` prepares the shared-version commit and tag. Repository configuration intentionally disables its publish and push phases; GitHub Actions owns release artifacts and crates.io trusted publication.
- An explicitly authorized application tag push validates and publishes provenance-attested GitHub archives, the container image, and every crates.io package in dependency order. The `crates-io` environment is part of the trusted-publisher OIDC identity, not a second required-reviewer gate. A manual `Release` workflow dispatch with `publish_to_crates=true` is only the idempotent recovery path for an existing tag; it skips immutable versions already present.
- Keep [`.github/release-crates.txt`](.github/release-crates.txt) in package-dependency order, including internal build and dev dependencies that `cargo package` resolves. Pull-request and release-metadata validation check exact coverage, uniqueness, and ordering so a newly public crate cannot be silently omitted or published before a dependency needed for verification.
- Every public package must have a crates.io GitHub trusted-publisher configuration for repository `dekopon-agents/dekopon`, workflow `release.yml`, and environment `crates-io`. A brand-new crate name cannot use OIDC before it exists: bootstrap it only with explicit authorization and a narrowly scoped credential, register that trusted publisher immediately, then revoke the bootstrap credential.
- Before tagging, update release-facing status/install text in the root and crate READMEs, then run `cargo package --workspace --locked` in addition to the full test, lint, docs, and dependency checks. Package verification proves local archives; it does not prove the crates publication list is complete, which is why the workflow checks both.
- Treat crate versions and Git tags as immutable. Never move a release tag, overwrite a published package, expose a long-lived token in a workflow, or print credentials while diagnosing publication. Routine publication uses the OIDC workflow; an explicitly authorized local recovery must use the narrowest credential available and revoke it when the recovery is complete.
- The publication job may retry only crates.io's explicit new-package `429`, waiting until the server-provided retry time. Any other upload or API failure must stop rather than being hidden by a generic retry loop.
- After publication, verify every public package through crates.io and test fresh version-pinned installs of `dekopon`, `dekopon-run`, and `dekopon-brokerd`. Do not announce success based only on an upload command.

## Working method

1. Inspect `git status --short --branch`; preserve unrelated work and start follow-ups from current `main`.
2. Classify the requested behavior as **Current**, **Committed direction**, or **Exploration** using `docs/design.md`.
3. Identify the process that owns the data and the process that owns the authority.
4. Inspect the relevant implementation, nearest tests, generated files, and mirrored contracts; do not infer current behavior from roadmap prose.
5. Make the smallest coherent change that preserves the authority boundary.
6. Add failure-path, serialization, CLI, or compile-fail tests appropriate to the change.
7. Update affected documentation and examples in the same pull request.
8. Run the scope-specific checks in `docs/development.md`; never report a check or remote operation as successful unless it was verified.

If the requested implementation conflicts with the design or security model, stop and surface the conflict for human decision rather than silently choosing a new architecture.
