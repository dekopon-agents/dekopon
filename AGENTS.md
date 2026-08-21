# Guidance for coding agents

## Required reading

**Always read [`docs/design.md`](docs/design.md) first.** It is the canonical overview of Dekopon's product model, vocabulary, authority transition, component ownership, and the distinction between current behavior and committed direction.

Then read [`docs/development.md`](docs/development.md). It maps source and tests, records generated and mirrored files, explains the separate provider workspace, and gives scope-specific validation commands.

Finally, read the documents selected by the work:

| If the change touches… | Read… | Because… |
|---|---|---|
| Any product behavior or architecture | [`docs/design.md`](docs/design.md) | It defines the non-negotiable invariants and accepted design decisions. |
| Capabilities, actors, identity, policy, credentials, providers, evidence, audit, or external effects | [`docs/security-model.md`](docs/security-model.md) | It defines trusted inputs, untrusted content, threats, and current limitations. |
| Crate boundaries, dependencies, protocols, daemon/broker separation, async, Wasmtime, or Cedar | [`docs/architecture.md`](docs/architecture.md) | It maps design responsibilities to current and future implementation boundaries. |
| Operator auth, catalog CLI parsing, config discovery, resource reads, output, or exit codes | [`docs/cli.md`](docs/cli.md) | It is the current operator contract. |
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
- Parse config once into typed resources; do not spread YAML handling through command execution.
- Provider schemas are model-facing metadata, not complete host validation; providers must validate capability-specific input.
- The SDK and host provider WIT files must remain identical. The SDK copy is also the source for the published `dekopon:provider` WIT package; preserve its import-free boundary and bump its WIT version before changing an already-published contract. The canonical `dekopon:http` WIT file and every checked-in guest or broker-host mirror must also remain identical. Never hand-edit generated provider `.wasm` files; rebuild them from their Rust source.
- Root workspace commands do not cover the separate workspaces under `examples/providers/`; validate each affected provider workspace explicitly.
- Do not publish crates, create releases, weaken branch protection, or add credentials without explicit human authorization.

## Known failure patterns — check your diff against each

These are the classes a deep review actually found, repeatedly. Each is checkable.

- Never discard an error's cause. `map_err(|_| …)`, `let _ = fallible()`, and bool/Option returns from multi-cause checks are bugs: emit a tracing event carrying the reason at the discard site, or return an error naming which check failed.
- Classify errors on the axis the caller acts on: retryable vs permanent, executed vs not-executed. Never report a permanent exhaustion as transient, a completed result as timed out, or exit 0 with the daemon's work dead.
- Validation reports every conflict, then fails. Never stop at the first error; never last-wins on duplicate keys.
- Never hold a span guard (`Entered`/`EnteredSpan`) across `.await`; use `.instrument(span)` or `in_scope`.
- Everything that grows or blocks needs a bound and an owner: a peer-claimed length is a limit to enforce, never a size to preallocate; state retained across model turns needs dedup/eviction; every spawned thread, connection, and network read needs a deadline and something that observes its exit.
- INFO-level telemetry volume must not scale with model turns, script words, or repeat iterations — but every refusal/failure path must emit its cause once.
- Construct expensive resources once: HTTP/model clients, Wasmtime engines, linkers, compiled components, and worker threads live at process or session scope, never per request/message/invocation.
- Every new pub item, dependency, config field, and error variant needs a non-test consumer in the same PR; otherwise make it private or delete it. Parsed-but-unread config and unreachable variants are bugs, not future-proofing.
- One definition per fact. A pre-validator mirroring an enforcing layer (CLI vs API server, gate vs broker, `Display` vs serde, copied constant) must share the definition or carry an equality-pinning test; a mirror that accepts what the authority rejects is worse than no mirror.
- New or renamed telemetry/audit event names land in [`docs/observability.md`](docs/observability.md) in the same PR; grep `docs/` and crate READMEs for every identifier or event you rename or remove.

## Release and publication discipline

Releases require explicit human authorization for the named version only — never standing permission. Follow [`README.md`](README.md#maintainer-release-process) and [`docs/development.md`](docs/development.md#dependencies-crates-ci-or-releases) exactly; CI validates changelog entries, the crates publication list, and package metadata on every pull request, so do not improvise around a red check. `cargo release` only prepares the shared-version commit and tag; GitHub Actions owns publication. Crate versions and Git tags are immutable; never print or expose credentials while diagnosing publication. After publishing, verify crates.io and fresh version-pinned installs of `dekopon`, `dekopon-run`, and `dekopon-brokerd` — an upload command is not verification.

## Working method

1. Inspect `git status --short --branch`; preserve unrelated work and start follow-ups from current `main`.
2. Classify the requested behavior as **Current**, **Committed direction**, or **Exploration** using `docs/design.md`.
3. Identify the process that owns the data and the process that owns the authority.
4. Inspect the relevant implementation, nearest tests, generated files, and mirrored contracts; do not infer current behavior from roadmap prose.
5. Make the smallest coherent change that preserves the authority boundary.
6. Add failure-path, serialization, CLI, or compile-fail tests appropriate to the change. A failure-path test asserts the surfaced error/log carries the underlying cause; a validation test constructs at least two simultaneous conflicts and asserts both are reported.
7. Update affected documentation and examples in the same pull request.
8. Run the scope-specific checks in `docs/development.md`; never report a check or remote operation as successful unless it was verified.

If the requested implementation conflicts with the design or security model, stop and surface the conflict for human decision rather than silently choosing a new architecture.
