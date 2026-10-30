# Upgrade to the current release

This page moves a deployment to 0.74.0, or back to 0.73.0, without ever running a mixed daemon
pair. Follow [every upgrade](#every-upgrade), then the [0.74.0 steps](#0740);
[`CHANGELOG.md`](../CHANGELOG.md) records what each release changed.

## Every upgrade

### Upgrade both daemons together

Install `dekopon-brokerd` and `dekopon-gatewayd` from one release. The protocol between them has no
cross-release compatibility or negotiation
([version and compatibility](../crates/dekopon-broker-protocol/README.md#version-and-compatibility)).

### Check the configuration with the new release

Run the new release's `dekopon-brokerd check` and `dekopon-gatewayd check` on the configuration each
daemon will serve, before starting either daemon on it
([broker](../crates/dekopon-brokerd/README.md#check-a-configuration),
[gateway](gatewayd.md#check-a-configuration)). Where the new binaries can sit beside the running
ones, check before stopping anything, as [on a Mac](run-on-mac.md#upgrade-and-rollback).

A check that exits 1 lists every problem the new release refuses. Fix the configuration as the
current docs describe, apply the [0.74.0 steps](#0740) and check again; start nothing until both
checks exit 0. A broker refusal of `chatMemory` routes is fixed as
[optional provider storage and chat memory](../crates/dekopon-brokerd/README.md#optional-provider-storage-and-chat-memory)
describes.

### Restart the broker first and stop it last

The gateway asks the broker for its capabilities at startup and exits non-zero when the broker does
not answer; while it drains, its sessions still call the broker.

1. Stop `dekopon-gatewayd`.
2. Send `dekopon-brokerd` `SIGTERM` or `SIGINT` and wait for it to exit. On a clean drain it
   finishes open connections, logs `broker_stopped` and exits 0; past its shutdown grace it aborts
   them and exits non-zero ([shutdown](../crates/dekopon-brokerd/README.md#configuration)).
3. Replace both binaries. With a managed provider set and the default `compileOnLoad: false`, run
   the new broker's `provider precompile`: the broker reads compiled components and refuses to
   start with `CompiledArtifactMissing` when its engine has none
   ([managed provider sets](../crates/dekopon-brokerd/README.md#managed-provider-sets)).
4. Start `dekopon-brokerd` and wait until `dekopon-brokerd probe --socket PATH`, run as the
   broker's user, succeeds ([health probe](../crates/dekopon-brokerd/README.md#health-probe)).
5. Start `dekopon-gatewayd`.

### Upgrade the chart

- Run `helm upgrade` from `oci://ghcr.io/dekopon-agents/charts/dekopon` with `--version` set to the
  chart version in [Install](kubernetes.md#install), not the application version
  ([two version numbers](kubernetes.md#two-version-numbers)).
- Values that set `image.tag` or `image.digest` keep that image: set it to `v0.74.0`,
  `v0.74.0-wasmtime-optimization` or that release's digest.
- The pod runs both daemons from one image, is replaced with `Recreate`, and starts the broker
  first and stops it last ([what it deploys](kubernetes.md#what-it-deploys)). Its `precompile`
  init container does step 3's precompile on every pod start.
- Set `configCheck.enabled` to run both checks before the release applies
  ([checking configuration](kubernetes.md#checking-configuration-before-a-sync)).

### Roll back

Keep the previous release's binaries, or its chart version and image, with the configuration and
provider lock it ran, until the new release has served. To roll back, run the steps above with that
pair and that configuration. There is no reverse data migration:

- **Compiled providers.** `precompile` keeps the cache for the running engine and removes the rest,
  so run the previous broker's `precompile` before starting it. Under the chart, the init container
  does.
- **Conversation journal** (`sessions.journal.path`). The gateway deletes a file it cannot decode
  when it next reads it, and logs `gateway_journal_reset`.
- **Wakes** (`sessions.wakes.path`). A line the gateway cannot parse stops it at startup; removing
  the file resets every pending wake and PR subscription.
- **External effects.** Rolling back undoes nothing a provider already did.
- **Chart.** `helm rollback` skips the configuration-check and provider-sync Jobs, whose default
  hooks are `pre-install,pre-upgrade`. Roll back with `helm upgrade` at the previous chart version
  and values to run them.

## 0.74.0

**Upgrade from 0.73.0.** No configuration change is needed. Both serving daemons accept an optional
`--env-file PATH` ([gateway](gatewayd.md#environment-file),
[broker](../crates/dekopon-brokerd/README.md#environment-file)); the chart passes none.

**Roll back to 0.73.0.**

- Remove `--env-file` from every launch command and set its variables in the daemon's environment
  instead: a 0.73.0 daemon refuses the option.
- Nothing else needs undoing: 0.74.0 changes no configuration, stored format or wire frame.
- 0.73.0 lacks the Wasmtime 49.0.5 fixes for RUSTSEC-2026-0422, RUSTSEC-2026-0423 and
  RUSTSEC-2026-0424.
