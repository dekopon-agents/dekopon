# dekopon-policy

The only place [Cedar](https://cedarpolicy.com) appears in Dekopon. It wraps `cedar-policy` behind a
bounded, deterministic API that `dekopon-broker` and `dekopon-brokerd` consume; nothing else in the
workspace depends on it.

## What it decides, and what it does not

Cedar answers one question: **may this principal take this action on this resource in this
context?** It does not decide how narrowly the broker then executes the result. Timeouts, output
ceilings, allowed HTTP destinations and methods, call budgets, and credential binding live in
owner-authored *constraint sets* inside `dekopon-broker`, validated at startup against loaded
provider manifests, the component host's independent ceilings, and the credential store.
*Committed direction:* the legacy `credential`/`agents.<id>.credentials` bindings will be replaced by
public DRNs, retaining the broker's execution ceilings and separate `secret.use` authorization
([migration requirements](../../docs/design.md#legacy-credential-bindings)).

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
`agent.prompt` and `secret.use` actions. None of the entities carry attributes: a policy matches
them with `==` and `in`, and there is no entity data for an expression to read.

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

`PolicyWorld::new` refuses all reserved-action collisions and duplicate capability names together as
`PolicyBuildError::WorldConflicts`, with sorted, unique `reserved` and `duplicates` lists. At least
one list is nonempty; no conflicting world is returned.

## Context

Every action requires the string fields `via` and `agent`. Capability actions additionally require
`effect` and `risk`; `agent.prompt` requires no additional fields; `secret.use` additionally requires
`capability`, `provider`, and `sink`, identifying the exact capability, provider, and native sink the
public DRN was proposed for. A request missing a required field is refused before policy evaluation,
even if a policy would otherwise permit it.

All three action types optionally carry `subject`, `transportKind`, `transport`, `trigger`, and
`conversation`. When present, `conversation` requires `kind` and `id`; its `container` and `thread`
fields are optional. Policies must guard optional fields with `has` before reading them. Ordinary
policies' strict validation also prevents a field from being read on an action that never carries it.

Every value is rendered by the broker from authenticated transport state or owner-controlled
configuration:

- `via` — the attestor peer an attested context was derived through. Direct peers have no `via`, so
  their requests fail the required-context check.
- `subject` — the canonical external subject an attested context stands for.
- `agent` — the agent identity of an agent actor. Human and service actors have no `agent`, so their
  requests fail the required-context check.
- `effect` / `risk` — the trusted classification the broker will execute under,
  matched byte for byte against the loaded manifest at startup.

Invocation context excludes message content and arbitrary provider input; at the per-request Cedar point, `context.request` exposes bounded outgoing HTTP facts, including only representable buffered JSON record bodies of at most 16 KiB. A public DRN remains a separate strongly validated resource on a separate action, inert without an owner binding.

## Determinism and bounds

Policies compile once at startup; the per-request Cedar point converts bounded HTTP facts without parsing policy source or resolving entities.

- Policy source is capped at 1 MiB and 1024 static policies.
- Templates are refused: an unlinked template is policy that silently never applies.
- The schema is generated from the declared world and ordinary policies are validated against it in
  Cedar's **strict** mode. Guarded request-referencing forbids bypass strict validation and evaluate
  schemaless; their shape is checked at load. Ordinary unknown actions, entity types, or ill-typed
  expressions refuse construction with the validator's own diagnostics.
- Empty policy text is valid and permits nothing.
- Any evaluation error at decision time denies, and surfaces as a stable
  `PolicyDecision::errors_present` flag rather than error text. Policy source reaches a caller only
  through construction errors, never through a decision — and `Debug` prints a digest, not the
  policies.

## Explaining a decision

`PolicyDecision::determining_policy_ids` carries the sorted identifiers of policies that decided the
answer; `erroring_policy_ids` separately names policies that errored. The broker writes determining
IDs as `policy.ids` and request-point errors as `policy.error_ids` without copying evaluator text. Every statement
requires an `@id("…")` annotation naming it, because Cedar's positional names (`policy0`, `policy1`,
…) shift whenever a policy is added, which an audit trail cannot tolerate:

```cedar
@id("chat-agent-upper")
permit(principal == Dekopon::Principal::"cpetersen",
       action == Dekopon::Action::"cli-probe.upper",
       resource == Dekopon::Provider::"cli-probe")
when { context.via == "dekopon-gatewayd" };
```

Names must be bounded portable identifiers and unique across the set; two policies sharing one name
would make an explanation ambiguous, so it refuses startup.

`PolicyEngine::digest()` is a `sha256:` fingerprint over the canonicalized policy set plus the
sorted entity and action identifiers, domain-separated with `dekopon-policy-v1\0`. Two brokers
reporting the same digest evaluated the same authorization surface. It is recorded alongside
`policy.ids` as `policy.digest`, and it is a correlation aid rather than a wire-format contract.

Part of the [Dekopon](https://github.com/dekopon-agents/dekopon) workspace; see `docs/design.md` for
the authority model and `docs/security-model.md` for the trust boundaries this adapter informs but
never enforces.
