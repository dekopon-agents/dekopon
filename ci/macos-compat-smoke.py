#!/usr/bin/env python3

import argparse
import hashlib
import http.client
import http.server
import json
import os
from pathlib import Path
import plistlib
import re
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import urlsplit

from slack_socket_fixture import SlackSocketFixture


VERSION = "0.72.0"
PROVIDER_SHA256 = "64914f342a367129f69f857c78871e37c967dfb8eff8215c047899007e326462"
GH_PROVIDER_SHA256 = "b2abb6a8a2dec689cb345b2b394b41a291d66196b2672a8fd86a014fe5cdee32"
SUBJECT = "slack.t0123abc.u0123abc"
COLLEAGUE_SUBJECT = "slack.t0123abc.u456def"
DENIED_SUBJECT = "slack.t0123abc.udenied"
UNMAPPED_SUBJECT = "slack.t0123abc.uunknown"
BROKER_PAT = "synthetic-broker-pat-${GITHUB_TOKEN}-literal"
BROKER_GITHUB_TOKEN = "broker-environment-value-must-not-expand"
MODEL_KEY = "synthetic-model-key-$literal-only"
SLACK_APP_TOKEN = "xapp-synthetic-app-token-only"
SLACK_BOT_TOKEN = "xoxb-synthetic-bot-token-only"
SECRETS = (BROKER_PAT, MODEL_KEY, SLACK_APP_TOKEN, SLACK_BOT_TOKEN)
DIRECT_CHANNEL = "D0123ABC"
REPLY = "Read-only provider call passed."
PR_REPLY = "PR metadata read passed."
HTTP_DENIED_REPLY = "The broker refused the out-of-scope HTTP request."


def private_json(path, value):
    path.write_text(json.dumps(value), encoding="utf-8")
    path.chmod(0o600)


def contains_credential(text):
    return "synthetic-" in text or any(secret in text for secret in SECRETS)


def assert_no_credentials(text, source):
    if contains_credential(text):
        raise RuntimeError(f"Synthetic credential appeared in {source}")


def redacted(text):
    for secret in SECRETS:
        text = text.replace(secret, "[redacted synthetic credential]")
    return text


def failure_text(error):
    return redacted(f"{error.__cause__}; {error}" if error.__cause__ else str(error))


def command(args, env, check=True, timeout=15):
    result = subprocess.run(args, env=env, capture_output=True, text=True, timeout=timeout)
    assert_no_credentials(result.stdout + result.stderr, f"{Path(args[0]).name} command output")
    if check and result.returncode:
        raise RuntimeError(f"{Path(args[0]).name} {args[1]} failed ({result.returncode}): {result.stderr.strip()}")
    return result


