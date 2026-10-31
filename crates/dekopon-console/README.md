# dekopon-console

A terminal client for a catalog agent behind a running broker. It runs the model session itself
and proposes each capability call for a subject the broker must map; naming a subject grants
nothing. Anyone running code as the console's UID can propose any subject the broker lets that UID
attest.

Run `dekopon-console --help` ([guide](https://github.com/dekopon-agents/dekopon/blob/main/docs/deploy.md), [design](https://github.com/dekopon-agents/dekopon/blob/main/docs/design.md)).
