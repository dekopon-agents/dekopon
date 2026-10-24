#!/usr/bin/env bash
set -Eeuo pipefail
umask 022

usage() {
  echo "usage: $0 <ref> <scenario> [--profile fake|real|codex]" >&2
  exit 2
}

[ "$#" -ge 2 ] || usage
ref="$1"
scenario="$2"
shift 2
profile=fake
while [ "$#" -gt 0 ]; do
  case "$1" in
    --profile) profile="${2:?}"; shift 2 ;;
    *) usage ;;
  esac
done
case "$profile" in fake | real | codex) ;; *) usage ;; esac
if [ "$profile" = real ] && [ -z "${OPENROUTER_API_KEY:-}" ]; then
  echo "--profile real reads OPENROUTER_API_KEY from the environment; it is unset" >&2
  exit 2
fi
if [ "$profile" = codex ] && ! [ -f "${LAB_CODEX_AUTH_JSON:-}" ]; then
  echo "--profile codex reads the auth file named by LAB_CODEX_AUTH_JSON; it is unset or not a file" >&2
  exit 2
fi

lab="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(git -C "$lab" rev-parse --show-toplevel)"
work="${LAB_WORK:-$lab/.work}"
build="${LAB_BUILD:-debug}"
otlp_port="${LAB_OTLP_PORT:-4318}"
collector_image="otel/opentelemetry-collector-contrib:0.115.1@sha256:d2da12c4336a79758826700be9e21ecf4a9f7d945b7f8a58ba55ee3fa45427c8"

for command in cargo docker git python3 setpriv; do
  command -v "$command" >/dev/null || { echo "missing required command: $command" >&2; exit 1; }
done
case "$build" in debug | release) ;; *) echo "LAB_BUILD must be debug or release" >&2; exit 2 ;; esac
python3 -c "import sys; sys.path.insert(0, '$lab'); import lab; sys.exit('$scenario' not in lab.SCENARIOS)" \
  || { echo "unknown scenario: $scenario" >&2; exit 2; }

if ! sha="$(git -C "$repo" rev-parse --verify --quiet "$ref^{commit}")"; then
  git -C "$repo" fetch --quiet origin "$ref"
  sha="$(git -C "$repo" rev-parse --verify "FETCH_HEAD^{commit}")"
fi
src="$work/src/$sha"
if [ ! -d "$src" ]; then
  mkdir -p "$work/src"
  git -C "$repo" worktree add --quiet --detach "$src" "$sha"
fi

cargo_flags=(--locked -p dekopon-brokerd -p dekopon-gatewayd --target-dir "$work/target")
[ "$build" = release ] && cargo_flags+=(--release)
(cd "$src" && cargo build "${cargo_flags[@]}")

run_id="$(date -u +%Y%m%dT%H%M%SZ)-${sha:0:8}-$scenario"
run="$lab/runs/$run_id"
state="${LAB_STATE_ROOT:-/var/tmp/dekopon-lab}/$run_id"
mkdir -p "$run" "$state/bin"
chmod 0755 "$state" "$state/bin"
for binary in dekopon-brokerd dekopon-gatewayd; do
  install -m 0755 "$work/target/$build/$binary" "$state/bin/$binary"
done
install -m 0644 "$src/examples/providers/cli-probe-provider.wasm" "$state/bin/cli-probe-provider.wasm"
(cd "$state/bin" && sha256sum dekopon-brokerd dekopon-gatewayd cli-probe-provider.wasm) >"$run/artifacts.sha256"

if [ "$(id -u)" -eq 0 ] && command -v useradd >/dev/null; then
  getent group lab-ipc >/dev/null || groupadd --system lab-ipc
  for user in lab-broker lab-gateway; do
    id "$user" >/dev/null 2>&1 || useradd --system --user-group --no-create-home --shell /usr/sbin/nologin "$user"
  done
  broker_uid="$(id -u lab-broker)"
  gateway_uid="$(id -u lab-gateway)"
  ipc_gid="$(getent group lab-ipc | cut -d: -f3)"
  separation="separate UIDs"
  if [ "$profile" = codex ]; then
    gateway_uid="$(stat -c %u "$(dirname "$LAB_CODEX_AUTH_JSON")")"
    separation="separate UIDs; gatewayd runs as the owner of the Codex auth directory"
  fi
else
  broker_uid="$(id -u)"
  gateway_uid="$broker_uid"
  ipc_gid="$(id -g)"
  separation="one shared UID: the lab is not running as root"
fi

container="dekopon-lab-otel-$$"
cleanup() {
  status=$?
  set +e
  if docker container inspect "$container" >/dev/null 2>&1; then
    docker stop --time 10 "$container" >/dev/null
    docker logs "$container" >"$run/collector.log" 2>&1
    docker rm "$container" >/dev/null
  fi
  rm -rf "$state"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

docker run --detach --name "$container" --network host --user 0 \
  --env "LAB_OTLP_PORT=$otlp_port" \
  --volume "$lab/otelcol.yaml:/etc/lab/otelcol.yaml:ro" \
  --volume "$run:/out" \
  "$collector_image" --config=/etc/lab/otelcol.yaml >/dev/null
for _ in $(seq 1 60); do
  if curl --silent --output /dev/null --max-time 2 -X POST -H 'Content-Type: application/json' \
    --data '{}' "http://127.0.0.1:$otlp_port/v1/traces"; then
    break
  fi
  sleep 1
done
curl --silent --fail --output /dev/null --max-time 2 -X POST -H 'Content-Type: application/json' \
  --data '{}' "http://127.0.0.1:$otlp_port/v1/traces"

python3 - "$run/meta.json" <<PYMETA
import json, sys
json.dump({"ref": "$ref", "sha": "$sha", "scenario": "$scenario", "profile": "$profile", "build": "$build",
           "started_utc": "$(date -u +%Y-%m-%dT%H:%M:%SZ)", "collector_image": "$collector_image",
           "uids": {"broker": $broker_uid, "gateway": $gateway_uid, "ipc_gid": $ipc_gid,
                    "separation": "$separation"}}, open(sys.argv[1], "w"), indent=1)
PYMETA

lab_status=0
python3 "$lab/lab.py" --run "$run" --state "$state" --bin "$state/bin" --scenario "$scenario" \
  --profile "$profile" --otlp "http://127.0.0.1:$otlp_port" \
  --broker-uid "$broker_uid" --gateway-uid "$gateway_uid" --ipc-gid "$ipc_gid" \
  ${LAB_RUST_LOG:+--rust-log "$LAB_RUST_LOG"} ${LAB_REAL_MODEL:+--real-model "$LAB_REAL_MODEL"} \
  ${LAB_CODEX_AUTH_JSON:+--codex-auth "$LAB_CODEX_AUTH_JSON"} ${LAB_CODEX_MODEL:+--codex-model "$LAB_CODEX_MODEL"} \
  || lab_status=$?

docker stop --time 10 "$container" >/dev/null
docker logs "$container" >"$run/collector.log" 2>&1
docker rm "$container" >/dev/null
python3 "$lab/report.py" "$run"
echo "run directory: $run"
exit "$lab_status"
