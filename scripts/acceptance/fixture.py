#!/usr/bin/env python3
"""Real Go account-state fixture with a synthetic catalog and disposable accounts."""

from contextlib import contextmanager
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import signal
import subprocess
from time import monotonic
from urllib.parse import quote, urlencode, urlsplit


class Failure(RuntimeError):
    pass


def check(condition, message):
    if not condition:
        raise Failure(message)


def private_env(root):
    paths = {"HOME": "home", "XDG_CONFIG_HOME": "config", "XDG_DATA_HOME": "data",
             "XDG_CACHE_HOME": "cache", "XDG_STATE_HOME": "state", "TMPDIR": "tmp",
             "XDG_RUNTIME_DIR": "runtime"}
    env = {"PATH": os.environ["PATH"], "LANG": "C.UTF-8"}
    for key, name in paths.items():
        path = root / name
        path.mkdir(mode=0o700)
        env[key] = str(path)
    return env


def stop(process, graceful=False):
    if process.poll() is None and not graceful:
        process.terminate()
    try:
        return process.wait(timeout=20 if graceful else 5)
    except subprocess.TimeoutExpired:
        process.kill()
        return process.wait(timeout=5)


def build(source, root, env, module_cache=None):
    """Overlay only composition/lifecycle, not any endpoint, store or wire logic."""
    # The overlay key must match Go's physical checkout path (e.g. /var/home,
    # not a /home alias), or Go can silently compile the unmodified fixture.
    source = source.resolve(strict=True)
    fixture_source = "cmd/offline-fixture/main.go"
    git = ["git", "--no-optional-locks", "-C", str(source)]
    try:
        revision = subprocess.check_output(git + ["rev-parse", "HEAD"], text=True, timeout=10).strip()
        clean = subprocess.check_output(git + ["status", "--porcelain"], text=True, timeout=10)
    except subprocess.SubprocessError as error:
        raise Failure("cannot read Go checkout revision") from error
    check(not clean, "use a clean Go checkout to give the fixture an exact revision")
    original = source / fixture_source
    contents = original.read_text()
    # Match cmd/spofify's AuthConfig: this store supplies both the canonical
    # playback resolver and the personal socket worker. Keep the source checkout
    # read-only, and join the hijacked sockets before closing the fixture DBs.
    for old, new in (
        ("BaseURL: origin, PersonalLibrary: web.NewPersonalWithOffline(db, libraryRoot),",
         "BaseURL: origin, PersonalLibrary: web.NewPersonalWithOffline(db, libraryRoot), PlaybackCatalog: db,"),
        ("shutdownErr := server.Shutdown(shutdownCtx)",
         "socketErr := handler.(interface{ Shutdown(context.Context) error }).Shutdown(shutdownCtx)\n"
         "\tshutdownErr := server.Shutdown(shutdownCtx)"),
        ("return errors.Join(encodeErr, serveErr, inputErr, shutdownErr)",
         "return errors.Join(encodeErr, serveErr, inputErr, socketErr, shutdownErr)"),
    ):
        check(contents.count(old) == 1, "Go fixture composition changed; update the acceptance overlay")
        contents = contents.replace(old, new)
    replacement = root / "fixture-main.go"
    replacement.write_text(contents)
    overlay = root / "overlay.json"
    overlay.write_text(json.dumps({"Replace": {str(original): str(replacement)}}))
    binary = root / "spofify-fixture"
    module_cache = module_cache or Path.home() / "go" / "pkg" / "mod"
    check(module_cache.is_dir(), "use an existing prepopulated Go module cache (--go-module-cache)")
    build_env = dict(env, GOENV="off", GOWORK="off", GOPROXY="off", GOSUMDB="off",
                     GOTOOLCHAIN="local", GOTMPDIR=env["TMPDIR"],
                     GOCACHE=str(root / "cache" / "go-build"), GOMODCACHE=str(module_cache))
    check(shutil.which("go") is not None, "use installed Go or supply an already-built fixture")
    command = ["go", "build", "-p=2", "-mod=readonly", "-buildvcs=false", "-overlay", str(overlay),
               "-ldflags", f"-X main.sourceRevision={revision}", "-o", str(binary), "./cmd/offline-fixture"]
    with subprocess.Popen(command, cwd=source, env=build_env, start_new_session=True) as process:
        try:
            code = process.wait(timeout=360)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
    check(code == 0, "Go fixture build failed (requires offline Go/modules and writable mmap for the linker)")
    return binary, revision


