#!/usr/bin/env bash
set -Eeuo pipefail
umask 022

usage() {
  echo "usage: $0 <ref> <scenario> [--profile fake|real|codex] [--diagnostic]" >&2
  exit 2
}

lab="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ "${LAB_SUPERVISED:-}" != 1 ]; then
  command -v python3 >/dev/null || { echo "lab: infrastructure: missing required command: python3" >&2; exit 5; }
  exec python3 "$lab/contract.py" "$@"
fi
trap 'echo "lab: infrastructure: runner command failed" >&2; exit 5' ERR
[ "$#" -ge 2 ] || usage
ref="$1"
scenario="$2"
shift 2
profile=fake
diagnostic=false
while [ "$#" -gt 0 ]; do
  case "$1" in
    --profile) [ "$#" -ge 2 ] || usage; profile="$2"; shift 2 ;;
    --diagnostic) diagnostic=true; shift ;;
    *) usage ;;
  esac
done
case "$profile" in fake | real | codex) ;; *) usage ;; esac
if [ "$profile" = real ] && [ -z "${OPENROUTER_API_KEY:-}" ]; then
  echo "--profile real reads OPENROUTER_API_KEY from the environment; it is unset" >&2
  exit 5
fi
if [ "$profile" = codex ] && ! [ -f "${LAB_CODEX_AUTH_JSON:-}" ]; then
  echo "--profile codex reads the auth file named by LAB_CODEX_AUTH_JSON; it is unset or not a file" >&2
  exit 5
fi

lab="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(git -C "$lab" rev-parse --show-toplevel)"
work="${LAB_WORK:-$lab/.work}"
build="${LAB_BUILD:-debug}"
otlp_port="${LAB_OTLP_PORT:-4318}"
collector_image="${LAB_COLLECTOR_IMAGE:-otel/opentelemetry-collector-contrib:0.115.1@sha256:d2da12c4336a79758826700be9e21ecf4a9f7d945b7f8a58ba55ee3fa45427c8}"

for command in cargo docker git python3 setpriv curl; do
  command -v "$command" >/dev/null || { echo "missing required command: $command" >&2; exit 5; }
done
case "$build" in debug | release) ;; *) echo "LAB_BUILD must be debug or release" >&2; exit 2 ;; esac
scenario_status=0
python3 - "$lab" "$scenario" <<'PYSCENARIO' || scenario_status=$?
import sys
sys.path.insert(0, sys.argv[1])
try:
    import lab
except Exception:
    print("lab: infrastructure: lab.py import failed", file=sys.stderr)
    sys.exit(5)
if sys.argv[2] not in lab.SCENARIOS:
    print("unknown scenario: " + sys.argv[2], file=sys.stderr)
    sys.exit(2)
PYSCENARIO
[ "$scenario_status" -eq 0 ] || exit "$scenario_status"

if ! sha="$(git -C "$repo" rev-parse --verify --quiet "$ref^{commit}")"; then
  git -C "$repo" fetch --quiet origin "$ref"
  sha="$(git -C "$repo" rev-parse --verify "FETCH_HEAD^{commit}")"
fi
run_id="$(date -u +%Y%m%dT%H%M%SZ)-${sha:0:8}-$scenario"
run="$lab/runs/$run_id"
state="${LAB_STATE_ROOT:-/var/tmp/dekopon-lab}/$run_id"
mkdir -p "$run" "$state/bin"
chmod 0755 "$state" "$state/bin"
printf '%s\n' "$run" >"$LAB_RUN_POINTER"
echo "run directory: $run"
container="dekopon-lab-otel-$$"
# Called indirectly by the EXIT trap.
# shellcheck disable=SC2329
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
trap 'cleanup' EXIT
trap 'exit 5' INT
trap 'exit 5' TERM

src="$work/src/$sha"
target="$work/target"
overlay=None
if [ -n "${LAB_EXTRA_CA:-}" ]; then
  grep -q "BEGIN CERTIFICATE" "$LAB_EXTRA_CA" || { echo "LAB_EXTRA_CA holds no PEM certificate" >&2; exit 2; }
  patch="$lab/overlay/extra-ca.patch"
  patch_sha="$(sha256sum "$patch" | cut -d' ' -f1)"
  ca_sha="$(sha256sum "$LAB_EXTRA_CA" | cut -d' ' -f1)"
  src="$work/src/$sha-extra-ca-${patch_sha:0:12}-${ca_sha:0:12}"
  target="$work/target-extra-ca"
  overlay="{\"name\": \"extra CA\", \"patch_sha256\": \"$patch_sha\", \"ca_sha256\": \"$ca_sha\"}"
