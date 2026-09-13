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
stage=$(mktemp)
container=""
cleanup() {
  if [ -n "$container" ]; then
    docker logs "$container" || true
    docker rm -f "$container" >/dev/null || true
  fi
  sudo rm -rf -- "$work"
  rm -f -- "$stage"
}
trap cleanup EXIT
sudo chown 65532:65532 "$work"
sudo chmod 0700 "$work"

# write_config <tier> [unknown-field] — the smallest broker.json the image's binary accepts, plus an
# optional leading field no binary knows, so its strict decode refusal lists the fields it does.
write_config() {
  python3 - "$stage" "$@" <<'PY'
import json
import sys
stage, tier = sys.argv[1], sys.argv[2]
config = {}
if len(sys.argv) > 3:
    config[sys.argv[3]] = 0
config.update({
    "apiVersion": "dekopon.dev/brokerd/v1alpha1",
    "socketPath": "/proof/broker.sock",
    "brokerPrincipal": "image-broker",
    "policyRevision": "image-proof",
    "providers": ["/opt/dekopon/providers/cli-probe-provider.wasm"],
    "identities": [{"uid": 65532, "principal": "image-peer",
                    "actor": {"type": "service", "principal": "image-peer"}}],
})
if tier in ("checkpoint", "audit-file"):
    config["auditPath"] = "/proof/audit.jsonl"
if tier == "checkpoint":
    config["checkpointPath"] = "/proof/checkpoint.json"
    config["checkpointLockPath"] = "/proof/checkpoint.lock"
with open(stage, "w") as handle:
    json.dump(config, handle)
PY
  sudo install -m 0600 -o 65532 -g 65532 "$stage" "$work/broker.json"
}

# Released binaries are immutable bytes with their own configuration contract, so detect it from
# them rather than attributing them to the current checkout. Three tiers:
#   checkpoint  no `probe` subcommand (0.12.0 and earlier); requires `auditPath` and the checkpoint pair.
#   audit-file  has `probe` but still requires `auditPath`, the on-disk audit sink.
#   current     has no on-disk audit sink and refuses `auditPath` as an unknown field.
# The first is read off the command surface. The second and third share it, so a strict decode
# of a field neither knows tells them apart: the refusal names every field the binary accepts.
help=$(docker run --rm "$image" dekopon-brokerd --help)
tier=current
if ! grep -Eq '^[[:space:]]+probe[[:space:]]' <<<"$help"; then
  tier=checkpoint
else
  write_config current x-dekopon-image-proof
  refusal=$(docker run --rm --mount "type=bind,src=$work,dst=/proof" \
    "$image" dekopon-brokerd --config /proof/broker.json 2>&1 || true)
  if ! grep -q 'x-dekopon-image-proof' <<<"$refusal"; then
    echo "error: the broker did not refuse an unknown configuration field by name:" >&2
    echo "$refusal" >&2
    exit 1
  fi
  if grep -q 'auditPath' <<<"$refusal"; then tier=audit-file; fi
fi
echo "released broker configuration tier: $tier"
write_config "$tier"
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
# The broker binds only after compiling and describing the configured cli-probe component.
# New releases additionally exercise the existing bounded protocol probe in the image.
if [ "$tier" != checkpoint ]; then
  docker exec "$container" dekopon-brokerd probe --socket /proof/broker.sock
else
  echo "legacy released binary: component-load/socket-startup proof (no probe subcommand)"
fi
docker stop --time 10 "$container" >/dev/null
test "$(docker inspect --format '{{.State.ExitCode}}' "$container")" = 0
