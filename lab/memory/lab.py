#!/usr/bin/env python3
import argparse
import csv
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import fakemodel  # noqa: E402

SUBJECT = "drn:tel:number::16034700182"
AGENT = "lab-agent"
ROUTES = ("summary", "plain", "none")
SUMMARIZER_TIMEOUT_MS = 20000
CHAT_TIMEOUT_MS = 30000
TURN_DEADLINE_S = 150
PROXY_ENV = {"HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "NO_PROXY", "no_proxy", "SSL_CERT_FILE"}
POLICY = '''@id("lab-user-may-prompt")
permit(principal == Dekopon::Principal::"lab-user",
 action == Dekopon::Action::"agent.prompt", resource == Dekopon::Agent::"lab-agent")
 when { context has via && context.via == "dekopon-gatewayd" };
@id("lab-agent-cli-probe-upper")
permit(principal == Dekopon::Principal::"lab-user",
 action == Dekopon::Action::"cli-probe.upper", resource == Dekopon::Provider::"cli-probe")
 when { context has via && context.via == "dekopon-gatewayd"
 && context has agent && context.agent == "lab-agent" };
'''


def utc(ts=None):
    return datetime.fromtimestamp(ts if ts is not None else time.time(), timezone.utc).isoformat(
        timespec="milliseconds")


class Lab:
    def __init__(self, args):
        self.args = args
        self.run = Path(args.run)
        self.state = Path(args.state)
        self.bin = Path(args.bin)
        self.ids = {"broker": args.broker_uid, "gateway": args.gateway_uid, "ipc": args.ipc_gid}
        self.dirs = {name: self.state / name for name in ("broker", "ipc", "gateway")}
        self.journal = self.dirs["gateway"] / "journal"
        self.events = open(self.run / "events.jsonl", "a", encoding="utf-8")
        self.turns = []
        self.findings = {}
        self.pids = {}
        self.daemons = {}
        self.sampling = threading.Event()
        self.budget = None
        real = None
        if args.profile == "real":
            self.budget = fakemodel.Budget(args.cost_cap, args.price_in, args.price_out)
            real = {"model": args.real_model, "budget": self.budget}
        tls = (os.environ["LAB_FAKE_TLS_CERT"], os.environ["LAB_FAKE_TLS_KEY"]) \
            if os.environ.get("LAB_FAKE_TLS_CERT") else None
        self.model = fakemodel.FakeModel(self.run / "model-calls.jsonl", real, tls).start()
        self.summaries_on = True
        self.codex_auth = self.dirs["gateway"] / "codex" / "auth.json"
        self.codex_original = None

    def event(self, name, **fields):
        self.events.write(json.dumps({"utc": utc(), "t": time.time(), "event": name, **fields}) + "\n")
        self.events.flush()
        print(f"[lab] {name} {json.dumps(fields) if fields else ''}", flush=True)

    def own(self, path, who, mode):
        if os.geteuid() == 0:
            os.chown(path, self.ids[who], self.ids[who])
        os.chmod(path, mode)

    def prepare(self):
        for name, mode in (("broker", 0o700), ("gateway", 0o700)):
            self.dirs[name].mkdir(parents=True, exist_ok=True)
            self.own(self.dirs[name], name, mode)
        if self.args.profile == "codex":
            self.copy_codex_auth()
        self.dirs["ipc"].mkdir(parents=True, exist_ok=True)
        if os.geteuid() == 0:
            os.chown(self.dirs["ipc"], self.ids["broker"], self.ids["ipc"])
        os.chmod(self.dirs["ipc"], 0o710 if self.ids["broker"] != self.ids["gateway"] else 0o700)
        wasm = self.dirs["broker"] / "cli-probe-provider.wasm"
        shutil.copyfile(self.bin / "cli-probe-provider.wasm", wasm)
        self.own(wasm, "broker", 0o600)
        telemetry = lambda service: {"endpoint": self.args.otlp, "transport": "http",  # noqa: E731
                                     "serviceName": service, "exportTimeoutMs": 5000}
        self.write("broker", "policies.cedar", POLICY)
        self.write("broker", "broker.json", {
            "apiVersion": "dekopon.dev/brokerd/v1alpha1",
            "socketPath": str(self.dirs["ipc"] / "broker.sock"),
            "policiesPath": str(self.dirs["broker"] / "policies.cedar"),
            "providers": [str(wasm)],
            "identities": [{"uid": self.ids["gateway"], "principal": "dekopon-gatewayd",
                            "actor": {"type": "service", "principal": "dekopon-gatewayd"},
                            "attestor": {"namespaces": ["drn:tel:*:*:*"]}}],
            "principals": {"lab-user": {"subjects": [SUBJECT]}},
            "capabilities": {"cli-probe": {"constraints": {"timeoutMs": 30000},
                                           "capabilities": {"cli-probe.upper": {}}}},
            "telemetry": telemetry("lab-brokerd")})
        self.write("gateway", "catalog.json", {
            "apiVersion": "dekopon.dev/v1alpha1", "kind": "Agent", "metadata": {"name": AGENT},
            "spec": {"description": "Memory lab agent", "enabled": True,
                     "instructions": "Answer briefly. Run bash commands when asked.",
                     "modelClass": "reasoning"}})
        self.write_gateway_config()
        self.journal.mkdir(exist_ok=True)
        self.own(self.journal, "gateway", 0o700)

    def write(self, who, name, value):
        text = value if isinstance(value, str) else json.dumps(value, indent=1)
        path = self.dirs[who] / name
        path.write_text(text)
        self.own(path, who, 0o600)
        (self.run / "config").mkdir(exist_ok=True)
        (self.run / "config" / name).write_text(text)
        return path

    def write_gateway_config(self):
        endpoint = f"{self.model.scheme}://127.0.0.1:{self.model.port}/v1"
        model = lambda name, model_id, classes, timeout: {  # noqa: E731
            "name": name, "kind": "openaiCompatible", "model": model_id, "endpoint": endpoint,
            "apiKeyEnv": "LAB_MODEL_KEY", "timeoutMs": timeout, "classes": classes, "stream": False}
        capture = {"maxItemBytes": 4 * 1024 * 1024}
        summary_memory = {"mode": "persistent", "recall": "journal", "capture": capture,
                          "selection": "ladder",
                          "summaries": {"model": "summarizer", "parts": [256, 1024]}
                          if self.summaries_on else "off"}
        route = lambda conversation, memory: {  # noqa: E731
            "transport": "dev", "conversation": {"kind": ["directMessage"], "ids": [conversation]},
            "agent": AGENT, "memory": memory, "limits": {"maxSteps": 4, "maxCapabilityCalls": 4}}
        self.write("gateway", "gateway.json", {
            "apiVersion": "dekopon.dev/gatewayd/v1alpha1",
            "catalogPath": str(self.dirs["gateway"] / "catalog.json"),
            "broker": {"socketPath": str(self.dirs["ipc"] / "broker.sock"),
                       "serverUid": self.ids["broker"]},
            "transports": [{"name": "dev", "kind": "local",
                            "socketPath": str(self.dirs["gateway"] / "dev.sock")}],
            "models": [self.chat_model(model),
                       self.summarizer_model(model)],
            "routes": [route("summary", summary_memory),
                       route("plain", {"mode": "persistent", "recall": "journal", "capture": capture,
                                       "summaries": "off"}),
                       route("none", {"mode": "persistent", "recall": "none"})],
            "sessions": {"journal": {"path": str(self.journal)},
                         "assets": {"path": str(self.dirs["gateway"] / "assets")}},
            "shutdownGraceMs": 15000,
            "telemetry": {"endpoint": self.args.otlp, "transport": "http",
                          "serviceName": "lab-gatewayd", "exportTimeoutMs": 5000}})

    def codex_model(self, name, classes, timeout):
        return {"name": name, "kind": "chatgptSubscription", "model": self.args.codex_model,
                "authFile": str(self.codex_auth), "timeoutMs": timeout, "classes": classes}

    def chat_model(self, model):
        if self.args.profile == "codex":
            return self.codex_model("chat", ["reasoning"], 120000)
        return model("chat", fakemodel.CHAT_MODEL, ["reasoning"], CHAT_TIMEOUT_MS)

    def summarizer_model(self, model):
        if self.args.profile == "codex" and self.args.codex_summarizer:
            return self.codex_model("summarizer", ["summary"], SUMMARIZER_TIMEOUT_MS)
        return model("summarizer", fakemodel.SUMMARY_MODEL, ["summary"], SUMMARIZER_TIMEOUT_MS)

    def copy_codex_auth(self):
        directory = self.dirs["gateway"] / "codex"
        directory.mkdir(exist_ok=True)
        self.own(directory, "gateway", 0o700)
        self.codex_original = Path(self.args.codex_auth).read_bytes()
        self.codex_auth.write_bytes(self.codex_original)
        self.own(self.codex_auth, "gateway", 0o600)

    def write_back_codex_auth(self):
        if self.codex_original is None or not self.codex_auth.exists():
            return
        current = self.codex_auth.read_bytes()
        if current == self.codex_original:
            return
        target = Path(self.args.codex_auth)
        original = target.stat()
        temporary = target.with_name(f".{target.name}.lab-{os.getpid()}")
        descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "wb") as out:
            out.write(current)
            out.flush()
            os.fsync(out.fileno())
        if os.geteuid() == 0:
            os.chown(temporary, original.st_uid, original.st_gid)
        os.replace(temporary, target)
        self.codex_original = current
        self.event("codex-auth-written-back")

    def command(self, name, *arguments):
        who = "broker" if name == "brokerd" else "gateway"
        config = self.dirs[who] / ("broker.json" if who == "broker" else "gateway.json")
        env = {"PATH": os.defpath, "HOME": str(self.dirs[who]), "RUST_LOG": self.args.rust_log}
        if who == "gateway":
            env["LAB_MODEL_KEY"] = "lab-fake-model-key"
            if self.args.profile == "codex":
                env.update({key: value for key, value in os.environ.items() if key in PROXY_ENV})
        argv = [str(self.bin / f"dekopon-{name}"), *(arguments or ("--config",)), str(config)]
        if self.ids["broker"] != self.ids["gateway"]:
            argv = ["setpriv", f"--reuid={self.ids[who]}", f"--regid={self.ids[who]}",
                    f"--groups={self.ids['ipc']}", "--", *argv]
        return argv, env, self.dirs[who]

    def check_gateway(self):
        argv, env, cwd = self.command("gatewayd", "check")
        with open(self.run / "gatewayd.check.log", "wb") as out:
            code = subprocess.run(argv, env=env, cwd=cwd, stdout=out, stderr=subprocess.STDOUT,
                                  timeout=120).returncode
        self.event("gateway-config-check", returncode=code)
        if code != 0:
            raise RuntimeError(f"dekopon-gatewayd check exited with {code}; see gatewayd.check.log")

    def start(self, name):
        argv, env, cwd = self.command(name)
        stdout = open(self.run / f"{name}.stdout.log", "ab")
        stderr = open(self.run / f"{name}.stderr.log", "ab")
        process = subprocess.Popen(argv, env=env, stdout=stdout, stderr=stderr, cwd=cwd)
        stdout.close()
        stderr.close()
        self.daemons[name] = process
        self.pids[name] = process.pid
        ready = self.dirs["ipc"] / "broker.sock" if name == "brokerd" else self.dirs["gateway"] / "dev.sock"
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(f"{name} exited with {process.returncode} before readiness")
            if ready.exists() and (name == "brokerd" or self.connectable(ready)):
                self.event("daemon-ready", daemon=name, pid=process.pid)
                return process
            time.sleep(0.1)
        raise TimeoutError(f"{name} readiness deadline")

    @staticmethod
    def connectable(path):
        try:
            with socket.socket(socket.AF_UNIX) as probe:
                probe.settimeout(1)
                probe.connect(str(path))
            return True
        except OSError:
            return False

    def stop(self, name):
        process = self.daemons.get(name)
        if process is None or process.poll() is not None:
            return
        process.terminate()
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
        self.event("daemon-stopped", daemon=name, pid=process.pid, returncode=process.returncode)

    def kill9(self, name):
        process = self.daemons[name]
        os.kill(process.pid, signal.SIGKILL)
        process.wait(timeout=10)
        self.event("daemon-killed", daemon=name, pid=process.pid, signal="SIGKILL")

    def sample(self):
        with open(self.run / "rss.csv", "w", newline="", encoding="utf-8") as out:
            writer = csv.writer(out)
            writer.writerow(["utc", "t", "daemon", "pid", "rss_kib", "hwm_kib", "threads", "cpu_ticks"])
            while not self.sampling.wait(1.0):
                now = time.time()
                for name, pid in list(self.pids.items()):
                    row = proc_status(pid)
                    if row:
                        writer.writerow([utc(now), f"{now:.3f}", name, pid, *row])
                out.flush()

    def snapshot_journal(self, label):
        target = self.run / label
        if target.exists():
            shutil.rmtree(target)
        shutil.copytree(self.journal, target, symlinks=True)
        self.event("journal-snapshot", label=label, bytes=tree_bytes(target))

    def conversation(self, conversation_id):
        return Conversation(self, conversation_id)

    def close(self):
        for name in ("gatewayd", "brokerd"):
            self.stop(name)
        self.write_back_codex_auth()
        self.sampling.set()
        self.model.close()
        self.events.close()


