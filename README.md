# Dekopon

Dekopon is an extensible runtime for self-hosted AI agents. Providers are WebAssembly components; the model proposes, a separate broker authorizes and executes; and provider credentials can never reach the model. The three goals that decide what belongs here are the [constitution](docs/design.md#constitution).

> **Status:** pre-1.0 and in active production use. The one known production deployment is the maintainer's Raspberry Pi homelab: Kubernetes on a home fiber connection, serving Discord, Slack, and WhatsApp. This is not a general production-readiness guarantee. Both daemons are Unix-only. `dekopon-brokerd` maps configured peer UIDs to trusted context under the [current local process boundary](docs/security-model.md#current-local-process-boundary); `dekopond` holds chat and model credentials and no broker authority.

## Design documentation

Start with [`docs/design.md`](docs/design.md) for the product model, authority flow, component boundaries, and accepted decisions. [`docs/development.md`](docs/development.md) maps source, tests, generated artifacts, separate workspaces, and validation. [`docs/inference.md`](docs/inference.md) traces Slack model calls through prompt caching and bounded memory down to literal Rust and wire JSON. [`docs/README.md`](docs/README.md) provides task-based reading paths; repository-wide agent instructions live in [`AGENTS.md`](AGENTS.md). See [`CHANGELOG.md`](CHANGELOG.md) for the history of every application and chart tag.

## What it does

Ordered by the goals it serves. Credentials stay inside the broker:

- Public inert secret DRNs, decided by a separate Cedar `secret.use` grant against an owner-only source/use map, with invocation-pinned secure-file/Kubernetes/1Password/Vault/AWS/GCP/Azure adapters (including broker-private Kubernetes TokenRequest minting), canonical host/method/path/query bounds, native Basic/Bearer rendering, binding-swap refusal, and a credential echo check. Providers never see values; a provider command may propose a reference. See [`docs/secrets.md`](docs/secrets.md).
- One capability presents a different credential per acting agent through `credential`/`agents.<id>.credentials`. *Committed direction:* these bindings will be replaced by public DRNs ([migration requirements](docs/design.md#legacy-credential-bindings)).
- Credential-free self-inspection: an authorized session calls `inspect_agent_config` for its exact standing prompt, route limits, and the capabilities Cedar currently grants that sender. Raw policy, identity, endpoints, paths, and every credential name or value stay out.

Correlated tracing from receipt to execution:

- Correlated OpenTelemetry traces and logs across transport receipt, agent session, model turn, shell command, broker decision, provider invocation, and native HTTP egress. [`examples/otel-traces`](examples/otel-traces/README.md) runs an OpenObserve receiver and a real gateway/broker smoke test.
- Broker audit as one structured log record per decision and per outcome, inside the same trace.

Extensibility through Wasm providers:

- A Rust provider SDK, a bounded Wasmtime component host with a fresh store per call, and a typed native/real-component testkit (`dekopon-provider-sdk-testkit`); broker-host fixture tests run external and storage components against exact grants. Inside an authorized invoke, providers read the wall clock, a monotonic clock (`dekopon:clock@1.1.0`) and bounded OS entropy (`dekopon:random@0.1.0`) with no extra grant.
- A published streaming `dekopon:http@1.2.0` contract, a guest Rust facade, a bounded native HTTP engine, an asynchronous broker component host, a deny-by-default authorization core, and a bounded identity-free Unix protocol.
- Broker-owned JSONL and durable-file provider storage, plus optional on-demand durable chat memory: model-queryable only under an effective all-three grant, recorded once after gateway-attested transport acceptance, never automatically replayed into a prompt.
- An offline `dekopon-brokerd provider` manager for exact fully qualified OCI tags or manifest digests: strict desired and generated-lock files, a content-addressed component store, complete provider-set validation before atomic activation, offline list and verify, an offline `precompile` that fills and repairs the compiled-component cache before startup so the long-lived broker only reads it, and a startup comparison of locked digest, length, and provider ID against the exact Wasmtime input buffer. It adds no daemon-startup network path.
- Out-of-tree providers must be deployed independently against the current stdio provider contract; the image and CLI archives bundle no out-of-tree components. A SQLite-compatible [`turso-sql`](https://github.com/dekopon-agents/dekopon-provider-turso-sql) provider is available separately.
- Strongly typed identifiers and an invocation typestate that separates a proposal from broker authorization.
- A sandboxed bash-flavored script interpreter (`dekopon-shell`) whose provider command words propose capability calls instead of spawning operating-system processes, with compound commands (`if`/`for`/`while`/`until`/`case`/`{ ...; }`) as pipeline stages, `[[ ... ]]`, enforced `set -e`/`-u`/`-o pipefail`, `read`, real parameter expansion, and two script-addressable streams. The shared agent layer hands it to a model as its `bash` tool, so a multi-step plan is one tool call rather than many round trips.
- An unprivileged Tokio lifecycle seam (`dekopon-process`) that runs one typed async operation as one payload-free traced task and joins it before returning; if the outer caller is dropped, its required observer receives the full outcome anyway. The agent's broker leg is a cancellable node tied to gateway session Stop.

The operator surface on top:

- Strict YAML and JSON agent resources, with duplicate, invalid-name and unknown-field detection reported in one refusal.
- Configuration as directories: each daemon and the agent catalog may read a folder of small files — one per tenant, provider or transport — where any collision refuses startup, and `dekopon-brokerd check` / `dekopond check` run the startup validation offline as a pre-flight. Principals carry Cedar groups and providers group their capabilities, so one statement grants a family or a provider's reads. See [upgrading to 0.22.0](docs/upgrading.md#principals-groups-and-capability-blocks-0220).
- Wakes: an agent may come back to the same conversation after a delay or when a model-free watch probe fires; probe runs are refused every writing capability.
- Isolated model-account authentication through `dekopond auth`, with table, wide, JSON, YAML, and name status output.
- A chat gateway over Slack Socket Mode, Discord Gateway, Telegram long polling, a signed Meta WhatsApp Cloud API webhook with bounded PNG/JPEG photo inputs, and an owner-only local socket. Authenticated messages route to catalog agents while the broker remains the only authority.
- Both daemons export OTLP to a receiver on a private CA: the standard `OTEL_EXPORTER_OTLP_CERTIFICATE` adds its PEM roots beside WebPKI, and chart 0.11.0 wires it through `otlp.caBundle`. See [upgrading to 0.21.0](docs/upgrading.md#otlp-private-ca-0210).
- Provider HTTPS can add CA roots independently of exact-authority non-public egress, and the broker supplies per-provider owner settings only during authorized invocation. A URL-free, bounded OpenObserve provider can now be installed separately; it is **not** bundled or granted by this release. See [upgrading to 0.20.0](docs/upgrading.md#provider-https-and-owner-settings-0200).
- Messages and photos from the same sender steer their running turn: routes choose model-only interruption (`abort`, default) or the next step boundary. Other senders and wakes queue follow-ups under their own broker legs; eight pending items fit per conversation. Accepted steers and queued non-wake follow-ups get a best-effort 👀 acknowledgment when a liveness target exists. Photos still download only on demand. See [chat steering](docs/dekopond.md#sessions).
- Attachments a person sends: an image or document becomes a numbered chat asset named in the prompt, which a model opens on demand rather than carrying on every turn, under media-type, byte, attempt, and per-conversation limits.
- Opt-in native liveness after fresh authorization: automatic progress prefers native status or a typing/reaction indicator over redundant progress messages; explicit editable progress and optional answer streaming remain available, driven by one per-session policy task, with Slack Agent Working/Stop sessions and a classic `:tangerine:` reaction fallback. A liveness failure never changes the answer, and a stop — a native Stop, a cancel button, a configured stop word, an operator shutdown, or a wall-clock bound — is cooperative rather than rollback.
- Slack Agent channel threads owned per authenticated sender after fresh authorization: that sender continues without repeating the mention, and the optional `decline_chat_reply` decision lets the agent post nothing when a reply would only take the last word. Ambient channel history never reaches routing or inference.
- Image generation is a broker-authorized provider effect, not a gateway model tool. Explicit broker-authorized `asset.send` delivers retained assets to Slack, Discord, Telegram, WhatsApp, or the local socket under each transport's supported media types. Exact `chat-asset:<N>` proposal references automatically resolve to scoped read-only descriptors. Neither direction passes attachment bytes through the model.

## What does not work yet

Semantic or vector memory, cross-agent sharing, task memory, deletion and export UX, and encryption at rest do not exist. Durable chat turns carry across broker and gateway restarts only inside one provider/agent/sender/transport/channel/conversation scope, and are read on demand with `memory recent` or `memory search`; recording never deduplicates, so a redelivered message becomes a second stored turn. SQL reaches providers only as the optional out-of-tree component, and no shipped memory path uses it.

There is no catalog operator CLI and no general invocation CLI. Secret sources need explicit strict bootstrap files: Vault dynamic leases, AWS ambient role chains and IRSA, GCP ADC and WIF, Azure managed identity, kubeconfig exec plugins, custom CAs for sources other than `kubernetesTokenRequest`, and caching or stale fallback do not exist. Catalog provider and status resources are declarations only. The broker's provider manager has exact-reference `sync`, `list`, `verify`, and `precompile` only: no SemVer ranges, private-registry credentials or custom roots, publisher-provenance verification, install/update/remove/prune lifecycle, revocation response, or container-staging integration. A digest proves bytes rather than publisher identity, so archive staging verifies release provenance separately; operators verify any provider artifacts they deploy. Only the broker can execute the provider effects the catalog example represents.

## Install

- **Mac:** [Run Dekopon on a Mac](docs/run-on-mac.md) covers Homebrew, verified archives,
  source builds, model authentication, and a read-only local session. No Kubernetes is required.
- **Kubernetes:** [Deploy Dekopon on Kubernetes](docs/kubernetes.md) covers the Helm chart,
  ArgoCD, configuration and credentials, storage, upgrades, and shutdown ordering.

The [Homebrew tap](https://github.com/dekopon-agents/homebrew-tap) and
[release archives](https://github.com/dekopon-agents/dekopon/releases) also support Linux on ARM64
and x86-64. See the [native installation methods](docs/run-on-mac.md#install) for the daemon pair;
select the Linux archive for your architecture. The archives contain no provider components. Both daemons are Unix-only and must come from the
same release. [Container image details](docs/container-image.md) cover image contents, provenance,
and local image builds; [development](docs/development.md) covers building and checking a checkout.

## Run the flagship example

[`examples/conditional-write`](examples/conditional-write/README.md) is the whole system in one deployment: a mapped sender asks in Slack for a record to be updated, the gateway attests to the sender and decides nothing, and the broker authorizes one bounded read and one etag-pinned conditional write. The delete the same component exposes is absent, and unreachable: no constraint set describes it. The broker injects a token bound to `api.example.com` and emits audit records naming the person who asked; the token is never visible to the model, shell session, or component. Catalog, broker configuration, Cedar policy, credentials template, gateway configuration, and the deny table are pinned against the real machinery by `crates/dekopon-brokerd/tests/examples.rs`.

The GitHub reviewer walkthrough ships with its provider: [`examples/pr-summarizer-linter`](https://github.com/dekopon-agents/dekopon-provider-gh/blob/main/examples/pr-summarizer-linter/README.md) in `dekopon-provider-gh`.

## Catalog example

[`examples/catalog/dekopon.yaml`](examples/catalog/dekopon.yaml) is the library/gateway catalog fixture.
Run its loading and authority tests with `cargo test -p dekopon-config --test examples --locked`.

The `reviewer` may read pull requests and may propose a review comment only through the explicit `github.pull-request.comment` external-write capability. The `reviewer` also mounts one skill directory, `skills/pull-request-review`, whose `SKILL.md` name and description the model sees and whose body it reads on demand with `read_skill`. It has no approval capability, just like the end-to-end example above: approval is a separately named action rather than a stronger grade of “write.” This local file is catalog-only; the flagship example adds the broker policy, execution constraints, credential boundary, gateway route, and audit proof needed to make its comment real. The disabled `snooper` has one read-only repository capability.

See [`docs/cli.md`](docs/cli.md) for model-auth commands, formats, and exit codes.

## Providers and sandboxed scripts

Provider components execute only through the separate authorization broker. See the
[`provider SDK`](crates/dekopon-provider-sdk/README.md) for the guest interface and
[`development guide`](docs/development.md#provider-contract-or-host) for fixture builds.

[`dekopon-shell`](crates/dekopon-shell/README.md) provides the sandboxed bash-flavored
interpreter, structured JSON values, `jq`, text builtins, and independent bounds. The shared
agent layer offers it as the `bash` model tool: one script can express a multi-step plan, but
only the broker may authorize each proposed capability call.

[`examples/otel-traces`](examples/otel-traces/README.md) provides the OpenObserve receiver
and a real gateway/broker smoke test. Model inference and credential contracts are in
[`docs/inference.md`](docs/inference.md) and [`docs/chatgpt-credential.md`](docs/chatgpt-credential.md).

## Security model

Proposals carry untrusted intent; authorization, provider credentials, privileged host I/O, evidence, and audit records belong to a separate boundary ([constitution](docs/design.md#constitution)). Rust type visibility reinforces that distinction but never replaces process isolation, authentication, or policy enforcement. `dekopon-brokerd` establishes trusted context only from Unix peer credentials and an owner-controlled exact mapping; payloads cannot claim identity or authority. Its authorization decisions come from Cedar and its execution bounds from a separate owner-authored constraint catalog, so a policy edit can broaden who may act and can never widen how far an action reaches. The gateway never creates or receives authorized invocations; it submits untrusted proposals and receives public broker results.

Read [`docs/security-model.md`](docs/security-model.md) for trust assumptions and current limitations.

## Roadmap

Sequencing, the next milestones, and deferred scope live in [`docs/roadmap.md`](docs/roadmap.md); roadmap items are intentions, not shipped features.

## Maintainer release process

Releases separate reviewed preparation from automated publication:

1. Start from a clean, current `main`. Update release-facing status/install text in the root and crate READMEs before tagging—the packaged README is immutable on crates.io. Write a dated `[VERSION]` section in [`CHANGELOG.md`](CHANGELOG.md) from `git log v<PREVIOUS>..HEAD --format=%B | grep '^Changelog: '`, grouped by category, plus anything already under `[Unreleased]`, and leave an empty `[Unreleased]` heading; CI and the tag workflow reject a missing or empty release section. Run the full validation matrix in [`docs/development.md`](docs/development.md), including `cargo package --workspace --locked`.
2. Use `cargo release <VERSION>` to preview the shared-version commit and tag, then `cargo release <VERSION> --execute` after review. [`release.toml`](release.toml) creates the commit and tag but intentionally does not push or publish anything.
3. Let pull-request CI verify formatting, clippy, tests, rustdoc, package contents, dependency policy, the changelog, and the gateway privilege boundary before landing the version commit. CI does not repeat those expensive jobs on the resulting `main` commit. Push the matching `v<VERSION>` tag; that explicitly authorized tag is the single publication gate. The `Release` workflow checks the immutable tag against the shared workspace version, changelog, and publication plan, builds and attests three CLI archives, creates the GitHub release, publishes the container image, updates the Homebrew tap, and publishes every public crate in checked dependency order through a short-lived OIDC credential.
4. Ensure every public package has the crates.io GitHub trusted publisher `dekopon-agents/dekopon`, workflow `release.yml`, environment `crates-io`. The environment name is part of that OIDC identity but has no required-reviewer rule; approving the release tag is sufficient. A brand-new crate name must be bootstrapped with an explicitly authorized scoped credential, then registered immediately. If tag publication is interrupted, dispatch the same `Release` workflow with the existing tag and `publish_to_crates=true`; the recovery packages only the immutable tag, does not rebuild its other artifacts, and skips crate versions already present. An explicit crates.io new-package `429` waits until the server's retry time, while every other publication failure stops the job.
5. Verify the GitHub release, every crates.io package version, and fresh `cargo install --locked ... --version <VERSION>` commands before announcing the release.

The dependency-ordered list currently contains 21 publishable crates (including `dekopon-model-token-governor` and `dekopon-model-proxy`) in [`.github/release-crates.txt`](.github/release-crates.txt). Pull-request CI and release validation fail if that list omits a publishable workspace crate, includes a private/unknown crate, contains duplicates, or places a dependent before its dependency. Never move an existing tag or attempt to overwrite a published crate version; fix release automation on `main` and cut a new patch version when published bytes must change.

Chart releases use the independent `dekopon-chart-<VERSION>` namespace. Before creating that tag, bump `charts/dekopon/Chart.yaml`, write a dated `[dekopon-chart-<VERSION>]` section from the `Changelog:` lines of chart commits since the previous chart tag, and run the chart checks. Pull-request CI compares the chart version with the changelog, and `chart-publish.yml` repeats the check before it packages or pushes anything.

## Homebrew tap automation

Publishing a release also updates [`dekopon-agents/homebrew-tap`](https://github.com/dekopon-agents/homebrew-tap). [`.github/workflows/homebrew-tap.yml`](.github/workflows/homebrew-tap.yml) is a reusable workflow that [`release.yml`](.github/workflows/release.yml) calls as a job needing the one that publishes the release, and it also runs on manual dispatch against an existing tag. It renders `Formula/dekopon.rb` with [`.github/scripts/render-homebrew-formula.py`](.github/scripts/render-homebrew-formula.py) from the archives that release actually attached, taking each `sha256` from the published `.sha256` sidecar rather than recomputing it.

It derives platforms from the release rather than from a list held here, so adding a target needs no change to the tap. A target the generator cannot place in a Homebrew `on_macos`/`on_linux` block is a hard error rather than a silently dropped platform. The generator keeps two hand-maintained sets: `EXECUTABLES`, the binaries every archive carries, and `RETIRED`, holding targets a past release shipped that the tap must stop offering — currently `x86_64-apple-darwin`. Re-running the same release renders identical bytes and commits nothing; re-running an *older* release is refused rather than rolling the tap backwards; a release marked prerelease is skipped, because the tap tracks stable releases.

It is called rather than triggered by `release: published`, because that event cannot fire here: the release is created by `GITHUB_TOKEN`, and GitHub does not start workflow runs from events raised by its own token. The `needs` edge is what keeps it from racing the release the formula has to describe. It stays a separate workflow file rather than steps inside the release job because re-running only the tap update avoids repeating the whole build matrix. The release object exists before it starts, so a genuine tap failure reddens a run whose release was published — which is the accurate report, and is why the operator situations below skip instead of failing.

### The cross-repository credential

`GITHUB_TOKEN` is scoped to the repository running the workflow, so it cannot push to the tap. The workflow mints a short-lived installation token from a GitHub App instead. **This is one-time manual setup by a maintainer.** Until both secrets exist *and* the App is installed on the tap, the workflow logs a warning naming the missing half and skips; an unfinished credential setup never fails a release.

In the `dekopon-agents` organization, at **Settings → Developer settings → GitHub Apps → New GitHub App**:

1. Name it, for example, `dekopon-tap-updater`.
2. **Uncheck Webhook → Active.** The default is on, and a webhook with no listener is noise.
3. Under **Permissions → Repository permissions**, grant **Contents: Read and write**, and nothing else.
4. Create the app and note the numeric **App ID**.
5. **Generate a private key.** GitHub offers the `.pem` download exactly once; save it before leaving the page, and generate a replacement if it is lost.
6. **Install the app** — creating it grants nothing, and the two secrets below prove only that it exists. Choose **Install App → dekopon-agents → Only select repositories → `homebrew-tap`**. Skipping this makes the token mint `404`; the workflow treats that as the same unfinished setup as a missing secret and skips with a warning naming this step, rather than failing with an action stack trace.

Then add two repository secrets to `dekopon-agents/dekopon` under **Settings → Secrets and variables → Actions**: `TAP_APP_ID`, the numeric App ID, and `TAP_APP_PRIVATE_KEY`, the full `.pem` contents including the `-----BEGIN`/`-----END` lines.

An App rather than a personal access token: the minted token expires within the hour and carries one permission on one repository, so a leaked log leaks something already expiring; the credential belongs to the organization rather than to one maintainer, so it survives that person rotating their own; and it will not quietly expire a year later and break releases.

## Organization and package names

[`dekopon-agents`](https://github.com/dekopon-agents) is the GitHub organization that hosts the project. **Dekopon** is the product and Cargo workspace. The executables are `dekopond` and `dekopon-brokerd`. Organization naming does not change the product name.

## Contributing and license

See [`CONTRIBUTING.md`](CONTRIBUTING.md), [`SECURITY.md`](SECURITY.md), and the [Code of Conduct](CODE_OF_CONDUCT.md). Dekopon is dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
