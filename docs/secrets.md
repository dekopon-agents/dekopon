# Public secret references and the private secret map

**Status: current on this development branch.** This document defines the broker-owned secret
system: public inert DRNs, a separate Cedar decision, an owner-only map to physical stores,
invocation-pinned resolution, and native HTTP Basic/Bearer sinks. Existing implicit
`credential`/`credentialByAgent` bindings remain supported unchanged.

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

The typed DRN/secret-use field is never copied into provider JSON or either provider WIT interface.
The Wasm component sees the same `{uri, method, headers, body}` input it saw before. A DRN is public
text, so a model can still quote those characters as ordinary provider data; doing so has no secret
semantics and grants no resolution. Resolved bytes are
passed only from the broker resolver to `dekopon-http-host`, beside an authorization committing to
the same DRN, sink, and binding identifier. `dekopon-broker-host` rejects a swapped credential.

This guarantees that Dekopon's model, gateway, protocol results, provider memory, evidence, audit,
and normal telemetry do not receive secret bytes. The authorized remote endpoint necessarily does.
The native host rejects a response containing the raw secret or complete rendered Authorization
value, but an endpoint can transform or semantically encode it; destination trust and narrow
upstream credentials remain part of the boundary.

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

Knowing a DRN grants nothing. It is safe to copy and remains inert after revocation. Names can still
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

`-U` is accepted as an alias for the second form because it was part of the original Dekopon DRN
proposal; `-u` and `--user` are the curl-compatible spellings.

The complete `${...}` value must be one canonical DRN. Literal passwords, prefixes/suffixes, `${drn:…}` markers in URLs, headers or bodies, and arbitrary
interpolation are rejected. Bare DRN characters elsewhere are ordinary public text and have no
resolution semantics. The marker is removed
before provider input is built. Immediate/direct invokers refuse secret use; only a broker-backed
leg forwards the typed top-level proposal. Every broker-backed session reaches it — `dekopon-run
prompt --broker` and a `dekopond` chat session — because invocation is one
method, so a wrapper that records a call or stops one at a cancellation boundary cannot drop the
proposal on the way through.

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
inventory and never appear in prompts, audit, evidence, provider metadata, or the web UI.
Bootstrap credentials are never DRN-addressable, preventing resolver cycles and use of a source-store token as application material. The map file itself is likewise prohibited as a `secureFile` source. In the Helm chart,
`broker.secretBootstrapFiles` copies operator-managed Secret keys into broker-only `0600` files;
`broker.secretSourceVolumes` mounts AtomicWriter sources read-only into the broker only. Expiring AWS
sessions and GCP/Azure/Kubernetes access tokens must be refreshed out of band; a chart-copied file
changes only after a pod rollout, and no ambient workload-identity refresh chain is claimed.

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

The grammar deliberately excludes percent encoding, backslashes, controls, whitespace, repeated
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
redaction but do not gain Kubernetes Secret storage/RBAC properties retroactively. The existing chart's init-copy path remains
valid and can be consumed through `secureFile`.

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
or stale cache is used. A loopback `endpoint` override exists for deterministic tests; production
defaults to the regional AWS endpoint.

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

## Resolution and rotation

Startup parses and validates the map, locators, scopes and bootstrap paths without contacting a
remote source. After dual authorization and durable decision audit, the broker resolves exactly one
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
- no adapter retries automatically.

A source timeout or malformed/oversized response produces a fixed broker failure. Response/error
bodies and private locators are not copied into public errors. Source kind and a low-cardinality
category are available only in broker logs.

## Evidence, audit and telemetry

The authorized proposal serialization commits to the public DRN and sink. The effective execution
constraints commit to the binding ID, owner `mapRevision`, and exact narrowed scope. Optional decision/execution audit
fields record the public DRN and sink; the legacy `credential` field remains legacy-only. Raw value,
backend, locator, selector, source revision, path/query, headers and bodies remain absent. Legacy
records omit the new optional fields and retain their serialized bytes and chain hashes.

Payload telemetry can expose model-authored scripts, and therefore public DRNs, when explicitly
enabled. It still cannot expose resolved bytes through a secret resolver or provider input because
those bytes never enter either value.

## Current non-goals

- arbitrary secret interpolation, headers, URL/query/body placement, environment variables, files,
  or a `resolve-secret -> bytes` interface;
- provider-visible secret references or a new HTTP/provider WIT package;
- Vault dynamic leases and lifecycle;
- 1Password direct service-account SDK mode or file fields;
- AWS ambient credential/role chains, GCP ADC/WIF, Azure managed identity, kubeconfig exec plugins;
- custom secret-source CA bundles, mTLS, request signing as a provider sink;
- cache/stale serving, automatic retries, or transformed-reflection prevention;
- claims that a ConfigMap is a secret store or that an allowed endpoint cannot exfiltrate what it
  legitimately receives.
