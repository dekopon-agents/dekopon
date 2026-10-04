# Dekopon Helm chart

Chart `0.21.1` deploys application `v0.32.0`. With empty `image.tag` and `image.digest`, defaults
render `ghcr.io/dekopon-agents/dekopon:v0.32.0`; set `image.digest` to the published
multi-platform index digest to pin it. The image helper selects `image.digest` before `image.tag`.
With `broker.providerSet.enabled: true`, the chart runs `dekopon-brokerd provider precompile`
after file preparation and before broker startup, using the broker image, UID, provider-set
subPath and resource limits. The image must be v0.32.0 or later; earlier images lack the command.
The sync hook must have written the lock and blobs before the pod starts. Without a managed
provider set, there is no precompile init container.

Read the [Kubernetes installation and operations guide](https://github.com/dekopon-agents/dekopon/blob/main/docs/kubernetes.md)
for installation, configuration, credential and storage handling, versioning, and chart publishing.

This pointer is included in the chart archive so `helm show readme` can locate the canonical guide.
