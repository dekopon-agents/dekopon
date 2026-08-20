# PR summarizer and linter

A maintainer sends a Slack DM — “summarize and lint PR 7 in owner/repo” — and receives a concise
answer while the pull request gets one `COMMENT` review containing the summary, actionable lint
findings, and check status.

The gateway authenticates the message and vouches for the sender; it decides nothing. The broker
maps that Slack subject to the principal `maintainer`, checks Cedar policy, exposes six narrow
capabilities, injects a GitHub token bound to `api.github.com`, executes the `gh` WebAssembly
component, and hash-links every decision and result into an audit chain naming the person who
asked. The token is never visible to the model, shell session, agent, or component that uses it.

This workflow can comment. It cannot approve, request changes, or merge. Those are separate `gh`
capabilities absent from the catalog, policy, and broker constraint sets, and tests prove the
omission against the checked-in provider manifest.

| File | What it is | Who reads it |
|---|---|---|
| [`dekopon.yaml`](dekopon.yaml) | Catalog: one agent, six capabilities, one provider | `dekopond`, `dekopon` |
| [`broker.yaml`](broker.yaml) | Identities, mappings, providers, and execution constraints | `dekopon-brokerd` |
| [`policies.cedar`](policies.cedar) | Who may drive the agent and which actions it may reach | `dekopon-brokerd` |
| [`broker-credentials.yaml.example`](broker-credentials.yaml.example) | GitHub token template | `dekopon-brokerd` |
| [`dekopond.yaml`](dekopond.yaml) | Transport, model, route, and session bounds | `dekopond` |

Nothing here is a mock. `crates/dekopon-brokerd/tests/examples.rs` loads the checked-in `gh`
component, compiles this policy against the world these files declare, and asserts the allow and
deny table. Config and gateway tests load the same files through their production decoders.

## What “lint” means here

The agent reviews bounded GitHub data: pull-request metadata, changed-file patches, a unified diff,
head check runs, and selected files at the observed head SHA. It can identify likely correctness,
security, reliability, maintainability, and test-coverage problems. It cannot check out the branch,
run repository commands, or claim that a compiler or project linter passed. Its standing orders
require it to say when truncation or missing context limits a conclusion.

Repository text is untrusted throughout. A pull-request description, diff, source file, or check
message can influence the model’s proposal, but cannot assert identity, widen policy, select a
credential, or authorize the resulting comment.

## 1. Create the Slack app

Follow [`../slack/README.md`](../slack/README.md): create the app from
[`manifest-agent.yaml`](../slack/manifest-agent.yaml), generate the app-level token (`xapp-…`, scope
`connections:write`), install it for the bot token (`xoxb-…`), and find the workspace `T…` team ID
and permitted sender’s `U…` member ID. Socket Mode needs no public HTTP endpoint or inbound firewall
rule. Slack shows its native Working/Stop session while an authorized review runs; a workspace
without the Agent feature degrades to the temporary `:tangerine:` reaction.

## 2. Create the GitHub token

Create a fine-grained personal access token scoped to only the repositories this agent may review:

- **Contents: Read-only** — `gh.content.read`
- **Pull requests: Read and write** — pull-request reads and one `COMMENT` review
- **Checks: Read-only** — `gh.pull-request.status`

GitHub has no comment-only pull-request permission. Dekopon narrows that provider permission by
exposing comment, approval, request-changes, and merge as separate capabilities. This deployment
constrains only comment. The token’s own repository scope remains important: Cedar does not inspect
provider input paths and therefore cannot restrict `owner/repo` itself.

Then create the owner-only credentials file:

```console
cp broker-credentials.yaml.example broker-credentials.yaml
chmod 600 broker-credentials.yaml
$EDITOR broker-credentials.yaml
```

`chmod 600` is enforced, not suggested. The broker rejects group or world readability, symlinks,
hard links, and wrong ownership. `broker-credentials.yaml` is ignored by Git.

## 3. Adjust the placeholders

| Placeholder | File | Replace with |
|---|---|---|
| `/home/xavier/.local/{run,state}/dekopon/…` | `broker.yaml` | Your socket, audit, and checkpoint paths |
| `/home/xavier/.local/run/dekopon/broker.sock` | `dekopond.yaml` | The same socket path |
| `uid: 501` | `broker.yaml` | Your UID (`id -u`) |
| `serverUid: 501` | `dekopond.yaml` | The same UID |
| `slack.t0123abcd` | `broker.yaml` | `slack.` plus the lowercased team ID |
| `slack.t0123abcd.u0123abcd` | `broker.yaml` | Lowercased `slack.<team>.<user>` |
| `github_pat_XXXX…` | `broker-credentials.yaml` | The fine-grained token |

