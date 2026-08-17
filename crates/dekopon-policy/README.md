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

Actions are one `Dekopon::Action::"<capability-id>"` per loaded capability, plus the fixed
`Dekopon::Action::"agent.prompt"`. None of the entities carry attributes: a policy matches them
with `==` and `in`, and there is no entity data for an expression to read.

Agents are the one type whose instances are not declared, because the agent catalog belongs to the
gateway rather than the broker. Everything else is enumerated, and that is what turns a typo into a
startup refusal: Cedar's validator checks types, not instances, so
`principal == Dekopon::Principal::"typo"` is perfectly well typed and would simply never match.
`PolicyEngine::new` walks every entity literal in every policy and refuses any name the declared
world does not contain.

## Context

Capability actions carry `{ via?, subject?, agent?, effect, risk, idempotency }`. `agent.prompt`
carries `{ via?, subject?, agent? }` — it has no capability to classify, and strict validation
turns a policy that reads `context.effect` there into a startup error rather than a per-request
evaluation failure.

Every value is rendered by the broker from authenticated transport state or owner-controlled
configuration:

- `via` — the attestor peer an attested context was derived through; absent for direct peers. This
  is the hinge that keeps attested and direct authority disjoint.
- `subject` — the canonical external subject an attested context stands for.
- `agent` — the agent identity of an agent actor; absent for human and service actors.
- `effect` / `risk` / `idempotency` — the trusted classification the broker will execute under,
  matched byte for byte against the loaded manifest at startup.

Message content and provider input are deliberately **not** context. Conditioning authorization on
untrusted input is a plausible future addition, but it needs a settled schema treatment for open
JSON first; until then no policy can be made to depend on a value the caller supplies.

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
