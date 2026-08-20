# Skylight private provider (unsupported Exploration)

> **Exploration only — opt-in, unofficial, private, and unsupported.** This proof of concept is not
> affiliated with, endorsed by, or supported by Skylight. Skylight publishes no public API for
> these routes; they can change without notice, and using them may violate applicable terms or
> trigger account enforcement. This is not a production integration.

`skylight-private` is a broker-only Rust component with exactly two Medium-risk, read-only,
idempotent capabilities:

| Capability | Fixed request | Projected output |
|---|---|---|
| `skylight.private.account.read` | `GET https://app.ourskylight.com/api/user` | `{"account":{"id":"…"}}` |
| `skylight.private.frames.list` | `GET https://app.ourskylight.com/api/frames` | At most 32 sorted frame IDs and optional 256-byte marked names |

Both inputs must be exactly `{}`. There is no account or frame selector, deleted-frame flag,
pagination, include, endpoint, URL, host, path, method, query, header, body, token, or generic JSON
escape hatch. Each invocation constructs one fixed HTTPS GET with constant `accept` and honest
Dekopon `user-agent` headers, sends it once, and never retries. The guest never sets
`authorization`, `cookie`, or `content-type`.

The private JSON:API response remains untrusted. Account output retains one non-empty ID of at most
128 UTF-8 bytes. Frame output retains unique bounded IDs plus only `attributes.name` (with
`attributes.label` fallback), UTF-8-truncates names with an ellipsis inside 256 bytes, sorts by ID,
and omits whole records to stay below the 32-record and 32-KiB serialized-output ceilings. Account
names, email, billing, subscriptions, activation codes, serials, media links, events, tasks,
arbitrary attributes, relationships, sessions, and credential fields are otherwise discarded.
Malformed known fields fail closed. HTTP 401 returns `reauth-required`; no refresh or retry follows
it.

## Required broker-only authority

A deployment must opt in by supplying **both** owner-authored constraint sets below. The duplicate
shape is deliberate: each capability is independently grantable, and neither a manifest import nor
this example grants authority. The native host follows no redirects, and the guest performs no
retry. Do not relax the host, method, request count, HTTPS-only posture, 10-second deadline, or byte
ceilings.

```yaml
constraintSets:
  skylight.private.account.read:
    provider: skylight-private
    effect: read-only
    risk: Medium
    idempotency: idempotent
    credential: skylight-poc-bearer
    constraints:
      timeoutMs: 10000
      maxOutputBytes: 32768
      http:
        allowedHosts: [app.ourskylight.com]
        allowedMethods: [GET]
        maxRequests: 1
        maxRequestBytes: 4096
        maxResponseBytes: 262144
        allowPlaintextLoopback: false
  skylight.private.frames.list:
    provider: skylight-private
    effect: read-only
    risk: Medium
    idempotency: idempotent
    credential: skylight-poc-bearer
    constraints:
      timeoutMs: 10000
      maxOutputBytes: 32768
      http:
        allowedHosts: [app.ourskylight.com]
        allowedMethods: [GET]
        maxRequests: 1
        maxRequestBytes: 4096
        maxResponseBytes: 262144
        allowPlaintextLoopback: false
```

The operator must obtain a disposable, short-lived access token out of band, only where authorized,
and install it solely in the existing owner-only broker credential store. Use a non-PII symbolic
name and bind it to exactly the production authority:

```yaml
# Illustrative shape only. Never commit the file or substitute a real token in source or fixtures.
apiVersion: dekopon.dev/broker-credentials/v1alpha1
credentials:
  - name: skylight-poc-bearer
    kind: bearerToken
    scheme: Bearer
    destinations: [app.ourskylight.com]
    secret: "<operator-supplied disposable access token>"
```

The native broker validates the guest request first, then injects `Authorization: Bearer …` where
the guest cannot observe it. The symbolic name is audit metadata; it must contain no email,
household name, account ID, or other PII. Credential values must never enter source, fixtures,
inputs, outputs, errors, logs, traces, evidence, audit, or names.

The current credential store is static and loaded at startup. Expiry or rotation can cause an
outage and may require operator replacement and broker restart. This component implements no OAuth,
login, PKCE, callback, MFA/CAPTCHA handling, refresh, token cache, revocation, expiry metadata, or
pre-expiry renewal. A separate follow-up would need a broker-owned expiring credential source with
owner-driven enrollment, protected refresh-token persistence, expiry skew, single-flight refresh,
atomic rotated-token replacement that preserves the previous refresh token when an omission requires
it, destination binding, revocation and reauthentication handling, and redaction tests. None belongs
in this guest.

One bearer may expose multiple accounts or frames. Dekopon policy cannot bind a private API path or
narrow the upstream token's resource scope, so the upstream token is the final resource boundary.
Projection is not declassification: the retained account ID, frame IDs, and frame names are still
sensitive household metadata exposed to the model or caller whenever either capability is granted.
No default catalog, image, policy, credential file, or deployment fixture includes this component.

## Provenance, tests, and regeneration

The route and response-shape evidence is pinned to
[`joshuaswarren/pyskylight`](https://github.com/joshuaswarren/pyskylight) commit
`69e4576b9035d71aacda9ade7a4afea05a663e94`. See the complete upstream MIT notice in
[`THIRD_PARTY_NOTICES.md`](THIRD_PARTY_NOTICES.md). Any future image, package, archive, or other
distribution of `skylight-private-provider.wasm` must carry that adjacent notice and this
experimental warning.

All behavior tests replace the send function directly with exact in-memory mocks. There is no
endpoint override, loopback fixture, live Skylight call, captured response, login, or OAuth request.
The broker-host integration exercises only a pre-network policy denial.

Regeneration requires exact Rust 1.89.0 and `wasm-tools` 1.236.1. The build remaps repository,
Cargo-home, and sysroot paths, normalizes checkout-derived local crate metadata without replacing a
configured compiler-cache wrapper, and rejects an artifact containing any original local root.
Pull-request CI runs the standalone checks and byte-compares a regeneration with the checked Wasm.

```console
cargo fmt --manifest-path examples/providers/skylight-private/Cargo.toml -- --check
cargo clippy --locked --manifest-path examples/providers/skylight-private/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path examples/providers/skylight-private/Cargo.toml
cargo check --locked --manifest-path examples/providers/skylight-private/Cargo.toml --target wasm32-unknown-unknown
examples/providers/skylight-private/build.sh
wasm-tools validate examples/providers/skylight-private-provider.wasm
wasm-tools component wit examples/providers/skylight-private-provider.wasm
```

The component must export only `describe` and `invoke`, import exactly
`dekopon:http/client@1.0.0`, and import no WASI. Direct `dekopon-run` rejects it because the
immediate linker remains empty.
