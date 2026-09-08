#!/usr/bin/env bash
# Exercise broker component loading in an already-built release image, not source HEAD.
# Requires Linux, Docker, Python 3 and sudo for the broker's numeric private-file owner.
set -euo pipefail
if [ "$#" -ne 1 ]; then
  echo "usage: $0 <image>" >&2
  exit 2
fi
image=$1
work=$(mktemp -d)
container=""
cleanup() {
  if [ -n "$container" ]; then
    docker logs "$container" || true
    docker rm -f "$container" >/dev/null || true
  fi
  sudo rm -rf -- "$work"
}
trap cleanup EXIT
# Released pre-retirement binaries still require checkpoint configuration. Detect their
# command surface rather than attributing these immutable bytes to the current checkout.
help=$(docker run --rm "$image" dekopon-brokerd --help)
legacy=true
if grep -Eq '^[[:space:]]+probe[[:space:]]' <<<"$help"; then legacy=false; fi
python3 - "$work" "$legacy" <<'PY'
import json
import pathlib
import sys
root = pathlib.Path(sys.argv[1])
config = {
    "apiVersion": "dekopon.dev/brokerd/v1alpha1",
    "socketPath": "/proof/broker.sock",
    "auditPath": "/proof/audit.jsonl",
    "brokerPrincipal": "image-broker",
    "policyRevision": "image-proof",
    "providers": ["/opt/dekopon/providers/echo-provider.wasm"],
    "identities": [{"uid": 65532, "principal": "image-peer",
                    "actor": {"type": "service", "principal": "image-peer"}}],
}
if sys.argv[2] == "true":
    config["checkpointPath"] = "/proof/checkpoint.json"
    config["checkpointLockPath"] = "/proof/checkpoint.lock"
(root / "broker.json").write_text(json.dumps(config))
PY
sudo chown -R 65532:65532 "$work"
sudo chmod 0700 "$work"
sudo chmod 0600 "$work/broker.json"
container=$(docker run -d --mount "type=bind,src=$work,dst=/proof" \
  "$image" dekopon-brokerd --config /proof/broker.json)
ready=false
for ((attempt=0; attempt<60; attempt++)); do
  if [ "$(docker inspect --format '{{.State.Running}}' "$container")" != true ]; then
    echo "error: broker exited before component loading and socket startup" >&2
    exit 1
  fi
  if sudo test -S "$work/broker.sock"; then ready=true; break; fi
  sleep 1
done
if [ "$ready" != true ]; then
  echo "error: broker component-load/startup deadline exceeded" >&2
  exit 1
fi
# The broker binds only after compiling and describing the configured echo component.
# New releases additionally exercise the existing bounded protocol probe in the image.
if [ "$legacy" = false ]; then
  docker exec "$container" dekopon-brokerd probe --socket /proof/broker.sock
else
  echo "legacy released binary: component-load/socket-startup proof (no probe subcommand)"
fi
docker stop --time 10 "$container" >/dev/null
test "$(docker inspect --format '{{.State.ExitCode}}' "$container")" = 0
