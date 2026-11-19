"""Synthetic loopback fixture for audio_smoke.py; not the Go playback service."""
from base64 import b64encode
from collections import deque
from contextlib import contextmanager
from hashlib import sha1
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from io import BytesIO
import json
from math import pi, sin
from queue import Empty, Queue
from secrets import token_hex
import socket
import struct
from threading import Thread
from time import monotonic
from urllib.parse import parse_qs, urlsplit
import wave

from fixture import Failure, check

PROTOCOL = "spofify.library-playback.v2"
SESSION, CONNECTION, EVENT = "synthetic-room", "synthetic-output", "synthetic-occurrence"
TRACK_ID, TITLE, SECONDS = "synthetic-audio-smoke", "Synthetic Audio Smoke Tone", 9
ARTISTS = [{"id": "synthetic-audio-artist", "name": "Synthetic oscillator"}]
CATALOG = dict(id=TRACK_ID, name=TITLE, artists=ARTISTS, albumName="Synthetic smoke",
               albumId="synthetic-audio-album", durationMs=1000 * SECONDS, hasAudio=True, kind="track",
               coverage=dict(tracks=1, inLibrary=1, albums=0))
TRACK = dict(id=TRACK_ID, name=TITLE, artists=ARTISTS, albumName=CATALOG["albumName"],
             albumId=CATALOG["albumId"], duration=SECONDS, key=1, order=0, intent="now")


