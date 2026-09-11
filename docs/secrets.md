# Public secret references and the private secret map

**Status: current.** This document defines the broker-owned secret system: public inert DRNs, a
separate Cedar decision, an owner-only map to physical stores, invocation-pinned resolution, and
native HTTP Basic/Bearer sinks. Implicit `credential`/`credentialByAgent` bindings run beside it,
under [`security-model.md`](security-model.md#per-agent-credentials-and-where-their-boundary-stops).
*Committed direction:* `credential`/`credentialByAgent` bindings will be replaced by public DRNs;
current configurations remain supported until that migration ships
([migration requirements](design.md#legacy-credential-bindings)).

## The guarantee

A model may name a secret; it never receives one.

```text
model-authored script
  -> typed SecretUseProposal carrying a public DRN
  -> ordinary capability Cedar decision
  -> separate secret.use Cedar decision over the exact DRN
  -> owner-authored SecretUseBinding
  -> one private-source lookup, after authorization
  -> authorization-bound native Basic/Bearer rendering
  -> constrained HTTP request
```

The typed DRN/secret-use field is never copied into provider JSON or either provider WIT interface;
the Wasm component sees an ordinary `{uri, method, headers, body}` input. A DRN is public text, so a
model may quote those characters as ordinary provider data; doing so has no secret semantics and
grants no resolution. Resolved bytes pass only from the broker resolver to `dekopon-http-host`,
beside an authorization committing to the same DRN, sink, and binding identifier.
`dekopon-broker-host` rejects a swapped credential.

Dekopon's model, gateway, protocol results, provider memory, evidence, audit, and telemetry
therefore never receive secret bytes. The authorized remote endpoint necessarily does. The native
host rejects a response containing the raw secret or complete rendered Authorization value, but an
endpoint can transform or semantically encode it; destination trust and narrow upstream credentials
remain part of the boundary, and [`design.md`](design.md#non-goals) rules out defending that case.

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

The sandboxed `curl` builtin recognizes only two exact credential forms:

```sh
curl --oauth2-bearer '${drn:com.xrl:secret:prod:api/token}' \
  https://api.example.com/v1/thing

curl -u 'userA:${drn:com.xrl:secret:prod:api/password}' \
  https://api.example.com/v1/thing
```

`-U` is an accepted alias for the second form; `-u` and `--user` are the curl-compatible spellings.

The complete `${...}` value must be one canonical DRN. Literal passwords, prefixes/suffixes,
`${drn:…}` markers in URLs, headers or bodies, and arbitrary interpolation are rejected. Bare DRN
characters elsewhere are ordinary public text with no resolution semantics, and the marker is
removed before provider input is built. Immediate/direct invokers refuse secret use; only a
broker-backed leg forwards the typed top-level proposal. Invocation is one method, so every
broker-backed session reaches it, a `dekopond` chat session included, and a wrapper that records a
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
`broker.secretSourceVolumes` mounts AtomicWriter sources read-only into the broker only. Expiring AWS
sessions and GCP/Azure/Kubernetes access tokens must be refreshed out of band for *this* system: no
adapter here renews anything, a chart-copied file changes only after a pod rollout, and no ambient
workload-identity refresh chain is claimed. The one credential the broker does renew itself is the
legacy `chatgptSubscription` kind below, which is a different mechanism entirely — a named credential
file rather than a DRN, and no private-map source. That legacy binding
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
URL or a literal loopback HTTP URL; `secureFile` and `kubernetesProjection` take neither field.

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
  declaredOrigin: secret # secret | configMap
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

The on-disk layout cannot prove whether kubelet sourced a Secret or ConfigMap, so every projection
entry must explicitly state `declaredOrigin`. A ConfigMap declaration requires `acknowledgeNonSecretSource: true`; this is
an explicit operator claim rather than filesystem attestation. Values receive Dekopon's downstream
redaction but do not gain Kubernetes Secret storage/RBAC properties retroactively.

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

*Committed direction:* these entries are selected through `credential`/`credentialByAgent`, which
will be replaced by public DRNs. The migration must retain the shared refresh sequence, destination
binding, and companion header described here; it is not implemented by the current private map
([migration requirements](design.md#legacy-credential-bindings)).

The two legacy kinds in `broker-credentials.yaml` differ in *when the value exists*, not in how it is
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

Both legacy kinds are direct-reflection checked the way a DRN-bound credential is: the native host
refuses a response whose body or headers carry the secret rather than returning it to the component.
A `bearerToken` `secret` is therefore held to a shape the check can search for — at least 16 bytes of
printable ASCII, no whitespace or control bytes — and an entry that breaks that rule refuses startup
by name. The same 16-byte floor holds for a DRN-resolved secret, which has no startup to refuse at
and fails its invocation instead.

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

**One file per holder.** Give the broker its own `dekopond auth chatgpt login --auth-file <path>`.
Pointing it at a `chatgptSubscription` *model*'s file would have two independent holders spending one
rotating refresh token, and the authorization server retires a predecessor on every rotation, so the
family is eventually revoked for both. See
[`chatgpt-credential.md`](chatgpt-credential.md#a-second-family-for-the-broker).

**Failure classification.** A retired family (`invalid_grant`, `refresh_token_reused`,
`refresh_token_invalidated`, `refresh_token_expired`) fails the invocation as
`credential-unavailable` and logs `broker_chatgpt_credential_reauth_required`: an operator must log in
again. Everything else — transport, a 5xx, a malformed token response — fails as
`credential-refresh-failed`. Either way the broker keeps serving every other capability.

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
  and a needle that short would deny answers that never carried it;
- no adapter retries automatically.

A source timeout or malformed/oversized response produces a fixed broker failure. Response/error
bodies and private locators are not copied into public errors. Source kind and a low-cardinality
category are available only in broker logs.

## Evidence, audit and telemetry

The authorized proposal serialization commits to the public DRN and sink. The effective execution
constraints commit to the binding ID, owner `mapRevision`, and exact narrowed scope. Optional decision/execution audit
fields record the public DRN and sink; the legacy `credential` field currently reports only the
`credential`/`credentialByAgent` path, which [will be replaced by public DRNs](design.md#legacy-credential-bindings).
This describes today's audit schema, not an already-shipped field migration. Raw value,
backend, locator, selector, source revision, path/query, headers and bodies are absent. A record
without those optional fields retains its serialized bytes and chain hashes.

Telemetry carries model-authored scripts and therefore public DRNs. It cannot carry resolved bytes,
which never enter a value it reads.

## Current non-goals

The project-wide list is [`design.md`](design.md#non-goals). Local to this feature:

- arbitrary secret interpolation, headers, URL/query/body placement, environment variables, files,
  or a `resolve-secret -> bytes` interface; the `chatgptSubscription` companion header is one fixed
  name chosen by that credential kind, not an owner- or provider-authored header;
- a generalized `oauth2RefreshToken { tokenEndpoint, clientId }` kind, or any provider-declared
  refresh callback: only owner-authored broker configuration may name a token endpoint, and one
  consumer does not justify the template machinery;
- provider-visible secret references or a new HTTP/provider WIT package;
- Vault dynamic leases and lifecycle;
- 1Password direct service-account SDK mode or file fields;
- AWS ambient credential/role chains, GCP ADC/WIF, Azure managed identity, kubeconfig exec plugins;
- custom secret-source CA bundles, mTLS, request signing as a provider sink;
- cache/stale serving, automatic retries, or transformed-reflection prevention;
- claims that a ConfigMap is a secret store or that an allowed endpoint cannot exfiltrate what it
  legitimately receives.
