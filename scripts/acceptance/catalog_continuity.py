#!/usr/bin/env python3
"""Native catalog continuation against disposable synthetic wire data only.

Build the desktop with SLINT_EMIT_DEBUG_INFO=1 and --features slint/mcp.
Run: python3 -B scripts/acceptance/catalog_continuity.py \\
    --desktop /path/to/desktop --capture-dir /tmp/catalog-continuity-unique
Requires private Cage/Wayland, wlr-randr, ImageMagick and owned loopback TCP.
No Go server, real sign-in/store/devices, or playback is used.
"""

import argparse
from base64 import b64decode
from collections import Counter
from contextlib import contextmanager
import http.server
import json
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
from tempfile import TemporaryDirectory
from threading import Event, Lock
from time import monotonic, sleep
from types import SimpleNamespace
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit

sys.dont_write_bytecode = True
from fixture import Failure, check, private_env
from native import ALT, CONTROL, LEFT, RETURN, bounds, desktop, element_id, wait
from settings import resize
import wire_ui as base


PAGE = 20
TRACK_COUNT = 523
STOP = "More results couldn’t be loaded."
GOLD = bytes.fromhex("dfbd78")
FORBIDDEN = re.compile(r"\b(?:load more|show more|more results|(?:first|next|previous|last) page|retry|try again)\b", re.I)
CREDIT = [{"id": base.ARTIST["id"], "name": base.ARTIST_NAME}]


def numbered(kind, stem, name, count, **fields):
    return [base.catalog_item(kind, f"continuity-{stem}-{i:03}", f"{name} {i:03}", **fields)
            for i in range(1, count + 1)]


RELEASES = [dict(base.ALBUM, relation="release")] + numbered("album", "album", "Continuity release", 61,
                                   artists=CREDIT, albumType="album", relation="release")
TRACKS = numbered("track", "track", "Continuity recording", TRACK_COUNT, durationMs=1000,
                  artists=CREDIT, albumId=base.ALBUM["id"], albumName=base.ALBUM_NAME, discNumber=1)
for index, item in enumerate(TRACKS, 1):
    item["trackNumber"] = index

QUERIES = {key: numbered("artist", key, f"Continuity {key} artist", count)
           for key, count in (("wheel", 62), ("keyboard", 25), ("race", 41),
                              ("intermediate", 1), ("final", 1), ("permanent", 41), ("stuck", 41))}
QUERIES["wheel"][0] = base.ARTIST
QUERIES["keyboard"][PAGE] = base.GUEST
# The keyboard journey leaves Guest Artist open. Following that same Artist
# from the final search would pop the prior trail entry (a direct return),
# so Back would correctly restore the earlier keyboard Search instead.
QUERIES["final"][0] = base.ARTIST
EXPIRY = "2099-01-01T00:00:00Z"


def offsets(rows):
    return list(range(0, len(rows), PAGE))


