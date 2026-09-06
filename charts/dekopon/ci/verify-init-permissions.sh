#!/usr/bin/env bash
# Prove the init container produces files dekopon-brokerd and dekopond will accept, and that the
# ChatGPT credential is seeded exactly once.
#
# A rendered manifest that looks right is not the same as a file that survives O_NOFOLLOW plus an
# owner and mode check, so this renders the chart, pulls the init container's *actual* command out
# of the manifest, and runs it verbatim in a container under the securityContext the chart renders,
# against a fixture built to match a Kubernetes projected volume exactly: real files under
# ..<timestamp>/, ..data -> ..<timestamp>, and key -> ..data/key.
#
# It asserts the filesystem tiers as each distinct UID. This is rendered-layout proof,
# not a replacement for brokerd/tests/ipc_process.rs real-daemon acceptance.
#
# Requires: helm, docker (with linux/arm64 emulation or an arm64 host), python3.
#
#   charts/dekopon/ci/verify-init-permissions.sh
#   PLATFORM=linux/amd64 charts/dekopon/ci/verify-init-permissions.sh
set -euo pipefail

chart_dir=$(cd "$(dirname "$0")/.." && pwd)
values="$chart_dir/values-pr-summarizer-linter.yaml"
work=$(mktemp -d)
resource="dekopon-init-$(python3 -c 'import uuid; print(uuid.uuid4().hex)')"
: > "$work/volumes"
: > "$work/containers"
cleanup() {
  while IFS= read -r name; do command docker rm -f "$name" >/dev/null 2>&1 || true; done < "$work/containers"
  while IFS= read -r name; do command docker volume rm "$name" >/dev/null 2>&1 || true; done < "$work/volumes"
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
# Unique owned names allow unconditional cleanup even when a Docker client is interrupted.
docker() {
  if [ "$1" = run ]; then
    shift
    local name
    name="$resource-$(python3 -c 'import uuid; print(uuid.uuid4().hex)')"
    echo "$name" >> "$work/containers"
    command docker run --name "$name" "$@"
  else
    command docker "$@"
  fi
}

platform=${PLATFORM:-linux/arm64}
busybox=busybox@sha256:fc6dddc4c44b1bfe37f41cae8e67d1693828e8f42a91862816d7953e2c9d3f23
python_image=python:3.13-alpine

# --------------------------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------------------------
extract='import yaml,sys
docs=[d for d in yaml.safe_load_all(sys.stdin) if d]
dep=[d for d in docs if d["kind"]=="Deployment"][0]
ic=dep["spec"]["template"]["spec"]["initContainers"][0]
assert ic["name"]=="prepare-files", ic["name"]
pod=dep["spec"]["template"]["spec"]
assert pod["securityContext"]["runAsUser"]==65532
assert pod["securityContext"]["runAsGroup"]==65532
assert pod["securityContext"]["supplementalGroups"]==[65534]
assert "fsGroup" not in pod["securityContext"]
security=ic["securityContext"]
assert security["runAsUser"]==security["runAsGroup"]==0
assert security["capabilities"]=={"drop":["ALL"],"add":["CHOWN","FOWNER"]}
assert security["readOnlyRootFilesystem"] and not security["allowPrivilegeEscalation"]
gateway=next(c for c in pod["containers"] if c["name"]=="gateway")
broker=next(c for c in pod["initContainers"] if c["name"]=="broker")
for c,uid in ((gateway,65533),(broker,65532)):
    assert c["securityContext"]["runAsUser"]==c["securityContext"]["runAsGroup"]==uid
    assert c["securityContext"]["capabilities"]=={"drop":["ALL"]}
    assert c["securityContext"]["readOnlyRootFilesystem"]
    assert not c["securityContext"]["allowPrivilegeEscalation"]
gm={m["name"]:m for m in gateway["volumeMounts"]}
bm={m["name"]:m for m in broker["volumeMounts"]}
assert "config" not in gm and "gateway-config" not in bm
assert "tmp" not in gm and "gateway-tmp" not in bm
assert bm["state"]["subPath"]=="broker"
assert "state" not in gm or gm["state"]["subPath"] != "broker"
assert "config-source" not in gm and "config-source" not in bm
expected={"config-source":"/dekopon-source", "config":"/etc/dekopon",
          "gateway-config":"/etc/dekopon-gateway", "runtime":"/run/dekopon",
          "state":"/var/lib/dekopon"}
if "provider-storage" in bm:
    expected.update({"provider-storage":"/var/lib/dekopon-provider-storage",
                     "provider-storage-key":"/etc/dekopon-storage-key",
                     "provider-storage-key-source":"/dekopon-storage-key-source"})
assert {m["name"]:m["mountPath"] for m in ic["volumeMounts"]}==expected
assert gm["gateway-config"]["mountPath"]==bm["config"]["mountPath"]=="/etc/dekopon"
assert gm["runtime"]["mountPath"]==bm["runtime"]["mountPath"]=="/run/dekopon"
assert bm["state"]["mountPath"]=="/var/lib/dekopon"
assert "state" not in gm or gm["state"]["mountPath"]=="/var/lib/dekopon/chatgpt"
config=next(d["stringData"] for d in docs if d["kind"]=="Secret" and "broker.yaml" in d.get("stringData",{}))
broker_config=yaml.safe_load(config["broker.yaml"])
gateway_config=yaml.safe_load(config["dekopond.yaml"])
assert gateway_config["broker"]["serverUid"]==broker["securityContext"]["runAsUser"]
peers={p["uid"]:p for p in broker_config["identities"]}
assert "attestor" in peers[gateway["securityContext"]["runAsUser"]]
assert 65532 in peers and "attestor" not in peers[65532]
assert ic["command"]==["/bin/sh","-c"]
assert ic["image"]=="busybox@sha256:fc6dddc4c44b1bfe37f41cae8e67d1693828e8f42a91862816d7953e2c9d3f23"
sys.stdout.write(ic["args"][0])'

# render_init <output-file> [extra helm args...]
render_init() {
  local out="$1"; shift
  helm template dekopon "$chart_dir" -f "$values" "$@" > "$work/render.yaml"
  if python3 -c 'import yaml' 2>/dev/null; then
    python3 -c "$extract" < "$work/render.yaml" > "$out"
  else
    docker run --rm -i -e PROG="$extract" "$python_image" \
      sh -c 'pip install --quiet --disable-pip-version-check pyyaml >/dev/null 2>&1; exec python3 -c "$PROG"' \
      < "$work/render.yaml" > "$out"
  fi
}

# run_init <script-file> — the rendered securityContext: root, everything dropped except CHOWN and
# FOWNER, no new privileges, read-only root filesystem.
run_init() {
  docker run --rm --platform "$platform" \
    --user 0:0 --group-add=65534 --cap-drop=ALL --cap-add=CHOWN --cap-add=FOWNER \
    --security-opt=no-new-privileges --read-only \
    -v "${resource}-src":/dekopon-source:ro -v "${resource}-src":/dekopon-storage-key-source:ro \
    -v "${resource}-etc":/etc/dekopon -v "${resource}-gateway-etc":/etc/dekopon-gateway \
    -v "${resource}-run":/run/dekopon -v "${resource}-state":/var/lib/dekopon \
    -v "${resource}-storage":/var/lib/dekopon-provider-storage -v "${resource}-storage-key":/etc/dekopon-storage-key \
    "$busybox" /bin/sh -c "$(cat "$1")"
}

# on_state <shell> — an unrestricted helper against the claim, for building fixtures and reading
# results back. Never the thing under test.
on_state() {
  docker run --rm -i --platform "$platform" -v "${resource}-state":/var/lib/dekopon "$busybox" sh -s
}

reset_mounts() {
  docker run --rm --platform "$platform" -v "${resource}-etc":/a -v "${resource}-run":/b -v "${resource}-state":/c \
    -v "${resource}-storage":/d -v "${resource}-storage-key":/e -v "${resource}-gateway-etc":/f "$busybox" \
    sh -c 'chmod 0777 /a /b /c /d /e /f; chown 0:0 /a /b /c /d /e /f'
}

credential_digest() {
  on_state <<'EOF' | tr -d '[:space:]'
sha256sum /var/lib/dekopon/chatgpt/chatgpt-auth.json | cut -d' ' -f1
EOF
}

assert_eq() {
  if [ "$2" = "$3" ]; then
    echo "PASS $1"
  else
    echo "FAIL $1: got '$2', want '$3'" >&2
    exit 1
  fi
}

for suffix in src etc gateway-etc run state storage storage-key; do
  v="$resource-$suffix"
  if docker volume inspect "$v" >/dev/null 2>&1; then
    echo "refusing preexisting test volume $v" >&2
    exit 1
  fi
  # Register before Docker can allocate and then fail or be interrupted.
  echo "$v" >> "$work/volumes"
  docker volume create "$v" >/dev/null
done

echo "==> building a projected-volume fixture (symlink farm, root-owned, 0400)"
python3 "$chart_dir/ci/check-init-cleanup.py" "$0"
docker run --rm -i --platform "$platform" -v "${resource}-src":/dekopon-source "$busybox" sh -s <<'FIXTURE'
set -eu
cd /dekopon-source
stamp=..2026_08_18_00_00_00.000000000
mkdir -p "$stamp"
printf 'apiVersion: dekopon.dev/brokerd/v1alpha1\n' > "$stamp/broker.yaml"
printf '@id("x") permit(principal, action, resource);\n' > "$stamp/policies.cedar"
printf 'apiVersion: dekopon.dev/broker-credentials/v1alpha1\ncredentials: []\n' > "$stamp/broker-credentials.yaml"
printf 'apiVersion: dekopon.dev/dekopond/v1alpha1\n' > "$stamp/dekopond.yaml"
printf '{"refresh":"SEED-REFRESH-TOKEN","expires_at":0}\n' > "$stamp/chatgpt-auth.json"
printf 'apiVersion: dekopon.dev/storage-key/v1alpha1\nkey: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n' > "$stamp/storage-key.yaml"
chmod 0400 "$stamp"/*
ln -sfn "$stamp" ..data
for k in broker.yaml policies.cedar broker-credentials.yaml dekopond.yaml chatgpt-auth.json storage-key.yaml; do
  ln -sfn "..data/$k" "$k"
done
chmod 0755 /dekopon-source
FIXTURE

# --------------------------------------------------------------------------------------------
# Part 1: the copy-every-start files
# --------------------------------------------------------------------------------------------
render_init "$work/init.sh"

echo "==> (a) cold start"
reset_mounts
run_init "$work/init.sh"

echo "==> (b) in-place restart: the emptyDirs still hold the previous run's 0700 directories"
run_init "$work/init.sh"

echo "==> stat of the result"
docker run --rm --platform "$platform" \
  -v "${resource}-etc":/etc/dekopon -v "${resource}-run":/run/dekopon -v "${resource}-state":/var/lib/dekopon "$busybox" \
  sh -c "stat -c '%n  uid=%u gid=%g mode=%a links=%h %F' /etc/dekopon/* /etc/dekopon /run/dekopon /var/lib/dekopon"

echo "==> broker filesystem tier checks, as UID 65532 (not real-daemon proof)"
cat > "$work/check.py" <<'CHECK'
import os, stat, sys
euid = os.geteuid()
fail = []
def ok(c, m):
    print(("PASS " if c else "FAIL ") + m)
    if not c:
        fail.append(m)

def owned_file(path, mask, label):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    except OSError as e:
        ok(False, f"{label}: {path} O_NOFOLLOW open failed ({e.strerror})")
        return
    st = os.fstat(fd)
    os.close(fd)
    ok(stat.S_ISREG(st.st_mode) and st.st_uid == euid
       and (st.st_mode & mask) == 0 and st.st_nlink == 1,
       f"{label}: {path} uid={st.st_uid} mode={oct(st.st_mode & 0o7777)} "
       f"nlink={st.st_nlink} mask={oct(mask)}")

def ancestors(d):
    p = d
    while True:
        st = os.lstat(p)
        ok(stat.S_ISDIR(st.st_mode)
           and ((st.st_mode & 0o022) == 0 or (st.st_mode & 0o1000) != 0),
           f"ancestor {p} mode={oct(st.st_mode & 0o7777)}")
        if p == "/":
            break
        p = os.path.dirname(p) or "/"

def private_parent(path):
    parent = os.path.realpath(os.path.dirname(path))
    ancestors(parent)
    st = os.lstat(parent)
    ok(stat.S_ISDIR(st.st_mode) and st.st_uid == euid and (st.st_mode & 0o077) == 0,
       f"private parent {parent} uid={st.st_uid} mode={oct(st.st_mode & 0o7777)}")

print("== Tier A: credentials (mode & 0o077) ==")
owned_file("/etc/dekopon/broker-credentials.yaml", 0o077, "credentials")
print("== Tier B: broker.yaml, policies.cedar (mode & 0o022) ==")
for f in ("broker.yaml", "policies.cedar"):
    owned_file(f"/etc/dekopon/{f}", 0o022, "config")
print("== Tier C and D: socket, audit, checkpoint and lock parents, and every ancestor ==")
for p in ("/var/lib/dekopon/broker/audit.jsonl",
          "/var/lib/dekopon/broker/audit-checkpoint.json", "/var/lib/dekopon/broker/audit-checkpoint.lock"):
    private_parent(p)
print("== what the broker creates for itself ==")
fd = os.open("/var/lib/dekopon/broker/audit.jsonl",
             os.O_RDWR | os.O_CREAT | os.O_APPEND | os.O_NOFOLLOW, 0o600)
st = os.fstat(fd)
os.close(fd)
ok((st.st_mode & 0o077) == 0 and st.st_nlink == 1 and st.st_uid == euid,
   f"audit.jsonl mode={oct(st.st_mode & 0o7777)} uid={st.st_uid} nlink={st.st_nlink}")
ancestors("/run/dekopon")
st = os.lstat("/run/dekopon")
ok(st.st_uid == 65532 and st.st_gid == 65534 and stat.S_IMODE(st.st_mode) == 0o710,
   "IPC parent is broker-owned, group-traversable and group-nonwritable")
print()
print("FAILURES:", len(fail))
sys.exit(1 if fail else 0)
CHECK
docker run --rm -i --platform "$platform" --user 65532:65532 --group-add=65534 --cap-drop=ALL \
  --security-opt=no-new-privileges \
  -v "${resource}-etc":/etc/dekopon -v "${resource}-run":/run/dekopon --mount "type=volume,src=${resource}-state,dst=/var/lib/dekopon/broker,volume-subpath=broker" \
  "$python_image" python3 - < "$work/check.py"

# --------------------------------------------------------------------------------------------
# Part 2: the ChatGPT credential, which is seeded once and then owned by the daemon
# --------------------------------------------------------------------------------------------
echo
echo "==> (c) ChatGPT credential enabled: cold start with nothing in the volume"
render_init "$work/init-chatgpt.sh" \
  --set gateway.chatgpt.enabled=true \
  --set gateway.chatgpt.existingSecret=dekopon-chatgpt-auth
reset_mounts
run_init "$work/init-chatgpt.sh"

docker run --rm --platform "$platform" -v "${resource}-state":/var/lib/dekopon "$busybox" \
  sh -c "stat -c '%n  uid=%u gid=%g mode=%a links=%h %F' /var/lib/dekopon/chatgpt /var/lib/dekopon/chatgpt/chatgpt-auth.json"

seeded=$(credential_digest)
source_digest=$(docker run --rm --platform "$platform" -v "${resource}-src":/s "$busybox" \
  sh -c 'sha256sum /s/chatgpt-auth.json | cut -d" " -f1' | tr -d '[:space:]')
assert_eq "cold start seeded the credential from the Secret" "$seeded" "$source_digest"

perms=$(docker run --rm --platform "$platform" -v "${resource}-state":/var/lib/dekopon "$busybox" \
  sh -c "stat -c '%u:%g:%a:%h:%F' /var/lib/dekopon/chatgpt/chatgpt-auth.json")
assert_eq "credential file permissions" "$perms" "65533:65533:600:1:regular file"
dperms=$(docker run --rm --platform "$platform" -v "${resource}-state":/var/lib/dekopon "$busybox" \
  sh -c "stat -c '%u:%a:%F' /var/lib/dekopon/chatgpt")
assert_eq "credential directory permissions" "$dperms" "65533:700:directory"

echo
echo "==> (d) the daemon rotates it: temp sibling + rename, exactly as save_credentials does,"
echo "        running as UID 65533 — this is what the 0700 directory is for"
cat > "$work/rotate.py" <<'ROTATE'
import json, os, stat, sys
path = "/var/lib/dekopon/chatgpt/chatgpt-auth.json"
# save_credentials replaces the extension rather than appending: chatgpt-auth.json -> .tmp-<pid>
temporary = os.path.splitext(path)[0] + f".tmp-{os.getpid()}"
fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, "w") as handle:
    json.dump({"refresh": "ROTATED-REFRESH-TOKEN", "expires_at": 1}, handle)
    handle.write("\n")
    handle.flush()
    os.fsync(handle.fileno())
os.replace(temporary, path)
st = os.lstat(path)
assert st.st_uid == os.geteuid() and (st.st_mode & 0o077) == 0 and st.st_nlink == 1, st
print(f"PASS daemon wrote a temp sibling and renamed it over the target as uid {os.geteuid()}")
ROTATE
docker run --rm -i --platform "$platform" --user 65533:65533 --group-add=65534 --cap-drop=ALL \
  --security-opt=no-new-privileges --mount "type=volume,src=${resource}-state,dst=/var/lib/dekopon/chatgpt,volume-subpath=chatgpt" \
  "$python_image" python3 - < "$work/rotate.py"

rotated=$(credential_digest)
if [ "$rotated" = "$seeded" ]; then
  echo "FAIL rotation did not change the file; the rest of this test would be meaningless" >&2
  exit 1
fi

echo
echo "==> (e) SEED ONCE: restart the pod and assert the rotated credential survives byte-for-byte"
# Everything a restart does: emptyDirs are recreated root-owned 0777, the claim persists.
docker volume rm -f "${resource}-etc" "${resource}-run" >/dev/null
docker volume create "${resource}-etc" >/dev/null
docker volume create "${resource}-run" >/dev/null
docker run --rm --platform "$platform" -v "${resource}-etc":/a -v "${resource}-run":/b "$busybox" \
  sh -c 'chmod 0777 /a /b; chown 0:0 /a /b'
run_init "$work/init-chatgpt.sh"
after_restart=$(credential_digest)
assert_eq "the live credential survived a restart unchanged" "$after_restart" "$rotated"

echo "==> (f) and again after an in-place restart where the emptyDirs also persist"
run_init "$work/init-chatgpt.sh"
after_second=$(credential_digest)
assert_eq "the live credential survived a second restart unchanged" "$after_second" "$rotated"

perms=$(docker run --rm --platform "$platform" -v "${resource}-state":/var/lib/dekopon "$busybox" \
  sh -c "stat -c '%u:%g:%a:%h:%F' /var/lib/dekopon/chatgpt/chatgpt-auth.json")
assert_eq "permissions after the daemon's own write and two restarts" \
  "$perms" "65533:65533:600:1:regular file"

echo
echo "==> (g) the gated re-seed does overwrite"
render_init "$work/init-reseed.sh" \
  --set gateway.chatgpt.enabled=true \
  --set gateway.chatgpt.existingSecret=dekopon-chatgpt-auth \
  --set gateway.chatgpt.reseed=true
run_init "$work/init-reseed.sh"
after_reseed=$(credential_digest)
assert_eq "reseed=true discarded the live credential and restored the seed" \
  "$after_reseed" "$source_digest"

echo
echo "==> (h) provider storage: retained separate claim and broker-only copied key"
render_init "$work/init-storage.sh" \
  --set providerStorage.enabled=true \
  --set providerStorage.existingKeySecret=dekopon-storage-key
reset_mounts
run_init "$work/init-storage.sh"
key_perms=$(docker run --rm --platform "$platform" -v "${resource}-storage-key":/k "$busybox" \
  sh -c "stat -c '%u:%g:%a:%h:%F' /k/storage-key.yaml")
assert_eq "provider namespace key permissions" "$key_perms" "65532:65532:600:1:regular file"
root_perms=$(docker run --rm --platform "$platform" -v "${resource}-storage":/s "$busybox" \
  sh -c "stat -c '%u:%g:%a:%F' /s")
assert_eq "provider storage root permissions" "$root_perms" "65532:65532:700:directory"
# The same key survives a restart copy with identical bytes; provider data is never cleared.
key_before=$(docker run --rm --platform "$platform" -v "${resource}-storage-key":/k "$busybox" \
  sh -c 'sha256sum /k/storage-key.yaml | cut -d" " -f1')
run_init "$work/init-storage.sh"
key_after=$(docker run --rm --platform "$platform" -v "${resource}-storage-key":/k "$busybox" \
  sh -c 'sha256sum /k/storage-key.yaml | cut -d" " -f1')
assert_eq "provider namespace key restart copy is stable" "$key_after" "$key_before"

# Valid gateway render must mount neither privileged storage volume.
helm template dekopon "$chart_dir" -f "$values" \
  --set providerStorage.enabled=true \
  --set providerStorage.existingKeySecret=dekopon-storage-key > "$work/storage-render.yaml"
python_check='import yaml,sys
for d in yaml.safe_load_all(sys.stdin):
  if not d or d.get("kind") != "Deployment": continue
  containers=d["spec"]["template"]["spec"].get("containers",[])
  gateway=next(c for c in containers if c["name"]=="gateway")
  names={m["name"] for m in gateway.get("volumeMounts",[])}
  assert "provider-storage" not in names and "provider-storage-key" not in names, names'
if python3 -c 'import yaml' 2>/dev/null; then
  python3 -c "$python_check" < "$work/storage-render.yaml"
else
  docker run --rm -i -e PROG="$python_check" "$python_image" \
    sh -c 'pip install --quiet --disable-pip-version-check pyyaml >/dev/null 2>&1; exec python3 -c "$PROG"' \
    < "$work/storage-render.yaml"
fi
echo "PASS gateway mounts neither provider storage nor namespace key"

if helm template collision "$chart_dir" \
  --set providerStorage.enabled=true \
  --set providerStorage.existingKeySecret=dekopon-storage-key \
  --set providerStorage.existingClaim=shared \
  --set state.existingClaim=shared >/dev/null 2>&1; then
  echo "FAIL exact audit/provider storage claim collision rendered" >&2
  exit 1
fi
echo "PASS exact audit/provider storage claim collision is rejected"

if helm template overlap "$chart_dir" \
  --set providerStorage.enabled=true \
  --set providerStorage.existingKeySecret=dekopon-storage-key \
  --set providerStorage.keyDir=/etc/dekopon >/dev/null 2>&1; then
  echo "FAIL provider namespace-key mount shadowed the copied configuration mount" >&2
  exit 1
fi
echo "PASS provider storage mount paths cannot shadow chart-owned mounts"

if helm template overlap-source "$chart_dir" \
  --set providerStorage.enabled=true \
  --set providerStorage.existingKeySecret=dekopon-storage-key \
  --set providerStorage.rootPath=/dekopon-source >/dev/null 2>&1; then
  echo "FAIL provider storage root shadowed the projected configuration source" >&2
  exit 1
fi
echo "PASS provider storage paths cannot shadow projected init sources"

if helm template normalized-collision "$chart_dir" \
  --set providerStorage.enabled=true \
  --set providerStorage.existingKeySecret=dekopon-storage-key \
  --set-string 'paths.configDir=/etc//dekopon-storage-key' >/dev/null 2>&1; then
  echo "FAIL a non-canonical chart path bypassed provider-storage mount collision checks" >&2
  exit 1
fi
echo "PASS all chart paths reject repeated-slash aliases before collision comparison"

if helm template packaged-provider-collision "$chart_dir" \
  --set providerStorage.enabled=true \
  --set providerStorage.existingKeySecret=dekopon-storage-key \
  --set providerStorage.rootPath=/opt/dekopon/optional-providers >/dev/null 2>&1; then
  echo "FAIL provider storage shadowed the packaged optional memory provider" >&2
  exit 1
fi
echo "PASS provider storage cannot shadow image-owned provider directories"

if helm template dot-key "$chart_dir" \
  --set providerStorage.enabled=true \
  --set providerStorage.existingKeySecret=dekopon-storage-key \
  --set providerStorage.keyFileName=.. >/dev/null 2>&1; then
  echo "FAIL provider namespace-key destination accepted a parent segment" >&2
  exit 1
fi
echo "PASS provider storage key names reject dot segments"

if helm template unsafe-storage-path "$chart_dir" \
  --set providerStorage.enabled=true \
  --set providerStorage.existingKeySecret=dekopon-storage-key \
  --set-string 'providerStorage.rootPath=/var/lib/bad;touch-bad' >/dev/null 2>&1; then
  echo "FAIL provider storage root accepted shell-active path text" >&2
  exit 1
fi
echo "PASS provider storage paths are safe shell and mount segments"

claim_perms=$(docker run --rm --platform "$platform" -v "${resource}-state":/s "$busybox" \
  sh -c "stat -c '%u:%g:%a:%F' /s")
assert_eq "neither daemon owns or traverses the claim root" "$claim_perms" "0:0:700:directory"

echo "==> distinct-UID processes against the rendered permission layout"
docker run --rm -i --platform "$platform" --user 0:0 \
  --cap-drop=ALL --cap-add=SETUID --cap-add=SETGID \
  --security-opt=no-new-privileges --read-only \
  -v "${resource}-etc":/etc/dekopon -v "${resource}-gateway-etc":/etc/dekopon-gateway \
  -v "${resource}-run":/run/dekopon \
  --mount "type=volume,src=${resource}-state,dst=/broker-state,volume-subpath=broker" \
  --mount "type=volume,src=${resource}-state,dst=/gateway-state,volume-subpath=chatgpt" \
  -v "${resource}-storage":/var/lib/dekopon-provider-storage \
  -v "${resource}-storage-key":/etc/dekopon-storage-key \
  "$python_image" python3 - < "$chart_dir/ci/check-ipc-layout.py"

echo "==> refuse old state layout without modifying its bytes"
on_state <<'OLD'
printf 'old-state-sentinel' > /var/lib/dekopon/audit.jsonl
OLD
if run_init "$work/init-storage.sh" > "$work/old-layout.log" 2>&1; then
  echo "FAIL unmigrated state claim accepted" >&2
  exit 1
fi
grep -q "move broker state" "$work/old-layout.log"
old=$(on_state <<'OLD'
cat /var/lib/dekopon/audit.jsonl
OLD
)
assert_eq "unmigrated state refused without overwriting" "$old" "old-state-sentinel"

# Missing/mismatched inline server pins cannot fall back to the gateway euid.
for pin in absent 65533; do
  python3 - "$values" "$work/bad-pin.yaml" "$pin" <<'PIN'
import sys
text = open(sys.argv[1]).read()
text = text.replace("        serverUid: 65532\n", "" if sys.argv[3] == "absent" else "        serverUid: 65533\n")
open(sys.argv[2], "w").write(text)
PIN
  if helm template bad-pin "$chart_dir" -f "$work/bad-pin.yaml" > /dev/null 2> "$work/pin.err"; then
    echo "FAIL invalid gateway server pin rendered" >&2
    exit 1
  fi
  grep -q 'explicitly pin broker.serverUid: 65532' "$work/pin.err"
  echo "PASS refused $pin gateway server pin"
done

# Changing one UID or widening credential groups cannot silently remove the boundary.
for argument in podSecurityContext.runAsUser=65533 podSecurityContext.runAsGroup=65533 \
  podSecurityContext.fsGroup=65534 'podSecurityContext.supplementalGroups[0]=65532' \
  gateway.chatgpt.subdir=broker; do
  if helm template unsafe "$chart_dir" -f "$values" \
    --set gateway.chatgpt.enabled=true --set gateway.chatgpt.existingSecret=seed \
    --set "$argument" > /dev/null 2> "$work/refused.err"; then
    echo "FAIL unsafe isolation option rendered: $argument" >&2
    exit 1
  fi
  echo "PASS refused $argument"
done

echo
echo "OK: every tier satisfied; ChatGPT is seed-once; provider storage/key are retained, separate, and broker-only."
