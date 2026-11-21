"""Loopback-only recorder in front of the real Go fixture; never logs credentials."""
from collections import deque
from contextlib import contextmanager
import http.client
from http.cookies import SimpleCookie
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from select import select
import socket
from threading import Event, Lock, Thread
from urllib.parse import urlsplit

from fixture import Failure, check
from playback import CONNECT, MAX_FRAME, SOCKET, apply_player


AUTH = {"/api/auth/login": "login", "/api/auth/session": "session", "/api/auth/logout": "logout"}
HOP_HEADERS = {"connection", "keep-alive", "transfer-encoding", "content-length", "upgrade"}


class Receipt:
    def __init__(self, go):
        manual = next(target for target in go["targets"] if target.get("playlistKind") == "manual")
        self.playlist_id = manual["id"]
        self.playlist_size = manual["total"]
        self.first_track = go["duplicateTrackId"]
        self._lock = Lock()
        self._events = []
        self._tokens = set()
        self._rolling_cookie = None
        self._session_failures = deque()
        self.allow_socket = Event()

    def record(self, *event):
        with self._lock:
            self._events.append(event)

    def events(self, since=0):
        with self._lock:
            return self._events[since:]

    def tokens(self):
        with self._lock:
            return self._tokens.copy()

    def rolling_cookie(self):
        with self._lock:
            return self._rolling_cookie

    def fail_next_sessions(self, *statuses):
        with self._lock:
            check(not self._session_failures, "unused rolling session fixture responses")
            self._session_failures.extend(statuses)

    def session_failure(self, method, cookie):
        with self._lock:
            if not self._session_failures:
                return None
            check(method == "GET" and cookie == self._rolling_cookie and cookie is not None,
                  "remembered session did not revalidate the expected fixture cookie")
            status = self._session_failures.popleft()
            self._events.append(("session", method, status))
            return status

    def auth(self, method, path, response, body):
        action = AUTH.get(path)
        if action is None:
            return
        if action in ("login", "session") and response.status == 200:
            cookie = SimpleCookie(response.getheader("Set-Cookie", ""))
            with self._lock:
                if "spofify_session" in cookie:
                    value = cookie["spofify_session"].value
                    self._tokens.add(value)
                    if response.getheader("X-Spofify-Session-Mode") == "rolling-v1":
                        self._rolling_cookie = "spofify_session=" + value
                csrf = json.loads(body).get("csrfToken")
                if csrf:
                    self._tokens.add(csrf)
        if action == "login":
            self.record(action, method, response.status, response.getheader("X-Spofify-Session-Mode"))
        else:
            self.record(action, method, response.status)


class Frames:
    """Read complete text messages from a passive copy of a WebSocket stream."""
    def __init__(self, receipt, *, client):
        self.receipt = receipt
        self.client = client
        self.upgraded = client
        self.buffer = bytearray()
        self.message = bytearray()
        self.fragmented = False
        self.connection = self.owner = None
        self.player = None

    def feed(self, data):
        self.buffer.extend(data)
        if not self.upgraded:
            end = self.buffer.find(b"\r\n\r\n")
            if end < 0:
                check(len(self.buffer) < 16384, "Go WebSocket upgrade exceeded bound")
                return
            check(self.buffer.split(b"\r\n", 1)[0].split()[1:2] == [b"101"], "Go WebSocket upgrade failed")
            del self.buffer[:end + 4]
            self.upgraded = True
        while len(self.buffer) >= 2:
            flags, size = self.buffer[:2]
            masked = bool(size & 128)
            size &= 127
            offset = 2 + {126: 2, 127: 8}.get(size, 0)
            if len(self.buffer) < offset + 4 * masked:
                return
            if offset > 2:
                size = int.from_bytes(self.buffer[2:offset], "big")
            check(masked == self.client and not flags & 112 and size + len(self.message) <= MAX_FRAME,
                  "unsupported Go WebSocket frame at recorder")
            head = offset + 4 * masked
            if len(self.buffer) < head + size:
                return
            mask, payload = self.buffer[offset:head], self.buffer[head:head + size]
            del self.buffer[:head + size]
            opcode = flags & 15
            if opcode in (8, 9, 10):
                continue
            check(opcode == (0 if self.fragmented else 1), "expected Go account text frame")
            self.message.extend((value ^ mask[i % 4] for i, value in enumerate(payload)) if masked else payload)
            self.fragmented = not bool(flags & 128)
            if not self.fragmented:
                message = json.loads(self.message)
                self.message.clear()
                kind = message["type"]
                if self.client:
                    self.receipt.record("client", kind)
                    if kind == "play":
                        selection = message.get("selection", {})
                        context = selection.get("context", {})
                        self.receipt.record("playlist-play", selection.get("kind") == "collection"
                                            and context.get("kind") == "playlist"
                                            and context.get("id") == self.receipt.playlist_id)
                else:
                    self.receive(message)

    def receive(self, message):
        kind = message["type"]
        if kind == "snapshot":
            self.connection, self.player = message["connectionId"], message["player"]
        if kind in ("snapshot", "outputs"):
            self.owner = message["owner"]
            self.receipt.record("owner", bool(self.owner))
        if kind == "player":
            self.player = apply_player(self.player, message["player"])
        if kind in ("snapshot", "outputs", "player") and self.player and self.connection == self.owner:
            queue, cursor = self.player["queue"], self.player["cursor"]
            if (len(queue) == self.receipt.playlist_size and queue[0]["id"] == self.receipt.first_track
                    and queue[0]["key"] == cursor["key"] and cursor["desired"]):
                self.receipt.record("owned-playlist-selection")


