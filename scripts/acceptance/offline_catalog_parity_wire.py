"""Owned wire and audio fixture for offline_catalog_parity.py; never contacts Go."""
from collections import Counter
from contextlib import contextmanager
from hashlib import sha256
import http.server
from io import BytesIO
import json
from math import pi, sin
import re
import struct
from threading import Event, RLock
from time import monotonic
from types import SimpleNamespace
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit
import wave

from fixture import check
import wire_ui as base

COUNT = 108
SECONDS = 120
ARTIST = base.ARTIST
ALBUM = base.ALBUM
TITLES = ["Parity Opening", "Parity Refrain", "Parity Bridge", "Parity Closing", "Parity Unsaved Coda"]
FREQUENCIES = dict(zip(TITLES, (220, 330, 440, 550, 660)))
TRACKS = [dict(base.TRACK, id=f"parity-track-{i}", name=title, trackNumber=i + 1,
               durationMs=SECONDS * 1000) for i, title in enumerate(TITLES)]
# Not just consecutive identical IDs: the marker between equal recordings
# exposes both reordering and a row Play that accidentally selects the first match.
ORDER = [0, 1, 2, 1, 3] + [2, 3, 0] * 34 + [1]
MANUAL = "Parity Manual Playlist"
RULES = "Parity Rules Playlist"


def playlist(key, name, kind, sequence):
    summary, rows, definition = base.playlist_case(key, name, len(sequence), kind=kind,
                                                  source="wire-parity-manual")
    for row, track in zip(rows, sequence, strict=True):
        row["track"] = TRACKS[track]
    return summary, rows, definition


PLAYLISTS = {item[0]["id"]: item for item in (
    playlist("parity-manual", MANUAL, "manual", ORDER),
    # Rule list is observed online, but intentionally not pinned; share saved
    # recordings with Manual plus one genuinely unavailable recording offline.
    playlist("parity-rules", RULES, "derived", [*ORDER[:4], 4, *ORDER[5:]]),
)}


def wav(hz):
    rate = 8000
    period = struct.pack(f"<{rate}h", *(round(7000 * sin(2 * pi * hz * t / rate)) for t in range(rate)))
    data = BytesIO()
    with wave.open(data, "wb") as file:
        file.setparams((1, 2, rate, 0, "NONE", "not compressed"))
        file.writeframes(period * SECONDS)
    return data.getvalue()


