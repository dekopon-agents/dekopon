# Dekopon design

Dekopon runs self-hosted AI agents whose tools are WebAssembly providers. This file holds the goals
and the reasons for the shape; [authorization](authorization.md) covers how a decision is made, and
the [security model](security-model.md) what Dekopon doesn't protect.

## Constitution

Three goals, in priority order. A change is judged by which one it serves; a subsystem that serves
none is a deletion candidate, however well built.

1. **Credentials the agent never holds.** A model can name a secret; it can't read one. Secret bytes
   stay in the broker, out of the model, shell, gateway, wire protocol, provider memory and
   telemetry. The broker's native HTTP engine injects a credential after the provider has built its
   request. The endpoint you authorize receives it, so you're trusting that endpoint.
2. **One trace per message.** From the telemetry store an operator can reconstruct a run: the
   message and its sender, prompts and answers, the script, each command and its output, every
   decision, provider input and output, and HTTP egress, all under the message's W3C trace id. Only
   secret bytes and the daemons' own credentials are left out. The trace is the aim, never a reason
   to stall a reply.
3. **Extensible through Wasm providers.** A capability arrives as an out-of-tree component with a
   manifest and command words, run under the owner's constraints. Nothing in this tree grows to add
   one.

### Non-goals

An argument to keep or add code on any of these grounds is rejected.

- **Idempotency, exactly-once, duplicate-effect defense, automatic retries.** A failed call goes
  back to the model, which decides whether to try again.
- **Crash durability, reconciliation, quarantine, tamper detection.** A failure fails fast, leaves
  one trace record and has a reset path. Detection that can't fix anything isn't built.
- **An authorized endpoint that transforms what it received.** The broker refuses a response that
  carries the raw credential. An endpoint that re-encodes it is trusted by your choice of endpoint.
- **End-user privacy from the operator.** Telemetry has no metadata-only mode. A person talking to a
  Dekopon agent is talking to its operator.
- **A compromised host, root, or a hostile process under the broker's UID.**
- **Production-sandbox claims for Wasmtime.**

## Authority flow

1. A chat service authenticates the sender. `dekopon-gatewayd` picks the route's agent and opens a
   session with the broker, naming the sender's subject (`drn:slack:user:t0123abc:u9xyz`), never
   a principal.
2. `dekopon-brokerd` authenticates the gateway by its Unix peer UID, checks that the subject lies
   inside the gateway's attestor grant, maps the subject to a principal and asks Cedar whether that
   principal may prompt that agent. A refusal ends the turn before any model call.
3. The model writes one bash script. Each provider command word in it becomes a proposal: a
   capability, its input and optionally a secret's public name. Anything can create a proposal; only
   the broker turns one into an authorization, and it never hands that authorization back.
4. For each proposal the broker re-checks the attestation, requires an owner-authored constraint
   set, asks Cedar, authorizes any secret use, emits the decision record, and runs the provider in a
   fresh Wasm store bounded by that constraint set.
