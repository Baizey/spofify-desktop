#!/usr/bin/env python3
"""Native Search entity-menu evidence against a disposable synthetic account.

Run both modes in private Bash workers using supplied native debug binaries
(SLINT_EMIT_DEBUG_INFO=1 and --features slint/mcp):
  python3 -B scripts/acceptance/search_entity_menus.py --mode before \
    --desktop /path/to/base --capture-dir ~/repos/spofify-desktop-temporary/search-entity-menus/before
  python3 -B scripts/acceptance/search_entity_menus.py --mode after \
    --desktop /path/to/candidate --capture-dir ~/repos/spofify-desktop-temporary/search-entity-menus/after

Requires Cage/Wayland, wlr-randr, ImageMagick, and owned worker-local TCP.
No Go service, installed app, real account, audio download, or audio playback
is used. Play/Add assertions inspect actual outgoing socket frames.
Use --wire-port and --mcp-port if worker TCP grants are scoped to specific ports.
"""

import argparse
from contextlib import contextmanager
import hashlib
import http.server
import json
from pathlib import Path
import re
import shutil
import signal
import struct
import subprocess
import sys
from threading import Lock
from types import SimpleNamespace
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit
import zlib

sys.dont_write_bytecode = True
from catalog_continuity import View, query
from fixture import Failure, check, private_env
from native import (ALT, ESCAPE, LEFT, RETURN, assert_disjoint, bounds, contains, desktop,
                    element_id, element_type, wait)
from settings import resize
import wire_ui as base


MODE = (1280, 840)
QUERY = "Prism"
EXPIRY = "2099-01-01T00:00:00Z"


def item(kind, key, name, *, available=True, **metadata):
    return dict(base.catalog_item(kind, f"prism-{key}", f"{QUERY} {name}", **metadata),
                hasAudio=kind == "track" and available,
                coverage=dict(tracks=1, inLibrary=int(available), albums=int(kind == "artist")))


def credit(artist):
    return {key: artist[key] for key in ("id", "name")}


ARTIST = item("artist", "artist", "Glass Ensemble", genres=["Chamber"])
QUIET_ARTIST = item("artist", "quiet-artist", "Silent Ensemble", available=False)
ALBUM = item("album", "album", "Glass Record", artists=[credit(ARTIST)], albumType="album",
             releaseDate="2025-05-01")
QUIET_ALBUM = item("album", "quiet-album", "Silent Record", available=False,
                   artists=[credit(QUIET_ARTIST)], albumType="album")


def recording(key, name, album, *, available=True, number=1):
    return item("track", key, name, available=available, artists=album["artists"],
                albumId=album["id"], albumName=album["name"], durationMs=113000,
                discNumber=1, trackNumber=number)


TRACK = recording("track", "Refraction", ALBUM)
TRACK_AUDIO = f"/api/library/tracks/{TRACK['id']}/audio"
NO_ART = recording("no-art", "Diffraction", ALBUM, number=2)
SILENT = recording("silent", "Transmission", QUIET_ALBUM, available=False)
TOP = (ALBUM, ARTIST, TRACK, NO_ART, SILENT, QUIET_ALBUM, QUIET_ARTIST)
ROWS = TOP + tuple(recording(f"tail-{i}", f"Study {i:02}", ALBUM, number=i + 2) for i in range(1, 14))
ENTITIES = {f"/api/library/{r['kind']}s/{r['id']}": r for r in TOP if r['kind'] != 'track'}
COLORS = {r["id"]: bytes.fromhex(color) for r, color in (
    (ALBUM, "e66844"), (ARTIST, "44a3e0"), (TRACK, "72d849"),
    (QUIET_ALBUM, "bb70e6"), (QUIET_ARTIST, "e1ad4c"))}
GOLD = bytes.fromhex("dfbd78")


def artwork(color):
    # High-contrast asymmetric squares distinguish typed artwork from a glyph,
    # from each other, and from an unexpected fallback to the parent album.
    def chunk(kind, payload):
        return (struct.pack("!I", len(payload)) + kind + payload +
                struct.pack("!I", zlib.crc32(kind + payload)))
    pixels = b"".join(b"\0" + (bytes.fromhex("243954") * 11 + color * 37 if y < 19 else color * 48)
                      for y in range(48))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack("!IIBBBBB", 48, 48, 8, 2, 0, 0, 0)) +
            chunk(b"IDAT", zlib.compress(pixels)) + chunk(b"IEND", b""))