def tone():
    rate = 48000
    cycle = struct.pack("<100h", *(int(10000 * sin(2 * pi * i / 100)) for i in range(100)))
    data = BytesIO()
    with wave.open(data, "wb") as raw:
        raw.setparams((1, 2, rate, 0, "NONE", "not compressed"))
        raw.writeframes(cycle * (rate * SECONDS // 100))  # 480 Hz
    return data.getvalue()


class Peer:
    def __init__(self, handler):
        self.sock, self.reader = handler.connection, handler.rfile
        self.incoming = Queue(maxsize=64)
        self.version = 1
        self.sequence = self.state_version = 0
        self.facts = None
        self.last_command = 0
        self.event = ""
        self.track = None

    def frame(self, kind, data):
        check(len(data) <= 65535, "synthetic outbound frame bound")
        length = bytes([len(data)]) if len(data) < 126 else b"\x7e" + struct.pack("!H", len(data))
        self.sock.sendall(bytes([128 | kind]) + length + data)

    def send(self, kind, **fields):
        self.frame(1, json.dumps(dict(type=kind, sessionId=SESSION, **fields)).encode())

    def read(self, size):
        data = self.reader.read(size)
        if len(data) != size:
            raise EOFError("desktop WebSocket closed")
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

    def expect(self, label, predicate, timeout=8):
        deadline = monotonic() + timeout
        seen = []
        while monotonic() < deadline:
            try:
                message = self.incoming.get(timeout=max(0, deadline - monotonic()))
            except Empty:
                break
            if isinstance(message, Exception):
                raise Failure(f"{label}: {message}") from message
            kind = message["type"]
            seen.append((kind, message.get("status", message.get("elapsed"))))
            if kind in ("ack", "report"):
                seq = message["clientSequence"]
                check(seq > self.sequence, "non-monotonic ACK/report sequence")
                self.sequence = seq
            if predicate(message):
                return message
        raise Failure(f"no {label} (observed types/status/elapsed: {seen})")

    def command(self, kind):
        msg = self.expect(kind + " command", lambda m: m["type"] == "command")
        cmd = msg["command"]
        check(int(msg["id"]) > self.last_command and msg["id"] == str(int(msg["id"])), "command id order")
        self.last_command = int(msg["id"])
        check(cmd["type"] == kind, f"expected {kind} but received {cmd['type']}")
        # A precondition may refer to the last state published before this click.
        check(msg.get("expectedStateVersion", self.state_version) == self.state_version, "command state fence")
        return msg

    def select_track(self):
        self.event = f"{EVENT}-{self.last_command}"
        self.track = dict(TRACK, key=self.last_command)

    def result(self, command, *, final=False):
        selecting = command["command"]["type"] == "queueIntent"
        self.send("result", result=dict(id=command["id"], outcome="accepted", code="", stateVersion=self.state_version,
                                        acceptedCount=int(selecting), placement="replace" if selecting else "none",
                                        execution="confirmed" if final else "pending", final=final))

    def playback(self, report=None, publish=False):
        old = self.facts or {"playbackSequence": 0}
        r = report or {}
        self.facts = dict(outputVersion=self.version, stateVersion=self.state_version, eventId=self.event,
                          playbackSequence=old["playbackSequence"] + 1, hasReportedPlayback=bool(report),
                          elapsed=r.get("elapsed", 0), duration=r.get("duration", SECONDS if self.event else 0),
                          isPlaying=r.get("isPlaying", False), isLoading=r.get("isLoading", False),
                          isBuffering=r.get("isBuffering", False), error=r.get("error", ""))
        if publish:
            self.send("playback", playback=self.facts)
        return self.facts

    def state(self, *, opening=False, selected=True, playing=False, elapsed=0, phase=None, source=False):
        self.state_version += 1
        playback = self.playback()
        state = dict(version=1, queue=[self.track] if self.track else [], position=0 if self.track else -1,
                     context=None, shuffle=False, repeat="off", desiredPlaying=playing, highKeys=[],
                     elapsed=elapsed, duration=SECONDS if self.event else 0, isPlaying=False,
                     isBuffering=False, isLoading=False, error="", truncated=False, notice="",
                     occurrence=(dict(key=self.track["key"], trackId=TRACK_ID, eventId=self.event,
                                      progress=0, reported=False) if self.track else None))
        target = self.hello["deviceId"]
        transfer = (dict(phase=phase, sourceDeviceId=target if source else "", targetDeviceId=target,
                         sourceConnectionId=CONNECTION if source else "", targetConnectionId=CONNECTION)
                    if phase else None)
        info = dict(sessionId=SESSION, stateVersion=self.state_version, outputVersion=self.version, state=state,
                    playback=playback, devices=[dict(deviceId=target, name=self.hello["name"],
                                                     available=True, canPlay=True)],
                    selectedDeviceId=target if selected else "", selectedConnectionId=CONNECTION if selected else "",
                    transfer=transfer, drainUntilMs=0)
        fields = dict(connectionId=CONNECTION, deviceId=target) if opening else {}
        self.send("snapshot" if opening else "state", snapshot=info, **fields)

    def instruct(self, kind, *, playing=False, elapsed=0, duration_ms=None, command=None, after_send=None):
        i = dict(id="synthetic-" + token_hex(8), type=kind, outputVersion=self.version,
                 eventId=self.event, elapsed=elapsed, desiredPlaying=playing)
        if self.track and kind != "release":
            i["track"] = self.track
        if command:
            i["commandId"] = command["id"]
        if duration_ms is not None:
            check(self.sequence > 0, "missing lease send-time basis")
            i["lease"] = dict(clientSequence=self.sequence, durationMs=duration_ms)
        self.send("instruction", instruction=i)
        if after_send:
            after_send()
        ack = self.expect(kind + " ACK", lambda m: m["type"] == "ack")
        self.binding(ack, i)
        check(ack["status"] == "ok", f"{kind} ACK status {ack['status']}")
        return i

    @staticmethod
    def binding(message, instruction):
        check(message["instructionId"] == instruction["id"] and
              all(message[key] == instruction[key] for key in ("eventId", "outputVersion")),
              f"incorrect {message['type']} binding for {instruction['type']}")

    def report(self, instruction, predicate, label, *, renew=True):
        def consider(m):
            if m["type"] != "report" or m["instructionId"] != instruction["id"]:
                return False  # a final observation for the retiring instruction can be in flight
            self.binding(m, instruction)
            check(not m["error"], f"executor reported {m['error']}")
            self.playback(m, publish=True)
            if renew and m["isPlaying"]:
                self.send("lease", lease=dict(outputVersion=m["outputVersion"], eventId=m["eventId"],
                                              clientSequence=m["clientSequence"], durationMs=8000))
            return predicate(m)
        return self.expect(label, consider)


@contextmanager
def audio_fixture():
    ready = {"username": "synthetic-audio", "password": token_hex(16)}
    token, csrf = token_hex(32), token_hex(32)
    cookie = "spofify_session=" + token
    session = dict(accountId="synthetic-audio-account", username=ready["username"], role="user", csrfToken=csrf)
    wav = tone()
    ticket, admitted = Queue(maxsize=1), Queue(maxsize=1)
    sockets, media, requests = [], [], deque(maxlen=15)

    class Handler(BaseHTTPRequestHandler):
        def setup(self):
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
            if self.path == "/api/me/player/connect" and body == {} and not sockets and ticket.empty():
                issued = token_hex(32)
                ticket.put_nowait(issued)
                return self.reply({"ticket": issued})
            self.reply(status=503)

        def upgrade(self):
            key = self.headers.get("Sec-WebSocket-Key", "")
            if (self.headers.get("Origin") != ready["origin"] or not key or sockets or
                    self.headers.get("Upgrade", "").lower() != "websocket" or
                    self.headers.get("Sec-WebSocket-Version") != "13" or
                    self.headers.get("Sec-WebSocket-Protocol") != PROTOCOL):
                return self.reply(status=400)
            sockets.append(self.connection)
            accept = b64encode(sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            self.protocol_version = "HTTP/1.1"
            self.send_response(101)
            for name, value in (("Upgrade", "websocket"), ("Connection", "Upgrade"),
                                ("Sec-WebSocket-Protocol", PROTOCOL), ("Sec-WebSocket-Accept", accept)):
                self.send_header(name, value)
            self.end_headers()
            peer = Peer(self)
            try:
                stream = peer.messages()
                hello = next(stream)
                check(hello.get("type") == "hello" and hello.get("ticket") == ticket.get_nowait() and
                      hello.get("deviceId") and hello.get("name") and hello.get("canPlay") is True,
                      "invalid synthetic hello/ticket/capability")
                peer.hello = {name: hello[name] for name in ("deviceId", "name")}
                self.connection.settimeout(45)
                admitted.put_nowait(peer)
                for message in stream:
                    peer.incoming.put(message, timeout=1)
            except (OSError, EOFError, ValueError, Failure, Empty) as error:
                if not hasattr(peer, "hello"):
                    admitted.put_nowait(error)
                elif not peer.incoming.full():
                    peer.incoming.put_nowait(error)

        def do_GET(self):
            if not self.trusted() or self.headers.get("Cookie") != cookie:
                return self.reply(status=401)
            path = urlsplit(self.path)
            args = parse_qs(path.query)
            if path.path == "/api/me/player/socket" and not path.query:
                return self.upgrade()
            if path.path == "/api/auth/session":
                return self.reply(session)
            if path.path == "/api/library/tracks/" + TRACK_ID + "/audio":
                self.reply(wav)
                media.append(len(wav))
                return
            if path.path == "/search":
                query = args.get("query", [""])[0].strip().lower()
                items = [CATALOG] if query and query in TITLE.lower() else []
                return self.reply(dict(items=items, resultCount=len(items), expiresAt="2099-01-01T00:00:00Z"))
            if path.path in ("/api/me/playlists", "/api/me/likes", "/api/me/recent", "/api/me/history"):
                return self.reply(dict(items=[], total=0, availableTotal=0, limit=int(args.get("limit", [40])[0]),
                                       offset=int(args.get("offset", [0])[0])))
            self.reply(status=404)

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        ready["origin"] = f"http://127.0.0.1:{server.server_port}"
        worker = Thread(target=server.serve_forever)
        worker.start()
        try:
            yield ready, admitted, media, requests
        finally:
            server.shutdown()
            worker.join(timeout=5)
            for sock in sockets:
                try:
                    sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
