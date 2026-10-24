import http.server
import json
import os
import re
import ssl
import threading
import time
import urllib.error
import urllib.request

CHAT_MODEL = "lab-chat"
SUMMARY_MODEL = "lab-summarizer"
TAG = re.compile(r"\[lab:([^\]]*)\]")
BIG_COMMAND = ("l=0123456789; i=0; while [ \"$i\" -lt 4 ]; do l=\"$l$l\"; i=$((i + 1)); done; "
               "n=0; while [ \"$n\" -lt 1800 ]; do echo \"$n $l\"; n=$((n + 1)); done")
PROBE_COMMAND = 'probe upper --text "lab probe" | jq -r .text'
OPENROUTER = "https://openrouter.ai/api/v1/chat/completions"
REAL_MAX_TOKENS = 1024


def tag_of(text):
    found = TAG.findall(text or "")
    if not found:
        return {}
    pairs = (item.split("=", 1) for item in found[-1].split(";") if "=" in item)
    return {key.strip(): value.strip() for key, value in pairs}


def content_text(message):
    content = message.get("content")
    if isinstance(content, list):
        return "".join(part.get("text", "") for part in content if isinstance(part, dict))
    return content or ""


class Budget:
    def __init__(self, cap_usd, price_in, price_out):
        self.cap = cap_usd
        self.price_in = price_in / 1e6
        self.price_out = price_out / 1e6
        self.spent = 0.0
        self.prompt_tokens = 0
        self.completion_tokens = 0
        self.exceeded = threading.Event()
        self.lock = threading.Lock()

    def admit(self, request_bytes):
        worst = request_bytes / 3 * self.price_in + REAL_MAX_TOKENS * self.price_out
        with self.lock:
            if self.spent + worst > self.cap:
                self.exceeded.set()
                return False
            return True

    def charge(self, usage):
        prompt = int(usage.get("prompt_tokens") or 0)
        completion = int(usage.get("completion_tokens") or 0)
        with self.lock:
            self.prompt_tokens += prompt
            self.completion_tokens += completion
            self.spent += prompt * self.price_in + completion * self.price_out
            if self.spent >= self.cap:
                self.exceeded.set()

    def snapshot(self):
        with self.lock:
            return {"cap_usd": self.cap, "spent_usd": round(self.spent, 6),
                    "prompt_tokens": self.prompt_tokens, "completion_tokens": self.completion_tokens,
                    "exceeded": self.exceeded.is_set()}


