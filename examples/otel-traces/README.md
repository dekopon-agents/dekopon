# Verify daemon telemetry in OpenObserve

This development fixture runs real `dekopon-brokerd` and `dekopond` processes against
one pinned [OpenObserve](https://openobserve.ai/) container. Only the OpenAI-compatible
model endpoint is a Python standard-library loopback stub. No real model or provider
credential is read. The gateway receives an explicit fake model credential in its
isolated environment; provider execution requires the fixture's broker policy.

## Run

Prerequisites: Docker Compose, `rustup` (the repository's `rust-toolchain.toml` selects the compiler), Python 3, `curl`, `base64`, and `jq`.
From the repository root:

```console
ci/fetch-external-provider-components.sh examples/providers echo
python3 examples/otel-traces/test-smoke.py
examples/otel-traces/smoke-test.sh
```

The script builds both daemons, creates private temporary configuration and a `0600`
local chat socket, sends one request, receives one JSON response line, and verifies two
model calls and the successful authorized echo's `broker.execution` audit record on the broker's
stdout. The model first proposes a bash tool call and then answers from its actual provider result.

Both daemons export traces over OTLP/HTTP and deliver logs as structured stdout. A
smoke-only Python shipper submits all captured JSON stdout records to OpenObserve's
JSON ingestion API and checks its per-record success/failure counts. Native
`trace_id` and `span_id` come from the active valid OpenTelemetry context in the
shared formatter; records outside valid context have neither ID. The fixture enables only `wasmtime::runtime::code_memory` debug
logging to obtain existing broker compilation records without compiler debug noise.

The bounded queries require `transport.receive`, `gateway.message`, `gateway.session`,
`broker.invocation`, `provider.compile`, and `provider.invoke`. Receipt/gateway/session/invocation/provider
invocation share a trace; startup compilation legitimately has its own. Each daemon's independently
retrieved native log pair must match an actual exported span, not a query-manufactured
ID. Queries fail on partial or saturated results, and remote log counts must equal
shipped counts. The payload sentinel must *appear* in both complete remote signal
responses — payloads are exported and a trace that lost them is a failed run — while the
fake-credential sentinel is rejected across local stdout, stderr, shipped records, and
both responses. Failure controls exercise missing/wrong correlation, ingestion rejection,
missing IDs, query truncation, connection failure, and redaction failures using the
actual smoke assertion blocks.

## Bounds and cleanup

`OPENOBSERVE_PORT` defaults to `5080` (CI uses `15080`), `OPENOBSERVE_STREAM` to
`dekopon_smoke`, and `OTEL_SERVICE_NAME` to `dekopon-daemons-smoke`.
`COMPOSE_PROJECT_NAME` can select an isolated project; pre-existing project resources
or an occupied loopback port are refused, never adopted. `DEKOPON_OTEL_SKIP_BUILD=1`
reuses both already-built `target/debug` daemon binaries.

The script bounds process lifetimes, socket/model reads, query attempts, response bytes,
and record counts. It always removes its owned processes, container, volume, and temporary
configuration, including on failure. It prints ingestion and final search responses for
bounded diagnostic evidence; failed runs also print bounded daemon/receiver diagnostics.
There is no keep-resources mode. The receiver uses disposable local-only credentials;
never supply production credentials to this fixture.

The Compose image pins multi-architecture OpenObserve `v0.92.0` by digest.
`OPENOBSERVE_IMAGE` can select a digest-identical registry mirror; CI uses the project's
mirror to avoid anonymous upstream rate limits. The
[mirror workflow](../../.github/workflows/mirror-image.yml) refuses unpinned sources.

See [observability](../../docs/observability.md) for production configuration and signal
redaction contracts. In OpenObserve, select organization `default`, the configured stream
under **Traces** or **Logs**, and filter traces by `service_name` to inspect the five span
families. Production ingestion across a machine boundary requires HTTPS and a dedicated
ingestion token; the loopback fixture is not a production deployment.
