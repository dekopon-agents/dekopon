# Upgrading Dekopon

**Status: current.** This is the operator's companion to [`CHANGELOG.md`](../CHANGELOG.md): what to
edit, in what order to restart, and which releases require a configuration change rather than a
binary swap. The changelog records *what changed*; this records *what you have to do about it*.

Dekopon is pre-1.0 and the local broker protocol is `v1alpha2`. There is no compatibility promise
across minor releases, and no automatic migration: the daemons refuse to start on configuration they
do not understand rather than guessing.

## Chat-scope triggers (unreleased)

Every chat attestation now carries a required `trigger`, and neither daemon accepts the other's
older frames: roll `dekopon-brokerd` and `dekopond` together. No configuration changes. Wakes are
off until a route sets `wakes: true` and `sessions.wakes` names a writable file; on Kubernetes that
file must sit on the gateway's state volume, outside the `sessions.journal` directory.

## Typed inference errors (0.20.0)

Rust embedders match `dekopon_model::error::InferenceError` instead of `ModelError`;
`Interrupted` is now `Cancelled`.

## OTLP private CA (0.21.0)

Nothing changes unless `OTEL_EXPORTER_OTLP_CERTIFICATE` is set. When it is, both daemons read that
PEM file at startup and trust its roots beside WebPKI for OTLP export over gRPC and HTTP; a path
that cannot be read or holds no certificate refuses startup, so mount the file before rolling. On
Kubernetes, chart 0.11.0's `otlp.caBundle: {configMap, key}` mounts it into both containers and
sets the variable. This is separate from the broker's `http.extraCABundles`, which still covers
provider HTTPS only.

## Provider HTTPS and owner settings (0.20.0)

Broker `http.extraCABundles` adds absolute PEM CA files for **all** provider HTTPS requests;
`http.nonPublicHttps` separately lists exact DNS authorities with explicit ports that may
resolve to private-unicast IPs. Neither setting grants a capability or disables the normal
HTTP host's DNS pinning, redirect refusal or per-capability destination check. A broker started
with either field on v0.19.0 rejects its configuration. Mount the CA bundle broker-only and
verify it exists before the new broker starts; no gateway, OTLP or frontend TLS setting changes.

`providerSettings.<provider-id>` is a bounded nonsecret JSON object visible only to the
matching provider during authorized `invoke`. An older runtime cannot link a provider
that imports `dekopon:settings/config@0.1.0`. Release/pin the v0.20.0 runtime first,
then release/pin such a provider artifact and add its settings. For OpenObserve the
settings hold `url`, `org`, and `stream`; the agent supplies none. Configure a separate
broker-bound search credential and a narrow Cedar grant before enabling agent queries.
OpenObserve OSS has no stream-level RBAC: the provider's generated SQL prevents an
untrusted agent from selecting another stream, but not a compromised provider or direct
use of its account. Verify the HTTPS peer and negative DNS/TLS cases before rollout.

## WhatsApp burst collection and unknown lengths (0.19.0)

Upgrade `dekopond` and `dekopon-brokerd` together. Asset inventory wire rows now carry `bytes: null`
when a chat service has not reported a length; numeric zero still means a known empty file. An old
broker rejects null. The alpha protocol remains `v1alpha2`, with no mixed-version compatibility
promise. The existing `dekopon:asset@0.1.0` WIT already represents unknown stored lengths, so this
correction needs no provider rebuild. Provider output descriptor lengths remain mandatory.

WhatsApp `debounceMs` now means a **quiet interval**, not a fixed non-sliding window. Its default
changes from 3000 to 5000ms. New `debounceMaxWaitMs` defaults to 15000ms from the first media
receipt and must be at least `debounceMs` when collection is enabled. Set both explicitly to tune
latency; equal values retain a fixed window. `debounceMs: 0` still bypasses collection regardless
of the maximum. Telegram's fixed three-second native-group window is unchanged.

Explicit values are preserved: an existing `debounceMs: 3000` still has a three-second quiet
interval and can split a six-photo burst with a 3050ms inter-photo gap. To use the new default,
remove that explicit value or set it to 5000. A hard maximum prevents sustained input from keeping
a batch open indefinitely; it does not queue work behind an active session, replay effects, or
guarantee a complete album. This change does not update deployed configuration automatically.

Persistent WhatsApp routes now retain freshly authorized photo-only arrivals associated with busy
work, within the existing conversation asset bounds, without restarting or queuing a model run.
Completion text or a separate acknowledgment asks whether another version is wanted; the next
request can use retained references, with downloads still lazy. Captioned messages and one-shot
routes keep their ordinary handling. Stop cancels late intake, and notices never claim a failed
run generated an image. No new capability grant or provider rebuild is required.

## Asset handles (0.18.0)

Remove route `providerAttachments` (including `maxPerReply`) and `chatAssetInputs`. Both now refuse
at configuration load, even an explicitly supplied empty list or null. There is no silent migration:
all exact proposal references resolve automatically to scoped read-only descriptors, while the broker
still authorizes capability and HTTP effects. Delivery now requires explicit `asset.send`, with four
new sends per turn. Grant it narrowly to the asset command provider, not every producer.

Upgrade core/broker/gateway together, then install providers rebuilt for `dekopon:asset` and streamed
HTTP. Old JSON result envelopes containing a top-level `attachments` array with a `base64` field
in any entry are refused after execution; ordinary attachment metadata remains provider JSON.
Old data-URL proposals are refused. Restart both daemons with the compatible provider set; references and queued sends are
process-local and do not survive restart. Configure broker-owned disk assets separately from gateway
scratch, sized for in-flight spools plus retained unlinked output files (chart default 320Mi).

There is no implicit send on attach, no retry after failed delivery and no sent-flag reset within an
asset generation. Telegram/WhatsApp adapters send PNG/JPEG; Slack/Discord/local accept concrete valid
media types. Route instructions should advertise the types and plan conversion explicitly. Earlier
version-specific route snippets below are historical and must not be copied into this version.

## Disk-backed chat assets (0.17.0)

Upgrade the gateway's scratch storage alongside the binary. Chart 0.9.0 uses disk-backed
`emptyDir` for gateway `/tmp`, sized independently by `volumeSizes.gatewayTmp` (320Mi default).
`volumeSizes.tmp` now sizes only broker scratch, which remains tmpfs. Older charts and custom
installations must provide a writable disk-backed process temporary directory themselves;
retaining a memory-backed `/tmp` does not move retained image bytes off RAM. Verify the actual
mount and capacity after rollout, including any existing overrides.