class TurnBudgetExhausted(Exception):
    pass


def proc_status(pid):
    try:
        status = Path(f"/proc/{pid}/status").read_text()
        stat = Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return None
    fields = dict(line.split(":", 1) for line in status.splitlines() if ":" in line)
    kib = lambda key: fields.get(key, "0 kB").split()[0]  # noqa: E731
    after = stat.rsplit(")", 1)[1].split()
    return [kib("VmRSS"), kib("VmHWM"), fields.get("Threads", "0").strip(), int(after[11]) + int(after[12])]


def tree_bytes(path):
    return sum(item.stat().st_size for item in Path(path).rglob("*") if item.is_file())


class Conversation:
    def __init__(self, lab, conversation_id):
        self.lab = lab
        self.id = conversation_id
        self.socket = None
        self.reader = None

    def connect(self):
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.connect(str(self.lab.dirs["gateway"] / "dev.sock"))
        self.reader = self.socket.makefile("rb")

    def close(self):
        if self.socket:
            self.reader.close()
            self.socket.close()
            self.socket = None

    def send(self, turn, phase="main", deadline=TURN_DEADLINE_S, **tag):
        if self.lab.args.max_turns and len(self.lab.turns) >= self.lab.args.max_turns:
            raise TurnBudgetExhausted
        if self.socket is None:
            self.connect()
        tag = {"turn": f"{self.id}-{turn}", **tag}
        prompt = f"[lab:{';'.join(f'{k}={v}' for k, v in tag.items())}] "
        if tag.get("act") == "tool-big":
            prompt += "Run a bash command that prints at least 256 KiB and tell me how many bytes it printed."
        elif tag.get("act") == "tool-probe":
            prompt += 'Run `probe upper --text "lab probe"` and tell me the result.'
        else:
            prompt += f"This is turn {turn}. Reply with one short sentence."
        line = json.dumps({"subject": SUBJECT, "conversation": {"kind": "directMessage", "id": self.id},
                           "text": prompt}) + "\n"
        sent = time.time()
        self.socket.settimeout(deadline)
        self.socket.sendall(line.encode())
        kind, reply, extra = "timeout", None, []
        try:
            while True:
                raw = self.reader.readline(1 << 20)
                if not raw:
                    kind = "disconnected"
                    break
                record = json.loads(raw)
                if "reply" in record and "kind" not in record:
                    kind, reply = "reply", record["reply"]
                    break
                extra.append(record)
                if time.time() - sent > deadline:
                    break
        except (TimeoutError, socket.timeout):
            pass
        except OSError:
            kind = "disconnected"
        received = time.time()
        row = {"turn": turn, "conversation": self.id, "phase": phase, "tag": tag, "sent": sent,
               "received": received, "wall_ms": round((received - sent) * 1000, 1), "kind": kind,
               "reply": reply, "other_lines": len(extra)}
        self.lab.turns.append(row)
        with open(self.lab.run / "replies.jsonl", "a", encoding="utf-8") as out:
            out.write(json.dumps({**row, "lines": extra[-8:]}) + "\n")
        print(f"[turn] {self.id}#{turn} {phase} {kind} {row['wall_ms']}ms {str(reply)[:60]!r}", flush=True)
        if kind != "reply":
            self.close()
        return row


