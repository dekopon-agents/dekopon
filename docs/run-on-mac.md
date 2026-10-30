# Run Dekopon on a Mac

This runbook takes a skilled engineer or coding agent from a clean Apple Silicon Mac to an
always-on Dekopon service: the released ARM64 daemons running as system LaunchDaemons under two
hidden accounts, answering Slack through one OpenRouter model. The broker (`dekopon-brokerd`)
holds policy, provider credentials and audit; the gateway (`dekopon-gatewayd`) serves Slack and the
model. The release supplies binaries, not a service. CI does not exercise the accounts,
LaunchDaemons, or sleep, logout and reboot recovery below, so the
[acceptance checks](#acceptance-before-switching) on the target Mac are their proof.

Choose one example first. It supplies the provider (the Wasm component that runs the agent's tools)
and every configuration file:

- [Slack Agent review channel](../examples/slack-github/README.md): an owner and a colleague get
  Cedar read grants on GitHub in one channel; the broker holds the PAT.
- [Harmless DM starter](../examples/mac-slack/README.md): one sender, one synthetic read-only tool,
  no PAT.

Then work in order:

1. [Install](#install) the daemon pair.
2. [Prepare Slack and the model](#prepare-slack-and-the-model).
3. [Prepare the broker and gateway](#prepare-the-broker-and-gateway).
4. [Check before installing a service](#check-before-installing-a-service).
5. [Run with launchd](#run-with-launchd).
6. Set up [sleep, logs, updates and recovery](#sleep-logs-updates-and-recovery).
7. Pass [acceptance](#acceptance-before-switching) before replacing another agent.

Out of scope: Docker Desktop ([container guide](container-image.md)),
[subscription authentication](cli.md) and the same-user
[local transport](gatewayd.md#local-development-transport), which trusts the caller's declared subject.

## Install

Install both daemons from one release for Apple Silicon (`aarch64-apple-darwin`). The binaries
declare macOS 11 but are checked only on macOS 15: use Sequoia 15 or later. Intel Macs have no
release archive or bottle.

- Use that tag's configuration and provider contracts: `main` can be ahead even when `--version`
  prints the release number.
- You need `sudo`, the [GitHub CLI](https://cli.github.com) signed in with `gh auth login` for the
  release, attestation and provider downloads, and [Homebrew](https://brew.sh) only for its path.
- macOS lacks the Linux-only address-space cap on the `jq` worker
  ([shell bounds](../crates/dekopon-shell/README.md#sandboxing)), and no provider brings
  Firecracker, KVM or an arbitrary host shell to a Mac.

### Prebuilt archives

Verify and unpack the [0.74.0 release](https://github.com/dekopon-agents/dekopon/releases/tag/v0.74.0)
archive. It holds both daemons, the optional console and reference documents.

```console
gh release download v0.74.0 --repo dekopon-agents/dekopon \
  --pattern 'dekopon-0.74.0-aarch64-apple-darwin.tar.gz*'
shasum -a 256 -c dekopon-0.74.0-aarch64-apple-darwin.tar.gz.sha256
gh attestation verify --repo dekopon-agents/dekopon \
  dekopon-0.74.0-aarch64-apple-darwin.tar.gz
tar xzf dekopon-0.74.0-aarch64-apple-darwin.tar.gz
```

On the first install, stage the pair in a versioned, root-owned directory at the paths the service
templates use:

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

- The guard creates no link when `current` exists, even as a dangling symlink; change a running
  service with the [upgrade procedure](#upgrade-and-rollback).
- An administrator owns the directory and the pointer; neither daemon may replace the binaries.
- Checksums and attestations prove byte integrity and build provenance, not Apple signing or
  notarization. If Gatekeeper blocks a binary, read its exact message and check the release
  provenance; never disable Gatekeeper system-wide.

### Homebrew

Homebrew replaces archive staging:

```console
brew install dekopon-agents/tap/dekopon
brew pin dekopon
dekopon-brokerd --version
dekopon-gatewayd --version
```

- Since Homebrew 6.0, a non-official tap loads only after trust; the fully qualified name taps
  `dekopon-agents/tap` and trusts only this formula, for your user only. Never run Homebrew as root.
- `brew pin` keeps `brew upgrade` from replacing the binaries under the services; upgrade only
  through the [upgrade procedure](#upgrade-and-rollback).
- Replace `/usr/local/lib/dekopon/current/` in every binary path, both plists included, with the
  literal output of `brew --prefix dekopon` plus `/bin/`. launchd expands neither `$(…)` nor `~`.
- Your login user owns the Homebrew prefix, so anything running as that user can replace the
  daemons; the archive path keeps them root-owned. Neither daemon account may write the prefix.

### Manual Homebrew compatibility check

A maintainer dispatches the manual [macOS workflow](../.github/workflows/macos-compat.yml) from the
default branch:

```console
gh workflow run macos-compat.yml --repo dekopon-agents/dekopon --ref main
gh run list --repo dekopon-agents/dekopon --workflow macos-compat.yml --event workflow_dispatch --limit 5
```

It installs the release from the tap on a hosted ARM64 runner and runs both daemons as same-user
launchd jobs against synthetic Slack, GitHub and model fixtures
([coverage](development.md#macos-homebrew-compatibility)). It does not prove separate-UID
LaunchDaemons, sleep, logout or reboot recovery, Gatekeeper, Slack plan entitlements, real PAT
permissions or OpenRouter billing, so pass [acceptance](#acceptance-before-switching) after a
green run too.

The released 0.74.0 tap passed `mode: env-file` on a hosted macOS 15 ARM64 runner: tap commit
`ad54c51917e50aeb59193940d12315775f6c05c6`, all three daemons 0.74.0 from tag `v0.74.0`
([run 41092694914](https://github.com/dekopon-agents/dekopon/actions/runs/41092694914)). That is
synthetic same-user evidence only, not live Slack, GitHub or OpenRouter traffic, separate-UID system
services, Gatekeeper or sleep and reboot recovery.

### From a checkout

Build from source only if you maintain a Rust toolchain, with the compiler pinned in
[`rust-toolchain.toml`](../rust-toolchain.toml) and the release tag:

```console
git clone https://github.com/dekopon-agents/dekopon.git
cd dekopon
git checkout v0.74.0
cargo install --locked --path crates/dekopon-brokerd
cargo install --locked --path crates/dekopon-gatewayd
```

The [development guide](development.md) owns build checks.

## Prepare Slack and the model

Create the Slack app and a dedicated OpenRouter key, choose one model, and collect three secrets.

1. Create the app from the
   [released Agent manifest](https://github.com/dekopon-agents/dekopon/blob/v0.74.0/examples/slack/manifest-agent.yaml)
   and get its `xapp-…` and `xoxb-…` tokens with the
   [Slack setup guide](../examples/slack/README.md#create-the-app-and-credentials). The workspace
   plan and admin policy must allow Slack's Agent features; otherwise choose the
   [classic manifest](../examples/slack/manifest.yaml) and `experience: classic` deliberately.
2. For the review channel, invite the bot to one private channel whose members may see the
   repository code and findings, as the [recipe](../examples/slack-github/README.md) explains.
3. Create a dedicated OpenRouter inference key, not a management key, with a finite USD
   [limit](https://openrouter.ai/docs/api/api-reference/api-keys/create-a-new-api-key) and an
   explicit reset: monthly, or none for a fixed trial. Use prepaid credit, keep automatic top-ups
   off, and skip BYOK, which bills separately.
4. Choose one exact, tool-capable OpenRouter model ID, never an auto-router; check its price and
   availability, and set it as `model:` in the example's `gateway.yaml`. Add `routing.only` only
   after checking that provider's slug and support
   ([routing controls](gatewayd.md#openrouter-model-settings)).

Spending limits:

- The key limit covers only requests billed through that key, not separately billed provider
  tools. OpenRouter's [in-flight accounting](https://openrouter.ai/docs/api_reference/limits) can
  overshoot it: leave headroom and watch the usage page.
- Dekopon's optional local meter counts raw input plus output tokens, not dollars. It can settle
  above its reservation, starts empty after a restart without telemetry restore, and never refuses
  background memory summaries ([token budgets](gatewayd.md#token-budgets)).

Keep the three values for the gateway's `credentials.env`, created in
[Credential files](#credential-files):

```sh
export DEKOPON_GATEWAYD_SLACK_APP_TOKEN='REPLACE_WITH_APP_TOKEN'
export DEKOPON_GATEWAYD_SLACK_BOT_TOKEN='REPLACE_WITH_BOT_TOKEN'
export OPENROUTER_API_KEY='REPLACE_WITH_DEDICATED_KEY'
```

## Prepare the broker and gateway

Create the accounts, directories and credential files, then install the provider and the example's
configuration. Dekopon creates no accounts; an administrator runs every command.

### Accounts and directories

Pick an unused group ID and two unused user IDs below 500; macOS 15 already uses 301–304. Both
checks must print nothing:

```console
IPC_GID=460 BROKER_UID=461 GATEWAY_UID=462
dscl . -list /Groups PrimaryGroupID | awk -v g="$IPC_GID" '$2 == g'
dscl . -list /Users UniqueID | awk -v a="$BROKER_UID" -v b="$GATEWAY_UID" '$2 == a || $2 == b'
```

In the same terminal, create the `dekopon-ipc` group, the hidden non-login accounts
`_dekopon_broker` and `_dekopon_gateway` in it, and the directories and log files in the table:

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

- Both `id` lines list `dekopon-ipc`. These UIDs are the examples' `BROKER_UID` and `GATEWAY_UID`;
  never copy Linux chart UIDs or assume UID 501.
- Each `ls -ld` line shows a real directory with no group or world `w`, unless its mode ends in `t`.
  `--env-file` also needs owner `root` on every line from `/` to `/usr/local/etc/dekopon`. A
  Homebrew once installed under `/usr/local` can leave `/usr/local/etc` or `/usr/local/var` owned
  by your login or group-writable, which both daemons refuse.

| Path | Owner and mode |
|---|---|
| `/usr/local/etc/dekopon/broker/`, broker provider and state directories | Broker, `0700`; configuration, Cedar policy and credentials `0600`; providers broker-owned, single-link, with protected parents |
| `/usr/local/etc/dekopon/gateway/`, gateway state directory | Gateway, `0700`; YAML, catalog and credential file `0600` |
| `/usr/local/var/run/dekopon/` | Broker, group `dekopon-ipc`, `0710`, no group write; socket `0660`, created by the broker |
| `/usr/local/var/log/dekopon-broker/`, `/usr/local/var/log/dekopon-gateway/` | Root, `0755`; each log file its daemon's, `0600` |
| Executables, `/Library/LaunchDaemons/*.plist` | Administrator, never daemon-writable; plists `root:wheel`, `0644` |

- Use regular files, never symlinks or hard links, for configuration and provider artifacts.
- Keep the IPC directory apart from private data; the
  [broker file and socket contracts](../crates/dekopon-brokerd/README.md#ipc-directory-and-distinct-peer-uids)
  are authoritative.
- Never fix a permission failure with `chmod -R 777` or by running both daemons as root.

### Credential files

Create each credential file empty with its owner and mode, then edit it as that owner; only the
review channel needs the PAT file:

```console
sudo install -o _dekopon_gateway -g dekopon-ipc -m 0600 /dev/null \
  /usr/local/etc/dekopon/gateway/credentials.env
sudo -u _dekopon_gateway -e /usr/local/etc/dekopon/gateway/credentials.env
sudo install -o _dekopon_broker -g dekopon-ipc -m 0600 /dev/null \
  /usr/local/etc/dekopon/broker/broker-credentials.yaml
sudo -u _dekopon_broker -e /usr/local/etc/dekopon/broker/broker-credentials.yaml
```

`sudo -e` keeps the owner and `0600`; plain `sudo vi` can leave a root-owned file. Never put a
secret on a command line. The PAT file's contents are in the
[recipe](../examples/slack-github/README.md#keep-the-github-pat-in-the-broker).

The gateway loads `credentials.env` itself with [`--env-file`](gatewayd.md#environment-file). The
file's contract:

- It holds the three `export` lines from [Prepare Slack and the model](#prepare-slack-and-the-model),
  with their fixed names and single quotes. Values are literal; nothing expands `$`. The PAT and
  every other provider credential belong to the broker.
- It uses Unix (LF) line endings, because the [`check --probe`](#check-before-installing-a-service)
  command sources it with `/bin/sh`.
- It is a single-link regular file owned by `_dekopon_gateway`, mode `0600`, in a `0700` directory
  whose ancestors are owned by root or that user and are not group- or world-writable. Otherwise
  the gateway refuses to serve, naming the path and line, never a value.
- Its values never go in gateway YAML, a plist, agent instructions, a repository or
  `launchctl setenv`. launchd does not inherit your shell's exports.

**Converting a file a shell used to source.** `--env-file` is not a drop-in replacement for a
shell-sourced file. A file written for `. credentials.env` may use `$VAR` expansion, backslash
escapes, `$(…)` or backtick commands, or `export` lines that expand other variables; the loader
reads each as literal text, so a daemon would receive the characters `$(…)`, not the command's
output. Resolve each such value by hand into its literal form in the editor that `sudo -e` opens,
never by sourcing the old file into the new one, and merge repeated keys, which the loader refuses.

```sh
# Before: a shell-sourced file
SUFFIX=example-suffix
export OPENROUTER_API_KEY="synthetic-$SUFFIX"
export DEKOPON_GATEWAYD_SLACK_BOT_TOKEN="$(cat /path/to/synthetic-bot-token)"

# After: literal values for --env-file
export OPENROUTER_API_KEY='synthetic-example-suffix'
export DEKOPON_GATEWAYD_SLACK_BOT_TOKEN='synthetic-bot-token-value'
```

### GitHub PAT under launchd

The broker reads the PAT only from `/usr/local/etc/dekopon/broker/broker-credentials.yaml`, named by
`credentialsPath`. The [recipe](../examples/slack-github/README.md#keep-the-github-pat-in-the-broker)
owns the file's contents, the PAT's permissions and its rotation.

- The file holds the literal secret, owned by `_dekopon_broker`, mode `0600`, in the `0700` broker
  directory; the broker configuration holds only references.
- The broker's native HTTP host adds the Authorization header for `api.github.com`; the provider
  Wasm, the gateway and the model never receive the PAT. Keep it out of Git, ordinary logs and
  backups.
- The broker reads no credential from its environment, and the file expands no `${...}`. Its plist
  needs no `EnvironmentVariables` or `--env-file`; `GITHUB_TOKEN` in a plist or a sourced `.env`
  does nothing.
- Rotate without a restart: with the gateway running, edit the file with the `sudo -u
  _dekopon_broker -e` line above. The broker's `stdout.log` shows `broker_reload_applied`, or
  `broker_reload_refused` if it kept the old PAT; then fix the file before the next restart.
- A broader PAT alone grants no extra Dekopon capability; broker capabilities and Cedar grants
  decide what runs.

### Provider and configuration

Install one reviewed, compatible provider from its own release, as the example shows, and grant only
the example's read capabilities for the first test. The core archive and the tap contain no
provider. There is no provider-free chat bot: the broker requires an installed compatible
provider, and the gateway refuses a sender with no granted capability.

- A direct Wasm path under the broker's private directory compiles at each broker boot; a managed
  store needs sync and precompile for this host
  ([provider lifecycle](../crates/dekopon-brokerd/README.md#managed-provider-sets)).
- Never copy an old bundled provider or a compiled cache from another CPU or release.

Replace the example's numeric UIDs, Slack team and member IDs and model placeholder, then install
each file as its owner, for example:

```console
sudo install -o _dekopon_broker -g dekopon-ipc -m 0600 broker.yaml policies.cedar \
  /usr/local/etc/dekopon/broker/
sudo install -o _dekopon_gateway -g dekopon-ipc -m 0600 gateway.yaml agents.yaml \
  /usr/local/etc/dekopon/gateway/
```

The gateway's peer principal is exactly `dekopon-gatewayd`; another name can pass `check` and still
fail gateway boot. The broker also maps its own UID, for the operator probe.

## Check before installing a service

Check both configurations, probe the model, then run both daemons in the foreground. Run each
command as the daemon's user, with the paths launchd will use:

```console
sudo -u _dekopon_broker /usr/local/lib/dekopon/current/dekopon-brokerd \
  check /usr/local/etc/dekopon/broker/broker.yaml
sudo -u _dekopon_gateway /usr/local/lib/dekopon/current/dekopon-gatewayd \
  check /usr/local/etc/dekopon/gateway/gateway.yaml
```

- A managed provider configuration also needs `--provider-set` and `--store`.
- `check` validates authored input only, not credentials, socket ownership, the Slack installation
  or model reachability. It accepts the examples' sample Slack IDs and any nonblank model ID.

`check --probe` sends one billable model request that proves the model ID exists and returns a tool
call ([probe contract](gatewayd.md#check-a-configuration)). `check` refuses `--env-file`, so the
shell sources the credentials:

```console
sudo -u _dekopon_gateway /bin/sh -c '. /usr/local/etc/dekopon/gateway/credentials.env &&
  exec /usr/local/lib/dekopon/current/dekopon-gatewayd \
  check --probe /usr/local/etc/dekopon/gateway/gateway.yaml'
```

Start the broker in the foreground:

```console
sudo -u _dekopon_broker /usr/local/lib/dekopon/current/dekopon-brokerd \
  --config /usr/local/etc/dekopon/broker/broker.yaml
```

Once it has started, probe it as the broker user from a second terminal:

```console
sudo -u _dekopon_broker /usr/local/lib/dekopon/current/dekopon-brokerd \
  probe --socket /usr/local/var/run/dekopon/broker.sock
```

The probe pins the server to its own effective UID and invokes nothing: it checks the broker's
health, not cross-UID access or a sender's permission. Start the gateway in a third terminal, with
its plist's arguments:

```console
sudo -u _dekopon_gateway /usr/local/lib/dekopon/current/dekopon-gatewayd \
  --config /usr/local/etc/dekopon/gateway/gateway.yaml \
  --env-file /usr/local/etc/dekopon/gateway/credentials.env
```

1. From a mapped sender (a Slack member under `principals` in `broker.yaml`), make the example's
   first request: a DM for the harmless starter, an @-mention in the mapped channel for the review
   channel. The review channel routes no DMs; an unrouted message gets no reply and no
   default-level log line.
2. Confirm a threaded reply, the usage in OpenRouter and the decision in the broker's terminal output.
3. Confirm an unmapped sender gets no model call.
4. Stop the gateway, then the broker.

## Run with launchd

Run both daemons as system LaunchDaemons, which start at boot and outlive logout. User LaunchAgents
would give both daemons one UID, and the broker refuses two identities with one UID.

Save these two property lists in your working directory. They hold no secrets, and the accounts
block created their log files.

- `KeepAlive` restarts an exited process; `bootout` stops one on purpose.
- `ExitTimeOut` must exceed the shutdown grace: the gateway's `shutdownGraceMs` and the broker's
  `serverLimits.shutdownGraceMs`, 120 seconds in the examples. Raise the 150 seconds if either
  daemon needs longer to drain.

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

Install and lint both, load the broker, wait for its probe, then load the gateway:

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

At boot, launchd starts the jobs in either order; a plist cannot express readiness. A gateway that
starts before the broker's socket exists exits with `broker is not reachable` in its `stdout.log`,
and `KeepAlive` restarts it after each 30-second `ThrottleInterval` until the broker serves.
`broker.ioTimeoutMs` covers only a running gateway's per-turn connects.

Stop the gateway before the broker, and wait for each to exit:

```console
sudo launchctl bootout system/dev.dekopon.gateway
while pgrep -U "$(id -u _dekopon_gateway)" >/dev/null; do sleep 2; done
sudo launchctl bootout system/dev.dekopon.broker
while pgrep -U "$(id -u _dekopon_broker)" >/dev/null; do sleep 2; done
```

- To restart the gateway, for example after editing `credentials.env`, run its two stop lines and
  then its `bootstrap` line.
- A plist left in `/Library/LaunchDaemons` loads again at boot; to keep a service off, move its
  plist out after stopping it.
- Never start a second, foreground gateway while the service runs.

## Sleep, logs, updates and recovery

Keep the Mac awake and powered, keep logs bounded and private, back up the configuration, and
upgrade or roll back the binary pair together.

### Power and sleep

- Turn on **System Settings → Energy → Prevent automatic sleeping when the display is off**
  ([Apple](https://support.apple.com/guide/mac-help/set-sleep-and-wake-settings-mchle41a6ccd/mac))
  and **Start up automatically after a power failure**. The display may sleep; `KeepAlive`
  restarts processes, not a sleeping Mac.
- With FileVault on, a restart after a power cut waits for an account password before launchd
  starts either daemon. On Apple silicon with macOS 26 or later,
  [Remote Login can unlock it over SSH](https://support.apple.com/guide/security/managing-filevault-sec8447f5049/web).
  Never disable disk encryption to get unattended startup.

### Logs and backups

- When a job keeps exiting, read `sudo launchctl print system/<label>`, the broker's `stderr.log`
  (`broker_exit`) and the gateway's `stdout.log` (`gateway_exit`). The gateway's `stderr.log`
  holds failures from before logging starts, such as a refused `--env-file` naming its path and
  line.
- JSON stdout carries broker audit and gateway diagnostics, and prompts and outputs can reach logs
  and telemetry: keep both private. The broker's `stdout.log` is its only local audit record.
- launchd does not bound log growth; set a size and retention policy before leaving the Mac
  unattended. To rotate, stop both jobs, archive or remove old logs with their private
  permissions, recreate the empty files with the accounts block's log `install` lines, and start
  the broker, then the gateway.
- Back up configurations, Cedar policies, provider locks and components, and any persistent storage
  privately. Keep tokens out of ordinary repository backups, and keep separately protected
  credential recovery material or be ready to issue fresh credentials.
- A journal, which the examples omit, stores chat text and attachment state that needs its own
  retention and backup policy. Restoring old configuration never undoes an external effect.

### Upgrade and rollback

Read [upgrading](upgrading.md), save the current configuration and provider lock, and extract the
candidate archive as in [Prebuilt archives](#prebuilt-archives). Stage and check the candidate pair
beside the running one; 0.75.0 stands for the new version:

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

Stop both jobs as in [Run with launchd](#run-with-launchd), then switch the pointer. `-h` replaces
the link itself; without it, `ln` follows `current` and creates a link inside the old release.

```console
sudo ln -sfh "/usr/local/lib/dekopon/$new" /usr/local/lib/dekopon/current
```

Load the broker, wait for its probe and load the gateway with that section's commands, then make
the example's first request again.

- **Homebrew:** stop both jobs, run `brew unpin dekopon && brew upgrade dekopon && brew pin dekopon`,
  check, and start them. The upgrade deletes the previous keg, so keep the previous release archive
  as the rollback pair.
- **Rollback:** stop both jobs, restore the configuration and provider state saved for the old
  release, run `sudo ln -sfh /usr/local/lib/dekopon/0.74.0 /usr/local/lib/dekopon/current` and start
  them. Use the retained binary pair with its own configuration and provider state, never an old
  binary against a newly migrated store; there is no universal reverse data migration.
- **Before 0.74.0:** an older release refuses `--env-file` ([upgrading](upgrading.md)). Rolling back
  past it means removing that option from the gateway plist and starting the gateway through a
  root-owned script that sources `credentials.env` and `exec`s the binary.
- Never run both releases at once; inspect uncertain external effects before retrying.

### Spending refusals

When OpenRouter refuses a request (key limit reached, credit exhausted or key revoked), Slack shows
only "The agent could not complete this request." and the gateway's `stdout.log` has
`gateway_session_failed` with `category` `provider` (HTTP 402) or `authentication` (401 or 403).

- Boot never contacts OpenRouter, so this is not a startup error. A missing or empty credential
  variable does stop the gateway at startup, naming the variable.
- The local token meter refuses with its own budget sentence instead.
- Fix the account, limit or credential file, restart the gateway and recheck usage. Never paste
  token values into reports.
- A restart resets an unrestored local meter but adds no OpenRouter credit. Stop the gateway first
  when diagnosing unexpected spend.

## Acceptance before switching

Pass each check on the target Mac before this installation replaces another agent:

1. `--version` shows the intended pair, both `check` commands pass with the installed provider,
   and the gateway's `stdout.log` shows `gateway_broker_ready`, proving the cross-UID socket.
2. A permitted request on the example's route gets a threaded answer, and a denied sender gets no
   model call. OpenRouter shows the usage within the saved key limit.
3. A small trial allocation reaches its limit and produces the [spending refusal](#spending-refusals).
4. Slack shows native Working/Stop, or the reaction fallback.
5. The service recovers from a restart, logout, full reboot, network loss and power cut, and the Mac
   stays awake. Pass means `sudo launchctl print system/dev.dekopon.gateway` shows a running `pid`,
   unchanged across logout; the gateway's `stdout.log` shows `gateway_broker_ready` after each
   restart or reboot and a fresh request is answered; and `pmset -g log` shows no system sleep
   overnight. Then inspect logs and disk use.
6. The previous system keeps its configuration and a rollback path. Stop its message consumer
   before Dekopon becomes the primary bot, so only one system consumes the route. Nothing is
   imported automatically: review each credential, conversation, skill and permission.
