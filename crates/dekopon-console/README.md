# dekopon-console

The console ships with Dekopon and uses the workspace version and local core crates.

An operator-controlled interactive client of `dekopon-brokerd`. The broker alone maps subjects, authenticates the Unix peer UID, checks `agent.prompt` and provider grants, and holds provider credentials. The broker refuses real conversation claims from the authenticated console peer; whoever can exec arbitrary code as the console UID can still propose subjects inside its broker attestor envelope; the profile picker is convenience, not authorization or Kubernetes user attribution.

## Install

```sh
brew install dekopon-agents/tap/dekopon-console
```

Release archives for Linux x86_64/aarch64 and macOS arm64 are on the GitHub releases page, with
provenance attestations. `ghcr.io/dekopon-agents/dekopon:<tag>` (linux/amd64, arm64) includes the console.
The chart's `console.enabled` runs `dekopon-console --idle` as UID 65535 in the Dekopon pod;
set the deployment's `console.image` to this Dekopon image. Exec sessions read these from the container environment:

| Variable | Flag |
|---|---|
| `DEKOPON_BROKER_SOCKET` | `--socket` |
| `DEKOPON_BROKER_UID` | `--server-uid` |
| `DEKOPON_CONFIG` | `--config` |
| `DEKOPON_CONSOLE_SUBJECT` | `--subject` |

```sh
kubectl exec -it deploy/dekopon -c console -- dekopon-console
```

## Run

```sh
dekopon-console --config /config/dekopon.yaml --profiles /config/console-profiles.yaml \
  --socket /run/dekopon/broker.sock --server-uid 65532
# exact catalog name plus an authored matching profile
dekopon-console --config /config/dekopon.yaml --profiles /config/console-profiles.yaml \
  --profile family-channel --agent lange-family --server-uid 65532
# explicitly subject-only, for a separately authorized deployment
dekopon-console --subject slack.t0123abc.u9xyz --agent reviewer --server-uid 65532
# separate idle PID1; no catalog, broker, model, or credential setup
dekopon-console --idle
```

Interactive commands need stdin/stdout TTY (`kubectl exec -it`). For bounded non-interactive shell commands, use `kubectl exec -i … -- dekopon-console --agent <AGENT> --structured --no-color` with **both** stdin and stdout non-TTY. Supply newline-terminated commands on stdin (up to 64 commands, 16 KiB each). A random process nonce frames `READY <nonce>` after the agent hop, then `RESULT <nonce> ok|error|timeout`, sanitized result lines and `END <nonce>` for each command. A submitted 65th command is refused in its own error frame and the process exits nonzero; a timeout emits an outcome-unknown frame and exits nonzero without running later commands. Result lines that begin with the active nonce marker are prefixed `> `; do not infer broker success solely from an `ok` frame. Output is bounded to 64 KiB per result. Input is not echoed; local asset input uses the TUI's `[input redacted]` label internally, but TUI audit/history do not promise general secret redaction. `--no-color` affects diagnostics, not the TTY guard. Default mode remains interactive.  A missing broker, incorrect server UID, refused peer/agent, unknown agent, missing profile, ambiguous same-agent profiles, or absent TTY fails clearly. The picker still shows every catalog agent; selecting one with no matching profile refuses rather than borrowing another agent's identity. An empty broker surface and a failed fresh `agent.prompt` gate prevent execution. An idle process never makes a model or broker request.

### Authored profiles

This strict YAML or JSON document is **read-only, credential-free operator data**. Only `defaultProfile`, when explicitly present, selects the bare invocation's subject; bare launch still opens the picker. `--profile` selects a bound agent and may not be combined with `--subject`. `--agent` selects an exact catalog name; if multiple profiles bind it, pass `--profile`. A profile bound to another agent is a refusal. Without an authored default/selected profile, an explicit `--subject` (or `DEKOPON_CONSOLE_SUBJECT`) is required. Unknown keys, duplicate names, unknown catalog agents, zero or excessive limits refuse before broker/model setup. Profile names use ASCII letters/digits/`-`/`_`; `maxSteps` is 1–64 and `maxCapabilityCalls` is 1–256. CLI `--model`, `--max-steps`, `--max-capability-calls` override profile fields; then the fallback is model `gpt-5.6-luna`, 8 steps, 16 calls. The selected model is always visible in the header. Keep the full credential-free catalog and referenced skill paths available; the catalog loader refuses missing skills.