def run_baseline(lab):
    conversations = {route: lab.conversation(route) for route in ROUTES}
    for turn in range(1, 31):
        for route, conversation in conversations.items():
            conversation.send(turn, act="tool-probe" if turn % 5 == 0 else "answer")
    for conversation in conversations.values():
        conversation.close()


def run_flood(lab):
    conversation = lab.conversation("summary")
    for turn in range(1, 21):
        conversation.send(turn, act="tool-big")
    conversation.close()
    tool = [c for c in lab.model.calls if c.get("status") == "answer-after-tool"]
    sizes = [c.get("tool_result_bytes", 0) for c in tool]
    lab.findings["tool results seen by model"] = (
        f"{len(sizes)} results, min {min(sizes, default=0)} B, max {max(sizes, default=0)} B")


def run_summarizer_behaviour(behaviour):
    def scenario(lab):
        summary, other = lab.conversation("summary"), lab.conversation("none")
        for turn in range(1, 11):
            summary.send(turn, sum=behaviour)
            other.send(turn)
        summary.close()
        other.close()
    return scenario


def run_restart_mid_summary(lab):
    conversation = lab.conversation("summary")
    for turn in range(1, 4):
        conversation.send(turn)
    killed_tag = "summary-4"
    conversation.send(4, sum="hang")
    started = lab.model.wait_started(killed_tag, timeout=30)
    lab.findings["summary in flight at kill"] = "yes" if started else "no (summarizer was never called)"
    time.sleep(1)
    lab.snapshot_journal("journal-at-kill")
    conversation.close()
    lab.kill9("gatewayd")
    killed_at = time.time()
    lab.start("gatewayd")
    for turn in range(5, 8):
        conversation.send(turn)
    conversation.close()
    time.sleep(SUMMARIZER_TIMEOUT_MS / 1000 + 2)
    after = [c for c in lab.model.summarizer_calls()
             if c["start"] > killed_at and c.get("tag", {}).get("turn") == killed_tag]
    lab.findings["summarizer re-called for killed turn after restart"] = len(after)
    lab.findings["killed turn in journal after restart"] = journal_mentions(lab.journal, killed_tag)


