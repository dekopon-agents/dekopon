# dekopon-policy

The only place [Cedar](https://cedarpolicy.com) appears in Dekopon. It wraps `cedar-policy` behind
a bounded, deterministic API that `dekopon-broker` and `dekopon-brokerd` consume; nothing else in
the workspace depends on it.

## What it decides, and what it deliberately does not

Cedar answers one question: **may this principal take this action on this resource in this
context?** It does not decide how narrowly the broker then executes the result. Timeouts, output
ceilings, allowed HTTP destinations and methods, call budgets, and credential binding live in
owner-authored *constraint sets* inside `dekopon-broker`, validated at startup against loaded
provider manifests, the component host's independent ceilings, and the credential store.

Keeping them apart is the point. A policy edit can broaden *who may act*; it can never widen a
timeout, reach a new host, or bind a credential that was not already bound. The two failure modes
stay independently reviewable, and the execution half keeps being checked against artifacts Cedar
knows nothing about.

## Entity and action model

Everything lives in the `Dekopon` namespace.

| Entity type | Instances | Role |
| --- | --- | --- |
| `Dekopon::Principal` | enumerated from peer identities and subject mappings | principal of every action |
| `Dekopon::Provider` | enumerated from loaded provider manifests | resource of every capability action |
| `Dekopon::Agent` | **not** enumerated; matched by UID | resource of `agent.prompt` |
| `Dekopon::Secret` | public DRNs from the owner-only private map | resource of `secret.use` |

Actions are one `Dekopon::Action::"<capability-id>"` per loaded capability, plus the fixed
`agent.prompt` and `secret.use` actions. None of the entities carry attributes: a policy matches them
with `==` and `in`, and there is no entity data for an expression to read.

Agents are the one type whose instances are not declared, because the agent catalog belongs to the
gateway rather than the broker. Everything else is enumerated, and that is what turns a typo into a
startup refusal: Cedar's validator checks types, not instances, so
`principal == Dekopon::Principal::"typo"` is perfectly well typed and would simply never match.
`PolicyEngine::new` walks every entity literal in every policy and refuses any name the declared
world does not contain.

`PolicyEngine::new_lenient` walks the same literals but separates two kinds of absence. A
**principal** comes from owner-authored identities, never from a loaded component, so an undeclared
one is a typo and stays fatal. An **action** or **provider** is derived from a loaded provider
manifest, so an undeclared one means that provider is not loaded — a legitimate state for a
deployment whose policy anticipates it. Those are reported as `UnresolvedName` and registered as
*phantoms*: names present in the generated schema and nowhere else.

The phantom exists so the policy survives validation whole. Dropping it instead would be worse than
it sounds — a grant reading `action in [a, b]` with only `a` loaded would lose *both*, turning "one
provider is missing" into "this agent can do nothing". A phantom takes away exactly the missing
capability and nothing else, and can never authorize an execution: it routes to no provider, the
broker refuses any constraint set naming an unrouted capability, and an invocation naming one is
denied `unconstrained-capability` before Cedar is consulted.

## Context

Capability actions carry `{ via?, subject?, agent?, effect, risk, idempotency }`. `agent.prompt`
carries `{ via?, subject?, agent? }`. `secret.use` carries those routing fields plus the exact
capability, provider and native sink the public DRN was proposed for. Strict validation prevents a
field from being read on an action that never carries it.

Every value is rendered by the broker from authenticated transport state or owner-controlled
configuration:

- `via` — the attestor peer an attested context was derived through; absent for direct peers. This
  is the hinge that keeps attested and direct authority disjoint.
- `subject` — the canonical external subject an attested context stands for.
- `agent` — the agent identity of an agent actor; absent for human and service actors.
- `effect` / `risk` / `idempotency` — the trusted classification the broker will execute under,
  matched byte for byte against the loaded manifest at startup.

Message content and arbitrary provider input are deliberately **not** context. A public DRN is the
one narrow caller-supplied exception: it is a strongly validated resource on a separate action and
remains inert without an independently validated owner binding. Open JSON still has no policy
schema and cannot influence a decision.

## Determinism and bounds

Everything is startup-fixed. There is no per-request parsing, compilation, or entity resolution.

- Policy source is capped at 1 MiB and 1024 static policies.
- Templates are refused: an unlinked template is policy that silently never applies.
- The schema is generated from the declared world and the policy set is validated against it in
  Cedar's **strict** mode. An unknown action, unknown entity type, or ill-typed expression refuses
  construction with the validator's own diagnostics.
- Empty policy text is valid and permits nothing.
- Any evaluation error at decision time denies, and surfaces as a stable
  `PolicyDecision::errors_present` flag rather than error text. Policy source reaches a caller only
  through construction errors, never through a decision — and `Debug` prints a digest, not the
  policies.

## Explaining a decision

`PolicyDecision::determining_policy_ids` carries the identifiers of the policies that decided the
answer, sorted, and the broker writes them into every audit record as `policy_ids`. Cedar names
text-parsed policies positionally (`policy0`, `policy1`, …); an optional `@id("…")` annotation
replaces that with a stable name, which is what an audit trail actually wants:

```cedar
@id("chat-agent-echo")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"echo.echo",
       resource == Dekopon::Provider::"echo")
when { context has via && context.via == "dekopond-gateway" };
```

Names must be bounded portable identifiers and unique across the set; two policies sharing one name
would make an explanation ambiguous, so it refuses startup.

`PolicyEngine::digest()` is a `sha256:` fingerprint over the canonicalized policy set plus the
sorted entity and action identifiers, domain-separated with `dekopon-policy-v1\0`. Two brokers
reporting the same digest evaluated the same authorization surface. It is recorded alongside
`policy_ids` as `policy_digest`, and it is a correlation aid rather than a wire-format contract.

Part of the [Dekopon](https://github.com/dekopon-agents/dekopon) workspace; see
`docs/design.md` for the authority model and `docs/security-model.md` for the trust
boundaries this adapter informs but never enforces.


## Core session controls (unreleased)

Two reserved actions, `agent.model.select` and `agent.effort.set`, apply only from `Principal` to
`Agent`; provider actions cannot collide with them. Their strict context requires `agent`,
`fromModel`, `toModel`, `fromEffort`, `toEffort`, and accepts only the existing optional trusted
routing fields (`via`, `subject`, `transportKind`, `transport`, `channel`, `conversation`).
The four selection strings are typed intent, not verified model state. No history, spend,
provider JSON, endpoint, credential or arbitrary context enters Cedar. The broker separately
requires a fresh `agent.prompt` decision and every changed dimension's permit; errors deny.
