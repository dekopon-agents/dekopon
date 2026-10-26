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

The formula installs both executables and `BROKER.md` / `GATEWAY.md` under
`$(brew --prefix dekopon)/share/doc/dekopon/`. It installs no provider, starts neither daemon,
generates no configuration, and defines no `brew services` service. Check the installed version
before choosing configuration and provider artifacts; Homebrew follows the tap rather than pinning
the release used by the archive commands below.

### Prebuilt archives

Release [0.57.0](https://github.com/dekopon-agents/dekopon/releases/tag/v0.57.0) provides standard
archives for macOS ARM64, Linux ARM64, and Linux x86-64, plus separate optimized-Wasmtime Linux
archives. Each standard archive includes both daemons, licences, and broker/gateway reference
documents, with a `.sha256` sidecar. The Mac example selects the standard 0.57.0 archive:

```console
gh release download v0.57.0 --repo dekopon-agents/dekopon \
  --pattern 'dekopon-0.57.0-aarch64-apple-darwin.tar.gz*'
shasum -a 256 -c dekopon-0.57.0-aarch64-apple-darwin.tar.gz.sha256
gh attestation verify --repo dekopon-agents/dekopon \
  dekopon-0.57.0-aarch64-apple-darwin.tar.gz
tar xzf dekopon-0.57.0-aarch64-apple-darwin.tar.gz
export PATH="$PWD/dekopon-0.57.0-aarch64-apple-darwin:$PATH"
```

Use the configuration contracts and examples from the same tag as the binaries; a checkout of
`main` can describe changes that are not yet in an archive. A matching printed version alone does
not prove a source build is the release: development commits can still carry that version.

### crates.io

The application release publishes the workspace's crates in dependency order through crates.io
trusted publishing. Install both daemons at the same available version:

```console
cargo install --locked --version 0.57.0 dekopon-brokerd
cargo install --locked --version 0.57.0 dekopon-gatewayd
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
git checkout v0.57.0
cargo install --locked --path crates/dekopon-brokerd
cargo install --locked --path crates/dekopon-gatewayd
dekopon-brokerd --version
dekopon-gatewayd --version
```

That builds the selected release tag. Use `main` only when deliberately testing development code.
The [development guide](development.md) owns source checks and provider-fixture builds.

## Prepare a local session

Installing the executables is not a configured local deployment. Prepare these inputs using the
contracts from the same release as the binaries:

- A compatible provider component or digest-pinned managed provider set, acquired separately.
  Follow the [provider lifecycle](../crates/dekopon-brokerd/README.md#managed-provider-sets).
  Neither the current formula nor the release archive supplies JSONPlaceholder. Do not copy a
  provider from the old 0.30.0 bundle into a current installation or assume a test fixture is a
  current starter component.
- Broker configuration, an owner-authored Cedar policy and a principal mapping granting only the
  intended read capability. The [broker reference](../crates/dekopon-brokerd/README.md#configuration)
  owns file permissions, provider loading and limits.
- An agent catalog and gateway configuration with a `local` transport, matching broker socket and
  server UID, and a model endpoint supporting tool calls. The
  [gateway reference](gatewayd.md#configuration) owns the schema; the
  [local transport contract](gatewayd.md#local-development-transport) owns the JSON message format.
  Dekopon does not install or start an inference server.

A same-user local experiment does not isolate the gateway from the broker's files or process
identity. The local transport trusts its caller's declared subject; it does not authenticate a
telephone identity. Keep this layout read-only and free of production provider credentials.
External-write deployments require the [separate-UID process boundary](security-model.md#current-local-process-boundary).
Relative file paths resolve against the configuration file's directory. Credential fields name
references or environment variables, never values; see [secrets](secrets.md).

Once your configuration files exist, check them before serving:

```console
dekopon-brokerd check "$HOME/.dekopon-demo/broker.yaml"
dekopon-gatewayd check "$HOME/.dekopon-demo/gateway.yaml"
```

These checks do not prove socket permissions, model reachability or successful provider calls.
Start the broker first; after its startup completes, probe it before starting the gateway:

```console
dekopon-brokerd --config "$HOME/.dekopon-demo/broker.yaml"
```

In a second terminal:

```console
dekopon-brokerd probe --socket "$HOME/.dekopon-demo/run/broker.sock"
dekopon-gatewayd --config "$HOME/.dekopon-demo/gateway.yaml"
```

Use the socket path you configured in both files if it differs from the example. A successful
probe authenticates and lists capabilities but invokes nothing. A real local message through the
configured model and provider is a separate acceptance check. Stop the gateway before the broker;
graceful shutdown does not roll back effects already sent to an endpoint.

This page does not provide a verified end-to-end 0.57.0 Mac starter. The former 0.30.0 walkthrough
relied on a bundled provider that current installs do not contain; replacing only its version
numbers would not establish compatibility.

### Use a ChatGPT subscription

Instead of running a local model, authenticate into Dekopon's isolated credential file:

```console
dekopon-gatewayd auth chatgpt login
dekopon-gatewayd auth chatgpt status
```

For a subscription-backed model, the gateway's `models` entry has this shape:

```yaml
models:
  - name: subscription
    kind: chatgptSubscription
    model: replace-with-your-subscription-model
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
  Slack setup, and your own API endpoint and token. It is not a credential-free Mac starter;
  current formulas bundle no provider.
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
