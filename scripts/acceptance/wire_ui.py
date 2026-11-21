"""UI-only synthetic wire fixture. Not Go integration or playback evidence."""
from base64 import b64decode, b64encode
from binascii import Error as Base64Error
from contextlib import contextmanager
from hashlib import sha1
import http.server
import json
import re
from secrets import token_hex
import struct
from threading import Lock, Thread
from time import monotonic
from urllib.parse import parse_qs, urlsplit

from fixture import Failure, check


PLAYLIST = "Synthetic Manual Duplicates"
TRACK_NAME = "Synthetic Tone 001"
ARTIST_NAME = "Synthetic Wire Ensemble"
GUEST_NAME = "Synthetic Guest Quartet"
NAME_ONLY = "Name-only contributor"
ALBUM_NAME = "Synthetic Wire Album"
DATE = "2025-01-01T00:00:00Z"
REVISION = 7


def catalog_item(kind, id, name, *, albums=0, **details):
    # Go domain.ListeningItem, as used by both /search and /api/library.
    return dict(kind=kind, id=id, name=name, hasAudio=kind == "track",
                coverage=dict(tracks=1, inLibrary=1, albums=albums), **details)


ARTIST = catalog_item("artist", "wire-artist-01", ARTIST_NAME, albums=2)
GUEST = catalog_item("artist", "wire-artist-02", GUEST_NAME, albums=1)
CREDITS = [{"id": a["id"], "name": a["name"]} for a in (ARTIST, GUEST)] + [{"id": "", "name": NAME_ONLY}]
ALBUM = catalog_item("album", "wire-album-01", ALBUM_NAME, artists=CREDITS,
                     albumType="album", releaseDate="2025-01-01")
# Go serializes source credits with a known identity but no recorded name.
# A release with this shape must not invalidate the artist's entire page.
ID_ONLY_RELEASE = catalog_item("album", "wire-album-02", "Uncredited Wire Single",
                               artists=[{"id": ARTIST["id"], "name": ""}],
                               albumType="single", releaseDate="2025-02-01")
TRACK = catalog_item("track", "wire-track-01", TRACK_NAME, durationMs=1000, artists=CREDITS,
                     albumId=ALBUM["id"], albumName=ALBUM_NAME, discNumber=1, trackNumber=1)
SINGLE_TRACK = catalog_item("track", "wire-track-02", "Synthetic Uncredited Wire Recording", durationMs=1000,
                            artists=ID_ONLY_RELEASE["artists"], albumId=ID_ONLY_RELEASE["id"],
                            albumName=ID_ONLY_RELEASE["name"], discNumber=1, trackNumber=1)
ITEMS = (ARTIST, GUEST, ALBUM, TRACK, ID_ONLY_RELEASE, SINGLE_TRACK)
DETAILS = {f"/api/library/{item['kind']}s/{item['id']}": item for item in ITEMS if item['kind'] != 'track'}
SUMMARY = {"id": "wire-playlist-01", "name": PLAYLIST, "description": "Two synthetic occurrences",
           "kind": "manual", "countState": "exact", "trackCount": 2, "availableCount": 2,
           "capabilities": {"editMetadata": True, "editEntries": True, "editDefinition": False,
                            "play": True, "copyToManual": False, "delete": True, "addTracks": True},
           "revision": 3, "createdAt": DATE, "updatedAt": DATE}
ENTRIES = [{"id": f"wire-entry-{i}", "position": i, "addedAt": DATE, "track": TRACK} for i in range(2)]
PROTOCOL = "spofify.account-state.v1"
SOCKET_PATH = "/api/me/player/socket"
TICKET_PATH = "/api/me/player/connect"
PLAYLIST_TOKEN = "wire-rows-v1"


def diagnostic_path(path):
    # Exclude query, fragments, encoded tokens and opaque account/playlist IDs.
    if re.fullmatch(r"/api/me/(likes(?:/status)?|recent|history|player/[a-z-]+|palette|playlists)", path):
        return path
    if path.startswith("/api/me/playlists/"):
        return "/api/me/playlists/<redacted>"
    return "/api/me/<redacted>"


