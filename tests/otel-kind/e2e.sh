#!/usr/bin/env bash
set -Eeuo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cluster_name="${KIND_CLUSTER_NAME:-dekopon-otel}"
namespace="dekopon-observability"
image="dekopon-run-otel:e2e"
local_port="${QUICKWIT_PORT_FORWARD:-17280}"
temporary="$(mktemp -d)"
port_forward_pid=""

for command in curl docker jq kind kubectl; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "missing required command: $command" >&2
    exit 1
  }
done

cleanup() {
  status=$?
  set +e
  if [ -n "$port_forward_pid" ]; then
    kill "$port_forward_pid" >/dev/null 2>&1 || true
    wait "$port_forward_pid" >/dev/null 2>&1 || true
  fi
  if [ "$status" -ne 0 ]; then
    echo "--- Kubernetes diagnostics ---" >&2
    kubectl -n "$namespace" get all -o wide >&2 || true
    kubectl -n "$namespace" describe pods >&2 || true
    kubectl -n "$namespace" logs deployment/postgres --all-containers --tail=200 >&2 || true
    kubectl -n "$namespace" logs deployment/quickwit --all-containers --tail=300 >&2 || true
    kubectl -n "$namespace" logs deployment/mock-openai --all-containers --tail=100 >&2 || true
    kubectl -n "$namespace" logs job/dekopon-run-otel-e2e --all-containers >&2 || true
  fi
  rm -rf "$temporary"
  exit "$status"
}
trap cleanup EXIT

if ! kind get clusters | grep -Fxq "$cluster_name"; then
  echo "kind cluster $cluster_name does not exist" >&2
  echo "create it with: kind create cluster --name $cluster_name --config deploy/quickwit-kind/kind.yaml" >&2
  exit 1
fi

if [ "${DEKOPON_E2E_SKIP_BUILD:-0}" != "1" ]; then
  docker build \
    --file "$root/tests/otel-kind/Dockerfile" \
    --tag "$image" \
    "$root"
fi
kind load docker-image --name "$cluster_name" "$image"

quickwit_config_existed=0
if kubectl -n "$namespace" get configmap quickwit-config >/dev/null 2>&1; then
  quickwit_config_existed=1
fi
kubectl apply -f "$root/deploy/quickwit-kind/stack.yaml"
# The Quickwit config is mounted with subPath, so force a new pod only when an existing test
# cluster may have received a changed ConfigMap. A fresh deployment must not start two nodes with
# the same node ID concurrently.
if [ "$quickwit_config_existed" -eq 1 ]; then
  kubectl -n "$namespace" rollout restart deployment/quickwit
fi
kubectl -n "$namespace" rollout status deployment/postgres --timeout=180s
kubectl -n "$namespace" rollout status deployment/quickwit --timeout=300s

# Assert that this fixture really exercises PostgreSQL metadata and node-local ephemeral splits.
kubectl -n "$namespace" get configmap quickwit-config -o jsonpath='{.data.quickwit\.yaml}' \
  | grep -Fq 'metastore_uri: postgres://quickwit@postgres:5432/quickwit'
kubectl -n "$namespace" get configmap quickwit-config -o jsonpath='{.data.quickwit\.yaml}' \
  | grep -Fq 'default_index_root_uri: file:///quickwit/splits'
kubectl -n "$namespace" get deployment quickwit -o json \
  | jq -e 'any(.spec.template.spec.volumes[]; .name == "splits" and has("emptyDir"))' >/dev/null

kubectl apply -f "$root/tests/otel-kind/mock-model.yaml"
kubectl -n "$namespace" rollout status deployment/mock-openai --timeout=180s
kubectl -n "$namespace" delete job dekopon-run-otel-e2e --ignore-not-found --wait=true
run_started_at="$(date +%s)"
kubectl apply -f "$root/tests/otel-kind/runner-job.yaml"