def collection(entity):
    return dict(kind="collection", context={key: entity[key] for key in ("kind", "id", "name")})


def play(selection):
    return dict(type="play", selection=selection, shuffle=False)


def add(selection):
    return dict(type="add", selection=selection, placement="low")


class Wire:
    def __init__(self, output, port):
        self.log = output / "wire.jsonl"
        self.port = port
        self.lock = Lock()
        self.events = []
        self.offline = False

    def record(self, event, **fields):
        with self.lock:
            entry = dict(event=event, **fields)
            self.events.append(entry)
            with self.log.open("a") as log:
                print(json.dumps(entry, sort_keys=True), file=log)

    def matches(self, kind):
        with self.lock:
            return [e for e in self.events if e["event"] == kind]

    def commands(self):
        return [entry["command"] for entry in self.matches("command")]

    @contextmanager
    def serve(self):
        wire = self

        class Account(base.AccountWire):
            def serve(self):
                try:
                    super().serve()
                finally:
                    wire.record("socket-closed")

            def initial(self):
                super().initial()
                wire.record("snapshot", owner=bool(self.snapshot["owner"]))

            def message(self):
                while True:
                    msg = super().message()
                    if msg.get("type") not in ("play", "add"):
                        return msg
                    # No execution is simulated: keep the synthetic snapshot
                    # idle. Only the exact frame received from the desktop counts.
                    check(len(wire.commands()) < 16, "unexpected Search command flood")
                    wire.record("command", command=msg)

        class Server(http.server.ThreadingHTTPServer):
            def __init__(self, address, handler):
                class Handler(handler):
                    def setup(self):
                        super().setup()
                        self.connection.settimeout(30)

                    def parse_request(self):
                        if not super().parse_request():
                            return False
                        path = urlsplit(self.path).path
                        # Record no cookies, query strings, tickets, or login body.
                        if wire.offline:
                            wire.record("unexpected-offline-http", method=self.command, path=path)
                            self.error(403)
                            return False
                        return True

                    def reply(self, value=None, status=200, headers=None):
                        if self.path in ("/api/auth/login", base.TICKET_PATH):
                            wire.record("auth-status", route=self.path, status=status)
                        return super().reply(value, status, headers)

                    def do_GET(self):
                        route = urlsplit(self.path)
                        path = route.path
                        args = parse_qs(route.query, keep_blank_values=True)
                        if path.endswith("/audio"):
                            valid = self.path == TRACK_AUDIO and self.trusted() and self.owned()
                            wire.record("audio", path=path, stub=valid)
                            if path != TRACK_AUDIO or self.path != TRACK_AUDIO:
                                return self.error(404)
                            if valid:
                                # Local preview may request the clicked recording
                                # while the server is still idle. Never serve audio.
                                return self.reply(status=404)
                            return  # trusted()/owned() already sent their rejection
                        if path.endswith("/artwork"):
                            if not (self.trusted() and self.owned()):
                                return
                            item_path = path.removesuffix("/artwork")
                            valid = set(ENTITIES) | {f"/api/library/tracks/{t['id']}" for t in ROWS if t['kind'] == 'track'}
                            if item_path not in valid or args:
                                wire.record("invalid", path=path)
                                return self.error(404)
                            color = COLORS.get(item_path.rsplit("/", 1)[1])
                            wire.record("artwork", path=path, status=200 if color else 404)
                            if not color:
                                return self.error(404)
                            body = artwork(color)
                            self.close_connection = True
                            self.send_response(200)
                            self.send_header("Content-Type", "image/png")
                            self.send_header("Content-Length", str(len(body)))
                            self.send_header("Connection", "close")
                            self.end_headers()
                            try:
                                self.wfile.write(body)
                            except OSError:
                                pass
                            return
                        if path == "/search":
                            if not (self.trusted() and self.owned()):
                                return
                            if args != {"query": [QUERY], "limit": [str(len(ROWS))]}:
                                wire.record("invalid", path=path)
                                return self.error(400)
                            wire.record("search", query=QUERY, rows=len(ROWS))
                            return self.reply(dict(items=ROWS, resultCount=len(ROWS), expiresAt=EXPIRY))
                        if path == "/api/library" or path in ENTITIES:
                            if not (self.trusted() and self.owned()):
                                return
                            if path in ENTITIES and not args:
                                wire.record("detail", path=path)
                                return self.reply(dict(item=ENTITIES[path], revision=base.REVISION))
                            albums = {ALBUM["id"]: [TRACK, NO_ART], QUIET_ALBUM["id"]: []}
                            artists = {ARTIST["id"]: [dict(ALBUM, relation="release")],
                                       QUIET_ARTIST["id"]: []}
                            for kind, selector, by_id, options in (
                                    ("tracks", "albumId", albums, {}),
                                    ("albums", "artistId", artists, {"releaseGroup": ["all"]})):
                                ident = args.get(selector, [""])[0]
                                if (path == "/api/library" and ident in by_id and args == dict(
                                        kind=[kind], scope=["all"], availability=["in-library"],
                                        offset=["0"], limit=["20"], **{selector: [ident]}, **options)):
                                    wire.record("listing", id=ident, kind=kind)
                                    return self.reply(dict(items=by_id[ident], total=len(by_id[ident]),
                                                           offset=0, limit=20, revision=base.REVISION))
                            wire.record("invalid", path=path)
                            return self.error(400)
                        if path not in ("/api/auth/session", base.SOCKET_PATH):
                            wire.record("invalid", path=path)
                        return super().do_GET()

                check(address == ("127.0.0.1", 0), "expected owned worker-local listener")
                super().__init__((address[0], wire.port), Handler)

        facade = SimpleNamespace(server=SimpleNamespace(BaseHTTPRequestHandler=http.server.BaseHTTPRequestHandler,
                                                         ThreadingHTTPServer=Server))
        with (patch.object(base, "http", facade), patch.object(base, "AccountWire", Account),
              patch.object(base, "PLAYLISTS", {}), base.wire_fixture() as ready):
            yield ready
        check(not self.matches("invalid"), f"unexpected fixture GET: {self.matches('invalid')}")
        check(all(e["path"] == TRACK_AUDIO and e["stub"] for e in self.matches("audio")),
              f"unexpected audio request: {self.matches('audio')}")
        check(not self.matches("unexpected-offline-http"), "unexpected HTTP after offline transition")


