#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
example_dir="$root/examples/otel-traces"
port="${OPENOBSERVE_PORT:-5080}"
project="${COMPOSE_PROJECT_NAME:-dekopon-otel-smoke-$$}"
stream="${OPENOBSERVE_STREAM:-dekopon_smoke}"
service_name="${OTEL_SERVICE_NAME:-dekopon-daemons-smoke}"
temporary=""
compose_owned=0
driver_pid=""

export OPENOBSERVE_PORT="$port"
export OPENOBSERVE_ROOT_EMAIL="${OPENOBSERVE_ROOT_EMAIL:-root@example.com}"
export OPENOBSERVE_ROOT_PASSWORD="${OPENOBSERVE_ROOT_PASSWORD:-DekoponSmoke#123}"

compose=(docker compose --project-name "$project" --file "$example_dir/compose.yaml")

for command in base64 cargo curl docker jq python3; do
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
  if [ -n "$driver_pid" ]; then
    kill -TERM "$driver_pid" 2>/dev/null || true
    wait "$driver_pid" 2>/dev/null || true
  fi
  if [ "$status" -ne 0 ]; then
    for log in "$temporary"/dekopon*.log; do
      [ ! -f "$log" ] || tail -n 100 "$log" >&2
    done
    echo "--- OpenObserve diagnostics ---" >&2
    if [ "$compose_owned" -eq 1 ]; then
      "${compose[@]}" ps >&2 || true
      "${compose[@]}" logs --no-color --tail=300 openobserve >&2 || true
    fi
    if [ -s "$temporary/search.json" ]; then
      echo "--- last trace search response ---" >&2
      jq . "$temporary/search.json" >&2 || cat "$temporary/search.json" >&2
    fi
  fi
  if [ "$compose_owned" -eq 1 ]; then
    "${compose[@]}" down --timeout 10 --volumes --remove-orphans >/dev/null 2>&1 || true
  fi
  for response in ingestion.json log-search.json search.json; do
    if [ -f "$temporary/$response" ]; then
      echo "--- $response ---"
      cat "$temporary/$response"
      echo
    fi
  done
  rm -rf "$temporary"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
temporary="$(mktemp -d)"
# Refuse collisions before creating anything; never adopt somebody else's receiver.
if [ -n "$(docker ps -aq --filter "label=com.docker.compose.project=$project")" ]   || [ -n "$(docker volume ls -q --filter "label=com.docker.compose.project=$project")" ]   || [ -n "$(docker network ls -q --filter "label=com.docker.compose.project=$project")" ]; then
  echo "Compose project is not empty: $project" >&2
  exit 1
fi
python3 - "$port" <<'PYPORT'
import socket, sys
with socket.socket() as listener:
    listener.bind(("127.0.0.1", int(sys.argv[1])))
PYPORT
compose_owned=1

"${compose[@]}" up --detach

for _ in $(seq 1 120); do
  if curl --connect-timeout 3 --max-time 10 --max-filesize 8388608 --fail --silent --show-error \
    "http://127.0.0.1:$port/healthz" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
curl --connect-timeout 3 --max-time 10 --max-filesize 8388608 --fail --silent --show-error "http://127.0.0.1:$port/healthz" >/dev/null

if [ "${DEKOPON_OTEL_SKIP_BUILD:-0}" != "1" ]; then
  (cd "$root" && cargo build --locked -p dekopon-brokerd -p dekopond)
fi

provider="$root/examples/providers/echo-provider.wasm"
for binary in dekopon-brokerd dekopond; do
  test -x "$root/target/debug/$binary" || {
    echo "$binary is missing; rerun without DEKOPON_OTEL_SKIP_BUILD=1" >&2
    exit 1
  }
done
test -f "$provider" || {
  echo "echo fixture is missing; run ci/fetch-external-provider-components.sh examples/providers echo" >&2
  exit 1
}

auth_token="$({ printf '%s:%s' "$OPENOBSERVE_ROOT_EMAIL" "$OPENOBSERVE_ROOT_PASSWORD"; } | base64 | tr -d '\r\n')"
printf 'Authorization: Basic %s\n' "$auth_token" >"$temporary/openobserve-auth-header"
run_started_us=$(( $(date +%s) * 1000000 - 60000000 ))
sentinel="DEKOPON_OTEL_SMOKE_INPUT_MUST_NOT_APPEAR"

