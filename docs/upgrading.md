# Upgrading Dekopon

**Status: current.** This is the operator's companion to [`CHANGELOG.md`](../CHANGELOG.md): what to
edit, in what order to restart, and which releases require a configuration change rather than a
binary swap. The changelog records *what changed*; this records *what you have to do about it*.

Dekopon is pre-1.0 and the local broker protocol is `v1alpha1`. There is no compatibility promise
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

### After 0.11.1 — optional public DRNs require a private map and second policy

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

## Pending the next release

These are implemented in this tree and sit under `[Unreleased]` in the changelog. They move into a
release section here when that release is tagged.

- **Delete `allowDevelopmentSubjects` from `broker.yaml` before upgrading the broker.** The field is
  gone, and `broker.yaml` rejects unknown fields, so leaving it is a startup failure rather than a
  value quietly ignored. Delete every `dev.*` `identityMappings` subject and attestor namespace with
  it: `dev` is no longer a subject service, so those lines no longer parse either. The field was off
  by default and no chart release could set it, so a deployment that never opted in has nothing to
  edit — and no persisted audit chain can carry a `dev.*` subject.
- **The interactive console left this repository.** `dekopon console` and the `dekopon-tui` crate
  now ship from [dekopon-console](https://github.com/dekopon-agents/dekopon-console), the way the
  `gh` provider did. `dekopon` is a local catalog and model-account CLI again, and a bare `dekopon`
  is the usage error it was before 0.11.0 rather than a full-screen view. Nothing loses authority:
  the console never held any.

## Related documents

- [`CHANGELOG.md`](../CHANGELOG.md) — the authoritative record of what each release contains.
- [`operations.md`](operations.md) — the running-system runbook, including audit recovery.
- [`broker-http.md`](broker-http.md#version-and-compatibility) — what a version mismatch actually
  does on the wire.
- [`catalog.md`](catalog.md) — the catalog schema an upgrade may need you to re-read.
- [`container-image.md`](container-image.md) — how the image is assembled and what it pins.
