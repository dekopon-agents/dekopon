# The conditional writer

Xavier's boss sends a Slack DM — "set the incident record to resolved" — and the record is
updated with Xavier's API token, under the boss's name, on the record. Or it is refused, because
the record moved between the read and the write, and nobody's edit gets silently overwritten.

The gateway authenticates the message and vouches for the sender; it decides nothing. The broker
maps that Slack identity to the principal `cpetersen`, checks a Cedar policy, resolves the two
capabilities that policy permits, injects an API token bound to `api.example.com`, executes the
`http-probe` WebAssembly component, and hash-links the result into an audit chain naming the person
who asked. The token is never visible to the model, the shell session, the agent, or the component
that uses it — the broker's native HTTP engine adds the header after the guest's own headers have
been validated, and audit records `credentialInjected: true` and never a value.

| File | What it is | Who reads it |
|---|---|---|
| [`dekopon.yaml`](dekopon.yaml) | The catalog: one agent, two capabilities, one provider | `dekopond`, `dekopon` |
| [`broker.yaml`](broker.yaml) | Broker configuration: identities, mappings, constraint sets | `dekopon-brokerd` |
| [`policies.cedar`](policies.cedar) | Who may do what, and through which gateway | `dekopon-brokerd` |
| [`broker-credentials.yaml.example`](broker-credentials.yaml.example) | The API token, after you copy it | `dekopon-brokerd` |
| [`dekopond.yaml`](dekopond.yaml) | Gateway configuration: transport, model, route | `dekopond` |

Nothing here is a mock. `crates/dekopon-brokerd/tests/examples.rs` loads the checked-in
`http-probe` component, compiles this policy against the world these files declare, and asserts the
decision table both directions, so the walkthrough cannot drift away from the machinery without CI
noticing.

## 1. Create the Slack app

Follow [`../slack/README.md`](../slack/README.md): create the app from the manifest, generate the
app-level token (`xapp-…`, scope `connections:write`), install it for the bot token (`xoxb-…`), and
find the two identifiers you will need below — the workspace `T…` team ID and the sender's `U…`
member ID. Socket Mode means no public HTTP endpoint and no inbound firewall hole.

## 2. Create the token and the credentials file

A bearer token for the upstream API, scoped as narrowly as that API allows:

- **read** on the record — `http-probe.fetch`
- **write** on the same record — `http-probe.conditional-write`

Most APIs have no permission narrower than "write". The narrowing that matters happens here
instead: the component exposes a `http-probe.purge` this deployment grants nowhere, and a
capability no constraint set describes is unreachable regardless of what the token could do.

Then:

```console
cp broker-credentials.yaml.example broker-credentials.yaml
chmod 600 broker-credentials.yaml
$EDITOR broker-credentials.yaml          # replace the replace-me_XXXX… placeholder
```

`chmod 600` is not advice. The broker checks this file harder than its own configuration — it
rejects group or world *readability*, not just writability — and refuses to start otherwise.
`broker-credentials.yaml` is deliberately absent from the repository and is in `.gitignore`, so
following this step cannot commit a secret.

## 3. Adjust the placeholders

Exactly these, and nothing else:

| Placeholder | File | Replace with |
|---|---|---|
| `/home/xavier/.local/{run,state}/dekopon/…` | `broker.yaml` | your own paths — four entries: socket, audit, checkpoint, checkpoint lock |
| `/home/xavier/.local/run/dekopon/broker.sock` | `dekopond.yaml` | the same socket path as `broker.yaml` |
| `uid: 501` | `broker.yaml` | your UID (`id -u`) |
| `serverUid: 501` | `dekopond.yaml` | the same UID |
| `slack.t0123abcd` | `broker.yaml`, `attestor.namespaces` | `slack.` + your lowercased team ID |
| `slack.t0123abcd.u0123abcd` | `broker.yaml`, `identityMappings` | the lowercased `slack.<team>.<user>` of the person allowed to use this |
| `replace-me_XXXX…` | `broker-credentials.yaml` | the token from step 2 |

Everything else already resolves. Relative paths in both configurations resolve against the
configuration file's own directory, so `../providers/http-probe-provider.wasm`, `policies.cedar`,
`broker-credentials.yaml`, and `dekopon.yaml` work from a checkout with no editing.

