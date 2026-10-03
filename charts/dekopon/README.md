# Dekopon Helm chart

Chart `0.20.0` deploys application `v0.31.0`, pinned to the published multi-platform image index
`sha256:f8874ef28fd366b98a9702e2c594204161d5e157733b8f49601f705a3a4e3a8f`.
The image helper selects `image.digest` before `image.tag`; defaults therefore render
`ghcr.io/dekopon-agents/dekopon@sha256:f8874ef28fd366b98a9702e2c594204161d5e157733b8f49601f705a3a4e3a8f`.
To select a different tag, also clear `image.digest`.

Read the [Kubernetes installation and operations guide](https://github.com/dekopon-agents/dekopon/blob/main/docs/kubernetes.md)
for installation, configuration, credential and storage handling, versioning, and chart publishing.

This pointer is included in the chart archive so `helm show readme` can locate the canonical guide.
