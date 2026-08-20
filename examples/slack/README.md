# Slack setup

This directory contains the Slack app profiles used by the
[PR summarizer and linter](../pr-summarizer-linter/README.md). Both receive direct messages and
explicit channel mentions over Socket Mode, open supported attachments on demand, publish
best-effort in-flight activity, and post replies. No public endpoint is needed.

Choose the profile before creating the app:

| Profile | Manifest | Workspace support | In-flight UX |
|---|---|---|---|
| Classic | [`manifest.yaml`](manifest.yaml) | Free or paid | Temporary `:tangerine:` reaction when configured |
| Agent | [`manifest-agent.yaml`](manifest-agent.yaml) | Slack Agent feature enabled by plan/admin | Native Working/Stop session UI, degrading to the reaction |

Slack's [Agent guide](https://docs.slack.dev/ai/developing-agents/) says some AI features require a
paid plan; the [Developer Program](https://api.slack.com/developer-program) offers a fully featured
sandbox for development. Slack exposes some Agent settings on free workspaces even when the API is
disabled. Dekopon never infers billing: an Agent installation that receives `feature_disabled` permanently downgrades that
transport to its configured reaction fallback. If that also lacks `reactions:write`, activity is a
no-op and the final reply is unchanged.

## Create the app and credentials

`dekopond` needs two different Slack credentials:

| Credential | Prefix | Purpose | Environment variable |
|---|---|---|---|
| App-level token | `xapp-…` | Opens the outbound Socket Mode connection | `DEKOPOND_SLACK_APP_TOKEN` |
| Bot User OAuth Token | `xoxb-…` | Identifies the bot, publishes activity/replies, and reads attachments | `DEKOPOND_SLACK_BOT_TOKEN` |

Neither token belongs in the app manifest or a Dekopon configuration file.

### Create the app

1. Open [Your Apps](https://api.slack.com/apps) and select **Create New App → From a manifest**.
2. Choose the workspace, paste either the classic [`manifest.yaml`](manifest.yaml) or paid/admin-
   enabled [`manifest-agent.yaml`](manifest-agent.yaml), and finish creating the app.
3. Select the new app from [Your Apps](https://api.slack.com/apps). Its **Basic Information** page
   has a URL like `https://api.slack.com/apps/{APP_ID}/general`, where `{APP_ID}` is the identifier
   Slack assigned to the app. Use that URL to return directly to these settings later.

Both manifests enable Socket Mode and the App Home messages tab. The classic profile adds
`reactions:write` for the explicitly configured fallback; remove that scope and leave activity off
if the classic deployment wants final replies only. The Agent profile additionally adds
`agent_view`, `assistant:write`, and `agent_session_stopped`, plus the `app_home_opened` event Slack
requires for Agent View. Opening App Home is not itself routed as a prompt.

### Generate the app-level token (`xapp-…`)

1. On the **Basic Information** page, scroll down to **App-Level Tokens** and select
   **Generate Token and Scopes**.
2. In the **Generate an app-level token** dialog:
   1. Enter `dekopon` under **Token Name**. This is only a label in Slack; another descriptive name
      works too.
   2. Select **Add Scope**, then choose `connections:write`. This is the only app-level scope
      Dekopon needs. The bot scopes from the manifest do not belong here.
   3. Confirm that the dialog shows the token name and `connections:write`. The **Generate** button
      becomes available only after both are present. If Slack shows another empty permission
      selector, leave it empty; a second scope is not required. Select **Generate**.
3. Slack shows the generated token and its scope. Select **Copy** and save the complete `xapp-…`
   value as `DEKOPOND_SLACK_APP_TOKEN`. After closing the dialog, the **App-Level Tokens** table
   should contain one row named `dekopon` with the `connections:write` scope. Select the token name
   in that table to reopen its **Copy** and **Revoke** controls.

Treat the token as a secret. Do not paste it into `manifest.yaml`, `dekopond.yaml`, an issue, or a
commit. Use **Revoke** in the token details if it is exposed.

### Install the app and get the bot token (`xoxb-…`)

1. Select **Install App** in the Slack app settings sidebar, then select **Install to Workspace**.
2. Review the requested bot permissions and select **Allow**.
3. After installation, copy the **Bot User OAuth Token** from **OAuth & Permissions**. Save the
   complete `xoxb-…` value as `DEKOPOND_SLACK_BOT_TOKEN`.

If the app was already installed, open **OAuth & Permissions** directly to find the bot token. If
bot scopes change later, select **Reinstall to Workspace** so the new scopes take effect.

## Configure in-flight activity

Activity is opt-in and starts only after the sender's fresh broker authorization succeeds. Busy,
unrouted, ambient, and unauthorized messages create no activity.

Classic/free profile:

```yaml
kind: slackSocketMode
experience: classic
activity:
  mode: native
  classicFallback: reaction
```

Agent profile:

```yaml
kind: slackSocketMode
experience: agent
activity:
  mode: native
  classicFallback: reaction
```

`experience` controls conversation semantics and never changes in response to a cosmetic API
failure. Classic DMs retain top-level replies and one whole-DM conversation. Agent DMs use one
Slack thread/session per root message, including their conversation history and Stop key.

In Agent mode, Dekopon sets `processing` once—Slack owns the standard Working UI and one-hour
processing timeout—sends the durable reply, and queues `active` cleanup.
Slack's Stop button produces an authenticated `agent_session_stopped` event. Dekopon cooperatively
prevents subsequent model turns and capability calls, suppresses the stale answer/history commit,
and posts `Stopped.` An already-running model request or provider effect cannot be rolled back and
may finish before the synchronous loop reaches its next cancellation boundary.

Activity failures never delay or fail the terminal reply. `feature_disabled`, `missing_scope`, and
other permanent Agent installation failures trip a per-transport breaker. Reaction cleanup removes
only a reaction this activity generation successfully added; a failed cleanup may leave a harmless
`:tangerine:` marker.

### Export the credentials

Export the values under the environment-variable names referenced by `dekopond.yaml`:

```console
export DEKOPOND_SLACK_APP_TOKEN=xapp-...
export DEKOPOND_SLACK_BOT_TOKEN=xoxb-...
```

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