def journal_mentions(journal, needle):
    hits = 0
    for path in Path(journal).rglob("*.jsonl"):
        hits += path.read_text(errors="replace").count(needle)
    return hits


def run_unloadable_journal(lab):
    conversations = {route: lab.conversation(route) for route in ("summary", "plain")}
    for turn in range(1, 5):
        for conversation in conversations.values():
            conversation.send(turn, phase="before")
    for conversation in conversations.values():
        conversation.close()
    lab.stop("gatewayd")
    lab.snapshot_journal("journal-before-corrupt")
    files = sorted((p for p in lab.journal.rglob("*.jsonl")), key=lambda p: p.stat().st_size)
    if not files:
        lab.findings["corrupted file"] = "none: the journal held no .jsonl file"
        lab.start("gatewayd")
        return
    victim = files[0]
    victim.write_bytes(b"\x00not a journal\n{\"torn")
    lab.findings["corrupted file"] = str(victim.relative_to(lab.journal))
    lab.event("journal-corrupted", file=str(victim.relative_to(lab.journal)))
    booted = time.time()
    lab.start("gatewayd")
    for turn in range(5, 8):
        for conversation in conversations.values():
            conversation.send(turn, phase="after")
    for conversation in conversations.values():
        conversation.close()
    lab.findings["boot after corruption"] = utc(booted)


