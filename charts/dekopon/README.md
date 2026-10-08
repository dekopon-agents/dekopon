# Dekopon Helm chart

Chart `0.26.0` targets application `v0.37.0`.
With empty `image.tag` and `image.digest`, the chart renders
`ghcr.io/dekopon-agents/dekopon:v0.37.0`; the image helper selects `image.digest` first.
With `broker.providerSet.enabled: true`, the chart runs `dekopon-brokerd provider precompile`
after file preparation and before broker startup, using the broker image, UID, provider-set
subPath and resource limits. The image must be v0.32.0 or later; earlier images lack the command.
The sync hook must have written the lock and blobs before the pod starts. Without a managed
provider set, there is no precompile init container.

Read the [Kubernetes installation and operations guide](https://github.com/dekopon-agents/dekopon/blob/main/docs/kubernetes.md)
for installation, configuration, credential and storage handling, versioning, and chart publishing.

This pointer is included in the chart archive so `helm show readme` can locate the canonical guide.