@contextmanager
def fixture(binary, root, env, expected_revision=None):
    base = root / "fixture"
    base.mkdir(mode=0o700)
    with (root / "fixture.stderr").open("w+b") as log, subprocess.Popen(
        [str(binary), "--root", str(base), "--listen", "127.0.0.1:0"], cwd=root,
        env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log,
    ) as process:
        ready = None
        try:
            line = bytearray()
            deadline = monotonic() + 40
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                while b"\n" not in line and len(line) < 65537:
                    check(selector.select(max(0, deadline - monotonic())), "Go fixture did not publish readiness")
                    chunk = os.read(process.stdout.fileno(), 65537 - len(line))
                    if not chunk:
                        break
                    line.extend(chunk)
            check(line.endswith(b"\n") and len(line) <= 65536, "Go fixture readiness missing or invalid")
            ready = json.loads(line)
            check(ready.get("event") == "ready" and ready.get("version") == 1,
                  "Go fixture returned an unsupported descriptor")
            revision = ready["sourceRevision"]
            check(re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", revision) and
                  (expected_revision is None or revision == expected_revision), "fixture revision mismatch")
            check(ready["stateRoot"] == str(base / "offline-fixture"), "unexpected fixture state root")
            origin = urlsplit(ready["origin"])
            check(origin.scheme == "http" and origin.netloc == f"127.0.0.1:{origin.port}" and
                  origin.port and not (origin.path or origin.query or origin.fragment),
                  "fixture must bind only to an ephemeral IPv4 loopback port")
            yield ready
        finally:
            process.stdin.close()
            code = stop(process, graceful=True)
            if ready is None and code != 0:
                log.seek(0)
                diagnostics = log.read(1500).decode(errors="replace").strip()
                if diagnostics:
                    print(f"Fixture startup diagnostic: {diagnostics}", flush=True)
        log.seek(0)
        receipt = None
        for line in log.read(64 * 1024).splitlines():
            try:
                item = json.loads(line)
            except ValueError:
                continue
            if isinstance(item, dict) and item.get("event") == "stopped":
                receipt = item
        check(code == 0 and receipt and all(receipt.get(k) is True for k in
              ("stopped", "clean", "stateRetained")) and receipt.get("stateRoot") == ready["stateRoot"]
              and receipt.get("sourceRevision") == ready["sourceRevision"], "fixture did not shut down cleanly")
        print("PASS Go fixture stopped and joined its workers", flush=True)


class Api:
    def __init__(self, ready, account=None):
        self.origin = ready["origin"]
        parsed_origin = urlsplit(self.origin)
        self.port = parsed_origin.port
        check(self.port and self.origin == f"http://127.0.0.1:{self.port}",
              "fixture API requires its disposable IPv4 loopback origin")
        self.cookie = self.csrf = ""
        if account:
            _, headers, body = self.request("/api/auth/login", method="POST",
                body={key: account[key] for key in ("username", "password")},
                headers={"X-Spofify-Login": "1", "X-Spofify-Session-Mode": "rolling-v1"})
            parsed = json.loads(body)
            check(parsed.get("accountId") == account["userId"], "logged in as the wrong fixture account")
            check(parsed.get("csrfToken") and headers.get("Set-Cookie", "").startswith("spofify_session="),
                  "fixture login did not issue session credentials")
            self.cookie, self.csrf = headers["Set-Cookie"].split(";", 1)[0], parsed["csrfToken"]

    def request(self, path, *, method="GET", body=None, headers=None, status=200):
        check(path.startswith("/") and not path.startswith("//") and not urlsplit(path).fragment,
              "fixture request must stay on its loopback origin")
        request_headers = {"Origin": self.origin, **(headers or {})}
        if self.cookie:
            request_headers["Cookie"] = self.cookie
        if self.csrf and method not in ("GET", "HEAD"):
            request_headers["X-CSRF-Token"] = self.csrf
        if body is not None:
            request_headers["Content-Type"] = "application/json"
            body = json.dumps(body).encode()
        conn = http.client.HTTPConnection("127.0.0.1", self.port, timeout=6)
        try:
            conn.request(method, path, body, request_headers)
            response = conn.getresponse()
            data = response.read(2 * 1024 * 1024 + 1)
            check(len(data) <= 2 * 1024 * 1024, "fixture response exceeded bound")
            allowed = status if isinstance(status, tuple) else (status,)
            check(response.status in allowed, f"{method} {path.split('?')[0]}: HTTP {response.status}, expected {status}")
            return response.status, response.headers, data
        finally:
            conn.close()

    def json(self, path, **kwargs):
        return json.loads(self.request(path, **kwargs)[2])


