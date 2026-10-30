#!/usr/bin/env python3

import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time


VERSION = "0.72.0"
PROVIDER_SHA256 = "64914f342a367129f69f857c78871e37c967dfb8eff8215c047899007e326462"
SUBJECT = "slack.t0123abc.u0123abc"
REPLY = "Read-only provider call passed."


def private_json(path, value):
    path.write_text(json.dumps(value), encoding="utf-8")
    path.chmod(0o600)


def configure(root, port):
    private_json(root / "broker.yaml", {
        "apiVersion": "dekopon.dev/brokerd/v1alpha1",
        "socketPath": str(root / "broker.sock"),
        "policiesPath": "policies.cedar",
        "strict": True,
        "providers": ["provider.wasm"],
        "identities": [{
            "uid": os.getuid(), "principal": "dekopon-gatewayd",
            "actor": {"type": "service", "principal": "dekopon-gatewayd"},
            "attestor": {"namespaces": ["slack.t0123abc"]},
        }],
        "principals": {"operator": {"subjects": [SUBJECT]}},
        "providerSettings": {"jsonplaceholder": {"baseUrl": f"http://127.0.0.1:{port}"}},
        "capabilities": {"jsonplaceholder": {"capabilities": {
            "jsonplaceholder.posts.get": {"constraints": {
                "timeoutMs": 10000,
                "http": {
                    "allowedHosts": [f"127.0.0.1:{port}"], "allowedMethods": ["GET"],
                    "maxRequests": 1, "maxRequestBytes": 8192, "maxResponseBytes": 65536,
                    "allowPlaintextLoopback": True,
                },
            }},
        }}},
    })
    private_json(root / "gateway.yaml", {
        "apiVersion": "dekopon.dev/gatewayd/v1alpha1",
        "catalogPath": "agents.yaml",
        "broker": {"socketPath": str(root / "broker.sock"), "serverUid": os.getuid()},
        "transports": [{"name": "local", "kind": "local", "socketPath": str(root / "gateway.sock")}],
        "models": [{
            "name": "stub", "kind": "openaiCompatible", "model": "stub",
            "endpoint": f"http://127.0.0.1:{port}/v1", "apiKeyEnv": "SMOKE_MODEL_KEY",
            "timeoutMs": 10000, "classes": ["general"], "stream": False,
        }],
        "routes": [{
            "transport": "local", "conversation": {"kind": ["directMessage"]},
            "subjects": [SUBJECT], "agent": "smoke", "model": "stub",
            "limits": {"maxSteps": 4, "maxCapabilityCalls": 1, "maxDurationMs": 30000},
            "memory": {"mode": "oneShot"},
        }],
        "sessions": {"maxConcurrent": 1, "maxConversations": 1, "assetRetentionBytes": 0},
        "shutdownGraceMs": 10000,
    })
    private_json(root / "agents.yaml", {
        "apiVersion": "dekopon.dev/v1alpha1", "kind": "Agent", "metadata": {"name": "smoke"},
        "spec": {"description": "Local compatibility fixture", "enabled": True,
                 "instructions": "Read post 7 using the granted provider.", "modelClass": "general"},
        "status": "Ready",
    })
    (root / "policies.cedar").write_text('''
@id("smoke-prompt")
permit(principal == Dekopon::Principal::"operator",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"smoke")
when { context.via == "dekopon-gatewayd" };
@id("smoke-post-read")
permit(principal == Dekopon::Principal::"operator",
       action == Dekopon::Action::"jsonplaceholder.posts.get",
       resource == Dekopon::Provider::"jsonplaceholder")
when { context.agent == "smoke" && context.via == "dekopon-gatewayd" };
''', encoding="utf-8")
    (root / "policies.cedar").chmod(0o600)


def fixture_handler(requests, failures):
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def respond(self, value, status=200):
            data = json.dumps(value).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            requests.append("provider")
            if self.path != "/posts/7" or self.headers.get("Authorization"):
                failures.append(f"Unexpected provider request: {self.path}")
                self.respond({"error": failures[-1]}, 400)
                return
            self.respond({"userId": 1, "id": 7, "title": "bounded-test-post", "body": "loopback-fixture"})

        def do_POST(self):
            requests.append("model")
            data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path != "/v1/chat/completions" or self.headers.get("Authorization") != "Bearer fake-local-only":
                failures.append("Unexpected model endpoint or fake credential")
                self.respond({"error": failures[-1]}, 400)
                return
            if requests == ["model"]:
                message = {"role": "assistant", "content": None, "tool_calls": [{
                    "id": "call-1", "type": "function", "function": {
                        "name": "bash", "arguments": json.dumps({"command": "placeholder posts get --post-id 7"}),
                    },
                }]}
            elif requests == ["model", "provider", "model"] and any(
                m.get("role") == "tool" and "bounded-test-post" in str(m.get("content", ""))
                for m in data["messages"]
            ):
                message = {"role": "assistant", "content": REPLY, "tool_calls": []}
            else:
                failures.append("Model did not receive the expected provider result")
                self.respond({"error": failures[-1]}, 400)
                return
            self.respond({"choices": [{"message": message}], "usage": {
                "prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120,
            }})
    return Handler


