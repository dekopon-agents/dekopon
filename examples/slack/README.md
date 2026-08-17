# Slack app for the dekopond gateway

[`manifest.yaml`](manifest.yaml) creates a Socket Mode Slack app with the minimum
surface the gateway consumes: DM events, explicit @-mentions in channels, and reply
authority. The manifest's own comments walk through creation and the two tokens.

This is step one of the [rubber-stamper walkthrough](../rubber-stamper/README.md), which
carries it through to a boss DMing a bot and a pull request getting approved under a
broker-held credential nothing in the session can see.

## From app to a running gateway

1. Create the app from the manifest (https://api.slack.com/apps → From a manifest).
2. Generate an app-level token with `connections:write` (`xapp-...`) and install the
   app to the workspace for the bot token (`xoxb-...`).
3. Export both under the names your dekopond config declares:

   ```console
   export DEKOPOND_SLACK_APP_TOKEN=xapp-...
   export DEKOPOND_SLACK_BOT_TOKEN=xoxb-...
   ```

   ```yaml
   # dekopond.yaml
   transports:
     - name: workspace-slack
       kind: slackSocketMode
       appTokenEnv: DEKOPOND_SLACK_APP_TOKEN
       botTokenEnv: DEKOPOND_SLACK_BOT_TOKEN
   ```

4. Map the humans. The broker — not the gateway — decides who a Slack user is, from
   canonical subjects in its owner-only configuration. The team ID is the `T...` value
   in the workspace URL (or `auth.test`); member IDs are the `U...` values under a
   profile's "Copy member ID". Both are lowercased into the canonical form:

   ```yaml
   # broker.yaml
   identityMappings:
     - subject: slack.t0123abc.u9xyz
       principal: cpetersen
   ```

   An unmapped sender is refused before any model call, so the app can be installed
   workspace-wide without widening who can actually reach an agent.

A message the app cannot see (a channel it is not in, an unaddressed channel message)
never reaches dekopond at all; a message it can see still has to pass the broker's
attestor grant, identity mapping, and policy before an agent does anything.
