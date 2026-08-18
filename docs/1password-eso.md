# 1Password and External Secrets — how a secret reaches a deployed Dekopon

This document traces one path end to end: a credential typed into 1Password, pulled into a Kubernetes cluster by the External Secrets Operator, and turned into a file that `dekopon-brokerd` will actually open. It is both the runbook for the two steps a human performs by hand and the explanation of what each layer in that path does and, more usefully, does not do.

**Status.** The cluster-side plumbing — an Argo CD `AppProject`, the operator, and a `ClusterSecretStore` pointed at the 1Password `Dekopon` vault — is deployed and is quoted here from the manifests that are merged in the cluster repository, `xrl/rpi-homelab`. Nothing consumes it yet: there is no `ExternalSecret` for Dekopon, and this guide stops one step short of a running pod. The last link in the chain, materializing a projected Secret into a file the broker's hygiene checks accept, is not something External Secrets can do at all; [what ESO does not solve](#what-eso-does-not-solve) is the part of this document worth reading twice.

## The path

Six layers, each with a job the next one cannot do:

1. **1Password service account** — a token scoped to `read_items` on one vault, minted once by a human.
2. **A Kubernetes `Secret` holding that token** — `op-rpi`, created by hand, never committed.
3. **`ClusterSecretStore`** — makes the vault readable from every namespace in the cluster.
4. **`ExternalSecret`** — the per-application declaration of *which* vault items become *which* Kubernetes Secret. **This does not exist for Dekopon yet.**
5. **`Secret`** — an ordinary Kubernetes Secret, written and refreshed by the operator.
6. **A file on disk inside the pod** — owner-only, regular, single-link. **ESO cannot produce this**, and neither can any Kubernetes volume.

Steps 1 and 2 are the runbook below. Step 3 is deployed. Step 4 arrives with the first application that needs a secret. Steps 5 and 6 are where the interesting failure lives.

## What is deployed today

### The `AppProject`

`apps/external-secrets-project.yaml` is the first scoped project in that cluster; every other application there still runs under `default`, which permits every repository, every destination, and every cluster-scoped resource.

```yaml
apiVersion: argoproj.io/v1alpha1
kind: AppProject
metadata:
  name: external-secrets
  namespace: argocd
  annotations:
    argocd.argoproj.io/sync-wave: "-2"
spec:
  destinations:
    - namespace: "external-secrets"
      server: "*"
  sourceRepos:
    - "https://charts.external-secrets.io"
    - "git@github.com:xrl/rpi-homelab.git"
  clusterResourceWhitelist:
    - group: ""
      kind: "Namespace"
    - group: "apiextensions.k8s.io"
      kind: "CustomResourceDefinition"
    - group: "rbac.authorization.k8s.io"
      kind: "*"
    - group: "admissionregistration.k8s.io"
      kind: "*"
    - group: "external-secrets.io"
      kind: "ClusterSecretStore"
```

The scoping is worth reading as a list of what the operator's Applications may touch and nothing else: two source repositories, one destination namespace, and five cluster-resource entries — two of them group wildcards, because the chart's RBAC and webhook objects span several kinds each. ESO is the widest-reaching workload in that cluster — it installs 25 CRDs, cluster RBAC, and a validating webhook — which is precisely the argument for giving it a project rather than leaving it under `default`. A project is not a security boundary against a compromised operator; it is a boundary against a mistake in the chart or in the repository, and the resources listed above are the exhaustive statement of what such a mistake could reach.

Two entries exist here that a store riding along in a root kustomize build would not need: the git repository in `sourceRepos`, and `external-secrets.io/ClusterSecretStore` in `clusterResourceWhitelist`. Both are consequences of the split described next. The git URL must be the SSH form — the repository credential Secret in that cluster is keyed on that exact URL, and an `https://` spelling fails authentication rather than falling back.