class Wire:
    def __init__(self, output, port=0):
        self.port = port
        check(len(ORDER) == COUNT, "wrong offline parity fixture occurrence count")
        self.log = output / "wire.jsonl"
        self.lock = RLock()
        self.start = monotonic()
        self.phase = "online"
        self.events = []
        self.violations = []
        self.release_probe = Event()
        self.accept_probe = False
        self.seen_probe = False
        self.waveforms = {}
        for track in TRACKS:
            audio = wav(FREQUENCIES[track["name"]])
            self.waveforms[track["id"]] = (sha256(audio).hexdigest(), audio)

    def record(self, event, **fields):
        with self.lock:
            item = dict(seconds=round(monotonic()-self.start, 6), phase=self.phase, event=event, **fields)
            self.events.append(item)
            with self.log.open("a") as log:
                print(json.dumps(item, sort_keys=True), file=log)
            return item

    def change(self, phase):
        with self.lock:
            self.phase = phase
            self.record("phase")

    def bad(self, reason):
        with self.lock:
            self.violations.append(reason)
            self.record("violation", reason=reason)

    def matches(self, event, **fields):
        with self.lock:
            return [item for item in self.events if item["event"] == event and
                    all(item.get(key) == value for key, value in fields.items())]

    def begin_probe(self):
        with self.lock:
            self.release_probe.clear()
            self.accept_probe = False
            self.seen_probe = False
            self.change("reconnecting")

    def finish_probe(self, success):
        with self.lock:
            self.accept_probe = success
            if not success:
                self.change("offline")
            self.release_probe.set()

    def manifest(self, handler, pid, args):
        summary, rows, _ = PLAYLISTS[pid]
        offset = 0
        if "cursor" in args:
            cursor = args["cursor"]
            match = re.fullmatch(r"of1_([0-9]+)", cursor[0]) if len(cursor) == 1 else None
            offset = int(match[1]) if match else -1
        if (args.get("limit") != ["20"] or set(args) - {"limit", "cursor"}
                or offset not in range(0, len(rows), 20)):
            self.bad("invalid offline playlist manifest offset")
            return handler.error(400)
        end = min(offset + 20, len(rows))
        return handler.reply(dict(version=1, playlist={key: summary[key] for key in ("id", "kind", "name", "revision")},
                                  fence="f1_" + "a" * 64, limit=20, total=len(rows), recordedAudioTotal=len(rows),
                                  nextCursor=f"of1_{end}" if end < len(rows) else None,
                                  items=[dict(occurrenceId=r["id"], position=r["position"], track=r["track"])
                                         for r in rows[offset:end]]))

    @contextmanager
    def serve(self):
        wire = self
        session = {}

        class Account(base.AccountWire):
            def frame(self, opcode, payload):
                if opcode == 1:
                    msg = json.loads(payload)
                    wire.record("server-frame", kind=msg.get("type"))
                super().frame(opcode, payload)

            def message(self):
                # Retain base framing limits, but distinguish incoming JSON from
                # socket teardown rather than treating a close frame as app traffic.
                while True:
                    opcode, size = self.read(2)
                    check(opcode in (129, 136, 137, 138) and size & 128, "invalid fixture WebSocket frame")
                    size &= 127
                    if size == 126 and opcode == 129:
                        size = struct.unpack("!H", self.read(2))[0]
                    check(size < (8192 if opcode == 129 else 126), "fixture WebSocket request exceeded bound")
                    mask = self.read(4)
                    payload = bytes(value ^ mask[i % 4] for i, value in enumerate(self.read(size)))
                    if opcode == 129:
                        msg = json.loads(payload)
                        kind = msg.get("type")
                        personal = msg.get("command") if kind == "personal" else None
                        meta = {"command": personal.get("type"), "playlist": personal.get("playlistId"),
                                "offset": personal.get("offset", 0)} if personal else {}
                        item = wire.record("client-frame", kind=kind, **meta)
                        if item["phase"] != "online":
                            wire.bad(f"{kind} WebSocket frame while {item['phase']}")
                            raise EOFError
                        return msg
                    item = wire.record("socket-control", opcode=opcode & 15)
                    if opcode == 136:
                        self.frame(8, payload)
                        raise EOFError
                    if item["phase"] != "online":
                        wire.bad(f"non-close WebSocket control while {item['phase']}")
                        raise EOFError
                    if opcode == 137:
                        self.frame(10, payload)

            def serve(self):
                wire.record("socket-open")
                try:
                    super().serve()
                finally:
                    wire.record("socket-close")

        class Server(http.server.ThreadingHTTPServer):
            def __init__(self, address, handler):
                class Handler(handler):
                    def setup(self):
                        super().setup()
                        self.connection.settimeout(30)

                    def parse_request(self):
                        if not super().parse_request():
                            return False
                        path = urlsplit(self.path).path  # no credential, query or ticket data in artifacts
                        method = self.command
                        with wire.lock:
                            item = wire.record("http", method=method, path=path)
                            if item["phase"] == "online":
                                return True
                            if (item["phase"] == "reconnecting" and method == "GET" and
                                    path == "/api/auth/session" and self.path == path and
                                    self.headers.get("Cache-Control") == "no-cache, no-store" and not wire.seen_probe):
                                wire.seen_probe = True
                                return True
                            wire.bad(f"unexpected {method} {path} while {item['phase']}")
                        self.error(403)
                        return False

                    def reply(self, value=None, status=200, headers=None):
                        if self.path == "/api/auth/login" and status == 200:
                            session.update(value)
                        return super().reply(value, status, headers)

                    def do_GET(self):
                        route = urlsplit(self.path)
                        args = parse_qs(route.query, keep_blank_values=True)
                        path = route.path
                        if path == "/api/auth/session" and self.headers.get("Cache-Control") == "no-cache, no-store":
                            if not (self.trusted() and self.owned()):
                                return
                            wire.record("probe-held")
                            opened = wire.release_probe.wait(3.8)
                            with wire.lock:
                                if opened and wire.accept_probe and wire.phase == "reconnecting":
                                    self.reply(session)
                                    wire.record("probe-response", status=200)
                                    wire.change("online")
                                else:
                                    self.reply(status=503)
                                    wire.record("probe-response", status=503)
                            return
                        match = re.fullmatch(r"/api/me/offline/v1/(playlists|tracks)/([a-z0-9-]+)/(manifest|media|audio)", path)
                        if match:
                            if not (self.trusted() and self.owned()):
                                return
                            kind, ident, resource = match.groups()
                            if kind == "playlists" and ident in PLAYLISTS and resource == "manifest":
                                return wire.manifest(self, ident, args)
                            if kind == "tracks" and ident in wire.waveforms:
                                digest, audio = wire.waveforms[ident]
                                version = "sha256:" + digest
                                if resource == "media" and not args:
                                    return self.reply(dict(version=1, trackId=ident, sha256=digest, mediaVersion=version,
                                                           format="wav", contentType="audio/wav", byteLength=len(audio)))
                                if resource == "audio" and args == {"version": [version]}:
                                    self.close_connection = True
                                    self.send_response(200)
                                    for key, value in (("Content-Type", "audio/wav"), ("ETag", f'"{version}"'),
                                                       ("Content-Length", str(len(audio))), ("Connection", "close")):
                                        self.send_header(key, value)
                                    try:
                                        self.end_headers()
                                        self.wfile.write(audio)
                                    except OSError:
                                        pass
                                    return
                            wire.bad("unexpected offline transfer request")
                            return self.error(404)
                        if path == "/api/library" and args.get("albumId") == [ALBUM["id"]]:
                            if not (self.trusted() and self.owned()):
                                return
                            check(args.get("kind") == ["tracks"], "album route requested the wrong catalog kind")
                            offset = int(args.get("offset", ["0"])[0])
                            limit = int(args.get("limit", ["40"])[0])
                            return self.reply(dict(items=TRACKS[offset:offset + limit], total=len(TRACKS),
                                                   offset=offset, limit=limit, revision=base.REVISION))
                        return super().do_GET()

                check(address == ("127.0.0.1", 0), "fixture listeners must be worker-local only")
                super().__init__((address[0], wire.port), Handler)

        items = (ARTIST, base.GUEST, ALBUM, *TRACKS)
        facade = SimpleNamespace(server=SimpleNamespace(BaseHTTPRequestHandler=http.server.BaseHTTPRequestHandler,
                                                         ThreadingHTTPServer=Server))
        with (patch.object(base, "http", facade), patch.object(base, "AccountWire", Account),
              patch.object(base, "PLAYLISTS", PLAYLISTS), patch.object(base, "ITEMS", items),
              base.wire_fixture() as ready):
            try:
                yield ready
                check(not ready["play_requests"](), "fixture received an ordinary online Play during a local playback scenario")
            finally:
                wire.release_probe.set()
                print("WIRE", json.dumps(Counter(item["event"] for item in self.events), sort_keys=True), flush=True)
        check(not self.violations, "; ".join(self.violations))