def account_checks(ready, api, other):
    from playback import Peer

    playlists = [t for t in ready["targets"] if t["kind"] == "playlist"]
    expected = {p["id"] for p in playlists}

    def scoped_ids(peer):
        return {p["id"] for p in peer.personal_snapshot["playlists"]}

    with Peer(api, name="Acceptance writer") as writer, Peer(api) as observer:
        check(scoped_ids(writer) == scoped_ids(observer) == expected, "primary playlist snapshot scope")
        check(writer.personal_snapshot["recent"] == observer.personal_snapshot["recent"] == [], "fresh fixture history")
        written = {}
        for target in playlists:
            listing = observer.playlist(target["id"])
            summary, rows = listing["summary"], listing["rows"]
            check(summary["kind"] == target["playlistKind"] and summary["name"] == target["name"] and
                  len(rows) == target["total"], "complete scoped playlist rows")
            check(sum(r["track"]["id"] == ready["duplicateTrackId"] for r in rows) == 3,
                  "playlist lost duplicate occurrences")
            if summary["kind"] == "manual":
                manual, manual_rows = target, rows
            else:
                check(listing["definition"]["duplicates"] == "keep", "derived rules missing from scoped read")

        # Real commands run on the store's account lane. Match committed deltas
        # from another connection, then ask a fresh connection to read the DB.
        name = "Disposable acceptance edits"
        track = ready["duplicateTrackId"]
        writer.personal("createPlaylist", name=name, description="Socket round trip", trackId=track)
        created = observer.personal_change(lambda ch: ch.get("playlist", {}).get("name") == name)["playlist"]
        pid = created["id"]
        first = observer.playlist(pid)["rows"]
        check(created["trackCount"] == len(first) == 1 and first[0]["track"]["id"] == track,
              "playlist creation did not commit its initial entry")
        writer.personal("addEntry", playlistId=pid, trackId=track)
        added = observer.personal_change(lambda ch: ch.get("playlist", {}).get("id") == pid
                                         and "added" in ch.get("entries", {}))["entries"]["added"]
        check(added["track"]["id"] == track and added["id"] != first[0]["id"], "duplicate write lost identity")
        writer.personal("moveEntry", playlistId=pid, entryId=added["id"], after="")
        observer.personal_change(lambda ch: ch.get("entries", {}).get("moved") == added["id"])
        with Peer(api, name="Acceptance reconnect") as readback, Peer(other) as foreign:
            check(scoped_ids(readback) == expected | {pid}, "fresh account socket missed committed playlist")
            rows = readback.playlist(pid)["rows"]
            check([r["id"] for r in rows] == [added["id"], first[0]["id"]] and
                  all(r["track"]["id"] == track for r in rows), "fresh readback lost duplicate move")
            check(scoped_ids(foreign) == {ready["otherPlaylistId"]} and
                  not foreign.snapshot["player"]["queue"], "foreign account socket leaked personal or player state")
        writer.personal("deletePlaylist", playlistId=pid)
        observer.personal_change(lambda ch: ch.get("deleted") == pid)

        # Play selects the exact duplicate occurrence. This is real canonical
        # state, not evidence of rendering: all these observers have canPlay=false.
        chosen = manual_rows[1]
        writer.send(dict(type="play", selection=dict(kind="track", track=dict(id=track, entryId=chosen["id"]),
                                                      context=dict(kind="playlist", id=manual["id"]))))
        replace = observer.wait_frame(lambda f: f.get("player", {}).get("queue", {}).get("kind") == "replace")["player"]
        player = observer.snapshot["player"]
        queue, cursor = player["queue"], player["cursor"]
        check(replace["source"] == writer.connection_id and len(queue) == manual["total"] and
              queue[0]["key"] == cursor["key"] and queue[0]["entryId"] == chosen["id"] and
              {r["entryId"] for r in queue if r["id"] == track} == {r["id"] for r in manual_rows[:3]} and
              cursor["desired"] and not cursor["playing"] and not observer.snapshot["owner"],
              "canonical Play lost selection, occurrence keys, or desired/observed distinction")
        writer.send(dict(type="add", placement="next", selection=dict(kind="track", track=dict(id=track))))
        insert = observer.wait_frame(lambda f: f.get("player", {}).get("queue", {}).get("kind") == "insert")["player"]
        now = observer.snapshot["player"]
        check(insert["source"] == writer.connection_id and now["cursor"] == cursor and
              [r["key"] for r in now["queue"]] == [queue[0]["key"], insert["queue"]["rows"][0]["key"],
                                                     *(r["key"] for r in queue[1:])] and
              now["queue"][1]["id"] == track and now["queue"][1]["intent"] == "next",
              "canonical Add did not insert a new next occurrence")
        with Peer(api, name="Acceptance reconnect") as readback:
            check(readback.snapshot["player"]["queue"] == now["queue"] and
                  readback.snapshot["player"]["cursor"] == cursor and scoped_ids(readback) == expected,
                  "fresh socket did not read back canonical player/playlist state")
        writer.send(dict(type="close"))
        observer.wait(lambda snapshot: not snapshot["player"]["queue"] and not snapshot["player"]["cursor"]["key"])
    print("PASS real account socket: scoped personal snapshots/rows; committed create, add, move, delete; "
          "canonical Play/Add and reconnect readback (no audio executor)", flush=True)


