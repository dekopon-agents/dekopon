# Dekopon

Dekopon is a capability-oriented control plane for self-hosted AI agents. **Version 0.9.0** pairs a declarative local agent catalog with a one-tool model runner, a JSON-native sandboxed scripting language, isolated WebAssembly providers, a separately deployed authorization broker, an unprivileged chat gateway, durable hash-linked audit, and correlated OpenTelemetry traces and logs.

> **Status:** this tree is a substantial, testable foundation, but it is not production-ready. `dekopon` manages the local catalog and model-account login. `dekopon-run` can call an operator-selected model, execute import-free read-only components, or submit identity-free proposals as an unprivileged broker client; it has no broker authority or provider credentials. The separate Unix-only `dekopon-brokerd` executable authenticates one owner-UID trust domain, evaluates a deny-by-default Cedar policy set against owner-authored execution constraints, resolves destination-bound provider credentials, invokes constrained providers, records durable audit, and can explicitly bind an unauthenticated GET-only operational web view. The Unix-only `dekopond` daemon connects to chat services and routes messages to catalog agents, holding chat and model credentials but no broker authority. The operator CLI is integrated with neither.

## Design documentation

Start with [`docs/design.md`](docs/design.md) for the product model, authority flow, component boundaries, and accepted decisions. [`docs/development.md`](docs/development.md) maps source, tests, generated artifacts, separate workspaces, and validation. [`docs/inference.md`](docs/inference.md) traces Slack model calls through prompt caching and bounded memory down to literal Rust and wire JSON. [`docs/README.md`](docs/README.md) provides task-based reading paths; repository-wide agent instructions live in [`AGENTS.md`](AGENTS.md). See [`CHANGELOG.md`](CHANGELOG.md) for the history of every application and chart tag.

## What works today in 0.9.0

- Strict YAML and JSON resources for agents, capabilities, and providers.
- Cross-reference validation with duplicate and unknown-field detection.
- A local, deterministic `dekopon` operator CLI with catalog commands, model-account authentication, and table, wide, JSON, YAML, and name output.
- Strongly typed identifiers and an invocation typestate that distinguishes proposals from broker authorization.
- A realistic local GitHub catalog with no embedded credentials.
- A Rust provider SDK plus a bounded Wasmtime component host with a fresh store per call.
- A published buffered `dekopon:http@1.0.0` contract, guest Rust facade, bounded native HTTP engine, asynchronous broker component host, deny-by-default authorization/evidence/audit core, and bounded identity-free Unix protocol.
- A separately deployed `dekopon-brokerd` that owns a private Unix socket, derives trusted context from peer UID mapping, restores replay state from verified durable audit, atomically checkpoints the count/head and rejects rollback relative to retained local state, and drains bounded connections on shutdown.
- A checked-in JSONPlaceholder broker provider with separately authorized post-read and external-write capabilities; all automated network tests use loopback mocks.
- `dekopon-run` direct invocation, an OpenAI-compatible or ChatGPT-subscription prompt loop offering a single sandboxed scripting tool, local Chrome traces, correlated OTLP/HTTP traces and audit-safe lifecycle logs, and explicit bounded broker capability/invocation client commands.
- An unprivileged chat gateway over Slack Socket Mode, Discord Gateway, Telegram long polling, and an owner-only local socket. Authenticated messages route to catalog agents while the broker remains the only authority.
- Opt-in native in-flight feedback after fresh authorization: Slack Agent Working/Stop sessions with
  a classic/free `:tangerine:` reaction fallback, Discord typing, and Telegram topic-aware chat
  actions. Activity failure never changes the answer, and Stop is cooperative rather than rollback.
- Unreleased: Slack Agent channel threads become owned per authenticated sender only after fresh
  authorization. That sender can continue without repeating the mention, while the optional
  `decline_chat_reply` decision lets the agent post nothing when a response would only take the
  last word. Ambient channel history never reaches routing or inference.
