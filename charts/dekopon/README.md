# Dekopon Helm chart

Deploys Dekopon as one pod: `dekopon-brokerd`, plus `dekopon-gatewayd` when enabled (the broker then
runs as a native sidecar), under separate UIDs that talk only over a Unix socket. There is no
TCP path between them, because the broker identifies each caller by the socket peer's UID. Out of the
box the broker starts with a probe provider and grants nothing.

`values.yaml` is the reference. Installation and operations are in
[the deployment guide](https://github.com/dekopon-agents/dekopon/blob/main/docs/deploy.md).
