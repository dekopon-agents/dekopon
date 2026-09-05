# Upgrading Dekopon

**Status: current.** This is the operator's companion to [`CHANGELOG.md`](../CHANGELOG.md): what to
edit, in what order to restart, and which releases require a configuration change rather than a
binary swap. The changelog records *what changed*; this records *what you have to do about it*.

Dekopon is pre-1.0 and the local broker protocol is `v1alpha3`. There is no compatibility promise
across minor releases, and no automatic migration: the daemons refuse to start on configuration they
do not understand rather than guessing.

## Two rules that apply to every upgrade

### Upgrade all four executables together

`dekopon`, `dekopon-run`, `dekopon-brokerd`, and `dekopond` are separately installable — Homebrew,
crates.io, release archives, the container image, and the Helm chart with its own `image.tag` — so a
mixed set is easy to end up with by accident. Do not. The local broker protocol has one version
constant and both envelopes are strict-decoded; a newer broker adding a field to a response an older
client already understands makes that response undecodable, which is the failure a partial upgrade
most reliably produces. [`broker-http.md`](broker-http.md#version-and-compatibility) has the exact
mechanics. The container image and the chart ship all four from one release for this reason.

### Restart the broker first and stop it last

`dekopond` asks the broker for capabilities once at startup and **exits non-zero** if the broker does
not answer, so a gateway started against a stopped broker crash-loops rather than waiting. Shutdown
runs the other way: the gateway drains first so no session is mid-invocation when the broker begins
synchronizing its audit chain.

Dekopon ships no service units, so the order is yours to enforce whatever supervises the processes:

1. Stop `dekopond`.
2. Signal `dekopon-brokerd` with `SIGINT` or `SIGTERM` and let it finish. It stops accepting, drains
   bounded in-flight connections, synchronizes the audit and checkpoint appends, logs the verified
   chain head, and removes only the socket inode it created.
3. Replace the binaries and make any configuration edits the release notes below call for.
4. Start `dekopon-brokerd` and wait for it to be answering on its socket.
5. Start `dekopond`.

Under the Helm chart this ordering is structural rather than procedural: the broker is a native
sidecar with a startup probe, so Kubernetes will not start `dekopond` until the broker answers a real
request, and terminates them in the reverse order.

**Never move the audit chain or its checkpoint as part of an upgrade.** Startup requires the
checkpoint to be an exact verified prefix of the audit file, and a mismatch fails closed and needs
explicit operator recovery. See [`operations.md`](operations.md#the-audit-chain-and-its-checkpoint).

## Release-by-release

Only releases that need an operator action appear here. A release absent from this list is a binary
swap in the order above.

### 0.12.0 → next (unreleased) — harness APIs and accounting

Rust embedders replace the `dekopon-agent` dependency/imports with `dekopon-harness`, then construct
`SessionEngine`, `SessionBootstrap` and harness history. There is no compatibility facade or old
`prompt` entrypoint. Out-of-tree console migration and new-name crates.io bootstrap have not run.
`ChatModel`/image adapters require an `AttemptRecorder`; record before content decoding and include
failed/cancelled inference and HTTP retry attempts, never fabricate missing usage. Retain
`JobAccounting` until host delivery is known; generated output alone is not accepted delivery.

Dashboards migrate from `accounting.model.turn`/separate image accounting to the versioned
`accounting.model.call`, `accounting.model.transition` and `accounting.model.job` schemas; choose
one aggregation level, never sum all three. Informational `ModelUsageReport` now derives solely
from tracker attempt observations, not a success-only observer. Unknown totals display unknown.
New transcript events identify context revision/full versus delta; the reader currently refuses
later full rebuilds. Checkpoints are version 2 process-local memory, not on-disk upgrade state.
See [the runtime contract and remaining integration gaps](harness.md).

**Every `models[].name` in `dekopond.yaml` must now be a configured-model identifier**:
`[a-z0-9][a-z0-9._-]{0,63}`, so lowercase, starting with a letter or digit, at most 64 bytes.
A 0.12.0 file naming a model `GPT-5`, `Local Qwen` or `_scratch` no longer starts, and the grammar
applies whether or not the deployment configures `controls:` — the name is the model's configured
identity everywhere it is used, not a controls field. Rename the model and every `routes[].model`
and `controls.models` entry that points at it in the same edit; the name is a local alias, so
renaming it changes no endpoint and no credential. The refusal names `models[].name` and the
offending value, and every offending name is reported in one startup failure.

`sessions.maxConcurrent` is now validated against the harness checkpoint store's lease ceiling
(`dekopon_harness::checkpoint::MAX_JOBS`, 128). A configuration asking for more sessions than the
store admits leases is refused at startup instead of turning the surplus into capacity failures
under load; the refusal names the field, the value and the constant.

### 0.12.0 → next (unreleased) — core controls and `v1alpha3`

Upgrade all four executables together. `authorizeControl` and the required host-only
`surfaceEpoch` change the protocol to `dekopon.dev/broker/v1alpha3`; `v1alpha1`/`v1alpha2`
envelopes refuse before dispatch. An older client cannot decode the newer refusal either.
Stop gateway, drain broker, retain audit and checkpoint, install the binaries, start broker,
then gateway. Never erase replay history to make a mixed installation start: older binaries
cannot decode the new `ControlDecision` audit variant. Existing event hashes remain valid.

Controls are disabled without `controlTargets`. Opt-in requires separate `agent.prompt`,
`agent.model.select` and/or `agent.effort.set` permits, and explicit attestor `chatScopes` for
chat controls. No subject-only scope fallback applies. Both changed dimensions require both
permissions. Configured model aliases are not endpoints and allowlisting them grants nothing.

`policy_digest` now hashes the two reserved control actions (`agent.model.select`,
`agent.effort.set`) unconditionally, so every deployment's digest changes on upgrade even when the
policy set is byte-identical. Audit records written either side of the upgrade carry different
digests for the same policy; that is expected and is not evidence of a policy change.

### 0.12.0 → next (unreleased) — command words run over `runCommand`

Not yet released; the version that carries it is named when it is cut. Nothing here needs a
configuration edit.

- **Upgrade the broker before its clients, and all four executables together.** The local protocol
  now uses `dekopon.dev/broker/v1alpha3`, and `dekopon-run --broker` and `dekopond` now send a provider
  command word as `runCommand` — the word, its argv, and the optional piped value — and read back
  the guest's own outcome. A newer broker still answers the legacy `resolveCommand`, with a
  rendered page degraded to a decline carrying its stdout then stderr. This is a legacy operation
  within the new envelope, not cross-version compatibility: `v1alpha2` clients are rejected before
  dispatch and must upgrade in lockstep.
- **Upgrade the hosts before a provider adopts `run-command`.** A component built against
  `dekopon:provider@0.3.0`'s `provider-cli` world exports `run-command`, which only a broker or
  runner at this version looks up; an older host finds no `resolve-command` behind the manifest's
  `commandWords` and refuses the component at load. Components built against `0.1.0` or `0.2.0`
  keep loading unchanged and never receive a piped value.
- **Embedders: `BrokerClient::resolve_command`, `RequestEnvelope::resolve_command`, and
  `Broker::resolve_command` are gone.** Call `BrokerClient::run_command` and `Broker::run_command`
  and match the `CommandRunOutcome` they return; `BrokerRequest::ResolveCommand` remains a request
  the broker answers, not one the client builds.

### 0.11.1 → 0.12.0 — optional public DRNs require a private map and second policy

Existing `credentialsPath`, `credential`, and `credentialByAgent` deployments need no migration and
retain byte-compatible legacy audit serialization. To opt into model-selected DRNs:

1. Install an owner-only `0600` `dekopon.dev/secret-map/v1alpha1` file and set `secretMapPath`.
2. Keep every binding narrower than the named capability constraint set.
3. Add a separate Cedar permit for `Dekopon::Action::"secret.use"` over each exact
   `Dekopon::Secret::"drn:…"`; a capability permit alone intentionally returns `secret-denied`.
4. Upgrade all clients with the broker. `InvocationRequest.secretUse` is optional and omitted from
   old calls, but an older strict broker rejects a new request carrying it.
5. Mount bootstrap/session files only into the broker. No source credential or private map belongs
   in `dekopond` or direct `dekopon-run`.

Startup validates descriptors without network. Remote source availability is first exercised after
an authorized invocation. See [`secrets.md`](secrets.md) for exact source fields and current
bootstrap limitations.

### 0.11.1 → 0.12.0 — the structural scrub

- **Delete `allowDevelopmentSubjects` from `broker.yaml` before upgrading the broker.** The field is
  gone, and `broker.yaml` rejects unknown fields, so leaving it is a startup failure rather than a
  value quietly ignored. Delete every `dev.*` `identityMappings` subject and attestor namespace with
  it: `dev` is no longer a subject service, so those lines no longer parse either. The field was off
  by default and no chart release could set it, so a deployment that never opted in has nothing to
  edit — and no persisted audit chain can carry a `dev.*` subject.
- **Declare `route:` on every chat-memory constraint set before upgrading the broker.** Durable chat
  memory used to be recognized by name: any capability spelled `memory.chat.*` and any provider
  called `memory-chat` was reserved, and renaming the shipped provider silently dropped that
  reservation. It is now the owner's declaration. Add `route: chatMemoryRecord`,
  `route: chatMemoryRecent`, and `route: chatMemorySearch` to the three `constraintSets` entries
  that make up the surface — exactly one set per role, all naming one provider, each already
  declaring `jsonl` chat storage at its role's access. Without them the sets are ordinary
  capabilities, `chatMemory` refuses to compose, and the broker fails to start rather than serving a
  memory surface nothing reserves. That refusal names the work: it lists every role no constraint
  set declares `route:` for, names all three, and says exactly one set must declare each. Startup
  reports every route conflict at once for the same reason. Deployments with no chat memory have
  nothing to edit: `route:` is optional, defaults to `generic`, and a set that
  omits it means exactly what it meant before. The wire protocol and audit record shapes are
  unchanged.
- **The local broker protocol moved to `dekopon.dev/broker/v1alpha2`; upgrade all four executables
  in one step.** The eleven request operations collapsed into six — `capabilities`,
  `resolveCommand`, `invoke`, `recordDeliveredTurn`, `publishAgentInventory`, `publishModelUsage` —
  because whether a caller speaks as its own peer, on behalf of a subject, or inside a chat scope is
  now an optional `attestation` field rather than a separate operation per shape. The retired tags
  (`capabilitiesFor`, `capabilitiesForChat`, `invokeFor`, `invokeForChat`, `resolveCommandForChat`,
  `recordDeliveredTurnForChat`) are gone rather than aliased: an alias would have had to carry the
  old field shapes too, and a mixed pair would then half-work. There is nothing to edit — no
  configuration file, policy, catalog, or audit record shape changes, and no persisted state is
  touched — but a mixed set of binaries now fails at the **envelope, in both directions**: the
  broker answers an `apiVersion` it does not know with `invalid-request` on the first request frame,
  before anything is authorized, accounted, or audited. A client never emits that code. An older
  client against a newer broker therefore fails on the *response* frame, cannot decode the refusal,
  and reports the outcome as unknown — but under `v1alpha1` that same failure came after the
  request had been decoded and run, so the proposal really had an unknown outcome; now nothing ran.
  That half is what this closes. The restart order in
  [Restart the broker first and stop it last](#restart-the-broker-first-and-stop-it-last) is
  unchanged and is what keeps the window shut: stop `dekopond`, drain and stop `dekopon-brokerd`,
  replace **all four** binaries, start the broker, then start the gateway. Do not roll one process
  at a time.

  A fifth broker client lives outside this repository:
  [dekopon-console](https://github.com/dekopon-agents/dekopon-console) pins
  `dekopon-agent = "=0.11.1"` and `dekopon-broker-protocol = "=0.11.1"`, so it still speaks
  `v1alpha1` and cannot talk to a broker built from this tree. It does not merely need a version
  bump: it calls `BrokerLeg::connect_attested`, which no longer exists, so moving its pin past
  0.11.1 is a source change to `crates/dekopon-tui/src/session.rs`. Leave the pin where it is until
  that lands, and do not run the console against an upgraded broker.
- **The interactive console left this repository.** `dekopon console` and the `dekopon-tui` crate
  now ship from [dekopon-console](https://github.com/dekopon-agents/dekopon-console), the way the
  `gh` provider did. `dekopon` is a local catalog and model-account CLI again, and a bare `dekopon`
  is the usage error it was before 0.11.0 rather than a full-screen view. Nothing loses authority:
  the console never held any.
- **Export every `apiKeyEnv` a bound route can reach before starting `dekopond`.** A model's
  `apiKeyEnv` naming a variable that is unset, exported blank, or not UTF-8 is now a startup
  refusal naming the model and the variable, never the value. It used to become a tokenless
  client that answered every message with a 401, and because the gateway builds one client per
  model and keeps it, exporting the key afterwards needed a restart anyway. Startup now resolves
  the credential of every model a bound route can reach, beside the image generator's, before any
  transport authenticates, and reports every unusable one at once. Nothing else changes: leaving
  `apiKeyEnv` out still means the endpoint needs no key, a configured model no route reaches has
  its variable left unread, and `dekopon-run` is unchanged — an unset or blank `--api-key-env`
  variable still means no bearer token. See [`dekopond.md`](dekopond.md#startup-fails-closed).
- **Model clients no longer follow an ambient `HTTPS_PROXY` or `ALL_PROXY`.** Every
  `dekopon-model` transport — the OpenAI-compatible chat client, the ChatGPT subscription client
  and its device-flow login, and the Images client — is built from one agent that sets no proxy
  and follows no redirect, so an exported proxy variable no longer carries a bearer token, the
  device-code exchange, or a prompt through a host nobody named to Dekopon. That is the stance
  `dekopon-http-host` already took for provider HTTP. It reaches `dekopond`, `dekopon-run`, and
  `dekopon auth chatgpt login`; nothing in a configuration file changes, and there is no field or
  flag to opt back in, so a model endpoint that was only reachable through that proxy is
  unreachable after the upgrade.

#### `imageGenerators:` becomes one `imageGenerator:` block

A gateway configures at most one image generator, so the named list is now a single optional object
and a route opts in with a flag instead of a name. `deny_unknown_fields` means the old shape does not
decode: `dekopond` refuses to start rather than ignoring the block. In `dekopond.yaml`:

```yaml
# before
imageGenerators:
  - name: openai-images
    kind: openaiImages
    model: gpt-image-1
    apiKeyEnv: OPENAI_IMAGE_API_KEY
    timeoutMs: 120000
routes:
  - transport: workspace-slack
    match: { kind: directMessage }
    agent: reviewer
    imageGenerator: openai-images

# after
imageGenerator:
  model: gpt-image-1
  apiKeyEnv: OPENAI_IMAGE_API_KEY
  timeoutMs: 120000
routes:
  - transport: workspace-slack
    match: { kind: directMessage }
    agent: reviewer
    imageGenerator: true
```

Drop `name:` and `kind:` — the endpoint was already fixed to OpenAI's public Images API and the name
had one referent. Deployments that configured more than one generator keep the one their routes
actually named. Nothing else changes: the credential is still an environment variable name read only
when a route opts in, and pairing an opted-in route with a `whatsappCloudApi` transport is still a
startup refusal.

### 0.2 → 0.3 — the broker configuration is a breaking migration

**This is the one upgrade that silently looks fine and is not.** `broker.yaml` keeps its
`dekopon.dev/brokerd/v1alpha1` API version across the change, so the version string tells you
nothing. What tells you is that the file is `deny_unknown_fields`: a `rules:` key that survives the
upgrade is a startup failure naming the unknown field, not a silently ignored section.

`rules` is replaced by two independent things. Exact matching became Cedar, and each rule split
along the line the new design draws — *who may act* moved into a policy file, *how narrowly the
broker then acts* became a constraint set keyed by capability.

Before:

```yaml
rules:
  - principal: local-user
    actor:
      type: human
      principal: local-user
    capability: echo.echo
    provider: echo
    effect: read-only
    risk: Low
    idempotency: idempotent
    constraints:
      timeoutMs: 30000
      maxOutputBytes: 1048576
```

After — `broker.yaml`:

```yaml
policiesPath: /home/dekopon/.config/dekopon/policies.cedar
constraintSets:
  echo.echo:
    provider: echo
    effect: read-only
    risk: Low
    idempotency: idempotent
    constraints:
      timeoutMs: 30000
      maxOutputBytes: 1048576
```

and `policies.cedar`, a new owner-only single-link file at mode `0600`, at most 1 MiB:

```cedar
@id("local-user-echo")
permit(principal == Dekopon::Principal::"local-user",
       action == Dekopon::Action::"echo.echo",
       resource == Dekopon::Provider::"echo")
unless { context has via };
```

Mechanical steps:

1. For each old rule, write a `constraintSets` entry keyed by its `capability`, carrying `provider`,
   `effect`, `risk`, `idempotency`, and `constraints` verbatim.
2. For each old rule, write one `permit` naming its `principal` and capability action. Rules that
   differed only by principal collapse into one policy with several principals or an `in` set.
3. Add `policiesPath`. It is **required** once any `constraintSets` entry exists.
4. Delete `rules:` entirely.
5. `@id("…")` every policy. That name is what audit records carry as `policy_ids`; without it Cedar
   names them positionally and inserting a policy renumbers the rest.

Startup validates the result against a schema generated from the deployment's own world, so a typo
in a principal or capability name is refused rather than becoming dead policy — with one exception,
agent names, described in [`broker-http.md`](broker-http.md#startup-validation).

0.3 also introduced `dekopond`. Adding it is not part of this migration; the broker upgrade stands
alone.

### 0.4 → 0.5 — broker and clients must be deployed in lockstep

The alpha broker protocol changed for policy-filtered command words and command resolution. This is
the release that made "upgrade all four together" a hard requirement rather than good practice: a
`dekopond` or `dekopon-run` from 0.4 cannot talk to a 0.5 broker, and the failure surfaces as a
protocol decode error rather than as a clean refusal.

Two other 0.5 changes can affect an existing deployment:

- **Provider loading became directory-aware and permission-checked.** `providers:` entries may now
  be directories, loaded non-recursively and deterministically, and every component file is checked
  for ownership, mode, and count. A `.wasm` file that was group-writable, or whose parent directory
  was, loaded before and is refused now. Fix the modes rather than the check.
- **Policy naming an unloaded provider became tolerated by default.** A policy referencing a provider
  the broker has not loaded now warns and continues instead of refusing startup; the name is
  registered as a schema-only phantom that no constraint set can bind. Set `strict: true` to keep
  the old refusal. Either way an undeclared *principal* stays fatal, and a capability nothing routes
  is denied `unconstrained-capability` at invocation in both modes.

### 0.5 → 0.6 — an OTLP endpoint carrying userinfo now fails startup

If `telemetry.endpoint` contains userinfo (`http://user:pass@collector`), the broker refuses to
start. Move the credential to `OTEL_EXPORTER_OTLP_HEADERS`, where it never enters the configuration
file, the process command line, or a span attribute. See
[`observability.md`](observability.md).

0.6 also added the `dekopon-webui` dashboard. It is **off unless `--http-bind` is supplied** and
opens no port otherwise, so upgrading changes no network surface by itself. If you do enable it,
read the access-boundary note in
[`crates/dekopon-brokerd/README.md`](../crates/dekopon-brokerd/README.md#read-only-web-ui) first: it
is unauthenticated and read-only, and the address you bind is the whole access control.

### 0.6 → 0.7 — standing instructions became readable

`inspect_agent_config` lets an authorized chat sender retrieve an agent's `instructions` verbatim.
Nothing to edit, but audit your catalog before upgrading: **an agent whose `instructions` contain a
secret, a token, or an internal hostname now discloses it to anyone the policy already lets drive
that agent.** See [`catalog.md`](catalog.md#instructions-is-untrusted-model-text-and-it-is-readable).

### 0.8.1 → 0.9 — Slack Agent experience needs a reinstalled app

Nothing in 0.9 is breaking, and native in-flight activity is opt-in and off by default. But
`experience: agent` on a Slack transport requires a **different Slack app manifest** from the classic
one: it subscribes to the Agent View App Home event, and owned-thread continuation additionally needs
`message.channels` / `message.groups` with `channels:history` and `groups:history`. Those are
installation-time scopes, so switching a transport to `agent` means updating the manifest and
reinstalling the app, not editing `dekopond.yaml` alone. Separate classic and Agent manifests are in
[`examples/slack/`](../examples/slack/README.md).

The gateway does not guess the workspace plan. If Agent status is unavailable — `feature_disabled`,
`missing_scope`, or an equivalent permanent installation error — it disables Agent status for that
transport and falls back to the configured reaction, then to nothing. A workspace on a plan without
Agent support therefore degrades rather than failing, which also means a half-finished manifest
update looks like a working deployment with no Working UI.

### 0.9 → 0.11.0 — everything here is opt-in

- **A transport endpoint override must be a literal loopback address.** `127.0.0.1` and `::1` are
  accepted; the name `localhost` is not, because what it resolves to is the resolver's decision. A
  configuration using `localhost` for a test override is a startup failure.
- **A route naming an image generator on the text-only WhatsApp transport is a startup failure**
  rather than a paid-for PNG with no delivery path.
- **Provider storage and durable chat memory are opt-in and all-or-nothing.** Adding the `storage`
  or `chatMemory` section to `broker.yaml` requires every field in it; omitting the section leaves
  the broker exactly as it was.
- **The `gh` shell builtin is gone from this repository.** It ships from
  [dekopon-provider-gh](https://github.com/dekopon-agents/dekopon-provider-gh) now, an out-of-tree
  provider component fetched and pinned like any other. The container image is unaffected — it still
  stages `gh` at a pinned, attested tag — so an operator running the image has nothing to do; one
  building a custom image from `examples/providers/` no longer finds `gh` there.

### Chart upgrades

The chart is versioned independently of the application: `dekopon-chart-*` tags publish the chart,
`v*.*.*` tags publish crates, archives, and the container image. `appVersion` is what `image.tag`
defaults to, so a chart release and an application release are two separate upgrades. To run a newer
application under an existing chart, set `image.tag` (or better, `image.digest`) rather than waiting
for a chart release. [`charts/dekopon/README.md`](../charts/dekopon/README.md#two-version-numbers)
has the full account, including the retained-claim behavior that makes `helm uninstall` leave the
audit chain in place.

## Related documents

- [`CHANGELOG.md`](../CHANGELOG.md) — the authoritative record of what each release contains.
- [`operations.md`](operations.md) — the running-system runbook, including audit recovery.
- [`broker-http.md`](broker-http.md#version-and-compatibility) — what a version mismatch actually
  does on the wire.
- [`catalog.md`](catalog.md) — the catalog schema an upgrade may need you to re-read.
- [`container-image.md`](container-image.md) — how the image is assembled and what it pins.
