# Slack Agent and OpenRouter on a Mac

This recipe gives one Slack sender a DM agent on the [Mac installation](../../docs/run-on-mac.md),
with one OpenRouter model and one read-only tool that fetches synthetic JSONPlaceholder posts. It
proves the installation answers and runs a tool before you add a GitHub PAT; for PR reviews, use
the [review-channel recipe](../slack-github/README.md).

- It grants no GitHub, host-shell, file or provider-write access and needs no provider API key.
- It pins Dekopon 0.74.0 and JSONPlaceholder provider 0.7.0. Read contracts from the
  [0.74.0 source](https://github.com/dekopon-agents/dekopon/tree/v0.74.0), not `main`.
- These files pass `check` on the released Linux 0.74.0 daemons with provider 0.7.0; that proves
  nothing about macOS services, Slack or OpenRouter.

Apply it in this order:

1. Follow the runbook from [Install](../../docs/run-on-mac.md#install) through the accounts,
   directories and `credentials.env` in
   [Prepare the broker and gateway](../../docs/run-on-mac.md#prepare-the-broker-and-gateway).
   Create the Slack app from the
   [0.74.0 Agent manifest](https://github.com/dekopon-agents/dekopon/blob/v0.74.0/examples/slack/manifest-agent.yaml)
   with [Slack setup](../slack/README.md#create-the-app-and-credentials).
2. Here, create five files, each mode `0600`, then [check them](#validate-and-accept):
   - in `/usr/local/etc/dekopon/broker/`, owned by `_dekopon_broker`:
     `jsonplaceholder-provider.wasm`, `broker.yaml` and `policies.cedar`;
   - in `/usr/local/etc/dekopon/gateway/`, owned by `_dekopon_gateway`: `agents.yaml` and
     `gateway.yaml`.
3. In the runbook, [check in the foreground](../../docs/run-on-mac.md#check-before-installing-a-service),
   then [run with launchd](../../docs/run-on-mac.md#run-with-launchd).
4. Here, [accept](#validate-and-accept), then complete the runbook's
   [acceptance](../../docs/run-on-mac.md#acceptance-before-switching) before switching another bot
   off.

## Install the released provider

Download, verify and install the pinned component in a private working directory:

```console
gh release download v0.7.0 --repo dekopon-agents/dekopon-provider-jsonplaceholder \
  --pattern 'jsonplaceholder-provider.wasm*'
shasum -a 256 -c jsonplaceholder-provider.wasm.sha256
gh attestation verify --repo dekopon-agents/dekopon-provider-jsonplaceholder \
  --signer-repo dekopon-agents/provider-workflows jsonplaceholder-provider.wasm
sudo install -o _dekopon_broker -g "$(id -gn _dekopon_broker)" -m 0600 \
  jsonplaceholder-provider.wasm /usr/local/etc/dekopon/broker/jsonplaceholder-provider.wasm
```

Use the [0.7.0 release](https://github.com/dekopon-agents/dekopon-provider-jsonplaceholder/releases/tag/v0.7.0)
with the constraints below; older provider README examples use fields that 0.74.0 rejects.

## Configure the broker

Replace these placeholders in all four configuration files before installing them with the
runbook's [install commands](../../docs/run-on-mac.md#prepare-the-broker-and-gateway):

| Placeholder | Value |
|---|---|
| `BROKER_UID`, `GATEWAY_UID` | Output of `id -u _dekopon_broker` and `id -u _dekopon_gateway` |
| `t0123abc`, `u0123abc` | Slack workspace and member IDs, lowercased ([Allow a sender](../slack/README.md#allow-a-sender)); names and email addresses are not IDs |
| `replace-with-exact-openrouter-model-id` | One exact OpenRouter model ID that supports tool calls ([model settings](../../docs/gatewayd.md#openrouter-model-settings)) |

`check` accepts every placeholder, so confirm none remain.

Save `/usr/local/etc/dekopon/broker/broker.yaml`:

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

- Keep `dekopon-gatewayd` as written: the gateway's startup and reload handshake requires that
  name and its attestor grant. Another name passes `check` and fails boot with
  `peer is not the reconciliation owner`.
- `broker-operator` lets the health probe authenticate as the broker's own UID; it grants no
  provider call.

Save `/usr/local/etc/dekopon/broker/policies.cedar`:

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

- The prompt permit and the provider permit are separate grants. The component can also create a
  synthetic post; the broker grants only GET.
- Keep the tool: the broker refuses an empty provider set and the gateway refuses a sender with
  no granted capability, so there is no chat-only variant.

## Configure the gateway

Save `/usr/local/etc/dekopon/gateway/agents.yaml`:

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

Save `/usr/local/etc/dekopon/gateway/gateway.yaml`:

```yaml
apiVersion: dekopon.dev/gatewayd/v1alpha1
catalogPath: agents.yaml
broker:
  socketPath: /usr/local/var/run/dekopon/broker.sock
  serverUid: BROKER_UID
transports:
  - name: slack
    kind: slackSocketMode
    experience: agent
    indicators:
      mode: native
      reaction: {working: auto, seen: off}
      progress: auto
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

- The only route is DMs from the mapped sender; no channel route exists. Slack scopes grant a
  sender no broker permission.
- Agent DMs are thread-scoped: reply in the bot's thread to keep the conversation.
- The gateway's `credentials.env` holds the two Slack tokens and `OPENROUTER_API_KEY` under the
  names above; the YAML holds only variable names.
- Set a spending limit on the OpenRouter key before starting the service
  ([budget](../../docs/run-on-mac.md#prepare-slack-and-the-model)). The 100,000-token daily
  `metering` limit counts raw input plus output, is not a dollar cap, can settle above its
  estimate, and starts empty after a restart because no telemetry restore is configured.
- The six-turn window lives in memory, expires after 15 idle minutes and is lost on restart. No
  journal, background summary or attachment retention is configured.

## Validate and accept

Check both files as their owners:

```console
sudo -u _dekopon_broker /usr/local/lib/dekopon/current/dekopon-brokerd check /usr/local/etc/dekopon/broker/broker.yaml
sudo -u _dekopon_gateway /usr/local/lib/dekopon/current/dekopon-gatewayd check /usr/local/etc/dekopon/gateway/gateway.yaml
```

The checks load the provider and validate configuration and policy; they prove nothing about
directory permissions, socket access, credentials or network. Expected warnings: persistent memory
has no journal, and the gateway reads three variables at boot. The runbook's
[`check --probe`](../../docs/run-on-mac.md#check-before-installing-a-service) sends one small paid
tool-call request that proves the model ID; it exercises neither Slack nor the broker.

Start the broker through the [runbook](../../docs/run-on-mac.md#check-before-installing-a-service),
then probe as its owner:

```console
sudo -u _dekopon_broker /usr/local/lib/dekopon/current/dekopon-brokerd probe --socket /usr/local/var/run/dekopon/broker.sock
```

Exit status 0 with no output is healthy. The probe expects the server's UID to equal its own, so
running it as `_dekopon_gateway` is not the cross-UID check. Start the gateway, then accept:

1. The mapped sender DMs the bot asking it to test the installed tool. Expect a post title, a
   successful `jsonplaceholder.posts.get` broker execution record, and matching OpenRouter usage.
2. A second, unmapped sender gets no model-backed reply.

Keep the binaries, provider checksum and these files together for
[rollback](../../docs/run-on-mac.md#sleep-logs-updates-and-recovery).
