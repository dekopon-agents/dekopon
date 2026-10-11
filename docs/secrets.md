# Public secret references and the private secret map

**Status: current.** This document defines the broker-owned secret system: public inert DRNs, a
separate Cedar decision, an owner-only map to physical stores, invocation-pinned resolution, and
native HTTP Basic/Bearer sinks. Implicit `credential`/`agents.<id>.credentials` bindings run beside it,
under [`security-model.md`](security-model.md#per-agent-credentials-and-where-their-boundary-stops).
*Committed direction:* `credential`/`agents.<id>.credentials` bindings will be replaced by public DRNs;
current configurations remain supported until that migration ships
([migration requirements](design.md#legacy-credential-bindings)).

## The guarantee

A model may name a secret; it never receives one.

```text
model-authored script
  -> provider command word with a public DRN on its argv
  -> the guest's run-command answer: capability, input, typed SecretUseProposal
  -> ordinary capability Cedar decision
  -> separate secret.use Cedar decision over the exact DRN
  -> owner-authored SecretUseBinding
  -> one private-source lookup, after authorization
  -> authorization-bound native Basic/Bearer rendering
  -> constrained HTTP request
```

A DRN never appears in a provider's invoke input or in either provider WIT interface; the invoked
component sees an ordinary `{uri, method, headers, body}` input. A provider's command proposal may
name one — the command guest reads it from argv and returns it — and the broker authorizes that
exactly like any other secret use. A DRN is public text, so a
model may quote those characters as ordinary provider data; doing so has no secret semantics and
grants no resolution. Resolved bytes pass only from the broker resolver to `dekopon-http-host`,
beside an authorization committing to the same DRN, sink, and binding identifier.
`dekopon-broker-host` rejects a swapped credential.

Dekopon's model, gateway, protocol results, provider memory, evidence, audit, and telemetry
therefore never receive secret bytes. The authorized remote endpoint necessarily does. The native
host rejects a response containing the raw secret or complete rendered Authorization value, but an
endpoint can transform or semantically encode it; destination trust and narrow upstream credentials
remain part of the boundary, and [`design.md`](design.md#non-goals) rules out defending that case.

Plaintext is the one part of destination trust the broker does enforce for itself. The native HTTP
host refuses `http://` to anything but a loopback destination, because a credential injected into a
request that crosses a network in the clear is a credential on that network, and that is goal 1's
whole subject. A broker owner who runs a service that speaks only plaintext on a network they
control names that exact host in `broker.yaml`:

```yaml
http:
  plaintextHosts:
    - rpi.lan
    - openobserve.openobserve.svc
```

Entries are exact hostnames matched case-insensitively: no wildcards, no ports — the rule is about
the scheme rather than the socket — and no scheme or path. The list is validated at startup, so an
entry that could never match a host refuses the broker instead of denying a request weeks later,
and it is logged once at INFO so the trace log carries the decision. It reaches nothing on its own:
the capability's constraint set still has to name the destination in `allowedHosts` and still has
to set `allowPlaintextLoopback`, which keeps its name and now reads as "this capability may use
plaintext wherever the broker permits it". Empty, the default, is exactly the loopback-only rule.

## Public DRNs

The canonical grammar is:

```text
drn:<naming-authority>:secret:<realm>:<logical-path>
```

For example:

```text
drn:com.xrl:secret:prod:payments/blah-api-password
```

A DRN contains no backend, endpoint, account, region, cluster, namespace, vault, item, key, field,
selector, or version. Those are private-map data. A DRN is lowercase ASCII, at most 512 bytes, has
one DNS-like naming authority, a validated realm, and slash-separated nonempty path segments. It
has no percent encoding, whitespace, query, fragment, backslash, empty segment, `.` or `..`.

Knowing a DRN grants nothing. It is safe to copy and remains inert after revocation. A name can
disclose logical purpose, so deployments that consider `prod/payroll` sensitive should choose a
less descriptive logical path.

## Agent syntax

A provider command proposes secret use; the shell has no secret syntax of its own. The model passes
the DRN on the command word's argv, and the guest's `run-command` answer returns it as `secretUse`
beside the capability and input:

```text
{"outcome": "proposed",
 "capability": "http-probe.fetch",
 "input": {…},
 "secretUse": {"kind": "httpBearer", "secret": "drn:com.xrl:secret:prod:api/token"}}
```

`httpBasic` adds `username`, and `secretUse` is absent when the proposal names no secret. A provider
sets it with the typed SDK's `Proposal::with_secret_use`. Providers still using the previously
published `dekopon-provider-sdk` 0.15.0 set the historical `CommandInvocation::secret_use` field.

*Committed direction:* the out-of-tree `curl` provider takes the word `curl` and accepts the two
credential forms the retired shell builtin did:

```sh
curl --oauth2-bearer '${drn:com.xrl:secret:prod:api/token}' \
  https://api.example.com/v1/thing

curl -u 'userA:${drn:com.xrl:secret:prod:api/password}' \
  https://api.example.com/v1/thing
```

The broker checks the proposal, not the provider's parsing of it: a `secretUse` whose DRN is not
canonical does not decode, and the owner's binding — never the argv — fixes the sink, username,
destination, and injection count. Bare DRN characters elsewhere are ordinary public text with no
resolution semantics. Immediate/direct invokers refuse secret use; only a broker-backed leg
forwards the typed top-level proposal. Invocation is one method, so every
broker-backed session reaches it, a `dekopon-gatewayd` chat session included, and a wrapper that records a
call or stops one at a cancellation boundary cannot drop the proposal on the way through.

## Two independent policies

A capability grant does not imply secret use:

```cedar
@id("caller-may-fetch")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"http-probe.fetch",
       resource == Dekopon::Provider::"http-probe");
```

The exact DRN needs its own statement:

```cedar
@id("caller-may-use-api-token")
permit(principal == Dekopon::Principal::"caller",
       action == Dekopon::Action::"secret.use",
       resource == Dekopon::Secret::"drn:com.xrl:secret:prod:api/token")
when { context.capability == "http-probe.fetch"
    && context.provider == "http-probe"
    && context.sink == "httpBearer" };
```

The secret context also carries the authenticated routing fields already used by capability policy:
`via`, `subject`, `agent`, and optional chat scope. An unknown, unbound, wrong-sink, wrong-username,
or policy-denied DRN produces the same `secret-denied` invocation outcome. Source lookup happens
only after both allows and the durable decision append.

## Private map

`broker.yaml` opts in with an owner-only file:

```yaml
secretMapPath: /etc/dekopon/secret-map.yaml
```

The map must be a server-owned regular single-link `0600` file opened without following a symlink,
under the same hard 1 MiB read ceiling as other trusted inputs. `mapRevision` is owner-authored
authority metadata: bump it whenever a physical source, selector, projection, or binding meaning
changes. Effective secret bindings plus that revision enter authority-bound durable-memory
continuity, while values never do. Physical locators and bootstrap paths are sensitive deployment
inventory and never appear in prompts, audit, evidence, or provider metadata.
Bootstrap credentials are never DRN-addressable, which prevents resolver cycles and use of a
source-store token as application material; the map file itself is prohibited as a `secureFile`
source. In the Helm chart,
`broker.secretBootstrapFiles` copies operator-managed Secret keys into broker-only `0600` files;
`broker.secretSourceVolumes` mounts AtomicWriter sources read-only into the broker only. AWS
sessions and GCP/Azure/Kubernetes API access tokens from other map sources must be refreshed
externally; a chart-copied file changes only after a pod rollout. `oauth2Refresh` is the one
refreshing map source. `kubernetesTokenRequest` instead reads a live kubelet-rotated
API token and mints an audience-specific token per invocation. The broker also renews the legacy
`chatgptSubscription` kind below, a different mechanism — a named credential file rather than a DRN. That legacy binding
[will be replaced by public DRNs](design.md#legacy-credential-bindings), preserving refresh support.

```yaml
apiVersion: dekopon.dev/secret-map/v1alpha1
mapRevision: prod-2026-08-25
secrets:
  - drn: drn:com.xrl:secret:prod:api/password
    source:
      kind: onePasswordConnect
      endpoint: https://connect.internal
      tokenFile: /run/dekopon-bootstrap/onepassword-token
      vault: vlt_abc123
      item: itm_def456
      field: password
      timeoutMs: 10000
    projection:
      format: utf8
    bindings:
      - id: api-basic-password
        capability: http-probe.fetch
        sink: httpBasic
        basicUsername: userA
        allowedHosts: [api.example.com]
        allowedMethods: [GET]
        allowedPaths:
          - match: exact
            path: /v1/thing
        allowQuery: false
        maxInjections: 1
```

Per binding, `basicUsername` is required for `sink: httpBasic` and must be absent for `httpBearer`;
`allowQuery` defaults to `false` and `maxInjections` to `1`; every other field is required.
Every binding is checked against the capability constraint set. Its hosts and methods must be a
subset, and its injection count cannot exceed `maxRequests`. A map cannot introduce HTTP authority.
Duplicate DRNs, binding IDs, or `(DRN, capability, sink, username)` tuples fail startup. The map
holds at most 256 DRNs, 64 bindings per DRN, and 1,024 bindings total. Validation reports all
map-level conflicts together.

## Path and query enforcement

A secret binding uses exact or segment-prefix path rules:

```yaml
allowedPaths:
  - match: exact
    path: /api/v1/thing
  - match: segmentPrefix
    path: /api/v1/items
```

The grammar excludes percent encoding, backslashes, controls, whitespace, repeated
slashes, query/fragment text, and literal `.`/`..` segments. Matching uses the canonical path from
the same URL object dispatched by the native client. Segment prefix matches `/api/v1/items` and
`/api/v1/items/7`, not `/api/v1/items-admin`. Trailing slash is significant. Query is denied unless
`allowQuery: true`; query values are never a place a secret may be inserted.

This is an HTTP routing boundary, not row-level authorization. If one exact endpoint accepts an
object identifier in a request body, Dekopon does not infer that object's authority from the path.

## Source and projection model

Each source resolves to bounded bytes. An optional private projection runs after fetch:

```yaml
projection:
  format: raw        # raw | utf8 | json | yaml
  pointer: /password # JSON/YAML only, RFC 6901
  decodeBase64: false
```

JSON and YAML use a strict value decoder: duplicate keys, non-string map keys, non-finite numbers,
over-depth documents, over-count containers, oversized keys/scalars, and trailing JSON are refused.
YAML anchors, aliases and custom tags are conservatively disabled by rejecting `&`, `*`, or `!`
bytes before parsing—even inside quoted YAML; use JSON or raw projection when those literal bytes
are required.
The selected value must be a string. Base64 decoding is explicit and occurs after selection.
Source and final Basic/Bearer material ceilings are 1 MiB and 4 KiB respectively; source responses also stop at
128 headers/64 KiB of header bytes, and bootstrap header tokens stop at 16 KiB. Empty material is refused by the
native Basic/Bearer constructor.
Every remote source kind also accepts `timeoutMs` (default `10000`, at most `120000`), the deadline
for its single fetch, and every configured `endpoint` or `vaultUrl` must be a credential-free HTTPS
URL or a literal loopback HTTP URL, except `kubernetesTokenRequest`, which requires HTTPS with its
configured CA. `secureFile` and `kubernetesProjection` take neither field.

### `oauth2Refresh`

```yaml
source:
  kind: oauth2Refresh
  recordPath: /var/lib/dekopon/broker-chatgpt/example-oauth2.json
  tokenEndpoint: https://auth.example/oauth/token
  clientId: public-client-id
  timeoutMs: 10000
```

Only `recordPath`, `tokenEndpoint`, `clientId`, and optional `timeoutMs` are accepted. The broker
reads the broker-UID-owned, regular, single-link `0600` JSON record (at most 64 KiB):
`{"version":1,"access":"...","refresh":"...","expiresAt":<Unix seconds>}`. Version 1
and non-empty tokens are required. Every authorized resolution takes the sibling `.lock`, re-reads
the record, and reuses its access token until 60 seconds before expiry; then it sends one OAuth
refresh-token grant to the configured endpoint, writes the replacement `0600` file by atomic rename,
and returns the access token. An omitted response `refresh_token` retains the predecessor; no
in-memory token cache or retry exists. Two DRNs may share one record and lock only when their
`tokenEndpoint` and `clientId` match. The record needs its own file-name stem: the broker writes
`<stem>.tmp-<pid>` beside it and sweeps `<stem>.tmp-*` under the lock, so a sibling with the same
stem (for example ChatGPT `auth.json` beside `auth.oauth`) shares those temporaries. The configured
endpoint must use HTTPS or literal loopback HTTP; redirects and ambient proxies are disabled.

`reauthorization-required` means the OAuth error code says the token family is retired: re-import
a working record. Other 4xx responses are `rejected`, 5xx and connection failures are `transport`,
a deadline is `timeout`, and a malformed/incomplete response is `malformed`. An insecure file is
`insecure-file`; a missing file is `io`.
`bootstrap-reflected` means the access contains the current or predecessor refresh token; a
reflected rotation is saved already expired (`expiresAt: 0`), so the next resolution refreshes
again rather than serving it.
`too-large` means the record file on read or the response headers exceed their ceilings, or the
returned token exceeds the final material limit after the rotation is saved. An over-ceiling
response body or serialized record is `malformed`.
`internal` means a blocking or refresh task did not complete.
If saving a rotated token fails, an otherwise valid invocation still returns the new access token
and logs the path and I/O error. A refresh whose response is lost costs
one re-import: the old refresh token remains on disk, and the next use may get `invalid_grant`.
A rotation the broker cannot parse or cannot save costs the same re-import.

The broker image is distroless: it has no shell, `cat`, `mv`, or `tar`, so neither `kubectl exec` with
`sh` nor `kubectl cp` works. To enroll or reset from a trusted local `record.json`, stop the broker
first: a refresh in flight could otherwise overwrite the reset. Replace `<namespace>`,
`<deployment>`, `<release>`, `<broker-pod>`, and `<state-claim>` with the live values; choose the
deployment's broker pod from the listing before scaling. The chart's generated state claim is
`<deployment>-state` unless `state.existingClaim` overrides it. Pause any controller that would
restore the deployment's replicas while the broker is stopped. The example assumes the
broker has a writable state mount at `paths.stateDir` (`/var/lib/dekopon`) with subdirectory
`broker-chatgpt` and record file `example-oauth2.json`; adjust the maintenance pod's `subPath` and
both `/record/...` paths to match the owner-controlled map and the broker's writable mount. That
subdirectory must already exist on the claim, owned by broker UID 65532 and writable by it.
Use the chart's pinned `initImage` (BusyBox), not the distroless broker image:

```sh
kubectl -n <namespace> get pods -l app.kubernetes.io/instance=<release>
kubectl -n <namespace> scale deployment/<deployment> --replicas=0
kubectl -n <namespace> wait --for=delete pod/<broker-pod> --timeout=180s
kubectl -n <namespace> run oauth2-record-reset --restart=Never --attach --stdin \
  --image=busybox@sha256:fc6dddc4c44b1bfe37f41cae8e67d1693828e8f42a91862816d7953e2c9d3f23 \
  --overrides='{"apiVersion":"v1","spec":{"securityContext":{"runAsNonRoot":true,"runAsUser":65532,"runAsGroup":65532},"containers":[{"name":"oauth2-record-reset","image":"busybox@sha256:fc6dddc4c44b1bfe37f41cae8e67d1693828e8f42a91862816d7953e2c9d3f23","stdin":true,"stdinOnce":true,"command":["/bin/sh","-c","set -eu; umask 077; cat > /record/example-oauth2.tmp && mv /record/example-oauth2.tmp /record/example-oauth2.json"],"volumeMounts":[{"name":"state","mountPath":"/record","subPath":"broker-chatgpt"}]}],"volumes":[{"name":"state","persistentVolumeClaim":{"claimName":"<state-claim>"}}]}}' \
  < record.json
kubectl -n <namespace> delete pod oauth2-record-reset --wait=true
kubectl -n <namespace> scale deployment/<deployment> --replicas=1
```

Do not scale up if the write fails; investigate and remove the maintenance pod first. There is no
enrollment subcommand. Upgrade the broker before activating the map: 0.27.0 brokers
refuse `oauth2Refresh` as an unknown kind.

### `secureFile`

```yaml
source:
  kind: secureFile
  path: /run/secrets/api-token
```

The file is opened `O_NOFOLLOW` and must be regular, single-link, broker-UID-owned, `0600`, and
bounded. This is the universal compatibility adapter for External Secrets, Secrets Store CSI,
Docker/Nomad/systemd credentials, SOPS/age output, and vendor sidecars after they materialize a
proper file.

### `kubernetesProjection`

```yaml
source:
  kind: kubernetesProjection
  root: /var/run/dekopon-api-secret
  key: credentials.json
  declaredOrigin: secret # secret | configMap | serviceAccountToken
  acknowledgeNonSecretSource: false # required true for configMap
```

A Kubernetes Secret or ConfigMap volume is not a JSON/YAML object. It is one decoded object key per
file. A key is JSON or YAML only when its own contents are that document. Secret `.data` and
ConfigMap `binaryData` have already been base64-decoded by kubelet; ConfigMap `.data` is UTF-8.

The adapter does not weaken the ordinary file loader. It reads the `..data` link, accepts one
relative generation component, opens that real generation directory without following another
symlink, and opens the configured one-component key with `O_NOFOLLOW`. An atomic `..data` swap
therefore selects either generation, never the user-visible key symlink. A group/world-writable
AtomicWriter root (commonly `01777`) is accepted only when `statvfs` proves the mount is read-only;
otherwise the root itself must not be group/world writable. The chart always mounts configured
secret sources read-only. `subPath` should not be used because it does not receive projected updates.

A projected ServiceAccount token uses `declaredOrigin: serviceAccountToken` with the same reader
and needs no `acknowledgeNonSecretSource`, just like `secret`. Kubelet refreshes the token at 80%
of its TTL; the broker reads the current projection per invocation. Mount it live through
`broker.secretSourceVolumes`, never an init copy or `subPath`.

The on-disk layout cannot prove whether kubelet sourced a Secret, ConfigMap, or ServiceAccount token, so every projection
entry must explicitly state `declaredOrigin`. A ConfigMap declaration requires `acknowledgeNonSecretSource: true`; this is
an explicit operator claim rather than filesystem attestation. Values receive Dekopon's downstream
redaction but do not gain Kubernetes Secret storage/RBAC properties retroactively.

### `kubernetesTokenRequest`

```yaml
source:
  kind: kubernetesTokenRequest
  endpoint: https://kubernetes.default.svc
  bootstrapRoot: /var/run/dekopon-secrets/kubernetes-api
  tokenKey: token
  caKey: ca.crt
  namespace: agents
  serviceAccount: runner-client
  audience: vm-runner
  expirationSeconds: 600
  timeoutMs: 10000
```

All fields except `timeoutMs` are required. `bootstrapRoot` is an absolute live AtomicWriter
projection containing the broker pod's **API-audience** ServiceAccount token and cluster CA PEM
bundle. `tokenKey` and `caKey` are single path components. Use a broker-only read-only projected
volume through `broker.secretSourceVolumes`, with a `serviceAccountToken` source and the namespace's
`kube-root-ca.crt` ConfigMap; never use an init copy or `subPath`. The CA bundle is read at startup,
so CA changes require a broker restart. Only those CA roots are trusted for this source; hostname
verification remains enabled. Redirects and ambient proxies are disabled.

The broker posts `authentication.k8s.io/v1` `TokenRequest` to
`/api/v1/namespaces/<namespace>/serviceaccounts/<serviceAccount>/token`, with exactly one audience
and the configured `expirationSeconds` (an integer from 600 through 4294967295). The API determines
the actual lifetime. The broker requires a valid future `status.expirationTimestamp`, rather than
assuming the requested duration was granted, and extracts only `status.token` as application
material. An expired/malformed response or failed issuance fails this invocation before the provider
runs. The receiving service still verifies JWT issuer, audience, subject and expiry on each use;
a token that expires during an invocation is not refreshed or retried.

Every authorized resolution mints anew and re-reads the bootstrap token, so kubelet rotation is
picked up immediately. There is no token cache, background task, persistence, stale fallback or
`boundObjectRef`. Existing connection admission bounds concurrent resolutions, and `timeoutMs`
bounds the API request and response under the existing source byte/header ceilings. Namespace,
ServiceAccount and audience come only from this private map, never from provider/model arguments.
The usual per-invocation `secret.use` decision and native sink/host/path/method limits remain required.

The target ServiceAccount must already exist. Grant the broker pod's actual ServiceAccount only
`create` on core `serviceaccounts/token`, restricted by `resourceNames: [runner-client]` in the
configured namespace. A separate ServiceAccount subject can isolate a delegated caller at its
verifier; it does not give a VM guest credentials. Offline JWT verification does not immediately
revoke tokens when a ServiceAccount or bound object is deleted; expiry and the verifier's allowlist
remain the cutoff mechanisms.

Upgrade the broker binary to a version supporting this source **before** activating the new private
map; older strict decoders reject it. No provider WIT or broker protocol change is required. A chart
already supporting broker-only `secretSourceVolumes` needs no new values key; render it to verify
mount isolation. Install the verifier's subject allowlist/quota and the ServiceAccount/RBAC/mounts
before enabling the caller's capability and exact-DRN Cedar grants.

### 1Password Connect

```yaml
source:
  kind: onePasswordConnect
  endpoint: https://connect.internal
  tokenFile: /run/dekopon-bootstrap/op-connect-token
  vault: stable-vault-id
  item: stable-item-id
  field: password
```

The adapter performs one bounded `GET /v1/vaults/{vault}/items/{item}` and selects exactly one field
by ID or label. IDs are recommended; duplicate label matches refuse. Direct service-account SDK
mode and file downloads are not current. 1Password service-account/ESO users can materialize a
Kubernetes Secret and use `kubernetesProjection` or `secureFile`.

### HashiCorp Vault KV

```yaml
source:
  kind: vaultKv2       # vaultKv1 is separate
  endpoint: https://vault.internal
  tokenFile: /run/dekopon-bootstrap/vault-token
  namespace: payments  # optional
  mount: secret
  path: apps/api
  key: password
  version: 7            # optional; absent means current
```

KV v1 and v2 are distinct variants; v2 inserts the API `data` segment and optionally requests one
integer version. The logical path never contains the API-internal segment. Dynamic leased secrets,
renewal and revocation are not current: treating a lease as an ordinary versioned value would make
expiry and outcome semantics wrong.

### AWS Secrets Manager

```yaml
source:
  kind: awsSecretsManager
  region: us-east-1
  credentialsFile: /run/dekopon-bootstrap/aws-session.yaml
  secretId: arn:aws:secretsmanager:us-east-1:123456789012:secret:api
  versionStage: AWSCURRENT # mutually exclusive with versionId
```

The strict session file is:

```yaml
accessKeyId: AKIA...
secretAccessKey: ...
sessionToken: ... # optional
```

The adapter signs one `GetSecretValue` request with SigV4 and accepts `SecretString` or decoded
`SecretBinary`. No ambient SDK credential chain, instance metadata, role assumption, IRSA, retry,
or stale cache is used. A loopback `endpoint` override exists on both AWS kinds for deterministic
tests; production defaults to the regional AWS endpoint.

### AWS SSM Parameter Store

```yaml
source:
  kind: awsSsmParameter
  region: us-east-1
  credentialsFile: /run/dekopon-bootstrap/aws-session.yaml
  name: /prod/api/password
  selector: current-label # optional version or label, appended as name:selector
```

One signed `GetParameter` request always sets `WithDecryption: true`. Parameter Store remains a
separate source kind from Secrets Manager.

### GCP Secret Manager

```yaml
source:
  kind: gcpSecretManager
  tokenFile: /run/dekopon-bootstrap/gcp-access-token
  project: project-id
  secret: api-password
  version: latest
  # location: us-central1 # optional regional resource
  # endpoint: https://... # required by deployments using a non-default regional endpoint
```

The adapter accesses one version and decodes `payload.data`. Numeric versions, aliases, and
`latest` stay private selectors. The returned `dataCrc32c` is required and verified before the
payload can become material. The current bootstrap is a strict access-token file; ADC and Workload
Identity Federation are not current.

### Azure Key Vault

```yaml
source:
  kind: azureKeyVault
  vaultUrl: https://example.vault.azure.net
  tokenFile: /run/dekopon-bootstrap/azure-access-token
  secret: api-password
  version: exact-version # optional; absent means current
```

The adapter calls the fixed Key Vault secrets API version and reads textual `value`. Managed
identity/workload identity token acquisition is outside this slice; the token is reread for every
invocation.

### Kubernetes API Secret and ConfigMap

```yaml
source:
  kind: kubernetesApi
  endpoint: https://kubernetes.default.svc
  tokenFile: /run/dekopon-bootstrap/kubernetes-token
  namespace: payments
  objectKind: secret # secret | configMap
  name: api-credential
  key: password
  acknowledgeNonSecretSource: false
```

Secret `.data` and ConfigMap `binaryData` are decoded; ConfigMap `.data` is returned as UTF-8.
ConfigMap requires `acknowledgeNonSecretSource: true`. This adapter uses an explicit broker-only
access-token file and public WebPKI roots; in-cluster custom CA files, kubeconfig exec plugins, and
ambient service-account mounting are not current. The chart keeps
`automountServiceAccountToken: false`; deployments must mount only the broker-specific token they
intend to grant.

## Legacy credentials the broker renews

*Committed direction:* these entries are selected through `credential`/`agents.<id>.credentials`, which
will be replaced by public DRNs. The migration must retain the shared refresh sequence, destination
binding, and companion header described here; it is not implemented by the current private map
([migration requirements](design.md#legacy-credential-bindings)).

The legacy kinds in `broker-credentials.yaml` differ in *when the value exists*, not in how it is
bound or audited. `bearerToken` carries a `secret` an operator rotates by hand. `chatgptSubscription`
carries an absolute `authFile` instead, because a ChatGPT subscription access token expires hourly and
its refresh token rotates on every renewal:

```yaml
apiVersion: dekopon.dev/broker-credentials/v1alpha1
credentials:
  - name: chatgpt-gpt-image
    kind: chatgptSubscription
    authFile: /var/lib/dekopon/broker-chatgpt/chatgpt-auth.json
    destinations: [chatgpt.com]
```

`secret` and `scheme` are prohibited for this kind, `authFile` is prohibited for `bearerToken`, and
the file is validated as a whole: every missing field, surplus field, malformed name and duplicate
name is reported in one startup refusal.

Every legacy kind goes through the credential echo check the way a DRN-bound credential does: the
native host refuses a response whose body or headers carry the secret rather than returning it to
the component. A `bearerToken` `secret` is therefore held to a shape the check can search for — at
least 16 bytes of printable ASCII, no whitespace or control bytes — and an entry that breaks that
rule refuses startup by name. The same 16-byte floor holds for a DRN-resolved secret, which has no
startup to refuse at and fails its invocation instead.

**Hygiene.** The `authFile` goes through the same Tier A check as the credentials file itself —
regular, owned by the broker's UID, `mode & 0o077 == 0`, one hard link, opened `O_NOFOLLOW`, under a
64 KiB ceiling — and its parent directory must be owner-only **and writable**, because a rotated
record is persisted by creating a sibling temporary file and renaming it over the target. A relative
path, a symlink, a group-readable file, a read-only parent, or a document that is not a supported
Dekopon credential each refuse startup naming the cause.

**Refresh and write-back.** Resolution happens once per authorized invocation, after the decision
audit record is emitted and before the component runs, through the one implementation of that protocol in
`dekopon-model`: take an advisory lock on a sibling `.lock` file, adopt a newer record another process
wrote, renew 60 s before expiry, and persist the rotated record atomically (same-directory temporary
file, `fsync`, rename, directory `fsync`). A renewal that reached the authorization server but could
not be written back logs `chatgpt_credential_save_failed` and continues on the in-memory token,
because by then the record on disk is the retired predecessor and failing would strand the only token
that still works. The renewal is the broker's own HTTPS call: it is not charged to the invocation's
`maxRequests`, produces no HTTP evidence entry, and is invisible to the component. Audit is unchanged
— `credentialInjected: true` and the symbolic name, never the token. `dekopon-broker` reaches the
resolution through an `Arc<dyn RefreshingCredential>` the deploying process supplies, so the broker
core still holds no token endpoint of its own.

**Startup destination coverage is unchanged.** A refreshing entry answers its `destinations` without
resolving anything, so the same startup check applies: a constraint set whose `allowedHosts` are not
all covered by the credential's `destinations` refuses to start, rather than discovering the mismatch
on the first invocation against an unreachable token endpoint.

**Two headers.** This kind presents `authorization: Bearer <access>` plus the fixed companion
`chatgpt-account-id: <accountId>`, because the route refuses the bearer token without the account
identifier and that identifier is a claim inside the token the guest never sees. It is one credential
with one destination binding, not a generic header sink (see
[Current non-goals](#current-non-goals)): a guest that sets the companion name is refused rather than
overwritten, its bytes stay outside accounted request size, and evidence gained no field for it.

**One file per holder.** Give the broker its own `dekopon-gatewayd auth chatgpt login --auth-file <path>`.
Pointing it at a `chatgptSubscription` *model*'s file would have two independent holders spending one
rotating refresh token, and the authorization server retires a predecessor on every rotation, so the
family is eventually revoked for both. See
[`chatgpt-credential.md`](chatgpt-credential.md#a-second-family-for-the-broker).

**Failure classification.** A retired family (`invalid_grant`, `refresh_token_reused`,
`refresh_token_invalidated`, `refresh_token_expired`) fails the invocation as
`credential-unavailable` and logs `broker_chatgpt_credential_reauth_required`: an operator must log in
again. Everything else — transport, a 5xx, a malformed token response — fails as
`credential-refresh-failed`. Either way the broker keeps serving every other capability.

### `githubApp`

`githubApp` acts as a GitHub App installation rather than as a person. A personal access token stays
a `bearerToken`. The entry names the App, the installation and an absolute path to the RSA private
key GitHub generated for the App (the `.pem` download, PKCS#1 or PKCS#8; not an SSH key):

```yaml
apiVersion: dekopon.dev/broker-credentials/v1alpha1
credentials:
  - name: github-app
    kind: githubApp
    appId: 123456
    installationId: 7890123
    privateKey: /var/lib/dekopon/broker-github/app.pem
    destinations: [api.github.com]
    repositories: [dekopon]   # optional downscope
    permissions:              # optional downscope
      contents: read
      issues: write
```

`secret`, `scheme` and `authFile` are prohibited for this kind, and `appId`, `installationId`,
`privateKey`, `repositories` and `permissions` are prohibited for the others. The key file passes the
same Tier A check as the credentials file under a 16 KiB ceiling; it is read once at startup and
never rewritten, so its parent needs no write permission. A relative path, an untrusted file, or a
document that is not an RSA private key refuses startup naming the cause, and every entry problem is
reported in the one refusal.

**Renewal.** An installation token lasts one hour. On the first authorized invocation, and on any
invocation within five minutes of the cached token's `expires_at`, the broker signs an RS256 JWT
(`iss` = `appId`, `iat` 60 s in the past to absorb clock drift, `exp` nine minutes ahead) and posts it
to `https://api.github.com/app/installations/{installationId}/access_tokens`. One lock per entry
serializes resolution, so concurrent invocations mint once and share the cached token. The token is
presented as `authorization: Bearer <token>` on the entry's `destinations` only. As with
`chatgptSubscription`, the mint is the broker's own HTTPS call: it is not charged to `maxRequests`,
produces no evidence entry, and the JWT, key and token appear in no log, span, error or audit record.

**Downscoping.** With `repositories` (names) or `permissions` (a map of GitHub permission name to access
level, such as `read` or `write`) set, the mint sends them as its JSON body and GitHub issues a token limited to that
subset of the installation's grant. Without either, the request has no body and the token carries
the installation's full grant. An empty list or map refuses startup instead of silently meaning
"everything".

**Failure classification.** A 401 from the mint means GitHub no longer accepts the App's JWT (a
revoked or rotated key, a deleted App, or the broker's clock is more than a minute ahead of GitHub's): the invocation fails as `credential-unavailable` and logs
`broker_github_app_credential_reauth_required`; an operator must correct clock skew, restore a deleted App, or replace a revoked or wrong key. Everything
else — transport, a 5xx, another refusal such as a removed installation, a malformed response —
fails as `credential-refresh-failed` and logs `broker_github_app_credential_refresh_failed`; the
next invocation tries again.

## Resolution and rotation

Startup parses and validates the map, locators, scopes and bootstrap paths without contacting a
remote source. After dual authorization and emitting the decision audit record, the broker resolves exactly one
snapshot. There is no cross-invocation cache and no stale fallback:

- a floating alias or projected generation rotates on the next invocation;
- deletion/missing fields fail closed;
- one in-flight invocation retains its resolved snapshot;
- bootstrap token/session files are reread each invocation; when the chart copied one through
  `secretBootstrapFiles`, changing its source Kubernetes Secret requires a pod rollout to refresh
  the copied file;
- a remote source response containing its own bootstrap token/session secret is refused as
  `bootstrap-reflected`, so a compromised source cannot turn its read credential into application
  material;
- a resolved secret shorter than 16 bytes fails the invocation as `invalid-material` before the
  provider runs, on either native sink. The resolved value is what the host searches responses for,
  and a value that short would deny answers that never carried it;
- no adapter retries automatically.

A source timeout or malformed/oversized response produces a fixed broker failure. Response/error
bodies and private locators are not copied into public errors. Source kind and a low-cardinality
category are available only in broker logs.

## Evidence, audit and telemetry

The authorized proposal serialization commits to the public DRN and sink. The effective execution
constraints commit to the binding ID, owner `mapRevision`, and exact narrowed scope. Optional decision/execution audit
fields record the public DRN and sink; the legacy `credential` field currently reports only the
`credential`/`agents.<id>.credentials` path, which [will be replaced by public DRNs](design.md#legacy-credential-bindings).
This describes today's audit schema, not an already-shipped field migration. Raw value,
backend, locator, selector, source revision, path/query, headers and bodies are absent. A record
without those optional fields retains its serialized bytes and chain hashes.

Telemetry carries model-authored scripts and command arguments, and therefore public DRNs, in the
transcript and on `shell.command.arguments` and `command.arguments`. It cannot carry resolved bytes,
which never enter a value it reads.

## Current non-goals

The project-wide list is [`design.md`](design.md#non-goals). Local to this feature:

- arbitrary secret interpolation, headers, URL/query/body placement, environment variables, files,
  or a `resolve-secret -> bytes` interface; the `chatgptSubscription` companion header is one fixed
  name chosen by that credential kind, not an owner- or provider-authored header;
- a generalized refresh template or provider-declared refresh callback: only the owner-authored
  `oauth2Refresh` source may name a token endpoint, for the OAuth refresh-token grant alone;
- secret references in provider invoke input, or a new HTTP/provider WIT package; a command guest
  reads a reference from argv only to propose it;
- Vault dynamic leases and lifecycle;
- 1Password direct service-account SDK mode or file fields;
- AWS ambient credential/role chains, GCP ADC/WIF, Azure managed identity, kubeconfig exec plugins;
- custom CA bundles for sources other than `kubernetesTokenRequest`, mTLS, request signing as a provider sink;
- an in-memory cache or stale serving, automatic retries, or transformed-reflection prevention;
- claims that a ConfigMap is a secret store or that an allowed endpoint cannot exfiltrate what it
  legitimately receives.