class AccountWire:
    """Bounded account replica and Play request probe, not an audio executor."""

    def __init__(self, handler, consume_ticket, writes, record_play):
        self.handler = handler
        self.consume_ticket = consume_ticket
        self.writes = writes
        self.record_play = record_play
        self.connection = token_hex(16)
        self.grant = token_hex(16)
        self.snapshot = None

    def read(self, length):
        data = self.handler.rfile.read(length)
        if len(data) != length:
            raise EOFError
        return data

    def frame(self, opcode, payload):
        check(len(payload) < 65536, "synthetic account response exceeded bound")
        size = bytes([len(payload)]) if len(payload) < 126 else b"\x7e" + struct.pack("!H", len(payload))
        self.handler.wfile.write(bytes([128 | opcode]) + size + payload)

    def send(self, message):
        self.frame(1, json.dumps(message, separators=(",", ":")).encode())

    def message(self):
        while True:
            opcode, size = self.read(2)
            check(opcode in (129, 136, 137, 138) and size & 128, "invalid synthetic account frame")
            size &= 127
            if size == 126 and opcode == 129:
                size = struct.unpack("!H", self.read(2))[0]
            check(size < (8192 if opcode == 129 else 126), "synthetic account request exceeded bound")
            mask = self.read(4)
            payload = bytes(value ^ mask[i % 4] for i, value in enumerate(self.read(size)))
            if opcode == 136:
                self.frame(8, payload)
                raise EOFError
            if opcode == 137:
                self.frame(10, payload)
            elif opcode == 129:
                return json.loads(payload)

    def personal(self, **update):
        self.send(dict(type="personal", update=update))

    def initial(self):
        self.send(self.snapshot)
        self.personal(snapshot=dict(playlists=[SUMMARY], recent=[]))

    def play(self, message):
        selection = message.get("selection")
        check(set(message) == {"type", "selection", "shuffle"} and message["shuffle"] is False
              and isinstance(selection, dict) and selection.get("kind") == "track"
              and isinstance(selection.get("track"), dict), "unexpected synthetic Play request")
        track = next((t for t in (TRACK, SINGLE_TRACK) if t["id"] == selection["track"].get("id")), None)
        check(track, "synthetic Play used an unknown track ID")
        run = self.record_play(selection)
        # A paused cursor exercises the app's actual player projection without
        # starting a download, emitting an audio grant, or mutating a Go account.
        row = dict(id=track["id"], name=track["name"], artists=track["artists"], albumId=track["albumId"],
                   albumName=track["albumName"], duration=1, key=run, order=1, intent="low")
        cursor = dict(key=run, chain="wire-play", visit=run, run=run, elapsed=0, desired=False, playing=False)
        self.snapshot["player"].update(queue=[row], cursor=cursor)
        self.send(dict(type="player", player=dict(source=self.connection, cursor=cursor,
                                                 queue=dict(kind="replace", rows=[row]))))

    def serve(self):
        hello = self.message()
        check(isinstance(hello, dict) and hello.get("type") == "hello"
              and set(hello) == {"type", "ticket", "deviceId", "name", "canPlay"}
              and isinstance(hello.get("ticket"), str) and self.consume_ticket(hello["ticket"])
              and isinstance(hello.get("deviceId"), str) and hello["deviceId"]
              and isinstance(hello.get("name"), str) and hello["name"]
              and isinstance(hello.get("canPlay"), bool), "invalid synthetic account hello")
        cursor = dict(key=0, chain="", visit=0, run=0, elapsed=0, desired=False, playing=False)
        player = dict(queue=[], cursor=cursor, repeat="off", shuffle=False)
        devices = [dict(id=self.connection, deviceId=hello["deviceId"], name=hello["name"], canPlay=hello["canPlay"])]
        owner = self.connection if hello["canPlay"] else ""
        self.snapshot = dict(type="snapshot", connectionId=self.connection, player=player, owner=owner, devices=devices)
        self.initial()
        while True:
            message = self.message()
            check(isinstance(message, dict), "synthetic account message must be an object")
            kind = message.get("type")
            if kind == "presence" and owner:
                self.send(dict(type="grant", basis=message["basis"], ms=8000, handle=self.grant))
            elif kind == "sync":
                self.initial()
            elif kind == "personal":
                command = message.get("command")
                check(isinstance(command, dict), "invalid synthetic personal command")
                if command.get("type") != "syncPlaylist":
                    self.writes.append("personal mutation")
                    return
                check(command.get("playlistId") == SUMMARY["id"] and command.get("offset", 0) == 0,
                      "synthetic account asked for an unknown playlist/offset")
                if command.get("known") != PLAYLIST_TOKEN:
                    self.personal(part=dict(summary=SUMMARY, token=PLAYLIST_TOKEN, offset=0, rows=ENTRIES, more=False))
            elif kind == "play":
                self.play(message)
            elif kind == "release":
                owner = ""
                self.snapshot["owner"] = ""
                self.send(dict(type="outputs", owner="", devices=devices))
            else:
                self.writes.append("unexpected account command")
                return