The file lives in `apps/` rather than a `projects/` directory because the root app-of-apps reads `path: apps` non-recursively; a subdirectory is silently ignored, which is the worst available failure mode.

### Two Applications, not one

The operator and the store are separate Applications. The reason is not taste.

**Argo dry-runs every resource in an Application before applying any of them.** A `ClusterSecretStore` shipped alongside the chart that defines its CRD therefore fails its first sync with `no matches for kind`, and a sync-wave annotation inside the Application does not help, because the dry-run happens before any wave runs. A second Application sidesteps the ordering entirely: by the time it syncs, the CRDs are established.

The operator, `apps/external-secrets.yaml`:

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: external-secrets
  namespace: argocd
  annotations:
    argocd.argoproj.io/sync-wave: "-1"
spec:
  project: external-secrets
  source:
    repoURL: https://charts.external-secrets.io
    targetRevision: 2.9.0
    chart: external-secrets
    helm:
      releaseName: external-secrets
      values: |
        resources:
          requests:
            cpu: 50m
            memory: 128Mi
          limits:
            cpu: 500m
            memory: 256Mi
        webhook:
          resources:
            requests:
              cpu: 25m
              memory: 64Mi
            limits:
              cpu: 250m
              memory: 192Mi
        certController:
          resources:
            requests:
              cpu: 25m
              memory: 64Mi
            limits:
              cpu: 250m
              memory: 192Mi
  destination:
    server: https://kubernetes.default.svc
    namespace: external-secrets
  syncPolicy:
    automated:
      prune: true
      selfHeal: true
    syncOptions:
      - CreateNamespace=true
      - ServerSideApply=true
```

The inline `helm.values` block overrides only what differs from the chart's own defaults. That cluster has no `LimitRange` and no `ResourceQuota`, so every workload states its own numbers and the chart ships `resources: {}` for all three deployments; everything else — one replica, CRD installation, no ServiceMonitor — is already the chart default and is not repeated.

The store, `apps/external-secrets-store.yaml`, carries no wave annotation at all:

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: external-secrets-store
  namespace: argocd
spec:
  project: external-secrets
  source:
    repoURL: git@github.com:xrl/rpi-homelab.git
    targetRevision: main
    path: manifests/external-secrets
  destination:
    server: https://kubernetes.default.svc
    namespace: external-secrets
  syncPolicy:
    automated:
      prune: true
      selfHeal: true
    syncOptions:
      - CreateNamespace=true
```

### Why the waves are negative

Two ordering constraints, one annotation each.

An `Application` whose `project:` names a project that does not exist is rejected by the application controller, and the `AppProject` and both Applications are children of the same app-of-apps sync. So the project takes `sync-wave: "-2"` and the operator `"-1"`.

They are negative rather than `0` and `1` because every other application in that cluster is unannotated, which means wave `0`. Numbering the new work upward would have inserted it into an existing ordering and changed when everything else syncs; numbering it downward runs it ahead of wave `0` and leaves every existing application's position untouched. The store then needs no annotation of its own: it sits in the default wave, by which point the operator's CRDs are established.

### `ServerSideApply=true` is a CRD rule, not an ESO quirk

The `secretstores` and `clustersecretstores` CRDs render at roughly 688 KB each. A client-side apply stores the full object in the `kubectl.kubernetes.io/last-applied-configuration` annotation, which the API server caps at 262144 bytes. Without `ServerSideApply=true` the first sync does not degrade — it fails outright with `metadata.annotations: Too long`.

This generalizes. Any Argo Application that installs CRDs of a serious size needs `ServerSideApply=true`, and the failure is at sync time rather than at review time, so it is worth setting before the first sync rather than after it.

### The `ClusterSecretStore`

`manifests/external-secrets/op-personal-dekopon-clusterstore.yaml`:

