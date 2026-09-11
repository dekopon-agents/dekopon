"""Fault-inject the actual init-test allocation/cleanup control flow; no real Docker.

A PATH-local Docker stub records simulated allocation before returning an error or
signalling its caller. Only the script prefix before fixture setup is executed;
its resource tracking, allocation loop and EXIT/signal traps are unchanged.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

script = Path(sys.argv[1]).resolve()
source = script.read_text()
marker = '\necho "==> building a projected-volume fixture (symlink farm, root-owned, 0400)"\n'
assert source.count(marker) == 1, "fixture setup marker changed"
prefix = source.split(marker, 1)[0]

stub = """#!/usr/bin/env python3
import json, os, pathlib, signal, sys
root = pathlib.Path(os.environ['CLEANUP_FIXTURE'])
args = sys.argv[1:]
assert len(args) == 3 and args[0] == 'volume', args
operation, name = args[1:]
assert name.startswith('dekopon-init-') and '/' not in name, name
path = root / 'objects' / name
with (root / 'calls.jsonl').open('a') as log:
    log.write(json.dumps([operation, name]) + '\\n')
mode = os.environ['CLEANUP_FAULT']
if operation == 'inspect':
    if mode == 'preexisting':
        path.write_text('preexisting-sentinel')
    sys.exit(0 if path.exists() else 1)
if operation == 'create':
    assert not path.exists(), name
    path.write_text('allocated')
    if mode == 'failure':
        sys.exit(42)
    if mode == 'term':
        os.kill(os.getppid(), signal.SIGTERM)
elif operation == 'rm':
    assert path.read_text() == 'allocated', 'must never remove preexisting objects'
    path.unlink()
else:
    raise AssertionError(args)
"""

for mode, expected_status in (("failure", 42), ("term", 143), ("preexisting", 1), ("success", 0)):
    with tempfile.TemporaryDirectory(prefix="init-cleanup-", dir=os.environ.get("TMPDIR")) as directory:
        root = Path(directory)
        (root / "bin").mkdir()
        (root / "objects").mkdir()
        (root / "temporary").mkdir()
        docker = root / "bin" / "docker"
        docker.write_text(stub)
        docker.chmod(0o700)
        env = dict(os.environ, PATH=str(root / "bin") + os.pathsep + os.environ["PATH"],
                   TMPDIR=str(root / "temporary"), CLEANUP_FIXTURE=str(root), CLEANUP_FAULT=mode)
        result = subprocess.run(["bash", "-c", prefix, str(script)], env=env,
                                capture_output=True, text=True, timeout=10)
        assert result.returncode == expected_status, (mode, result.returncode, result.stderr)
        calls = [json.loads(line) for line in (root / "calls.jsonl").read_text().splitlines()]
        created = [name for operation, name in calls if operation == "create"]
        removed = [name for operation, name in calls if operation == "rm"]
        assert removed == created, (mode, created, removed)
        assert not list((root / "temporary").iterdir()), "EXIT must also remove its work directory"
        remaining = list((root / "objects").iterdir())
        if mode == "preexisting":
            assert not created and not removed, calls
            assert len(remaining) == 1 and remaining[0].read_text() == "preexisting-sentinel"
            assert "refusing preexisting test volume" in result.stderr
        else:
            assert len(created) == (6 if mode == "success" else 1), calls
            assert not remaining, (mode, remaining)
        print(f"PASS init cleanup {mode}: exit={expected_status}, allocated={len(created)}, removed={len(removed)}")
