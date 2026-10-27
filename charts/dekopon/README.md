# Dekopon Helm chart

Chart `0.49.0` targets application `v0.62.0`.
With empty `image.tag` and `image.digest`, the chart renders
`ghcr.io/dekopon-agents/dekopon:v0.62.0`; a set `image.digest` wins over the tag.
With `console.enabled: true` and empty `console.image.tag` and `console.image.digest`,
the console uses the main image, including its tag, variant, or digest. An explicit console tag
or digest uses `console.image.repository`, which defaults to `ghcr.io/dekopon-agents/dekopon`.
With `broker.providerSet.enabled: true`, the chart runs `dekopon-brokerd provider precompile`
after file preparation and before broker startup, using the broker image, UID, provider-set
subPath and resource limits. The image must be v0.32.0 or later; earlier images lack the command.
The sync hook must have written the lock and blobs before the pod starts. Without a managed
provider set, there is no precompile init container.

Read the [Kubernetes installation and operations guide](https://github.com/dekopon-agents/dekopon/blob/main/docs/kubernetes.md)
for installation, configuration, credential and storage handling, versioning, and chart publishing.

This pointer is included in the chart archive so `helm show readme` can locate the canonical guide.

### Durable gateway attachments

Enable `gateway.journal.enabled` and `gateway.assets.enabled` together. Set gateway configuration
`sessions.assets.path` to `gateway.assets.mountPath` (default `/var/lib/dekopon-gatewayd/assets`),
beside `sessions.journal.path`. The asset directory is a separate gateway-owned subtree of the state
claim; its default subdirectory is `gateway-assets`. Overlapping journal/store paths and clashing
state subdirectories refuse rendering. The key and gateway image must roll out together: older
gateways refuse `sessions.assets`, and the new gateway requires it when the journal is configured.
The pin reverts as a whole; an older gateway ignores the asset directory.

The default attachment budget is 1 GiB. With a journal, retained uploads live on the state claim;
without a journal they remain in gateway temporary storage. Gateway temporary and broker output
volumes retain their own sizing limits and must accommodate the configured budget and staging.
`ci/test-assets.py` checks render-time refusals and mounts. `ci/verify-init-permissions.sh` also
runs the rendered init command and writes to the mounted asset subtree as the gateway UID on a
Docker local filesystem volume, modelling the claim's `local-path` backing.