def api_checks(ready):
    from playback import CONNECT

    Api(ready).request(CONNECT, method="POST", body={}, status=401)
    api = Api(ready, ready)
    other = Api(ready, ready["otherAccount"])
    targets = ready["targets"]
    account_checks(ready, api, other)

    search = api.json("/api/library?" + urlencode({"kind": "tracks", "scope": "all", "q": "Synthetic Tone", "limit": 10}))
    check(search["total"] == 47 and len(search["items"]) == 10 and
          all(t["name"].startswith("Synthetic Tone ") for t in search["items"]), "catalog search page")
    check(api.json("/api/library?kind=tracks&scope=all&q=nomatchzzzz")["total"] == 0, "catalog search no-match")
    album = next(target for target in targets if target["kind"] == "album")
    album_path = "/api/library/albums/" + quote(album["id"], safe="")
    check(api.json(album_path)["item"]["name"] == album["name"], "encoded album detail")
    tracks = api.json("/api/library?" + urlencode({"kind": "tracks", "scope": "all", "albumId": album["id"], "limit": 10}))
    check(tracks["total"] == 47 and len(tracks["items"]) == 10, "encoded album membership")
    unified_status, _, unified_body = api.request("/search?query=Synthetic+Tone", status=(200, 503))
    unified = json.loads(unified_body)
    if unified_status == 200:
        check(any(item["id"] == ready["duplicateTrackId"] for item in unified["items"]), "unified search lost fixture track")
    else:
        check(unified["code"] == "SEARCH_UNAVAILABLE", "unconfigured unified search error")
    print("PASS real catalog search and album browsing; unified search " +
          ("returned synthetic tracks" if unified_status == 200 else "honestly unavailable (no Typesense)"), flush=True)

    for target in targets:
        path = target["manifestUrl"]
        if target["kind"] == "playlist":
            other.request(path, status=404)
        page = api.json(path + "?limit=40")
        cursor = page["nextCursor"]
        check(cursor and len(page["items"]) == 40 and page["version"] == 1, "offline first page")
        next_page = api.json(path + "?" + urlencode({"cursor": cursor}))
        items = page["items"] + next_page["items"]
        check(next_page["nextCursor"] is None and page["fence"] == next_page["fence"] and
              len(items) == page["total"] == next_page["total"] == target["total"] and
              [i["position"] for i in items] == list(range(len(items))) and
              len({i["occurrenceId"] for i in items}) == len(items), "offline manifest completeness")
        ids = [item["track"]["id"] for item in items]
        check(ready["metadataOnlyTrackId"] not in ids and ready["missingFileTrackId"] in ids and
              ids.count(ready["duplicateTrackId"]) == (3 if target["kind"] == "playlist" else 1),
              "offline eligibility and duplicates")
    media = ready["media"]
    descriptor = api.json(media["mediaUrl"])
    check(all(descriptor[key] == value for key, value in media.items() if key != "mediaUrl"),
          "offline media descriptor")
    download = descriptor["downloadUrl"]
    _, headers, audio = api.request(download)
    check(len(audio) == media["byteLength"] and hashlib.sha256(audio).hexdigest() == media["sha256"] and
          audio[:4] == b"RIFF" and audio[8:12] == b"WAVE", "synthetic WAV or digest mismatch")
    etag = '"' + media["mediaVersion"] + '"'
    check(headers.get("ETag") == etag, "offline digest ETag")
    _, headers, part = api.request(download, headers={"Range": "bytes=8-11", "If-Range": etag}, status=206)
    check(headers.get("Content-Range") == f"bytes 8-11/{len(audio)}" and part == b"WAVE", "offline media Range")
    missing = quote(ready["missingFileTrackId"], safe="")
    check(api.json(f"/api/me/offline/v1/tracks/{missing}/media", status=404)["error"]["code"] ==
          "MEDIA_UNAVAILABLE", "published missing file handling")
    track = quote(media["trackId"], safe="")
    check(api.request(f"/api/library/tracks/{track}/audio")[2] == audio, "online listening audio")
    print("PASS four paginated offline manifests, verified audio and Range, missing-file rejection", flush=True)
    return api
