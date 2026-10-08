# Run Dekopon on a Mac

Dekopon runs natively on macOS as two Unix processes: `dekopon-brokerd` authorizes provider
calls, and `dekopon-gatewayd` runs the model and chat transports. Kubernetes, Docker, and a Slack workspace
are not needed for a local experiment. For a cluster deployment, use [Kubernetes](kubernetes.md).

The release archives and Homebrew tap support **Apple Silicon (ARM64)** Macs. There is no current
Intel Mac archive or bottle; an Intel Mac needs a [source build](#from-a-checkout), which is not
covered by the release build matrix. Both daemons must come from the same release or checkout.

## Install

### Homebrew

```console
brew tap dekopon-agents/tap
brew trust dekopon-agents/tap
brew install dekopon
dekopon-brokerd --version
dekopon-gatewayd --version
```

Homebrew 6 requires `brew trust` before loading a non-official tap. The
[tap](https://github.com/dekopon-agents/homebrew-tap) is regenerated from each release's actual
archives; it also supports Linux on ARM64 and x86-64.

The 0.32.0 formula installs both executables and `BROKER.md` / `GATEWAY.md` under
`$(brew --prefix dekopon)/share/doc/dekopon/`. It installs no provider, starts neither daemon,
generates no configuration, and defines no `brew services` service. The
[local walkthrough](#run-a-read-only-local-session) below uses the *older 0.30.0 release* and its
bundled provider; do not combine that component or configuration with 0.32.0.

### Prebuilt archives

Each [GitHub release](https://github.com/dekopon-agents/dekopon/releases) carries three
provenance-attested archives: macOS ARM64, Linux ARM64, and Linux x86-64. Each includes both
daemons, licences, and broker/gateway reference documents in 0.32.0, with a
`.sha256` sidecar beside each. This example selects 0.32.0 deliberately (run it only after publication):

```console
gh release download v0.32.0 --repo dekopon-agents/dekopon \
  --pattern 'dekopon-0.32.0-aarch64-apple-darwin.tar.gz*'
shasum -a 256 -c dekopon-0.32.0-aarch64-apple-darwin.tar.gz.sha256
gh attestation verify --repo dekopon-agents/dekopon \
  dekopon-0.32.0-aarch64-apple-darwin.tar.gz
tar xzf dekopon-0.32.0-aarch64-apple-darwin.tar.gz
export PATH="$PWD/dekopon-0.32.0-aarch64-apple-darwin:$PATH"
```

Use the configuration contracts and examples from the same tag as the binaries; a checkout of
`main` can describe changes that are not yet in an archive. A matching printed version alone does
not prove a source build is the release: development commits can still carry that version.

### crates.io

The application release publishes the workspace's crates in dependency order through crates.io
trusted publishing. Install both daemons at the same available version:

```console
cargo install --locked --version 0.41.0 dekopon-brokerd
cargo install --locked --version 0.41.0 dekopon-gatewayd
```

A crate publication can trail the Git tag or stop partway; use the tap or release archives if the
selected version is unavailable. This installs executables only, not provider components. Release
recovery belongs to the [maintainer release process](../README.md#maintainer-release-process).

### From a checkout

Install Rust with `rustup` and use the compiler pinned by [`rust-toolchain.toml`](../rust-toolchain.toml),
also the MSRV (edition 2024):

```console
git clone https://github.com/dekopon-agents/dekopon.git
cd dekopon
cargo install --locked --path crates/dekopon-brokerd
cargo install --locked --path crates/dekopon-gatewayd
dekopon-brokerd --version
dekopon-gatewayd --version
```

That builds `main`. For a release build, check out its tag before the `cargo install` commands.
The [development guide](development.md) owns source checks and provider-fixture builds.

## Run a read-only local session

This is a **same-user development setup**, with one explicitly granted public post-read capability.
It does not isolate the gateway from the broker's files or process identity: another process under
your UID can act as that peer. Do not add production credentials or external-write grants to this
layout. Those require separate runtime UIDs and the
[current local process boundary](security-model.md#current-local-process-boundary).

The configuration below uses the `capabilities` format supported by release **0.30.0**, not the
retired `constraintSets` format. You need a running OpenAI-compatible model endpoint that supports
tool calls, or the [ChatGPT subscription alternative](#use-a-chatgpt-subscription). The local
endpoint in the example is not installed or started by Dekopon.

### Prepare private files

Run these commands in one terminal. They create a dedicated directory outside the checkout. If it
already contains a setup you want to keep, choose another directory before writing the files.

```console
umask 077
mkdir -p "$HOME/.dekopon-demo/providers" "$HOME/.dekopon-demo/run"
chmod 700 "$HOME/.dekopon-demo" "$HOME/.dekopon-demo/providers" "$HOME/.dekopon-demo/run"
cd "$HOME/.dekopon-demo"
DEMO_DIR=$(pwd -P)
DEMO_UID=$(id -u)
install -m 600 "$(brew --prefix dekopon)/share/dekopon/providers/jsonplaceholder-provider.wasm" \
  "$DEMO_DIR/providers/jsonplaceholder-provider.wasm"
```

For an archive install, replace the `install` command's source with the extracted archive's
`providers/jsonplaceholder-provider.wasm`. For a source build, fetch the exact fixture from that
checkout with `ci/fetch-external-provider-components.sh examples/providers jsonplaceholder`, then
copy it here. Copy the component rather than symlinking it: the broker requires a regular,
single-link, broker-owned file and a broker-owned parent directory. `sudo` is not needed here.

Create the broker configuration and Cedar policy:

```console
cat > broker.yaml <<EOF_BROKER
apiVersion: dekopon.dev/brokerd/v1alpha1
socketPath: "$DEMO_DIR/run/broker.sock"
policiesPath: policies.cedar
providers: [providers/jsonplaceholder-provider.wasm]
identities:
  - uid: $DEMO_UID
    principal: dekopon-gatewayd
    actor: {type: service, principal: dekopon-gatewayd}
    attestor: {namespaces: [tel.16034700182]}
principals:
  local-user:
    subjects: [tel.16034700182]
capabilities:
  jsonplaceholder:
    capabilities:
      jsonplaceholder.posts.get:
        constraints:
          timeoutMs: 10000
          http:
            allowedHosts: [jsonplaceholder.typicode.com]
            allowedMethods: [GET]
            maxRequests: 1
            maxRequestBytes: 16384
            maxResponseBytes: 262144
EOF_BROKER
# Provider output is bounded by hostLimits.maxOutputBytes in broker configuration.
cat > policies.cedar <<'EOF_POLICY'
@id("local-user-may-prompt-reader")
permit(principal == Dekopon::Principal::"local-user",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"local-reader")
when { context.via == "dekopon-gatewayd" };
@id("local-reader-may-get-posts")
permit(principal == Dekopon::Principal::"local-user",
       action == Dekopon::Action::"jsonplaceholder.posts.get",
       resource == Dekopon::Provider::"jsonplaceholder")
when { context.via == "dekopon-gatewayd" && context.agent == "local-reader" };
EOF_POLICY
```

The subject is a development fixture, not proof of a telephone identity. The owner-only local
transport trusts the caller to declare it. The broker maps that exact subject and grants only
`agent.prompt` and `jsonplaceholder.posts.get`; the provider's separate create capability is absent.

Create an agent catalog and gateway configuration:

```console
cat > catalog.yaml <<'EOF_CATALOG'
apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata: {name: local-reader}
spec:
  description: Read public JSONPlaceholder posts
  enabled: true
  instructions: Use placeholder posts get to answer questions about public posts.
  modelClass: reasoning
EOF_CATALOG
cat > gateway.yaml <<EOF_GATEWAY
apiVersion: dekopon.dev/gatewayd/v1alpha1
catalogPath: catalog.yaml
broker:
  socketPath: "$DEMO_DIR/run/broker.sock"
  serverUid: $DEMO_UID
transports:
  - name: dev
    kind: local
    socketPath: "$DEMO_DIR/run/chat.sock"
models:
  - name: local-model
    kind: openaiCompatible
    endpoint: http://127.0.0.1:11434/v1
    model: replace-with-your-model
    timeoutMs: 120000
    classes: [reasoning]
routes:
  - transport: dev
    conversation: {kind: [directMessage]}
    agent: local-reader
    limits: {maxSteps: 4, maxCapabilityCalls: 4}
EOF_GATEWAY
chmod 600 broker.yaml policies.cedar catalog.yaml gateway.yaml
```

Edit `gateway.yaml` to name the model your endpoint serves and its actual URL. No API key is needed
when the local endpoint does not require one. If it does, set `apiKeyEnv` to the name of an
environment variable available only to the gateway; never put the key in the YAML. Relative file
paths resolve against the configuration file's directory. See the complete
[broker configuration contract](../crates/dekopon-brokerd/README.md#configuration) and
[gateway configuration contract](gatewayd.md#configuration) for other fields.

### Check, start, and send a message

Before starting either daemon:

```console
dekopon-brokerd check "$HOME/.dekopon-demo/broker.yaml"
dekopon-gatewayd check "$HOME/.dekopon-demo/gateway.yaml"
```

These are preflight checks, not a connection test. They do not prove runtime socket permissions,
model reachability, credentials, or a successful provider call. Start the broker in one terminal:

```console
dekopon-brokerd --config "$HOME/.dekopon-demo/broker.yaml"
```

In a second terminal, wait for its startup to finish, then probe it and start the gateway:

```console
dekopon-brokerd probe --socket "$HOME/.dekopon-demo/run/broker.sock"
dekopon-gatewayd --config "$HOME/.dekopon-demo/gateway.yaml"
```

A successful probe exits silently with status 0. It authenticates and lists capabilities; it
invokes nothing. The gateway requires the broker to be serving before it starts.

In a third terminal:

```console
nc -U "$HOME/.dekopon-demo/run/chat.sock"
```

Type one JSON line and press Return:

```json
{"subject":"tel.16034700182","text":"Use placeholder posts get --post-id 1 and summarize the post."}
```

The socket returns a JSON `reply` on the same connection. This request sends your prompt to the
configured model, and the authorized provider read contacts `jsonplaceholder.typicode.com`.
An unauthorized reply usually means the subject, agent, attestor, capability, or Cedar grant does
not match; inspect both daemons' logs. For a provider failure, also check outbound HTTPS access.
Stop the gateway with Ctrl-C before stopping the broker. Graceful shutdown drains rather than
rolling back an effect. See [operations](operations.md) and the
[local transport contract](gatewayd.md#local-development-transport).

### Use a ChatGPT subscription

Instead of running a local model, authenticate into Dekopon's isolated credential file:

```console
dekopon-gatewayd auth chatgpt login
dekopon-gatewayd auth chatgpt status
```

Replace the gateway's `models` list with:

```yaml
models:
  - name: subscription
    kind: chatgptSubscription
    model: gpt-5-codex
    timeoutMs: 120000
    classes: [reasoning]
```

Run the gateway as the same user who logged in, so it resolves the same credential file. Keep that
file and its writable parent private; refresh rotates the credential through a temporary sibling
and rename. Do not export it to run locally or reuse its refresh-token family in another deployment.
The [auth CLI](cli.md) and [credential lifecycle](chatgpt-credential.md) describe path selection,
rotation, and intentional export for a cluster.

## Next steps and limits

- [Slack](../examples/slack/README.md), [Discord](../examples/discord/README.md), and
  [WhatsApp](../examples/whatsapp/README.md) keep their transport-specific app setup. Configure the
  real authenticated subject and broker grant before replacing the local transport.
- The [conditional writer](../examples/conditional-write/README.md) remains the credential-holding,
  conditional-write walkthrough. It requires a matching source checkout's `http-probe` fixture,
  Slack setup, and your own API endpoint and token. The JSONPlaceholder component bundled with
  the 0.30.0 Homebrew formula does not supply that example; formulas since 0.31.0 bundle no provider.
- The [OpenObserve example](../examples/otel-traces/README.md) adds a receiver and a real-daemon smoke
  test. Telemetry is optional for local startup; without a receiver, audit lasts only as long as
  you retain stdout. Prompts and outputs can reach telemetry; secret bytes must not.
- macOS runs the `jq` worker subprocess and its deadline/cancellation handling, but its Linux
  address-space cap is Linux-only. Do not infer that memory limit or production sandboxing from
  a successful Mac startup. See the [shell bounds](../crates/dekopon-shell/README.md#sandboxing).
- Review [upgrading](upgrading.md) before changing releases, keep the two daemons in lockstep, and
  restart the broker first and stop it last. Homebrew upgrades replace binaries, not running
  processes or your configuration.

The release workflow builds macOS binaries; that is not evidence of an end-to-end Mac deployment.
The local runtime, selected model, and provider path still need to be exercised on your Mac.
