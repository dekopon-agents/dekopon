#!/usr/bin/env bash
# Prove the init container produces files dekopon-brokerd and dekopond will accept.
#
# A rendered manifest that looks right is not the same as a file that survives O_NOFOLLOW plus an
# owner and mode check, so this renders the chart, pulls the init container's *actual* command out
# of the manifest, and runs it verbatim in a linux/arm64 container under the securityContext the
# chart renders, against a fixture built to match a Kubernetes projected volume exactly:
# real files under ..<timestamp>/, ..data -> ..<timestamp>, and key -> ..data/key.
#
# It then re-runs the daemons' own checks as UID 65532 and asserts every tier.
#
# Requires: helm, docker (with linux/arm64 emulation or an arm64 host), python3.
#
#   charts/dekopon/ci/verify-init-permissions.sh
set -euo pipefail

chart_dir=$(cd "$(dirname "$0")/.." && pwd)
values="$chart_dir/ci/rubber-stamper-values.yaml"
work=$(mktemp -d)
trap 'rm -rf "$work"; docker volume rm -f dkv-src dkv-etc dkv-run dkv-state >/dev/null 2>&1 || true' EXIT

platform=${PLATFORM:-linux/arm64}
busybox=busybox@sha256:fc6dddc4c44b1bfe37f41cae8e67d1693828e8f42a91862816d7953e2c9d3f23
python_image=python:3.13-alpine

echo "==> rendering the chart"
helm template dekopon "$chart_dir" -f "$values" > "$work/render.yaml"

echo "==> extracting the init container's command from the rendered manifest"
extract='import yaml,sys
docs=[d for d in yaml.safe_load_all(sys.stdin) if d]
dep=[d for d in docs if d["kind"]=="Deployment"][0]
ic=dep["spec"]["template"]["spec"]["initContainers"][0]
assert ic["name"]=="prepare-files", ic["name"]
sys.stdout.write(ic["args"][0])'
if python3 -c 'import yaml' 2>/dev/null; then
  python3 -c "$extract" < "$work/render.yaml" > "$work/init.sh"
else
  docker run --rm -i -e PROG="$extract" "$python_image" \
    sh -c 'pip install --quiet --disable-pip-version-check pyyaml >/dev/null 2>&1; exec python3 -c "$PROG"' \
    < "$work/render.yaml" > "$work/init.sh"
fi

for v in dkv-src dkv-etc dkv-run dkv-state; do
  docker volume rm -f "$v" >/dev/null 2>&1 || true
  docker volume create "$v" >/dev/null
done

echo "==> building a projected-volume fixture (symlink farm, root-owned, 0400)"
docker run --rm -i --platform "$platform" -v dkv-src:/dekopon-source "$busybox" sh -s <<'FIXTURE'
set -eu
cd /dekopon-source
stamp=..2026_08_18_00_00_00.000000000
mkdir -p "$stamp"
printf 'apiVersion: dekopon.dev/brokerd/v1alpha1\n' > "$stamp/broker.yaml"
printf '@id("x") permit(principal, action, resource);\n' > "$stamp/policies.cedar"
printf 'apiVersion: dekopon.dev/broker-credentials/v1alpha1\ncredentials: []\n' > "$stamp/broker-credentials.yaml"
printf 'apiVersion: dekopon.dev/dekopond/v1alpha1\n' > "$stamp/dekopond.yaml"
chmod 0400 "$stamp"/*
ln -sfn "$stamp" ..data
for k in broker.yaml policies.cedar broker-credentials.yaml dekopond.yaml; do ln -sfn "..data/$k" "$k"; done
chmod 0755 /dekopon-source
FIXTURE

reset_mounts() {
  docker run --rm --platform "$platform" -v dkv-etc:/a -v dkv-run:/b -v dkv-state:/c "$busybox" \
    sh -c 'chmod 0777 /a /b /c; chown 0:0 /a /b /c'
}

run_init() {
  # The rendered securityContext: root, everything dropped except CHOWN and FOWNER, no new
  # privileges, read-only root filesystem.
  docker run --rm --platform "$platform" \
    --user 0:0 --cap-drop=ALL --cap-add=CHOWN --cap-add=FOWNER \
    --security-opt=no-new-privileges --read-only \
    -v dkv-src:/dekopon-source:ro -v dkv-etc:/etc/dekopon -v dkv-run:/run/dekopon -v dkv-state:/var/lib/dekopon \
    "$busybox" /bin/sh -c "$(cat "$work/init.sh")"
}

echo "==> (a) cold start"
reset_mounts
run_init

echo "==> (b) in-place restart: the emptyDirs still hold the previous run's 0700 directories"
run_init

echo "==> stat of the result"
docker run --rm --platform "$platform" \
  -v dkv-etc:/etc/dekopon -v dkv-run:/run/dekopon -v dkv-state:/var/lib/dekopon "$busybox" \
  sh -c "stat -c '%n  uid=%u gid=%g mode=%a links=%h %F' /etc/dekopon/* /etc/dekopon /run/dekopon /var/lib/dekopon"

echo "==> the daemons' own checks, as UID 65532"
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
print("== Tier B: broker.yaml, policies.cedar, dekopond.yaml (mode & 0o022) ==")
for f in ("broker.yaml", "policies.cedar", "dekopond.yaml"):
    owned_file(f"/etc/dekopon/{f}", 0o022, "config")
print("== Tier C and D: socket, audit, checkpoint and lock parents, and every ancestor ==")
for p in ("/run/dekopon/broker.sock", "/var/lib/dekopon/audit.jsonl",
          "/var/lib/dekopon/audit-checkpoint.json", "/var/lib/dekopon/audit-checkpoint.lock"):
    private_parent(p)
print("== what the broker creates for itself ==")
fd = os.open("/var/lib/dekopon/audit.jsonl",
             os.O_RDWR | os.O_CREAT | os.O_APPEND | os.O_NOFOLLOW, 0o600)
st = os.fstat(fd)
os.close(fd)
ok((st.st_mode & 0o077) == 0 and st.st_nlink == 1 and st.st_uid == euid,
   f"audit.jsonl mode={oct(st.st_mode & 0o7777)} uid={st.st_uid} nlink={st.st_nlink}")
import socket as S
s = S.socket(S.AF_UNIX, S.SOCK_STREAM)
s.bind("/run/dekopon/broker.sock")
os.chmod("/run/dekopon/broker.sock", 0o600)
st = os.lstat("/run/dekopon/broker.sock")
ok(stat.S_ISSOCK(st.st_mode) and st.st_uid == euid
   and (st.st_mode & 0o077) == 0 and st.st_nlink == 1,
   f"broker.sock mode={oct(st.st_mode & 0o7777)} uid={st.st_uid} nlink={st.st_nlink}")
s.close()
print()
print("FAILURES:", len(fail))
sys.exit(1 if fail else 0)
CHECK
docker run --rm -i --platform "$platform" --user 65532:65532 --cap-drop=ALL \
  --security-opt=no-new-privileges \
  -v dkv-etc:/etc/dekopon -v dkv-run:/run/dekopon -v dkv-state:/var/lib/dekopon \
  "$python_image" python3 - < "$work/check.py"

echo
echo "OK: every tier satisfied by the rendered init container."