class Search(View):
    def __init__(self, window, output, magick):
        self.window, self.output, self.magick = window, output, magick

    def frame(self, name=None):
        png = super().frame(name)
        check(png.startswith(b"\x89PNG\r\n\x1a\n") and png[12:16] == b"IHDR" and
              struct.unpack("!II", png[16:24]) == MODE, "expected a 1280×840 native PNG")
        if name:
            print(f"CAPTURE {self.output / (name + '.png')}", flush=True)
        return png

    def nodes(self):
        # A Back or mode transition may replace the scroll element; don't keep
        # its original handle. Geometry and action identity come from one tree.
        return self.window.elements()

    def viewport(self, elements=None):
        scroll = element_id(self.nodes() if elements is None else elements, "catalog-scroll")
        check(scroll, "missing catalog viewport")
        return bounds(scroll)

    def line(self, item, *, visible=True):
        nodes = self.nodes()
        name = item["name"]
        title = "Open album for " + name if item["kind"] == "track" else "Open " + name
        for node in nodes:
            if node.get("accessibleRole") == "Button" and node.get("accessibleLabel") == title:
                row = next((e for e in nodes if element_type([e], "MediaLine") and contains(e, node)), None)
                scroll = element_id(nodes, "catalog-scroll")
                if row and (not visible or scroll and contains(scroll, row)):
                    return row
        return None

    def result(self, item):
        return wait(f"fully visible Search row: {item['name']}", lambda: self.line(item), 5)

    def control(self, item, label):
        return self.window.find(re.escape(label), roles=("Button",), within=self.result(item), timeout=3)

    def park(self):
        self.window.mcp.call("move_pointer", {"windowHandle": self.window.handle,
                                              "position": {"x": 4, "y": 5}})

    def hover(self, element):
        x, y, right, bottom = bounds(element)
        self.window.mcp.call("move_pointer", {"windowHandle": self.window.handle,
            "position": {"x": (x + right) / 2, "y": (y + bottom) / 2}})

    def assert_query(self, *, heading=True):
        if heading:
            self.window.title("Search")
        field = self.window.find(r"Search.*", roles=("TextInput",))
        check(field.get("accessibleValue") == QUERY, "Search no longer holds the exact mixed query")

    def artwork_evidence(self, mode):
        self.park()
        boxes = {}
        for item in TOP:
            row = self.result(item)
            cover = element_type(self.window.subtree(row), "TrackCover")
            check(cover and contains(row, cover), f"missing wide artwork/fallback slot: {item['name']}")
            boxes[item["name"]] = dict(row=bounds(row), cover=bounds(cover))
        def measurements(png):
            report = {}
            for item in TOP:
                name = item["name"]
                rgb, width, height = self.rgb(png, boxes[name]["cover"])
                counts = {ident: sum(rgb[i:i+3] == color for i in range(0, len(rgb), 3))
                          for ident, color in COLORS.items()}
                # Reserve an interior corner sample for badge comparison, not
                # the clipped outer edge of a rounded artwork shape.
                corner = [rgb[3 * (y * width + x):3 * (y * width + x) + 3]
                          for y in range(height - 13, height - 5) for x in range(width - 13, width - 5)]
                report[name] = dict(**boxes[name], color_pixels=counts,
                                    corner_own_color=corner.count(COLORS.get(item["id"])),
                                    cover_color_count=len({rgb[i:i+3] for i in range(0, len(rgb), 3)}))
            return report
        def rendered():
            image = self.frame()
            report = measurements(image)
            return image if all(report[r["name"]]["color_pixels"][r["id"]] > 450
                                for r in TOP if r["id"] in COLORS) else None
        wait("typed album, artist and track artwork rendered in Search", rendered)
        png = self.frame(f"{mode}-01-mixed-search-1280")
        report = measurements(png)
        for item in (NO_ART, SILENT):
            sample = report[item["name"]]
            check(not any(sample["color_pixels"].values()) and sample["cover_color_count"] > 4,
                  f"{item['name']} did not paint its own missing-image fallback")
        artist_corner = report[ARTIST["name"]]["corner_own_color"]
        album_corner = report[ALBUM["name"]]["corner_own_color"]
        track_corner = report[TRACK["name"]]["corner_own_color"]
        # Before: artist only. After: both typed images have marks, while
        # artwork on a track with audio must still occupy this whole patch.
        (self.output / "geometry.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        check(artist_corner < 30 and track_corner > 45 and
              (album_corner > 45 if mode == "before" else album_corner < 30),
              f"unexpected {mode} artwork corner pixels: album {album_corner}/64, "
              f"artist {artist_corner}/64, track {track_corner}/64")
        print(f"PASS {mode} typed artwork and artless fallbacks; artist/album corner pixels {artist_corner}/{album_corner}",
              flush=True)

    def menu_state(self, item, *, opened):
        title = item["name"]
        return self.control(item, ("Hide" if opened else "Show") + " actions for " + title)

    def menu(self, item, options, *, keyboard=False):
        if keyboard:
            self.window.key(RETURN)
        else:
            self.window.click_element(self.menu_state(item, opened=False))
        title = item["name"]
        self.menu_state(item, opened=True)
        expected = [op + " · " + title for op in options]
        for label in expected:
            self.window.find(re.escape(label), roles=("Button",), timeout=3)
        labels = [node.get("accessibleLabel") for node in self.nodes()
                  if node.get("accessibleRole") == "Button" and
                  (node.get("accessibleLabel") or "").endswith(" · " + title)]
        check(labels == expected, f"{title}: menu options {labels}, expected {expected}")
        return [re.escape(label) for label in expected]

    def dismissed(self, item):
        self.menu_state(item, opened=False)
        self.window.absent(r".+ · " + re.escape(item["name"]), roles=("Button",))
        self.assert_query()

    def focused_trigger(self, item):
        trigger = self.menu_state(item, opened=False)
        x, y, right, bottom = bounds(trigger)
        rgb, width, height = self.rgb(self.frame(), (x + 8, y, right - 8, bottom))
        def line(index):
            start = index * width * 3
            return sum(rgb[i:i+3] == GOLD for i in range(start, start + width * 3, 3)) >= width * .7
        return any(line(i) for i in range(3)) and any(line(height - 1 - i) for i in range(3))