OTEL_EXPORTER_OTLP_HEADERS="Authorization=Basic%20${auth_token},organization=default,stream-name=${stream}"   python3 "$example_dir/drive-turn.py" "$root" "$temporary"     "http://127.0.0.1:$port/api/default" "$service_name" &
driver_pid=$!
wait "$driver_pid"
driver_pid=""

# Smoke-only shipper: actual daemon JSON stdout, never records derived from traces.
python3 - "$temporary" "$port" "$stream" <<'PYSHIP'
import json, os, pathlib, re, sys, urllib.request
folder, port, stream = sys.argv[1:]
folder = pathlib.Path(folder)
records = []
for daemon in ("dekopond", "dekopon-brokerd"):
    path = folder / (daemon + ".log")
    assert path.stat().st_size <= 4 * 1024 * 1024, "stdout bound"
    rows = [json.loads(line) for line in path.read_text().splitlines()]
    assert rows and any("trace_id" not in row and "span_id" not in row for row in rows), "no-context record missing"
    assert any("trace_id" in row for row in rows), "native correlated stdout missing"
    for row in rows:
        if "trace_id" in row:
            assert re.fullmatch(r"[0-9a-f]{32}", row["trace_id"]) and int(row["trace_id"], 16)
            assert re.fullmatch(r"[0-9a-f]{16}", row["span_id"]) and int(row["span_id"], 16)
        else:
            assert "span_id" not in row, "partial native IDs"
        row["daemon"] = daemon
        records.append(row)
auth = (folder / "openobserve-auth-header").read_text().strip().split(": ", 1)[1]
for path in folder.glob("dekopon*.log"):
    assert path.stat().st_size <= 4 * 1024 * 1024, "diagnostic bound"
    text = path.read_text()
    for secret in ("DEKOPON_OTEL_SMOKE_INPUT_MUST_NOT_APPEAR",
                   "DEKOPON_OTEL_SMOKE_CREDENTIAL_MUST_NOT_APPEAR",
                   auth.removeprefix("Basic "), os.environ["OPENOBSERVE_ROOT_PASSWORD"]):
        assert secret not in text, "local telemetry redaction failed"
request = urllib.request.Request(f"http://127.0.0.1:{port}/api/default/{stream}/_json",
    data=json.dumps(records).encode(), headers={"Authorization": auth, "Content-Type": "application/json"})
with urllib.request.urlopen(request, timeout=15) as response:
    assert response.status == 200, "stdout ingestion failed"
    body = response.read(1048577)
    assert len(body) <= 1048576, "ingestion response bound"
    (folder / "ingestion.json").write_bytes(body)
    result = json.loads(body)
    assert result["code"] == 200, "ingestion rejected"
    assert sum(item["successful"] for item in result["status"]) == len(records), "ingestion count mismatch"
    assert all(item["failed"] == 0 for item in result["status"]), "ingestion record rejection"
(folder / "shipped.json").write_text(json.dumps(records))
PYSHIP

search_sql="SELECT * FROM \"$stream\""
observed=0
for _ in $(seq 1 60); do
  end_time_us=$(( $(date +%s) * 1000000 + 60000000 ))
  jq -nc \
    --arg sql "$search_sql" \
    --argjson start_time "$run_started_us" \
    --argjson end_time "$end_time_us" \
    '{query:{sql:$sql,start_time:$start_time,end_time:$end_time,from:0,size:10000}}' \
    >"$temporary/search-request.json"

  if curl --connect-timeout 3 --max-time 10 --max-filesize 8388608 --fail --silent --show-error \
    --header @"$temporary/openobserve-auth-header" \
    --header 'Content-Type: application/json' \
    --data-binary @"$temporary/search-request.json" \
    "http://127.0.0.1:$port/api/default/_search?type=traces" \
    >"$temporary/search.json" 2>/dev/null \
    && jq -e --arg service "$service_name" \
      'any(.hits[]?; .service_name == $service and .operation_name == "gateway.message")' \
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

for operation_name in gateway.message gateway.session broker.invocation provider.compile provider.invoke; do
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

