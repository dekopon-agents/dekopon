#!/usr/bin/env python3

import argparse
from contextlib import contextmanager, ExitStack
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import uuid

from model_fixture import bash_call, completion, latest_user_lines, read_messages
from slack_socket_fixture import SlackSocketFixture

APP_TOKEN = "xapp-fixture-app"
BOT_TOKEN = "xoxb-fixture-bot"
MODEL_KEY = "fixture-model-key"
USER = "U0123ABC"
TEAM = SlackSocketFixture.TEAM.lower()
CHANNEL = SlackSocketFixture.CHANNEL
SUBJECT = f"drn:slack:user:{TEAM}:{USER.lower()}"
CAPABILITY = "http-probe.fetch"
PROBE_BODY = "fixture-provider-response"


class RollbackFailure(RuntimeError):
    pass


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
    path.chmod(0o600)


def contains_credential(value):
    return any(secret in value for secret in (APP_TOKEN, BOT_TOKEN, MODEL_KEY))


@contextmanager
def serve_model(markers, calls, failures):
    class Handler(http.server.BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(10)

        def log_message(self, *_args):
            pass

        def respond(self, body, status=200):
            data = json.dumps(body).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            try:
                if self.path != "/probe" or self.headers.get_all("Authorization"):
                    raise ValueError("Unexpected provider request")
                if contains_credential(str(self.headers)) or len(calls) >= 8:
                    raise ValueError("Unexpected provider headers or request count")
                calls.append({"provider_path": self.path})
                self.respond({"probe": PROBE_BODY})
            except (ValueError, OSError) as error:
                failures.append(str(error))

        def do_POST(self):
            try:
                messages = read_messages(self, MODEL_KEY, contains_credential)
                index, lines = latest_user_lines(messages)
                matched = [line for line in lines if line in markers]
                if len(matched) != 1 or len(calls) >= 8:
                    raise ValueError("Unexpected model prompt or request count")
                marker = matched[0]
                tools = [message for message in messages[index + 1:] if message["role"] == "tool"]
                calls.append({"marker": marker, "tool_result": bool(tools)})
                if not tools:
                    message = bash_call(f"httpprobe fetch --uri http://127.0.0.1:{self.server.server_port}/probe")
                elif PROBE_BODY in json.dumps(tools):
                    message = {"role": "assistant", "content": markers[marker], "tool_calls": []}
                else:
                    raise ValueError("Missing loopback provider result")
                self.respond(completion(message))
            except (ValueError, KeyError, IndexError, StopIteration, TypeError, OSError) as error:
                failures.append(str(error))
                self.close_connection = True

    with http.server.HTTPServer(("127.0.0.1", 0), Handler) as server:
        worker = threading.Thread(target=server.serve_forever)
        worker.start()
        try:
            yield server.server_port
        finally:
            server.shutdown()
            worker.join()


def configure(root, slack_endpoint, port):
    host = f"127.0.0.1:{port}"
    socket_path = str(root / "broker.sock")
    write_json(root / "broker.yaml", {
        "apiVersion": "dekopon.dev/brokerd/v1alpha1",
        "socketPath": socket_path, "policiesPath": "policies.cedar", "strict": True,
        "providers": ["provider.wasm"],
        "identities": [{
            "uid": os.getuid(), "principal": "dekopon-gatewayd",
            "actor": {"type": "service", "principal": "dekopon-gatewayd"},
            "attestor": {"namespaces": [f"drn:slack:user:{TEAM}:*"]},
        }],
        "principals": {"fixture-user": {"subjects": [SUBJECT]}},
        "http": {"plaintextHosts": ["127.0.0.1"]},
        "capabilities": {"http-probe": {"capabilities": {CAPABILITY: {"constraints": {
            "timeoutMs": 10000, "http": {
                "allowedHosts": [host], "allowedMethods": ["GET"], "maxRequests": 1,
                "maxRequestBytes": 8192, "maxResponseBytes": 65536, "allowPlaintextLoopback": True,
                "requestTemplates": [{"method": "GET", "path": "/probe"}],
            },
        }}}}},
    })
    write_json(root / "gateway.yaml", {
        "apiVersion": "dekopon.dev/gatewayd/v1alpha1",
        "catalogPath": "agents.yaml",
        "broker": {"socketPath": socket_path, "serverUid": os.getuid()},
        "transports": [{
            "name": "slack", "kind": "slackSocketMode", "endpoint": slack_endpoint,
            "experience": "agent", "appTokenEnv": "FIXTURE_SLACK_APP_TOKEN",
            "botTokenEnv": "FIXTURE_SLACK_BOT_TOKEN",
            "indicators": {"mode": "native", "progress": "auto"},
        }],
        "models": [{
            "name": "stub", "kind": "openaiCompatible", "model": "stub",
            "endpoint": f"http://{host}/v1", "apiKeyEnv": "FIXTURE_MODEL_KEY",
            "timeoutMs": 10000, "classes": ["general"], "stream": False,
        }],
        "routes": [{
            "transport": "slack",
            "conversation": {"kind": ["channel", "thread"], "container": TEAM, "ids": [CHANNEL.lower()]},
            "agent": "fixture", "model": "stub",
            "limits": {"maxSteps": 4, "maxCapabilityCalls": 1, "maxDurationMs": 30000},
            "memory": {"mode": "persistent", "recall": "journal", "summaries": "off"},
        }],
        "sessions": {"journal": {"path": "journal"}, "assets": {"path": "assets"},
                     "maxConcurrent": 4, "maxConversations": 16},
        "shutdownGraceMs": 10000,
    })
    write_json(root / "agents.yaml", {
        "apiVersion": "dekopon.dev/v1alpha1", "kind": "Agent", "metadata": {"name": "fixture"},
        "spec": {"description": "Local rollback fixture", "enabled": True,
                 "instructions": "Read the loopback probe.", "modelClass": "general"},
        "status": "Ready",
    })
    policy = root / "policies.cedar"
    policy.write_text('''
@id("fixture-prompt")
permit(principal == Dekopon::Principal::"fixture-user",
       action == Dekopon::Action::"agent.prompt", resource == Dekopon::Agent::"fixture")
when { context.via == "dekopon-gatewayd" && context has transport && context.transport == "slack" };
@id("fixture-fetch")
permit(principal == Dekopon::Principal::"fixture-user",
       action == Dekopon::Action::"http-probe.fetch", resource == Dekopon::Provider::"http-probe")
when { context.via == "dekopon-gatewayd" && context has transport && context.transport == "slack"
       && context.agent == "fixture" };
''', encoding="utf-8")
    policy.chmod(0o600)
    for directory in ("journal", "assets"):
        (root / directory).mkdir(mode=0o700)


def events(path):
    with path.open(encoding="utf-8") as log:
        return [json.loads(line) for line in log if line.endswith("\n") and line.startswith("{")]


def wait_for(predicate, description, timeout, processes=()):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for process in processes:
            if process.poll() is not None:
                raise RuntimeError(f"Daemon {process.pid} exited with {process.returncode} during {description}")
        result = predicate()
        if result:
            return result
        time.sleep(0.05)
    raise RuntimeError(f"Timed out waiting for {description}")


@contextmanager
def daemon(root, lane, name, binary, env, exits):
    path = root / f"{lane}-{name}.log"
    args = [str(binary), "--worker-threads", "2", "--blocking-threads", "8",
            "--config", str(root / f"{name}.yaml")]
    with path.open("wb") as log:
        process = subprocess.Popen(args, cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT)
        try:
            yield process, path
        finally:
            running = process.poll() is None
            if running:
                process.terminate()
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            exits[name] = {"pid": process.pid, "exit": process.returncode, "sigterm": running}


def checked_command(root, label, args, env, timeout):
    result = subprocess.run(args, cwd=root, env=env, capture_output=True, text=True, timeout=timeout)
    (root / f"{label}.log").write_text(result.stdout + result.stderr, encoding="utf-8")
    if result.returncode:
        raise RuntimeError(f"{label} exited with {result.returncode}")
    return result.stdout.strip()


def binaries(root, args, env, report):
    versions = {}
    for lane, directory in (("B", args.b_bin_dir), ("A", args.a_bin_dir)):
        versions[lane] = {}
        report["binaries"][lane] = {}
        for name in ("broker", "gateway"):
            binary = (directory / f"dekopon-{name}d").resolve(strict=True)
            version = checked_command(root, f"{lane}-{name}-version", [binary, "--version"], env, 10)
            report["binaries"][lane][name] = {"path": str(binary), "version": version,
                                            "sha256": digest(binary)}
            checked_command(root, f"{lane}-{name}-check", [binary, "check", root / f"{name}.yaml"], env, 60)
            versions[lane][name] = binary
    return versions


def turn(slack, marker, expected, evidence, thread, reply_cursor):
    is_rollback = thread is not None
    evidence["reply_cursor"] = reply_cursor
    envelope, timestamp = slack.emit_mention(USER, marker, thread_ts=thread)
    thread = thread or timestamp
    evidence.update(envelope=envelope, message_ts=timestamp, thread_ts=thread,
                    channel=CHANNEL, user=USER, marker=marker)
    try:
        slack.wait_ack(envelope)
        evidence["acknowledged"] = True
        reply = slack.wait_reply(text=expected, after=evidence["reply_cursor"], timeout=45)
    except RuntimeError as error:
        slack.assert_clean()
        if slack.downstream_failures:
            raise RuntimeError("Model or provider fixture failed") from error
        if is_rollback:
            raise RollbackFailure(str(error)) from error
        raise
    evidence["reply"] = reply
    if reply.get("channel") != CHANNEL or reply.get("thread_ts") != thread:
        failure = RollbackFailure if is_rollback else RuntimeError
        raise failure("Slack reply was posted outside the expected channel or thread")
    return thread


def correlate(gateway_log, broker_log, evidence, provider_digest, processes, seed):
    delivery = wait_for(lambda: next((event for event in events(gateway_log)
                        if event.get("audit.event") == "reply.delivered"
                        and event.get("outcome") == "answered"
                        and any(span.get("conversation.thread") == evidence["thread_ts"]
                                and span.get("message.id") == evidence["message_ts"]
                                for span in event.get("spans", []))), None),
                        "gateway reply audit for the fixture thread", 15, processes)
    evidence["delivery"] = delivery
    trace = delivery.get("trace_id")
    if not trace:
        raise RuntimeError("Gateway reply audit omitted its trace ID")
    audit = wait_for(lambda: [event for event in events(broker_log)
                              if event.get("audit.event") == "broker.execution"
                              and event.get("trace_id") == trace], "correlated broker audit", 15, processes)
    evidence["broker_audit"] = audit
    if len(audit) != 1 or any(audit[0].get(key) != value for key, value in {
        "outcome": "Succeeded", "capability.id": CAPABILITY,
        "provider.artifact_sha256": provider_digest,
    }.items()):
        raise RuntimeError("Broker audit did not identify one successful call of the pinned provider")
    subject = audit[0].get("subject")
    if not subject:
        raise RuntimeError("Correlated broker audit omitted the Slack subject")
    if subject != SUBJECT:
        failure = RuntimeError if seed else RollbackFailure
        raise failure(f"Broker audited {subject} for the addressed Slack turn, expected {SUBJECT}")
    if seed:
        evidence["publication"] = wait_for(lambda: next((event for event in events(gateway_log)
            if event.get("event") == "gateway_memory_published" and event.get("trace_id") == trace), None),
            "correlated seed journal publication", 15, processes)


def run_lane(root, lane, version, env, slack, marker, expected, evidence, thread, cursor, provider_digest):
    exits = evidence["exits"] = {}
    with ExitStack() as stack:
        try:
            broker, broker_log = stack.enter_context(daemon(root, lane, "broker", version["broker"], env, exits))
            wait_for(lambda: (root / "broker.sock").is_socket(), "broker socket", 60, (broker,))
            checked_command(root, f"{lane}-broker-probe",
                            [version["broker"], "probe", "--socket", root / "broker.sock"], env, 10)
            gateway, gateway_log = stack.enter_context(daemon(root, lane, "gateway", version["gateway"], {
                **env, "FIXTURE_SLACK_APP_TOKEN": APP_TOKEN, "FIXTURE_SLACK_BOT_TOKEN": BOT_TOKEN,
                "FIXTURE_MODEL_KEY": MODEL_KEY,
            }, exits))
            wait_for(lambda: next((e for e in events(gateway_log) if e.get("event") == "gateway_started"), None),
                     "gateway startup", 60, (gateway, broker))
            slack.wait_connected()
            evidence["readiness"] = {"broker_pid": broker.pid, "gateway_pid": gateway.pid,
                                     "slack_connections": len(slack.connections)}
        except RuntimeError as error:
            slack.assert_clean()
            if lane == "A":
                raise RollbackFailure(f"A readiness failed: {error}") from error
            raise
        thread = turn(slack, marker, expected, evidence, thread, cursor)
        correlate(gateway_log, broker_log, evidence, provider_digest, (broker, gateway), lane == "B")
        evidence["transcript_cursor"] = len(slack.replies)
    if any(status["exit"] != 0 or not status["sigterm"] for status in exits.values()):
        raise RuntimeError(f"{lane} daemons did not exit cleanly after SIGTERM: {exits}")
    return thread


def rehearsal(root, args, report):
    markers = {lane: f"rollback-{lane}-{uuid.uuid4().hex}" for lane in ("B", "A")}
    replies = {marker: f"Probe passed for {marker}." for marker in markers.values()}
    calls, failures = [], []
    env = {"PATH": "/usr/bin:/bin", "TMPDIR": str(root), "RUST_LOG": "info"}
    shutil.copyfile(args.provider, root / "provider.wasm")
    provider_digest = digest(root / "provider.wasm")
    if provider_digest != args.provider_sha256:
        raise RuntimeError(f"Provider checksum mismatch: {provider_digest}")
    report["provider_sha256"] = provider_digest
    report["model_and_provider_calls"] = calls
    report["upstream_fixture_failures"] = failures
    with SlackSocketFixture(APP_TOKEN, BOT_TOKEN, (MODEL_KEY,), failures) as slack, \
            serve_model(replies, calls, failures) as port:
        try:
            configure(root, slack.endpoint, port)
            versions = binaries(root, args, env, report)
            thread, cursor = None, 0
            for lane in ("B", "A"):
                if lane == "A":
                    report["seed_journal_files"] = {
                        str(path.relative_to(root)): digest(path)
                        for path in sorted((root / "journal" / "fixture").rglob("*")) if path.is_file()
                    }
                    if not report["seed_journal_files"]:
                        raise RuntimeError("Seed publication left no journal files")
                    slack.wait_disconnected()
                    slack.expect_reconnect()
                marker = markers[lane]
                evidence = report[lane] = {}
                try:
                    thread = run_lane(root, lane, versions[lane], env, slack, marker, replies[marker],
                                      evidence, thread, cursor, provider_digest)
                    cursor = evidence["transcript_cursor"]
                finally:
                    log = root / f"{lane}-gateway.log"
                    if log.exists():
                        records = events(log)
                        for key, event in (("memory_startup", "gateway_memory_startup"),
                                           ("journal_resets", "gateway_journal_reset"),
                                           ("journal_recall", "gateway_recalled")):
                            evidence[key] = [record for record in records if record.get("event") == event]
            if report["B"]["delivery"]["trace_id"] == report["A"]["delivery"]["trace_id"]:
                raise RuntimeError("The two turns shared a trace ID")
            expected_calls = []
            for marker in markers.values():
                expected_calls += [{"marker": marker, "tool_result": False}, {"provider_path": "/probe"},
                                   {"marker": marker, "tool_result": True}]
            if failures or calls != expected_calls:
                raise RuntimeError("Unexpected model or provider fixture traffic")
        finally:
            write_json(root / "slack-transcript.json", {
                "calls": slack.calls, "sent": sorted(slack.sent), "acks": sorted(slack.acks),
                "replies": slack.replies,
            })


def main():
    parser = argparse.ArgumentParser(description="Rehearse a graceful Slack rollback with direct daemons and loopback fixtures.")
    parser.add_argument("--b-bin-dir", required=True, type=Path, help="Candidate daemon directory (seed first).")
    parser.add_argument("--a-bin-dir", required=True, type=Path, help="Rollback daemon directory.")
    parser.add_argument("--provider", required=True, type=Path, help="Shared http-probe component.")
    parser.add_argument("--provider-sha256", required=True, help="Expected SHA-256 of the component.")
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="deko-slack-", dir=Path("/tmp").resolve()))
    print(f"Evidence directory: {root}", flush=True)
    report = {"platform": {"system": os.uname().sysname, "machine": os.uname().machine}, "binaries": {}}
    try:
        rehearsal(root, args, report)
        report["verdict"] = "PASS"
    except RollbackFailure as error:
        report.update(verdict="FAIL", reason=str(error))
    except Exception as error:
        report.update(verdict="INCONCLUSIVE", reason=str(error))
    write_json(root / "verdict.json", report)
    print(report["verdict"] + (f": {report['reason']}" if "reason" in report else ""), flush=True)
    return {"PASS": 0, "FAIL": 1, "INCONCLUSIVE": 2}[report["verdict"]]


if __name__ == "__main__":
    sys.exit(main())
