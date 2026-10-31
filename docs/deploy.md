# Deploy

Kubernetes is the preferred deployment. The Mac path serves people moving from OpenClaw or Hermes.
Both run `dekopon-brokerd`, holding policy and provider credentials, and `dekopon-gatewayd`, holding
chat and model credentials, under separate UIDs joined by one Unix socket.

## Kubernetes

```console
helm upgrade --install dekopon oci://ghcr.io/dekopon-agents/charts/dekopon \
  -n dekopon --create-namespace -f my-values.yaml
```

This installs the newest chart; `--version <chart-version>` pins one. The chart has its own version,
and its `appVersion` names the release it deploys. [`values.yaml`](../charts/dekopon/values.yaml) is
the reference. The defaults start a broker with nothing to run; a chat deployment sets
`gateway.enabled` and supplies policy, configuration, a catalog and credentials, as
[`examples/conditional-write`](../examples/conditional-write/) does.

What the values don't make obvious:

- **The broker must map the gateway.** The gateway runs as UID 65533 and the broker as 65532. With
  `broker.configDirectory.peers` the chart renders that mapping, with the gateway's `attestor`
  grant. Inline configuration writes it into the broker's `identities` itself and sets the gateway's
  `broker.socketPath: /run/dekopon/broker.sock` and `broker.serverUid: 65532`; otherwise the broker
  refuses the gateway.
- **An init container copies the broker's files.** The broker refuses a configuration, policy or
  credential file that is a symlink, has a second link, belongs to another UID or is group-readable.
  A projected Secret key is a symlink and a `subPath` file is root-owned, so no volume passes. The
  `prepare-files` init container, root with only `CHOWN` and `FOWNER`, copies each into a memory
  volume as a fresh `0600` file. The namespace must admit it: Pod Security `baseline`, not
  `restricted`. A changed existing Secret reaches the broker only when you restart the pod.
- **The UIDs are fixed.** Broker `65532`, gateway `65533`, console `65535`, sharing only group
  `65534` for the socket. The image's providers belong to `65532`, and the broker loads only
  components it owns.
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
Homebrew's binaries; the release archives can be installed root-owned instead.

Dekopon creates no accounts or services. As an administrator:

1. Create a group, such as `dekopon-ipc`, and two hidden non-login accounts in it, `_dekopon_broker`
   and `_dekopon_gateway`. Under one UID the gateway could read the broker's credentials.
2. Give each account a `0700` directory, and the broker a `0710` socket directory in the group.
   Every ancestor must be owned by root and not group- or world-writable; an old Homebrew under
   `/usr/local` often leaves `/usr/local/etc` or `/usr/local/var` writable by your login.
3. Create each credential file empty, `0600`, owned by its daemon, then edit it with
   `sudo -u <account> -e`: chat and model tokens in the gateway's `--env-file`, provider
   credentials in the broker's `credentialsPath`. The broker reads no credential from its
   environment and, while the gateway is connected, reloads an edited file without a restart.
4. Map both UIDs in the broker's `identities`, the gateway's and its own for `probe`, and set the
   gateway's `broker.serverUid`.
5. Run each daemon's `check` as its account, then both once in the foreground.
6. Install two LaunchDaemons with `UserName`, `KeepAlive`, and an `ExitTimeOut` above the shutdown
   grace. Load the broker, wait for `dekopon-brokerd probe --socket <path>`, then the gateway. At
   boot the order is arbitrary; `KeepAlive` relaunches a gateway that found no broker.

Keep the Mac awake, and start each daemon with `--log-file`; launchd never rotates stdout.

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
   `app.slack.com/client/T…/C…`, shows the team ID, then the channel's. The broker's `principals`
   and the gateway identity's `attestor.namespaces` take them lowercased.

## Upgrades

Upgrade both daemons together from one release; their protocol has no cross-release negotiation.

1. Run the new release's `dekopon-brokerd check` and `dekopon-gatewayd check` on the configuration
   each will serve; start nothing until both pass.
2. Stop the gateway, then the broker.
3. Replace both binaries; with a managed provider set, run the new broker's `provider precompile`.
4. Start the broker, wait for its `probe`, then start the gateway.

`helm upgrade` does this in one pod, broker first and last; `configCheck.enabled` runs both checks
before the release applies. With Homebrew, step 3 is `brew unpin dekopon && brew upgrade dekopon &&
brew pin dekopon`, which deletes the previous keg: keep the previous release archive.

To roll back, run the same steps with the previous pair and the configuration and provider lock it
ran. Nothing migrates backwards:

- `precompile` prunes components compiled for other engines, so the old broker needs its own run.
- The gateway deletes a journal file it can't read, with that conversation's history.
- `helm rollback` skips the pre-upgrade hooks; `helm upgrade` to the previous chart version runs
  them.
- Rolling back undoes no external effect.