def capture_cases(view, wire, mode):
    w = view.window
    view.assert_query()
    try:
        view.artwork_evidence(mode)
    except Failure:
        if not (view.output / f"{mode}-01-mixed-search-1280.png").exists():
            view.frame(f"{mode}-01-mixed-search-diagnostic")
        raise
    view.accepted_results(len(ROWS))
    no_art_path = f"/api/library/tracks/{NO_ART['id']}/artwork"
    wait("the artless track gets a typed 404, not merely a pending image", lambda: any(
        e["path"] == no_art_path and e["status"] == 404 for e in wire.matches("artwork")), 5)
    for item in (TRACK, NO_ART):
        view.control(item, "Play " + item["name"])
    silent = w.subtree(view.result(SILENT))
    check(any("Unavailable" in (e.get("accessibleDescription") or "").split(", ")
              or e.get("accessibleLabel") == "Unavailable" for e in silent),
          "missing Unavailable status on the non-audio recording")
    check(not any(e.get("accessibleRole") == "Button" and (e.get("accessibleLabel") or "").startswith("Play ")
                  for e in silent), "unavailable track exposes playback")
    if mode == "before":
        view.menu(ALBUM, ["Add to queue"])
        view.frame("before-02-album-menu")
        w.key(ESCAPE)
        view.dismissed(ALBUM)
        view.menu(ARTIST, ["Add to queue"])
        view.frame("before-03-artist-menu")
        w.key(ESCAPE)
        view.dismissed(ARTIST)
        for item in (QUIET_ALBUM, QUIET_ARTIST):
            w.absent(re.escape("Show actions for " + item["name"]), roles=("Button",))
        check(not wire.commands(), "baseline captures unexpectedly sent playback or queue commands")
        return []

    expected = []
    def command_sent(frame):
        expected.append(frame)
        wait(f"wire {frame}", lambda: len(wire.commands()) >= len(expected), 5)
        check(wire.commands() == expected, f"wrong exact commands: {wire.commands()}; expected {expected}")

    for item in (ALBUM, ARTIST):
        options = ["Play", "Add to queue"]
        menu = view.menu(item, options)
        view.frame(f"after-02-{item['kind']}-playback-menu")
        w.key(ESCAPE)
        view.dismissed(item)
        wait("visible keyboard focus returned after Escape", lambda: view.focused_trigger(item), 3)
        view.menu(item, options, keyboard=True)
        w.key(RETURN)
        command_sent(play(collection(item)))
        view.dismissed(item)
        view.menu(item, options)
        w.click(menu[1])
        command_sent(add(collection(item)))
        view.dismissed(item)
    print("PASS exact album and artist collection contexts for Play and Add; Escape restores trigger", flush=True)

    for item in (QUIET_ALBUM, QUIET_ARTIST):
        options = ["Open " + item["kind"]]
        top = bounds(view.result(item))[1]
        view.menu(item, options)
        view.frame(f"after-03-{item['kind']}-fallback-menu")
        w.key(RETURN)
        w.title(item["name"])
        # Use the existing native history shortcut rather than clicking a new
        # focus target; focus must restore to the exact originating ellipsis.
        w.shortcut(ALT, LEFT)
        view.dismissed(item)
        check(abs(bounds(view.result(item))[1] - top) <= 1, "Back moved the fallback result")
        wait("visible ellipsis focus restored by Back", lambda: view.focused_trigger(item), 3)
        view.frame(f"after-04-{item['kind']}-back-focus")
        view.menu(item, options, keyboard=True)
        w.key(ESCAPE)
        view.dismissed(item)
    expected_details = {f"/api/library/{item['kind']}s/{item['id']}" for item in (QUIET_ALBUM, QUIET_ARTIST)}
    check({event["path"] for event in wire.matches("detail")} == expected_details,
          "fallback choices did not use the two exact typed routes")
    check(wire.commands() == expected, "opening a fallback or dismissing its menu issued playback")
    print("PASS fallback Open only, exact detail route, and Back restores each ellipsis", flush=True)

    title = "Open album for " + TRACK["name"]
    artist = "Open artist: " + ARTIST["name"]
    play_label = "Play " + TRACK["name"]
    more = "Show actions for " + TRACK["name"]
    assert_disjoint([(label, view.control(TRACK, label)) for label in (title, artist, play_label, more)])
    for label, entity in ((title, ALBUM), (artist, ARTIST)):
        w.click_element(view.control(TRACK, label))
        w.title(entity["name"])
        view.frame(f"after-05-track-{entity['kind']}-link")
        w.shortcut(ALT, LEFT)
        view.assert_query()
    check({event["path"] for event in wire.matches("detail")} == expected_details |
          {f"/api/library/{item['kind']}s/{item['id']}" for item in (ALBUM, ARTIST)},
          "track title and credit did not open the exact canonical Album and Artist routes")
    check(wire.commands() == expected, "track links started playback or queued the recording")
    view.hover(view.control(TRACK, play_label))
    w.click_element(view.control(TRACK, play_label))
    command_sent(play(dict(kind="track", track=dict(id=TRACK["id"]))))
    view.assert_query()
    view.menu(TRACK, ["Add to queue", "Add to playlist", "Save offline"])
    view.frame("after-06-track-menu")
    print("PASS track title, credit, Play and ellipsis remain distinct", flush=True)

    failures = []
    def wheel_dismisses(item):
        # Pointer wheel in the results lane, away from the trailing popup.
        # Watch a row that survives virtualization at either scroll position.
        anchor = NO_ART
        top = bounds(view.result(anchor))[1]
        view.scroll(-320)
        closed = r".+ · " + re.escape(item["name"])
        def moved():
            row = view.line(anchor, visible=False)
            return row and bounds(row)[1] < top - 20
        tag = f"after-07-{item['kind']}-wheel"
        try:
            wait("wheel moved the actual Search list", moved, 3)
            w.absent(closed, roles=("Button",))
            # The Search heading scrolls away; the sticky input stays visible.
            view.assert_query(heading=False)
            view.frame(tag + "-dismissed")
            print(f"PASS wheel moved Search and dismissed the {item['kind']} menu", flush=True)
        except Failure as error:
            failure = f"{item['kind']} menu wheel: {error}"
            failures.append(failure)
            print(f"FAIL {failure}", flush=True)
            view.frame(tag + "-failed")
            # Continue independent evidence collection. An Escape used only
            # after a failed assertion must never count as wheel dismissal.
            w.key(ESCAPE)
            w.absent(closed, roles=("Button",))

    wheel_dismisses(TRACK)
    view.scroll(100000)
    view.result(ALBUM)
    view.menu(ALBUM, ["Play", "Add to queue"])
    wheel_dismisses(ALBUM)
    view.scroll(100000)
    view.result(ALBUM)
    view.menu(ALBUM, ["Play", "Add to queue"])
    outside = w.title("Search")
    x, y = w.pointer_position_for_click("outside Search popup", element=outside)
    left, top, right, bottom = bounds(outside)
    check(left <= x < right and top <= y < bottom, "outside click missed the Search heading")
    view.dismissed(ALBUM)
    view.frame("after-07-outside-click-dismissed")
    check(wire.commands() == expected, "track ellipsis, wheel, or outside click issued a playback command")
    print("PASS pointer click outside the album menu dismisses it without navigation or playback", flush=True)

    w.click("Settings")
    w.section("Settings")
    w.click("Work offline")
    w.find("Go online", roles=("Button",))
    wait("owned account socket to retire", lambda: wire.matches("socket-closed"), 8)
    wire.offline = True
    offline_search_count = len(wire.matches("search"))
    query(w, QUERY)
    view.assert_query()
    # Don't equate a transient loading or empty-query frame with a projection.
    w.title("No matching results", timeout=5)
    for item in TOP:
        name = re.escape(item["name"])
        w.absent(r"(?:Open |Open album for |Play |Play track: |Show actions for )" + name, roles=("Button",))
    view.frame("after-08-offline-unsaved-not-playable")
    check(wire.commands() == expected and len(wire.matches("search")) == offline_search_count,
          "offline Search used the online backend")
    print("PASS offline Search excludes unsaved online rows; GAP: positive verified local audio not seeded", flush=True)
    return failures