def configure(root, port, gateway, slack_endpoint):
    credentials = root / "broker-credentials.yaml"
    credentials.write_text(f"""apiVersion: dekopon.dev/broker-credentials/v1alpha1
credentials:
  - name: fixture-pat
    kind: bearerToken
    scheme: Bearer
    destinations: ['127.0.0.1:{port}']
    secret: '{BROKER_PAT}'
""", encoding="utf-8")
    credentials.chmod(0o600)
    environment = root / "credentials.env"
    environment.write_text(
        f"export SMOKE_MODEL_KEY='{MODEL_KEY}'\n"
        f"export DEKOPON_GATEWAYD_SLACK_APP_TOKEN='{SLACK_APP_TOKEN}'\n"
        f"export DEKOPON_GATEWAYD_SLACK_BOT_TOKEN='{SLACK_BOT_TOKEN}'\n", encoding="utf-8")
    environment.chmod(0o600)
    wrapper = root / "gateway-wrapper"
    wrapper.write_text(f"""#!/bin/sh
set -eu
umask 077
. {shlex.quote(str(environment))}
exec {shlex.quote(str(gateway))} --worker-threads 2 --blocking-threads 8 \\
  --config {shlex.quote(str(root / 'gateway.yaml'))}
""", encoding="utf-8")
    wrapper.chmod(0o700)
    for path in (credentials, environment):
        status = path.stat()
        if status.st_mode & 0o777 != 0o600 or status.st_nlink != 1 or status.st_uid != os.getuid():
            raise RuntimeError(f"Private credential file has unsafe ownership, mode or links: {path.name}")
    private_json(root / "broker.yaml", {
        "apiVersion": "dekopon.dev/brokerd/v1alpha1",
        "socketPath": str(root / "broker.sock"),
        "policiesPath": "policies.cedar",
        "credentialsPath": "broker-credentials.yaml",
        "strict": True,
        "providers": ["provider.wasm", "gh-provider.wasm"],
        "identities": [{
            "uid": os.getuid(), "principal": "dekopon-gatewayd",
            "actor": {"type": "service", "principal": "dekopon-gatewayd"},
            "attestor": {"namespaces": ["slack.t0123abc"]},
        }],
        "principals": {
            "owner": {"subjects": [SUBJECT], "groups": ["pr-reviewers"]},
            "colleague": {"subjects": [COLLEAGUE_SUBJECT], "groups": ["pr-reviewers"]},
            "denied": {"subjects": [DENIED_SUBJECT]},
            "review-room": {"subjects": ["slack.t0123abc.c0123abc"]},
            "denied-room": {"subjects": ["slack.t0123abc.cdenied"]},
        },
        "providerSettings": {name: {"baseUrl": f"http://127.0.0.1:{port}"} for name in ("jsonplaceholder", "gh")},
        "http": {"plaintextHosts": ["127.0.0.1"]},
        "capabilities": {"jsonplaceholder": {"capabilities": {
            "jsonplaceholder.posts.get": {"credential": "fixture-pat", "constraints": {
                "timeoutMs": 10000,
                "http": {
                    "allowedHosts": [f"127.0.0.1:{port}"], "allowedMethods": ["GET"],
                    "maxRequests": 1, "maxRequestBytes": 8192, "maxResponseBytes": 65536,
                    "allowPlaintextLoopback": True,
                    "requestTemplates": [{"method": "GET", "path": "/posts/7"}],
                },
            }},
        }}, "gh": {"capabilities": {"gh.pull-request.read": {
            "credential": "fixture-pat",
            "constraints": {
                "timeoutMs": 10000,
                "http": {
                    "allowedHosts": [f"127.0.0.1:{port}"], "allowedMethods": ["GET"],
                    "maxRequests": 1, "maxRequestBytes": 8192, "maxResponseBytes": 65536,
                    "allowPlaintextLoopback": True,
                    "requestTemplates": [{"method": "GET", "path": "/repos/fixture-owner/fixture-repo/pulls/7"}],
                },
            },
        }}}},
    })
    private_json(root / "gateway.yaml", {
        "apiVersion": "dekopon.dev/gatewayd/v1alpha1",
        "catalogPath": "agents.yaml",
        "broker": {"socketPath": str(root / "broker.sock"), "serverUid": os.getuid()},
        "transports": [
            {"name": "local", "kind": "local", "socketPath": str(root / "gateway.sock")},
            {"name": "review-slack", "kind": "slackSocketMode", "endpoint": slack_endpoint,
             "experience": "agent", "appTokenEnv": "DEKOPON_GATEWAYD_SLACK_APP_TOKEN",
             "botTokenEnv": "DEKOPON_GATEWAYD_SLACK_BOT_TOKEN",
             "indicators": {"mode": "native", "progress": "auto"}},
        ],
        "models": [{
            "name": "stub", "kind": "openaiCompatible", "model": "stub",
            "endpoint": f"http://127.0.0.1:{port}/v1", "apiKeyEnv": "SMOKE_MODEL_KEY",
            "timeoutMs": 10000, "classes": ["general"], "stream": False,
        }],
        "routes": [{
            "transport": "local", "conversation": {"kind": ["directMessage"]},
            "agent": "smoke", "model": "stub",
            "limits": {"maxSteps": 4, "maxCapabilityCalls": 1, "maxDurationMs": 30000},
            "memory": {"mode": "oneShot"},
        }, {
            "transport": "review-slack",
            "conversation": {"kind": ["channel", "thread"], "container": "t0123abc", "ids": ["c0123abc", "cdenied"]},
            "agent": "smoke", "model": "stub",
            "limits": {"maxSteps": 4, "maxCapabilityCalls": 1, "maxDurationMs": 30000},
            "memory": {"mode": "oneShot"},
        }],
        "sessions": {"maxConcurrent": 4, "maxConversations": 16, "assetRetentionBytes": 0},
        "shutdownGraceMs": 10000,
    })
    private_json(root / "agents.yaml", {
        "apiVersion": "dekopon.dev/v1alpha1", "kind": "Agent", "metadata": {"name": "smoke"},
        "spec": {"description": "Local compatibility fixture", "enabled": True,
                 "instructions": "Read the requested synthetic post or PR using the granted provider.", "modelClass": "general"},
        "status": "Ready",
    })
    (root / "policies.cedar").write_text('''
@id("smoke-prompt")
permit(principal == Dekopon::Principal::"owner",
       action == Dekopon::Action::"agent.prompt",
       resource == Dekopon::Agent::"smoke")
when { context.via == "dekopon-gatewayd" && context has transport && context.transport == "local" };
@id("smoke-post-read")
permit(principal == Dekopon::Principal::"owner",
       action == Dekopon::Action::"jsonplaceholder.posts.get",
       resource == Dekopon::Provider::"jsonplaceholder")
when { context.agent == "smoke" && context.via == "dekopon-gatewayd"
    && context has transport && context.transport == "local" };
@id("slack-review-prompt")
permit(principal, action == Dekopon::Action::"agent.prompt", resource == Dekopon::Agent::"smoke")
when { (principal in Dekopon::Group::"pr-reviewers" || principal == Dekopon::Principal::"review-room")
    && context.via == "dekopon-gatewayd"
    && context has transport && context.transport == "review-slack"
    && context has transportKind && context.transportKind == "slack"
    && context has conversation && context.conversation has container
    && context.conversation.container == "t0123abc"
    && ["c0123abc", "cdenied"].contains(context.conversation.id) };
@id("slack-review-read")
permit(principal, action == Dekopon::Action::"gh.pull-request.read", resource == Dekopon::Provider::"gh")
when { (principal in Dekopon::Group::"pr-reviewers" || principal == Dekopon::Principal::"review-room")
    && context.agent == "smoke" && context.via == "dekopon-gatewayd"
    && context has transport && context.transport == "review-slack"
    && context has transportKind && context.transportKind == "slack"
    && context has conversation && context.conversation has container
    && context.conversation.container == "t0123abc"
    && ["c0123abc", "cdenied"].contains(context.conversation.id) };
''', encoding="utf-8")
    (root / "policies.cedar").chmod(0o600)