fi
if [ ! -d "$src" ]; then
  mkdir -p "$work/src"
  git -C "$repo" worktree add --quiet --detach "$src" "$sha"
  if [ "$overlay" != None ]; then
    git -C "$src" apply "$patch" || { git -C "$repo" worktree remove --force "$src"; echo "the extra-CA overlay does not apply at $sha" >&2; exit 5; }
    install -m 0644 "$LAB_EXTRA_CA" "$src/crates/dekopon-model/lab-extra-ca.pem"
  fi
fi

cargo_flags=(--locked -p dekopon-brokerd -p dekopon-gatewayd --target-dir "$target")
[ "$build" = release ] && cargo_flags+=(--release)
(cd "$src" && cargo build "${cargo_flags[@]}")

for binary in dekopon-brokerd dekopon-gatewayd; do
  install -m 0755 "$target/$build/$binary" "$state/bin/$binary"
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
else
  broker_uid="$(id -u)"
  gateway_uid="$broker_uid"
  ipc_gid="$(id -g)"
  separation="one shared UID: the lab is not running as root"
fi


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
           "diagnostic": "$diagnostic" == "true", "overlay": $overlay, "started_utc": "$(date -u +%Y-%m-%dT%H:%M:%SZ)", "collector_image": "$collector_image",
           "uids": {"broker": $broker_uid, "gateway": $gateway_uid, "ipc_gid": $ipc_gid,
                    "separation": "$separation"}}, open(sys.argv[1], "w"), indent=1)
PYMETA

codex_flags=()
if [ "$profile" = codex ]; then
  codex_flags=(--codex-auth "$LAB_CODEX_AUTH_JSON" --max-turns "${LAB_CODEX_MAX_TURNS:-120}")
  [ -z "${LAB_CODEX_MODEL:-}" ] || codex_flags+=(--codex-model "$LAB_CODEX_MODEL")
  [ "${LAB_CODEX_SUMMARIZER:-0}" != 1 ] || codex_flags+=(--codex-summarizer)
fi
lab_status=0
python3 "$lab/lab.py" --run "$run" --state "$state" --bin "$state/bin" --scenario "$scenario" \
  --profile "$profile" --otlp "http://127.0.0.1:$otlp_port" \
  --broker-uid "$broker_uid" --gateway-uid "$gateway_uid" --ipc-gid "$ipc_gid" \
  ${LAB_RUST_LOG:+--rust-log "$LAB_RUST_LOG"} ${LAB_REAL_MODEL:+--real-model "$LAB_REAL_MODEL"} \
  --cost-cap "${LAB_COST_CAP_USD:-2}" "${codex_flags[@]}" \
  || lab_status=$?

collector_status=0
collector_running="$(docker inspect --format '{{.State.Running}}' "$container")"
docker stop --time 10 "$container" >/dev/null
docker logs "$container" >"$run/collector.log" 2>&1
collector_exit="$(docker inspect --format '{{.State.ExitCode}}' "$container")"
if [ "$collector_running" != true ] || [ "$collector_exit" != 0 ]; then
  echo "lab: infrastructure: collector exited unexpectedly (running=$collector_running, exit=$collector_exit)" >&2
  collector_status=5
fi
docker rm "$container" >/dev/null
report_status=0
python3 "$lab/report.py" "$run" || report_status=$?
if [ "$lab_status" -ne 0 ] || [ "$collector_status" -ne 0 ]; then
  [ "$lab_status" -eq 0 ] || echo "lab: infrastructure: lab.py exited $lab_status" >&2
  exit 5
fi
if [ "$report_status" -ne 0 ]; then
  echo "lab: missing-evidence: report.py failed" >&2
  exit 4
fi
if [ "${LAB_FAULT_DROP_REPORT:-0}" = 1 ]; then
  rm -f "$run/summary.md" "$run/verdict.json"
fi
echo "run directory: $run"
exit "$lab_status"
