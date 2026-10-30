import http.server
import json
import logging
import threading
import time
from urllib.parse import parse_qs, urlsplit

from websockets.exceptions import ConnectionClosed
from websockets.sync.server import serve


class SlackSocketFixture:
    TEAM = "T0123ABC"
    CHANNEL = "C0123ABC"
    BOT = "U0BOTBOT"
    MAX_REQUESTS = 256
    MAX_EVENTS = 64

    def __init__(self, app_token, bot_token, forbidden_tokens=(), downstream_failures=()):
        if not app_token or not bot_token or app_token == bot_token:
            raise ValueError("Slack fixture needs distinct nonempty synthetic app and bot credentials")
        self.app_token, self.bot_token = app_token, bot_token
        self.forbidden = tuple(token for token in forbidden_tokens if token)
        self.body_forbidden = self.forbidden + (app_token, bot_token)
        self.downstream_failures = downstream_failures
        self.condition = threading.Condition()
        self.closing = threading.Event()
        self.connection = None
        self.connections = []
        self.handlers = []
        self._calls, self._replies, self._statuses, self._failures = [], [], [], []
        self._acks, self._sent = set(), set()
        self.sequence = 0
        self.http = self.websocket = None
        self.http_started = False
        self.threads = []

    def __enter__(self):
        logger = logging.Logger("slack-socket-fixture")
        logger.addHandler(logging.NullHandler())
        self.websocket = serve(
            self._socket, "127.0.0.1", 0, logger=logger, compression=None,
            open_timeout=5, close_timeout=2, ping_interval=None, max_size=65536, max_queue=8,
        )
        self.socket_url = f"ws://127.0.0.1:{self.websocket.socket.getsockname()[1]}/socket"
        try:
            self.http = http.server.HTTPServer(("127.0.0.1", 0), self._handler())
            self.endpoint = f"http://127.0.0.1:{self.http.server_port}"
            for server in (self.websocket, self.http):
                thread = threading.Thread(target=server.serve_forever)
                thread.start()
                self.threads.append(thread)
                if server is self.http:
                    self.http_started = True
        except BaseException:
            self.close()
            raise
        return self

    def __exit__(self, exception_type, _exception, _traceback):
        try:
            if exception_type is None:
                self.wait_all_acks()
                self.assert_clean()
        finally:
            self.close()

    def _fail(self, message):
        with self.condition:
            if len(self._failures) < 8:
                self._failures.append(message)
            self.condition.notify_all()

    def _contains_secret(self, value, tokens):
        if isinstance(value, str):
            return any(token in value for token in tokens)
        if isinstance(value, dict):
            return any(self._contains_secret(key, tokens) or self._contains_secret(item, tokens)
                       for key, item in value.items())
        if isinstance(value, (list, tuple)):
            return any(self._contains_secret(item, tokens) for item in value)
        return False

    def _handler(self):
        fixture = self

        class Handler(http.server.BaseHTTPRequestHandler):
            timeout = 3

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
                self.handle_api()

            def do_POST(self):
                self.handle_api()

            def handle_api(self):
                try:
                    size = int(self.headers.get("Content-Length", "0"))
                    if not 0 <= size <= 65536:
                        raise ValueError("body size")
                    raw = self.rfile.read(size).decode()
                    other_headers = [(name, value) for name, value in self.headers.items()
                                     if name.lower() != "authorization"]
                    if fixture._contains_secret(str(self.headers), fixture.forbidden) or fixture._contains_secret(
                        [other_headers, self.path, raw], fixture.body_forbidden
                    ):
                        fixture._fail("Secret crossed into Slack fixture traffic")
                        self.respond({"ok": False, "error": "fixture_secret_boundary"}, 400)
                        return
                    path = urlsplit(self.path)
                    expected = fixture.app_token if path.path == "/api/apps.connections.open" else fixture.bot_token
                    if self.headers.get_all("Authorization") != [f"Bearer {expected}"]:
                        fixture._fail("Slack API received the wrong fixture credential")
                        self.respond({"ok": False, "error": "invalid_auth"}, 401)
                        return
                    body = json.loads(raw) if raw else {}
                    if not isinstance(body, dict):
                        raise ValueError("body shape")
                    if fixture._contains_secret(body, fixture.body_forbidden):
                        fixture._fail("Secret crossed into Slack fixture traffic")
                        self.respond({"ok": False, "error": "fixture_secret_boundary"}, 400)
                        return
                    response = fixture._api(self.command, path, body)
                    self.respond(response)
                except (ValueError, OSError):
                    fixture._fail("Malformed Slack fixture HTTP request")
                    self.respond({"ok": False, "error": "fixture_bad_request"}, 400)

        return Handler

    def _api(self, method, path, body):
        with self.condition:
            if len(self._calls) >= self.MAX_REQUESTS:
                self._fail("Slack fixture request limit exceeded")
                return {"ok": False, "error": "fixture_request_limit"}
            self._calls.append({"method": method, "path": path.path, "body": body})
            if method == "POST" and path.path == "/api/auth.test":
                result = {"ok": True, "user_id": self.BOT, "team_id": self.TEAM}
            elif method == "POST" and path.path == "/api/apps.connections.open":
                result = {"ok": True, "url": self.socket_url}
            elif method == "GET" and path.path == "/api/conversations.info":
                channel = parse_qs(path.query).get("channel", [""])[0]
                if not channel.lower().startswith("c") or not channel.isalnum():
                    self._fail("Unexpected Slack fixture conversation")
                result = {"ok": True, "channel": {
                    "id": channel, "is_im": False, "is_mpim": False, "is_channel": True, "is_group": False,
                }}
            elif method == "POST" and path.path == "/api/chat.postMessage":
                if not isinstance(body.get("channel"), str) or not isinstance(body.get("text"), str):
                    self._fail("Slack reply omitted channel or text")
                self._replies.append(body)
                result = {"ok": True, "channel": body.get("channel"), "ts": f"1800000000.{len(self._replies):06d}"}
            elif method == "POST" and path.path == "/api/agents.sessions.setStatus":
                if not all(isinstance(body.get(key), str) for key in ("channel_id", "thread_ts", "status")):
                    self._fail("Agent status omitted its thread or state")
                self._statuses.append(body)
                result = {"ok": True}
            elif method == "POST" and path.path == "/api/assistant.threads.setStatus":
                result = {"ok": True}
            else:
                self._fail("Unexpected Slack fixture API method")
                result = {"ok": False, "error": "unknown_method"}
            self.condition.notify_all()
            return result

    def _socket(self, connection):
        with self.condition:
            self.handlers.append(threading.current_thread())
            if self.connections:
                self._fail("Unexpected repeated Slack socket")
                return
            self.connections.append(connection)
        try:
            request = connection.request
            if request.path != "/socket" or self._contains_secret(str(request.headers), self.body_forbidden):
                self._fail("Unexpected Slack socket handshake")
                return
            connection.send(json.dumps({"type": "hello", "num_connections": 1}))
            with self.condition:
                self.connection = connection
                self.condition.notify_all()
            while not self.closing.is_set():
                try:
                    raw = connection.recv(timeout=0.25)
                except TimeoutError:
                    continue
                if not isinstance(raw, str) or self._contains_secret(raw, self.body_forbidden):
                    self._fail("Unexpected data or secret in Slack socket traffic")
                    return
                frame = json.loads(raw)
                if not isinstance(frame, dict) or not isinstance(frame.get("envelope_id"), str):
                    raise ValueError("acknowledgement shape")
                if self._contains_secret(frame, self.body_forbidden):
                    self._fail("Unexpected data or secret in Slack socket traffic")
                    return
                with self.condition:
                    envelope = frame.get("envelope_id")
                    if envelope not in self._sent or envelope in self._acks:
                        self._fail("Unexpected or duplicate Slack envelope acknowledgement")
                        return
                    self._acks.add(envelope)
                    self.condition.notify_all()
        except ConnectionClosed:
            pass
        except (ValueError, OSError):
            self._fail("Malformed Slack socket traffic")
        finally:
            with self.condition:
                if self.connection is connection:
                    self.connection = None
                self.condition.notify_all()

    def _wait(self, predicate, description, timeout):
        with self.condition:
            deadline = time.monotonic() + timeout
            while True:
                self.assert_clean()
                result = predicate()
                if result:
                    return result
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    downstream = "; ".join(self.downstream_failures)
                    raise RuntimeError(f"Timed out waiting for Slack fixture {description}"
                                       + (f"; downstream failures: {downstream}" if downstream else ""))
                self.condition.wait(remaining)

    def wait_connected(self, timeout=15):
        return self._wait(lambda: self.connection, "connection", timeout)

    def _emit(self, event, timeout=15):
        connection = self.wait_connected(timeout)
        with self.condition:
            if self.sequence >= self.MAX_EVENTS:
                raise RuntimeError("Slack fixture event limit exceeded")
            self.sequence += 1
            envelope = f"fixture-envelope-{self.sequence}"
            timestamp = f"1700000000.{self.sequence:06d}"
            event.setdefault("ts", timestamp)
            frame = {"envelope_id": envelope, "type": "events_api", "accepts_response_payload": False,
                     "payload": {"team_id": self.TEAM, "event": event}}
            encoded = json.dumps(frame)
            if self._contains_secret(encoded, self.body_forbidden):
                raise RuntimeError("Secret appeared in a synthetic Slack event")
            self._sent.add(envelope)
        connection.send(encoded)
        return envelope, timestamp

    def emit_mention(self, user, text, thread_ts=None, channel=CHANNEL):
        event = {"type": "app_mention", "channel": channel.upper(), "user": user.upper(), "text": f"<@{self.BOT}> {text}"}
        if thread_ts is not None:
            event["thread_ts"] = thread_ts
        return self._emit(event)

    def emit_message(self, user, text, channel, channel_type, thread_ts=None):
        event = {"type": "message", "channel": channel.upper(), "channel_type": channel_type,
                 "user": user.upper(), "text": text}
        if thread_ts is not None:
            event["thread_ts"] = thread_ts
        return self._emit(event)

    def send_stop(self, user, thread_ts, channel=CHANNEL):
        return self._emit({"type": "agent_session_stopped", "channel": channel.upper(),
                           "thread_ts": thread_ts, "user": user.upper()})

    def wait_ack(self, envelope_id, timeout=15):
        self._wait(lambda: envelope_id in self._acks, "envelope acknowledgement", timeout)

    def wait_all_acks(self, timeout=15):
        self._wait(lambda: self._sent == self._acks, "all envelope acknowledgements", timeout)

    def wait_reply(self, text=None, thread_ts=None, timeout=15, after=0):
        return self._wait(lambda: next((reply for reply in self._replies[after:]
                         if (text is None or reply.get("text") == text)
                         and (thread_ts is None or reply.get("thread_ts") == thread_ts)), None), "reply", timeout)

    def wait_status(self, status, thread_ts, timeout=15):
        return self._wait(lambda: next((body for body in self._statuses
                         if body.get("status") == status and body.get("thread_ts") == thread_ts), None), "Agent status", timeout)

    @property
    def replies(self):
        with self.condition:
            return list(self._replies)

    @property
    def calls(self):
        with self.condition:
            return list(self._calls)

    @property
    def acks(self):
        with self.condition:
            return set(self._acks)

    @property
    def sent(self):
        with self.condition:
            return set(self._sent)

    def assert_clean(self):
        with self.condition:
            if self._failures:
                raise RuntimeError("; ".join(self._failures))

    def close(self):
        self.closing.set()
        if self.websocket is not None:
            self.websocket.shutdown()
        for connection in self.connections:
            connection.close()
        if self.http is not None:
            if self.http_started:
                self.http.shutdown()
            self.http.server_close()
        for thread in self.threads + self.handlers:
            thread.join(timeout=5)
            if thread.is_alive():
                raise RuntimeError("Slack fixture thread did not stop")