def interrupted(*_):
    raise KeyboardInterrupt


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--mode", required=True, choices=("before", "after"))
    parser.add_argument("--desktop", type=Path, required=True)
    parser.add_argument("--capture-dir", type=Path, required=True, help="new path under task scratch")
    parser.add_argument("--wire-port", type=int, default=0)
    parser.add_argument("--mcp-port", type=int, default=0)
    args = parser.parse_args()
    check(all(0 <= port <= 65535 for port in (args.wire_port, args.mcp_port)) and
          (not args.wire_port or args.wire_port != args.mcp_port), "ports must be distinct or ephemeral")
    scratch = (Path.home() / "repos/spofify-desktop-temporary/search-entity-menus").resolve()
    output = args.capture_dir.resolve()
    check(output != scratch and output.is_relative_to(scratch), f"use a new evidence directory under {scratch}")
    binary = args.desktop.resolve(strict=True)
    magick = shutil.which("magick") or shutil.which("convert")
    check(magick, "ImageMagick required to check rendered artwork and focus rings")
    scratch.mkdir(mode=0o700, exist_ok=True)
    output.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    output.mkdir(mode=0o700)
    with binary.open("rb") as source:
        (output / "binary.sha256").write_text(hashlib.file_digest(source, "sha256").hexdigest() + "\n")
    root = output / "private"
    root.mkdir(mode=0o700)
    env = private_env(root)
    no_bus = "unix:path=" + env["XDG_RUNTIME_DIR"] + "/no-bus"
    env.update(DBUS_SESSION_BUS_ADDRESS=no_bus, DBUS_SYSTEM_BUS_ADDRESS=no_bus)
    print(f"Native {args.mode} evidence: {output}", flush=True)
    wire = Wire(output, args.wire_port)
    with wire.serve() as ready, desktop(binary, root, env, multisize=True, mcp_port=args.mcp_port) as window:
        resize(window, MODE)
        check(window.window_properties()["scaleFactor"] == 1, "expected scale-1 1280px native window")
        window.find("Sign in")
        window.fill("Server( origin| URL| address)?", ready["origin"])
        window.fill("Username", ready["username"])
        window.fill("Password", ready["password"])
        window.click("Sign in")
        try:
            window.section("History")
        except Failure as error:
            labels = [e.get("accessibleLabel") for e in window.elements() if e.get("accessibleRole") == "Text"]
            # Never persist populated login fields, or take a screenshot of them.
            for secret in (ready["username"], ready["password"], ready["origin"]):
                labels = [str(label).replace(secret, "<fixture>") for label in labels]
            raise Failure(f"{error}; window text: {labels}") from error
        wait("synthetic account snapshot with an owned playback output", lambda: any(
            event["owner"] for event in wire.matches("snapshot")), 12)
        close = next((e for e in window.elements() if e.get("accessibleLabel") == "Close play queue"
                      and e.get("accessibleRole") == "Button" and e.get("accessibleEnabled")), None)
        if close:
            window.click_element(close)
        query(window, QUERY)
        view = Search(window, output, magick)
        failures = capture_cases(view, wire, args.mode)
    if args.mode == "before":
        check(not wire.matches("audio"), "baseline capture attempted to fetch audio")
    check(not failures, "; ".join(failures))
    print(f"PASS native Search entity menus ({args.mode}): {output}", flush=True)


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main()
    except (Failure, OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        sys.exit(f"FAIL: {error or type(error).__name__}")