class CatalogWire:
    def __init__(self, output):
        self.journal = output / "wire.jsonl"
        self.started = monotonic()
        self.lock = Lock()
        self.requests = []
        self.violations = []
        self.gates = {key: Event() for key in ("wheel", "keyboard", "race", "stuck")}
        self.race_replied = Event()

    def event(self, **event):
        with self.lock:
            with self.journal.open("a") as file:
                print(json.dumps({"seconds": round(monotonic() - self.started, 3), **event}), file=file)
            if event["event"] == "request":
                self.requests.append((event["route"], event["offset"]))
            elif event["event"] == "invalid":
                self.violations.append(event["route"])

    def seen(self, route):
        with self.lock:
            return [offset for key, offset in self.requests if route == key]

    def expect(self, route, expected):
        actual = self.seen(route)
        check(actual == expected, f"{route} wire offsets {actual}, expected {expected}")

    def release(self, route):
        self.event(event="release", route=route)
        self.gates[route].set()

    def respond(self, handler, path, args):
        # Take over only the paginated routes. Login, detail, account and other
        # endpoints retain wire_ui's origin, cookie, ticket and mutation checks.
        search = path == "/search"
        if search:
            key, offset = "", 0
            if set(args) == {"query", "limit"} and args["limit"] == [str(PAGE)] and len(args["query"]) == 1:
                key = args["query"][0].strip().casefold()
            elif set(args) == {"cursor"} and len(args["cursor"]) == 1:
                match = re.fullmatch(r"fixture-([a-z]+)-([1-9][0-9]*)", args["cursor"][0])
                if match:
                    key, offset = match[1], int(match[2])
            rows = QUERIES.get(key)
        else:
            artist = "artistId" in args
            key, rows = ("artist", RELEASES) if artist else ("album", TRACKS)
            expected = {"kind": ["albums" if artist else "tracks"], "scope": ["all"],
                        "availability": ["in-library"], "limit": [str(PAGE)]}
            if artist:
                expected.update(artistId=[base.ARTIST["id"]], releaseGroup=["all"])
            else:
                expected.update(albumId=[base.ALBUM["id"]])
            value = args.get("offset", [])
            offset = int(value[0]) if len(value) == 1 and re.fullmatch(r"[0-9]{1,4}", value[0]) else -1
            if {k: v for k, v in args.items() if k != "offset"} != expected or offset < 0:
                rows = None
        if rows is None or offset not in offsets(rows):
            self.event(event="invalid", route="search" if search else "library")
            return handler.error(400)
        self.event(event="request", route=key, offset=offset)
        if offset == PAGE and key in self.gates:
            # The true timeout never sends a status line or response bytes;
            # repeated attempts all hang until the private app has exited.
            opened = self.gates[key].wait(100 if key == "stuck" else 20)
            if key == "stuck":
                self.event(event="no-response", route=key, offset=offset)
                handler.close_connection = True
                return
            if not opened:
                self.event(event="invalid", route=f"{key} response hold exceeded test deadline")
                return handler.error(500)
        if key == "permanent" and offset == PAGE:
            self.event(event="response", route=key, offset=offset, status=410)
            return handler.reply({"error": "Synthetic search snapshot expired"}, 410)
        items = rows[offset:offset + PAGE]
        if search:
            result = dict(items=items, resultCount=len(rows), expiresAt=EXPIRY)
            if offset + PAGE < len(rows):
                result["nextCursor"] = f"fixture-{key}-{offset + PAGE}"
        else:
            result = dict(items=items, total=len(rows), revision=base.REVISION, offset=offset, limit=PAGE)
        handler.reply(result)
        self.event(event="response", route=key, offset=offset, status=200, rows=len(items))
        if key == "race" and offset == PAGE:
            self.race_replied.set()

    @contextmanager
    def serve(self):
        fixture = self

        # The base fixture defines its server locally; substitute its parent
        # without patching the process-wide stdlib http.server module.
        class CatalogServer(http.server.ThreadingHTTPServer):
            def __init__(self, address, handler):
                class Handler(handler):
                    def do_GET(self):
                        route = urlsplit(self.path)
                        args = parse_qs(route.query, keep_blank_values=True)
                        paged = (base.ARTIST["id"] in args.get("artistId", []) or
                                 base.ALBUM["id"] in args.get("albumId", []))
                        if route.path == "/search" or (route.path == "/api/library" and paged):
                            if self.trusted() and self.owned():
                                fixture.respond(self, route.path, args)
                        else:
                            super().do_GET()
                super().__init__(address, Handler)

        facade = SimpleNamespace(server=SimpleNamespace(BaseHTTPRequestHandler=http.server.BaseHTTPRequestHandler,
                                                        ThreadingHTTPServer=CatalogServer))
        with patch.object(base, "http", facade), base.wire_fixture() as ready:
            try:
                yield ready
                check(not ready["play_requests"](), "catalog acceptance must never start playback")
                check(not self.violations, f"invalid fixture requests: {self.violations}")
            finally:
                for gate in self.gates.values():
                    gate.set()
                with self.lock:
                    totals = Counter(key for key, _ in self.requests)
                    print("WIRE COUNTS " + json.dumps(dict(totals), sort_keys=True), flush=True)