def wait_for_socket(path, process):
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"{process.args[0]} exited with {process.returncode}")
        if path.exists():
            return
        time.sleep(0.1)
    raise RuntimeError(f"Timed out waiting for {path.name}")


def smoke(root, bin_dir, provider):
    component = provider.read_bytes()
    if hashlib.sha256(component).hexdigest() != PROVIDER_SHA256:
        raise RuntimeError("Expected the published JSONPlaceholder v0.7.0 component checksum")
    (root / "provider.wasm").write_bytes(component)
    (root / "provider.wasm").chmod(0o600)
    env = {"PATH": "/usr/bin:/bin", "TMPDIR": str(root), "RUST_LOG": "info", "SMOKE_MODEL_KEY": "fake-local-only"}
    binaries = {name: bin_dir / f"dekopon-{name}d" for name in ("broker", "gateway")}
    for binary in binaries.values():
        version = subprocess.check_output([str(binary), "--version"], env=env, text=True, timeout=10).strip()
        if version != f"{binary.name} {VERSION}":
            raise RuntimeError(f"Unexpected installed version: {version}")
        print(f"Binary: {binary} ({version})", flush=True)
    requests, failures, processes, logs = [], [], [], []
    server = http.server.HTTPServer(("127.0.0.1", 0), fixture_handler(requests, failures))
    thread = threading.Thread(target=server.serve_forever)
    thread.start()
    turn_completed = False
    try:
        configure(root, server.server_port)
        for name, binary in binaries.items():
            subprocess.run([str(binary), "check", str(root / f"{name}.yaml")],
                           cwd=root, env={**env, "RUST_LOG": "warn"}, check=True, timeout=60)
        for name, binary in binaries.items():
            log = (root / f"{name}.log").open("wb")
            logs.append(log)
            sizing = ["--worker-threads", "2", "--blocking-threads", "8"]
            if name == "broker":
                sizing += ["--cpu-threads", "2"]
            process = subprocess.Popen([str(binary), *sizing, "--config", str(root / f"{name}.yaml")],
                                       cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT)
            processes.append(process)
            wait_for_socket(root / f"{name}.sock", process)
            if name == "broker":
                subprocess.run([str(binary), "probe", "--socket", str(root / "broker.sock")],
                               cwd=root, env=env, check=True, timeout=10)
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(45)
            client.connect(str(root / "gateway.sock"))
            client.sendall((json.dumps({"subject": SUBJECT, "conversation": {
                "kind": "directMessage", "id": "fixture",
            }, "text": "Read post 7."}) + "\n").encode())
            with client.makefile("rb") as response:
                reply = json.loads(response.readline(65536))
        if reply != {"reply": REPLY} or requests != ["model", "provider", "model"] or failures:
            raise RuntimeError(f"Unexpected turn: {reply!r}, calls: {requests!r}, failures: {failures!r}")
        turn_completed = True
    finally:
        for process in reversed(processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
        for log in logs:
            log.close()
        if not turn_completed or any(process.returncode != 0 for process in processes):
            for name in binaries:
                path = root / f"{name}.log"
                if path.exists():
                    print(f"--- {name} log ---\n{path.read_text()}", flush=True)
    if [process.returncode for process in processes] != [0, 0]:
        raise RuntimeError(f"Daemons did not shut down cleanly: {[p.returncode for p in processes]}")
    executions = [json.loads(line) for line in (root / "broker.log").read_text().splitlines()]
    successes = [event for event in executions if event.get("audit.event") == "broker.execution"
                 and event.get("outcome") == "Succeeded"
                 and event.get("capability.id") == "jsonplaceholder.posts.get"]
    if len(successes) != 1 or successes[0].get("provider.artifact_sha256") != PROVIDER_SHA256:
        raise RuntimeError("Missing the successful broker audit event for the pinned provider")
    print("PASS: configuration checks, broker probe, two model calls, one authorized provider GET, final reply and clean shutdown.")
    print("Scope: same-UID process smoke with local fixtures; no launchd, separate-user isolation or live service acceptance.")


def main():
    parser = argparse.ArgumentParser(description="Exercise released daemons and a pinned Wasm provider with local fixtures.")
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--provider", type=Path, required=True)
    args = parser.parse_args()
    # macOS TMPDIR paths can exceed sockaddr_un; resolve /tmp's symlink before protected-file checks.
    with tempfile.TemporaryDirectory(prefix="deko-mac-", dir=Path("/tmp").resolve()) as directory:
        smoke(Path(directory), args.bin_dir.resolve(strict=True), args.provider.resolve(strict=True))


if __name__ == "__main__":
    main()