The principal `maintainer`, gateway principal `dekopond-gateway`, and agent
`pr-summarizer-linter` are internal names repeated across configuration and policy. Rename each
only if every occurrence changes together.

Relative paths resolve against the configuration file’s directory, so the provider component,
policy, credentials file, and catalog work from a checkout without edits.

```console
mkdir -p ~/.local/run/dekopon ~/.local/state/dekopon
chmod 700 ~/.local/run/dekopon ~/.local/state/dekopon
chmod 600 broker.yaml policies.cedar dekopond.yaml
```

Validate the unprivileged catalog first:

```console
$ dekopon --config dekopon.yaml validate
configuration valid: 1 agent(s), 6 capability(ies), 1 provider(s)

$ dekopon --config dekopon.yaml describe agent pr-summarizer-linter
```

This proves catalog cross-references and metadata only. Actual permission is decided by the broker.

## 4. Run the broker

```console
dekopon-brokerd --config broker.yaml
```

```json
{"level":"INFO","event":"broker_started","audit_records":0,"audit_head":"none","target":"dekopon_brokerd"}
```

Before binding its socket, the broker strictly validates policy, checks that every permitted
capability has a matching constraint set, loads the destination-bound credential, and verifies the
provider manifest. A mismatch is a startup failure, not a surprise during a review.

## 5. Run the gateway

```console
export DEKOPOND_SLACK_APP_TOKEN=xapp-...
export DEKOPOND_SLACK_BOT_TOKEN=xoxb-...
dekopond --config dekopond.yaml
```

```json
{"level":"INFO","event":"gateway_broker_ready","capability.count":0,"target":"dekopond"}
{"level":"INFO","event":"gateway_transport_connected","transport":"workspace-slack","kind":"slackSocketMode","target":"dekopond"}
{"level":"INFO","event":"gateway_started","transport.count":1,"route.count":1,"target":"dekopond"}
```

`capability.count: 0` is intentional. The startup probe asks what the gateway’s own direct identity
may do; both Cedar statements require an attested sender and agent. The gateway reaches the six
capabilities only while vouching for an allowed Slack subject.

## 6. Summarize and lint a pull request

> **maintainer:** Summarize and lint PR 7 in owner/repo, then post the result.

A session typically proposes commands like these through the sandboxed `gh` builtin:

```console
$ gh pr view 7 -R owner/repo
{"number":7,"title":"Cache the provider registry between invocations","state":"open",
 "draft":false,"headSha":"4f2a91c8e3b7d05a6c1f8e2b9d4a70c3e5f81b26","baseRef":"main",...}

$ gh pr files 7 -R owner/repo
{"files":[{"path":"crates/dekopon-broker-host/src/lib.rs",...}],"truncated":false}

$ gh pr diff 7 -R owner/repo
{"diff":"diff --git a/...","truncated":false}

$ gh pr status 7 -R owner/repo
{"headSha":"4f2a91c8e3b7d05a6c1f8e2b9d4a70c3e5f81b26","checkRuns":[...]}

$ gh content view crates/dekopon-broker-host/src/lib.rs -R owner/repo \
    --ref 4f2a91c8e3b7d05a6c1f8e2b9d4a70c3e5f81b26
{"path":"crates/dekopon-broker-host/src/lib.rs","encoding":"utf-8","content":"..."}

$ gh pr review 7 -R owner/repo --comment \
    --body "## Summary ...\n\n## Lint findings ...\n\n## Checks ..." \
    --expected-head-sha 4f2a91c8e3b7d05a6c1f8e2b9d4a70c3e5f81b26
{"reviewId":2418809931,"state":"COMMENTED","pullNumber":7,
 "headSha":"4f2a91c8e3b7d05a6c1f8e2b9d4a70c3e5f81b26","author":"review-bot",...}
```

Each `gh` command is one capability proposal, not an operating-system process. There is no real
`gh` executable, checkout, filesystem, or `gh api`. The raw passthrough is refused because a generic
GET would collapse separately authorized capabilities into everything the token can read.