def cassette_fixtures(root):
    responses = {
        "jsonplaceholder": ("/posts/7", "application/json", {
            "userId": 1, "id": 7, "title": "bounded-test-post", "body": "synthetic fixture",
        }, 1),
        "gh": ("/repos/fixture-owner/fixture-repo/pulls/7", "application/vnd.github+json", {
            "number": 7, "title": "Synthetic PR fixture", "state": "open",
            "head": {"ref": "fixture-branch", "sha": "a" * 40},
            "base": {"ref": "main", "sha": "b" * 40},
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
        }, 3),
    }
    for provider, (path, accept, body, count) in responses.items():
        directory = root / "cassettes" / provider
        directory.mkdir(parents=True, mode=0o700)
        for number in range(count):
            private_json(directory / f"{number + 1:04d}-GET.json", {
                "version": 1,
                "request": {"method": "GET", "path": path, "query": None,
                            "headers": {"accept": accept}, "body": None},
                "response": {"status": 200, "headers": {"content-type": "application/json"},
                             "body": {"json": body}},
            })


def wait_cassette(daemon):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if daemon.process.poll() is not None:
            raise RuntimeError(f"Cassette replay exited before readiness: {daemon.process.returncode}")
        output = daemon.path.read_text()
        assert_no_credentials(output, "cassette output")
        match = re.search(r"cassette listening on (http://127\.0\.0\.1:[0-9]+)", output)
        if match:
            return urlsplit(match[1])
        time.sleep(0.1)
    raise RuntimeError("Timed out waiting for cassette replay listener")