```yaml
apiVersion: external-secrets.io/v1
kind: ClusterSecretStore
metadata:
  name: op-personal-dekopon-clusterstore
  annotations:
    argocd.argoproj.io/sync-options: SkipDryRunOnMissingResource=true
spec:
  refreshInterval: 3600
  provider:
    onepasswordSDK:
      vault: Dekopon
      auth:
        serviceAccountSecretRef:
          name: op-rpi
          key: OP_TOKEN
          namespace: external-secrets
```

Four things in that document decide how the rest of this guide reads.

**`ClusterSecretStore`, not `SecretStore`.** The claim being made is "this cluster can read the `Dekopon` vault", not "this namespace can". A namespaced `SecretStore` would have to be duplicated into every namespace that ever wants a Dekopon secret, and each copy would need its own token Secret. There is no `spec.conditions`, so any namespace may name this store from an `ExternalSecret`.

**`onepasswordSDK`, not `onepassword`.** ESO ships two 1Password providers. `onepassword` talks to a self-hosted **1Password Connect** server — a second Deployment, two containers, and a credentials file to manage. `onepasswordSDK` uses 1Password's Go SDK against a service account token and needs no server. Upstream tags both providers *alpha*, so this is not a stability trade; one of them simply costs an extra deployment.

**`vault: Dekopon`** is the vault name as it appears in 1Password, with no `op://` prefix — the provider adds it.

**`serviceAccountSecretRef` is the contract the runbook has to satisfy.** Secret `op-rpi`, key `OP_TOKEN`, namespace `external-secrets`. None of those three are free choices in step 2 below; they are a reference that must resolve. The `namespace` field is mandatory here specifically because a `ClusterSecretStore` has no namespace of its own for the reference to default to.

`SkipDryRunOnMissingResource=true` is belt-and-braces. The wave ordering above already guarantees the CRD exists before this resource syncs; the annotation keeps a manual out-of-band sync from failing validation if it ever does not.

`refreshInterval: 3600` throttles store *validation* — a 1Password `list vaults` read — to hourly rather than the controller's roughly five-minute default. It is integer seconds, per the schema, and is unrelated to how often an `ExternalSecret` refreshes its own data.

## Runbook

Two steps, both performed once by a human, both producing something no manifest can produce.

### Step 1 — create the 1Password service account

#### Before you start: the Families-plan wall

`op account get` reports `Type: FAMILY` for this account. Service accounts on a Families plan require the **Family Organizer** role, and a member without it cannot create one — the command fails on entitlement, not on syntax.

If it does fail there, **1Password Connect is not the way around it.** Connect needs a credentials file issued from the same Secrets Automation surface that gates service accounts, so swapping the `onepasswordSDK` provider for `onepassword` moves the problem without solving it. That is a wall, not a workaround — the resolution is the account's role or plan — which is why it sits above the command rather than in a troubleshooting section at the bottom.

```console
op service-account create rpi-eso \
  --vault Dekopon:read_items \
  --account my.1password.com
```

`read_items` is the default when a `--vault` flag names no permission, and it is written out anyway: the alternatives are `write_items` and `share_items`, this account must never gain either, and a permission stated explicitly is one a later reader can check. **Vault permissions are immutable after creation** — widening or narrowing them means minting a new account.

`--can-create-vaults` exists. Do not pass it. A service account that can create vaults can create one outside the scope anybody reviewed.

**The token is printed exactly once and cannot be retrieved again.** There is no "show token" screen and no API to re-read it. Capture it before the terminal scrolls, and store it back in the `Dekopon` vault as an `API_CREDENTIAL` item so a future cluster rebuild does not require minting a replacement.

The account name is a label. Nothing in the cluster reads it; only the token matters. The cluster repository's own README names the same account `eso-rpi-homelab`, which is the same thing under a different label.

### Step 2 — seed the token into the cluster

```console
ssh xlange@rpi.lan
read -rs OP_TOKEN
sudo -n k3s kubectl create namespace external-secrets --dry-run=client -o yaml | sudo -n k3s kubectl apply -f -
sudo -n k3s kubectl -n external-secrets create secret generic op-rpi \
  --from-literal=OP_TOKEN="$OP_TOKEN"
unset OP_TOKEN
```