class View:
    def __init__(self, window, output, magick):
        self.window, self.output, self.magick = window, output, magick
        viewport = element_id(window.elements(), "catalog-scroll")
        check(viewport, "missing native catalog viewport")
        self.scroll_handle = viewport["handle"]

    def viewport(self, elements=None):
        viewport = element_id(elements, "catalog-scroll") if elements is not None else None
        if viewport is None:
            viewport = self.window.mcp.value("get_element_properties", {"elementHandle": self.scroll_handle})
        return bounds(viewport)

    def scroll(self, delta=-2200):
        x, y, right, bottom = self.viewport()
        self.window.mcp.call("dispatch_pointer_scroll", {"windowHandle": self.window.handle,
            "position": {"x": (x + right) / 2, "y": (y + bottom) / 2}, "deltaX": 0, "deltaY": delta})

    def row(self, item, *, deep=False, elements=None):
        role = "Button"
        label = ("Play track: " if item["kind"] == "track" else "Open ") + item["name"]
        mcp = self.window.mcp
        if deep:
            # The root tree's 1,000-node budget need not include a late album row.
            # Target the repeated row's action rectangle by its accessibility role.
            result = mcp.value("query_element_descendants", {
                "elementHandle": self.window.properties["rootElementHandle"],
                "queryStack": [{"matchDescendants": True}, {"matchElementAccessibleRole": role}], "findAll": True})
            candidates = (mcp.value("get_element_properties", {"elementHandle": h}) | {"handle": h}
                          for h in reversed(result.get("elementHandles", [])))
        else:
            if elements is None:
                elements = self.window.elements()
            candidates = elements
        _, top, _, bottom = self.viewport(elements)
        for element in candidates:
            if element.get("accessibleRole") == role and element.get("accessibleLabel") == label:
                _, y, _, end = bounds(element)
                if top - 1 <= y < end <= bottom + 1:
                    return element
        return None

    def visible(self, item, **kwargs):
        return wait(f"visible row {item['name']}", lambda: self.row(item, **kwargs), timeout=8)

    def frame(self, name=None):
        # Unlike native.Window.pixels, unchanged frames are valid evidence.
        image = next(part for part in self.window.mcp.call("take_screenshot", {
            "windowHandle": self.window.handle})["content"] if part["type"] == "image")
        check(image.get("mimeType") == "image/png", "expected native PNG")
        data = b64decode(image["data"], validate=True)
        if name:
            with (self.output / (name + ".png")).open("xb") as file:
                file.write(data)
        return data

    def rgb(self, data, box):
        scale = self.window.properties["scaleFactor"]
        x, y, right, bottom = (round(value * scale) for value in box)
        width, height = right - x, bottom - y
        check(width > 0 and height > 0, f"invalid screenshot crop {box}")
        crop = f"{width}x{height}+{x}+{y}"
        rgb = subprocess.run([self.magick, "png:-", "-crop", crop, "+repage", "-alpha", "off",
                              "-depth", "8", "rgb:-"], input=data, capture_output=True,
                             check=True, timeout=8).stdout
        check(len(rgb) == width * height * 3, f"incorrect crop dimensions for {box}")
        return rgb, width, height

    def focused(self, item, data=None):
        row = self.row(item)
        if not row:
            return False
        x, y, right, bottom = bounds(row)
        # MediaLine paints the gold outline beyond its padded content. Probe
        # straight top/bottom edges inside the middle half, away from corners.
        inset = (right - x) / 4
        rgb, width, height = self.rgb(data or self.frame(), (x + inset, y, right - inset, bottom))
        def edge(line):
            start = line * width * 3
            return sum(rgb[i:i + 3] == GOLD for i in range(start, start + width * 3, 3)) >= width * .8
        return any(edge(i) for i in range(3)) and any(edge(height - 1 - i) for i in range(3))

    def dom_items(self, element_id):
        # Target the native node directly instead of relying on the window's
        # root tree budget or on the copy used to present its state.
        mcp = self.window.mcp
        result = mcp.value("find_elements_by_id", {"windowHandle": self.window.handle,
                                                   "elementsId": element_id})
        return [mcp.value("get_element_properties", {"elementHandle": handle}) | {"handle": handle}
                for handle in result.get("elementHandles", [])]

    def accepted_results(self, expected, *, label="Search"):
        lists = self.dom_items("AppWindow::catalog-results")
        check(len(lists) == 1, f"expected one visible catalog results list, got {len(lists)}")
        result = lists[0]
        check((result.get("accessibleRole"), result.get("accessibleLabel"), result.get("accessibleItemCount"))
              == ("List", label, expected), f"unexpected accepted {label} results list: {result}")
        return result

    def visible_terminal_notice(self, last):
        row = self.row(last)
        if not row:
            return None
        # Notice text is a Text node, not a button or the wrapping Rectangle.
        # Require it to be within the catalog viewport, after the last row.
        left, top, right, bottom = self.viewport()
        row_bottom = bounds(row)[3]
        for message in self.dom_items("Notice::message"):
            if message.get("accessibleRole") != "Text" or not self.window.matches(message, re.escape(STOP)):
                continue
            x, y, x2, y2 = bounds(message)
            if left <= x < x2 <= right and max(top, row_bottom) <= y < y2 <= bottom:
                return message
        return None

    def no_pagers(self):
        for element in self.window.elements():
            if element.get("accessibleRole") == "Button":
                label = element.get("accessibleLabel") or ""
                check(not FORBIDDEN.search(label), f"unexpected catalog continuation button: {label}")


