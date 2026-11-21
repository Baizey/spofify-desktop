"""Controlled account-state peer for native audio smoke. Not the Go service."""
from base64 import b64encode
from collections import deque
from contextlib import contextmanager
from hashlib import sha1
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from io import BytesIO
import json
from math import isfinite, pi, sin
from queue import Empty, Queue
from secrets import token_hex
import socket
import struct
from threading import Lock, Thread
from time import monotonic
from urllib.parse import parse_qs, urlsplit
import wave

from fixture import Failure, check
from playback import CONNECT, PROTOCOL, SOCKET, WS_GUID

TRACK_ID, TITLE, SECONDS = "synthetic-audio-smoke", "Synthetic Audio Smoke Tone", 12
ARTISTS = [{"id": "synthetic-audio-artist", "name": "Synthetic oscillator"}]
CATALOG = dict(id=TRACK_ID, name=TITLE, artists=ARTISTS, albumName="Synthetic smoke",
               albumId="synthetic-audio-album", durationMs=1000 * SECONDS, hasAudio=True, kind="track",
               coverage=dict(tracks=1, inLibrary=1, albums=0))
TRACK = dict(id=TRACK_ID, name=TITLE, artists=ARTISTS, albumName=CATALOG["albumName"],
             albumId=CATALOG["albumId"], duration=SECONDS, intent="low")
CURSOR = dict(key=0, chain="", visit=0, run=0, elapsed=0, desired=False, playing=False)
BINDING = ("key", "chain", "visit", "run")
CHOICES = frozenset(("play", "add", "pause", "resume", "seek", "next", "previous", "close", "clear",
                     "remove", "jump", "move", "repeat", "shuffle", "select", "rename", "personal"))