Two names appear in several files and must agree with each other rather than with anything of
yours: the principal `cpetersen` (`broker.yaml` mapping, both statements in `policies.cedar`), the
gateway principal `dekopond-gateway` (`broker.yaml` identity, both `via` conditions), and the agent
`xaviers-conditional-writer` (`dekopon.yaml` metadata, `dekopond.yaml` route, both policy statements).
Rename them together or not at all.

Then make the directories private and the configurations owner-only:

```console
mkdir -p ~/.local/run/dekopon ~/.local/state/dekopon
chmod 700 ~/.local/run/dekopon ~/.local/state/dekopon
chmod 600 broker.yaml policies.cedar dekopond.yaml
```

The catalog half is checkable before anything runs:

```console
$ dekopon --config dekopon.yaml validate
configuration valid: 1 agent(s), 2 capability(ies), 1 provider(s)

$ dekopon --config dekopon.yaml describe agent xaviers-conditional-writer
```

That validates cross-references and capability metadata, and nothing else — the `dekopon` CLI reads
the catalog and never contacts the broker. What the agent may actually do is decided two files away.

## 4. Run the broker

```console
dekopon-brokerd --config broker.yaml
```

```json
{"timestamp":"2026-01-14T09:12:03.114Z","level":"INFO","event":"broker_started","audit_records":0,"audit_head":"none","target":"dekopon_brokerd"}
```

`audit_records: 0` and `audit_head: "none"` are a fresh chain. On the next start they are the
retained count and the verified head — and if the audit file and its checkpoint disagree by more
than the one-record crash window, the broker refuses to start rather than continuing over a gap.

Everything the broker will ever permit was decided by the time this line printed: the policy is
compiled and strictly validated, every capability it can permit has a constraint set, and the
credential is loaded and destination-bound. A misconfiguration here is a startup failure, never a
surprise at 2 a.m.

## 5. Run the gateway

```console
export DEKOPOND_SLACK_APP_TOKEN=xapp-...
export DEKOPOND_SLACK_BOT_TOKEN=xoxb-...
export OPENAI_IMAGE_API_KEY=sk-...
dekopond --config dekopond.yaml
```

```json
{"level":"INFO","event":"gateway_broker_ready","capability.count":0,"target":"dekopond"}
{"level":"INFO","event":"gateway_transport_connected","transport":"workspace-slack","kind":"slackSocketMode","target":"dekopond"}
{"level":"INFO","event":"gateway_started","transport.count":1,"route.count":1,"target":"dekopond"}
```

`capability.count: 0` is the point, not a warning. That probe asks the broker what the *gateway's
own identity* may do, and the answer is nothing: both policy statements require `context.via`, and
a directly connected peer has no `via`. The gateway can reach capabilities only while carrying a
subject it is authorized to vouch for.

The tokens live in the environment because `dekopond.yaml` names variables and never values;
pasting a token where a variable name belongs is a startup failure rather than a secret sitting in
a config file.

## 6. The DM

> **cpetersen:** set the incident record to resolved

The session:

1. **Attestation.** `dekopond` derives the canonical subject `slack.t0123abcd.u0123abcd` from the
   authenticated Slack envelope and opens a broker leg with `capabilitiesFor(subject, agent)`. The
   broker checks its attestor grant covers that namespace, maps the subject to `cpetersen`, checks
   `agent.prompt` on `Dekopon::Agent::"xaviers-conditional-writer"`, and answers with two
   capabilities. A sender who fails any of those steps gets `You're not authorized to use this
   agent.` and **no model call is made**.
2. **The session.** The agent's `instructions` become the system prompt, and its only tool is the
   sandboxed shell. It writes something like this — one tool call, several capability invocations:

   ```console
   $ http-probe.fetch --input '{"uri":"https://api.example.com/records/incident-4711"}'
   {"status":200,"bodyBytes":8931,"headerCount":7,
    "bodyText":"{\"id\":\"incident-4711\",\"state\":\"investigating\",\"etag\":\"\\\"v7\\\"\"}", ...}

   $ http-probe.conditional-write --input '{"uri":"https://api.example.com/records/incident-4711",
       "expectedEtag":"\"v7\""}'
   {"observedEtag":"\"v7\"","readStatus":200,"writeStatus":200}
   ```

   Each command word is one capability proposal, not a subprocess: there is no binary and no
   shell behind it, and there is no generic passthrough — one would collapse two separately
   policed capabilities into "everything the token can reach".
3. **The answer.** The session's final text goes back to the DM:

   > **dekopond:** Set incident-4711 to resolved. Read and wrote against etag `"v7"`, both
   > calls returned 200.

   A failed session says exactly one thing instead: `The agent could not complete this request.` A
   model's own error text, a provider message, and a transport diagnostic are all things chat is
   the last place for; the operator reads the category from telemetry.
