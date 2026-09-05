# View Dekopon traces in OpenObserve

This example starts one local [OpenObserve](https://openobserve.ai/) container with one Docker volume, sends `dekopon-run` traces and audit-safe lifecycle logs over OTLP/HTTP, and shows how to inspect them. It is a development fixture, not a production deployment.

## Prerequisites

- Docker with Compose
- Rust 1.89 or newer
- `curl`, `base64`, and `jq`

The example pins the multi-architecture OpenObserve `v0.92.0` image by digest, and pulls it from
upstream so that running this needs nothing but a container runtime.

`OPENOBSERVE_IMAGE` overrides where that image comes from. CI sets it to a digest-identical copy in
the project's own registry, because an anonymous pull from a public registry is rate-limited across
every runner leaving the same address — a red build that says nothing about the change under test.
[`.github/workflows/mirror-image.yml`](../../.github/workflows/mirror-image.yml) makes those copies
and refuses any source that is not digest-pinned.

## 1. Start OpenObserve

From the repository root, choose local-only credentials and start the container:

```console
export OPENOBSERVE_ROOT_EMAIL=root@example.com
export OPENOBSERVE_ROOT_PASSWORD='replace-this-local-password'
docker compose -f examples/otel-traces/compose.yaml up -d

until curl -fsS http://127.0.0.1:5080/healthz >/dev/null; do sleep 1; done
```

Open <http://127.0.0.1:5080/> and sign in with those credentials. OpenObserve stores its local data in the Compose-managed `openobserve-data` volume.

## 2. Configure Dekopon

OpenObserve reads the organization from the OTLP endpoint path over HTTP — over gRPC it must instead arrive as an `organization` header — and authentication plus the target stream from headers. `dekopon-run` treats `OTEL_EXPORTER_OTLP_ENDPOINT` as a generic OTLP/HTTP base and appends `/v1/traces` and `/v1/logs` itself.

For this disposable local account, construct the Basic token from the root credentials:

```console
auth_token="$(printf '%s:%s' \
  "$OPENOBSERVE_ROOT_EMAIL" "$OPENOBSERVE_ROOT_PASSWORD" \
  | base64 | tr -d '\r\n')"

export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:5080/api/default
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Basic%20${auth_token},organization=default,stream-name=dekopon"
export OTEL_SERVICE_NAME=dekopon-run-local
```

`OTEL_EXPORTER_OTLP_HEADERS` follows the OpenTelemetry header format and URL-decodes values, so the space after `Basic` is written as `%20`. The credential is read directly by the OTLP exporter; it is not accepted as a CLI argument or attached to telemetry.

For a shared OpenObserve installation, create a dedicated organization ingestion token in OpenObserve and copy the generated `Authorization=Basic%20...` value instead of reusing the root password. Use HTTPS whenever the exporter crosses a machine or untrusted network; plain HTTP exposes both the token and telemetry payload.

## 3. Generate a trace

Fetch and run the exact standalone echo v0.1.0 fixture:

```console
ci/fetch-external-provider-components.sh examples/providers echo
cargo run --locked -p dekopon-run -- \
  invoke \
  --provider examples/providers/echo-provider.wasm \
  echo.echo --input '{"message":"observed"}'
```

The short-lived runner flushes traces and logs before exiting. A configured exporter that cannot deliver telemetry makes the command fail rather than silently reporting a fully observed run.

## 4. View traces

In OpenObserve:

1. Select the `default` organization.
2. Open **Traces**.
3. Select the `dekopon` stream.
4. Filter on `service_name = 'dekopon-run-local'`.
5. Open a result to inspect the `runner.command`, `runner.invoke`, `provider.compile`, and `provider.invoke` span hierarchy.

OpenObserve keeps logs and traces as separate stream types even when both use the `dekopon` stream name. Lifecycle logs carry the active trace and span identifiers, so they can be correlated with the performance trace.

## Automated smoke test

The same single-container path used by CI is available locally:

```console
examples/otel-traces/smoke-test.sh
```

The script creates an isolated Compose project, builds `dekopon-run`, sends one invocation, polls OpenObserve's trace search API, and asserts that the expected runner and provider spans arrived. It also checks that a sentinel provider input did not appear in the exported trace. The container and volume are removed when the test exits; set `DEKOPON_OTEL_KEEP=1` to retain them for inspection. `OPENOBSERVE_PORT` (default `5080`; CI uses `15080`) and `OPENOBSERVE_STREAM` (default `dekopon_smoke`) pick the host port and the stream, and `DEKOPON_OTEL_SKIP_BUILD=1` reuses an already-built `target/debug/dekopon-run` instead of running `cargo build`.

## Read sessions back

`dekopon-run session` reads the log stream this receiver holds back out again: `list` finds the sessions a deployment ran, `show` reconstructs one transcript, and `replay` puts it to a model again with its scripts answered from the recording. It searches the same organization base the exporter posts to, over `/_search?type=logs`, with the credential read from an environment variable named on the command line, so a value never appears in an argument.

With the exports from step 2 in place, derive both settings from them. The header value is sent verbatim, so the space after `Basic` is a space here rather than the `%20` the exporter's header list needs:

```console
export DEKOPON_OPENOBSERVE_URL="$OTEL_EXPORTER_OTLP_ENDPOINT"
export DEKOPON_OPENOBSERVE_AUTHORIZATION="Basic ${auth_token}"
```

The default stream, `dekopon`, is the `stream-name` step 2 sets; `--openobserve-stream` (or `DEKOPON_OPENOBSERVE_STREAM`) names another, and `--openobserve-auth-env` reads a differently named variable.

Sessions are listed from `accounting.model.call` records, which only a model loop writes: the `invoke` in step 3 leaves spans and lifecycle logs but no session. `show` and `replay` also need the transcript, which reaches the receiver only when the session ran with payload telemetry on — `--otel-telemetry-payloads true`, or `DEKOPON_OTEL_TELEMETRY_PAYLOADS` — so record one that way:

```console
cargo run --locked -p dekopon-run -- \
  --otel-telemetry-payloads true \
  prompt \
  --provider examples/providers/echo-provider.wasm \
  --model "$MODEL" \
  'Echo the word observed.'
```

Payload telemetry sends the prompt, every script with its output, and the answer to this receiver, which is why the flag is off by default and why the HTTPS advice in step 2 applies. Then:

```console
cargo run --locked -p dekopon-run -- session list --since 1h
cargo run --locked -p dekopon-run -- session show --trace-id 4bf92f3577b34da6a3ce929d0e0e4736
cargo run --locked -p dekopon-run -- session show --trace-id 4bf92f3577b34da6a3ce929d0e0e4736 --json > session.json
cargo run --locked -p dekopon-run -- session replay --from-file session.json --model "$MODEL"
```

`list` prints one row per trace in the window, newest first — `--trace-id` takes the value under `TRACE` — and includes sessions recorded metadata-only; `show` or `replay` of one of those fails with `trace <ID> has <N> accounted model turn(s) but no transcript`. `replay --from-file` needs no receiver, which is how a recording outlives `docker compose … down --volumes`. The flags are documented in [`../../docs/run.md`](../../docs/run.md) and the loop they close in [`../../docs/improvement.md`](../../docs/improvement.md).

## Stop the development instance

```console
docker compose -f examples/otel-traces/compose.yaml down
```

Add `--volumes` to delete the local OpenObserve data as well:

```console
docker compose -f examples/otel-traces/compose.yaml down --volumes
```

See [`../../docs/observability.md`](../../docs/observability.md) for the complete signal model and data-minimization contract.
