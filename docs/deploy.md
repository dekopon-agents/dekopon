# Deploy

Kubernetes is the preferred deployment. The Mac path serves people moving from OpenClaw or Hermes.
Both run `dekopon-brokerd`, holding policy and provider credentials, and `dekopon-gatewayd`, holding
chat and model credentials, joined by one Unix socket. Write policy as the [authorization
example](authorization.md#example) does, including its request `forbid`.

## Shapes

| Shape | For | Trade |
|---|---|---|
| [One account](#one-account) | a personal Mac or Linux box | no `sudo`; [scoping, not separation](security-model.md#current-local-process-boundary) |
| [Two accounts](#two-accounts) | a shared host | separation; every edit needs `sudo` |
| [Kubernetes](#kubernetes) | teams | separation |

Nothing listens on a network port unless you configure a `githubWebhook` or `whatsappCloudApi`
transport or the model proxy. Slack Socket Mode and Discord connect out over WebSockets, Telegram
long-polls, and model and provider calls are outbound HTTPS.

## Files

Each daemon refuses a symlink, a second hard link, or a file another UID owns (the gateway also
takes root's on a read-only mount). Configuration and policy must not be group- or world-writable;
the secret map, credential files and `--env-file` must be `0600`. Directories above the socket,
providers and storage must not be group- or world-writable unless sticky, and above `--env-file`
and `--log-file` must belong to root or the daemon. `dekopon-brokerd check` skips credential files
and an unreadable secret map, so their owner and mode fail only at start. macOS refuses a socket
path of 104 bytes or more.

## Kubernetes

```console
helm upgrade --install dekopon oci://ghcr.io/dekopon-agents/charts/dekopon \
  -n dekopon --create-namespace -f my-values.yaml
```

[`values.yaml`](../charts/dekopon/values.yaml) is the reference. The defaults start a broker with
nothing to run; a chat deployment sets `gateway.enabled` and supplies policy, configuration, a
catalog and credentials, as [`examples/conditional-write`](../examples/conditional-write/) does.

What the values don't make obvious:

- **The broker must map the gateway.** The gateway runs as UID 65533 and the broker as 65532. With
  `broker.configDirectory.peers` the chart renders that mapping, with the gateway's `attestor`
  grant. Inline configuration writes it into the broker's `identities` itself and sets the gateway's
  `broker.socketPath: /run/dekopon/broker.sock` and `broker.serverUid: 65532`; otherwise the broker
  refuses the gateway.
- **An init container copies the broker's files.** No volume passes the [file checks](#files): a
  projected Secret key is a symlink and a `subPath` file is root-owned. The `prepare-files` init
  container, root with only `CHOWN` and `FOWNER`, copies each into a memory volume as a fresh `0600`
  file. The namespace must admit it: Pod Security `baseline`, not
  `restricted`. A changed existing Secret reaches the broker only when you restart the pod.
- **The image's providers belong to `65532`**, and the broker loads only components it owns.
- **A hook Job syncs the provider set.** With `broker.providerSet` and `broker.providerSync`, a
  pre-install and pre-upgrade Job runs `dekopon-brokerd provider sync` onto the state claim, and a
  `precompile` init container compiles before the broker starts. The claim must already exist
  (`state.existingClaim`). Argo CD runs Helm hooks before every wave, so make it a `Sync` hook after
  its ConfigMap through `broker.providerSync.annotations`. A failed sync leaves the old pod serving.
- **The ChatGPT credential is seeded once.** Login is a browser device flow, so log in on a
  workstation and export a Secret:

  ```console
  dekopon-gatewayd auth chatgpt login
  dekopon-gatewayd auth chatgpt export --expose-credential --namespace dekopon | kubectl apply -f -
  ```

  Set `gateway.chatgpt.enabled` and `existingSecret: dekopon-chatgpt-auth`, the Secret `export` emits. Init copies the file onto the state claim
  only when none is there. The gateway rotates the refresh token and writes it back, so after the
  first refresh the Secret is a retired token; never copy it back. That rotation is why the
  Deployment is one replica with `Recreate`. After a deliberate re-login, set
  `gateway.chatgpt.reseed: true` for one roll. A broker credential of this kind (`broker.chatgpt`)
  needs a second login with `--auth-file`: two holders of one token family eventually revoke it.

The chart runs `ghcr.io/dekopon-agents/dekopon`: both daemons and the console, distroless, with a
`-wasmtime-optimization` tag for the memory-optimized Wasmtime.

## Mac

On Apple silicon:

```console
brew install dekopon-agents/tap/dekopon
brew pin dekopon
```

The pin keeps `brew upgrade` away from running services. Anything running as your login can replace
Homebrew's binaries; the release archives can be installed root-owned instead. Dekopon creates no
accounts or services. Start each daemon with `--log-file`, since launchd never rotates stdout, and
keep the Mac awake. While the gateway is connected, the broker reloads an edited secret map, policy
file or `broker.yaml` key listed in `SWAP` ([`config.rs`](../crates/dekopon-brokerd/src/config.rs))
within seconds, and refuses any other change until a restart.

### One account

Both daemons run as your login, so you edit configuration in place in a git checkout, and an agent
running as you can read it. Anything else running as you can read the credentials too.

1. Keep credentials, the socket, the journal and logs in a `0700` directory the checkout ignores.
2. Map only your UID in the broker's `identities`, as `dekopon-gatewayd` with its `attestor` grant;
   the gateway's `broker.serverUid` defaults to its own UID.
3. Install two LaunchAgents with `KeepAlive`, `Umask` 63 and an `ExitTimeOut` above the shutdown
   grace.
4. To restart, run both `check`s, then `launchctl bootout gui/$UID/<label>` the gateway, then the
   broker, waiting until `launchctl print` fails, since `bootout` returns first. Then `launchctl
   bootstrap gui/$UID` the broker, wait for `dekopon-brokerd probe --socket <path>`, and bootstrap
   the gateway.

LaunchAgents run only while you are logged in, and FileVault rules out automatic login after a
reboot.

### Two accounts

Every edit goes through `sudo`, since a file must belong to the daemon that reads it. As an
administrator:

1. Create a group, such as `dekopon-ipc`, and two hidden non-login accounts in it, `_dekopon_broker`
   and `_dekopon_gateway`.
2. Give each account a `0700` directory, and the broker a `0710` socket directory in the group. An
   old Homebrew under `/usr/local` often leaves `/usr/local/etc` or `/usr/local/var` owned by your
   login, failing the [ancestor check](#files).
3. Create each credential file empty, `0600`, owned by its daemon, then edit it with
   `sudo -u <account> -e`: chat and model tokens in the gateway's `--env-file`, provider
   secrets in the files the broker's secret map names. The broker reads no credential from its
   environment.
4. Map both UIDs in the broker's `identities`, the gateway's and its own for `probe`, and set the
   gateway's `broker.serverUid`.
5. Run each daemon's `check` as its account, then both once in the foreground.
6. Install two LaunchDaemons with `UserName`, `KeepAlive`, and an `ExitTimeOut` above the shutdown
   grace. Load the broker, wait for `dekopon-brokerd probe --socket <path>`, then the gateway. At
   boot the order is arbitrary; `KeepAlive` relaunches a gateway that found no broker.

## Slack

1. Create the app from [`manifest-agent.yaml`](../examples/slack/manifest-agent.yaml) with the
   transport's `experience: agent`. When the workspace plan or admin policy doesn't allow Slack's
   Agent features, use [`manifest.yaml`](../examples/slack/manifest.yaml) with the default
   `experience: classic`.
2. Generate an app-level token (`xapp-…`) with the `connections:write` scope, which no manifest can
   declare, then install the app for its bot token (`xoxb-…`).
3. Put both in the gateway's environment under the names `appTokenEnv` and `botTokenEnv` give: its
   `--env-file` on a Mac, `gateway.env` Secret references on Kubernetes.
4. A member's ID (`U…`) is under **Copy member ID** in their profile; the web client's URL,
   `app.slack.com/client/T…/C…`, shows the team ID, then the channel's. Principals and attestor
   scopes take them as `drn:slack:user:T…:U…` and `drn:slack:*:T…:*`.

## Upgrades

Upgrade both daemons together from one release; their protocol has no cross-release negotiation.

1. Run the new release's `dekopon-brokerd check` and `dekopon-gatewayd check` on the configuration
   each will serve; start nothing until both pass.
2. Stop the gateway, then the broker.
3. Replace both binaries; with a managed provider set, run the new broker's `provider precompile`.
4. Start the broker, wait for its `probe`, then start the gateway.

`helm upgrade` does this in one pod, broker first and last; `configCheck.enabled` runs both checks
before the release applies. With Homebrew, step 3 is `brew unpin dekopon && brew upgrade dekopon &&
brew pin dekopon`, which deletes the previous keg: keep the previous release archive. On one
account, steps 2 and 4 are its [restart](#one-account) order.

To roll back, run the same steps with the previous pair and the configuration and provider lock it
ran. Nothing migrates backwards:

- `precompile` prunes components compiled for other engines, so the old broker needs its own run.
- The gateway deletes a journal file it can't read, with that conversation's history.
- `helm rollback` skips the pre-upgrade hooks; `helm upgrade` to the previous chart version runs
  them.
- Rolling back undoes no external effect.