4. **What the next message remembers.** The route is `mode: persistent`, so this exchange is kept
   as one `(question, answer)` pair in the gateway's memory and replayed ahead of the next message
   from *this sender in this conversation* — which is what makes a follow-up like "and 4712?"
   answerable. Only the question and the answer are kept: the `http-probe.fetch` output above is
   dropped at write-back and never replayed. Nothing is written to disk, nothing reaches the broker, and
   fifteen idle minutes or a narrowed grant drops it. Set the route to `mode: oneShot` — or leave
   the `conversation:` block out, which means the same thing — and every message starts from an
   empty prompt again.

### The refusals worth knowing

`http-probe.conditional-write` re-reads the resource before it writes anything and carries the
etag it observed in an `if-match` header. The refusal happens before any write leaves the
component:

| Code | When |
|---|---|
| `precondition-failed` | the caller passed `expectedEtag` and the resource has moved since |
| `http-failed` | the pre-read itself failed, so there is nothing to pin a write to |
| `invalid-input` | no `uri`, which is refused without any host call at all |

That is why the capability is classified `conditional` rather than `idempotent`, and why its
constraint set allows two requests and two methods: one `GET`, then one `POST` carrying an etag
that was true a moment ago. A retry against an unchanged record converges; a retry after someone
else's edit refuses instead of overwriting work nobody read.

## 7. What the audit chain now holds

```console
tail -1 ~/.local/state/dekopon/audit.jsonl | jq .
```

```json
{
  "sequence": 6,
  "previousHash": "sha256:1d0a…",
  "event": {
    "type": "execution",
    "invocation": "dekopond-session-9f1c4a7b0e35d268-3",
    "trace": "dekopond-session-9f1c4a7b0e35d268",
    "principal": "cpetersen",
    "actor": { "type": "agent", "agent": "xaviers-conditional-writer" },
    "via": "dekopond-gateway",
    "attested_subject": "slack.t0123abcd.u0123abcd",
    "capability": "http-probe.conditional-write",
    "provider": "http-probe",
    "authorized_by": "local-broker",
    "decision_id": "allow-dekopond-session-9f1c4a7b0e35d268-3",
    "policy_revision": "conditional-write-2026-01",
    "policy_ids": ["conditional-writer-surface"],
    "policy_digest": "sha256:7c31…",
    "effect": "external-write",
    "risk": "High",
    "idempotency": "conditional",
    "credential": "api-token",
    "outcome": "Succeeded",
    "duration_ms": 812,
    "output_digest": "sha256:9ab4…",
    "http_calls": [
      { "method": "GET",  "authority": "api.example.com", "status": 200,
        "requestBytes": 214, "responseBytes": 8931, "credentialInjected": true },
      { "method": "POST", "authority": "api.example.com", "status": 200,
        "requestBytes": 486, "responseBytes": 1204, "credentialInjected": true }
    ]
  },
  "recordHash": "sha256:c7e2…"
}
```

What each part is doing:

- `principal: cpetersen` — the effect is attributed to the person who asked, not to the process
  that relayed the message. `via: dekopond-gateway` records which gateway vouched, and
  `attested_subject` records the claim it made. All three, or none of them: a direct peer's record
  has no `via` and no subject.
- `policy_ids` — the `@id("…")` names from `policies.cedar`. That is why writing them is worth it:
  positional names renumber when a policy is inserted above them. `policy_digest` fingerprints the
  whole evaluated policy set, so two brokers reporting the same digest evaluated the same surface.
- Two `http_calls` — the pre-read and the write, exactly the budget the constraint set allowed.
  `credentialInjected: true` says broker-held authority was presented; the value appears nowhere,
  and `requestBytes` deliberately excludes the injected header so its length cannot leak either.
- `credential: api-token` — *which* authority, by the symbolic name in `broker.yaml`. One example
  has one token, so it reads as redundant here; a deployment whose constraint set names a different
  credential per agent is one where the two organizations' writes would otherwise be identical
  records.
- No record body, no request payload, no written text, no Slack message, no URL path or query. The
  chain records that something happened and to what; provider output is a digest.