@contextmanager
def recorded_origin(go):
    upstream = urlsplit(go["origin"])
    check(go["origin"] == f"http://127.0.0.1:{upstream.port}", "recorder requires a loopback Go fixture")
    receipt = Receipt(go)
    stopping = Event()

    class Handler(BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(5)

        def log_message(self, *_args):
            pass

        def forward(self):
            if (self.headers.get("Host") != f"127.0.0.1:{self.server.server_port}"
                    or self.headers.get("Origin") not in (None, origin)
                    or not self.path.startswith("/") or self.path.startswith("//")):
                self.send_error(400)
                return
            headers = dict(self.headers.items())
            for name in list(headers):
                if name.lower() == "host":
                    headers[name] = upstream.netloc
                if name.lower() == "origin":
                    headers[name] = go["origin"]
            try:
                if self.path == "/api/auth/session":
                    status = receipt.session_failure(self.command, self.headers.get("Cookie"))
                    if status is not None:
                        self.send_response_only(status)
                        self.send_header("Content-Length", "0")
                        self.end_headers()
                        return
                if self.path in (CONNECT, SOCKET) and not receipt.allow_socket.is_set():
                    # Keep Go's output room untouched until the remembered entry.
                    # All password and session routes still hit the real server.
                    self.send_response_only(503)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                if self.path == SOCKET and self.headers.get("Upgrade", "").lower() == "websocket":
                    self.websocket(headers)
                    return
                size = int(self.headers.get("Content-Length", 0))
                check(0 <= size <= 16384, "recorder request exceeded bound")
                body = self.rfile.read(size) if size else None
                conn = http.client.HTTPConnection("127.0.0.1", upstream.port, timeout=8)
                try:
                    conn.request(self.command, self.path, body, headers)
                    response = conn.getresponse()
                    data = response.read(MAX_FRAME + 1)
                    check(len(data) <= MAX_FRAME, "recorder response exceeded bound")
                    path = urlsplit(self.path).path
                    receipt.auth(self.command, path, response, data)
                    if path.endswith(("/audio", "/download")):
                        receipt.record("media")
                    self.send_response_only(response.status)
                    for name, value in response.getheaders():
                        if name.lower() not in HOP_HEADERS:
                            self.send_header(name, value)
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                finally:
                    conn.close()
            except (BrokenPipeError, ConnectionResetError):
                pass  # Window may exit during a read.
            except (Failure, OSError, http.client.HTTPException, ValueError, KeyError):
                # No exceptions/HTTP logging containing paths, bodies or headers.
                receipt.record("recorder-error")

        do_GET = do_POST = do_PUT = do_DELETE = do_PATCH = do_HEAD = forward

        def websocket(self, headers):
            with socket.create_connection(("127.0.0.1", upstream.port), timeout=5) as remote:
                request = f"GET {SOCKET} HTTP/1.1\r\n" + "".join(f"{k}: {v}\r\n" for k, v in headers.items())
                remote.sendall((request + "\r\n").encode("ascii"))
                frames = {self.connection: Frames(receipt, client=True), remote: Frames(receipt, client=False)}
                while not stopping.is_set():
                    readable, _, _ = select(list(frames), [], [], 0.25)
                    for source in readable:
                        data = source.recv(65536)
                        if not data:
                            return
                        frames[source].feed(data)
                        target = remote if source is self.connection else self.connection
                        target.sendall(data)

    class Server(ThreadingHTTPServer):
        daemon_threads = False  # server_close joins its handlers.

        def handle_error(self, _request, _address):
            receipt.record("recorder-error")  # Never emit a raw request traceback.

    with Server(("127.0.0.1", 0), Handler) as server:
        origin = f"http://127.0.0.1:{server.server_port}"
        thread = Thread(target=server.serve_forever, name="remember-login-go-recorder")
        thread.start()
        try:
            yield dict(go, origin=origin), receipt
        finally:
            stopping.set()
            server.shutdown()
            thread.join(timeout=5)
    check(("recorder-error",) not in receipt.events(), "Go receipt recorder failed")