**The names are the `ClusterSecretStore`'s.** Secret `op-rpi`, key `OP_TOKEN`, namespace `external-secrets` — change any one of them and the store resolves nothing. Re-running the same `create secret` command after a `delete` is also how the token is rotated later.

**`read -rs` rather than an argument, on purpose.** A token passed on a command line is a process argument: visible in `ps` to every user on the host for as long as the command runs, and recorded verbatim in shell history. `read` puts it in a shell variable instead, `-s` keeps it off the terminal, `-r` stops a backslash in the token from being eaten as an escape, and `unset` drops it when the work is done. The token does still reach `kubectl` as an argument in the line above, so it is exposed in `ps` for the lifetime of one command; what this avoids is a value written into a history file that outlives the session.

The namespace is created idempotently first because Argo's `CreateNamespace=true` only creates it at the operator's first sync, and this Secret needs somewhere to live if it is seeded before then. Applying the client-side dry-run output is the idempotent form; a plain `create namespace` fails on the second run.

**This one credential is created by hand and is never committed.** It is the bootstrap the whole chain derives from, and it can read every item in the vault. That is a different class of secret from an application password, and it is a deliberate departure from the surrounding convention: `manifests/openobserve/secret.yaml` in the same repository commits a root password as plain base64, on the reasoning that the host is LAN-only and the blast radius is one homelab app. Neither half of that reasoning survives contact with a token that reads an entire vault. The cost of the departure is honest — `external-secrets-store` is not self-bootstrapping, and a rebuilt cluster needs this step re-run by hand before the store goes ready.

### Verify

```console
sudo -n k3s kubectl get clustersecretstore op-personal-dekopon-clusterstore
```

`Valid` in the status column is the gate. Until step 2 is done the store reports `NotReady` with a missing-secret error, which is the expected state of a correct deployment waiting on a human rather than a broken sync. A store that is `Valid` proves the token exists, is well-formed, and can list the vault; it proves nothing about whether any particular item is readable.

## What comes next, and does not exist yet

An `ExternalSecret` is the per-application declaration that names this store, names the 1Password items to read, and names the Kubernetes `Secret` to write. **There is no `ExternalSecret` for Dekopon.** Writing one is the job of the change that first deploys a Dekopon component into a cluster, because the mapping from vault items to Secret keys is a property of that deployment and not of this plumbing.

When that change lands it will need to state two things this document cannot state for it: which vault items back which of the broker's credential entries, and what happens on deletion. ESO's `deletionPolicy` defaults to `Retain`, so removing an item from 1Password revokes nothing — the Kubernetes Secret and every copy already mounted into a pod survive. Revocation is an action taken at the credential's own issuer, not in the vault.

## What ESO does not solve

External Secrets is a *provisioning* mechanism. It ends at a Kubernetes `Secret`, and **a Kubernetes Secret is not a file `dekopon-brokerd` will open.**

### The checks

`dekopon-brokerd` reads its configuration, its Cedar policy, and its credentials under one hygiene discipline and refuses anything that does not meet it; `dekopond` applies the same rule to its own configuration file. The checks live in [`crates/dekopon-brokerd/src/credentials.rs`](../crates/dekopon-brokerd/src/credentials.rs), [`crates/dekopon-brokerd/src/socket.rs`](../crates/dekopon-brokerd/src/socket.rs), and [`crates/dekopond/src/config.rs`](../crates/dekopond/src/config.rs):

| File | Opened with | Must be |
|---|---|---|
| `broker.yaml`, `policies.cedar`, `dekopond.yaml` | `O_NOFOLLOW` | regular, `uid == geteuid()`, `nlink == 1`, `mode & 0o022 == 0`, byte-capped |
| `broker-credentials.yaml` | `O_NOFOLLOW` | the same, but `mode & 0o077 == 0` |
| provider `.wasm`, audit path | `symlink_metadata` | regular, server-owned, `nlink == 1`, `mode & 0o022 == 0`, protected parents, no group/world-writable non-sticky ancestor |

