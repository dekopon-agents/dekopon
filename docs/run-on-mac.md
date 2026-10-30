# Run Dekopon on a Mac

For an always-on Apple Silicon Mac, use the native release binaries and **launchd**, with a
separate user for each daemon. Start with a Slack Agent app, one explicitly selected OpenRouter
model, and a dedicated prepaid API key with an OpenRouter spending limit. This path needs no
local model, Docker, Kubernetes, Linux VM, or Firecracker.

**Status: released building blocks; manual setup.** The release and tap supply executables,
not a configured Mac service. There is no native installer or account-provisioning command. The
[harmless Slack Agent starter](../examples/mac-slack/README.md) has been checked with released binaries on Linux;
its actual Mac/Slack/OpenRouter acceptance remains an operator step. The broker requires an installed compatible provider, and the
gateway refuses a sender with no granted capability: a provider-free chat bot is not currently
supported. For useful GitHub work, follow the [Slack Agent review-channel guide](../examples/slack-github/README.md):
it maps an owner and colleague to explicit read profiles and keeps the PAT in the broker. Complete
the acceptance checks below before replacing an existing agent.

## Choose the process manager

| Method | Starts when | Fit for a home Mac |
|---|---|---|
| System launchd jobs, distinct daemon users | Boot; remains running after logout | Recommended for a persistent installation. Requires administrator setup of users, files and two plists. |
| User LaunchAgents | User login; ends at logout | Read-only local trial. Two processes under one login do not isolate broker files or credentials. |
| `brew services` | No Dekopon service is defined | The current formula installs binaries only. |
| Docker Desktop | When its Linux VM and Docker start | Adds a VM and container lifecycle to manage. Use the [container guide](container-image.md) only if you already operate that stack. |

Apple documents [launchd jobs and their login/boot lifecycle](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html).
Homebrew's [service commands](https://docs.brew.sh/Manpage#services-subcommand) wrap that manager;
they do not supply a missing service definition. No Mac RAM, idle-power or startup-time benchmark
is published here. Archive size is a download measurement, not resident memory.

## Install

The released Mac target is **Apple Silicon / ARM64** (`aarch64-apple-darwin`). Intel Macs have no
current archive or bottle; a source build is outside the release build matrix. Both daemons must
come from the same release. Use that tag's configuration and provider contracts: `main` may have
unreleased changes even when `--version` still prints the release number.

### Prebuilt archives