def fixture_handler(requests, failures, tool_commands, replay, stop_started, stop_release, stop_finished):
    class Handler(http.server.BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(10)

        def log_message(self, *_args):
            pass

        def respond(self, value, status=200, allow_disconnect=False):
            data = json.dumps(value).encode()
            assert_no_credentials(data.decode(), "fixture response")
            try:
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
            except (BrokenPipeError, ConnectionResetError):
                if not allow_disconnect:
                    failures.append("Unexpected fixture response disconnect")

        def do_GET(self):
            prefix = "jsonplaceholder" if self.path == "/posts/7" else "gh"
            requests.append(prefix)
            if self.path not in ("/posts/7", "/repos/fixture-owner/fixture-repo/pulls/7") or self.headers.get_all("Authorization") != [f"Bearer {BROKER_PAT}"]:
                failures.append("Unexpected provider path or broker credential")
                self.respond({"error": failures[-1]}, 400)
                return
            other_headers = json.dumps([(key, value) for key, value in self.headers.items() if key.lower() != "authorization"])
            if contains_credential(other_headers):
                failures.append("Gateway credential reached the provider API")
                self.respond({"error": failures[-1]}, 400)
                return
            connection = http.client.HTTPConnection(replay.hostname, replay.port, timeout=10)
            try:
                connection.request("GET", f"/{prefix}{self.path}", headers={"Accept": self.headers.get("Accept", "")})
                response = connection.getresponse()
                body = response.read(65536)
                assert_no_credentials(body.decode(), "cassette provider response")
                if response.status != 200:
                    failures.append("Cassette replay did not match the provider request")
                    self.respond({"error": failures[-1]}, 400)
                    return
                self.respond(json.loads(body))
            finally:
                connection.close()

        def do_POST(self):
            requests.append("model")
            data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path != "/v1/chat/completions" or self.headers.get_all("Authorization") != [f"Bearer {MODEL_KEY}"]:
                failures.append("Unexpected model endpoint or fake credential")
                self.respond({"error": failures[-1]}, 400)
                return
            other_headers = json.dumps([(key, value) for key, value in self.headers.items() if key.lower() != "authorization"])
            if contains_credential(other_headers + json.dumps(data)):
                failures.append("A credential reached model input or the wrong HTTP client")
                self.respond({"error": failures[-1]}, 400)
                return
            user = next(message for message in reversed(data["messages"]) if message["role"] == "user")
            prompt = user["content"].splitlines()[-1]
            prompt = re.sub(r"<@[A-Za-z0-9]+>\s*", "", prompt).strip()
            if prompt not in ("Read post 7.", "Read post 8.", "Review PR 7.", "Wait for Stop."):
                failures.append("Model received an unexpected fixture prompt")
                self.respond({"error": failures[-1]}, 400)
                return
            if prompt == "Wait for Stop.":
                stop_started.set()
                if not stop_release.wait(15):
                    failures.append("Held model request was not released after Stop")
                try:
                    self.respond({"choices": [{"message": {"role": "assistant", "content": "Cancelled answer must not be delivered."}}],
                                  "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}, allow_disconnect=True)
                finally:
                    stop_finished.set()
                return
            blocked = prompt == "Read post 8."
            pull_request = prompt == "Review PR 7."
            tool_messages = [message for message in data["messages"] if message["role"] == "tool"]
            if not tool_messages:
                tool_command = "gh pr view 7 -R fixture-owner/fixture-repo --json title,state" if pull_request else f"placeholder posts get --post-id {8 if blocked else 7}"
                tool_commands.append(tool_command)
                message = {"role": "assistant", "content": None, "tool_calls": [{
                    "id": "call-1", "type": "function", "function": {
                        "name": "bash", "arguments": json.dumps({"command": tool_command}),
                    },
                }]}
            elif pull_request and "Synthetic PR fixture" in json.dumps(tool_messages) and "OPEN" in json.dumps(tool_messages):
                message = {"role": "assistant", "content": PR_REPLY, "tool_calls": []}
            elif not pull_request and not blocked and "bounded-test-post" in json.dumps(tool_messages):
                message = {"role": "assistant", "content": REPLY, "tool_calls": []}
            elif blocked and "jsonplaceholder.posts.get: failed: denied" in json.dumps(tool_messages):
                message = {"role": "assistant", "content": HTTP_DENIED_REPLY, "tool_calls": []}
            else:
                failures.append("Model did not receive the expected provider result")
                self.respond({"error": failures[-1]}, 400)
                return
            self.respond({"choices": [{"message": message}], "usage": {
                "prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120,
            }})
    return Handler


def launchd_domain(env):
    if sys.platform != "darwin":
        raise RuntimeError("--launchd requires Darwin; no launchd lifecycle was tested")
    failures = []
    for kind in ("gui", "user"):
        domain = f"{kind}/{os.getuid()}"
        result = command(["/bin/launchctl", "print", domain], env, check=False)
        if result.returncode == 0:
            print(f"Existing launchd domain: {domain}", flush=True)
            return domain
        failures.append(f"{domain}: exit {result.returncode}: {result.stderr.strip()}")
    raise RuntimeError("No existing user launchd domain; no bootstrap attempted: " + "; ".join(failures))


class Daemon:
    def __init__(self, name, args, root, env, domain):
        self.name, self.args, self.root, self.env, self.domain = name, args, root, env, domain
        self.process = None
        self.log = None
        self.target = None
        self.absent = False
        self.returncode = None
        self.path = root / f"{name}.log"
        self.path.touch(mode=0o600)

    def start(self):
        if self.domain is None:
            self.log = self.path.open("wb")
            self.process = subprocess.Popen(self.args, cwd=self.root, env=self.env,
                                            stdout=self.log, stderr=subprocess.STDOUT)
            return
        label = f"dev.dekopon.compat.{self.root.name}.{self.name}"
        plist = self.root / f"{self.name}.plist"
        plist.write_bytes(plistlib.dumps({
            "Label": label,
            "ProgramArguments": ["/usr/bin/env", "-i", *[f"{key}={value}" for key, value in self.env.items()], *self.args],
            "WorkingDirectory": str(self.root), "RunAtLoad": True, "ExitTimeOut": 15, "Umask": 63,
            "StandardOutPath": str(self.path), "StandardErrorPath": str(self.path),
        }))
        plist.chmod(0o600)
        command(["/usr/bin/plutil", "-lint", str(plist)], self.env)
        self.target = f"{self.domain}/{label}"
        print(f"Owned launchd job: {self.target}", flush=True)
        command(["/bin/launchctl", "bootstrap", self.domain, str(plist)], self.env)

    def status(self):
        if self.process is not None:
            return self.process.pid, self.process.poll()
        result = command(["/bin/launchctl", "print", self.target], self.env, check=False)
        if result.returncode:
            label = self.target.rsplit("/", 1)[1]
            if f'Could not find service "{label}" in domain' in result.stderr:
                self.absent = True
                return None, None
            raise RuntimeError(f"Cannot determine owned launchd job state: {self.target}; print={result.returncode}: {result.stderr.strip()}")
        self.absent = False
        pid = re.search(r"^\s*pid = ([0-9]+)$", result.stdout, re.MULTILINE)
        code = re.search(r"^\s*last exit code = ([0-9]+)$", result.stdout, re.MULTILINE)
        return int(pid[1]) if pid else None, int(code[1]) if code and not pid else None

    def stop(self):
        if self.process is not None:
            if self.process.poll() is None:
                self.process.terminate()
                try:
                    self.process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=5)
            self.returncode = self.process.returncode
            self.log.close()
            return
        if self.target is None:
            return
        failure = None
        try:
            pid, self.returncode = self.status()
            if pid is not None:
                command(["/bin/launchctl", "kill", "SIGTERM", self.target], self.env)
                deadline = time.monotonic() + 15
                while time.monotonic() < deadline:
                    pid, self.returncode = self.status()
                    if pid is None and self.returncode is not None:
                        break
                    if self.absent:
                        raise RuntimeError(f"Owned launchd job disappeared without a recorded exit status: {self.target}")
                    time.sleep(0.1)
                if pid is not None:
                    command(["/bin/launchctl", "kill", "SIGKILL", self.target], self.env)
                    raise RuntimeError(f"Owned launchd job needed forced termination: {self.target}")
                if self.returncode is None:
                    raise RuntimeError(f"No recorded exit status after stopping owned launchd job: {self.target}")
        except BaseException as error:
            failure = error
            raise
        finally:
            if not self.absent:
                result = command(["/bin/launchctl", "bootout", self.target], self.env, check=False)
                self.status()
                if not self.absent:
                    raise RuntimeError(f"Owned launchd job remains loaded after bootout ({result.returncode}): {self.target}") from failure
                if self.returncode == 0:
                    print(f"Stopped with exit 0 and confirmed unloaded: {self.target}", flush=True)


def wait_for_socket(path, daemon):
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        pid, code = daemon.status()
        if daemon.absent:
            raise RuntimeError(f"Owned launchd job disappeared before readiness: {daemon.target}")
        if code is not None:
            raise RuntimeError(f"{daemon.name} exited before readiness with {code}")
        if path.exists() and pid is not None:
            print(f"Ready: {daemon.name}, pid {pid}", flush=True)
            return
        time.sleep(0.1)
    raise RuntimeError(f"Timed out waiting for {path.name}")


def turn(root, subject, text, conversation):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(45)
        client.connect(str(root / "gateway.sock"))
        client.sendall((json.dumps({"subject": subject, "conversation": {
            "kind": "directMessage", "id": conversation,
        }, "text": text}) + "\n").encode())
        with client.makefile("rb") as response:
            line = response.readline(65536).decode()
    assert_no_credentials(line, "gateway reply")
    return json.loads(line)


def gateway_events(root):
    return [json.loads(line) for line in (root / "gateway.log").read_text().splitlines(keepends=True)
            if line.endswith("\n")]


def wait_gateway_event(root, predicate, description):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        for event in gateway_events(root):
            if predicate(event):
                return event
        time.sleep(0.05)
    raise RuntimeError(f"Missing gateway evidence: {description}")


def event_in_thread(event, thread):
    return any(span.get("conversation.thread") == thread
               for span in [event.get("span", {}), *event.get("spans", [])])


def slack_turns(root, slack, requests, stop_started, stop_release, stop_finished):
    owner = SUBJECT.rsplit(".", 1)[1]
    threads = {}
    for subject in (SUBJECT, COLLEAGUE_SUBJECT):
        user = subject.rsplit(".", 1)[1]
        before = list(requests)
        envelope, thread = slack.emit_mention(user, "Review PR 7.")
        slack.wait_ack(envelope)
        status = slack.wait_status("processing", thread)
        if status.get("initiator_user_id", "").casefold() != user:
            raise RuntimeError("Slack Working status did not identify the requesting member")
        reply = slack.wait_reply(PR_REPLY, thread)
        if reply.get("channel", "").casefold() != "c0123abc":
            raise RuntimeError("PR reply was posted outside the review channel")
        if "initiator_user_id" in slack.wait_status("active", thread):
            raise RuntimeError("Slack active status retained a processing initiator")
        wait_gateway_event(root, lambda event: event.get("audit.event") == "reply.delivered"
                           and event.get("outcome") == "answered" and event_in_thread(event, thread),
                           "completed Slack PR reply delivery")
        if requests != before + ["model", "gh", "model"]:
            raise RuntimeError("Slack PR review did not make exactly two model calls and one GH read")
        threads[subject] = thread
    before, answered = list(requests), len(slack.replies)
    envelope, _ = slack.emit_message(owner, "Review PR 7.", slack.CHANNEL, "channel", threads[SUBJECT])
    slack.wait_ack(envelope)
    slack.wait_reply(PR_REPLY, threads[SUBJECT], after=answered)
    if requests != before + ["model", "gh", "model"]:
        raise RuntimeError("The owner's unmentioned thread follow-up did not make two model calls and one GH read")
    for subject, channel, reason in ((UNMAPPED_SUBJECT, "c0123abc", "unmapped-subject"),
                                     (DENIED_SUBJECT, "c0123abc", "agent-denied"),
                                     (SUBJECT, "cdenied", "attestation-refused")):
        before = list(requests)
        envelope, thread = slack.emit_mention(subject.rsplit(".", 1)[1], "Review PR 7.", channel=channel)
        slack.wait_ack(envelope)
        wait_gateway_event(root, lambda event: event.get("event") == "gateway_session_rejected"
                           and event.get("reason") == reason and event_in_thread(event, thread)
                           and any(span.get("conversation.id") == channel
                                   for span in [event.get("span", {}), *event.get("spans", [])])
                           and (reason == "attestation-refused"
                                or (event.get("transport") == "review-slack" and event.get("subject") == subject)),
                           "Slack sender/room authorization refusal")
        if channel == "cdenied":
            broker_events = [json.loads(line) for line in (root / "broker.log").read_text().splitlines()]
            if not any(event.get("event") == "broker_capabilities_refused"
                       and event.get("reason") == "agent-denied" and event.get("subject") == SUBJECT
                       and event.get("agent") == "smoke" and event.get("via") == "dekopon-gatewayd"
                       and json.loads(event.get("policy_ids", "[]")) == ["slack-review-prompt"]
                       for event in broker_events):
                raise RuntimeError("Missing the broker policy refusal for the mapped room")
        if requests != before or any(reply.get("thread_ts") == thread and reply.get("text") == PR_REPLY for reply in slack.replies):
            raise RuntimeError("Denied Slack sender or room reached downstream inference/provider work")
    before = list(requests)
    envelope, direct = slack.emit_message(owner, "Review PR 7.", DIRECT_CHANNEL, "im")
    slack.wait_ack(envelope)
    wait_gateway_event(root, lambda event: event.get("event") == "gateway_message_ignored"
                       and event.get("reason") == "unrouted" and event.get("transport") == "review-slack"
                       and event_in_thread(event, direct), "direct message ignored by the channel-only route")
    if requests != before or any(DIRECT_CHANNEL in (call["body"].get("channel"), call["body"].get("channel_id"))
                                 for call in slack.calls):
        raise RuntimeError("A direct message on the channel-only Slack route reached the agent or got a reply")
    before = list(requests)
    envelope, thread = slack.emit_mention(SUBJECT.rsplit(".", 1)[1], "Wait for Stop.")
    slack.wait_ack(envelope)
    status = slack.wait_status("processing", thread)
    if status.get("initiator_user_id", "").casefold() != SUBJECT.rsplit(".", 1)[1] or not stop_started.wait(10):
        raise RuntimeError("Native Stop was not tested against the initiating user's active model request")
    envelope, _ = slack.send_stop(SUBJECT.rsplit(".", 1)[1], thread)
    slack.wait_ack(envelope)
    wait_gateway_event(root, lambda event: event.get("event") == "gateway_session_stop_requested"
                       and event.get("transport") == "review-slack" and event.get("via") == "native-stop",
                       "accepted native Stop request")
    wait_gateway_event(root, lambda event: event.get("event") == "gateway_session_cancelled"
                       and event_in_thread(event, thread), "cancellation of the held Slack turn")
    wait_gateway_event(root, lambda event: event.get("kind") == "terminal_cancelled"
                       and event.get("by") == "user:native-stop" and event_in_thread(event, thread),
                       "native Stop cancellation of the active turn")
    if "initiator_user_id" in slack.wait_status("active", thread):
        raise RuntimeError("Slack active status retained a cancelled initiator")
    slack.wait_reply("Stopped.", thread)
    stop_release.set()
    if not stop_finished.wait(10):
        raise RuntimeError("The cancelled model fixture did not finish after release")
    if requests != before + ["model"]:
        raise RuntimeError("Native Stop allowed an unexpected provider call or model retry")
    slack.wait_all_acks()
    return thread


def smoke(root, bin_dir, provider, gh_provider, cassette_binary, use_launchd):
    for source, filename, expected in ((provider, "provider.wasm", PROVIDER_SHA256),
                                       (gh_provider, "gh-provider.wasm", GH_PROVIDER_SHA256)):
        component = source.read_bytes()
        if hashlib.sha256(component).hexdigest() != expected:
            raise RuntimeError(f"Unexpected pinned provider checksum: {filename}")
        (root / filename).write_bytes(component)
        (root / filename).chmod(0o600)
    env = {"PATH": "/usr/bin:/bin", "TMPDIR": str(root), "RUST_LOG": "info"}
    domain = launchd_domain(env) if use_launchd else None
    binaries = {name: bin_dir / f"dekopon-{name}d" for name in ("broker", "gateway")}
    for binary in binaries.values():
        version = command([str(binary), "--version"], env, timeout=10).stdout.strip()
        if version != f"{binary.name} {VERSION}":
            raise RuntimeError(f"Unexpected installed version: {version}")
        print(f"Binary: {binary} ({version})", flush=True)
    if command([str(cassette_binary), "--version"], env).stdout.strip() != "cassette 0.1.0":
        raise RuntimeError("Expected the installed cassette 0.1.0 binary")
    print(f"Replay binary: {cassette_binary} (cassette 0.1.0)", flush=True)
    requests, failures, daemons, tool_commands = [], [], [], []
    stop_started, stop_release, stop_finished = threading.Event(), threading.Event(), threading.Event()
    slack = SlackSocketFixture(SLACK_APP_TOKEN, SLACK_BOT_TOKEN, (BROKER_PAT, MODEL_KEY), failures)
    server = thread = None
    http_started = False
    stop_thread = None
    failure = None
    try:
        cassette_fixtures(root)
        cassette = Daemon("cassette", [str(cassette_binary), "replay", "--dir", str(root / "cassettes"),
                                       "--listen", "127.0.0.1:0"], root, env, None)
        daemons.append(cassette)
        cassette.start()
        replay = wait_cassette(cassette)
        slack.__enter__()
        server = http.server.HTTPServer(("127.0.0.1", 0), fixture_handler(
            requests, failures, tool_commands, replay, stop_started, stop_release, stop_finished))
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        http_started = True
        configure(root, server.server_port, binaries["gateway"], slack.endpoint)
        for name, binary in binaries.items():
            command([str(binary), "check", str(root / f"{name}.yaml")],
                    {**env, "RUST_LOG": "warn"}, timeout=60)
        for name, binary in binaries.items():
            if name == "broker":
                args = [str(binary), "--worker-threads", "2", "--blocking-threads", "8",
                        "--cpu-threads", "2", "--config", str(root / "broker.yaml")]
                daemon_env = {**env, "GITHUB_TOKEN": BROKER_GITHUB_TOKEN}
            else:
                args = [str(root / "gateway-wrapper")]
                daemon_env = {**env, "RUST_LOG": "info,dekopon_gatewayd::gateway=debug"}
            daemon = Daemon(name, args, root, daemon_env, domain)
            daemons.append(daemon)
            daemon.start()
            wait_for_socket(root / f"{name}.sock", daemon)
            if name == "broker":
                command([str(binary), "probe", "--socket", str(root / "broker.sock")], env, timeout=10)
        slack.wait_connected()
        reply = turn(root, SUBJECT, "Read post 7.", "positive")
        if reply != {"reply": REPLY} or requests != ["model", "jsonplaceholder", "model"] or failures:
            raise RuntimeError(f"Positive turn failed; calls: {requests!r}, failures: {failures!r}")
        for subject, conversation in ((UNMAPPED_SUBJECT, "unmapped"), (DENIED_SUBJECT, "denied")):
            before = list(requests)
            reply = turn(root, subject, "Read post 7.", conversation)
            if requests != before or not reply.get("reply") or reply["reply"] == REPLY:
                raise RuntimeError(f"{conversation} subject was not refused before downstream calls")
        reply = turn(root, SUBJECT, "Read post 8.", "http-denied")
        if reply != {"reply": HTTP_DENIED_REPLY} or requests != ["model", "jsonplaceholder", "model", "model", "model"] or failures:
            raise RuntimeError(f"HTTP constraint refusal failed; calls: {requests!r}, failures: {failures!r}")
        if tool_commands != ["placeholder posts get --post-id 7", "placeholder posts get --post-id 8"]:
            raise RuntimeError("The positive and HTTP-denied turns did not select the intended tool arguments")
        stop_thread = slack_turns(root, slack, requests, stop_started, stop_release, stop_finished)
        slack.assert_clean()
    except BaseException as error:
        failure = error
        raise
    finally:
        stop_release.set()
        cleanup_errors = []
        for daemon in reversed(daemons):
            try:
                daemon.stop()
            except Exception as error:
                cleanup_errors.append(failure_text(error))
        if server is not None:
            if http_started:
                server.shutdown()
            server.server_close()
        if http_started:
            thread.join(timeout=5)
            if thread.is_alive():
                cleanup_errors.append("Fixture server thread did not exit")
        try:
            slack.close()
        except Exception as error:
            cleanup_errors.append(redacted(str(error)))
        if failure is not None or cleanup_errors or any(daemon.returncode != 0 for daemon in daemons):
            for name in ("broker", "gateway", "cassette"):
                path = root / f"{name}.log"
                if path.exists():
                    print(f"--- {name} log ---\n{redacted(path.read_text())}", flush=True)
        if cleanup_errors:
            raise RuntimeError("Cleanup failed: " + "; ".join(cleanup_errors)) from failure
    if [daemon.returncode for daemon in daemons] != [0, 0, 0]:
        raise RuntimeError(f"Daemons did not shut down cleanly: {[d.returncode for d in daemons]}")
    for name in ("broker", "gateway", "cassette"):
        assert_no_credentials((root / f"{name}.log").read_text(), f"{name} daemon log")
    executions = [json.loads(line) for line in (root / "broker.log").read_text().splitlines()]
    successes = [event for event in executions if event.get("audit.event") == "broker.execution"
                 and event.get("outcome") == "Succeeded"
                 and event.get("capability.id") == "jsonplaceholder.posts.get"]
    if len(successes) != 1 or successes[0].get("provider.artifact_sha256") != PROVIDER_SHA256:
        raise RuntimeError("Missing the successful broker audit event for the pinned provider")
    failed = [event for event in executions if event.get("audit.event") == "broker.execution"
              and event.get("outcome") == "Failed"
              and event.get("capability.id") == "jsonplaceholder.posts.get"]
    if len(failed) != 1:
        raise RuntimeError("Missing the broker execution failure for the denied HTTP request")
    reviews = [event for event in executions if event.get("audit.event") == "broker.execution"
               and event.get("outcome") == "Succeeded" and event.get("capability.id") == "gh.pull-request.read"]
    if len(reviews) != 3 or {event.get("principal") for event in reviews} != {"owner", "colleague"} or any(
        event.get("provider.artifact_sha256") != GH_PROVIDER_SHA256 or event.get("effect") != "ReadOnly" for event in reviews
    ):
        raise RuntimeError("Missing owner-and-colleague read-only executions of the pinned GH provider")
    if requests.count("model") != 11 or requests.count("jsonplaceholder") != 1 or requests.count("gh") != 3 or failures:
        raise RuntimeError("Unexpected cumulative downstream activity after authorization and Stop checks")
    if tool_commands != ["placeholder posts get --post-id 7", "placeholder posts get --post-id 8",
                         *["gh pr view 7 -R fixture-owner/fixture-repo --json title,state"] * 3]:
        raise RuntimeError("Unexpected cumulative tool work after native Stop")
    slack.assert_clean()
    if slack.sent != slack.acks:
        raise RuntimeError("Some Slack envelopes were not acknowledged")
    assert_no_credentials(json.dumps(slack.calls + slack.replies), "Slack captured requests and replies")
    if sum(reply.get("text") == PR_REPLY for reply in slack.replies) != 3 or any(
        reply.get("thread_ts") == stop_thread and reply.get("text") != "Stopped." for reply in slack.replies
    ):
        raise RuntimeError("A denied or cancelled Slack turn delivered an unexpected answer")
    gateway_log_events = gateway_events(root)
    rejections = {(event.get("subject"), event.get("reason")) for event in gateway_log_events
                  if event.get("event") == "gateway_session_rejected" and event.get("transport") == "local"}
    if rejections != {(UNMAPPED_SUBJECT, "unmapped-subject"), (DENIED_SUBJECT, "agent-denied")}:
        raise RuntimeError("Local refusals did not record exactly the unmapped and policy-denied gateway reasons")
    print("PASS: private broker credential file kept ${GITHUB_TOKEN} literal; shell-sourced gateway environment, configuration checks and broker probe.")
    print("PASS: actual cassette replay served four credential-bound provider GETs; eleven model calls include one cancelled request.")
    print("PASS: out-of-scope HTTP request refused without reaching the provider fixture; credentials and their synthetic- prefix absent from checks, logs, model inputs, tool results and replies.")
    print("PASS: real Slack Agent adapter acknowledged all envelopes on one connection; owner and colleague received threaded PR reads; the owner's unmentioned thread follow-up was answered; unmapped/denied senders and missing room grant made no downstream calls; the owner's direct message matched no channel-only route.")
    print("PASS: processing status identified the initiator and active status omitted it; native Stop cancelled a held model request without provider work or a normal answer.")
    print("PASS: clean daemon shutdown" + (" and owned launchd jobs unloaded." if domain else "."))
    print("Scope: fixture concurrency 4, same-UID " + ("user launchd jobs" if domain else "foreground processes")
          + "; deterministic cassette and Slack wire fixtures, not in-process Fauxlack or live services. No separate-user isolation, system LaunchDaemons or boot/logout/restart recovery.")


def main():
    parser = argparse.ArgumentParser(description="Exercise released daemons and a pinned Wasm provider with local fixtures.")
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--provider", type=Path, required=True)
    parser.add_argument("--gh-provider", type=Path, required=True)
    parser.add_argument("--cassette-bin", type=Path, required=True)
    parser.add_argument("--launchd", action="store_true", help="Require an existing Darwin user launchd domain; never fall back to processes.")
    args = parser.parse_args()
    # macOS TMPDIR paths can exceed sockaddr_un; resolve /tmp's symlink before protected-file checks.
    root = Path(tempfile.mkdtemp(prefix="deko-mac-", dir=Path("/tmp").resolve()))
    remove = False
    try:
        smoke(root, args.bin_dir.resolve(strict=True), args.provider.resolve(strict=True),
              args.gh_provider.resolve(strict=True), args.cassette_bin.resolve(strict=True), args.launchd)
        remove = True
    except Exception as error:
        raise SystemExit(failure_text(error)) from None
    finally:
        if remove:
            shutil.rmtree(root)
        else:
            print(f"Retained private fixture directory: {root}", file=sys.stderr)


if __name__ == "__main__":
    main()