The credentials file is the strict one because the threat it answers is different. Everywhere else the question is "could another process have *changed* this?", which group and world *write* bits answer. For a file whose entire content is secrets, the question is "could another process *read* this?", so the mask covers group and world read as well — `0o077` rather than `0o022`. The module comment says so directly: readability is the whole threat.

### Why every mount shape fails

A Kubernetes `Secret` mounted as a volume is not a directory of files. It is a symlink farm: each `key` is a symlink to `..data/key`, and `..data` is itself a symlink to a timestamped directory, which is how the kubelet swaps a refreshed Secret in atomically. Three shapes, three distinct failures:

| Mount shape | What the broker sees |
|---|---|
| `secret` or `configMap` volume | `O_NOFOLLOW` refuses to traverse the symlink — the open fails with `ELOOP` before any ownership or mode check runs |
| the same, plus `subPath` | a real regular file, but `uid` is root and the mode is `0644` — fails the ownership check, and the credentials file fails `0o077` as well |
| either, plus `fsGroup` | the pod's UID owns the file, but the mode is `0640` — passes `0o022`, **fails `0o077`**, so the credentials file specifically is rejected |

That third row is the one that catches people: `fsGroup` is the standard fix for "the container cannot read its mounted Secret", it makes the file group-readable, and group-readable is exactly what the credentials file refuses. The fix for the general problem is the cause of the specific one.

**So no Kubernetes volume can present a file either daemon will read.** The answer is an init container that copies each projected file into a real owner-only regular file — an `install -m 0600` per file, owner-only directories, and a `stat` assertion afterwards — with the projected volume visible only to that init container and never to the daemons. The chart-side implementation of exactly that lives in the `feat/helm-chart` work ([#71](https://github.com/dekopon-agents/dekopon/pull/71)), which also carries the measured evidence behind the table above; it is not merged, so nothing in this repository ships it today.

**ESO solves provisioning, not file hygiene.** Adopting it removes the question of how a secret gets into the cluster and leaves the question of how it becomes a file entirely untouched.

### The ChatGPT credential is a different problem again

One credential does not fit the pattern at all. The `chatgptSubscription` model kind's credential file holds a refresh token that **rotates**: each refresh invalidates its predecessor and the replacement is written back through a same-directory temporary file and an atomic rename. That needs a writable *directory*, not a writable file, and it means a read-only projected Secret breaks at the first refresh and presents an already-invalidated token on the next restart.

The lifecycle it needs instead is seed-once: export a working local credential, store it in the vault, project it, copy it into a writable directory on first start only, and let refreshes persist there while the vault copy drifts out of date. That work — a `dekopon auth chatgpt export` command and the document describing the lifecycle — is on the `feat/auth-export` branch ([#72](https://github.com/dekopon-agents/dekopon/pull/72)) and is not merged. Treat it as the authority on that credential rather than reasoning about it from this document.

## Related documents

- [`security-model.md`](security-model.md) — the trust boundaries the file hygiene above enforces, and what a single-UID deployment does and does not separate.
- [`../crates/dekopon-brokerd/README.md`](../crates/dekopon-brokerd/README.md) — the configuration, credentials, and policy file contracts in full, including the credentials file this guide's Secret would eventually become.
- [`dekopond.md`](dekopond.md) — the gateway's configuration, which names environment variables rather than secrets and so consumes an ESO-provisioned Secret differently from the broker.
- [`broker-http.md`](broker-http.md) — how a resolved credential is bound to a destination and injected, once it exists as a file.
- [`observability.md`](observability.md) — the other half of this cluster's deployment story, including the OpenObserve endpoint the same host serves.