def query(window, value):
    window.shortcut(CONTROL, "f")
    window.shortcut(CONTROL, "a")
    window.key(value)


def next_request(view, wire, route, offset):
    def wheel_until_requested():
        if offset in wire.seen(route):
            return True
        view.scroll()
        return False
    wait(f"wheel on {route} requests {offset}", wheel_until_requested, timeout=8)


def traverse(view, wire, route, rows, *, already=0, checkpoint=None):
    for offset in offsets(rows)[1:]:
        if offset <= already:
            continue
        next_request(view, wire, route, offset)
        if checkpoint is not None and offset == checkpoint:
            print(f"PASS {route} requested offset {offset} by wheel", flush=True)
    view.scroll()
    last = rows[-1]
    def reach_last():
        view.scroll()
        return view.row(last, deep=len(rows) > 100)
    wait(f"last {route} row actually visible: {last['name']}", reach_last, timeout=12)
    view.frame(f"{route}-last-row")
    view.no_pagers()
    view.scroll()
    view.scroll()
    sleep(.4)
    wire.expect(route, offsets(rows))
    print(f"PASS {route}: wheel reached {len(rows)} rows, {len(offsets(rows))} exact requests, then stopped", flush=True)


def results_height(window):
    layout = element_id(window.elements(), "catalog-results")
    check(layout, "missing catalog results layout")
    return layout.get("size", {}).get("height", 0)


def appended(window, old_height):
    # An HTTP receipt alone doesn't prove native layout has accepted new rows.
    wait("appended rows laid out in the native viewport", lambda: results_height(window) > old_height + 100)