This is the service path used below. Stage a verified archive in a versioned,
operator-controlled directory so upgrades and rollback are deliberate. Release [0.72.0](https://github.com/dekopon-agents/dekopon/releases/tag/v0.72.0)
provides macOS ARM64 and Linux ARM64/x86-64 archives; the optimized-Wasmtime variants are Linux-only.
The standard archive also contains the optional console and reference documents, but no providers.

```console
gh release download v0.72.0 --repo dekopon-agents/dekopon \
  --pattern 'dekopon-0.72.0-aarch64-apple-darwin.tar.gz*'
shasum -a 256 -c dekopon-0.72.0-aarch64-apple-darwin.tar.gz.sha256
gh attestation verify --repo dekopon-agents/dekopon \
  dekopon-0.72.0-aarch64-apple-darwin.tar.gz
tar xzf dekopon-0.72.0-aarch64-apple-darwin.tar.gz
```

For the first installation, create the exact paths the service templates use:

```console
sudo install -d -o root -g wheel -m 0755 /usr/local/lib/dekopon/0.72.0
sudo install -o root -g wheel -m 0755 \
  dekopon-0.72.0-aarch64-apple-darwin/dekopon-brokerd \
  dekopon-0.72.0-aarch64-apple-darwin/dekopon-gatewayd \
  /usr/local/lib/dekopon/0.72.0/
test ! -e /usr/local/lib/dekopon/current && \
  test ! -L /usr/local/lib/dekopon/current && \
  sudo ln -s /usr/local/lib/dekopon/0.72.0 /usr/local/lib/dekopon/current
/usr/local/lib/dekopon/current/dekopon-brokerd --version
/usr/local/lib/dekopon/current/dekopon-gatewayd --version
```

The guard creates no link if `current` already exists, including a dangling symlink; follow the upgrade procedure
for an existing service. An administrator owns the directory and pointer; neither daemon may
replace the binaries. Keep the extracted reference documents with the release. Checksums prove
byte integrity and GitHub attestations establish build provenance; neither is an Apple signing or
notarization claim. If macOS blocks execution, inspect the exact Gatekeeper message and release
provenance rather than disabling Gatekeeper system-wide.

### Homebrew

```console
brew tap dekopon-agents/tap
brew trust dekopon-agents/tap
brew install dekopon-agents/tap/dekopon
dekopon-brokerd --version
dekopon-gatewayd --version
```

Homebrew 6 requires trust for a non-official tap. The maintained
[tap](https://github.com/dekopon-agents/homebrew-tap) tracks stable releases; check its
[formula](https://github.com/dekopon-agents/homebrew-tap/blob/main/Formula/dekopon.rb) and the
installed versions instead of assuming a successful release updated it. The formula installs
both daemons and their reference documents, but no provider, configuration or service. The
separate `dekopon-console` formula is optional. Do not run Homebrew itself as root.

Homebrew is an alternative to archive staging. To use its binaries with the service templates,
run `brew --prefix dekopon` and substitute that literal prefix plus `/bin/` for every
`/usr/local/lib/dekopon/current/` binary path, including the gateway wrapper. launchd does not
expand `$(brew --prefix dekopon)` or `~` in a plist. Account, configuration and directory setup is
still required, and the Homebrew prefix must not be writable by either daemon account.

### Manual Homebrew compatibility check

The [macOS compatibility workflow](../.github/workflows/macos-compat.yml) installs the real tap
on GitHub's ARM64 `macos-15` runner. It checks the installed release and Mach-O architecture,
then runs a minimal provider-backed turn with the installed broker and gateway, a pinned
JSONPlaceholder component, and loopback model/API fixtures. It uses no Slack, OpenRouter or
GitHub PAT credentials and does not build replacement daemon binaries.

Once that workflow exists on the repository's default branch, a maintainer can run it manually:

```console
gh workflow run macos-compat.yml --repo dekopon-agents/dekopon --ref main
gh run list --repo dekopon-agents/dekopon --workflow macos-compat.yml --event workflow_dispatch --limit 5
```

GitHub requires the workflow to be on the default branch before dispatch; selecting a draft
branch with `--ref` does not register a new workflow. This lane is manual, not a required PR
check. Its one-user, read-only process smoke does not prove separate-user permissions, launchd,
sleep/logout/reboot recovery, Gatekeeper behavior or live integrations. Complete those checks
below even after a green run. The [development instructions](development.md#macos-homebrew-compatibility)
also explain how to run the same fixture locally.

### From a checkout

Source installation is for operators prepared to maintain a Rust toolchain. Use the compiler
pinned in [`rust-toolchain.toml`](../rust-toolchain.toml), and check out the release tag:

```console
git clone https://github.com/dekopon-agents/dekopon.git
cd dekopon
git checkout v0.72.0
cargo install --locked --path crates/dekopon-brokerd
cargo install --locked --path crates/dekopon-gatewayd
```

Alternatively, `cargo install --locked --version 0.72.0 <daemon-name>` uses crates.io, if both
crates have finished publishing. Neither method installs providers or a service. The
[development guide](development.md) owns build and provider-fixture checks.

## Prepare Slack and the model

Use the [Slack Agent manifest from the release](https://github.com/dekopon-agents/dekopon/blob/v0.72.0/examples/slack/manifest-agent.yaml).
It enables Slack's Agent view and native Working/Stop session UI. The workspace plan and admin
policy must allow those features. It still uses **Socket Mode**, with the same two credentials;
Agent API support does not introduce a public listener or webhook URL.

The [Slack setup guide](../examples/slack/README.md#create-the-app-and-credentials) owns app
creation: create the app from this manifest, generate an `xapp-…` token with `connections:write`,
install it to the workspace, and obtain its `xoxb-…` bot token. For the
[GitHub review channel](../examples/slack-github/README.md), invite the bot to one channel and map
the owner, colleague and channel separately. Use a private channel whose members may see the
repository code and findings: invocation grants do not hide replies from other channel members,
and reviewed content also reaches the configured model. The [harmless DM starter](../examples/mac-slack/README.md)
can first prove the model and provider path without a GitHub PAT.

Pair the Agent manifest with `experience: agent`. Agent DMs and channel continuations are scoped
to threads; a new channel conversation requires a mention. A freshly authorized sender can
continue their owned thread without mentioning again. Another participant must mention and
pass their own authorization. Agent mode uses Slack's Stop control; `cancelButton: true` is
invalid for this profile. Stop is cooperative and cannot undo a GitHub effect already sent.

A permanent Agent-status error such as `feature_disabled` can fall back to a configured reaction;
that changes the indicator, not the thread semantics or workspace entitlement. Verify native
Working/Stop during acceptance. If the workspace cannot use the Agent profile, deliberately
choose the [classic manifest](../examples/slack/manifest.yaml) and `experience: classic` instead;
it is a separate installation choice, not promised Agent API parity.

Create a **dedicated OpenRouter key** with a finite USD limit and an explicit reset policy (for
example monthly, or no reset for a fixed trial allocation). Verify the saved key's limit and
usage in OpenRouter. Give the gateway an inference key, not a management key. Use prepaid credit,
keep automatic top-ups off for a finite trial, and avoid BYOK for this first installation: BYOK introduces a separate provider bill. Account credit is
shared across keys; a separate key makes this installation's usage and revocation identifiable.
See OpenRouter's [key limits](https://openrouter.ai/docs/api/api-reference/api-keys/create-a-new-api-key)
and [BYOK billing](https://openrouter.ai/docs/guides/overview/auth/byok).

The OpenRouter key limit controls requests billed through that key; separately billed provider
tools are outside it. Dekopon's optional meter counts **raw input plus
output tokens**, not dollars; it resets on restart without best-effort telemetry restore and can
settle above its reservation. Background memory summaries are not refused by that local meter.
OpenRouter's own [in-flight accounting](https://openrouter.ai/docs/api_reference/limits) also has
rollout and estimation limits, so do not promise an exact zero-overshoot cap. Leave headroom and
check the provider's usage page. No setting here changes a Claude or ChatGPT subscription quota.

Choose one exact OpenRouter model ID that supports tools, check its current price and availability,
and replace the model placeholder in the [starter](../examples/mac-slack/README.md). Do not start with an auto-router. A false
`allowFallbacks` disables provider failover but does not pin one provider; add `routing.only` only
after checking that provider's current slug and support. Model controls are forwarded requests,
not a guarantee that every upstream provider honors them.

Store these three values in a private file readable only by the gateway user, for example
`/usr/local/etc/dekopon/gateway/credentials.env`:

```sh
export DEKOPON_GATEWAYD_SLACK_APP_TOKEN='REPLACE_WITH_APP_TOKEN'
export DEKOPON_GATEWAYD_SLACK_BOT_TOKEN='REPLACE_WITH_BOT_TOKEN'
export OPENROUTER_API_KEY='REPLACE_WITH_DEDICATED_KEY'
```

Use mode `0600` under a `0700` gateway-owned directory. This is a shell file: keep the fixed
variable names, quote values, and source only your own file. Create it with an editor rather than
pasting secrets into a command that will remain in shell history. Do not put these values in
gateway configuration YAML, plists, agent instructions, a repository, or `launchctl setenv`.
launchd does not inherit your interactive shell's exports or automatically read `.env` files.
The gateway wrapper below explicitly sources this private shell file; its filename is not a
built-in dotenv feature. Provider credentials belong only to the broker.

### GitHub PAT under launchd

For GitHub tools, use the [review-channel guide's private credential file](../examples/slack-github/README.md#keep-the-github-pat-in-the-broker)
at `/usr/local/etc/dekopon/broker/broker-credentials.yaml`. The broker loads it through
`credentialsPath` in its own configuration and selects the symbolic `github-pat` binding for
permitted capabilities. Its native HTTP host adds the Authorization header for `api.github.com`;
the provider Wasm, gateway and model never receive the PAT. Both Slack users borrow this same
GitHub credential: Dekopon audits their distinct Slack identities, while GitHub sees the PAT
account. Their own GitHub logins are not independently checked.

This is intentionally a **private credentials YAML file containing the literal secret**, owned
by `_dekopon_broker`, mode `0600`, under its `0700` directory. It is separate from ordinary
broker configuration, which contains references. Use an editor to create it privately, not a
command line containing the token. The broker launchd plist below runs the binary directly and
needs no `EnvironmentVariables` entry or secret-loading wrapper.

Setting `GITHUB_TOKEN` in a plist or sourcing a broker `.env` does not populate this credential:
the released broker has no dotenv loader and this bearer-token field does not expand `${...}`.
Do not add the PAT to the gateway's `credentials.env`; that would give the wrong process the
secret. Keep the file out of Git and ordinary logs/backups. Rotate it privately, stop the gateway
then the broker, and restart broker then gateway to load the replacement. The review guide
separates read access from optional publishing permission; a broader PAT alone grants no
additional Dekopon capability.

## Prepare the broker and gateway

An administrator must first create distinct non-login accounts (the templates use
`_dekopon_broker` and `_dekopon_gateway`) and a shared `dekopon-ipc` group. Record their actual
numeric UIDs; do not copy Linux chart UIDs or assume the Mac login is UID 501. Both accounts need
membership in the IPC group. Dekopon does not create these accounts.

| Files | Owner and permissions |
|---|---|
| `/usr/local/etc/dekopon/broker/` and broker provider/state directories | Broker-owned `0700`; config, Cedar policy and private credentials `0600`. Providers must also be broker-owned single-link files with protected parents. |
| `/usr/local/etc/dekopon/gateway/` and gateway state directory | Gateway-owned `0700`; gateway YAML, catalog and credential file `0600`. |
| `/usr/local/var/run/dekopon/` | Broker-owned, group `dekopon-ipc`, mode `0710`; the broker creates its socket as `0660`. No group write permission. |
| Separate broker/gateway log directories | Owned by their respective daemon, `0700`; log files `0600`. |
| Executables, wrappers and `/Library/LaunchDaemons/*.plist` | Administrator-controlled; not writable by either daemon. Plists conventionally `root:wheel`, `0644`. |

Use regular files, not symlinks or hard links, for configuration and provider artifacts. Unsafe
path ancestors also cause startup refusals. Keep the IPC directory separate from private data.
The [broker file and socket contracts](../crates/dekopon-brokerd/README.md#ipc-directory-and-distinct-peer-uids)
are authoritative. Do not solve a permission failure with `chmod -R 777` or by running both
daemons as root.

Install a reviewed, compatible provider from its own release and give the selected sender one
narrow read capability for the first test. Follow the
[provider lifecycle](../crates/dekopon-brokerd/README.md#managed-provider-sets) or use a direct Wasm
path under the broker's private directory. Direct paths compile at each broker boot; managed
stores require sync/precompile for the selected host. Do not copy an old bundled provider or a
compiled cache from another CPU/release. The core archive and tap contain no starter provider.

Choose one complete example: the [Slack Agent review channel](../examples/slack-github/README.md)
for GitHub tools and owner/colleague profiles, or the [harmless DM starter](../examples/mac-slack/README.md)
using JSONPlaceholder before adding a PAT. Each supplies broker configuration, Cedar policy,
an agent catalog and budgeted OpenRouter gateway configuration. Replace its numeric UIDs,
Slack team/member IDs and model placeholder before checking it. The peer principal must be
exactly `dekopon-gatewayd`; another name can pass config checks and still fail gateway boot.
The broker also maps its own UID for the operator probe.

The harmless starter selects Slack Agent DMs, one concurrent session, bounded tool/output/history
limits, and a rolling token meter. The review-channel example scopes routes and grants to one channel. It keeps recent text only in memory; restart loses it.
Attachments, journal summaries, follow, ambient triggers, wakes, detached jobs and the guest model
proxy are not enabled. Its numeric bounds are a starting point, not a price estimate. Add durable
history only after choosing retention and accounting for its additional inference. The complete
model and metering contracts live in [gatewayd](gatewayd.md#openrouter-model-settings).

## Check before installing a service

Install an administrator-owned executable wrapper at `/usr/local/libexec/dekopon-gateway`.
This same entry point is used in the foreground and by launchd:

```sh
#!/bin/sh
set -eu
umask 077
. /usr/local/etc/dekopon/gateway/credentials.env
exec /usr/local/lib/dekopon/current/dekopon-gatewayd \
  --config /usr/local/etc/dekopon/gateway/gateway.yaml
```

Run each check **as that daemon's intended user**, with the same paths it will use under launchd:

```console
sudo -u _dekopon_broker /usr/local/lib/dekopon/current/dekopon-brokerd \
  check /usr/local/etc/dekopon/broker/broker.yaml
sudo -u _dekopon_gateway /usr/local/lib/dekopon/current/dekopon-gatewayd \
  check /usr/local/etc/dekopon/gateway/gateway.yaml
```

A managed provider configuration also needs the check's `--provider-set` and `--store` arguments.
Checks validate authored input; they do not establish runtime credentials, socket ownership,
Slack installation, or model reachability. A gateway `check --probe` makes a billable model
request and has its own [probe contract](gatewayd.md#check-a-configuration).

Start the broker in the foreground, wait for startup, then probe its socket as the broker user:

```console
sudo -u _dekopon_broker /usr/local/lib/dekopon/current/dekopon-brokerd \
  --config /usr/local/etc/dekopon/broker/broker.yaml
```

In another terminal:

```console
sudo -u _dekopon_broker /usr/local/lib/dekopon/current/dekopon-brokerd \
  probe --socket /usr/local/var/run/dekopon/broker.sock
```

The probe pins the server to its own effective UID; it is an owner health check, not a
cross-UID gateway test. A successful probe invokes nothing and does not prove a Slack sender is
permitted. Start the gateway in a third terminal:

```console
sudo -u _dekopon_gateway /usr/local/libexec/dekopon-gateway
```

Send a DM from the mapped sender, and ask it to perform the chosen harmless provider read. Confirm a
reply, the expected model usage in OpenRouter, and the broker decision in the logs. Confirm another
unmapped sender gets no model call. Stop the gateway first, then the broker.

## Run with launchd

Use these two property lists under `/Library/LaunchDaemons/`. Create all directories and log
files with the ownership above first. `KeepAlive` restarts an exited process; use `bootout` for
an intentional stop. Set `ExitTimeOut` longer than the configured shutdown grace; increase the
example's 150 seconds if either daemon needs more time to drain.

`dev.dekopon.broker.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.dekopon.broker</string>
  <key>ProgramArguments</key><array>
    <string>/usr/local/lib/dekopon/current/dekopon-brokerd</string>
    <string>--config</string><string>/usr/local/etc/dekopon/broker/broker.yaml</string>
  </array>
  <key>UserName</key><string>_dekopon_broker</string>
  <key>GroupName</key><string>dekopon-ipc</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>ExitTimeOut</key><integer>150</integer>
  <key>Umask</key><integer>63</integer>
  <key>StandardOutPath</key><string>/usr/local/var/log/dekopon-broker/stdout.log</string>
  <key>StandardErrorPath</key><string>/usr/local/var/log/dekopon-broker/stderr.log</string>
</dict></plist>
```

`dev.dekopon.gateway.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>dev.dekopon.gateway</string>
  <key>ProgramArguments</key><array><string>/usr/local/libexec/dekopon-gateway</string></array>
  <key>UserName</key><string>_dekopon_gateway</string>
  <key>GroupName</key><string>dekopon-ipc</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>30</integer>
  <key>ExitTimeOut</key><integer>150</integer>
  <key>Umask</key><integer>63</integer>
  <key>StandardOutPath</key><string>/usr/local/var/log/dekopon-gateway/stdout.log</string>
  <key>StandardErrorPath</key><string>/usr/local/var/log/dekopon-gateway/stderr.log</string>
</dict></plist>
```

Validate and load the broker. Wait for a successful probe before loading the gateway:

```console
plutil -lint /Library/LaunchDaemons/dev.dekopon.broker.plist \
  /Library/LaunchDaemons/dev.dekopon.gateway.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/dev.dekopon.broker.plist
sudo -u _dekopon_broker /usr/local/lib/dekopon/current/dekopon-brokerd \
  probe --socket /usr/local/var/run/dekopon/broker.sock
sudo launchctl bootstrap system /Library/LaunchDaemons/dev.dekopon.gateway.plist
sudo launchctl print system/dev.dekopon.broker
sudo launchctl print system/dev.dekopon.gateway
```

At boot launchd can start either job first; a plist does not express dependency readiness. The
gateway retries a missing/refusing broker connection within `broker.ioTimeoutMs`. A boot failure
exits the gateway and `KeepAlive` restarts it after throttling; a failed request in an already
running gateway can be tried again. Check that a cold boot with provider compilation eventually
serves a DM.
The templates do not claim a tested macOS service installer; run `plutil`, permission checks and
the boot/logout checks on the target Mac.

To stop, remove the gateway job before the broker job, and wait for each to exit:

```console
sudo launchctl bootout system/dev.dekopon.gateway
sudo launchctl bootout system/dev.dekopon.broker
```

`bootout` unloads the running job; files left in `/Library/LaunchDaemons` load again at boot. To
keep a service off across reboots, move its plist out of that directory after stopping it. Never
start a second foreground gateway while the service is running.

## Sleep, logs, updates and recovery

On a Mac desktop, enable **System Settings → Energy → Prevent automatic sleeping when the display
is off**, as described by [Apple](https://support.apple.com/guide/mac-help/set-sleep-and-wake-settings-mchle41a6ccd/mac).
The display may sleep; a sleeping computer cannot answer Slack. `KeepAlive` restarts a process,
not a sleeping Mac. Reboot, logout, network recovery and sleep/wake are separate acceptance checks.
FileVault may require an unlock before boot proceeds; do not disable disk encryption to obtain
unattended startup. Power-loss restart behavior also needs a test on the actual Mac.

Read `launchctl print` and each daemon's stderr when a job repeatedly exits. JSON stdout carries
broker audit and gateway diagnostics; prompts and outputs may reach logs/telemetry, so keep them
private. launchd's file redirection does not bound log growth. Before leaving it unattended,
choose a size/retention policy; a simple maintenance procedure is to stop both jobs, archive or
remove old logs with their private permissions preserved, then restart broker and gateway. An
external log collector is optional and adds its own resource and retention cost. There is no
separate durable local audit archive.

Keep configurations, Cedar policies, provider locks/components, and any configured persistent
storage in private backups. Exclude tokens from ordinary repository backups. Preserve separately
protected credential recovery material or be prepared to issue fresh credentials. This starter
has no journal; adding one creates chat text and attachment state that needs its own retention
and backup policy. Restoring old configuration never undoes an external effect.

For an upgrade, read [upgrading](upgrading.md), save the current binaries/configuration/provider
lock, stage and check the candidate pair, stop gateway then broker, switch both binaries, start
and probe broker, then start gateway and send a real DM. Homebrew replaces installed binaries,
not already-running processes or configuration; an automatic upgrade is not a verified rollout.
For rollback use the retained binary **pair and matching configuration/provider state**, not an
old binary against a newly migrated store. There is no universal reverse data migration. Do not
run both releases concurrently, and inspect uncertain external effects before retrying.

If a credential is revoked or the key limit is reached, inspect the OpenRouter/Slack account and
the credential variable names in startup errors; do not paste token values into reports. Fix the
private credential file, restart the gateway, and recheck usage. A restart does not replenish
OpenRouter credit, even though it resets an unrestored local token meter. Stop the gateway first
when diagnosing unexpected spend.

## Acceptance before switching

1. Check the exact installed pair, configuration, provider compatibility and cross-UID socket;
   prove a permitted Slack DM and a denied sender, then inspect OpenRouter usage and the saved
   key limit. Use a small trial allocation to exercise the limit/refusal path.
2. Prove service restart, logout and full reboot recovery on the actual Mac; verify network
   recovery and that the machine remains awake. Inspect logs and disk use after an overnight run.
3. Keep the previous system's configuration and a rollback path. Stop its message consumer before
   enabling Dekopon as the primary bot, so only one system consumes that route. No credentials,
   conversations, skills or permissions are imported automatically; review each grant explicitly.

The release workflow builds the Mac binaries, but Linux checks and plist parsing do not prove
these Mac acceptance steps. macOS also lacks the Linux-only address-space cap on the `jq` worker;
see [shell bounds](../crates/dekopon-shell/README.md#sandboxing). Adding a provider does not give a
Mac Firecracker/KVM or arbitrary host-shell support. Keep advanced deployment, security and
provider references; a coding assistant needs their contracts to configure this path correctly.

For a same-user, read-only local experiment, the [local transport contract](gatewayd.md#local-development-transport)
is still available. It trusts the caller's declared subject and provides no separate-UID
isolation. For subscription authentication instead of the API-key path, use the
[auth CLI](cli.md) and [credential lifecycle](chatgpt-credential.md).
