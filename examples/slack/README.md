# Slack setup

Create the Slack app, export its two tokens, and map senders and channels to broker principals.
`dekopon-gatewayd` connects outbound over Socket Mode; no public endpoint is needed. On a Mac, start
from [Run on a Mac](../../docs/run-on-mac.md), which links here. The
[review channel](../slack-github/README.md), [DM starter](../mac-slack/README.md) and
[conditional writer](../conditional-write/README.md) recipes use this app.

1. [Choose a manifest](#choose-a-manifest).
2. [Create the app and credentials](#create-the-app-and-credentials).
3. [Allow a sender](#allow-a-sender) and invite the bot.
4. Optionally, configure [attachment replies](#configure-attachment-replies),
   [in-flight indicators](#configure-in-flight-indicators) and [thread follow](#follow-a-thread).

## Choose a manifest

Choose before creating the app; each manifest pairs with one transport `experience`.

| | Classic | Agent |
|---|---|---|
| Manifest | [`manifest.yaml`](manifest.yaml) | [`manifest-agent.yaml`](manifest-agent.yaml) |
| Transport | `experience: classic` (default) | `experience: agent` |
| Workspace | Free or paid | Slack Agent features enabled by plan and admin policy |
| In-flight indicator | Temporary `:tangerine:` reaction when configured | Native Working/Stop session UI, falling back to the reaction |
| Direct messages | Top-level replies, one conversation per DM | One thread and session per root message |
| Channel thread without `follow` | Each message must mention the bot | After an authorized mention, the same sender continues without one |
| Stop | A `cancelButton: true` button, or a `stop` or `cancel` reply | Slack's Stop control (`cancelButton` is refused), or a `stop` or `cancel` reply |

- Slack's [Agent guide](https://docs.slack.dev/ai/developing-agents/) says some AI features need a
  paid plan even though their settings appear on every plan; the
  [Developer Program](https://api.slack.com/developer-program) offers a fully featured sandbox.
  Choose Agent only where the plan and admin policy enable it; Dekopon never infers the plan.
- A refused authorization or a gateway restart ends an Agent continuation. Another sender must
  mention the bot and is authorized separately.

## Create the app and credentials

`dekopon-gatewayd` reads two Slack tokens from environment variables:

| Credential | Prefix | Purpose | Environment variable |
|---|---|---|---|
| App-level token | `xapp-…` | Opens the outbound Socket Mode connection | `DEKOPON_GATEWAYD_SLACK_APP_TOKEN` |
| Bot User OAuth Token | `xoxb-…` | Identifies the bot, posts replies and liveness, and reads and uploads attachments | `DEKOPON_GATEWAYD_SLACK_BOT_TOKEN` |

Keep both tokens out of the manifest, every Dekopon configuration file, issues and commits.

### Create the app

1. In [Your Apps](https://api.slack.com/apps), select **Create New App → From a manifest**.
2. Choose the workspace, paste the classic [`manifest.yaml`](manifest.yaml) or the Agent
   [`manifest-agent.yaml`](manifest-agent.yaml), and create the app.
3. Select the new app in [Your Apps](https://api.slack.com/apps). Its **Basic Information** page is
   `https://api.slack.com/apps/{APP_ID}/general`.

Both manifests enable Socket Mode and the App Home messages tab, and request:

- `files:write`, for provider attachment replies; remove it only when no route grants `asset.send`.
- `reactions:write`, for optional working and seen reactions; remove it and leave both reactions
  off for final replies only.
- `channels:history` with `message.channels` and `groups:history` with `message.groups`, for
  [thread follow](#follow-a-thread) and Agent continuation. Other unmentioned channel messages are
  dropped before routing.
- `reactions:read`, `reaction_added` and `users:read`, so a human thumbs-down ends following.

The Agent manifest adds the `agent_view` feature, the `assistant:write` scope, and the
`agent_session_stopped` and `app_home_opened` events. Opening App Home is not a prompt.

### Generate the app-level token (`xapp-…`)

1. On **Basic Information**, under **App-Level Tokens**, select **Generate Token and Scopes**.
2. Name the token, for example `dekopon`, select **Add Scope**, and choose `connections:write`.
   It is the only scope this token needs; bot scopes do not belong here.
3. Select **Generate**. It enables once the name and scope are set; leave any extra empty scope
   selector empty.
4. Select **Copy** and save the `xapp-…` value as `DEKOPON_GATEWAYD_SLACK_APP_TOKEN`.

The **App-Level Tokens** table then lists the token with `connections:write`. Select its name to
reopen **Copy**, or to **Revoke** an exposed token.

### Install the app and get the bot token (`xoxb-…`)

1. In the settings sidebar, select **Install App → Install to Workspace**, review the bot
   permissions, and select **Allow**.
2. In **OAuth & Permissions**, copy the **Bot User OAuth Token** and save the `xoxb-…` value as
   `DEKOPON_GATEWAYD_SLACK_BOT_TOKEN`.

After changing bot scopes, select **Reinstall to Workspace** so they take effect.

### Export the credentials

Export both under the variable names `gatewayd.yaml` references:

```console
export DEKOPON_GATEWAYD_SLACK_APP_TOKEN=xapp-...
export DEKOPON_GATEWAYD_SLACK_BOT_TOKEN=xoxb-...
```

## Allow a sender

Collect the Slack IDs, invite the bot, then map each sender:

1. Copy the sender's member ID (`U…`) with **Copy member ID** in their Slack profile.
2. Open the channel in a web browser. Its URL, `https://app.slack.com/client/T…/C…`, holds the
   workspace ID (`T…`) and then the channel ID (`C…`).
3. In each channel a route names, run `/invite @dekopon`.

Lowercase the workspace and member IDs and add the canonical subject to `broker.yaml`:

```yaml
principals:
  maintainer:
    subjects: [slack.t0123abc.u9xyz]
```

Make sure the gateway identity's attestor namespace covers that workspace:

```yaml
attestor:
  namespaces: [slack.t0123abc]
```

An unmapped sender is refused before any model call. In a channel, a new conversation starts
only with a mention.

## Configure attachment replies

A route delivers a provider file only after the broker authorizes `asset.send`; attaching alone
keeps the file. The Slack adapter accepts any concrete valid media type and refuses invalid
labels, so name the accepted set in the route instructions.

Each file goes through `files.getUploadURLExternal`, a tokenless upload to Slack's returned,
origin-checked URL, then `files.completeUploadExternal` to the authenticated channel or thread.
The bot token goes only to the two Web API calls, never to the upload URL.
[Asset handles](../../docs/gatewayd.md#asset-handles-and-delivery) lists the bounds.

## Configure in-flight indicators

Indicators are opt-in and start only after fresh broker authorization. Set them on the transport.

Classic/free profile:

```yaml
kind: slackSocketMode
experience: classic
indicators:
  mode: native
  reaction: { working: auto, seen: off }
  progress: message
  cancelButton: true
```

Agent profile, optionally showing opted-in route notes on the thread status line:

```yaml
kind: slackSocketMode
experience: agent
indicators:
  mode: native
  reaction: { working: auto, seen: off }
  progress: off
  statusText: true
```

- `statusText: true` shows notes from a route with `progressNotes: true`; omit it for native status
  only. The [status-text contract](../../docs/gatewayd.md#slack-socket-mode) covers its lease and
  hidden Stop control.
- With `reaction.seen: on` on the resolved route, accepted steers and queued non-wake messages get
  a best-effort `eyes`. It acknowledges admission, not permission to run.
- Unauthorized, busy, unrouted and ambient messages get no indicator; a busy refusal can still get
  its text reply.
- An Agent transport that receives `feature_disabled` permanently falls back to its configured
  reaction, or to no indicator without `reactions:write`; replies and `experience` stay unchanged.
- Indicator failures never delay or fail the reply. A failed cleanup can leave a `:tangerine:`
  reaction.
- Stop is cooperative: it blocks further model turns and capability calls, drops the stale answer
  and its history commit, and posts `Stopped.`. A model request or provider effect already running
  cannot be rolled back
  ([stopping a run](../../docs/gatewayd.md#liveness-progress-and-stopping-a-run)).

## Follow a thread

Set `follow: true` on a `[channel, thread]` Slack route and configure `sessions.journal`. After an
authorized `follow stay`, the bot hears every speaker's unmentioned replies in that thread, answers
only when useful, and runs each batch under its newest speaker's fresh authorization.

`mute`, a configured stop word, an authorized `follow leave`, a human thumbs-down on an agent
message in the thread, or expiry ends following; only a delivered reply renews the 24-hour ticket.
[Slack thread follow](../../docs/gatewayd.md#slack-thread-follow) covers batching and installation.
For top-level PR links, also set
[`ambient.pullRequestLinks`](../../docs/gatewayd.md#slack-pull-request-links).
