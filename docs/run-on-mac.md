# Run Dekopon on a Mac

For an always-on Apple Silicon Mac, use the native release binaries and **launchd**, with a
separate user for each daemon. Start with a Slack Agent app, one explicitly selected OpenRouter
model, and a dedicated prepaid API key with an OpenRouter spending limit. This path needs no
local model, Docker, Kubernetes, Linux VM, or Firecracker. Both launchd jobs run the binaries
directly: the gateway loads its Slack and OpenRouter secrets from a private file with
[`--env-file`](gatewayd.md#environment-file), and the broker keeps the GitHub PAT in its own
credentials file.

**Status: released building blocks; manual setup.** The release and tap supply executables,
not a configured Mac service. There is no native installer or account-provisioning command. The
[harmless Slack Agent starter](../examples/mac-slack/README.md) passes the released 0.74.0 configuration checks;
its actual Mac/Slack/OpenRouter acceptance remains an operator step. The broker requires an installed compatible provider, and the
gateway refuses a sender with no granted capability: a provider-free chat bot is not currently
supported. For useful GitHub work, use the [Slack Agent review-channel guide](../examples/slack-github/README.md):
it maps an owner and a colleague to broker principals in one reviewer group with Cedar read grants,
and keeps the PAT in the broker. Work through this page in order, taking the provider and
configuration files from one example at [Prepare the broker and gateway](#prepare-the-broker-and-gateway),
and complete the acceptance checks below before replacing an existing agent.

## Choose the process manager

| Method | Starts when | Fit for a home Mac |
|---|---|---|
| System launchd jobs, distinct daemon users | Boot; remains running after logout | Recommended for a persistent installation. Requires administrator setup of users, files and two plists. |
| User LaunchAgents | User login; ends at logout | Cannot run the examples: both daemons would share one UID, and the broker refuses two identities with the same UID. |
| `brew services` | Not used | A formula declares one service, run as the invoking user or one `--sudo-service-user`; Dekopon needs two daemons under two accounts, so the formula defines none. |
| Docker Desktop | When its Linux VM and Docker start | Adds a VM and container lifecycle to manage. Use the [container guide](container-image.md) only if you already operate that stack. |

Apple documents [launchd jobs and their login/boot lifecycle](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html).
No Mac RAM, idle-power or startup-time benchmark is published here. Archive size is a download
measurement, not resident memory.

## Install

The released Mac target is **Apple Silicon / ARM64** (`aarch64-apple-darwin`). The 0.74.0
binaries declare macOS 11 as their minimum but have been checked only on macOS 15; use Sequoia 15
or later. Intel Macs have no current archive or bottle; a source build is outside the release
build matrix. Both daemons must come from the same release. Use that tag's configuration and
provider contracts: `main` may have unreleased changes even when `--version` still prints the
release number.

You need an administrator account for `sudo`, and the [GitHub CLI](https://cli.github.com), which
macOS does not include, signed in with `gh auth login`: it refuses the release, attestation and
provider downloads here and in the examples without a GitHub account. Only the Homebrew path needs
[Homebrew](https://brew.sh).

### Prebuilt archives

This is the service path used below. Stage a verified archive in a versioned,
operator-controlled directory so upgrades and rollback are deliberate. Release [0.74.0](https://github.com/dekopon-agents/dekopon/releases/tag/v0.74.0)
provides macOS ARM64 and Linux ARM64/x86-64 archives; the optimized-Wasmtime variants are Linux-only.
The standard archive also contains the optional console and reference documents, but no providers.

```console
gh release download v0.74.0 --repo dekopon-agents/dekopon \
  --pattern 'dekopon-0.74.0-aarch64-apple-darwin.tar.gz*'
shasum -a 256 -c dekopon-0.74.0-aarch64-apple-darwin.tar.gz.sha256
gh attestation verify --repo dekopon-agents/dekopon \
  dekopon-0.74.0-aarch64-apple-darwin.tar.gz
tar xzf dekopon-0.74.0-aarch64-apple-darwin.tar.gz
```

For the first installation, create the exact paths the service templates use:

```console
sudo install -d -o root -g wheel -m 0755 /usr/local/lib/dekopon/0.74.0
sudo install -o root -g wheel -m 0755 \
  dekopon-0.74.0-aarch64-apple-darwin/dekopon-brokerd \
  dekopon-0.74.0-aarch64-apple-darwin/dekopon-gatewayd \
  /usr/local/lib/dekopon/0.74.0/
test ! -e /usr/local/lib/dekopon/current && \
  test ! -L /usr/local/lib/dekopon/current && \
  sudo ln -s /usr/local/lib/dekopon/0.74.0 /usr/local/lib/dekopon/current
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
brew install dekopon-agents/tap/dekopon
brew pin dekopon
dekopon-brokerd --version
dekopon-gatewayd --version
```

Since Homebrew 6.0, Homebrew loads a non-official tap's code only after trust. Installing the
fully qualified name taps `dekopon-agents/tap` and trusts only this formula, for your user only.
`brew pin` keeps a routine `brew upgrade` from replacing the binaries under the services; upgrade
only inside the [upgrade procedure](#sleep-logs-updates-and-recovery). The maintained
[tap](https://github.com/dekopon-agents/homebrew-tap) tracks stable releases; check its
[formula](https://github.com/dekopon-agents/homebrew-tap/blob/main/Formula/dekopon.rb) and the
installed versions instead of assuming a successful release updated it. The formula installs
both daemons and their reference documents, but no provider, configuration or service. The
separate `dekopon-console` formula is optional. Do not run Homebrew itself as root.

Homebrew is an alternative to archive staging. To use its binaries with the service templates,
run `brew --prefix dekopon` and substitute that literal prefix plus `/bin/` for every
`/usr/local/lib/dekopon/current/` binary path, including both plists. launchd does not
expand `$(brew --prefix dekopon)` or `~` in a plist. Account, configuration and directory setup is
still required, and the Homebrew prefix must not be writable by either daemon account. Homebrew's
prefix is owned by your login user, so anything running as that user can replace the daemons; the
archive path keeps them root-owned.

### Manual Homebrew compatibility check

A manual [macOS workflow](../.github/workflows/macos-compat.yml) installs this release from the tap
on a GitHub ARM64 runner and runs both daemons as temporary same-user launchd jobs against synthetic
Slack, GitHub and model fixtures; the [development guide](development.md#macos-homebrew-compatibility)
lists what it asserts. Its `daemons: checkout` input builds candidate binaries from the dispatched
commit instead, which is not release evidence. It uses its own fixture configuration, not an
example's files, and does not prove
separate-UID LaunchDaemons, sleep/logout/reboot recovery, Gatekeeper, Slack plan entitlements, real
PAT permissions or OpenRouter billing, so complete the checks below even after a green run. A
maintainer dispatches it from the default branch:

```console
gh workflow run macos-compat.yml --repo dekopon-agents/dekopon --ref main
gh run list --repo dekopon-agents/dekopon --workflow macos-compat.yml --event workflow_dispatch --limit 5
```

### From a checkout

Source installation is for operators prepared to maintain a Rust toolchain. Use the compiler
pinned in [`rust-toolchain.toml`](../rust-toolchain.toml), and check out the release tag:

```console
git clone https://github.com/dekopon-agents/dekopon.git
cd dekopon
git checkout v0.74.0
cargo install --locked --path crates/dekopon-brokerd
cargo install --locked --path crates/dekopon-gatewayd
```

Alternatively, `cargo install --locked --version 0.74.0 <daemon-name>` uses crates.io, if both
crates have finished publishing. Neither method installs providers or a service. The
[development guide](development.md) owns build and provider-fixture checks.

## Prepare Slack and the model

Use the [Slack Agent manifest from the release](https://github.com/dekopon-agents/dekopon/blob/v0.74.0/examples/slack/manifest-agent.yaml).
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
See OpenRouter's [key limit fields](https://openrouter.ai/docs/api/api-reference/api-keys/create-a-new-api-key)
and [BYOK billing](https://openrouter.ai/docs/guides/overview/auth/byok).

The OpenRouter key limit controls requests billed through that key; separately billed provider
tools are outside it. Dekopon's optional meter counts **raw input plus
output tokens**, not dollars; it resets on restart without best-effort telemetry restore and can
settle above its reservation. Background memory summaries are not refused by that local meter.
OpenRouter's own [in-flight accounting](https://openrouter.ai/docs/api_reference/limits) also has
rollout and estimation limits, so do not promise an exact zero-overshoot cap. Leave headroom and
check the provider's usage page. No setting here changes a Claude or ChatGPT subscription quota.

Choose one exact OpenRouter model ID that supports tools, check its current price and availability,
and replace the `model:` placeholder in the chosen example's `gateway.yaml`. Do not start with an
auto-router. A false `allowFallbacks` disables provider failover but does not pin one provider;
add `routing.only` only after checking that provider's current slug and support. Model controls
are forwarded requests, not a guarantee that every upstream provider honors them.

Store these three values in the gateway's private file
`/usr/local/etc/dekopon/gateway/credentials.env`, which you create and fill once the accounts exist,
in [Prepare the broker and gateway](#prepare-the-broker-and-gateway):

```sh
export DEKOPON_GATEWAYD_SLACK_APP_TOKEN='REPLACE_WITH_APP_TOKEN'
export DEKOPON_GATEWAYD_SLACK_BOT_TOKEN='REPLACE_WITH_BOT_TOKEN'
export OPENROUTER_API_KEY='REPLACE_WITH_DEDICATED_KEY'
```

The gateway reads this file itself with **`--env-file`** (added in 0.74.0), so its launchd job
runs the binary directly with no wrapper script. Before serving it refuses the file unless it is a
single-link regular file owned by `_dekopon_gateway`, mode `0600`, in a `0700` directory whose
ancestors are owned by root or that user and not group- or world-writable; a refusal names the
path and line, never a value. Values are literal: keep the fixed variable names and single
quotes, and expect no `$` expansion. The [environment file contract](gatewayd.md#environment-file)
gives the full grammar and precedence. Save it with Unix (LF) line endings, because the
`check --probe` command below sources it with `/bin/sh`. Do not put these values in gateway
configuration YAML, plists, agent instructions, a repository, or `launchctl setenv`; launchd does
not inherit your interactive shell's exports. Provider credentials belong only to the broker.

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
broker configuration, which contains references. You create and fill it with the accounts in
[Prepare the broker and gateway](#prepare-the-broker-and-gateway). The broker launchd plist below
runs the binary directly and needs no `EnvironmentVariables` entry or `--env-file`.

Setting `GITHUB_TOKEN` in a plist or sourcing a broker `.env` does not populate this credential:
the broker reads no credential from its environment and this bearer-token field does not expand
`${...}`.
Do not add the PAT to the gateway's `credentials.env`; that would give the wrong process the
secret. Keep the file out of Git and ordinary logs/backups. To rotate the PAT, edit the file again
as `_dekopon_broker`: with both daemons running, the broker loads a valid replacement within
seconds and logs `broker_reload_applied` in its `stdout.log`; `broker_reload_refused` means it kept
the old PAT, so fix the file before the next restart. The review guide separates read access
from optional publishing permission; a broader PAT alone grants no additional Dekopon capability.

## Prepare the broker and gateway

An administrator creates two hidden non-login accounts, `_dekopon_broker` and `_dekopon_gateway`,
and a shared `dekopon-ipc` group; Dekopon does not create them. Pick unused IDs below 500 (macOS 15
already uses 301-304). Both checks must print nothing:

```console
IPC_GID=460 BROKER_UID=461 GATEWAY_UID=462
dscl . -list /Groups PrimaryGroupID | awk -v g="$IPC_GID" '$2 == g'
dscl . -list /Users UniqueID | awk -v a="$BROKER_UID" -v b="$GATEWAY_UID" '$2 == a || $2 == b'
```

In the same terminal, create the group, both accounts with their IPC membership, and the
directories and log files in the table below:

```console
sudo dseditgroup -o create -i "$IPC_GID" -r 'Dekopon IPC' dekopon-ipc
sudo dscl . -create /Users/_dekopon_broker
sudo dscl . -create /Users/_dekopon_broker UniqueID "$BROKER_UID"
sudo dscl . -create /Users/_dekopon_broker PrimaryGroupID "$IPC_GID"
sudo dscl . -create /Users/_dekopon_broker NFSHomeDirectory /var/empty
sudo dscl . -create /Users/_dekopon_broker UserShell /usr/bin/false
sudo dscl . -create /Users/_dekopon_broker Password '*'
sudo dscl . -create /Users/_dekopon_broker IsHidden 1
sudo dscl . -create /Users/_dekopon_gateway
sudo dscl . -create /Users/_dekopon_gateway UniqueID "$GATEWAY_UID"
sudo dscl . -create /Users/_dekopon_gateway PrimaryGroupID "$IPC_GID"
sudo dscl . -create /Users/_dekopon_gateway NFSHomeDirectory /var/empty
sudo dscl . -create /Users/_dekopon_gateway UserShell /usr/bin/false
sudo dscl . -create /Users/_dekopon_gateway Password '*'
sudo dscl . -create /Users/_dekopon_gateway IsHidden 1
sudo dseditgroup -o edit -a _dekopon_broker -t user dekopon-ipc
sudo dseditgroup -o edit -a _dekopon_gateway -t user dekopon-ipc
id _dekopon_broker
id _dekopon_gateway

sudo install -d -o root -g wheel -m 0755 /usr/local/etc/dekopon \
  /usr/local/var/log/dekopon-broker /usr/local/var/log/dekopon-gateway
sudo install -d -o _dekopon_broker -g dekopon-ipc -m 0700 /usr/local/etc/dekopon/broker
sudo install -d -o _dekopon_gateway -g dekopon-ipc -m 0700 /usr/local/etc/dekopon/gateway
sudo install -d -o _dekopon_broker -g dekopon-ipc -m 0710 /usr/local/var/run/dekopon
for f in stdout stderr; do
  sudo install -o _dekopon_broker -g dekopon-ipc -m 0600 /dev/null \
    /usr/local/var/log/dekopon-broker/$f.log
  sudo install -o _dekopon_gateway -g dekopon-ipc -m 0600 /dev/null \
    /usr/local/var/log/dekopon-gateway/$f.log
done
ls -ld / /usr /usr/local /usr/local/etc /usr/local/etc/dekopon /usr/local/var /usr/local/var/run
```

Both `id` lines must list `dekopon-ipc`. The examples' `BROKER_UID` and `GATEWAY_UID` are these
UIDs; do not copy Linux chart UIDs or assume the Mac login is UID 501.

| Files | Owner and permissions |
|---|---|
| `/usr/local/etc/dekopon/broker/` and broker provider/state directories | Broker-owned `0700`; config, Cedar policy and private credentials `0600`. Providers must also be broker-owned single-link files with protected parents. |
| `/usr/local/etc/dekopon/gateway/` and gateway state directory | Gateway-owned `0700`; gateway YAML, catalog and credential file `0600`. |
| `/usr/local/var/run/dekopon/` | Broker-owned, group `dekopon-ipc`, mode `0710`; the broker creates its socket as `0660`. No group write permission. |
| `/usr/local/var/log/dekopon-broker/` and `/usr/local/var/log/dekopon-gateway/` | Root-owned `0755`; each log file owned by its daemon, `0600`. |
| Executables and `/Library/LaunchDaemons/*.plist` | Administrator-controlled; not writable by either daemon. Plists `root:wheel`, `0644`. |

Create each credential file empty with its owner and mode, then fill it as that owner; the PAT file
is needed only for the review channel:

```console
sudo install -o _dekopon_gateway -g dekopon-ipc -m 0600 /dev/null \
  /usr/local/etc/dekopon/gateway/credentials.env
sudo -u _dekopon_gateway -e /usr/local/etc/dekopon/gateway/credentials.env
sudo install -o _dekopon_broker -g dekopon-ipc -m 0600 /dev/null \
  /usr/local/etc/dekopon/broker/broker-credentials.yaml
sudo -u _dekopon_broker -e /usr/local/etc/dekopon/broker/broker-credentials.yaml
```

`sudo -e` opens a copy in your own editor and writes it back as the owner, so the file keeps its
owner and `0600`; plain `sudo vi` can leave a root-owned file. Never put a secret on a command line,
which shell history keeps.

Use regular files, not symlinks or hard links, for configuration and provider artifacts. Every
directory above the broker socket and provider files must be a real directory that is not group-
or world-writable unless sticky: each line of the `ls -ld` above shows no `w` after the owner's
permissions, or ends in `t`. The gateway's `--env-file` also requires every directory above
`credentials.env` to be owned by root or `_dekopon_gateway`, so the `ls -ld` lines from `/` to
`/usr/local/etc/dekopon` must show `root`. A Homebrew once installed under `/usr/local` can leave
`/usr/local/etc` and `/usr/local/var` owned by your login or group-writable, which the daemons
refuse. Keep the IPC directory separate from private data.
The [broker file and socket contracts](../crates/dekopon-brokerd/README.md#ipc-directory-and-distinct-peer-uids)
are authoritative. Do not solve a permission failure with `chmod -R 777` or by running both
daemons as root.

Install a reviewed, compatible provider from its own release and grant only the chosen example's
read capabilities for the first test. Follow the
[provider lifecycle](../crates/dekopon-brokerd/README.md#managed-provider-sets) or use a direct Wasm
path under the broker's private directory. Direct paths compile at each broker boot; managed
stores require sync/precompile for the selected host. Do not copy an old bundled provider or a
compiled cache from another CPU/release. The core archive and tap contain no starter provider.

Choose one complete example: the [Slack Agent review channel](../examples/slack-github/README.md)
for GitHub tools and owner/colleague read grants, or the [harmless DM starter](../examples/mac-slack/README.md)
using JSONPlaceholder before adding a PAT. Each supplies broker configuration, Cedar policy,
an agent catalog and budgeted OpenRouter gateway configuration. Replace its numeric UIDs,
Slack team/member IDs and model placeholder before checking it, then install each file as its
owner, for example:

```console
sudo install -o _dekopon_broker -g dekopon-ipc -m 0600 broker.yaml policies.cedar \
  /usr/local/etc/dekopon/broker/
sudo install -o _dekopon_gateway -g dekopon-ipc -m 0600 gateway.yaml agents.yaml \
  /usr/local/etc/dekopon/gateway/
```

The peer principal must be exactly `dekopon-gatewayd`; another name can pass config checks and
still fail gateway boot. The broker also maps its own UID for the operator probe.

The harmless starter selects Slack Agent DMs, one concurrent session, bounded tool/output/history
limits, and a rolling token meter. The review-channel example scopes routes and grants to one channel. It keeps recent text only in memory; restart loses it.
Attachments, journal summaries, follow, ambient triggers, wakes, detached jobs and the guest model
proxy are not enabled. Its numeric bounds are a starting point, not a price estimate. Add durable
history only after choosing retention and accounting for its additional inference. The complete
model and metering contracts live in [gatewayd](gatewayd.md#openrouter-model-settings).

## Check before installing a service

Run each check **as that daemon's intended user**, with the same paths it will use under launchd:

```console
sudo -u _dekopon_broker /usr/local/lib/dekopon/current/dekopon-brokerd \
  check /usr/local/etc/dekopon/broker/broker.yaml
sudo -u _dekopon_gateway /usr/local/lib/dekopon/current/dekopon-gatewayd \
  check /usr/local/etc/dekopon/gateway/gateway.yaml
```

A managed provider configuration also needs the check's `--provider-set` and `--store` arguments.
Checks validate authored input; they do not establish runtime credentials, socket ownership,
Slack installation, or model reachability. Plain `check` also accepts the examples' Slack IDs and
any nonblank model ID. A gateway `check --probe` makes one billable model request proving the model
ID exists and returns a tool call, and has its own [probe contract](gatewayd.md#check-a-configuration).
`check` refuses `--env-file`, so load the credentials file for the probe with the shell:

```console
sudo -u _dekopon_gateway /bin/sh -c '. /usr/local/etc/dekopon/gateway/credentials.env &&
  exec /usr/local/lib/dekopon/current/dekopon-gatewayd \
  check --probe /usr/local/etc/dekopon/gateway/gateway.yaml'
```

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
permitted. Start the gateway in a third terminal, with the same arguments its plist uses:

```console
sudo -u _dekopon_gateway /usr/local/lib/dekopon/current/dekopon-gatewayd \
  --config /usr/local/etc/dekopon/gateway/gateway.yaml \
  --env-file /usr/local/etc/dekopon/gateway/credentials.env
```

From a mapped sender, make the chosen example's first request: a DM for the harmless starter, an
@-mention in the mapped channel for the review channel. The review channel routes no DMs, and the
gateway ignores an unrouted message without replying or logging at the default level. Confirm a
threaded reply, the expected model usage in OpenRouter, and the broker decision in the logs.
Confirm another unmapped sender gets no model call. Stop the gateway first, then the broker.

## Run with launchd

Save these two property lists in your working directory; they contain no secrets, and the account
block above created their log files. `KeepAlive` restarts an exited process; use `bootout` for
an intentional stop. Set `ExitTimeOut` longer than the configured shutdown grace (gateway
`shutdownGraceMs`, broker `serverLimits.shutdownGraceMs`, both 120 seconds in the examples);
increase the example's 150 seconds if either daemon needs more time to drain.

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
  <key>ProgramArguments</key><array>
    <string>/usr/local/lib/dekopon/current/dekopon-gatewayd</string>
    <string>--config</string><string>/usr/local/etc/dekopon/gateway/gateway.yaml</string>
    <string>--env-file</string><string>/usr/local/etc/dekopon/gateway/credentials.env</string>
  </array>
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

Install and validate both, load the broker, and wait for a successful probe before loading the
gateway:

```console
sudo install -o root -g wheel -m 0644 dev.dekopon.broker.plist dev.dekopon.gateway.plist \
  /Library/LaunchDaemons/
plutil -lint /Library/LaunchDaemons/dev.dekopon.broker.plist \
  /Library/LaunchDaemons/dev.dekopon.gateway.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/dev.dekopon.broker.plist
until sudo -u _dekopon_broker /usr/local/lib/dekopon/current/dekopon-brokerd \
  probe --socket /usr/local/var/run/dekopon/broker.sock; do sleep 2; done
sudo launchctl bootstrap system /Library/LaunchDaemons/dev.dekopon.gateway.plist
sudo launchctl print system/dev.dekopon.broker
sudo launchctl print system/dev.dekopon.gateway
```

At boot launchd can start either job first; a plist does not express dependency readiness. A
gateway that starts before the broker's socket exists exits at once, logging
`broker is not reachable` to its `stdout.log`, and `KeepAlive` starts it again after
`ThrottleInterval` until the broker serves; `broker.ioTimeoutMs` covers only a running gateway's
per-request connects. After a cold boot with provider compilation, check that the gateway's
`stdout.log` shows `gateway_broker_ready` and that a fresh request on the example's route is
answered.
The templates do not claim a tested macOS service installer; run `plutil`, permission checks and
the boot/logout checks on the target Mac.

To stop, remove the gateway job before the broker job, and wait for each to exit:

```console
sudo launchctl bootout system/dev.dekopon.gateway
while pgrep -U "$(id -u _dekopon_gateway)" >/dev/null; do sleep 2; done
sudo launchctl bootout system/dev.dekopon.broker
while pgrep -U "$(id -u _dekopon_broker)" >/dev/null; do sleep 2; done
```

To restart the gateway, for example after editing `credentials.env`, run its two stop lines, then
its `bootstrap` line. `bootout` unloads the running job; files left in `/Library/LaunchDaemons`
load again at boot. To keep a service off across reboots, move its plist out of that directory
after stopping it. Never start a second foreground gateway while the service is running.

## Sleep, logs, updates and recovery

On a Mac desktop, enable **System Settings → Energy → Prevent automatic sleeping when the display
is off**, as described by [Apple](https://support.apple.com/guide/mac-help/set-sleep-and-wake-settings-mchle41a6ccd/mac).
The display may sleep; a sleeping computer cannot answer Slack. `KeepAlive` restarts a process,
not a sleeping Mac. Reboot, logout, network recovery and sleep/wake are separate acceptance checks.
Also turn on **Start up automatically after a power failure** in the same settings. With FileVault
on, a restart after a power cut waits for an account password before launchd starts either
daemon; on Apple silicon with macOS 26 or later,
[Remote Login can unlock it over SSH](https://support.apple.com/guide/security/managing-filevault-sec8447f5049/web).
Do not disable disk encryption to obtain unattended startup, and test a power cut on the actual Mac.

When a job repeatedly exits, read `sudo launchctl print system/<label>`, the broker's `stderr.log`
(its `broker_exit` record) and the gateway's `stdout.log` (its `gateway_exit` record); the
gateway's `stderr.log` holds failures before logging starts, such as a refused `--env-file`
naming its path and line. JSON stdout
carries broker audit and gateway diagnostics; prompts and outputs may reach logs/telemetry, so
keep them private. launchd's file redirection does not bound log growth. Before leaving it
unattended, choose a size/retention policy. The broker's `stdout.log` is its only local audit
record. A simple maintenance procedure is to stop both jobs, archive or remove old logs with their
private permissions preserved, recreate the empty files with the account block's log `install`
lines, then restart broker and gateway. An external log collector is optional and adds its own
resource and retention cost.

Keep configurations, Cedar policies, provider locks/components, and any configured persistent
storage in private backups. Exclude tokens from ordinary repository backups. Preserve separately
protected credential recovery material or be prepared to issue fresh credentials. This starter
has no journal; adding one creates chat text and attachment state that needs its own retention
and backup policy. Restoring old configuration never undoes an external effect.

For an upgrade, read [upgrading](upgrading.md) and save the current configuration and provider
lock. Extract the candidate archive as in [Prebuilt archives](#prebuilt-archives) (0.75.0 stands
for its version here), then stage and check the candidate pair beside the running one:

```console
new=0.75.0
sudo install -d -o root -g wheel -m 0755 "/usr/local/lib/dekopon/$new"
sudo install -o root -g wheel -m 0755 "dekopon-$new-aarch64-apple-darwin/dekopon-brokerd" \
  "dekopon-$new-aarch64-apple-darwin/dekopon-gatewayd" "/usr/local/lib/dekopon/$new/"
sudo -u _dekopon_broker "/usr/local/lib/dekopon/$new/dekopon-brokerd" \
  check /usr/local/etc/dekopon/broker/broker.yaml
sudo -u _dekopon_gateway "/usr/local/lib/dekopon/$new/dekopon-gatewayd" \
  check /usr/local/etc/dekopon/gateway/gateway.yaml
```

Stop both jobs and wait for them to exit as in [Run with launchd](#run-with-launchd), then switch
the pointer:

```console
sudo ln -sfh "/usr/local/lib/dekopon/$new" /usr/local/lib/dekopon/current
```

`-h` replaces the link itself; without it `ln` follows `current` and creates a link inside the
old release directory. Load the broker, wait for its probe and load the gateway with that
section's `bootstrap` commands, then make the example's first request again. With Homebrew, stop
both jobs, run
`brew unpin dekopon && brew upgrade dekopon && brew pin dekopon`, check, and start them; the
upgrade deletes the previous keg, so keep the previous release archive as the rollback pair.
For rollback, stop both jobs, restore the configuration and provider state saved for the old
release, run `sudo ln -sfh /usr/local/lib/dekopon/0.74.0 /usr/local/lib/dekopon/current`, and start
them again. Use the retained binary **pair and matching configuration/provider state**, not an
old binary against a newly migrated store. A release before 0.74.0 refuses `--env-file`
([upgrading](upgrading.md)); rolling back past it means removing that option from the gateway
plist and starting the gateway through a root-owned script that sources `credentials.env` and
`exec`s the binary.
There is no universal reverse data migration. Do not run both releases concurrently, and inspect
uncertain external effects before retrying.

If OpenRouter refuses a request (key limit reached, credit exhausted or key revoked), Slack shows
only "The agent could not complete this request." and the gateway's `stdout.log` has
`gateway_session_failed` with `category` `provider` (HTTP 402) or `authentication` (401/403).
Boot never contacts OpenRouter, so this is not a startup error; the local token meter instead
replies with its own budget sentence. A missing or empty credential variable does stop the gateway
at startup, naming the variable. Fix the account, limit or private credential file, restart the
gateway, and recheck usage; do not paste token values into reports. A restart does not replenish
OpenRouter credit, even though it resets an unrestored local token meter. Stop the gateway first
when diagnosing unexpected spend.

## Acceptance before switching

1. Check the exact installed pair (`--version`), configuration (`check`), provider compatibility
   and cross-UID socket (`gateway_broker_ready` in the gateway's `stdout.log`). Prove that one
   permitted request on the chosen example's route (a DM for the harmless starter, an @-mention in
   the mapped channel for the review channel) gets a threaded answer and a denied sender gets no
   model call, then inspect OpenRouter usage and the saved key limit. Use a small trial allocation
   to exercise the limit/refusal path; expect the refusal described above.
2. Prove service restart, logout and full reboot recovery on the actual Mac; verify network
   recovery and that the machine remains awake. Pass means
   `sudo launchctl print system/dev.dekopon.gateway` shows a running `pid`, unchanged across
   logout; the gateway's `stdout.log` shows `gateway_broker_ready` after each restart or reboot and
   a fresh request is answered; and `pmset -g log` shows no system sleep overnight. Then inspect
   logs and disk use.
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
