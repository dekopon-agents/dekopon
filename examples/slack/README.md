# Slack setup

This directory contains the Slack app used by the
[PR summarizer and linter](../pr-summarizer-linter/README.md). It receives direct messages and
explicit channel mentions over Socket Mode, opens supported attachments on demand, and posts
replies. No public endpoint is needed.

## Create the app

1. Open [Your Apps](https://api.slack.com/apps), select **Create New App → From a manifest**, choose
   a workspace, and paste [`manifest.yaml`](manifest.yaml).
2. Under **Basic Information → App-Level Tokens**, create a token with `connections:write`. Save the
   resulting `xapp-…` value.
3. Under **Install App**, install the app to the workspace. Save the bot token (`xoxb-…`).
4. Export both tokens using the names in `dekopond.yaml`:

   ```console
   export DEKOPOND_SLACK_APP_TOKEN=xapp-...
   export DEKOPOND_SLACK_BOT_TOKEN=xoxb-...
   ```

The manifest already enables Socket Mode, the app Home messages tab, and the minimum bot scopes for
DMs, mentions, replies, and attachment reads.

## Allow a sender

Find the workspace team ID (`T…`) and the sender’s member ID (`U…`, available from **Copy member
ID** in their profile). Lowercase both and add the canonical subject to `broker.yaml`:

```yaml
identityMappings:
  - subject: slack.t0123abc.u9xyz
    principal: maintainer
```

Also make sure the gateway identity’s attestor namespace covers that workspace:

```yaml
attestor:
  namespaces: [slack.t0123abc]
```

An unmapped sender is refused before any model call. In channels, the app responds only when it is
mentioned; direct messages route normally.
