#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
example_dir="$root/examples/otel-traces"
port="${OPENOBSERVE_PORT:-5080}"
project="${COMPOSE_PROJECT_NAME:-dekopon-otel-smoke-$$}"
stream="${OPENOBSERVE_STREAM:-dekopon_smoke}"
service_name="${OTEL_SERVICE_NAME:-dekopon-run-smoke}"
temporary="$(mktemp -d)"

export OPENOBSERVE_PORT="$port"
export OPENOBSERVE_ROOT_EMAIL="${OPENOBSERVE_ROOT_EMAIL:-root@example.com}"
export OPENOBSERVE_ROOT_PASSWORD="${OPENOBSERVE_ROOT_PASSWORD:-DekoponSmoke#123}"

compose=(docker compose --project-name "$project" --file "$example_dir/compose.yaml")

for command in base64 cargo curl docker jq; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "missing required command: $command" >&2
    exit 1
  }
done

if ! [[ "$port" =~ ^[0-9]+$ ]] || ((port < 1 || port > 65535)); then
  echo "OPENOBSERVE_PORT must be a valid TCP port" >&2
  exit 1
fi
if ! [[ "$stream" =~ ^[a-zA-Z0-9_]+$ ]]; then
  echo "OPENOBSERVE_STREAM must contain only letters, digits, and underscores" >&2
  exit 1
fi

cleanup() {
  status=$?
  set +e
  if [ "$status" -ne 0 ]; then
    echo "--- OpenObserve diagnostics ---" >&2
    "${compose[@]}" ps >&2 || true
    "${compose[@]}" logs --no-color --tail=300 openobserve >&2 || true
    if [ -s "$temporary/search.json" ]; then
      echo "--- last trace search response ---" >&2
      jq . "$temporary/search.json" >&2 || cat "$temporary/search.json" >&2
    fi
  fi
  if [ "${DEKOPON_OTEL_KEEP:-0}" != "1" ]; then
    "${compose[@]}" down --volumes --remove-orphans >/dev/null 2>&1 || true
  else
    echo "OpenObserve left running as Compose project $project" >&2
  fi
  rm -rf "$temporary"
  exit "$status"
}
trap cleanup EXIT

"${compose[@]}" up --detach

for _ in $(seq 1 120); do
  if curl --fail --silent --show-error \
    "http://127.0.0.1:$port/healthz" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
curl --fail --silent --show-error "http://127.0.0.1:$port/healthz" >/dev/null

if [ "${DEKOPON_OTEL_SKIP_BUILD:-0}" != "1" ]; then
  cargo build --locked --manifest-path "$root/Cargo.toml" -p dekopon-run
fi

runner="$root/target/debug/dekopon-run"
provider="$root/examples/providers/echo-provider.wasm"
test -x "$runner" || {
  echo "dekopon-run is missing; rerun without DEKOPON_OTEL_SKIP_BUILD=1" >&2
  exit 1
}
test -f "$provider" || {
  echo "echo fixture is missing; run ci/fetch-external-provider-components.sh examples/providers echo" >&2
  exit 1
}

auth_token="$({ printf '%s:%s' "$OPENOBSERVE_ROOT_EMAIL" "$OPENOBSERVE_ROOT_PASSWORD"; } | base64 | tr -d '\r\n')"
printf 'Authorization: Basic %s\n' "$auth_token" >"$temporary/openobserve-auth-header"
run_started_us=$(( $(date +%s) * 1000000 - 60000000 ))
sentinel="DEKOPON_OTEL_SMOKE_INPUT_MUST_NOT_APPEAR"

OTEL_EXPORTER_OTLP_ENDPOINT="http://127.0.0.1:$port/api/default" \
OTEL_EXPORTER_OTLP_HEADERS="Authorization=Basic%20${auth_token},organization=default,stream-name=${stream}" \
OTEL_SERVICE_NAME="$service_name" \
OTEL_RESOURCE_ATTRIBUTES="deployment.environment.name=smoke,e2e.test.id=openobserve" \
  "$runner" \
    --no-color \
    --otel-export-timeout-ms 15000 \
    invoke \
    --provider "$provider" \
    echo.echo --input "{\"message\":\"$sentinel\"}" \
    >"$temporary/runner.json"