The comment capability pre-reads the pull request and compares the supplied expected head SHA. If
new commits arrived after inspection, it returns `head-changed` before the POST. The agent’s orders
say not to retry: the new revision needs a new review. Drafts may receive comments, but closed or
merged pull requests are refused.

The final Slack answer reports what was posted and its review ID, after which the Agent session
returns to `active` (or the fallback reaction is removed). On an internal failure it instead
receives the fixed sentence `The agent could not complete this request.`; model, provider, and
transport error text never leaks back into chat.

The route is `mode: persistent`, so a follow-up like “explain the first finding” can use the compact
`(question, answer)` pair kept in gateway memory. Tool calls and GitHub output are not retained.
Nothing is written to disk or sent to the broker, and an idle timeout or changed grant drops the
history.

## 7. Inspect the audit chain

```console
tail -1 ~/.local/state/dekopon/audit.jsonl | jq .
```

The terminal comment record has this shape (hashes abbreviated):

```json
{
  "sequence": 10,
  "previousHash": "sha256:1d0a…",
  "event": {
    "type": "execution",
    "invocation": "dekopond-session-9f1c4a7b0e35d268-5",
    "trace": "dekopond-session-9f1c4a7b0e35d268",
    "principal": "maintainer",
    "actor": { "type": "agent", "agent": "pr-summarizer-linter" },
    "via": "dekopond-gateway",
    "attested_subject": "slack.t0123abcd.u0123abcd",
    "capability": "gh.pull-request.comment",
    "provider": "gh",
    "policy_revision": "pr-summarizer-linter-2026-01",
    "policy_ids": ["pr-summarizer-linter-gh-surface"],
    "effect": "external-write",
    "risk": "Medium",
    "idempotency": "conditional",
    "credential": "github-pat",
    "outcome": "Succeeded",
    "output_digest": "sha256:9ab4…",
    "http_calls": [
      { "method": "GET", "authority": "api.github.com", "status": 200,
        "credentialInjected": true },
      { "method": "POST", "authority": "api.github.com", "status": 200,
        "credentialInjected": true }
    ]
  },
  "recordHash": "sha256:c7e2…"
}
```

The chain records trusted attribution, policy identity, bounded HTTP metadata, outcome, and output
digest. It does not record the Slack message, repository path, diff, source, comment body, URL path
or query, headers, token, or provider output. Earlier decision/execution pairs share the trace ID.

## Refusals worth knowing

| Symptom | Likely cause |
|---|---|
| `You're not authorized to use this agent.` | Subject outside the attestor grant, no identity mapping, or no `agent.prompt` policy |
| Authorized sender, zero capabilities | Capability policy’s `context.agent` or `context.via` does not match the session policy |
| `head-changed` | New commits arrived after inspection; start a new review instead of retrying |
| `pr-closed` | Pull request is closed or merged; no comment was posted |
| Broker says a capability has no constraint set | Policy and `broker.yaml` disagree; startup failed closed |
| Broker says credential is unknown or host is uncovered | Credentials file is absent/misnamed or lacks `api.github.com` |
| Gateway exits naming an environment variable | Slack or model credential variable is unset; only its name is logged |

Approval, request-changes, and merge do not produce runtime provider errors here: they are absent
from the session. A model-written command using one reports the exact missing capability and exits
`127`; policy and constraints independently omit it as defense in depth.

## Current limitation

`dekopond` and `dekopon-brokerd` run under one UID in this example because the broker socket is
owner-only. The attestor grant therefore provides attribution and deny-by-default shape, not
isolation from another process under that UID. A dedicated gateway UID remains committed direction.

Prompt injection is not solved. Repository text can manipulate model output and remain in a
persistent conversation until its bounds evict it. Containment comes from explicit capabilities,
trusted identity mapping, per-message authorization, broker-held credentials, constrained provider
execution, and audit — not from trusting the summary.

## Related

- [`../slack/`](../slack/README.md) — app manifest, tokens, and Slack identifiers.
- [`../providers/gh/`](../providers/gh/README.md) — all nineteen capabilities and no raw passthrough.
- [`../../docs/dekopond.md`](../../docs/dekopond.md) — routing, sessions, and conversation bounds.
- [`../../crates/dekopon-brokerd/README.md`](../../crates/dekopon-brokerd/README.md) — broker configuration and recovery.
- [`../../docs/security-model.md`](../../docs/security-model.md) — trust boundaries and limitations.
- [`../local/dekopon.yaml`](../local/dekopon.yaml) — a smaller catalog-only reviewer declaration.
