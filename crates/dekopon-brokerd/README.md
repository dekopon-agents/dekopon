# dekopon-brokerd

Dekopon's privileged half: it authenticates each socket peer by UID, asks Cedar who may act,
bounds the call by the operator's constraint set, and runs the Wasm provider, injecting a
credential neither provider nor agent sees. Any process under a mapped UID acts as its principal.

Run `dekopon-brokerd --help`, and `dekopon-brokerd check <CONFIG>` before deploying
([guide](https://github.com/dekopon-agents/dekopon/blob/main/docs/deploy.md), [design](https://github.com/dekopon-agents/dekopon/blob/main/docs/design.md)).
