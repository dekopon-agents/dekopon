---
name: Wish
about: A queued change, opened with the rubric every open issue carries
labels: enhancement
---

| State | Breaking | Effort | Gateway | Broker | Provider contract | Goal |
|---|---|---|---|---|---|---|
| ? | ? | ? | ? | ? | ? | ? |

**Done when:** ?
**Also touches:** ?
**Blocked by:** none
**Open decisions:** none

## Why

## Where it stands

## Shape

<!--
Rubric values. Replace every ? before filing.

State: not started | designed (a settled design, nothing built) | partly shipped (cite the PR) |
  shipped (close the issue instead)
Breaking: no, or yes (scope) when the change gets a CHANGELOG "Breaking (scope)" bullet and a
  docs/upgrading.md section. Scopes: config, broker protocol, provider SDK, API, audit records,
  behavior (a deleted guarantee)
Effort: S  one PR, one crate, under ~300 changed lines (#246)
        M  a few crates, ~300-1,500 lines (#252)
        L  ~1,500-5,000 lines, or gateway and broker together, or a new host import or provider
           repo (#238, #217)
        XL over ~5,000 lines, or a fleet-wide provider re-pin (#260, #257)
Gateway: yes if it touches dekopond, dekopon-agent, dekopon-shell or dekopon-model
Broker: yes if it touches dekopon-brokerd, -broker, -broker-host, -http-host, -storage-host,
  -policy or -capability
Provider contract: none | SDK (additive Rust API, no re-pin) | WIT add (new import, no package
  bump) | WIT bump (every provider re-pins)
Goal: 1 credentials unleakable | 2 one trace, complete | 3 extensible through Wasm |
  product (user-facing behavior) | hygiene (memory, tests, docs, deletions)
Done when: the observable final state, one to three checkable sentences
Open decisions: each with the answer to take
-->