`sessions.assetRetentionBytes` bounds the gateway-owned asset LRU (256 MiB default); zero disables
asset retention and delivery. Size scratch for that budget plus temporary IO headroom. Retention
is ephemeral, not restart persistence: model history keeps weak references and reclaimed images
become release notices. An unavailable provider image input refuses the edit rather than fetching
again or substituting the original image. Scratch errors do not retry already-executed provider
calls. Transient decode/request/upload allocations and filesystem page cache still use memory;
this release does not claim measured production RAM savings.

Version 0.17.0 introduced a fixed WhatsApp media-first window with `debounceMs: 3000`; the
[0.19.0 quiet-interval correction](#whatsapp-burst-collection-and-unknown-lengths-0190)
supersedes that timing. Telegram native media groups retain a fixed three-second collection window.
See [asset handling](dekopond.md#chat-assets) for the retention and delivery contract.

## Bounded chat-transport recovery (0.17.0)

No configuration changes are required. All adapters now use the
[shared recovery defaults](dekopond.md#connection-recovery). Connections start concurrently;
`gateway_started` no longer means every adapter is connected. Replace alerts on
`gateway_transports_degraded` with recovery/exit monitoring: exhausting even one adapter now
terminates the gateway nonzero after bounded draining rather than serving a permanently degraded
set forever. Ensure the process supervisor restarts failed gateways. Kubernetes restarts the
container; it need not replace the pod. Broker authority and outbound-effect retry behavior do not
change.

## Mapped compiled providers (0.17.0)

Remove broker `compileCachePath`; it is now an unknown field. Managed `providerSet` startup uses
`storePath/cwasm/v1` by default. Its first boot compiles and writes the cache; subsequent compatible
boots verify selected compiled hashes once and map the files. The provider store must be writable
on a miss. Legacy `providers:` paths continue source compilation without writing a cache.

Set `compileOnLoad: true` to disable all cwasm reads/writes, including when a cache error blocks
startup. Errors do not trigger repair or fallback. For manual cache removal, first stop every
process using it; never modify mapped artifacts in place. The old compressed cache is unused and
can be removed offline. Engine upgrades select new compatibility indexes rather than reusing
incompatible compiled objects. No automatic pruning occurs; see the [cache limits and tracing
contract](../crates/dekopon-brokerd/README.md#compilation-cache-and-the-concurrent-memory-budget).

Embeddings: rename `BrokerHostOptions::compile_cache_dir` to `cwasm_dir` and honor its immutable
trusted-file contract, or use `None`. SDK `host::engine` now takes only `Config`; Wasmtime's
compressed-cache feature is no longer enabled. Provider startup is sequential to bound compiler
memory; `provider.compile` now reports whole-load wall time, with separate compilation stages.

## Native-first progress (0.16.0)

An absent liveness block and `mode: off` remain disabled. Inside enabled liveness, omitted
`progress` now means `auto`: native status or typing/reaction avoids redundant placeholder posts,
with delayed message fallback only where supported. Explicit `progress: message` is unchanged;
change it to `auto` to prefer the native indicator and a fresh complete answer. `progress: off`
suppresses prose only.

`progressDetail: off` no longer suppresses explicitly requested answer streaming. Set `stream: false`
if that coupling previously kept answers unstreamed. Streaming still requires enabled liveness and
an implemented stream surface. Explicit Auto cancel buttons retain message-backed controls; a
button with progress Off and no stream, or an effective detail-Off route without streaming, now
refuses startup rather than silently hiding the button. WhatsApp stream/button refusals remain.
See [current presentation behavior and limitations](dekopond.md#liveness-progress-and-stopping-a-run).

## `activity:` becomes `liveness:` (0.14.0)

Rename the `activity:` block on every transport in the gateway configuration to `liveness:` before
upgrading. No transport has such a field any more, so a file that still carries one never decodes:
startup stops there, and the refusal names `liveness:` and says the old `activity.classicFallback`
moves to `liveness.classicFallback`, rather than listing the keys a transport does accept. There is
no alias and no migration read — a block that had become a no-op would leave an operator believing a
surface was configured — and because the file never decodes, this is the one gateway refusal that
arrives on its own instead of beside every other problem in the file.

```yaml
# before
    activity:
      mode: native
      classicFallback: reaction

# after
    liveness:
      mode: native                 # off | native, unchanged
      classicFallback: reaction    # unchanged; slackSocketMode only, and refused elsewhere
      progress: message            # explicit editable progress
      stream: false                # new; refused on whatsappCloudApi
      cancelButton: false          # new; refused on whatsappCloudApi and on experience: agent
```

`mode` and `classicFallback` keep their meanings and their values, so a rename alone reproduces
the reply-only behavior with `mode: off`. With `mode: native`, progress now defaults to Auto;
`stream` and `cancelButton` still default off. The block is now
accepted on `whatsappCloudApi` and `local` too, which previously had none.

Three settings are new and optional:

- **`stopWords:`**, top-level, default `[stop, cancel]`. A message that is exactly one of these
  words — after the bot mention and trailing punctuation are stripped, case-insensitively — stops
  the session the same sender is running in that conversation. Set it to your deployment's language,
  or leave it out. An empty list is refused: to switch this off, there is nothing to switch off,
  because a stop word fires only against that sender's own running session.
- **`progressDetail:`** on a route, `off | plain | detailed`, default `plain`. `off` reproduces the
  previous release's behavior for a route whose transport also leaves `progress: off`.
- **`limits.maxDurationMs`** on a route, optional. A wall-clock bound on one session counted from
  the moment the agent starts working. `0` is refused.

`ChatModel` changed shape in the same release: `complete` now takes a per-event callback and
streaming is not optional at the trait, and `complete_with` is gone. That reaches an operator only
through the new `stream:` field on an `openaiCompatible` model (default `true`); the
`chatgptSubscription` backend has always streamed and has no such field. An embedder that called
`dekopon_agent`'s loop directly compiles against the new signature or pins 0.13.

## Conversations replace route matches and chat-scope breadths; `memory:` replaces the route's `conversation:` (0.14.0)

Every route's `match:` becomes `conversation:`, and `kind` is a list or the word `any`.
`{ kind: directMessage }` becomes `conversation: { kind: [directMessage] }`; `{ kind: channel, channel: X }`
becomes `conversation: { kind: [channel, thread], ids: [X] }` if threads under X should answer
(they were silent before; this is the one behavior change) or `[channel]` if not;
`{ kind: channel }` becomes `conversation: { kind: [channel, thread] }`. Slack multi-person DMs
were `channel`; they are now `groupDirectMessage`, so a Slack route that should keep answering them writes
`kind: any` or adds `groupDirectMessage` to the list. The route's memory window, which was also called
`conversation:`, is `memory:` with the same fields; a file with a `mode:` under `conversation:`
refuses to start and names the rename. In `broker.yaml`, every `chatScopes` entry drops `breadth`:
`transportWide` becomes `conversation: { kind: any }`, `exactChannel` becomes
`conversation: { kind: [channel, thread], ids: [channel] }` (`[directMessage]` for a Slack `D…` id), and
`exactConversation` is gone — grants no longer name a thread, so a grant on one exact thread
becomes a grant on its parent. In Cedar, read `context.conversation.id` and, guarded by
`context.conversation has thread`, `context.conversation.thread`. **Grep your policy file for
`context.channel` rather than relying on the validator.** An unguarded `context.channel == "…"`
refuses to load, and so does comparing `context.conversation` to a string — but the guarded form
every 0.13 example used, `context has channel && context.channel == "…"`, still loads: Cedar types
the guard on an attribute the schema no longer declares as false and short-circuits, so the
statement quietly stops matching instead of failing. A capability pin written that way becomes a
capability nobody has. Durable chat-memory
namespaces on WhatsApp, Telegram topics, and Discord threads change shape and start empty;
Slack and Discord channels keep theirs. Restart the broker first, then the gateway.

## Reinstall both Slack apps (0.14.0)

Both manifests under [`../examples/slack/`](../examples/slack/README.md) gain `channels:read`,
`groups:read`, `im:read`, and `mpim:read`, and the classic manifest enables interactivity. A scope
change is not live until the install is reissued, so update each app from its manifest and reinstall
it to the workspace. Without the read scopes the gateway cannot place an `app_mention` — the event
carries no `channel_type` — and every mention is dropped with
`drop.reason = conversation-unresolved`; without interactivity a classic cancel button renders and
the press reaches nothing. No gateway configuration changes.

## Rebuild every provider component on the 0.13.0 SDK (0.14.0)

**Breaking.** Every pre-0.13 provider compatibility path is gone. Re-pin every provider artifact
your broker loads to a build on `dekopon-provider-sdk` 0.13.0 or later — and rebuild and re-release
any provider you own yourself — *before* upgrading the broker. There is no deprecation window and no
flag: a component that has not moved fails at load, and the broker refuses to start with it in its
provider set.

Two things stop working:

- **A manifest carrying `idempotency` is refused at `describe`.** 0.13.0 read the retired field and
  dropped it so an already-signed `.wasm` kept loading for one release; 0.14.0 restores
  `deny_unknown_fields` on `ProviderCapability`, so the field is now an unknown field like any
  other and the refusal names it. A component is a signed artifact, not a source file an upgrade
  can edit, so the only fix is a rebuild.
- **A component exporting only `resolve-command` no longer loads.** The host looks a command export
  up by name and now looks up `run-command` alone. A manifest declaring `commandWords` behind a
  component that exports no callable `run-command` is refused at load, naming the provider and the
  path. Build against `dekopon:provider@0.3.0`'s `provider-cli` world and implement
  `Provider::run_command`; the `provider-commands` world and its `resolve-command` export stay in
  the published `0.3.0` package text — published versions are immutable — but nothing calls them.

A component that exports only `describe` and `invoke` no longer loads either, whatever SDK built it:
0.15.0 also [refuses a provider with capabilities and no command word](#providers-run-on-argv-only-0150).

`gh` 0.3.0, `curl` 0.2.0, `turso-sql` 0.2.0, `memory-chat` 0.2.0, `gpt-image` 0.1.0, and
`openobserve` 0.1.0 satisfy both rules; pin those versions or later ones. `mediawiki`,
`jsonplaceholder`, `ripgrep`, `python`, and `skylight-private` 0.2.0 are built on the 0.13.0 SDK and
declare no word; their 0.3.0 releases declare `wikipedia`, `placeholder`, `rg`, `python`, and
`skylight`, so pin 0.3.0 or later. `echo` is retired, and no release of it
declares one. A provider you build yourself must be rebuilt on the 0.13.0 SDK the same way.

Embedders lose the machinery with it:

- `dekopon_provider_sdk::Provider::resolve_command`, `CommandResolution`,
  `export_provider_with_commands!`, and `host::RESOLVE_COMMAND_EXPORT` are gone.
  `Provider::run_command` no longer has a default that delegates to the rewrite, or any default:
  every provider implements it.
- `host::CommandExport` collapsed to `Present` / `Absent` / `Mismatched { found }`: there is one
  command export, so the variant no longer carries which name was found, and neither does
  `host::CommandExportProblem::Mismatched` or
  `dekopon_broker_host::BrokerHostError::CommandExportSignature`.
- `host::parse_command_run` is gone. Decode the guest's answer as
  `serde_json::from_str::<CommandRunOutcome>`.
- **The `resolveCommand` broker operation is removed from the wire.** `BrokerRequest::ResolveCommand`
  and `BrokerResponse::CommandResolution` no longer exist, and a broker answers the operation tag
  with `invalid-request`. It was kept for one release for a client predating `runCommand`; no
  in-tree client has sent it since 0.13.0. Send `runCommand` and match the `CommandRunOutcome`.

## A route may set the script deadline (0.15.2)

Optional, and nothing has to change: a route that writes no `limits.scriptTimeoutMs` keeps the 30
second deadline every script has always run under. Write one where a capability is genuinely slow.
Before this release the number was `dekopon-shell`'s own default and no configuration reached it, so
an image edit through `gpt-image.edit` — which routinely needs more than 30 seconds, where a generate
finishes in about 24 — was killed at exactly 30 with `dekopon-shell: script exceeded its 30000ms
deadline`, and the provider call still in flight was abandoned with the script.

```yaml
routes:
  - transport: workspace-slack
    conversation: { kind: [directMessage] }
    agent: image-editor
    limits:
      maxSteps: 8
      maxCapabilityCalls: 16
      maxDurationMs: 300000      # unchanged: the whole session's wall clock
      scriptTimeoutMs: 240000    # new: one script's deadline, default 30000
```

Two startup refusals come with it, each of a setting that decodes cleanly and could never take
effect: a `scriptTimeoutMs` of `0`, like every other zero bound in this file, and a
`scriptTimeoutMs` greater than the same route's `maxDurationMs`, where the session is cancelled
first. That second refusal names both numbers, because which of the two to move is the operator's
choice. `maxDurationMs` is unchanged and remains the session bound.

## A failed invocation carries the provider's own code and message (0.15.1)

**Upgrade both daemons together.** A failed `InvocationResult` gains `detail`, a `{ code, message }`
object, whenever the broker's classification came from a provider's own typed failure. The result is
strict-decoded in both directions, so a 0.15.0 `dekopond` reading a 0.15.1 broker's failed result
cannot decode it — exactly as [`runCommand` couples in 0.15.0](#providers-run-on-argv-only-0150).
There is no tolerant read and no default: a gateway that quietly dropped the field would discard the
only record of why a provider refused. The protocol version stays `dekopon.dev/broker/v1alpha2`.

Nothing in the configuration changes, and no policy, capability, or credential moves.

What an operator sees change:

- **The classification is unchanged.** `error` is still `provider-failure`, `provider-timeout`,
  `storage-quota`, and the rest. Alerts, dashboards, and the chat-memory category allowlist that
  group on it keep working exactly as they did. `detail` is additional, not a replacement.
- **`broker.execution` and `broker.execute` gain `error.code` and `error.message`**, present only
  for a typed provider failure. Provider-authored text, bounded at 128 and 1024 bytes and cut on a
  character boundary with `…[truncated]`, so retention grows by at most about a kilobyte per failed
  invocation. See [`observability.md`](observability.md#the-broker-audit-record).
- **A script's failure line grew.** `gpt-image.edit: failed: provider-failure` becomes
  `gpt-image.edit: failed: provider-failure: upstream-rejected: the image route refused the request
  with HTTP 400 (moderation_blocked)`. The exit status is unchanged. Anything parsing that line by
  splitting on `: ` past the classification sees more fields; the model reading it now relays the
  upstream refusal instead of inventing one.

### Embedder steps

- `dekopon_capability::InvocationResult` gains `detail: Option<ProviderFailureDetail>`, and
  `dekopon_shell::CapabilityCallResult::Failed` gains the same field. A struct literal or exhaustive
  pattern has to name it.
- `dekopon_core::ProviderFailureDetail`, `MAX_FAILURE_CODE_BYTES`, and `MAX_FAILURE_MESSAGE_BYTES`
  are the one definition of the pair and its bounds; `dekopon-broker-protocol` re-exports the type.
- `dekopon_broker::AuditEvent::Execution` gains `error_detail`, serialized as `errorDetail`.

## Providers run on argv only (0.15.0)

**Breaking.** A script reaches a provider only through that provider's command word, and the broker
refuses to start with a provider that declares capabilities and no word. There is no shim, no dual
path, and no deprecation window.

Four things stop working:

- **Bare capability words.** `wikipedia_page --title X` and `cli-probe.upper --text x` are ordinary
  unknown commands: exit 127, `command not found`, whether or not the session holds the capability.
  The shell no longer rewrites `--kebab-case` flags into a camelCase JSON object. A provider parses
  its own argv, so nothing sits between the flag the model typed and the field the provider reads.
- **Invoking through `cap`.** `cap <id> …`, with flags or a raw `{json}` object, is a usage error at
  exit 2. `cap --list` (`-l`) prints the capability identifiers the session was granted, and
  `cap --describe <id>` (`-d`) prints the identifier and its description, without `inputSchema`. A
  word's `--help` documents its arguments.
- **The shell `curl` builtin.** `curl` is `command not found` until a provider claims the word, and
  DRN use moved to provider commands ([`secrets.md`](secrets.md#agent-syntax)). No released provider
  proposes a DRN yet.
- **A word-less provider.** Startup names every provider whose manifest has capabilities and an
  empty `commandWords` in one error, beside any other provider-set conflict. This reverses the 0.13.0
  promise that describe/invoke-only components keep loading: a component built on the
  `dekopon:provider@0.1.0` or `0.2.0` base world has no `run-command` export and cannot declare a
  word.

Capability identifiers, Cedar policy, constraint sets, and constraint-set `credential:` injection
are unchanged. `probe upper --text hello` authorizes as `cli-probe.upper`.

### Operator steps

1. **Re-pin every provider to a release that declares a command word, before upgrading the
   broker.** [The rebuild section above](#rebuild-every-provider-component-on-the-0130-sdk-0140)
   lists which fleet releases do.
2. **Remove echo.** The container image no longer carries `/opt/dekopon/providers/echo-provider.wasm`
   and `ci/fetch-external-provider-components.sh` no longer fetches it, so a `providers:` entry naming
   it refuses startup. Delete that entry, every `echo.*` constraint set, and every Cedar statement
   naming an `echo.*` action or `Dekopon::Provider::"echo"`. The image's no-network default and the
   chart's default inline configuration are now the in-tree `cli-probe`: word `probe`, capabilities
   `cli-probe.upper`, `cli-probe.count`, and `cli-probe.reverse`.
3. **Rewrite agent `instructions` and skills that teach the old forms.** Search them for `$(name.`,
   `cap --describe`, `cap <id>`, capability identifiers used as words (`wikipedia_`,
   `gh.pull-request.`), and `curl`, and point the model at the provider's own `<word> --help`. A
   skill still teaching `wikipedia_page --title` gets exit 127 on every call.
4. **Upgrade both daemons together.** `runCommand` now requires a `traceParent`, and both envelopes
   are strict-decoded: a 0.14.0 `dekopond` against a 0.15.0 broker has every command word refused
   `invalid-request`, and a 0.15.0 `dekopond` against a 0.14.0 broker fails the same way. The
   protocol version stays `dekopon.dev/broker/v1alpha2`, so nothing earlier in the connection says
   so.
5. **Re-point telemetry queries, and size retention for what the trace now holds.**
   - `shell.command.kind` loses `capability` and `not-granted`: a provider word is
     `provider-command`, and a capability-shaped word is `not-found`.
   - `shell.command` and `provider.run_command` carry the arguments, the piped value, and the
     output, each up to 4096 bytes plus `…[truncated]`, beside a `.bytes` total. A loop exports
     every iteration's.
   - `broker.command_run` is a new span per `runCommand`
     ([`observability.md`](observability.md#broker-execution-spans)).
   - Storage-backed spans and audit records carry the subject, agent, capability, provider, policy
     fields, and `input` every other invocation does, and storage evidence carries exact `readBytes`
     and `writeBytes`. Whose conversation a chat-memory call read is now in the telemetry store,
     which was always inside the operator's boundary.

### Provider-author steps

1. **Build on `dekopon-provider-sdk` 0.13.0 or later against the `provider-cli` world**
   (`include dekopon:provider/provider-cli@0.3.0;`) and export with `export_provider_with_cli!`. The
   0.15.0 SDK deletes `export_provider!` and `export_provider_with_bindings!`.
2. **Declare a command word** in the manifest's `command_words`. It may not be a word the shell owns
   (`dekopon_core::RESERVED_COMMAND_WORDS`, which no longer holds `curl`) or another loaded
   provider's word; either is a startup conflict. A provider whose capabilities only a gateway route
   reaches, such as chat memory's record route, still needs one.
3. **Implement `Provider::run_command`**, which the 0.15.0 SDK requires. Parse argv — the SDK's
   `clap` feature maps a `clap::Command` tree — build the JSON your `invoke` already accepts, and
   return `CommandRun::proposal(capability, input)`. Render `--help` and usage errors yourself as
   `CommandRun::Rendered`. `stdin` is the piped value's display text. Flags are yours to spell:
   nothing rewrites them on the way in.
4. **To offer a secret**, return a `CommandInvocation` whose `secret_use` names the DRN the argv
   carried, as `SecretUseProposal::HttpBearer { secret }` or `HttpBasic { secret, username }`. That
   needs the 0.15.0 SDK. On the wire it is `secretUse`, absent when `None`, so a 0.13.0 provider's
   output is unchanged. The provider never receives the material; the broker authorizes the use as
   [`secrets.md`](secrets.md#two-independent-policies) describes.
5. **Rebuild, release, and re-pin.**

### Embedders

- `BrokerClient::run_command` and `RequestEnvelope::run_command` take a `TraceParent`: pass the one
  the calling span carries, as `invoke` does.
- `CommandInvocation`, `CommandRunOutcome::Proposed`, and `dekopon-shell`'s `CommandRun::Proposed`
  gain `secret_use`; a struct literal or exhaustive pattern has to name it.
- `CapabilityInvoker::grants_namespace` and the shell's `curl_capability` plumbing are gone.
- `dekopon_core::bounded_attribute` and `MAX_ATTRIBUTE_BYTES` are the span-attribute bound the shell
  and the broker host share.

## Telemetry payloads (0.13.0)

Remove `telemetryPayloads` from the `telemetry:` block of both `broker.yaml` and the gateway
configuration before upgrading. Both sections reject unknown fields, so a daemon started on a file
that still carries the key refuses to start and names it. Nothing replaces it, and there is no
metadata-only mode to fall back to.

What you export widens. Spans and log records now always carry what the key used to gate: the
provider input on `broker.authorize` and `provider.invoke`, the full request URL with its path and
query on `http.request`, the verbatim model transcript, the inbound chat text with its sender's
canonical subject, the prompt cache key, and every command word a script ran including the ones the
model wrote. Size the store's retention for that, and treat access to it as access to every
conversation the system has handled. The exclusions are unchanged and were never part of the gate:
secret bytes and the gateway's own credentials never reach telemetry, and HTTP request and response
headers and bodies stay out of spans.

## Catalog `Capability` and `Provider` documents (0.13.0)

Delete every `kind: Capability` and `kind: Provider` document from every catalog before upgrading.
A catalog carrying one refuses to load, naming the kind:

```text
dekopon.yaml: 1 validation problem found:
  - document 2: kind Capability is no longer part of the catalog; remove the document. Capabilities and providers come from the broker, which builds them from provider manifests and its own constraint sets
```

Nothing else changes. The `Agent` documents are untouched, `dekopond` binds routes exactly as
before, and no broker configuration moves: those documents were never consulted at run time, and
the capability surface a session reaches has always been the broker's answer under that agent's
attestation. An agent's own `capabilities:` and `providers:` lists stay valid to author and stay
read by nothing.

One check goes with them. A route's `chatAssetInputs` was held to the capabilities the catalog
declared, so a misspelled identifier there refused startup; now it loads and the marker is passed
to the provider verbatim, which reads as the provider rejecting its own input. Check those
identifiers against the broker's `constraintSets` by hand.

## The `idempotency` classification is gone (0.13.0)

Delete every `idempotency:` line from `broker.yaml` (`constraintSets`) and from the chart values
that render them (`broker.constraintSets`). `ConstraintSet` decodes with `deny_unknown_fields`, so
a retained line is a **startup refusal**, not an ignored field. A catalog cannot carry one at all:
`kind: Capability` documents are gone
[as above](#catalog-capability-and-provider-documents-0130).

Drop `context.idempotency` from every Cedar policy. The attribute is no longer declared on the
capability action, and strict validation refuses a policy set that reads an attribute the schema
does not declare, so a retained clause is also a startup refusal. Idempotency, exactly-once and
duplicate-effect defense are [named non-goals](design.md#non-goals); `effect` and `risk` are
unchanged and still matched byte for byte against the provider manifest.

A provider **component** was the one exception, for one release only: `dekopon-provider-sdk`
0.13.0 accepted and dropped an `idempotency` field in the manifest a component returns from
`describe`. [0.14.0 removed that tolerance](#rebuild-every-provider-component-on-the-0130-sdk-0140)
and a component still emitting the field is now refused at `describe`. A provider's Rust source
changed here too: `ProviderCapability` no longer has the field, and
`dekopon_provider_sdk::Idempotency` no longer exists.

The field was also one byte of the authority surface every storage namespace generation is keyed
by. [Provider storage starts empty](#provider-storage-starts-empty-0130) covers that
rotation; it needs no separate step.

## Broker dashboard retirement (0.13.0)

Remove the broker listener flag and chart value when upgrading. The UI and its reporting feed
are retired; see the [lockstep and refusal contract](../crates/dekopon-broker-protocol/README.md#version-and-compatibility).
Provider HTTP, gateway webhooks, model accounting and daemon tracing remain.

## Replay ledger removal (0.13.0)

`brokerLimits.maxReplayIds` is now an unknown field and refuses startup. Remove it from `broker.yaml`
and from the chart's `broker.config.brokerLimits` before upgrading; `brokerLimits` itself is optional
and may go with it.

The broker no longer remembers invocation identifiers, so a resubmitted proposal is authorized and
executed again, and a redelivered turn can be recorded into chat memory twice. Duplicate-effect
defence is a [non-goal](design.md#non-goals): a caller that must not repeat an effect must not
resubmit it. The identifier itself is unchanged — it still binds an attestation to its proposal and
names the call in audit.

## Aggregate guest memory ceiling (0.13.0)

`hostLimits.maxTotalMemoryBytes` now defaults to **256 MiB** — four concurrent provider stores at
the default 64 MiB per store — where it was previously unset and the aggregate unbounded. A broker
that runs more than four provider invocations at once will now refuse the fifth with a resource
failure instead of growing toward `serverLimits.maxConnections` × `maxMemoryBytes`, which is 4 GiB
at the defaults. Raise it in `hostLimits` for a container that has the memory:

```yaml
hostLimits:
  maxTotalMemoryBytes: 1073741824
```

`hostLimits` defaults field by field, so that one line is the whole change. An explicit
`maxTotalMemoryBytes: null` restores the unbounded behavior. A deployment that already sets the
field — including the Helm chart's own `268435456` — is unaffected.

## Legacy bearer credential shape (0.13.0)

Read every `kind: bearerToken` entry in `broker-credentials.yaml` before upgrading. Its `secret` must
now be at least 16 bytes of printable ASCII with no whitespace, control or non-ASCII bytes — the
previous rule accepted any non-empty printable value, spaces included — and the broker refuses to
start, naming the credential and not the value, on one that is not. The secret is now also the byte
string the native HTTP host searches responses for before returning them, so a short or
phrase-shaped value would deny answers that never carried it. A real issued token satisfies both
rules already; a hand-written placeholder or a development stub may not. Replace such a value with
the real credential rather than working around the refusal.

## Broker audit configuration (0.13.0)

Delete `auditPath` and `serverLimits.auditMaxLineBytes` from `broker.yaml` before upgrading. Both
are unknown fields, and `broker.yaml` rejects unknown fields, so a broker started on a file that
still names either refuses to start and names the field. A configuration that still carries
`checkpointPath`, `checkpointLockPath`, or `serverLimits.auditMaxRecords` from 0.12.0 is refused the
same way; delete those too. `serverLimits` stays all-or-nothing, so every field it has left is still
required when the section is present.

There is no on-disk audit. Every broker decision is a `broker.decision` or `broker.execution` record
on the daemon's stdout JSON, and an OTLP log record as well once `telemetry` names a receiver
([the broker audit record](observability.md#the-broker-audit-record)). An existing `audit.jsonl` is
read by nothing and may be deleted. To keep audit past a pod restart, configure `telemetry`; without
it, audit lasts as long as whatever keeps the broker's stdout. Losing the log exporter loses audit.

Under the Helm chart, the chart's default `broker.yaml` sets neither field. A `broker.yaml` you
supply yourself — `broker.config.inline`, or the key `broker.config.existingSecret` names — must
drop them too.

Library callers of `run` receive unit on clean shutdown.

## Provider wall clock import (0.13.0)

A provider component that imports `dekopon:clock/wall@1.0.0` requires a broker built with this
release or newer. An older broker refuses it when it loads the component, before it binds its
socket:

```text
could not instantiate broker provider component <path>: component imports instance `dekopon:clock/wall@1.0.0`, but a matching implementation was not found in the linker
```

Upgrade the broker before installing such a provider. Components that do not import the clock load
unchanged on this release; from 0.15.0 a component also needs
[a command word](#providers-run-on-argv-only-0150). A provider that reads the clock
outside `invoke` — from `describe` or `run-command` — now fails that
call as `DescribeUsedHostImport` or `RunCommandUsedHostImport`.

## Provider storage direct-write contract

Provider storage applies each write immediately; provider traps, invalid responses and cancellation
can leave completed writes. Applications must not assume invocation-wide rollback, atomic
cross-file commit, or crash recovery. Inactive generations remain charged to the root quota.

The strict storage limits object accepts only live bounds. Configure the complete object using
`StorageLimits` defaults/current fields; omit all GC scheduling/TTL and startup recovery-count
settings. `maxPendingTransactions` remains the compatibility spelling for concurrent invocation
handle admission. Retained stores with
unknown root or generation entries are refused; there is no automatic migration,
recursive cleanup or trusted import of legacy layout bytes. Preserve such data offline rather
than deleting entries to bypass a refusal. A separately provisioned private storage root starts
empty and does not restore previous data.

## Provider storage starts empty (0.13.0)

Provider storage starts empty at this release. Before upgrading a broker that has `storage:`
configured:

1. Stop the broker.
2. Move the storage root aside. Keep it if those bytes matter; nothing in this release reads it.
3. Delete `storage.namespaceKeyPath` and `storage.maxQuarantinedNamespaces` from `broker.yaml`.
   Both are refused by name at startup.
4. In the chart, delete `providerStorage.existingKeySecret`, `existingKeySecretKey`, `keyDir` and
   `keyFileName`, which `helm template` refuses by name, then delete the key Secret itself.

A root from an earlier release does not open: its `layout` still carries `keyCommitment` and it
still holds `quarantine/`, and startup refuses it as a corrupt layout naming the root. Nothing
migrates it. Every namespace name is derived differently now, so there is no subset of it — stable
continuity included — that a new broker could address, and nothing about it is corrupt in the sense
of `storage_namespace_reset`: it is simply a different layout. This one note covers every storage
change in this release: the key deletion, the quarantine-limit removal, and the idempotency byte
leaving the authority surface. None of them needs a separate step.

After the upgrade, a corrupt namespace no longer stops the broker, and startup no longer validates
namespaces at all. The invocation that finds a corrupt authority pointer or generation resets that
namespace to a fresh generation and fails once with `storage-corrupt`; its `storage_namespace_reset`
record names the base token, both generations and the path. A namespace whose own shape is wrong — a
symlink, hard link or wrong mode under `namespaces/<base>` — is skipped by the startup quota walk
(`storage_root_entry_ignored`) and fails every grant naming the entry. To clear one, stop the broker
and remove `namespaces/<base>`; that conversation starts empty. The `broker.execute` span's
`storage.namespace` names the base for any traced invocation.

## Two rules that apply to every upgrade

### Upgrade both daemon executables together

`dekopon-brokerd` and `dekopond` are separately installable — Homebrew,
crates.io, release archives, the container image, and the Helm chart with its own `image.tag` — so a
mixed set is easy to end up with by accident. Do not. The local broker protocol has one version
constant and both envelopes are strict-decoded; a newer broker adding a field to a response an older
client already understands makes that response undecodable, which is the failure a partial upgrade
most reliably produces. [`dekopon-brokerd` contract](../crates/dekopon-broker-protocol/README.md#version-and-compatibility) has the exact
mechanics. The container image and the chart ship both daemon executables from one release for this reason.

### Restart the broker first and stop it last

`dekopond` asks the broker for capabilities once at startup and **exits non-zero** if the broker does
not answer, so a gateway started against a stopped broker crash-loops rather than waiting. Shutdown
runs the other way: the gateway drains first so no session is mid-invocation when the broker begins
draining.

Dekopon ships no service units, so the order is yours to enforce whatever supervises the processes:

1. Stop `dekopond`.
2. Signal `dekopon-brokerd` with `SIGINT` or `SIGTERM` and let it finish. It stops accepting, drains
   bounded in-flight connections, removes only the socket inode it created, logs `broker_stopped`,
   and flushes any configured OTLP exporter, whose last batch carries the final audit records.
3. Replace the binaries and make any configuration edits the release notes below call for.
4. Start `dekopon-brokerd` and wait for it to be answering on its socket.
5. Start `dekopond`.

Under the Helm chart this ordering is structural rather than procedural: the broker is a native
sidecar with a startup probe, so Kubernetes will not start `dekopond` until the broker answers a real
request, and terminates them in the reverse order.

Read the broker's audit records before restarting it to investigate a refusal: without `telemetry`,
they exist only in its stdout. Restarting does not make an uncertain external effect safe to retry.
See [`operations.md`](operations.md#audit).

## Release-by-release

Only releases that need an operator action appear here. A release absent from this list is a binary
swap in the order above.

### 0.12.0 → 0.13.0 — command words run over `runCommand`, and `imageGenerator:` is removed

One item here — the removed `imageGenerator:` — **does** need a configuration edit; nothing else
does.

- **Building the crates needs Rust 1.98.1.** Every published crate now declares
  `rust-version = "1.98.1"` (was 1.89.0), the same compiler the repository pins, so `cargo install`
  on an older compiler refuses them. Release archives, the container image, and the chart are
  unaffected. An embedding that calls `dekopon-provider-sdk`'s `host` functions moves to Wasmtime 48
  with it.
- **`chatgptSubscription` is a new broker credential kind; nothing existing has to change.** Every
  `bearerToken` entry in `broker-credentials.yaml` keeps its exact meaning. The new kind takes an
  absolute `authFile` instead of a `secret` and `scheme`, and the broker refuses to start when a
  `bearerToken` entry sets `authFile` or a `chatgptSubscription` entry sets `secret` or `scheme` — a
  stricter refusal than before, since per-kind fields were previously unvalidated because there was
  one kind. Startup also now reports *every* problem in that file at once rather than the first;
  embedders matching `CredentialsError::InvalidName` or `CredentialsError::InvalidCredential` should
  match `CredentialsError::Invalid { problems }` instead.
- **Adopting it needs its own ChatGPT login.** Run
  `dekopond auth chatgpt login --auth-file <path>` a second time rather than pointing the broker at a
  `chatgptSubscription` model's file: the refresh token rotates and the authorization server retires
  its predecessor, so two holders of one document revoke the family for both. The `authFile` must be
  an owner-only `0600` single-link regular file in an owner-only **writable** directory, because a
  rotated record is persisted by renaming a sibling temporary file over the target. Under the Helm
  chart, `broker.chatgpt.*` (new in chart `0.4.0`) seeds it once into
  `<paths.stateDir>/broker-chatgpt/`. Full lifecycle in
  [`chatgpt-credential.md`](chatgpt-credential.md#a-second-family-for-the-broker).
- **Two new classified invocation failures exist.** A capability presenting a refreshing credential
  can now fail as `credential-unavailable` (the refresh-token family is retired; an operator must log
  in again, and `broker_chatgpt_credential_reauth_required` says so at error level) or
  `credential-refresh-failed` (transport, a 5xx, a malformed token response; retrying is the whole
  remedy). Every other capability keeps serving. Alerting on
  `broker_chatgpt_credential_reauth_required` and on the `broker.credential.refresh` span's
  `outcome = rotated-unsaved` is the operational change; see
  [`observability.md`](observability.md#broker-failure-events).

- **Upgrade the broker before its clients, and both daemons together.** The local protocol
  stays `dekopon.dev/broker/v1alpha2`, but `dekopond` now sends a provider
  command word as `runCommand` — the word, its argv, and the optional piped value — and reads back
  the guest's own outcome. A newer broker still answers the legacy `resolveCommand`, with a
  rendered page degraded to a decline carrying its stdout then stderr, so an older client keeps
  working for one release. The reverse does not hold: an older broker refuses `runCommand` as
  `invalid-request` at the `operation` tag, indistinguishable from a corrupt frame, so a newer
  client against an older broker reports every command word as a failed run until the broker moves.
- **Upgrade the broker host before a provider adopts `run-command`.** A component built against
  `dekopon:provider@0.3.0`'s `provider-cli` world exports `run-command`, which the broker host
  at this version looks up; an older host finds no `resolve-command` behind the manifest's
  `commandWords` and refuses the component at load. Components built against `0.1.0` or `0.2.0`
  keep loading unchanged and never receive a piped value.
- **Embedders: `BrokerClient::resolve_command`, `RequestEnvelope::resolve_command`, and
  `Broker::resolve_command` are gone.** Call `BrokerClient::run_command` and `Broker::run_command`
  and match the `CommandRunOutcome` they return; `BrokerRequest::ResolveCommand` remains a request
  the broker answers, not one the client builds.

#### `imageGenerator:` is removed; delivery is a route opt-in instead

Image generation and its credential belong to the provider/broker path, not the gateway. The `imageGenerator:`
gateway block, the `routes[].imageGenerator` flag, and the `generate_image` model tool are all gone,
and because `dekopond.yaml` is strict-decoded a file still naming either one **refuses to start**
with the unknown field's name rather than quietly ignoring it. Delete both, and delete the
`apiKeyEnv` variable from the deployment's environment — it is read by nothing now.

Generating an image is a provider effect now: a provider capability produces the bytes, Cedar decides
each call, and the broker audits it. The gateway's job is delivery, and a route opts into that:

```yaml
# before
imageGenerator:
  model: gpt-image-1
  apiKeyEnv: OPENAI_IMAGE_API_KEY
  timeoutMs: 120000
routes:
  - transport: workspace-slack
    match: { kind: directMessage }
    agent: reviewer
    imageGenerator: true

# after — no gateway block at all
routes:
  - transport: workspace-slack
    match: { kind: directMessage }
    agent: reviewer
    providerAttachments:
      maxPerReply: 1
    chatAssetInputs: [gpt-image.edit]
```

`providerAttachments.maxPerReply` is how many files one reply may carry; omitting the block means a
capability's `attachments` key is stripped and refused, which is what every route does today.
`chatAssetInputs` lists the capabilities whose input may name one of the conversation's own
attachments as `chat-asset:<N>`, so a person's photo can reach a remix capability; a capability left
out of the list receives such a string verbatim.
`maxPerReply: 0` is refused — omit the block instead — and pairing `providerAttachments` with a
`whatsappCloudApi` transport is still a startup refusal. A model that calls `generate_image` now takes
the ordinary unknown-tool path and ends the session.

There is no replacement that keeps the old shape. A deployment that wants images needs a provider
offering an image capability, a constraint set and Cedar statement for it in the broker, and the route
opt-in above. [`dekopond.md`](dekopond.md#asset-handles-and-delivery) has the
conventions and their bounds.

### 0.11.1 → 0.12.0 — optional public DRNs require a private map and second policy

Existing `credentialsPath`, `credential`, and `credentialByAgent` deployments need no migration for
this release and retain byte-compatible legacy audit serialization. *Committed direction:* the
`credential`/`credentialByAgent` bindings will be replaced by public DRNs in a future migration,
preserving broker-owned refresh ([requirements](design.md#legacy-credential-bindings)).
To opt into the currently implemented model-selected DRN path:

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
  it: `dev` is not a subject service, so those lines do not parse either. The field was off
  by default and no chart release could set it, so a deployment that never opted in has nothing to
  edit — and no persisted audit log can carry a `dev.*` subject.
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
  in one step.** The request operations collapsed to one per verb,
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
  bump: it calls `BrokerLeg::connect_attested`, which does not exist, so moving its pin past
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
- **Model clients follow no ambient `HTTPS_PROXY` or `ALL_PROXY`.** Every
  `dekopon-model` transport — the OpenAI-compatible chat client and the ChatGPT subscription client
  with its device-flow login — is built from one agent that sets no proxy
  and follows no redirect, so an exported proxy variable carries no bearer token, the
  device-code exchange, or a prompt through a host nobody named to Dekopon. That is the stance
  `dekopon-http-host` already took for provider HTTP. It reaches `dekopond`, `dekopon-run`, and
  `dekopond auth chatgpt login`; nothing in a configuration file changes, and there is no field or
  flag to opt back in, so a model endpoint that was only reachable through that proxy is
  unreachable after the upgrade.
- **Chat transports and the OTLP exporter follow no ambient proxy either.** Slack, Discord,
  Telegram and WhatsApp each build their one HTTP client from a single `credential_client` shape
  that sets no proxy, follows no redirect, and replays no request, so an exported proxy variable
  carries no Slack app or bot token, Discord or Telegram bot token, or WhatsApp Graph access token
  — nor the messages they authenticate — through a host nobody named to Dekopon.
  `dekopon-telemetry`'s OTLP/HTTP client takes the same stance, so the ingest header in
  `OTEL_EXPORTER_OTLP_HEADERS` stops travelling that way too. Nothing in a configuration file
  changes and there is no flag to opt back in. A chat service reachable only through a proxy is
  unreachable after the upgrade, and **a collector reachable only through `HTTPS_PROXY` must now be
  addressed directly** — otherwise export stops, and because audit is one structured log record per
  broker decision emitted through that exporter, losing it loses audit. Point `telemetry.endpoint`
  at the collector itself, or run one on the host.

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
   `effect`, `risk`, and `constraints` verbatim.
2. For each old rule, write one `permit` naming its `principal` and capability action. Rules that
   differed only by principal collapse into one policy with several principals or an `in` set.
3. Add `policiesPath`. It is **required** once any `constraintSets` entry exists.
4. Delete `rules:` entirely.
5. `@id("…")` every policy. That name is what audit records carry as `policy_ids`; without it Cedar
   names them positionally and inserting a policy renumbers the rest.

Startup validates the result against a schema generated from the deployment's own world, so a typo
in a principal or capability name is refused rather than becoming dead policy — with one exception,
agent names, described in [`dekopon-brokerd` contract](../crates/dekopon-brokerd/README.md#catalog-ownership-at-policy-startup).

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
  rather than a paid-for PNG with no delivery path. (The `imageGenerator:` block itself was removed
  after 0.12.0. Current WhatsApp routes support descriptor-backed PNG/JPEG delivery through authorized
  `asset.send` and bounded image editing;
  see the [current transport contract](dekopond.md#meta-whatsapp-cloud-api).)
- **Provider storage and durable chat memory are opt-in and all-or-nothing.** Adding the `storage`
  or `chatMemory` section to `broker.yaml` requires every field in it; omitting the section leaves
  the broker exactly as it was.
- **The `gh` shell builtin is gone from this repository.** It ships from
  [dekopon-provider-gh](https://github.com/dekopon-agents/dekopon-provider-gh) now, an out-of-tree
  provider component fetched and pinned like any other. The container image is unaffected — it still
  stages `gh` at a pinned, attested tag — so an operator running the image has nothing to do; one
  building a custom image from `examples/providers/` does not find `gh` there.

### Chart upgrades

The chart is versioned independently of the application: `dekopon-chart-*` tags publish the chart,
`v*.*.*` tags publish crates, archives, and the container image. `appVersion` is what `image.tag`
defaults to, so a chart release and an application release are two separate upgrades. To run a newer
application under an existing chart, set `image.tag` (or better, `image.digest`) rather than waiting
for a chart release. [`charts/dekopon/README.md`](../charts/dekopon/README.md#two-version-numbers)
has the full account, including the retained-claim behavior that makes `helm uninstall` leave the
state claim and the credentials it holds in place.

## Related documents

- [`CHANGELOG.md`](../CHANGELOG.md) — the authoritative record of what each release contains.
- [`operations.md`](operations.md) — the running-system runbook, including where broker audit lives.
- [`dekopon-brokerd` contract](../crates/dekopon-broker-protocol/README.md#version-and-compatibility) — what a version mismatch actually
  does on the wire.
- [`catalog.md`](catalog.md) — the catalog schema an upgrade may need you to re-read.
- [`container-image.md`](container-image.md) — how the image is assembled and what it pins.

## 0.13.0: standalone catalog CLI retirement

The standalone `dekopon` package/executable and its get/describe/validate/config commands are
removed. Install the matching `dekopond` from this source revision and use
`dekopond auth chatgpt {login,status,logout,export}` instead. Auth flags belong after `auth`;
serving still requires `dekopond --config PATH`. Existing isolated credential files, export
Secret labels/keys/default names, and refresh behavior are unchanged. Published 0.12.0 archives
remain historical artifacts; they do not provide the new gateway auth command.