@contextmanager
def wire_fixture():
    ready = {"username": "synthetic-wire", "password": token_hex(16), "userId": "wire-account-01"}
    token, csrf = token_hex(32), token_hex(32)
    session = {"accountId": ready["userId"], "username": ready["username"], "role": "user", "csrfToken": csrf}
    violations = []
    plays = []
    play_lock = Lock()

    def record_play(selection):
        with play_lock:
            check(len(plays) < 8, "synthetic Play request cap exceeded")
            plays.append(selection)
            return len(plays)

    def play_requests():
        with play_lock:
            return plays.copy()

    ready["play_requests"] = play_requests
    tickets = {}
    ticket_lock = Lock()

    def consume_ticket(ticket):
        with ticket_lock:
            return tickets.pop(ticket, 0) > monotonic()

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        def setup(self):
            super().setup()
            self.connection.settimeout(5)

        def log_message(self, *_args):
            pass

        def reply(self, value=None, status=200, headers=None):
            self.close_connection = True
            body = json.dumps(value).encode() if value is not None else b""
            self.send_response(status)
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Type", "application/json")
            self.send_header("Connection", "close")
            self.send_header("Content-Length", str(len(body)))
            for name, value in (headers or {}).items():
                self.send_header(name, value)
            try:
                self.end_headers()
                if body:
                    self.wfile.write(body)
            except OSError:
                pass

        def error(self, status):
            self.reply({"error": "Synthetic wire fixture does not serve this request"}, status)

        def trusted(self):
            if self.headers.get("Host") != ready["origin"].removeprefix("http://") or self.headers.get("Origin") not in (None, ready["origin"]):
                self.error(403)
                return False
            return True

        def owned(self):
            if self.headers.get("Cookie") != "spofify_session=" + token:
                self.error(401)
                return False
            return True

        def do_POST(self):
            if not self.trusted():
                return
            if self.headers.get("Origin") != ready["origin"]:
                self.error(403)
                return
            if self.path == "/api/auth/login":
                try:
                    size = int(self.headers.get("Content-Length", "0"))
                    if not 0 < size <= 1024:
                        raise ValueError
                    credentials = json.loads(self.rfile.read(size))
                except (ValueError, OSError):
                    self.error(400)
                    return
                if credentials != {k: ready[k] for k in ("username", "password")} or self.headers.get("X-Spofify-Login") != "1":
                    self.error(401)
                    return
                self.reply(session, headers={"Set-Cookie": f"spofify_session={token}; Path=/; HttpOnly; Max-Age=3600; SameSite=Lax"})
            elif self.owned():
                if self.path == "/api/auth/logout" and self.headers.get("X-CSRF-Token") == csrf:
                    self.reply(status=204)
                elif self.path == TICKET_PATH and self.headers.get("X-CSRF-Token") == csrf:
                    try:
                        size = int(self.headers.get("Content-Length", "0"))
                        if not 0 < size <= 1024 or json.loads(self.rfile.read(size)) != {}:
                            raise ValueError
                    except (ValueError, OSError):
                        return self.error(400)
                    now = monotonic()
                    ticket = token_hex(32)
                    with ticket_lock:
                        for old in [key for key, expiry in tickets.items() if expiry <= now]:
                            del tickets[old]
                        if len(tickets) >= 8:
                            return self.error(429)
                        tickets[ticket] = now + 20
                    self.reply(dict(ticket=ticket))
                else:
                    self.reject_mutation()

        def reject_mutation(self):
            path = urlsplit(self.path).path
            if path.startswith("/api/me/"):
                violations.append(f"unexpected {self.command} {diagnostic_path(path)}")
            self.error(404)

        def account_socket(self):
            key = self.headers.get("Sec-WebSocket-Key", "")
            try:
                valid_key = len(b64decode(key, validate=True)) == 16
            except (Base64Error, ValueError):
                valid_key = False
            if (not valid_key or self.headers.get("Upgrade", "").casefold() != "websocket"
                    or "upgrade" not in self.headers.get("Connection", "").casefold().replace(" ", "").split(",")
                    or self.headers.get("Sec-WebSocket-Version") != "13"
                    or self.headers.get("Sec-WebSocket-Protocol") != PROTOCOL
                    or self.headers.get("Origin") != ready["origin"]):
                violations.append("invalid account WebSocket upgrade")
                return self.error(400)
            accept = b64encode(sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
            self.close_connection = True
            self.send_response(101)
            self.send_header("Upgrade", "websocket")
            self.send_header("Connection", "Upgrade")
            self.send_header("Sec-WebSocket-Accept", accept.decode())
            self.send_header("Sec-WebSocket-Protocol", PROTOCOL)
            self.end_headers()
            try:
                AccountWire(self, consume_ticket, violations, record_play).serve()
            except (EOFError, BrokenPipeError, ConnectionResetError):
                pass  # The owned window exits before the fixture.
            except TimeoutError:
                violations.append("synthetic account socket stopped receiving frames")
            except (Failure, KeyError, TypeError, ValueError):
                violations.append("invalid synthetic account message")

        def do_PATCH(self):
            if self.trusted() and self.owned():
                self.reject_mutation()

        do_DELETE = do_PUT = do_PATCH

        def do_GET(self):
            if not self.trusted() or not self.owned():
                return
            route = urlsplit(self.path)
            args = parse_qs(route.query)
            try:
                limit = int(args.get("limit", ["40"])[0])
                offset = int(args.get("offset", ["0"])[0])
                if not 1 <= limit <= 100 or offset < 0:
                    raise ValueError
            except ValueError:
                self.error(400)
                return

            def page(items, **extra):
                self.reply(dict(items=items[offset:offset + limit], total=len(items), limit=limit,
                                offset=offset, **extra))

            if route.path == "/api/auth/session":
                self.reply(session)
            elif self.path == SOCKET_PATH:
                self.account_socket()
            elif route.path.startswith("/api/me/"):
                violations.append(f"unexpected GET {diagnostic_path(route.path)}")
                self.error(404)
            elif re.fullmatch(r"/api/library/tracks/[^/]+", route.path):
                violations.append("unexpected standalone Track detail request")
                self.error(404)
            elif route.path in DETAILS and not args:
                self.reply({"item": DETAILS[route.path], "revision": REVISION})
            elif route.path == "/api/library":
                kind = args.get("kind", [""])[0]
                artist = args.get("artistId", [""])[0]
                album = args.get("albumId", [""])[0]
                group = args.get("releaseGroup", ["all"])[0]
                if (set(args) - {"kind", "scope", "availability", "limit", "offset", "artistId", "albumId", "releaseGroup"}
                        or kind not in ("artists", "albums", "tracks")
                        or args.get("scope") != ["all"] or args.get("availability") != ["in-library"]):
                    return self.error(400)
                if artist:
                    if kind != "albums" or artist not in (ARTIST["id"], GUEST["id"]) or album:
                        return self.error(400)
                    if group not in ("all", "albums", "singles", "appearances"):
                        return self.error(400)
                    found = [dict(ALBUM, relation="release")] if group in ("all", "albums") else []
                    if artist == ARTIST["id"] and group in ("all", "singles"):
                        found.append(dict(ID_ONLY_RELEASE, relation="release"))
                elif album:
                    if kind != "tracks" or album not in (ALBUM["id"], ID_ONLY_RELEASE["id"]) or "releaseGroup" in args:
                        return self.error(400)
                    found = [TRACK if album == ALBUM["id"] else SINGLE_TRACK]
                else:
                    found = [item for item in ITEMS if item["kind"] + "s" == kind]
                page(found, revision=REVISION)
            elif route.path == "/search":
                query = args.get("query", [""])[0].strip().casefold()
                kind = args.get("kind", [""])[0]
                if not query or set(args) - {"query", "kind", "limit"} or kind not in ("", "artist", "album", "track"):
                    return self.error(400)
                found = [item for item in ITEMS if query in item["name"].casefold() and (not kind or item["kind"] == kind)]
                # This tiny fixture fits in one page at the desktop's search size.
                if len(found) > limit:
                    return self.error(400)
                self.reply({"items": found, "resultCount": len(found), "expiresAt": "2099-01-01T00:00:00Z"})
            else:
                self.error(404)

    class Server(http.server.ThreadingHTTPServer):
        daemon_threads = False  # server_close joins each bounded request handler.

    # Per-run ephemeral listener, no state copied from Go or any actual account.
    with Server(("127.0.0.1", 0), Handler) as server:
        ready["origin"] = f"http://127.0.0.1:{server.server_port}"
        worker = Thread(target=server.serve_forever, name="synthetic-ui-wire-fixture")
        worker.start()
        try:
            yield ready
        finally:
            server.shutdown()
            worker.join(timeout=5)
    check(not violations, "synthetic account fixture: " + ", ".join(sorted(set(violations))))
