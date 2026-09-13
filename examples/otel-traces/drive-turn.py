#!/usr/bin/env python3
"""Real daemon fixture; only the OpenAI-compatible model is a loopback stand-in.

Configuration, policy, bash response and local request follow
crates/dekopond/tests/gateway.rs broker_config/gateway_config/ask/boot_in_with_scope.
"""
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import stat
import subprocess
import sys
import threading
import time

PAYLOAD = "DEKOPON_OTEL_SMOKE_INPUT_MUST_APPEAR"
CREDENTIAL = "DEKOPON_OTEL_SMOKE_CREDENTIAL_MUST_NOT_APPEAR"
ANSWER = "The authorized probe completed."


def main():
    root, directory, endpoint, service = sys.argv[1:]
    root, directory = Path(root), Path(directory).resolve()
    processes = []
    logs = []
    calls = []
    failures = []

    class Model(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass  # No HTTP headers or request payloads in diagnostics.

        def do_POST(self):
            try:
                self.connection.settimeout(10)
                assert self.path == "/v1/chat/completions", "unexpected model path"
                assert self.headers.get("Authorization") == "Bearer " + CREDENTIAL, "missing fixture credential"
                size = int(self.headers.get("Content-Length", "0"))
                assert 0 < size <= 1048576, "model request bound"
                request = json.loads(self.rfile.read(size))
                assert CREDENTIAL not in json.dumps(request), "credential entered model payload"
                calls.append(1)
                if len(calls) == 1:
                    assert any(m.get("role") == "user" and PAYLOAD in m.get("content", "")
                               for m in request["messages"]), "missing local request"
                    message = {"role": "assistant", "content": None, "tool_calls": [{
                        "id": "probe-call", "type": "function", "function": {
                            "name": "bash", "arguments": json.dumps({
                                "script": f'probe upper --text "{PAYLOAD}" | jq -r .text'})}}]}
                else:
                    assert len(calls) == 2, "extra model call"
                    assert any(m.get("role") == "tool" and PAYLOAD in m.get("content", "")
                               for m in request["messages"]), "real probe output missing"
                    message = {"role": "assistant", "content": ANSWER, "tool_calls": []}
                body = json.dumps({"choices": [{"message": message}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            except Exception as error:
                failures.append(type(error).__name__)
                self.send_error(500, "model fixture rejected request")

    model = http.server.HTTPServer(("127.0.0.1", 0), Model)
    model.timeout = 1
    thread = threading.Thread(target=model.serve_forever, daemon=True)
    thread.start()

    def write(name, value):
        path = directory / name
        path.write_text(json.dumps(value) if not isinstance(value, str) else value)
        path.chmod(0o600)
        return str(path)

    def start(binary, config, extra_env=None):
        # Deliberately do not inherit ambient model/provider credentials or proxy settings.
        env = {"PATH": os.defpath, "HOME": str(directory), "RUST_LOG": "info,wasmtime::runtime::code_memory=debug",
               "OTEL_EXPORTER_OTLP_HEADERS": os.environ["OTEL_EXPORTER_OTLP_HEADERS"]}
        env.update(extra_env or {})
        log = open(directory / (binary + ".log"), "wb")
        stderr = open(directory / (binary + ".stderr.log"), "wb")
        logs.extend([log, stderr])
        process = subprocess.Popen([str(root / "target/debug" / binary), "--config", config],
                                   env=env, stdout=log, stderr=stderr)
        processes.append(process)
        return process

    def ready(name, process):
        path = directory / name
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            assert process.poll() is None, "daemon exited before readiness"
            if path.exists():
                mode = path.lstat()
                assert stat.S_ISSOCK(mode.st_mode) and stat.S_IMODE(mode.st_mode) == 0o600, "socket not private"
                assert mode.st_uid == os.geteuid(), "socket owner mismatch"
                return path
            time.sleep(0.1)
        raise TimeoutError("daemon readiness deadline")

    def stop(process):
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=25)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
                raise TimeoutError("daemon shutdown deadline")
        assert process.returncode == 0, "daemon failed"

    def interrupted(_signum, _frame):
        raise RuntimeError("fixture interrupted")

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    signal.signal(signal.SIGALRM, interrupted)
    signal.alarm(210)
    try:
        telemetry = {"endpoint": endpoint, "transport": "http", "serviceName": service,
                     "exportTimeoutMs": 15000}
        policy = '''permit(principal == Dekopon::Principal::"smoke-user",
 action == Dekopon::Action::"agent.prompt", resource == Dekopon::Agent::"chat-agent")
 when { context has via && context.via == "dekopond-gateway" };
permit(principal == Dekopon::Principal::"smoke-user",
 action == Dekopon::Action::"cli-probe.upper", resource == Dekopon::Provider::"cli-probe")
 when { context has via && context.via == "dekopond-gateway"
 && context has agent && context.agent == "chat-agent" };
'''
        broker_config = write("broker.json", {
            "apiVersion": "dekopon.dev/brokerd/v1alpha1",
            "socketPath": str(directory / "broker.sock"),
            "brokerPrincipal": "broker-smoke", "policyRevision": "policy-smoke",
            "policiesPath": write("policies.cedar", policy),
            "providers": [str(root / "examples/providers/cli-probe-provider.wasm")],
            "identities": [{"uid": os.geteuid(), "principal": "dekopond-gateway",
                "actor": {"type": "service", "principal": "dekopond-gateway"},
                "attestor": {"namespaces": ["tel"], "chatScopes": [{
                    "kind": "local", "transport": "dev",
                    "conversation": {"kind": "any", "ids": ["dev"]},
                    "localSubjectService": "tel"}]}}],
            "identityMappings": [{"subject": "tel.16034700182", "principal": "smoke-user"}],
            "constraintSets": {"cli-probe.upper": {"provider": "cli-probe", "effect": "read-only",
                "risk": "Low",
                "constraints": {"timeoutMs": 30000, "maxOutputBytes": 1048576}}},
            "telemetry": telemetry})
        broker = start("dekopon-brokerd", broker_config)
        ready("broker.sock", broker)
        catalog = write("catalog.json", {"apiVersion": "dekopon.dev/v1alpha1", "kind": "Agent",
            "metadata": {"name": "chat-agent"}, "spec": {"description": "Smoke agent",
                "enabled": True, "instructions": "Use the authorized probe.", "modelClass": "reasoning"}})
        gateway_config = write("gateway.json", {
            "apiVersion": "dekopon.dev/dekopond/v1alpha1", "catalogPath": catalog,
            "broker": {"socketPath": str(directory / "broker.sock"), "serverUid": os.geteuid()},
            "transports": [{"name": "dev", "kind": "local", "socketPath": str(directory / "dev.sock")}],
            # The stub answers one JSON completion and ignores `stream`, which is exactly the
            # endpoint `stream: false` exists for; the default asks for an event stream.
            "models": [{"name": "stub", "kind": "openaiCompatible", "model": "smoke-model",
                "endpoint": f"http://127.0.0.1:{model.server_port}/v1", "apiKeyEnv": "SMOKE_MODEL_KEY",
                "timeoutMs": 15000, "classes": ["reasoning"], "stream": False}],
            "routes": [{"transport": "dev", "conversation": {"kind": ["directMessage"]},
                "agent": "chat-agent",
                "limits": {"maxSteps": 4, "maxCapabilityCalls": 4}}],
            "sessions": {"maxConcurrent": 1}, "shutdownGraceMs": 15000, "telemetry": telemetry})
        gateway = start("dekopond", gateway_config, {"SMOKE_MODEL_KEY": CREDENTIAL})
        path = ready("dev.sock", gateway)
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(60)
            client.connect(str(path))
            client.sendall((json.dumps({"subject": "tel.16034700182",
                "conversation": {"kind": "directMessage", "id": "dev"},
                "text": PAYLOAD}) + "\n").encode())
            with client.makefile("rb") as reader:
                line = reader.readline(65537)
            assert len(line) <= 65536 and line.endswith(b"\n"), "response line bound"
            assert json.loads(line)["reply"] == ANSWER, "unexpected local reply"
        assert len(calls) == 2 and not failures, "model exchange failed"
        stop(gateway)
        stop(broker)
        # The audit record is the broker's own stdout JSON line, whatever the exporter did with it.
        records = [json.loads(line) for line in (directory / "dekopon-brokerd.log").read_text().splitlines()]
        assert any(r.get("audit.event") == "broker.execution" and r.get("capability.id") == "cli-probe.upper"
                   and r.get("outcome") == "Succeeded" and r.get("principal") == "smoke-user"
                   for r in records), "authorized probe execution missing from broker audit"
        print("Real broker + gateway: private local turn, two model calls, authorized probe audit verified")
    finally:
        signal.alarm(0)
        for process in reversed(processes):
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
        model.shutdown()
        model.server_close()
        thread.join(timeout=12)
        for log in logs:
            log.close()


if __name__ == "__main__":
    main()