class FakeModel:
    def __init__(self, calls_path, real=None):
        self.calls_path = calls_path
        self.real = real
        self.lock = threading.Lock()
        self.calls = []
        self.started = threading.Condition(self.lock)
        self.summary_starts = []
        self.stopping = threading.Event()
        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), self.handler())
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def port(self):
        return self.server.server_port

    def start(self):
        self.thread.start()
        return self

    def close(self):
        self.stopping.set()
        self.server.shutdown()
        self.server.server_close()

    def record(self, row):
        with self.started:
            self.calls.append(row)
            self.started.notify_all()
            with open(self.calls_path, "a", encoding="utf-8") as out:
                out.write(json.dumps(row) + "\n")

    def wait_started(self, turn, timeout):
        with self.started:
            return self.started.wait_for(lambda: turn in self.summary_starts, timeout)

    def wait_summaries_idle(self, timeout):
        with self.started:
            return self.started.wait_for(lambda: len(self.summary_starts) <= sum(
                1 for row in self.calls if row["model"] == SUMMARY_MODEL), timeout)

    def summarizer_calls(self):
        with self.lock:
            return [row for row in self.calls if row["model"] == SUMMARY_MODEL]

    def handler(self):
        lab = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def do_POST(self):
                started = time.time()
                size = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(size)
                request = json.loads(body)
                model = request.get("model")
                row = {"start": started, "model": model, "request_bytes": size}
                try:
                    if model == SUMMARY_MODEL:
                        status = lab.summarize(self, request, row)
                    else:
                        status = lab.chat(self, request, row)
                except (BrokenPipeError, ConnectionResetError):
                    status = "client-gone"
                row.update(end=time.time(), status=status)
                row["duration_ms"] = round((row["end"] - started) * 1000, 1)
                lab.record(row)

        return Handler

    def reply(self, handler, code, payload):
        data = json.dumps(payload).encode()
        handler.send_response(code)
        handler.send_header("Content-Type", "application/json")
        handler.send_header("Content-Length", str(len(data)))
        handler.end_headers()
        handler.wfile.write(data)

    def hang(self, handler):
        handler.connection.settimeout(0.5)
        while not self.stopping.is_set():
            try:
                if handler.connection.recv(1) == b"":
                    return "hung-client-gone"
            except TimeoutError:
                continue
            except OSError:
                return "hung-client-gone"
        return "hung-until-shutdown"

    def scripted_failure(self, handler, behaviour):
        if behaviour == "fail":
            self.reply(handler, 503, {"error": {"message": "lab scripted outage", "code": 503}})
            return "503"
        if behaviour == "hang":
            return self.hang(handler)
        if behaviour == "refuse":
            self.reply(handler, 200, {"choices": [{"index": 0, "finish_reason": "stop", "message": {
                "role": "assistant", "content": None, "refusal": "I can't help with that."}}]})
            return "refused"
        return None

    def answer(self, handler, message, usage=None):
        payload = {"choices": [{"index": 0, "finish_reason": "tool_calls" if message.get("tool_calls")
                                else "stop", "message": message}]}
        payload["usage"] = usage or {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0}
        self.reply(handler, 200, payload)

    def chat(self, handler, request, row):
        messages = request.get("messages", [])
        users = [content_text(m) for m in messages if m.get("role") == "user"]
        tag = tag_of(users[-1] if users else "")
        row.update(tag=tag, kind="chat")
        time.sleep(int(tag.get("lat", "50")) / 1000)
        act = tag.get("act", "answer")
        failed = self.scripted_failure(handler, act)
        if failed:
            return failed
        if self.real:
            return self.forward(handler, request, row)
        last = messages[-1] if messages else {}
        turn = tag.get("turn", "?")
        if last.get("role") == "tool":
            output = content_text(last)
            row["tool_result_bytes"] = len(output.encode())
            self.answer(handler, {"role": "assistant", "content":
                                  f"turn {turn} done; tool output was {len(output.encode())} bytes"})
            return "answer-after-tool"
        if act in ("tool-big", "tool-probe"):
            command = BIG_COMMAND if act == "tool-big" else PROBE_COMMAND
            self.answer(handler, {"role": "assistant", "content": None, "tool_calls": [{
                "id": f"lab-{turn}", "type": "function",
                "function": {"name": "bash", "arguments": json.dumps({"command": command})}}]})
            return "tool-call"
        self.answer(handler, {"role": "assistant", "content": f"turn {turn} answered"})
        return "answer"

    def summarize(self, handler, request, row):
        messages = request.get("messages", [])
        system = next((content_text(m) for m in messages if m.get("role") == "system"), "")
        source = "\n".join(content_text(m) for m in messages if m.get("role") == "user")
        tag = tag_of(source)
        row.update(tag=tag, kind="summary")
        with self.started:
            self.summary_starts.append(tag.get("turn"))
            self.started.notify_all()
        time.sleep(int(tag.get("slat", "1500")) / 1000)
        failed = self.scripted_failure(handler, tag.get("sum", "ok"))
        if failed:
            return failed
        if self.real:
            return self.forward(handler, request, row)
        ids = re.findall(r"part IDs: ([^.]*)\.", system)
        labels = [label.strip() for label in ids[0].split(",")] if ids else ["core"]
        parts = [{"id": label, "text": f"turn {tag.get('turn', '?')} {label} summary"} for label in labels]
        self.answer(handler, {"role": "assistant", "content": json.dumps({"parts": parts})})
        return "summary"

    def forward(self, handler, request, row):
        budget = self.real["budget"]
        request = dict(request, model=self.real["model"], stream=False, max_tokens=REAL_MAX_TOKENS)
        data = json.dumps(request).encode()
        if not budget.admit(len(data)):
            self.reply(handler, 402, {"error": {"message": "lab cost cap reached"}})
            return "cap-refused"
        upstream = urllib.request.Request(OPENROUTER, data=data, method="POST", headers={
            "Content-Type": "application/json",
            "Authorization": "Bearer " + os.environ["OPENROUTER_API_KEY"]})
        context = ssl.create_default_context(cafile=os.environ.get("SSL_CERT_FILE") or None)
        try:
            with urllib.request.urlopen(upstream, timeout=120, context=context) as response:
                code, payload = response.status, json.loads(response.read())
        except urllib.error.HTTPError as error:
            code, payload = error.code, {"error": {"message": f"upstream {error.code}"}}
        budget.charge(payload.get("usage") or {})
        row["usage"] = payload.get("usage")
        self.reply(handler, code, payload)
        return f"real-{code}"
