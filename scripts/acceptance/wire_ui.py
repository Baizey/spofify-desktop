"""UI-only synthetic wire fixture. Not Go integration or playback evidence."""
from contextlib import contextmanager
import http.server
import json
from secrets import token_hex
from threading import Thread
from urllib.parse import parse_qs, urlsplit


PLAYLIST = "Synthetic Manual Duplicates"
TRACK_NAME = "Synthetic Tone 001"
DATE = "2025-01-01T00:00:00Z"
TRACK = {"kind": "track", "id": "wire-track-01", "name": TRACK_NAME, "hasAudio": True,
         "durationMs": 1000, "artists": [{"id": "wire-artist-01", "name": "Synthetic Wire Ensemble"}],
         "albumId": "wire-album-01", "albumName": "Synthetic Wire Album",
         "coverage": {"tracks": 1, "inLibrary": 1, "albums": 0}}
SUMMARY = {"id": "wire-playlist-01", "name": PLAYLIST, "description": "Two synthetic occurrences",
           "kind": "manual", "countState": "exact", "trackCount": 2, "availableCount": 2,
           "capabilities": {"editMetadata": True, "editEntries": True, "editDefinition": False,
                            "play": True, "copyToManual": False, "delete": True, "addTracks": True},
           "revision": 3, "createdAt": DATE, "updatedAt": DATE}
ENTRIES = [{"id": f"wire-entry-{i}", "position": i, "addedAt": DATE, "track": TRACK} for i in range(2)]
PLAYLIST_PATH = "/api/me/playlists/" + SUMMARY["id"]


@contextmanager
def wire_fixture():
    ready = {"username": "synthetic-wire", "password": token_hex(16), "userId": "wire-account-01"}
    token, csrf = token_hex(32), token_hex(32)
    session = {"accountId": ready["userId"], "username": ready["username"], "role": "user", "csrfToken": csrf}

    class Handler(http.server.BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(5)

        def log_message(self, *_args):
            pass

        def reply(self, value=None, status=200, headers=None):
            body = json.dumps(value).encode() if value is not None else b""
            self.send_response(status)
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Type", "application/json")
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
                else:
                    self.error(503 if self.path == "/api/me/player/connect" else 404)

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
            elif route.path == "/api/me/playlists":
                page([SUMMARY])
            elif route.path == PLAYLIST_PATH:
                self.reply(SUMMARY)
            elif route.path == PLAYLIST_PATH + "/entries":
                page(ENTRIES)
            elif route.path == "/api/me/likes":
                page([], availableTotal=0)
            elif route.path in ("/api/me/recent", "/api/me/history"):
                page([])
            elif route.path == "/search":
                query = args.get("query", [""])[0].strip().lower()
                found = [TRACK] if query and query in TRACK_NAME.lower() and args.get("kind", ["track"])[0] == "track" else []
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
