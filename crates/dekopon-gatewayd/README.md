# dekopon-gatewayd

Dekopon's unprivileged half: it routes chat messages to catalog agents, runs the model with a
sandboxed shell, and proposes every capability call to the broker. It holds model keys, never
policy or provider credentials, and gives coding agents in outside VMs keyless model access.

Run `dekopon-gatewayd --help`, and `dekopon-gatewayd check <CONFIG>` before deploying
([guide](https://github.com/dekopon-agents/dekopon/blob/main/docs/deploy.md), [design](https://github.com/dekopon-agents/dekopon/blob/main/docs/design.md)).