def pointer_journey(view, wire):
    window = view.window
    rows = QUERIES["wheel"]
    query(window, "wheel")
    view.visible(rows[0])
    wire.expect("wheel", [0])
    next_request(view, wire, "wheel", PAGE)
    # Wheel reached the demand zone. Hold at least one fully visible old row
    # near the tail and verify its exact pixels did not move when rows arrived.
    elements = window.elements()
    for anchor_item in reversed(rows[:PAGE]):
        anchor = view.row(anchor_item, elements=elements)
        if anchor:
            break
    check(anchor, "no fully visible pre-append Search anchor")
    old_height = results_height(window)
    old_bounds = bounds(anchor)
    before = view.frame("wheel-held-anchor")
    sleep(.35)
    wire.expect("wheel", [0, PAGE])
    view.no_pagers()
    wire.release("wheel")
    appended(window, old_height)
    new_bounds = bounds(view.visible(anchor_item))
    check(max(abs(a - b) for a, b in zip(old_bounds, new_bounds)) <= .1,
          f"append moved {anchor_item['name']}: {old_bounds} → {new_bounds}")
    after = view.frame("wheel-appended-anchor")
    x, y, right, bottom = old_bounds
    crop = (x + 3, y + 2, right - 3, bottom - 2)
    check(view.rgb(before, crop) == view.rgb(after, crop), "native anchor pixels changed during append")
    print(f"PASS delayed append kept {anchor_item['name']} at y={y:g}, identical row pixels", flush=True)
    traverse(view, wire, "wheel", rows, already=PAGE)
    view.scroll(100000)
    window.click_element(view.visible(base.ARTIST))
    view.visible(RELEASES[0])
    wire.expect("artist", [0])
    view.frame("artist-first-page")
    traverse(view, wire, "artist", RELEASES)
    view.scroll(100000)
    window.click_element(view.visible(base.ALBUM))
    view.visible(TRACKS[0])
    wire.expect("album", [0])
    view.frame("album-first-page")
    traverse(view, wire, "album", TRACKS, checkpoint=500)


def tab_to(view, item, limit):
    for _ in range(limit):
        if view.focused(item):
            return
        view.window.key("\t")
    check(view.focused(item), f"Tab did not visibly focus {item['name']}")


def keyboard_journey(view, wire):
    window = view.window
    rows = QUERIES["keyboard"]
    query(window, "keyboard")
    view.visible(rows[0])
    wire.expect("keyboard", [0])
    # No pointer or accessibility activation: Tab traversal itself reveals
    # the boundary and issues the continuation request.
    tab_to(view, rows[PAGE - 1], 120)
    wait("keyboard focus revealed continuation boundary", lambda: wire.seen("keyboard") == [0, PAGE])
    old_height = results_height(window)
    view.frame("keyboard-held-focus")
    wire.release("keyboard")
    appended(window, old_height)
    check(view.focused(rows[PAGE - 1], view.frame("keyboard-appended-focus")), "append lost keyboard focus")
    tab_to(view, base.GUEST, 8)
    focused = view.visible(base.GUEST)
    view.frame("keyboard-next-page-focus")
    window.key(RETURN)
    window.title(base.GUEST_NAME)
    view.frame("keyboard-activated-artist")
    window.shortcut(ALT, LEFT)
    wait("Back restored the actual row focus", lambda: view.focused(base.GUEST))
    returned = view.visible(base.GUEST)
    check(abs(bounds(returned)[1] - bounds(focused)[1]) <= .1,
          f"Back moved the focused row: {bounds(focused)} → {bounds(returned)}")
    view.frame("keyboard-back-restored")
    window.key(RETURN)
    window.title(base.GUEST_NAME)
    wire.expect("keyboard", [0, PAGE])
    print("PASS keyboard-only boundary demand, focus across append, Enter, Back at same y, and Enter again", flush=True)


def stale_journey(view, wire):
    window = view.window
    query(window, "race")
    view.visible(QUERIES["race"][0])
    next_request(view, wire, "race", PAGE)
    view.frame("race-held-old-search")
    query(window, "intermediate")
    query(window, "final")
    final_item = QUERIES["final"][0]
    final_row = view.visible(final_item)
    view.accepted_results(1)
    window.click_element(final_row)
    window.title(final_item["name"])
    view.frame("race-navigated-before-release")
    wire.release("race")
    check(wire.race_replied.wait(5), "fixture did not attempt to send the stale response")
    sleep(.4)
    window.title(final_item["name"])
    view.frame("race-navigated-after-release")
    window.shortcut(ALT, LEFT)
    final_row = view.visible(final_item)
    result = view.accepted_results(len(QUERIES["final"]))
    row_actions = [element for element in window.subtree(result)
                   if element.get("accessibleRole") == "Button"
                   and (element.get("accessibleLabel") or "").startswith("Open ")]
    check([element.get("handle") for element in row_actions] == [final_row["handle"]],
          f"Back did not restore precisely the final result: {row_actions}")
    window.absent(r"Open Continuity (race|intermediate) artist .*", roles=("Button",))
    view.frame("race-back-current-search")
    wire.expect("race", [0, PAGE])
    wire.expect("final", [0])
    check(wire.seen("intermediate") in ([], [0]), "unexpected intermediate search traffic")
    print("PASS held response cannot overwrite a rapid edit, navigation, or its Back target", flush=True)