```yaml
defaultProfile: family-channel
profiles:
  - name: family-channel
    agent: lange-family
    subject: slack.t0123abc.u9xyz
    model: gpt-5.6-luna
    maxSteps: 8
    maxCapabilityCalls: 16
```

Authored profiles cannot set conversation scope; an old `scope` key is refused. Only `--smoke-conversation` selects broker-derived synthetic memory. Real chat scope belongs to the authenticated gateway, not the console. Do not reuse the gateway's UID or spoof its `via`.

### Model and data limits

The published model clients support a private ChatGPT subscription `--auth-file` (`chatgpt-auth.console.json` by default), or `--endpoint URL` plus optional `--api-key-env NAME` for OpenAI-compatible endpoints. A named but absent/empty bearer variable refuses **when a model turn is attempted**, not while browsing. The console credential must not share the gateway's rotating refresh-token family; explicit `--auth-file` overrides the shared-file guard at the operator's risk. The chart should reference a separately owned API-key Secret or isolated subscription credential state, never mount broker/gateway credentials. No model is contacted for catalog browsing, empty grants or idle. `--server-uid` defaults to local effective UID only for same-owner development; the separate console UID 65535 / broker UID 65532 deployment must pass 65532 and have IPC group 65534, without write access to the socket parent.

The UI opens the selected agent's Shell ready to type, with Agents / Shell / Capabilities tabs and one wrapped command/output/input transcript. PageUp/PageDown page visual rows even while typing; Home/End jump to top/bottom (usually Fn-arrow keys on compact Mac keyboards, if the terminal forwards them). Esc requests cooperative stop while busy. A failure opens its full report: the operation, subject, agent, profile, scope and socket, the whole `source()` chain, and the broker's refusal code and message verbatim. It is a full-screen pane without side borders, so a terminal selection copies clean text; `e` reopens the last one, it is logged at WARN (redirected stderr or OTLP), and it is printed again on quit. brokerd keeps the refusal reason (`attestation-denied`, `unmapped-subject`, `agent-denied`, `policy-error`) off the wire, so the report names the broker log event that carries it. The capabilities tab shows each command word's broker help page. There is no Turns or inference UI entry; underlying model/session machinery remains in the library. The retained session library uses the released shared prompt loop, mounted `read_skill`, safe `inspect_agent_config` and progress metadata; the shell UI uses broker-granted provider command words and help. Cooperative cancellation reaches manual shell commands but does **not** undo effects already accepted. The capability pane shows the current broker leg’s W3C trace ID; on submission the fresh leg adopts the console turn’s trace. Each manual command creates a fresh console root; shell, broker and provider spans inherit it through blocking work. Without export configured this is local context correlation only, not durable telemetry evidence. Provider stdin/stdout travel over broker stream descriptors, so bounded shell pipelines consume provider stdout directly rather than a buffered JSON result. Stderr diagnostics and nonzero exit status remain visible; proposal inspection reports pipe presence without inline stdin. Manual shell arguments/results appear in the terminal transcript; output and command text are control-sanitised. Fresh agent entry resets transcript, shell state and model history. Manual scripts are Dekopon's bounded interpreter, not OS shell. It does not synthesize inbound delivery, chat replies, production transcript replay, or conversation memory. `suggest_improvement` remains deliberately off (not enabled by the approved session contract); OpenRouter models are not wired into the console. Attached and provider-returned assets share a volatile 16-item / 64 MiB inventory for the selected agent entry. Their numeric `chat-asset:<N>` IDs can be used as later provider file inputs within that same entry, across separate shell submissions; each invocation remains bounded to five descriptors / 40 MiB. `:asset attach-base64 --type <mime> <BASE64>` stores decoded Identity bytes (1 MiB encoded / 512 KiB decoded limits). Re-entering or exiting resets the inventory; removal invalidates IDs. `send` cannot deliver to a chat transport. `--smoke-conversation` claims only the broker-derived synthetic scope for ordinary routes, and `:smoke record '<text>'` records a synthetic delivered turn; without that flag the local command refuses. A provider's own read command is used when deployed; there is no built-in `:smoke recent`. This is not real chat replay. Capacity/retention failures are reported after the effect executed and must not be retried automatically. No binary presentation or transport replay is provided. This release must not be marketed as full image or transport replay.

## Dependency / verification boundary

