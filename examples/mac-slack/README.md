# Slack and OpenRouter on a Mac

This is a narrow starter for the [Mac runbook](../../docs/run-on-mac.md): one Slack sender,
one OpenRouter model, and one read-only tool that fetches synthetic JSONPlaceholder posts.
It grants no GitHub, host-shell, file, or provider-write access. Add useful providers later with
their own reviewed grants; this example establishes that the installation can answer and run a tool.

The example targets **Dekopon 0.72.0** and **JSONPlaceholder provider 0.7.0**. Both released Linux
daemons passed the configuration checks, broker probe, and a local-transport turn with that
provider against loopback model/API fixtures. That verifies the component and schema together;
it does not verify macOS startup, separate service UIDs, a real Slack installation, or an actual
OpenRouter request. Complete the Mac runbook's acceptance checks before switching another bot off.
Use the [0.72.0 source](https://github.com/dekopon-agents/dekopon/tree/v0.72.0) for the matching
contracts; `main` can be ahead of the release.

## Accounts, files, and one provider

First install the two daemons and prepare the service accounts and directories in the
[Mac runbook](../../docs/run-on-mac.md). This example expects `_dekopon_broker` and
`_dekopon_gateway` to have distinct UIDs and the shared IPC group described there.
The broker owns `/usr/local/etc/dekopon/broker/`; the gateway owns
`/usr/local/etc/dekopon/gateway/`. Each directory is private to its service account.
The shared socket is `/usr/local/var/run/dekopon/broker.sock`.

The Homebrew `dekopon` formula does not install this provider. In a private download directory,
using a recent GitHub CLI that includes `gh attestation`:

```console
gh release download v0.7.0 --repo dekopon-agents/dekopon-provider-jsonplaceholder \
  --pattern 'jsonplaceholder-provider.wasm*'
shasum -a 256 -c jsonplaceholder-provider.wasm.sha256
gh attestation verify --repo dekopon-agents/dekopon-provider-jsonplaceholder \
  --signer-repo dekopon-agents/provider-workflows jsonplaceholder-provider.wasm
sudo install -o _dekopon_broker -g "$(id -gn _dekopon_broker)" -m 0600 \
  jsonplaceholder-provider.wasm /usr/local/etc/dekopon/broker/jsonplaceholder-provider.wasm
```

The [provider release](https://github.com/dekopon-agents/dekopon-provider-jsonplaceholder/releases/tag/v0.7.0)
is separate from the application release. Use these component bytes with the constraints below;
older provider README examples use fields that 0.72.0 rejects. There is no need for Docker, an OCI
registry client, a Rust build, or a provider API key for this starter.

## Broker configuration and policy

Save these as the named files. Replace `BROKER_UID` and `GATEWAY_UID` with the numeric output of
`id -u _dekopon_broker` and `id -u _dekopon_gateway`. Replace `t0123abc` and `u0123abc` everywhere
with the lowercased Slack workspace and member IDs from the [Slack setup](../slack/README.md).
These are IDs, not names or email addresses.

`/usr/local/etc/dekopon/broker/broker.yaml`:

```yaml
apiVersion: dekopon.dev/brokerd/v1alpha1
socketPath: /usr/local/var/run/dekopon/broker.sock
policiesPath: policies.cedar
strict: true
providers: [jsonplaceholder-provider.wasm]
identities:
  - uid: BROKER_UID
    principal: broker-operator
    actor: {type: service, principal: broker-operator}
  - uid: GATEWAY_UID
    principal: dekopon-gatewayd
    actor: {type: service, principal: dekopon-gatewayd}
    attestor: {namespaces: [slack.t0123abc]}
principals:
  operator:
    subjects: [slack.t0123abc.u0123abc]
capabilities:
  jsonplaceholder:
    capabilities:
      jsonplaceholder.posts.get:
        constraints:
          timeoutMs: 10000
          http:
            allowedHosts: [jsonplaceholder.typicode.com]
            allowedMethods: [GET]
            maxRequests: 1
            maxRequestBytes: 8192
            maxResponseBytes: 65536
            allowPlaintextLoopback: false
```

Keep the gateway peer principal literally `dekopon-gatewayd`: 0.72.0 uses that name and its
attestor grant for the gateway's startup/reload handshake. Another name can pass offline checks
and still fail boot with `peer is not the reconciliation owner`. The separate `broker-operator`
entry lets a health probe authenticate as the broker's own UID; it grants no provider call.

`/usr/local/etc/dekopon/broker/policies.cedar`:

```cedar
@id("operator-may-prompt")
permit(principal == Dekopon::Principal::"operator",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"home-assistant")
when { context has via && context.via == "dekopon-gatewayd" };

@id("operator-may-read-posts")
permit(principal == Dekopon::Principal::"operator",
       action == Dekopon::Action::"jsonplaceholder.posts.get",
       resource == Dekopon::Provider::"jsonplaceholder")
when { context has agent && context.agent == "home-assistant"
    && context has via && context.via == "dekopon-gatewayd" };
```

The prompt permission and the provider permission are separate. The component also knows how to
create a synthetic post, but this broker grants only GET. Removing every provider does not make a
chat-only bot: the broker refuses an empty provider set, and the gateway refuses an empty
capability grant before calling the model.

## Gateway catalog and configuration

`/usr/local/etc/dekopon/gateway/agents.yaml`:

```yaml
apiVersion: dekopon.dev/v1alpha1
kind: Agent
metadata: {name: home-assistant}
spec:
  description: A bounded personal Slack assistant
  enabled: true
  instructions: >-
    Answer concisely using only the granted tools. When asked to test the installed tool,
    run placeholder posts get --post-id 7 and report the returned title.
  modelClass: general
status: Ready
```

`/usr/local/etc/dekopon/gateway/gateway.yaml`:

```yaml
apiVersion: dekopon.dev/gatewayd/v1alpha1
catalogPath: agents.yaml
broker:
  socketPath: /usr/local/var/run/dekopon/broker.sock
  serverUid: BROKER_UID
transports:
  - name: slack
    kind: slackSocketMode
    experience: classic
    indicators:
      mode: native
      reaction: {working: auto, seen: off}
      progress: message
      cancelButton: true
    appTokenEnv: DEKOPON_GATEWAYD_SLACK_APP_TOKEN
    botTokenEnv: DEKOPON_GATEWAYD_SLACK_BOT_TOKEN
models:
  - name: router
    kind: openrouter
    model: replace-with-exact-openrouter-model-id
    apiKeyEnv: OPENROUTER_API_KEY
    timeoutMs: 120000
    classes: [general]
    generation: {maxOutputTokens: 2048}
    routing: {allowFallbacks: false, requireParameters: true}
routes:
  - transport: slack
    conversation: {kind: [directMessage]}
    subjects: [slack.t0123abc.u0123abc]
    agent: home-assistant
    model: router
    limits:
      maxSteps: 4
      maxCapabilityCalls: 4
      maxDurationMs: 180000
    memory:
      mode: persistent
      scope: privateConversation
      recall: none
      summaries: off
      maxTurns: 6
      maxBytes: 32768
sessions:
  maxConcurrent: 1
  maxConversations: 16
  assetRetentionBytes: 0
metering:
  budgets:
    home-assistant:
      meters:
        - {kind: rolling, limit: 100000, period: 1d}
shutdownGraceMs: 120000
```

Choose one exact OpenRouter model ID that supports tool calls. The configuration uses the native
`openrouter` client; do not add `endpoint` or `stream` fields to this model kind. Disabling fallback
does not pin an upstream provider. Add a supported `routing.only` list only if you need that
restriction; [OpenRouter settings](../../docs/gatewayd.md#openrouter-model-settings) describe the fields.

Create the Slack app from the classic [0.72.0 manifest](https://github.com/dekopon-agents/dekopon/blob/v0.72.0/examples/slack/manifest.yaml), then obtain the
`xapp-…` app token with `connections:write` and the installed bot's `xoxb-…` token using the
[Slack setup](../slack/README.md). The manifest enables more features than this DM-only route uses. Its scopes do not grant a sender
broker permission, and no channel route is enabled here. No public listener is needed.

Put those two values and `OPENROUTER_API_KEY` in the gateway service's private environment source
from the Mac runbook, using the exact names above. The YAML contains variable names, never tokens.
Set an OpenRouter key spending limit before starting the service. The local 100,000-token rolling
limit is an additional guard, not a dollar cap: it counts raw input plus output, can settle above
an estimate, and starts empty after a restart. No telemetry-backed restore is configured here.

The six-turn window is in memory and expires after the default 15-minute idle timeout. A restart
forgets it; this starter writes no journal, runs no background summaries, and retains no attachments.
The offline checker warns that persistent history has no journal. That warning is expected for
this bounded starter.

## Check, start, and accept

Make broker configuration and policy files owned by `_dekopon_broker`, gateway configuration and
catalog files owned by `_dekopon_gateway`, and all four files mode `0600`. Keep both directories
mode `0700`. The shared IPC directory has different permissions; follow the Mac runbook rather
than making either private configuration directory group-readable.

```console
sudo -u _dekopon_broker /usr/local/lib/dekopon/current/dekopon-brokerd check /usr/local/etc/dekopon/broker/broker.yaml
sudo -u _dekopon_gateway /usr/local/lib/dekopon/current/dekopon-gatewayd check /usr/local/etc/dekopon/gateway/gateway.yaml
```

The commands use the versioned installation selected by the Mac runbook. The checks load the provider and validate authored configuration and policy. They do not prove
runtime directory permissions, socket access, credentials, or network connectivity. With the
OpenRouter key loaded in the gateway service environment, `dekopon-gatewayd check … --probe`
also sends a small paid tool-call canary; it does not exercise Slack or the broker.

Start the broker through the Mac runbook, then probe as its owner:

```console
sudo -u _dekopon_broker /usr/local/lib/dekopon/current/dekopon-brokerd probe --socket /usr/local/var/run/dekopon/broker.sock
```

Success is exit status 0 with no output. The probe has no `--server-uid` option and expects the
server UID to equal its own effective UID; running it as `_dekopon_gateway` is not the cross-UID
acceptance check. Start the gateway, then have the authorized Slack user send the bot a DM asking it
to test the installed tool. Expect a post title, a successful `jsonplaceholder.posts.get` broker
execution record, and matching usage in OpenRouter. A second, unmapped Slack sender must receive
no model-backed response.

Finally perform the Mac runbook's service restart, reboot/login, sleep, and spending-limit checks.
Retain the installed binaries, provider checksum, and these configuration files together for
rollback. This example migrates no OpenClaw configuration, history, skills, or credentials.