for operation_name in gateway.session broker.invocation provider.invoke; do
  jq -e --arg service "$service_name" --arg operation "$operation_name" '
    [.hits[] | select(.service_name == $service and .operation_name == "gateway.message") | .trace_id] as $ids
    | any(.hits[]; .service_name == $service and .operation_name == $operation
        and (.trace_id as $id | $ids | index($id) != null))' "$temporary/search.json" >/dev/null || {
    echo "span not joined to gateway trace: $operation_name" >&2
    exit 1
  }
done

trace_id="$(jq -r --arg service "$service_name" \
  '.hits[] | select(.service_name == $service and .operation_name == "gateway.message") | .trace_id' \
  "$temporary/search.json" | head -n 1)"
test -n "$trace_id"

# Daemon exit alone cannot prove exporter delivery: require correlated remote logs.
logs_observed=0
for _ in $(seq 1 60); do
  end_time_us=$(( $(date +%s) * 1000000 + 60000000 ))
  jq -nc \
    --arg sql "$search_sql" \
    --argjson start_time "$run_started_us" \
    --argjson end_time "$end_time_us" \
    '{query:{sql:$sql,start_time:$start_time,end_time:$end_time,from:0,size:10000}}' \
    >"$temporary/log-search-request.json"

  if curl --connect-timeout 3 --max-time 10 --max-filesize 8388608 --fail --silent --show-error \
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

# Independently read-back logs must preserve the shipped native pair and match a real span
# from each process; a trace-only success or a gateway-only log cannot satisfy this gate.
python3 - "$temporary" "$trace_id" <<'PYCORRELATE'
import json, pathlib, sys
folder = pathlib.Path(sys.argv[1])
trace = sys.argv[2]
responses = [json.loads((folder / name).read_text()) for name in ("search.json", "log-search.json")]
for response in responses:
    assert len(response["hits"]) < 10000, "query saturated"
    assert response["total"] == len(response["hits"]), "query truncated"
    assert not response.get("is_partial", False), "partial search"
spans, logs = [response["hits"] for response in responses]
shipped = json.loads((folder / "shipped.json").read_text())
assert len(logs) == len(shipped), "remote log coverage mismatch"
for daemon in ("dekopond", "dekopon-brokerd"):
    pairs = {(row["trace_id"], row["span_id"]) for row in shipped
             if row["daemon"] == daemon and row.get("trace_id")
             and (daemon != "dekopond" or row["trace_id"] == trace)}
    assert pairs, f"no local native IDs for {daemon}"
    assert any(row.get("daemon") == daemon
               and (row.get("trace_id"), row.get("span_id")) in pairs
               and any(span.get("trace_id") == row["trace_id"] and span.get("span_id") == row["span_id"]
                       for span in spans) for row in logs), f"independent correlation missing for {daemon}"
print("Both daemon stdout native ID pairs independently correlated with remote spans")
PYCORRELATE

# Redaction has to hold on the log path too; the traces check above never reads this stream.
if grep -Fq "$sentinel" "$temporary/log-search.json"; then
  echo "provider input leaked into exported logs" >&2
  exit 1
fi

# Check payload and actual fixture credentials in both signals and local daemon diagnostics.
python3 - "$temporary" <<'PYREDACT'
import os, pathlib, sys
folder = pathlib.Path(sys.argv[1])
auth = (folder / "openobserve-auth-header").read_text().strip().split("Basic ", 1)[1]
paths = [folder / name for name in ("search.json", "log-search.json", "shipped.json")]
paths.extend(folder.glob("dekopon*.log"))
for path in paths:
    assert path.stat().st_size <= 8 * 1024 * 1024, "telemetry response bound"
    text = path.read_text()
    for secret in ("DEKOPON_OTEL_SMOKE_INPUT_MUST_NOT_APPEAR",
                   "DEKOPON_OTEL_SMOKE_CREDENTIAL_MUST_NOT_APPEAR",
                   auth, os.environ["OPENOBSERVE_ROOT_PASSWORD"]):
        assert secret not in text, "sentinel or credential leaked into telemetry"
PYREDACT

printf 'OpenObserve OTLP smoke test passed: %s spans and %s correlated log records in trace %s\n' \
  "$(jq --arg service "$service_name" '[.hits[] | select(.service_name == $service)] | length' "$temporary/search.json")" \
  "$(jq --arg trace_id "$trace_id" '[.hits[] | select(.trace_id == $trace_id)] | length' "$temporary/log-search.json")" \
  "$trace_id"