Both console crates are private workspace members with path dependencies on current core. `DEKOPON_CONFIG`/`--config` may name a catalog file or a directory of `*.yaml` agents (the chart's ConfigMap mount), with `instructionsFile` read beside each. To exercise the broker boundary without live requests, set `DEKOPON_TEST_BROKERD` to a separately verified broker executable and `DEKOPON_TEST_PROBE_WASM` to core's pure `cli-probe-provider.wasm`, then run `cargo test -p dekopon-tui --test broker --locked`. The test skips only when those variables are absent; a submitted compatibility claim must provide them and record the artifact/ref tested. It tests attested/denied/empty surfaces, UID pin, authenticated console real-scope refusal, provider help, pure read, ungranted call, typed secret-use denial, a provider-asset registration through the real broker, shell/model-script runtime exit/output for that asset, and a real nested child-script round trip through the console ShellRuntime. The asset fixture's CLI cannot express `assetMode`, so its test-only command-word parser proposes that input; the broker effect, descriptor and console leg are real. This does not prove the stock http-probe CLI can request an asset. The nested test uses an in-memory WAT component adapted from core tag v0.33.0's brokerd spawn fixture and runs only when `DEKOPON_TEST_BROKERD` is set; it asserts child stdout and status zero. This is a synthetic `printf child` provider, not the production Python/gh provider. Console 0.2.0 passed it against the released v0.22.0 `aarch64-apple-darwin` `dekopon-brokerd` with the v0.22.0 `cli-probe` and `http-probe` components. This proves the tested broker wire/fixture paths, not untested provider effects. Workspace CI builds the broker and supplies the checked component paths for these integration tests.

The [`dekopon-tui` crate](../dekopon-tui/README.md) owns rendering, transcript, and terminal state. Copyright licensed under [Apache-2.0](../../LICENSE-APACHE) or [MIT](../../LICENSE-MIT).


### Console telemetry

`--telemetry /etc/dekopon/console-telemetry.yaml` enables both OTLP traces and correlated logs.
No telemetry network/export occurs by default; `--idle` ignores this option and does not read
its file or initialize exporters. Interactive execution always maintains local OTel context so
broker proposals carry the actual turn trace even without console export. Diagnostics go only
to redirected stderr (never into the TUI); orderly quit/refusal flushes and shuts down exporters.

The strict YAML file is a console-owned serialization mirroring core daemon telemetry keys,
validated by published `dekopon_telemetry::ExporterSettings` (that type is not deserializable):

```yaml
endpoint: https://telemetry.example.invalid/otlp
transport: http                 # grpc (default) or http
serviceName: dekopon-console     # default
exportTimeoutMs: 5000            # default, positive; export and shutdown deadline
```

`endpoint` is required and must be a credential-free base URL without query/fragment/userinfo.
Unknown keys and invalid settings fail startup with safe diagnostics. Ingestion authentication
uses only the SDK’s existing `OTEL_EXPORTER_OTLP_HEADERS` environment convention, never YAML
or command-line credential values. Do not share gateway credential/state mounts. A future chart
lane may mount this small read-only file at the example path; no chart value/mount or deployment
is added here. Its configured receiver must be inside the operator trust boundary: telemetry
contains complete prompts, answers, scripts, arguments and results under the normal shared
core payload rules, excluding secret bytes and model/OTLP/provider credentials. There is no
metadata-only/payload suppression switch. The collector/broker must both export to reconstruct
the whole trace; no traceparent is sent to external model endpoints.

Model routing is persistently labelled `model (CLI override)`, `model (profile)`, or
`model (default)` on its own header row, including after agent/profile switches. An explicit
`--model` wins over authored profile models; this is not an exact reproduction of that profile.

The trace regression in `crates/dekopon-tui/tests/support/trace.rs` uses a loopback synthetic
OpenAI-compatible server, OTLP protobuf collector, real broker process and pure `cli-probe`
Wasm. With the fixture variables below, it checks two distinct turn traces, causal parentage
through model/script/broker/provider, full correlated synthetic payloads and credential exclusion.
It never calls a live model/provider. Fixture-less runs explicitly skip broker integration:

```sh
DEKOPON_TEST_BROKERD=/path/to/verified/dekopon-brokerd \
DEKOPON_TEST_PROBE_WASM=/path/to/verified/cli-probe-provider.wasm \
DEKOPON_TEST_ASSET_WASM=/path/to/verified/http-probe-provider.wasm \
cargo test --workspace --locked -- --test-threads=1
```