Records 1 through 5 are the rest of the same session: a `decision` and an `execution` for the
read, then the `decision` that allowed this write. Every identifier in the session shares
the `trace`, and each invocation extends it with a counter, so `grep dekopond-session-9f1c4a7b0e35d268`
recovers the whole conversation's effects. Each record carries `previousHash` and `recordHash`, and
the broker verifies the entire chain on every start.

## 8. When it does not work

| Symptom | Cause | Where it shows |
|---|---|---|
| Slack replies `You're not authorized to use this agent.` | any one of three: the sender's subject is not in `identityMappings`; the peer identity has no `attestor` grant, or the subject sits outside its `namespaces`; or no policy permits `agent.prompt` for that principal and agent — check the `via` condition names your gateway's `principal`. The broker answers all three identically on purpose: a refusal must not disclose whether a subject is even mapped. | gateway stdout, `{"event":"gateway_session_rejected","reason":"attestation-refused"}`. **No audit record** — a refused capability listing is not a proposal, so there is nothing to audit and no model call was paid for. Work the three causes in the configuration. |
| The same reply, but the log says `"reason":"unauthorized"` | attested, mapped, and permitted to drive the agent — and policy grants it zero capabilities. Usually the second policy statement's `context.agent` or `context.via` disagreeing with the first's. | gateway stdout |
| The agent answers that it could not write | the write was denied or refused | a `decision` with `"allowed": false` and a `reason` (`attestation-denied`, `unmapped-subject`, `agent-denied`, `unconstrained-capability`, or an empty `policy_ids` deny-by-default), or a `precondition-failed` provider error when the record moved |
| Broker exits: `policy permits capability X, which has no constraint set` | `policies.cedar` names a capability `broker.yaml` does not constrain | startup, before the socket is bound |
| Broker exits: `constraint set for X names unknown credential "api-token"` | `broker-credentials.yaml` was never copied, or names the credential differently | startup |
| Broker exits: `broker credentials must be single-link, owned by the server UID, and unreadable by group and world` | `chmod 600 broker-credentials.yaml` | startup |
| Broker exits: `constraint set for X allows host "…" outside credential "api-token" destinations` | an `allowedHosts` entry the credential is not bound to | startup |
| Gateway exits at startup naming a variable | `DEKOPOND_SLACK_APP_TOKEN`, `DEKOPOND_SLACK_BOT_TOKEN`, or `OPENAI_IMAGE_API_KEY` is unset — reported by name, never by value. Delete the `imageGenerator:` block and the route's `imageGenerator: true` if you do not want the third one | startup |
| Gateway exits: broker unreachable | the broker is not running, or the two socket paths disagree | the `gateway_broker_ready` probe never logs |

The split matters when you are debugging: a session refused *before* it starts leaves a gateway log
line and an empty audit chain, while anything refused *during* one leaves an audited denial naming
the gateway, the subject, and the reason. Both refuse; only one of them ever proposed anything.

## What this deployment does not buy yet

`dekopond` and `dekopon-brokerd` run under one UID here, because the broker's owner-only socket
currently requires every configured peer UID to equal the server's. Under one UID an attestor grant
buys attribution and deny-by-default scoping, not isolation — any process running as you can
already act as the configured gateway peer. `via` and namespace scoping become real separation only
when the gateway has its own UID, and that deployment is committed direction rather than current
behavior. [`../../docs/security-model.md`](../../docs/security-model.md) states this in full.

This route is `mode: persistent`, so the gateway replays a bounded window of earlier turns
from the same sender into the next prompt. Nothing carries beyond that conversation: the
agent has no memory that outlives it.

## Related

- [`../slack/`](../slack/README.md) — the Slack app manifest, both tokens, and finding the `T…`/`U…` identifiers.
- [`../providers/http-probe/`](../providers/http-probe/README.md) — the component this deployment executes, and the `http-probe.purge` it deliberately never grants.
- [`dekopon-provider-gh`](https://github.com/dekopon-agents/dekopon-provider-gh) — the same shape at nineteen capabilities, shipped from its own repository.
- [`../../docs/dekopond.md`](../../docs/dekopond.md) — transports, routing, session bounds, and the authorization flow.
- [`../../crates/dekopon-brokerd/README.md`](../../crates/dekopon-brokerd/README.md) — every configuration field, and the checkpoint/recovery contract.
- [`../../crates/dekopon-policy/README.md`](../../crates/dekopon-policy/README.md) — what Cedar decides here and what it deliberately does not.
- [`../local/dekopon.yaml`](../local/dekopon.yaml) — the catalog-only example, whose `reviewer` may comment and deliberately holds no approval capability.