- A chat gateway that can be shown what a person attached: an image or a document becomes a numbered chat asset named in the prompt, which a model opens on demand rather than carrying on every turn. Discord photos and files follow the same bounded lazy path as Slack and Telegram.
- Explicit route-scoped image generation: an existing chat model may call one fixed-endpoint OpenAI Images meta tool, yielding one bounded PNG delivered natively to Slack, Discord, Telegram, or the local socket without entering conversation memory, telemetry, providers, or broker protocol.
- Credential-free agent self-inspection: an authorized gateway session can call `inspect_agent_config` to read its exact standing prompt, route limits, and current effective Cedar grants. Raw policy, identity, endpoints, paths, and every credential name or value stay out.
- A sandboxed bash-flavored script interpreter (`dekopon-shell`) whose command words dispatch to provider capabilities instead of operating-system processes. `dekopon-run shell` runs one script by hand and `dekopon-run prompt` hands the same interpreter to a model as its only tool, so a multi-step plan is one tool call rather than many round trips.
- Unreleased: generic broker-owned JSONL and durable-file provider storage plus optional on-demand
  durable chat memory. Memory is model-queryable only under an effective all-three grant and is
  recorded once after gateway-attested complete transport acceptance; it is never automatically
  replayed into a prompt.
- Unreleased: a text-only Meta WhatsApp Cloud API transport with exact raw-body HMAC verification,
  bounded process-local message-ID deduplication, `whatsapp.<wa_id>` subjects, one-attempt pinned
  Graph API replies, and an opt-in chart ClusterIP port for exact-path Traefik routing. It adds no
  broker authority and exposes no operational UI.

New in 0.9.0 — native chat activity without moving authority:

- **Slack Agent sessions.** An explicitly Agent-configured transport uses Slack's native Working/Stop lifecycle, setting `processing` only after fresh broker authorization and returning the session to `active` after the durable reply. Stop cooperatively prevents later model turns, capability calls, stale answers, and history commits; it cannot roll back work already in progress.
- **Compatible classic fallback.** Agent capability failures degrade to an opt-in fixed `:tangerine:` reaction and then no-op. Classic and Agent conversation semantics remain explicit, activity stays off by default, and no cosmetic failure changes or delays the answer.
- **Native leases elsewhere.** Discord typing and Telegram topic-aware chat actions renew only while authorized work is running. Targets and cadence come from authenticated transport state rather than model content.

New in 0.8.0 — Discord support without moving authority:

- **Outbound Gateway transport.** `dekopond` connects to Discord Gateway v10 without an inbound HTTP endpoint, accepts DMs and explicit structured guild mentions, treats native threads as channel identities, and filters bot, webhook, self-authored, and system messages before admission.
- **Resilient, quiet replies.** Heartbeat ACKs, dispatch sequence tracking, Resume, Invalid Session handling, session-start limits, fatal close-code handling, bounded 429 retry, and deduplication keep reconnects controlled. Replies split at Discord's 2,000-unit boundary and disable every user, role, `@everyone`, and reply mention.
- **Bounded photos and files.** Discord attachments reuse the lazy `Chat Asset #N` flow. Signed CDN URLs are host-validated and fetched without the bot token; expired URLs refresh through the exact source message and attachment ID. The existing media, byte, attempt, and conversation limits remain in force.

New in 0.7.0 — credential-free self-inspection without moving authority:

- **Ask the agent what it is.** Every authorized gateway session offers `inspect_agent_config`, which returns the agent identifier, description, model class, exact standing instructions, route/session bounds, conversation behavior, and the capabilities Cedar currently grants that sender through that agent.
- **Effective grants, not configuration access.** The view is built from the fresh `capabilitiesFor(subject, agent)` response the gateway already needs. Denied or merely declared capabilities are absent, as are raw Cedar, policy identifiers and digests, principal/subject/channel/transport identity, execution constraints, endpoints, paths, credential references, credential names, and credential values.
- **Bounded and non-authoritative.** Each result has a 128 KiB all-or-nothing ceiling and calls are repeatable under the prompt loop's shared per-turn tool and model-step bounds. Inspection spends no capability budget, makes no broker invocation, grants nothing, and creates no durable broker audit record.

New in 0.6.0 — operational visibility without moving authority:

- **An explicitly enabled web UI.** `dekopon-webui` is embedded in `dekopon-brokerd` behind `--http-bind`; without that flag there is no TCP listener. `/` redirects to `/ui`, and the unauthenticated surface accepts only GET and HEAD. The network around the selected bind address is the access boundary.
- **Provider documentation from the loaded components.** The index and rustdoc-like detail pages show validated manifests, capability schemas and command words, plus the local artifact path, byte length, SHA-256 digest, and Wasmtime-visible component imports and exports.
- **Process-local runtime visibility.** Host-observed compilation, store, instantiation, invocation, fuel, resource-limiter, and HTTP counters appear beside every configured host ceiling. Credential-free OTLP endpoint, transport, service, timeout, and payload settings are shown without header or resource-attribute values.
- **Content-free gateway reporting.** `dekopond` best-effort reports a bounded normalized agent inventory and provider-reported model usage. Those summaries reset with the broker and never feed identity, Cedar policy, constraints, credentials, execution, evidence, replay, or durable audit.

New in 0.5.0 — chat that can see what you sent it. One documented invariant was deliberately rewritten to get there, and it is the only thing in this release that moved:

- **Files in chat.** A message carrying a screenshot used to be dropped before it was routed, because Slack stamps `subtype: file_share` on any upload. It routes now, and each attachment becomes a numbered chat asset named in the prompt — `Chat Asset #1 — screenshot.png (image/png, 214 KB)` — that a model opens with a `fetch_chat_asset` tool. Pull rather than push: bytes cost tokens on every turn they appear in, most turns do not need them, and one base64 screenshot is larger than a conversation's entire history budget. The audit log records `agent.asset.fetched`, so "did it actually look" is a question with an answer. Images and documents, over Slack and Telegram. See [`docs/dekopond.md`](docs/dekopond.md#chat-assets).
- **The gateway fetches attachment bytes**, which three documents previously recorded as a deliberate refusal. The argument replacing it: an attachment is part of the message that carried it, and a chat service delivers it by reference rather than by value, so resolving that reference is how the gateway hears the whole request — with the bot token the daemon already holds. No policy, no provider credential, no authorization path, no write. What bounds it is arithmetic rather than authority: a media-type allowlist, 8 MiB per attachment enforced while the response streams rather than after it, four fetches per session, and a per-conversation ceiling on how many stay addressable. [`docs/security-model.md`](docs/security-model.md) records the change and the reasoning.
- **Slack answers render.** A model writes CommonMark; Slack's `text` field is mrkdwn, so every answer arrived with its formatting as literal punctuation. Answers post in a Block Kit `markdown` block, which Slack renders itself — and which carries tables and task lists that mrkdwn cannot express at all.
- **Providers bring their own command words.** A provider declares the words its capabilities answer to, and those words cross the local protocol instead of being fixed by the shell.
- **A broker loads providers from a directory** rather than an enumerated list, and policy tolerates names no loaded provider declares, so adding a provider is one change rather than two.
- **`dekopon-model` carries images and documents.** A message is text unless it is built with parts, and a text message serializes to exactly the bytes it did before. The public `Serialize` is now the redacted audit rendering rather than the chat-completions wire shape — the two were one type, which put a base64 attachment one careless `to_string` from the audit log.

## What does not work yet

Automatic memory replay, semantic/vector memory, cross-agent sharing, task memory, deletion/export UX,
SQLite, and encryption-at-rest claims do not exist. Optional durable chat turns carry across broker
and gateway restarts only inside one provider/agent/sender/transport/channel/conversation scope and
are retrieved on demand with `memory recent` or `memory search`. JSONL deduplication is permanent
but finite; recording stops with `dedup-capacity` while reads continue. `dekopond` also runs under
the same UID as the broker, so its attestor grant buys attribution and deny-by-default scoping
rather than isolation; a dedicated gateway UID remains committed direction.

