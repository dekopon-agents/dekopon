# Quickwit 0.9 development stack on kind

This manifest is a disposable observability backend for `dekopon-run`. It deploys:

- Quickwit `0.9.0` with its native OTLP/gRPC receiver on port `7281`;
- the built-in `otel-logs-v0_9` and `otel-traces-v0_9` indexes, created automatically when the OTLP service starts;
- a small PostgreSQL metastore; and
- node-local `emptyDir` volumes for Quickwit splits, indexing scratch data, and PostgreSQL metadata.

Everything is ephemeral. Deleting the namespace, replacing the pods, or deleting the kind cluster can delete data. PostgreSQL uses trust authentication only inside this isolated development fixture; do not copy that setting into a shared cluster.

## Start the stack

```console
kind create cluster --name dekopon-otel \
  --config deploy/quickwit-kind/kind.yaml
kubectl apply -f deploy/quickwit-kind/stack.yaml
kubectl -n dekopon-observability rollout status deployment/postgres
kubectl -n dekopon-observability rollout status deployment/quickwit --timeout=5m
```

Inspect the two indexes or open the Quickwit UI through a local port-forward:

```console
kubectl -n dekopon-observability port-forward service/quickwit 7280:7280
curl http://127.0.0.1:7280/api/v1/indexes
open http://127.0.0.1:7280/ui/
```

## Send `dekopon-run` telemetry

For a runner on the host, forward the OTLP/gRPC port:

```console
kubectl -n dekopon-observability port-forward service/quickwit 7281:7281

OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:7281 \
OTEL_SERVICE_NAME=dekopon-run-dev \
  cargo run -p dekopon-run -- prompt \
    --provider examples/providers/echo-provider.wasm \
    --model qwen3 \
    'Use the echo tool with the message hello'
```

A runner inside the namespace can use `http://quickwit:7281`. The equivalent CLI settings are:

```text
--otlp-endpoint http://quickwit:7281
--otel-service-name dekopon-run
--otel-logs-index otel-logs-v0_9
--otel-traces-index otel-traces-v0_9
```

The index flags become Quickwit's `qw-otel-logs-index` and `qw-otel-traces-index` gRPC metadata. `OTEL_RESOURCE_ATTRIBUTES` is also honored by the OpenTelemetry SDK.

## Run the end-to-end test

The repository test builds a non-root `dekopon-run` image, loads it into the existing cluster, runs a two-turn model/tool session against a local mock endpoint, and queries both Quickwit indexes:

```console
KIND_CLUSTER_NAME=dekopon-otel tests/otel-kind/e2e.sh
```

It checks the PostgreSQL/`emptyDir` topology, expected agent/model/provider spans, audit-safe lifecycle events, log-to-trace correlation, and absence of sentinel prompt/tool payloads. The same script runs in CI.

## Scope and cleanup

This stack receives telemetry emitted by `dekopon-run`; it does not tail or forward `dekopon-brokerd`, Kubernetes, or host process logs. OTLP lifecycle events are operational audit telemetry, not broker authorization evidence or the broker's durable hash-linked audit log.

```console
kind delete cluster --name dekopon-otel
```