def run_summaries_off_vs_on(lab):
    for phase, enabled in (("summaries-off", False), ("summaries-on", True)):
        if lab.summaries_on != enabled:
            lab.stop("gatewayd")
            lab.snapshot_journal(f"journal-{'on' if lab.summaries_on else 'off'}")
            shutil.rmtree(lab.journal)
            lab.journal.mkdir()
            lab.own(lab.journal, "gateway", 0o700)
            lab.summaries_on = enabled
            lab.write_gateway_config()
            lab.start("gatewayd")
        conversation = lab.conversation("summary")
        for turn in range(1, 21):
            conversation.send(turn, phase=phase)
        conversation.close()


SCENARIOS = {
    "baseline": run_baseline,
    "flood": run_flood,
    "summarizer-hang": run_summarizer_behaviour("hang"),
    "summarizer-fail": run_summarizer_behaviour("fail"),
    "summarizer-refuse": run_summarizer_behaviour("refuse"),
    "restart-mid-summary": run_restart_mid_summary,
    "unloadable-journal": run_unloadable_journal,
    "summaries-off-vs-on": run_summaries_off_vs_on,
    "config-check": None,
}


def main():
    parser = argparse.ArgumentParser()
    optional = ("rust-log", "real-model", "codex-auth", "codex-model")
    for name in ("run", "state", "bin", "scenario", "profile", "otlp", *optional):
        parser.add_argument(f"--{name}", required=name not in optional)
    for name in ("broker-uid", "gateway-uid", "ipc-gid"):
        parser.add_argument(f"--{name}", type=int, required=True)
    parser.add_argument("--codex-summarizer", action="store_true")
    parser.add_argument("--max-turns", type=int, default=0)
    for name, default in (("cost-cap", 2.0), ("price-in", 0.15), ("price-out", 0.60)):
        parser.add_argument(f"--{name}", type=float, default=default)
    args = parser.parse_args()
    args.rust_log = args.rust_log or "info"
    args.real_model = args.real_model or "openai/gpt-4o-mini"
    args.codex_model = args.codex_model or "gpt-5-codex"
    if args.profile == "codex" and not args.codex_auth:
        raise SystemExit("--profile codex needs --codex-auth")
    if args.scenario not in SCENARIOS:
        raise SystemExit(f"unknown scenario {args.scenario}; one of {', '.join(SCENARIOS)}")
    lab = Lab(args)
    sampler = threading.Thread(target=lab.sample, daemon=True)
    status = "completed"
    signal.signal(signal.SIGTERM, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt()))
    try:
        lab.prepare()
        lab.check_gateway()
        if SCENARIOS[args.scenario] is not None:
            sampler.start()
            lab.start("brokerd")
            lab.start("gatewayd")
            lab.event("scenario-start", scenario=args.scenario)
            SCENARIOS[args.scenario](lab)
            lab.event("scenario-end", scenario=args.scenario)
            drained = lab.model.wait_summaries_idle(SUMMARIZER_TIMEOUT_MS / 1000 + 10)
            lab.event("summaries-drained" if drained else "summaries-still-running")
            if lab.budget and lab.budget.exceeded.is_set():
                status = "aborted: cost cap reached"
            time.sleep(2)
    except TurnBudgetExhausted:
        status = f"aborted: turn budget of {args.max_turns} reached"
        lab.event("turn-budget-exhausted", turns=len(lab.turns))
    except KeyboardInterrupt:
        status = "interrupted"
    except Exception as error:
        status = f"error: {type(error).__name__}: {error}"
        lab.event("scenario-error", error=str(error))
    finally:
        try:
            lab.stop("gatewayd")
            if lab.journal.exists():
                lab.snapshot_journal("journal")
        finally:
            lab.close()
            if sampler.is_alive():
                sampler.join(timeout=3)
    write_outputs(lab, status)
    print(f"[lab] {status}", flush=True)
    return 0 if status == "completed" else 5