def tone():
    rate = 48000
    data = BytesIO()
    with wave.open(data, "wb") as raw:
        raw.setparams((1, 2, rate, 0, "NONE", "not compressed"))
        # Distinguish actual decoder seeks from an updated elapsed label on
        # audio that is still playing from the previous position.
        for hz, seconds in ((480, 2), (360, 4), (600, SECONDS - 6)):
            cycle = struct.pack("<400h", *(int(10000 * sin(2 * pi * i * hz / rate)) for i in range(400)))
            raw.writeframes(cycle * (rate * seconds // 400))
    return data.getvalue()


class Peer:
    def __init__(self, handler):
        self.sock, self.reader = handler.connection, handler.rfile
        self.incoming = Queue(maxsize=128)
        self.id = "synthetic-" + token_hex(16)
        self.handle = "synthetic-" + token_hex(16)
        self.player = dict(queue=[], cursor=CURSOR.copy(), repeat="off", shuffle=False)
        self.owner = ""
        self.grant_ms = 8000
        self.last_grant = None
        self.bases = set()
        self.last_key = 0
        self.hello = None

    def frame(self, kind, data):
        check(len(data) <= (125 if kind >= 8 else 65535), "synthetic outbound frame bound")
        length = bytes([len(data)]) if len(data) < 126 else b"\x7e" + struct.pack("!H", len(data))
        self.sock.sendall(bytes([128 | kind]) + length + data)

    def send(self, kind, **fields):
        self.frame(1, json.dumps(dict(type=kind, **fields)).encode())

    def read(self, size):
        data = self.reader.read(size)
        if len(data) != size:
            raise EOFError("desktop account socket closed")
        return data

    def messages(self):
        while True:
            first, length = self.read(2)
            kind = first & 15
            check(first in (129, 136, 137, 138) and length & 128, "unexpected desktop WebSocket frame")
            length &= 127
            if length == 126:
                length = struct.unpack("!H", self.read(2))[0]
            check(length <= (125 if kind >= 8 else 16384), "desktop WebSocket frame too large")
            mask = self.read(4)
            data = bytes(x ^ mask[i % 4] for i, x in enumerate(self.read(length)))
            if kind == 8:
                raise EOFError("desktop sent WebSocket close")
            if kind == 9:
                self.frame(10, data)
            elif kind == 10:
                yield {"type": "pong", "data": data}
            else:
                yield json.loads(data)

    def roster(self):
        return [dict(id=self.id, deviceId=self.hello["deviceId"], name=self.hello["name"], canPlay=True)]

    def snapshot(self):
        self.send("snapshot", connectionId=self.id, player=self.player, owner=self.owner, devices=self.roster())
        self.send("personal", update=dict(snapshot=dict(playlists=[], recent=[])))

    def open(self, *, selected=True, prior=None):
        self.owner = self.id if selected else ""
        if prior is not None:
            self.player = {**prior.player, "cursor": dict(prior.player["cursor"], playing=False)}
            self.last_key = prior.last_key
        self.snapshot()
        self.send("outputs", owner=self.owner, devices=self.roster())

    def drop_output(self):
        self.owner = ""
        self.cursor(run=self.player["cursor"]["run"] + 1, playing=False)
        self.send("outputs", owner=self.owner, devices=self.roster())

    def claim_output(self):
        check(not self.owner, "cannot claim an already selected synthetic output")
        self.owner = self.id
        self.cursor(run=self.player["cursor"]["run"] + 1, playing=False)
        self.send("outputs", owner=self.owner, devices=self.roster())

    def transport(self, msg):
        kind = msg["type"]
        if kind == "sync":
            check(set(msg) == {"type"}, "unexpected account sync fields")
            self.snapshot()
        elif kind == "presence":
            basis = msg.get("basis")
            check(type(basis) is int and basis >= 0 and basis not in self.bases and
                  set(msg) in ({"type", "basis"}, {"type", "basis", "handle", "base", "actual"}),
                  "invalid account presence basis or reattachment fields")
            self.bases.add(basis)
            if self.owner == self.id and self.grant_ms is not None:
                self.send("grant", basis=basis, ms=self.grant_ms, handle=self.handle)
                # The Go grant is measured from *client send*, so this time
                # bounds expiration from above, not from below.
                self.last_grant = monotonic()

    def expect(self, label, predicate, timeout=8):
        deadline = monotonic() + timeout
        seen = deque(maxlen=16)
        while monotonic() < deadline:
            try:
                message = self.incoming.get(timeout=max(0, deadline - monotonic()))
            except Empty:
                break
            if isinstance(message, Exception):
                raise Failure(f"{label}: {message}") from message
            kind = message["type"]
            seen.append((kind, message.get("elapsed")))
            self.transport(message)
            if predicate(message):
                return message
        raise Failure(f"no {label} (last types/elapsed: {list(seen)})")

    def ping(self, label):
        data = label.encode()
        self.frame(9, data)
        self.expect(label + " pong", lambda m: m == {"type": "pong", "data": data})

    def command(self, kind, *, before=None):
        def choice(msg):
            if before is not None and msg["type"] == "fact":
                before(msg)
            return msg["type"] in CHOICES

        msg = self.expect(kind + " account choice", choice)
        check(msg["type"] == kind, f"expected {kind} but received {msg['type']}")
        fields = {"type"} | {"seek": {"value"}, "repeat": {"repeat"}, "shuffle": {"shuffle"}}.get(kind, set())
        if kind == "play":
            fields.add("selection")
            fields.update(k for k in ("shuffle", "seed") if k in msg)
        check(set(msg) == fields, f"unexpected fields in account {kind} choice")
        return msg

    def mode(self, kind, value, cursor):
        command = self.command(kind, before=lambda msg: self.observe(cursor, msg))
        check(command[kind] == value, f"expected {kind} {value}, got {command[kind]}")
        self.player[kind] = value
        self.send("player", player={kind: value})

    def play(self, command):
        check(command["selection"] == {"kind": "track", "track": {"id": TRACK_ID}} and
              not command.get("shuffle") and "seed" not in command, "unexpected synthetic track selection")
        self.last_key += 1
        rows = [dict(TRACK, key=self.last_key, order=self.last_key)]
        cursor = dict(CURSOR, key=self.last_key, chain="chain-" + token_hex(12), desired=True)
        self.player = dict(self.player, queue=rows, cursor=cursor)
        self.send("player", player=dict(source=self.id, queue=dict(kind="replace", rows=rows),
                                        cursor=cursor, shuffle=False))
        return cursor

    def cursor(self, *, observed=False, **changes):
        cursor = dict(self.player["cursor"], **changes)
        self.player = dict(self.player, cursor=cursor)
        self.send("player", player=dict(cursor=cursor, **({"observed": True} if observed else {})))
        return cursor

    def control(self, message):
        kind = message["type"]
        changes = dict(run=self.player["cursor"]["run"] + 1, playing=False)
        if kind == "seek":
            changes["elapsed"] = message["value"]
        else:
            changes["desired"] = kind == "resume"
        return self.cursor(**changes)

    def observe(self, cursor, msg):
        if not all(msg[k] == cursor[k] for k in BINDING):
            return False  # final sample for the previous run can already be in flight
        check(not msg.get("error") and isfinite(msg["elapsed"]) and msg["elapsed"] >= 0 and
              all(type(msg[k]) is bool for k in ("playing", "buffering", "ended")) and
              msg["ended"] == ("landed" in msg), "invalid native account fact")
        if msg["ended"]:
            # Go's repeat-off terminal cursor keeps the row and visit but
            # rewinds for Resume; the native fact still reports the real EOF.
            self.cursor(observed=msg["landed"] == 0, elapsed=0, playing=False,
                        desired=False, run=cursor["run"] + 1)
        else:
            self.cursor(observed=True, elapsed=msg["elapsed"], playing=msg["playing"])
        return True

    def fact(self, cursor, predicate, label, timeout=8):
        def matches(msg):
            check(msg["type"] not in CHOICES, f"unexpected {msg['type']} while waiting for {label}")
            return msg["type"] == "fact" and self.observe(cursor, msg) and predicate(msg)

        return self.expect(label, matches, timeout)

    def no_replay(self, duration, *, pong=None, label="unowned reconnect"):
        deadline = monotonic() + duration
        received_pong = False
        while monotonic() < deadline:
            try:
                msg = self.incoming.get(timeout=max(0, deadline - monotonic()))
            except Empty:
                break
            if isinstance(msg, Exception):
                raise Failure(f"account socket dropped during {label}: {msg}") from msg
            check(msg["type"] in ("presence", "sync", "pong", "release"),
                  f"unexpected account choice or native fact during {label}: {msg['type']}")
            if msg["type"] == "pong" and msg["data"] == pong:
                received_pong = True
            self.transport(msg)
        check(pong is None or received_pong, "new account connection did not answer ping")

    def disconnect(self):
        # Drop the transport, without publishing pause/release or granting more time.
        self.sock.shutdown(socket.SHUT_RDWR)


@contextmanager
def audio_fixture(*, wav=None, peer_class=Peer, port=0, accepted_connections=None):
    ready = {"username": "synthetic-audio", "password": token_hex(16)}
    token, csrf = token_hex(32), token_hex(32)
    cookie = "spofify_session=" + token
    session = dict(accountId="synthetic-audio-account", username=ready["username"], role="user", csrfToken=csrf)
    if wav is None:
        wav = tone()
    tickets, sockets = set(), set()
    lock = Lock()
    admitted, media, requests = Queue(maxsize=8), [], deque(maxlen=24)

    class Handler(BaseHTTPRequestHandler):
        def setup(self):
            if accepted_connections is not None:
                accepted_connections.append(None)  # no peer addresses or request data
            super().setup()
            self.connection.settimeout(4)

        def log_request(self, code="-", size="-"):
            requests.append((self.command, urlsplit(self.path).path, code))

        def log_message(self, *_args):
            pass

        def reply(self, value=None, status=200, headers=None):
            body = value if isinstance(value, bytes) else json.dumps(value).encode()
            self.send_response(status)
            self.send_header("Content-Type", "audio/wav" if isinstance(value, bytes) else "application/json")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            for key, val in (headers or {}).items():
                self.send_header(key, val)
            self.end_headers()
            self.wfile.write(body)

        def trusted(self, *, write=False):
            return (self.headers.get("Host") == ready["origin"].removeprefix("http://") and
                    self.headers.get("Origin") in ((ready["origin"],) if write else (None, ready["origin"])))

        def do_POST(self):
            if not self.trusted(write=True):
                return self.reply(status=403)
            size = int(self.headers.get("Content-Length", "0"))
            if not 0 < size <= 1024:
                return self.reply(status=400)
            body = json.loads(self.rfile.read(size))
            if self.path == "/api/auth/login" and self.headers.get("X-Spofify-Login") == "1":
                if body == {key: ready[key] for key in ("username", "password")}:
                    return self.reply(session, headers={"Set-Cookie": cookie + "; Path=/; HttpOnly; Max-Age=3600; SameSite=Lax"})
            if self.headers.get("Cookie") != cookie or self.headers.get("X-CSRF-Token") != csrf:
                return self.reply(status=401)
            if self.path == CONNECT and body == {}:
                issued = token_hex(32)
                with lock:
                    if len(tickets) >= 8:
                        return self.reply(status=503)
                    tickets.add(issued)
                return self.reply({"ticket": issued})
            self.reply(status=404)

        def upgrade(self):
            key = self.headers.get("Sec-WebSocket-Key", "")
            if (self.headers.get("Origin") != ready["origin"] or not key or
                    self.headers.get("Upgrade", "").lower() != "websocket" or
                    self.headers.get("Sec-WebSocket-Version") != "13" or
                    self.headers.get("Sec-WebSocket-Protocol") != PROTOCOL):
                return self.reply(status=400)
            accept = b64encode(sha1((key + WS_GUID).encode()).digest()).decode()
            self.protocol_version = "HTTP/1.1"
            self.send_response(101)
            for name, value in (("Upgrade", "websocket"), ("Connection", "Upgrade"),
                                ("Sec-WebSocket-Protocol", PROTOCOL), ("Sec-WebSocket-Accept", accept)):
                self.send_header(name, value)
            self.end_headers()
            peer = peer_class(self)
            with lock:
                sockets.add(self.connection)
            try:
                stream = peer.messages()
                hello = next(stream)
                check(set(hello) == {"type", "ticket", "name", "deviceId", "canPlay"} and
                      hello["type"] == "hello" and hello["deviceId"] and hello["name"] and
                      hello["canPlay"] is True, "invalid synthetic account hello or capability")
                with lock:
                    check(hello["ticket"] in tickets, "invalid or already used synthetic account ticket")
                    tickets.remove(hello["ticket"])
                peer.hello = {name: hello[name] for name in ("deviceId", "name")}
                self.connection.settimeout(45)
                admitted.put(peer, timeout=1)
                for message in stream:
                    peer.incoming.put(message, timeout=1)
            except Exception as error:
                target = admitted if peer.hello is None else peer.incoming
                if not target.full():
                    target.put_nowait(error)
            finally:
                with lock:
                    sockets.discard(self.connection)

        def do_GET(self):
            if not self.trusted() or self.headers.get("Cookie") != cookie:
                return self.reply(status=401)
            path = urlsplit(self.path)
            if path.path == SOCKET and not path.query:
                return self.upgrade()
            if path.path == "/api/auth/session":
                return self.reply(session)
            track = "/api/library/tracks/" + TRACK_ID
            if path.path == track + "/audio":
                self.reply(wav)
                media.append(len(wav))
                return
            if path.path == track + "/lyrics":
                return self.reply(dict(version=1, trackId=TRACK_ID, recordingVersion="recording-v1",
                                       sourceGeneration="local-v1", state="plain", plainText="Fixture verse",
                                       attribution=dict(name="Fixture lyrics", ownership="library"),
                                       lines=[], offlineTransferAllowed=False))
            if path.path == "/search":
                query = parse_qs(path.query).get("query", [""])[0].strip().lower()
                items = [CATALOG] if query and query in TITLE.lower() else []
                return self.reply(dict(items=items, resultCount=len(items), expiresAt="2099-01-01T00:00:00Z"))
            self.reply(status=404)

    class Server(ThreadingHTTPServer):
        daemon_threads = False

    with Server(("127.0.0.1", port), Handler) as server:
        ready["origin"] = f"http://127.0.0.1:{server.server_port}"
        worker = Thread(target=server.serve_forever)
        worker.start()
        try:
            yield ready, admitted, media, requests
        finally:
            server.shutdown()
            worker.join(timeout=5)
            with lock:
                for sock in sockets:
                    try:
                        sock.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