def stops(view, wire, route, *, attempts, timeout):
    rows = QUERIES[route]
    query(view.window, route)
    view.visible(rows[0])
    next_request(view, wire, route, PAGE)
    view.frame(f"{route}-requested")
    last = rows[PAGE - 1]
    def stopped():
        view.scroll()
        return view.visible_terminal_notice(last)
    wait(f"{route} shows a terminal notice below the accepted rows", stopped, timeout=timeout)
    view.frame(f"{route}-stopped")
    view.no_pagers()
    view.accepted_results(PAGE)
    view.visible(last)
    view.window.absent(re.escape("Open " + rows[PAGE]["name"]), roles=("Button",))
    start = monotonic()
    while monotonic() - start < 2:
        view.scroll(460)
        view.scroll()
        sleep(.25)
    view.visible(last)
    view.accepted_results(PAGE)
    check(view.visible_terminal_notice(last), f"{route} lost its terminal notice after scrolling")
    view.no_pagers()
    wire.expect(route, [0] + [PAGE] * attempts)
    view.frame(f"{route}-stopped-after-wheel")
    print(f"PASS {route}: accepted rows remain, {attempts} continuation attempt(s), no manual or automatic restart", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--desktop", required=True, type=Path, help="prebuilt native desktop with Slint MCP/debug info")
    parser.add_argument("--capture-dir", required=True, type=Path, help="new directory for PNGs and wire.jsonl")
    args = parser.parse_args()
    binary = args.desktop.resolve(strict=True)
    magick = shutil.which("magick") or shutil.which("convert")
    check(magick, "ImageMagick is needed for exact crop and focus pixel checks")
    output = args.capture_dir.resolve()
    local_artifacts = (Path.home() / "repos/spofify-desktop-temporary/catalog-continuity").resolve()
    check(output.is_relative_to(Path("/tmp").resolve()) or output.is_relative_to(local_artifacts),
          "use a new capture directory under /tmp or ~/repos/spofify-desktop-temporary/catalog-continuity")
    output.mkdir(mode=0o700)
    wire = CatalogWire(output)
    with TemporaryDirectory(prefix="spofify-catalog-", dir="/tmp") as name:
        root = Path(name)
        private_binary = root / "spofify-desktop"
        shutil.copy2(binary, private_binary)
        private_binary.chmod(0o700)
        try:
            with wire.serve() as ready, desktop(private_binary, root, private_env(root), multisize=True) as window:
                resize(window, (1200, 740))
                window.find("Sign in")
                window.fill("Server( origin| URL| address)?", ready["origin"])
                window.fill("Username", ready["username"])
                window.fill("Password", ready["password"])
                window.click("Sign in")
                window.section("History")
                view = View(window, output, magick)
                pointer_journey(view, wire)
                keyboard_journey(view, wire)
                stale_journey(view, wire)
                stops(view, wire, "permanent", attempts=1, timeout=8)
                stops(view, wire, "stuck", attempts=5, timeout=95)
        finally:
            log = root / "desktop.log"
            if log.exists():
                shutil.copyfile(log, output / "desktop.log")
    print(f"PASS synthetic native catalog continuation; evidence: {output}", flush=True)


def interrupted(*_):
    raise KeyboardInterrupt


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main()
    except (Failure, OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        sys.exit(f"FAIL: {error or type(error).__name__}")
