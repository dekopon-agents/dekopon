# The conditional writer

A Slack DM asks an agent to update a shared record. [Cedar](policies.cedar) lets the mapped sender,
`cpetersen`, reach two `http-probe` capabilities, only via the gateway, only for this agent. The
write refuses if the record moved. The broker adds the API token; the model, shell and component
never see it.

## Run it

1. Create a Slack app from [`manifest-agent.yaml`](../slack/manifest-agent.yaml). Note both
   tokens, the team ID and your member ID.
2. Copy `broker-credentials.yaml.example` to `broker-credentials.yaml`, `chmod 600` it, and paste
   your API's bearer token over `replace-me_…`.
3. In `broker.yaml` and `gatewayd.yaml`, set your socket path, your UID for `501`, and your
   lowercased team and member IDs for `t0123abcd` and `u0123abcd`. Set your API's host for
   `api.example.com` in `broker.yaml` and the credentials file.
4. Make the socket directory `0700`, and both configurations and `policies.cedar` `0600`.
5. Serve the model `gatewayd.yaml` names; start each daemon in its own terminal:

   ```console
   dekopon-brokerd --config broker.yaml
   DEKOPON_GATEWAYD_SLACK_APP_TOKEN=xapp-… DEKOPON_GATEWAYD_SLACK_BOT_TOKEN=xoxb-… \
     dekopon-gatewayd --config gatewayd.yaml
   ```

6. DM the bot a record URL and the state to set.

## What the run shows

- At startup the gateway, acting for itself, reports `capability.count: 0`.
- Any other sender is refused before a model call.
- `http-probe.purge` has no constraint set, so it never runs.
- The broker's stdout records each execution with the principal, the policy and
  `credentialInjected: true`, never the token.

Both daemons run as your UID here, so any process of yours can act as the gateway. The chart
separates them.