if ! kubectl -n "$namespace" wait \
  --for=condition=complete \
  job/dekopon-run-otel-e2e \
  --timeout=240s; then
  kubectl -n "$namespace" wait \
    --for=condition=failed \
    job/dekopon-run-otel-e2e \
    --timeout=1s >/dev/null 2>&1 || true
  exit 1
fi
runner_output="$(kubectl -n "$namespace" logs job/dekopon-run-otel-e2e)"
grep -Fq 'Dekopon OTLP end-to-end run completed.' <<<"$runner_output"

kubectl -n "$namespace" port-forward service/quickwit "$local_port:7280" \
  >"$temporary/port-forward.log" 2>&1 &
port_forward_pid=$!
for _ in $(seq 1 60); do
  if curl --fail --silent --show-error "http://127.0.0.1:$local_port/health/readyz" \
    > /dev/null 2>&1; then
    break
  fi
  sleep 1
done
curl --fail --silent --show-error "http://127.0.0.1:$local_port/health/readyz" >/dev/null

search_index() {
  index=$1
  output=$2
  curl --fail --silent --show-error --get \
    "http://127.0.0.1:$local_port/api/v1/$index/search" \
    --data-urlencode 'query=service_name:dekopon-run-e2e' \
    --data-urlencode "start_timestamp=$run_started_at" \
    --data-urlencode 'max_hits=200' \
    --output "$output"
}

# Quickwit 0.9 commits the built-in OTEL indexes asynchronously. Search until both signals are
# visible rather than sleeping for a fixed interval.
observed=0
for _ in $(seq 1 90); do
  if search_index otel-logs-v0_9 "$temporary/logs.json" \
    && search_index otel-traces-v0_9 "$temporary/traces.json" \
    && jq -e '.num_hits > 0' "$temporary/logs.json" >/dev/null \
    && jq -e '.num_hits > 0' "$temporary/traces.json" >/dev/null; then
    observed=1
    break
  fi
  sleep 2
done
if [ "$observed" -ne 1 ]; then
  echo "timed out waiting for OTLP logs and traces in Quickwit" >&2
  exit 1
fi

for span_name in \
  runner.command \
  runner.prompt \
  prompt.session \
  prompt.model_turn \
  model.complete \
  prompt.tool_call \
  provider.invoke
do
  jq -e --arg span_name "$span_name" \
    'any(.hits[]; .span_name == $span_name)' \
    "$temporary/traces.json" >/dev/null
 done

for audit_event in \
  runner.command.started \
  agent.session.started \
  agent.model.completed \
  agent.tool.invocation.started \
  agent.tool.invocation.completed \
  agent.session.completed \
  runner.command.completed
do
  jq -e --arg audit_event "$audit_event" \
    'any(.hits[]; .attributes["audit.event"] == $audit_event)' \
    "$temporary/logs.json" >/dev/null
 done

# At least one audit log must carry the same generated trace ID as its performance spans.
log_trace_id="$(
  jq -r '.hits[] | select((.trace_id // "") != "") | .trace_id' "$temporary/logs.json" \
    | head -n 1
)"
test -n "$log_trace_id"
jq -e --arg trace_id "$log_trace_id" \
  'any(.hits[]; .trace_id == $trace_id)' \
  "$temporary/traces.json" >/dev/null

# Prompts, model tool arguments, and provider input/output are deliberately absent from both
# telemetry signals. These sentinels are present in the actual e2e interaction.
for secret in \
  DEKOPON_E2E_PROMPT_SECRET_DO_NOT_EXPORT \
  DEKOPON_E2E_TOOL_SECRET_DO_NOT_EXPORT
do
  if grep -Fq "$secret" "$temporary/logs.json" "$temporary/traces.json"; then
    echo "sensitive prompt or tool payload leaked into telemetry: $secret" >&2
    exit 1
  fi
 done

printf 'Quickwit OTLP e2e passed: %s logs, %s spans\n' \
  "$(jq -r '.num_hits' "$temporary/logs.json")" \
  "$(jq -r '.num_hits' "$temporary/traces.json")"