def write_outputs(lab, status):
    summaries = lab.model.summarizer_calls()
    with open(lab.run / "latency.csv", "w", newline="", encoding="utf-8") as out:
        writer = csv.writer(out)
        writer.writerow(["turn", "conversation", "phase", "act", "sum", "sent_utc", "reply_utc", "wall_ms",
                         "kind", "summary_in_flight", "any_summary_overlap"])
        for row in lab.turns:
            own = [call for call in summaries
                   if str(call.get("tag", {}).get("turn", "")).rsplit("-", 1)[0] == row["conversation"]]
            row["summary_in_flight"] = any(call["start"] < row["received"] and call["end"] > row["sent"]
                                           for call in own)
            overlap = any(call["start"] < row["received"] and call["end"] > row["sent"] for call in summaries)
            writer.writerow([row["turn"], row["conversation"], row["phase"], row["tag"].get("act", "answer"),
                             row["tag"].get("sum", "ok"), utc(row["sent"]), utc(row["received"]),
                             row["wall_ms"], row["kind"], "yes" if row["summary_in_flight"] else "no",
                             "yes" if overlap else "no"])
    meta = json.loads((lab.run / "meta.json").read_text())
    if lab.args.profile == "codex":
        meta["codex"] = {"model": lab.args.codex_model, "summarizer_on_codex": lab.args.codex_summarizer,
                         "turn_budget": lab.args.max_turns}
    meta.update(status=status, summarizer_timeout_ms=SUMMARIZER_TIMEOUT_MS, findings=lab.findings, cost=lab.budget.snapshot() if lab.budget else None,
                finished_utc=utc())
    (lab.run / "meta.json").write_text(json.dumps(meta, indent=1))


if __name__ == "__main__":
    sys.exit(main())
