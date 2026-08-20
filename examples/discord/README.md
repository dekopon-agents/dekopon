# Discord bot setup

`dekopond` connects to Discord Gateway v10 over an outbound WebSocket, so it needs no inbound HTTP
endpoint. It answers direct messages and explicit bot mentions in guild channels, including Discord
thread channels. Photos and files use the same lazy `Chat Asset #N` flow as Slack and Telegram: the
model sees bounded metadata first and downloads bytes only if it calls `fetch_chat_asset`.

## 1. Create and install the bot

1. In the [Discord Developer Portal](https://discord.com/developers/applications), create an
   application and add a bot.
2. On the bot page, reset/copy the bot token and store it as a secret. Do not put it in
   `dekopond.yaml`.
3. Open the application's **OAuth2** page. Its address has this shape (the application ID below is
   illustrative; use the ID Discord assigned to your application):

   ```text
   https://discord.com/developers/applications/123456789012345678/oauth2
   ```

4. Scroll down to **OAuth2 URL Generator**. Under **Scopes**, select only **bot**. Selecting that
   checkbox expands the **Bot Permissions** section below it.
5. In the expanded permissions, select exactly these four entries and leave **Administrator** and
   every other permission unchecked:
   - **View Channels** under General Permissions
   - **Send Messages** under Text Permissions
   - **Send Messages in Threads** under Text Permissions
   - **Read Message History** under Text Permissions
6. Copy the **Generated URL** at the bottom of the form. The URL used for the one-time server
   installation step will look like this; again, the application ID is illustrative:

   ```text
   https://discord.com/oauth2/authorize?client_id=123456789012345678&permissions=274877975552&integration_type=0&scope=bot
   ```

   Open that URL, choose the intended server, and authorize the app. The installing account needs
   permission to manage that server. This does not require a redirect URI, client secret,
   `applications.commands` scope, or OAuth2 code grant.
7. The transport identifies with the non-privileged `GUILD_MESSAGES` and `DIRECT_MESSAGES` intents.
   Leave Presence, Server Members, and Message Content under **Privileged Gateway Intents** disabled.
   Discord exposes content and attachments in DMs and in guild messages that explicitly mention the
   bot, which is the exact wakeup surface the gateway accepts.

The transport implements heartbeat ACK detection, Resume after reconnect, Invalid Session handling,
fatal close-code handling, identify/session-start limits, and jittered reconnect backoff. Slash
commands and interactions are not used.

## 2. Configure the transport

```console
export DEKOPOND_DISCORD_BOT_TOKEN='...'
```

```yaml
transports:
  - name: community-discord
    kind: discordGateway
    botTokenEnv: DEKOPOND_DISCORD_BOT_TOKEN

routes:
  - transport: community-discord
    match: { kind: directMessage }
    agent: reviewer
  - transport: community-discord
    match: { kind: channel, channel: "123456789012345678" }
    agent: reviewer
  - transport: community-discord
    match: { kind: channel } # any other channel where the bot is mentioned
    agent: reviewer
```

Enable **Developer Mode** in Discord and use **Copy Channel ID** to obtain a channel snowflake.
Discord threads are channels in the Gateway API, so a thread has its own channel ID. A catch-all
channel route covers it automatically; a route naming only the parent channel does not also claim
its transient thread IDs.

In guild channels, Discord's structured `mentions` array decides whether the bot was addressed.
Ambient messages never start a model session. Direct messages are addressed by definition. Bot,
webhook, self-authored, and system messages are dropped.

## 3. Map Discord users at the broker

Discord user IDs are global, so the canonical subject is `discord.<user id>` with no guild segment.
Use **Copy User ID** in Developer Mode:

```yaml
identities:
  - uid: 65532
    principal: dekopond-gateway
    actor: { kind: service, id: dekopond-gateway }
    attestor:
      namespaces: [discord]

identityMappings:
  - subject: discord.987654321098765432
    principal: maintainer
```

The subject is still routing metadata rather than authority. The broker alone resolves it through
`identityMappings`, and Cedar must separately permit that principal to drive the routed agent.
An unmapped Discord sender is refused before any model call.

## Photos and files

A Discord attachment contributes its untrusted filename, media type, and reported size to the
conversation inventory. The existing gateway bounds still apply: supported image/document media
types only, 8 MiB streamed maximum per fetch, four fetches per session, and 32 retained references
per conversation. A route's model needs `modalities: [image]` before photos are offered; documents
do not require image support.

Downloads accept only HTTPS Discord CDN/media hosts in production, never carry the bot token, do
not follow redirects, and stop streaming at the byte ceiling. Discord attachment URLs are signed
and expire. If a retained URL returns 401, 403, or 404, the gateway re-reads the exact source message
from Discord REST, selects the same attachment ID, validates the refreshed CDN URL, and retries the
bounded download. Attachment bytes are dropped after the model request and are never written to
conversation history.

## Replies

Discord limits one message to 2,000 characters, so a bounded Dekopon answer is split without losing
text when necessary. The first guild post is a native reply to the request; subsequent chunks stay
in the same channel. Every post sets an empty `allowed_mentions` policy with `replied_user: false`,
so model-authored `@everyone`, role names, user mentions, and reply references cannot generate
notifications. Discord Markdown is sent unchanged.