5. Each HTTP request is [checked again](authorization.md#each-outbound-request-is-checked-again);
   the broker then injects the credential.
6. The provider's output streams back into the script.

Nothing caches a decision: each turn and invocation is authorized afresh, and replayed conversation
text is prompt text, never authorization input.

## Why two processes and two UIDs

The gateway talks to chat services, runs the model loop and parses whatever the internet sends: it's
the large, exposed half. The broker holds policy, provider credentials and provider storage. With
separate processes under separate UIDs, a gateway compromise gets chat tokens, model keys,
conversation history and the power to speak for the subjects its attestor grant covers. It doesn't
get provider credentials or policy, and it can't widen a constraint.

A type the gateway can't construct (`AuthorizedInvocation`) is only defense in depth: one memory bug
in a shared process would cross it. CI keeps the halves apart: the gateway's dependency tree may not
contain the broker, policy or HTTP-host crates, and the broker's may not contain the agent, shell or
config crates (the two "excludes" steps in `.github/workflows/ci.yml`).

The broker authenticates local peers by kernel UID, not a token. The [current local process
boundary](security-model.md#current-local-process-boundary) describes the chart's layout.

## Trace as audit

There is no audit store. Each broker decision and execution is one structured log record in the
message's W3C trace beside the prompt and script that caused it, and each per-request HTTP decision
is a record too. One correlation id answers "why did this happen" without a second store holding
another copy of sensitive data.

`dekopon-brokerd` runs `TraceOnlyAuditLog`, which keeps nothing: the record goes to the console log
and, when telemetry is configured, to OTLP. Both queues are bounded, and nothing in a serving path
waits on them. A full console queue drops lines and later logs how many it dropped
(`crates/dekopon-telemetry/src/install.rs`); a full OTLP batch queue drops records
(`crates/dekopon-telemetry/src/exporter.rs`). Lose the exporter, or overflow it, and those audit
lines are gone; nothing reconstructs them. The telemetry store holds every prompt and output; it
sits inside the operator's trust boundary.

A `telemetry` block in each daemon's configuration turns the export on
([broker](../crates/dekopon-brokerd/src/config.rs), [gateway](../crates/dekopon-gatewayd/src/config.rs);
[example](../examples/conditional-write/broker.yaml)).
The model's prompt lands in `agent.model.prompt` records and its reply in
`agent.model.answer` ([`prompt.rs`](../crates/dekopon-agent/src/prompt.rs)).

## Keyless model access for outside VMs

Coding agents in Firecracker VMs, such as `claude`, `pi` and `codex`, need a model. A key inside the
VM is a key whatever runs there can print. So the gateway serves the model APIs on its own mTLS
listener, reachable only through the VM jail's egress gateway. The jail names the VM. The proxy maps
it to an agent, replaces whatever model the VM names with that agent's own model and generation
settings, injects the key, and admits the call against the agent's token budget before sending it.
Whatever token the VM's client sends is ignored.

The proxy lives in the gateway because the gateway's model client already holds the model keys and
the broker takes on no model orchestration. Nothing points a VM's client at the proxy: the script
that starts the client sets its base URL (for Claude Code, `ANTHROPIC_BASE_URL`) to the jail's model
host. What the proxy trusts is in the [security
model](security-model.md#the-guest-model-proxy-trusts-the-jail-to-name-its-vm); the code is
`crates/dekopon-model-proxy` and `crates/dekopon-gatewayd/src/proxy.rs`.

### Token budgets

An agent can have a token budget, and its own model calls and its proxied calls draw on the same one
([`crates/dekopon-model-token-governor`](../crates/dekopon-model-token-governor)). Each call
reserves an estimate before it's sent and settles to the reported usage, so concurrent sessions
can't both pass and overspend. A call costs raw input plus output, and cached input counts in full:
a coding agent re-sends its whole cached context on every request, so size its budget for that, not
for chat. Budgets live in gateway memory. Restoring them at boot from the telemetry store is best
effort, under-counts whatever the exporter dropped, and assumes one gateway at a time, which the
chart guarantees by never running two pods at once.

## Accepted implementation decisions

- **Startup complains; invocation enforces.** The broker's strict setting chooses whether
  configuration naming an absent capability refuses startup. Either way, a capability with no
  constraint set is denied before Cedar runs, and that refusal is the control. A policy naming an
  unloaded capability keeps the name as a schema-only phantom instead of losing the policy: dropping
  a grant written `action in [a, b]` because `b` isn't loaded would revoke `a` too. An undeclared
  principal is always fatal. Principals come from owner configuration, so a missing one is a typo.
- **Host imports only for what a sandbox can't have.** Egress, broker storage and assets, time,
  entropy, standard streams and child scripts are host imports. Parsers, SQL engines and dataframes
  compile into the provider that uses them. A host import is privileged broker code and a contract
  every provider links against; compute shipped inside a provider versions with that provider.
- **No native runtime plugins.** Broker host services are statically linked, so nothing a provider
  imports can load native code into the process that holds the credentials.

### Credential bindings

Besides [public DRNs](authorization.md#secretuse-and-public-drns), a constraint set may name one
symbolic credential from the broker's owner-only credentials file, and a per-agent binding may swap
in a different one when that agent acts (`ConstraintCatalog::credential_for` in
`crates/dekopon-broker/src/lib.rs`). The credential belongs to the operation, not the provider,
because the confused deputy is the same component doing a different operation. Keying the override
on the agent reuses the partition routes already make, so two organizations reach one capability
with two tokens. An entry is a fixed bearer token or a credential the broker renews itself: a
ChatGPT subscription or a GitHub App installation token.

Bindings and DRNs end in the same broker-side injection: a proposed or default DRN wins, otherwise
the binding applies. Only bindings carry a ChatGPT subscription or a GitHub App token today.