search_sql="SELECT * FROM \"$stream\""
observed=0
for _ in $(seq 1 60); do
  end_time_us=$(( $(date +%s) * 1000000 + 60000000 ))
  jq -nc \
    --arg sql "$search_sql" \
    --argjson start_time "$run_started_us" \
    --argjson end_time "$end_time_us" \
    '{query:{sql:$sql,start_time:$start_time,end_time:$end_time,from:0,size:200}}' \
    >"$temporary/search-request.json"

  if curl --fail --silent --show-error \
    --header @"$temporary/openobserve-auth-header" \
    --header 'Content-Type: application/json' \
    --data-binary @"$temporary/search-request.json" \
    "http://127.0.0.1:$port/api/default/_search?type=traces" \
    >"$temporary/search.json" 2>/dev/null \
    && jq -e --arg service "$service_name" \
      'any(.hits[]?; .service_name == $service and .operation_name == "runner.command")' \
      "$temporary/search.json" >/dev/null; then
    observed=1
    break
  fi
  sleep 2
done

if [ "$observed" -ne 1 ]; then
  echo "timed out waiting for Dekopon traces in OpenObserve" >&2
  exit 1
fi

for operation_name in runner.command runner.invoke provider.compile provider.invoke; do
  jq -e --arg service "$service_name" --arg operation_name "$operation_name" \
    'any(.hits[]?; .service_name == $service and .operation_name == $operation_name)' \
    "$temporary/search.json" >/dev/null || {
      echo "missing expected trace span: $operation_name" >&2
      exit 1
    }
done

if grep -Fq "$sentinel" "$temporary/search.json"; then
  echo "provider input leaked into exported traces" >&2
  exit 1
fi

trace_id="$(jq -r --arg service "$service_name" \
  '.hits[] | select(.service_name == $service and .operation_name == "runner.command") | .trace_id' \
  "$temporary/search.json" | head -n 1)"
test -n "$trace_id"

# Logs are the other half of the exported contract, and the half nothing else covers. Delivery
# alone is already implied by the runner exiting 0 (a logs flush failure is fatal), so what this
# asserts is correlation: a lifecycle record must carry the same trace_id its spans did, which is
# the whole basis for pivoting from a log result to its trace.
logs_observed=0
for _ in $(seq 1 60); do
  end_time_us=$(( $(date +%s) * 1000000 + 60000000 ))
  jq -nc \
    --arg sql "$search_sql" \
    --argjson start_time "$run_started_us" \
    --argjson end_time "$end_time_us" \
    '{query:{sql:$sql,start_time:$start_time,end_time:$end_time,from:0,size:200}}' \
    >"$temporary/log-search-request.json"

  if curl --fail --silent --show-error \
    --header @"$temporary/openobserve-auth-header" \
    --header 'Content-Type: application/json' \
    --data-binary @"$temporary/log-search-request.json" \
    "http://127.0.0.1:$port/api/default/_search?type=logs" \
    >"$temporary/log-search.json" 2>/dev/null \
    && jq -e --arg trace_id "$trace_id" \
      'any(.hits[]?; .trace_id == $trace_id)' \
      "$temporary/log-search.json" >/dev/null; then
    logs_observed=1
    break
  fi
  sleep 2
done

if [ "$logs_observed" -ne 1 ]; then
  echo "timed out waiting for a log record carrying trace $trace_id" >&2
  exit 1
fi

# Redaction has to hold on the log path too; the traces check above never reads this stream.
if grep -Fq "$sentinel" "$temporary/log-search.json"; then
  echo "provider input leaked into exported logs" >&2
  exit 1
fi

printf 'OpenObserve OTLP smoke test passed: %s spans and %s correlated log records in trace %s\n' \
  "$(jq --arg service "$service_name" '[.hits[] | select(.service_name == $service)] | length' "$temporary/search.json")" \
  "$(jq --arg trace_id "$trace_id" '[.hits[] | select(.trace_id == $trace_id)] | length' "$temporary/log-search.json")" \
  "$trace_id"