There is still no independently retained/signed/remote audit checkpoint service and no operator-CLI integration with the broker or the daemon — `dekopon` reads the catalog and nothing else. Catalog provider and status resources remain declarations only. The immediate `dekopon-run` host exposes no WASI or custom imports and rejects every mutating capability, so it cannot read GitHub or post the review comment represented by the catalog example; only the broker can.

## Install

### Homebrew (macOS and Linux)

```console
brew tap dekopon-agents/tap
brew trust dekopon-agents/tap
brew install dekopon
```

That installs **all four** executables — `dekopon`, `dekopon-run`, `dekopon-brokerd`, and `dekopond` — plus the example JSONPlaceholder provider component, so one machine can run the broker and the gateway and actually exercise the authority boundary rather than only read the catalog. `brew install` prints where `BROKER.md`, `GATEWAY.md`, and the component landed. `brew trust` is not optional: Homebrew 6 refuses to load a formula from a non-official tap until you trust it.

The tap is [`dekopon-agents/homebrew-tap`](https://github.com/dekopon-agents/homebrew-tap), and its formula is regenerated from the archives each release actually publishes rather than from a platform list maintained by hand. It covers **macOS on ARM64, and Linux on ARM64 and x86-64**.

Not Intel Macs. `0.3.0` did ship an `x86_64-apple-darwin` archive, but [#74](https://github.com/dekopon-agents/dekopon/pull/74) removed that target from the release matrix, so `0.4.0` onward has none. Offering the `0.3.0` archive would install cleanly on an Intel Mac and then dead-end at the next `brew upgrade`, which is worse than being plainly unsupported. Download the [v0.3.0 archive](https://github.com/dekopon-agents/dekopon/releases/tag/v0.3.0) directly or build from a checkout instead.

From there, [`examples/pr-summarizer-linter`](examples/pr-summarizer-linter/README.md) is the next step: it is the only walkthrough that puts the gateway, broker, policy, and a credential-holding provider to work together.

### Prebuilt archives

Three provenance-attested archives — macOS on ARM64, and Linux on ARM64 and x86-64 — are attached to the [v0.9.0 GitHub release](https://github.com/dekopon-agents/dekopon/releases/tag/v0.9.0). Each carries all four executables, the example component, and the broker and gateway configuration contracts, with a `.sha256` sidecar beside it:

```console
gh release download v0.9.0 --repo dekopon-agents/dekopon \
  --pattern 'dekopon-0.9.0-aarch64-apple-darwin.tar.gz*'
shasum -a 256 -c dekopon-0.9.0-aarch64-apple-darwin.tar.gz.sha256
gh attestation verify --repo dekopon-agents/dekopon \
  dekopon-0.9.0-aarch64-apple-darwin.tar.gz
tar xzf dekopon-0.9.0-aarch64-apple-darwin.tar.gz
```

### crates.io

The unreleased workspace contains twenty-three public crates; the published `0.9.0` release contains twenty-one. Each application release tag publishes that version's packages in checked dependency order through crates.io trusted publishing:

```console
cargo install --locked --version 0.9.0 dekopon
cargo install --locked --version 0.9.0 dekopon-run
cargo install --locked --version 0.9.0 dekopon-brokerd
cargo install --locked --version 0.9.0 dekopond
```

`0.3.0` was never published and is being left that way — its tag and GitHub release exist, but no crate carries that version. `dekopon` additionally carries `0.1.0` and `0.2.0` from before the workspace was split.

Crates.io publication is part of the release-tag workflow (see [Maintainer release process](#maintainer-release-process)). A manual dispatch remains available only to recover an interrupted tag publication; already-published immutable versions are skipped.

### From a checkout

With stable Rust (MSRV 1.89.0, edition 2024):

```console
git clone https://github.com/dekopon-agents/dekopon.git
cd dekopon
cargo install --locked --path crates/dekopon
cargo install --locked --path crates/dekopon-run
cargo install --locked --path crates/dekopon-brokerd
cargo install --locked --path crates/dekopond
dekopon version
dekopon-run --version
```

### Container image

A multi-architecture container image publishes to `ghcr.io/dekopon-agents/dekopon` when a release is published. `v0.4.0` is the first release [`.github/workflows/container-image.yml`](.github/workflows/container-image.yml) runs for; `v0.3.0` predates the workflow and has no image. It carries the executables from the archives above, byte for byte, rather than a separately compiled set, alongside the checked-in provider components. It runs as UID 65532 and lets the command select the binary. Read [`docs/container-image.md`](docs/container-image.md) before deploying it: the broker refuses to start unless its runtime directories are owned by that UID and mode `0700`.

### Before running the broker

`dekopon-brokerd` requires an owner-controlled strict configuration, private socket/audit/checkpoint directories, and pinned provider component paths:

```console
dekopon-brokerd --config /path/to/broker.yaml
# Optional unauthenticated informational UI:
dekopon-brokerd --config /path/to/broker.yaml --http-bind=0.0.0.0:8080
```

See [`crates/dekopon-brokerd/README.md`](crates/dekopon-brokerd/README.md) before enabling this privileged process. Direct `inspect`, `invoke`, and `prompt` never connect to it; only explicit `dekopon-run broker ...` commands do.

For Kubernetes, [`charts/dekopon`](charts/dekopon/README.md) runs both daemons as one pod sharing the broker socket. It is published to `oci://ghcr.io/dekopon-agents/charts/dekopon` on `dekopon-chart-*` tags, a namespace deliberately separate from the `v*.*.*` tags that publish crates, archives, and the container image, so a chart fix ships without an application release. Published chart `0.1.0` retains `appVersion: 0.4.0` and is immutable; the working tree now declares `appVersion: 0.9.0`, so the next `dekopon-chart-*` release deploys `v0.9.0` by default. Until then, or to run any other release, deployments select it through the chart's `image.tag` or `image.digest` value.

## Run the flagship example

[`examples/pr-summarizer-linter`](examples/pr-summarizer-linter/README.md) is the whole system in one deployment: a maintainer asks in Slack for a pull-request summary and lint review, the gateway attests to the sender and decides nothing, and the broker authorizes six narrow GitHub capabilities before posting one head-pinned `COMMENT` review. Approval, request-changes, and merge are absent. The broker injects a token bound to `api.github.com` and hash-links audit records naming the person who asked; the token is never visible to the model, shell session, or component. Catalog, broker configuration, Cedar policy, credentials template, gateway configuration, and the deny table are pinned against the real machinery by `crates/dekopon-brokerd/tests/examples.rs`.

## Run the catalog example

```console
dekopon --config examples/local/dekopon.yaml get agents
dekopon --config examples/local/dekopon.yaml get agents -o wide
dekopon --config examples/local/dekopon.yaml get agent reviewer -o yaml
dekopon --config examples/local/dekopon.yaml get capabilities -o name
dekopon --config examples/local/dekopon.yaml get providers
dekopon --config examples/local/dekopon.yaml describe agent reviewer
dekopon --config examples/local/dekopon.yaml validate
dekopon --config examples/local/dekopon.yaml config view -o json
```

The `reviewer` may read pull requests and may propose a review comment only through the explicit `github.pull-request.comment` external-write capability. It has no approval capability, just like the end-to-end example above: approval is a separately named action rather than a stronger grade of “write.” This local file is catalog-only; the flagship example adds the broker policy, execution constraints, credential boundary, gateway route, and audit proof needed to make its comment real. The disabled `snooper` has one read-only repository capability.

See [`docs/cli.md`](docs/cli.md) for discovery precedence, formats, and exit codes.

## Run a Rust provider immediately

The checked-in component is generated from [`examples/providers/echo/src/lib.rs`](examples/providers/echo/src/lib.rs), which implements `dekopon_provider_sdk::Provider`:

```console
cargo run -p dekopon-run -- inspect \
  --provider examples/providers/echo-provider.wasm
cargo run -p dekopon-run -- invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.echo --input '{"message":"hello"}'
cargo run -p dekopon-run -- invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.ransom-case --input '{"message":"Hello, World!"}'
cargo run -p dekopon-run -- --trace trace.json invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.echo --input '{}'
OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:5080/api/default \
OTEL_EXPORTER_OTLP_HEADERS='Authorization=Basic%20<INGESTION_TOKEN>,organization=default,stream-name=dekopon' \
  cargo run -p dekopon-run -- invoke \
    --provider examples/providers/echo-provider.wasm \
    echo.echo --input '{}'
```

[`examples/otel-traces`](examples/otel-traces/README.md) provides a one-container OpenObserve receiver, UI walkthrough, and automated smoke test.

Prompt mode targets an OpenAI-compatible endpoint (defaulting to local Ollama at `http://127.0.0.1:11434/v1`) or uses the isolated ChatGPT/Codex device login managed by `dekopon auth chatgpt`. See [`docs/run.md`](docs/run.md) for subscription login, provider builds, prompt usage, limits, benchmarking, and authority restrictions.

## Script several capability calls as one plan

[`crates/dekopon-shell`](crates/dekopon-shell/README.md) is a sandboxed bash-flavored interpreter whose command words are capability invocations rather than operating-system processes, with `jq` and the usual text builtins alongside them. `dekopon-run shell` runs one script by hand:

```console
cargo run -p dekopon-run -- shell \
  --provider examples/providers/echo-provider.wasm \
  'echo.echo --message hi | jq -r .message'
```

Every variable is a JSON value, every bound is hand-built and configurable, and every dropped bash construct either fails by name or is documented as inert.

`dekopon-run prompt` hands that same interpreter to a model as its **only** tool, so one tool call carries a whole multi-step plan instead of one capability per turn, and the tool surface stays a single schema however many capabilities an operator grants. Adding `--broker` lets those scripts reach capabilities direct mode provably cannot — anything performing I/O — while the broker remains the sole authority over them.

## Security model

A model may propose an invocation, but only the broker may turn it into an authorized invocation. Proposals carry untrusted intent; authorization, provider credentials, privileged host I/O, evidence, and audit records belong to a separate boundary. Rust type visibility reinforces this distinction but never replaces process isolation, authentication, or policy enforcement. `dekopon-brokerd` establishes that context only from Unix peer credentials and an owner-controlled exact mapping; payloads cannot claim identity or authority. Its authorization decisions come from Cedar and its execution bounds from a separate owner-authored constraint catalog, so a policy edit can broaden who may act and can never widen how far an action reaches. `dekopon-run` never creates or receives authorized invocations: direct mode executes only import-free components declaring `read-only`, while broker mode submits untrusted proposals and prints broker results.

Read [`docs/security-model.md`](docs/security-model.md) for trust assumptions and current limitations.

## Roadmap

The next architectural milestones are independent checkpoint retention or signing, operator-CLI integration with the broker and the daemon, a dedicated gateway UID, and memory lifecycle UX (deletion/export) beyond the current optional on-demand durable chat-turn store. Broker-owned credentials, Cedar, identity/attestation, the unprivileged `dekopond`, and its bounded per-sender conversation history shipped in 0.3.0; 0.4.0 added distribution rather than authority. See [`docs/roadmap.md`](docs/roadmap.md); roadmap items are intentions, not shipped features.

## Maintainer release process

Releases deliberately separate reviewed preparation from automated publication:

1. Start from a clean, current `main`. Update release-facing status/install text in the root and crate READMEs before tagging—the packaged README is immutable on crates.io. Move the completed [`CHANGELOG.md`](CHANGELOG.md) entries from `[Unreleased]` into a dated `[VERSION]` section and leave an `[Unreleased]` heading for later work; CI and the tag workflow reject a missing or empty release section. Run the full validation matrix in [`docs/development.md`](docs/development.md), including `cargo package --workspace --locked`.
2. Use `cargo release <VERSION>` to preview the shared-version commit and tag, then `cargo release <VERSION> --execute` after review. [`release.toml`](release.toml) creates the commit and tag but intentionally does not push or publish anything.
3. Let pull-request CI verify formatting, clippy, tests, rustdoc, package contents, dependency policy, the changelog, and the runner privilege boundary before landing the version commit. CI does not repeat those expensive jobs on the resulting `main` commit. Push the matching `v<VERSION>` tag; that explicitly authorized tag is the single publication gate. The `Release` workflow checks the immutable tag against the shared workspace version, changelog, and publication plan, builds and attests three CLI archives, creates the GitHub release, publishes the container image, updates the Homebrew tap, and publishes every public crate in checked dependency order through a short-lived OIDC credential.
4. Ensure every public package has the crates.io GitHub trusted publisher `dekopon-agents/dekopon`, workflow `release.yml`, environment `crates-io`. The environment name is part of that OIDC identity but has no required-reviewer rule; approving the release tag is sufficient. A brand-new crate name must still be bootstrapped with an explicitly authorized scoped credential, then registered immediately. If tag publication is interrupted, dispatch the same `Release` workflow with the existing tag and `publish_to_crates=true`; the recovery packages only the immutable tag, does not rebuild its other artifacts, and skips crate versions already present. An explicit crates.io new-package `429` waits until the server's retry time, while every other publication failure stops the job.
5. Verify the GitHub release, every crates.io package version, and fresh `cargo install --locked ... --version <VERSION>` commands before announcing the release.

The dependency-ordered crate list lives in [`.github/release-crates.txt`](.github/release-crates.txt). Pull-request CI and release validation fail if that list omits a publishable workspace crate, includes a private/unknown crate, contains duplicates, or places a dependent before its dependency. Never move an existing tag or attempt to overwrite a published crate version; fix release automation on `main` and cut a new patch version when published bytes must change.

Chart releases use the independent `dekopon-chart-<VERSION>` namespace. Before creating that tag, bump `charts/dekopon/Chart.yaml`, move the chart's completed changelog bullets into a dated `[dekopon-chart-<VERSION>]` section, and run the chart checks. Pull-request CI compares the chart version with the changelog, and `chart-publish.yml` repeats the check before it packages or pushes anything.

## Homebrew tap automation

Publishing a release also updates [`dekopon-agents/homebrew-tap`](https://github.com/dekopon-agents/homebrew-tap). [`.github/workflows/homebrew-tap.yml`](.github/workflows/homebrew-tap.yml) is a reusable workflow that [`release.yml`](.github/workflows/release.yml) calls as a job needing the one that publishes the release, and it also runs on manual dispatch against an existing tag. It renders `Formula/dekopon.rb` with [`.github/scripts/render-homebrew-formula.py`](.github/scripts/render-homebrew-formula.py) from the archives that release actually attached, taking each `sha256` from the published `.sha256` sidecar rather than recomputing it.

It derives platforms from the release rather than from a list held here, so adding a target needs no change to the tap. A target the generator cannot place in a Homebrew `on_macos`/`on_linux` block is a hard error rather than a silently dropped platform. The one hand-maintained entry is the generator's `RETIRED` set, holding targets a past release shipped that the tap must stop offering — currently `x86_64-apple-darwin`, dropped from the matrix in [#74](https://github.com/dekopon-agents/dekopon/pull/74). Re-running the same release renders identical bytes and commits nothing; re-running an *older* release is refused rather than rolling the tap backwards; a release marked prerelease is skipped, because the tap tracks stable releases.

It is called rather than triggered by `release: published`, because that event cannot fire here: the release is created by `GITHUB_TOKEN`, and GitHub does not start workflow runs from events raised by its own token. The `needs` edge is what keeps it from racing the release the formula has to describe. It stays a separate workflow file rather than steps inside the release job because re-running only the tap update avoids repeating the whole build matrix. The release object exists before it starts, so a genuine tap failure now reddens a run whose release was published — which is the accurate report, and is why the operator situations below skip instead of failing.

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

[`dekopon-agents`](https://github.com/dekopon-agents) is the GitHub organization that hosts the project. **Dekopon** is the product, `dekopon` is the CLI binary and Cargo workspace, and `dekopon` is the intended primary crates.io package. Organization naming does not change the product or package name.

## Contributing and license

See [`CONTRIBUTING.md`](CONTRIBUTING.md), [`SECURITY.md`](SECURITY.md), and the [Code of Conduct](CODE_OF_CONDUCT.md). Dekopon is dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
