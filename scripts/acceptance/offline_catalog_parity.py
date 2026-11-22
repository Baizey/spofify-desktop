#!/usr/bin/env python3
"""Rendered offline catalog parity on one private scale-1 Slint display.

Build: SLINT_EMIT_DEBUG_INFO=1 cargo build --offline --locked --features slint/mcp
Run: python3 -B scripts/acceptance/offline_catalog_parity.py --desktop <debug binary> \
     --capture-dir /tmp/offline_catalog_parity-unique [--pcm]

Requires Cage/Wayland, wlr-randr, ImageMagick and worker-local loopback TCP.
For exact loopback port grants, pass --wire-port 39187 --mcp-port 39188.
In a FUSE-mediated worker, pass --scratch-dir /dev/shm for the private profile
and download store. --pcm also starts a disposable PipeWire graph.
The stock desktop tests in-place transition on Settings, where its mode
button lives. To test a currently focused playlist and Search too, build
`cargo build --offline --locked --features slint/mcp --example offline_catalog_parity`
(with SLINT_EMIT_DEBUG_INFO=1), pass that executable as --desktop, and add
--in-place-fixture. It uses the production UI and Desk/engine page action.
"""
import argparse
from collections import Counter
from contextlib import nullcontext
from dataclasses import dataclass
from hashlib import sha256
import json
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
from tempfile import TemporaryDirectory
from time import sleep

sys.dont_write_bytecode = True
from audio_smoke import heard_tone, output as private_pcm
from catalog_continuity import View, query
from fixture import Failure, check, private_env
from native import (ALT, DOWN, END, ESCAPE, LEFT, RETURN, bounds, contains, desktop,
                    element_id, element_type, player_bar, verify, wait)
from offline_catalog_parity_wire import ALBUM, COUNT, FREQUENCIES, MANUAL, ORDER, RULES, TITLES, Wire
from settings import resize

MODE = (1280, 840)
ROW_TITLE = TITLES[1]
SAVED = "Saved offline"
SONG = re.compile(r"Song (\d+)(?:, (?:Playing|Current track))?")
FORBIDDEN = r"Load more|Show more|Next page|Previous page|First page|Last page|Retry|Try again"


def center(box):
    x, y, r, b = box
    return ((x + r) / 2, (y + b) / 2)


def delta(a, b):
    return max(map(lambda xy: abs(xy[0]-xy[1]), zip(a, b, strict=True)))


def name_filter(label):
    return re.escape(label)


@dataclass
class Frame:
    png: bytes
    geometry: dict


class StaleNativeElement(RuntimeError):
    pass


class Review(View):
    def __init__(self, window, output, magick, wire):
        # Reuse View's raster and wheel helpers, not its startup viewport handle.
        self.window, self.output, self.magick = window, output, magick
        self.wire = wire
        self.log = output / "geometry.jsonl"
        self.failures = []

    def event(self, **fields):
        with self.log.open("a") as log:
            print(json.dumps(fields, sort_keys=True), file=log)

    def park(self):
        self.window.mcp.call("move_pointer", {"windowHandle": self.window.handle, "position": {"x": 5, "y": 5}})

    def find(self, pattern, *, roles=("Button",), timeout=4):
        return self.window.find(pattern, roles=roles, timeout=timeout)

    def native_call(self, name, arguments):
        reply = self.window.mcp.rpc("tools/call", {"name": name, "arguments": arguments})
        if reply.get("isError"):
            messages = [part.get("text", "") for part in reply.get("content", []) if part.get("type") == "text"]
            if any(message.startswith("Error: Invalid element handle") for message in messages):
                raise StaleNativeElement(name)
            raise Failure(f"native {name} failed: {messages}")
        return json.loads(reply["content"][0]["text"])

    def stale_only(self, label, operation):
        for attempt in range(3):
            try:
                return operation()
            except StaleNativeElement:
                self.event(recover_stale_handle=label, attempt=attempt + 1)
                if attempt < 2:
                    sleep(.04)
        raise Failure(f"{label}: native element was replaced in three consecutive lookups")

    def visible_nodes(self):
        def read_current_tree():
            root = self.window.window_properties()["rootElementHandle"]
            return self.native_call("get_element_tree", {"elementHandle": root, "maxElements": 1000})["elements"]
        return self.stale_only("current catalog tree", read_current_tree)

    def scroll_node(self):
        def read_current_scroll():
            viewport = element_id(self.window.elements(), "catalog-scroll")
            check(viewport is not None, "missing current native catalog viewport")
            handle = viewport["handle"]
            return self.native_call("get_element_properties", {"elementHandle": handle}) | {"handle": handle}
        return self.stale_only("current catalog viewport", read_current_scroll)

    def viewport(self, elements=None):
        # Inherited scroll() calls us after every navigation/layout change.
        viewport = element_id(elements, "catalog-scroll") if elements is not None else None
        return bounds(viewport if viewport is not None else self.scroll_node())

    def visible_songs(self):
        # Take occurrence faces and viewport bounds from the *same* tree.
        elements = self.visible_nodes()
        viewport = element_id(elements, "catalog-scroll")
        if viewport is None:
            return {}
        lines = [e for e in elements if element_type([e], "MediaLine") and contains(viewport, e)]
        result = {}
        for face in elements:
            match = SONG.fullmatch(face.get("accessibleDescription", ""))
            label = face.get("accessibleLabel", "")
            if not match or not (face.get("accessibleRole") == "Text" or label.startswith("Play track: ")):
                continue
            line = next((e for e in lines if contains(e, face)), None)
            if line is not None:
                number = int(match[1])
                check(number not in result, f"duplicate native occurrence face Song {number}")
                result[number] = (line, face)
        return result

    def song(self, number, timeout=4):
        return wait(f"fully visible Song {number}", lambda: self.visible_songs().get(number), timeout)

    def near(self, first, last):
        for _ in range(18):
            rows = self.visible_songs()
            if first in rows and last in rows:
                return rows
            indexes = sorted(rows)
            check(len(indexes) >= 2, f"not enough visible occurrences while seeking {first}: {indexes}")
            a, b = indexes[:2]
            stride = (bounds(rows[b][0])[1] - bounds(rows[a][0])[1]) / (b-a)
            self.scroll(max(-5800, min(5800, (a - max(1, first-1)) * stride)))
            sleep(.10)
        raise Failure(f"wheel did not continuously reach Songs {first}–{last}")

    def clicked(self, label, resolve):
        def press():
            element = resolve()
            check(element is not None, f"missing native input target: {label}")
            self.window.mcp.call("start_event_recording", {})
            try:
                self.native_call("click_element", {"elementHandle": element["handle"], "action": "SingleClick"})
            finally:
                events = self.window.mcp.value("stop_event_recording", {})
            check(not events.get("droppedCount") and not events.get("unknownEventCount"), "incomplete native pointer log")
            positions = [event["event"]["pointerPressed"]["position"] for event in events.get("events", [])
                         if "pointerPressed" in event["event"] and event["windowHandle"] == self.window.handle]
            check(len(positions) == 1, f"expected one native pointer press for {label}")
            x, y = positions[0]["x"], positions[0]["y"]
            l, t, r, b = bounds(element)
            check(l <= x < r and t <= y < b, f"pointer {(x, y)} missed {label}: {(l, t, r, b)}")
            self.event(press=label, xy=[x, y], bounds=[l, t, r, b], target=element.get("accessibleLabel"))
        self.stale_only(label, press)

    def song_face(self, number, title=None):
        _, face = self.song(number)
        if title is not None:
            check(face.get("accessibleRole") == "Button" and face.get("accessibleLabel") == "Play track: " + title
                  and face.get("accessibleEnabled", True), f"Song {number} is not an actionable {title} face")
        return face

    def header_button(self, label):
        header = element_type(self.window.elements(), "PlaylistDetailHeader")
        check(header, "missing ordinary playlist detail header")
        children = self.native_call("get_element_tree", {"elementHandle": header["handle"], "maxElements": 500})["elements"]
        return next((e for e in children if e.get("accessibleRole") == "Button" and e.get("accessibleEnabled", True)
                     and self.window.matches(e, label)), None)

    def open_detail(self, title):
        self.window.click("Open " + name_filter(title))
        self.window.title(title)
        self.find("Back")

    def nav(self, name):
        self.window.click(name)
        self.window.section(name)

    def pin(self, title):
        self.open_detail(title)
        self.find("Save offline")
        self.window.click("Save offline")
        def committed():
            return next((e for e in self.window.elements() if self.window.matches(e, "Remove offline save") and
                         e.get("accessibleChecked") and e.get("accessibleDescription") == "Saved on this device"), None)
        wait(f"committed offline audio for {title}", committed, 28)
        self.window.click("Back")
        self.find("Open " + name_filter(title))

    def geometry(self, title, rows, labels):
        self.park()
        vp = self.scroll_node()
        geometry = {"viewport": bounds(vp)}
        for key, element in [("title", self.window.title(title)), ("back", element_id(self.window.elements(), "back-button"))]:
            if element is not None:
                geometry[key] = bounds(element)
        # Use the native header's subtree so Play in the dock or any row
        # cannot accidentally serve as the collection Play witness.
        header = element_type(self.window.elements(), "PlaylistDetailHeader")
        if header:
            geometry["header"] = bounds(header)
            children = self.window.subtree(header, max_elements=500)
            for label in labels:
                element = next((e for e in children if self.window.matches(e, label) and
                                e.get("accessibleRole") == "Button"), None)
                if element is not None:
                    geometry["control:" + label] = bounds(element)
                    glyph = element_type(self.window.subtree(element), "Glyph")
                    if glyph and contains(element, glyph):
                        geometry["glyph:" + label] = bounds(glyph)
        for key, row in rows.items():
            children = self.window.subtree(row)
            geometry["row:" + key] = bounds(row)
            words = element_id(children, "words")
            if words:
                # Metadata may lose its interactive affordance or change word
                # separators without moving the baseline of a surviving credit.
                x, y, r, b = bounds(words)
                geometry["ink:" + key] = (x + 4, y, min(r, x + 190), min(b, y + 21))
                for label, child in (("artist", element_id(children, "artist-line")),
                                     ("track-title", element_type(children, "TrackTitle"))):
                    if child and contains(row, child):
                        l, t, r, b = bounds(child)
                        geometry[f"{label}:{key}"] = (l, t, min(r, l + 140), b)
        return geometry

    def sample(self, tag, title, rows, labels=()):
        geometry = self.geometry(title, rows, labels)
        png = self.frame(tag)
        self.event(frame=tag, sha256=sha256(png).hexdigest(), boxes=geometry)
        return Frame(png, geometry)

    def ink(self, frame, box):
        rgb, width, height = self.rgb(frame.png, box)
        points = [(i % width, i // width) for i in range(width * height)
                  if max(rgb[3*i:3*i+3]) > 158 and sum(rgb[3*i:3*i+3]) > 345]
        if not points:
            return None
        xs, ys = zip(*points)
        x, y = map(round, box[:2])
        return (x + min(xs), y + min(ys), x + max(xs), y + max(ys))

    def optical_center(self, frame, box):
        rgb, width, height = self.rgb(frame.png, box)
        pixels = [rgb[i:i+3] for i in range(0, len(rgb), 3)]
        background = Counter(pixels).most_common(1)[0][0]
        marks = [(i % width, i // width) for i, color in enumerate(pixels)
                 if sum(abs(a-b) for a, b in zip(color, background)) >= 150]
        if not marks:
            return None
        x, y = zip(*marks)
        return (round(box[0]) + (min(x)+max(x)+1)/2, round(box[1]) + (min(y)+max(y)+1)/2)

    def compare(self, label, online, offline):
        # Record all drift before failing; management buttons may be removed
        # offline, but shared text and controls should still read as the same UI.
        common = online.geometry.keys() & offline.geometry.keys()
        comparable_ink = {k for k in common if k == "title" or k.startswith("ink:")}
        metrics = {}
        for k in sorted(common):
            a, b = online.geometry[k], offline.geometry[k]
            if a[2] <= a[0] or a[3] <= a[1] or b[2] <= b[0] or b[3] <= b[1]:
                continue
            if k in comparable_ink:
                a, b = self.ink(online, a), self.ink(offline, b)
                if a and b:
                    metrics[k + ":ink"] = delta(a, b)
            elif k.startswith(("artist:", "track-title:")):
                first, second = self.ink(online, a), self.ink(offline, b)
                if first and second:
                    metrics[k + ":baseline"] = abs(first[3] - second[3])
            elif k.startswith(("control:", "glyph:")):
                metrics[k + ":center-x"] = abs(center(a)[0] - center(b)[0])
                metrics[k + ":center-y"] = abs(center(a)[1] - center(b)[1])
                if k.startswith("glyph:"):
                    first, second = self.optical_center(online, a), self.optical_center(offline, b)
                    if first and second:
                        metrics[k + ":optical-offset"] = delta(
                            (first[0]-center(a)[0], first[1]-center(a)[1]),
                            (second[0]-center(b)[0], second[1]-center(b)[1]))
            elif k.startswith("row:") or k in ("back", "header", "viewport"):
                metrics[k + ":edges"] = delta((a[0], a[1], a[2]), (b[0], b[1], b[2]))
        self.event(comparison=label, measured=metrics, removed=sorted(online.geometry.keys()-common),
                   added=sorted(offline.geometry.keys()-common))
        check(metrics, f"{label}: no common visible geometry")
        drifts = {key: gap for key, gap in metrics.items() if gap > 1}
        check(not drifts, f"{label}: more than 1px shared-geometry or rendered ink drift {drifts}")

    def list_page(self, tag):
        self.nav("Playlists")
        lines = {}
        try:
            for label in (MANUAL, RULES):
                face = self.find("Open " + name_filter(label))
                line = next((e for e in self.window.elements() if element_type([e], "MediaLine") and contains(e, face)), None)
                check(line, f"{label}: missing ordinary playlist row")
                lines[label] = line
        except Failure:
            self.frame(tag + "-playlists-missing-rows")
            raise
        self.event(listing=tag, rows={name: bounds(line) for name, line in lines.items()})
        return self.sample(tag + "-playlists", "Playlists", lines)

    def detail_top(self, tag, title):
        try:
            self.open_detail(title)
            wait("first five rows in native detail", lambda: all(n in self.visible_songs() for n in (1, 5)), 4)
            rows = self.near(1, 5)
        except Failure:
            self.frame(tag + "-unavailable")
            raise
        # Use occurrence identity plus title, not just on-screen numbering:
        # a genuine local subset may display a different fifth track.
        points = {f"{n}:{rows[n][1].get('accessibleLabel', '').removeprefix('Play track: ')}":
                  rows[n][0] for n in range(1, 6)}
        return self.sample(tag + "-top", title, points, ("Play", "Add to queue", "Remove offline save"))

    def detail(self, tag, title):
        top = self.detail_top(tag, title)
        self.window.absent(FORBIDDEN, roles=("Button",))
        if tag == "offline-rules":
            self.window.absent(r"Play(?: track:)? " + name_filter(TITLES[4]), roles=("Button",))
            check(self.song(5)[1].get("accessibleLabel") != "Play track: " + TITLES[4],
                  "the local detail made the intentionally unsaved coda playable")
        last = COUNT - (tag == "offline-rules")
        rows = self.near(99, 102)
        boxes = [bounds(rows[n][0]) for n in range(99, 103)]
        check(all(abs(a[0]-b[0]) <= 1 and 0 <= b[1]-a[3] <= 3 for a, b in zip(boxes, boxes[1:])),
              f"{title}: old 100/101 boundary is not a single continuous list: {boxes}")
        self.park()
        self.frame(tag + "-boundary")
        self.event(boundary=tag, songs=list(range(99, 103)), boxes=boxes)
        self.clicked(f"{tag} focus Song 100", lambda: self.song_face(100))
        self.window.key(DOWN)
        with verify(self.failures, f"{tag} keyboard crossing visible focus"):
            wait("focus paint on Song 101", lambda: self.focused_song(101), 3)
            self.frame(tag + "-focus")
        self.window.key(END)
        with verify(self.failures, f"{tag} keyboard End reaches the tail"):
            self.song(last, timeout=3)
            check(self.focused_song(last), f"{tag}: End did not visibly focus the final occurrence")
            self.frame(tag + "-end")
        self.window.shortcut(ALT, LEFT)
        self.find("Open " + name_filter(title))
        self.window.absent("Back", roles=("Button",))
        return top

    def focused_song(self, number):
        context = self.visible_songs().get(number)
        if context is None:
            return False
        x, y, r, b = bounds(context[0])
        inset = (r - x) / 4
        rgb, width, height = self.rgb(self.frame(), (x + inset, y, r - inset, b))
        def gold(line):
            return sum(rgb[i:i+3] == bytes.fromhex("dfbd78")
                       for i in range(line * width * 3, (line+1) * width * 3, 3)) > width * .8
        return any(gold(i) for i in range(3)) and any(gold(height-1-i) for i in range(3))

    def sample_search(self, tag):
        self.window.title("Search")
        face = self.find("Play track: " + ROW_TITLE)
        search = self.find(r"Search.*", roles=("TextInput",))
        check(search.get("accessibleValue") == ROW_TITLE, f"{tag}: Search query lost or rewritten")
        line = next((e for e in self.window.elements() if element_type([e], "MediaLine") and contains(e, face)), None)
        check(line, f"{tag}: search result missing an ordinary catalog row")
        self.event(search=tag, input_name=search.get("accessibleLabel"), query=ROW_TITLE)
        return self.sample(tag + "-search", "Search", {ROW_TITLE: line}), search.get("accessibleLabel")

    def search(self, tag):
        self.nav("Playlists")
        query(self.window, ROW_TITLE)
        result = self.sample_search(tag)
        self.window.shortcut(ALT, LEFT)
        self.window.section("Playlists")
        return result

    def album_from_search(self, *, offline):
        self.nav("Playlists")
        query(self.window, ROW_TITLE)
        self.window.title("Search")
        self.clicked("recording title opens its album", lambda: self.find("Open album for " + name_filter(ROW_TITLE)))
        self.window.title(name_filter(ALBUM["name"]))
        for name in TITLES[:-1]:
            self.find("Play track: " + name_filter(name))
        unsaved_play = "Play track: " + name_filter(TITLES[-1])
        if offline:
            self.window.absent(unsaved_play, roles=("Button",))
            self.frame("offline-title-opens-partial-album")
        else:
            self.find(unsaved_play)
        self.window.click("Back")
        self.sample_search("title-back-offline" if offline else "title-back-online")
        self.window.shortcut(ALT, LEFT)
        self.window.section("Playlists")

    def fixture_offline_intent(self, root):
        path = root / "parity-mode-intent"
        pending = path.with_suffix(".next")
        self.wire.record("fixture-offline-intent")
        pending.write_text("work offline\n")
        pending.rename(path)
        wait("isolated fixture consumed its offline action", lambda: not path.exists(), 2)
        self.wire.change("offline")
        wait("account socket retired", lambda: len(self.wire.matches("socket-open")) == len(self.wire.matches("socket-close")))

    def in_place(self, root, title=None):
        tag = "in-place-manual" if title else "in-place-search"
        self.nav("Playlists")
        if title:
            self.open_detail(title)
            self.near(99, 102)
            self.clicked("focus Song 100 before mode change", lambda: self.song_face(100))
            self.window.key(DOWN)
            self.park()
            wait("focused Song 101 before mode change", lambda: self.focused_song(101), 3)
            before = bounds(self.song(101)[0])
            self.frame(tag + "-before")
        else:
            query(self.window, ROW_TITLE)
            before, label = self.sample_search(tag + "-before")
        self.fixture_offline_intent(root)
        self.frame(tag + "-immediately-after")
        if title:
            # No navigation, new query, or user input intervenes between evidence.
            check(delta(bounds(self.song(101, timeout=3)[0]), before) <= 1,
                  "mode change moved the focused playlist occurrence")
            check(self.focused_song(101), "mode change lost playlist keyboard focus")
            self.window.key(DOWN)
            check(self.focused_song(102), "keyboard traversal stopped after the in-place mode change")
            self.frame(tag + "-keyboard-after")
        else:
            after, after_label = self.sample_search(tag + "-after")
            check(label == after_label, "mode change changed Search input identity")
            self.compare(tag, before, after)
        self.window.shortcut(ALT, LEFT)
        self.window.section("Playlists")
        self.window.absent("Back", roles=("Button",))

    def go_offline(self, tag):
        self.nav("Settings")
        self.frame(tag + "-settings-before")
        self.window.click("Work offline")
        self.wire.change("offline")
        wait("online socket retired", lambda: len(self.wire.matches("socket-open")) == len(self.wire.matches("socket-close")))
        self.frame(tag + "-entered-offline")
        with verify(self.failures, f"{tag} offline does not redirect the current Settings route"):
            self.window.section("Settings")
            self.find("Go online")

    def reconnect(self, tag, *, success):
        self.nav("Settings")
        self.find("Go online")
        count = len(self.wire.matches("probe-held"))
        self.wire.begin_probe()
        try:
            self.window.click("Go online")
            wait("the explicit reconnect validation GET is held", lambda: len(self.wire.matches("probe-held")) > count, 2.5)
            self.find("Stay offline", timeout=1)
            self.frame(tag + "-held-validation")
            if not success:
                self.window.click("Stay offline")
        finally:
            self.wire.finish_probe(success)
        self.find("Work offline" if success else "Go online")
        self.window.section("Settings")
        if success:
            wait("reconnected authenticated account socket", lambda: len(self.wire.matches("socket-open")) >
                 len(self.wire.matches("socket-close")))
        self.frame(tag + "-validation-finished")

    def queue_viewport(self):
        handles = self.native_call("find_elements_by_id", {"windowHandle": self.window.handle,
                                   "elementsId": "AppWindow::queue-scroll"}).get("elementHandles", [])
        check(len(handles) <= 1, "ambiguous native queue viewport")
        return self.native_call("get_element_properties", {"elementHandle": handles[0]}) if handles else None

    def queue_rows(self):
        def read_rows():
            scroll = self.queue_viewport()
            pane = element_id(self.window.elements(), "queue-pane")
            if not pane or not scroll or bounds(scroll)[2] <= bounds(scroll)[0]:
                return []
            result = self.native_call("query_element_descendants", {
                "elementHandle": pane["handle"], "findAll": True,
                "queryStack": [{"matchElementTypeNameOrBase": "MediaLine"}],
            })
            identities = []
            for handle in result.get("elementHandles", []):
                tree = self.native_call("get_element_tree", {"elementHandle": handle, "maxElements": 128})
                line, *children = tree["elements"]
                # A row can be partly clipped while the title is readable.
                # Check the title line's actual bounds inside the viewport.
                words = element_id(children, "words")
                if not words:
                    continue
                x, y, r, b = bounds(words)
                _, top, _, bottom = bounds(scroll)
                if not (top <= y and min(b, y + 20) <= bottom):
                    continue
                names = {(e.get("accessibleLabel") or "").removeprefix("Play track: ").removeprefix("Open album for ") for e in children
                         if e.get("accessibleRole") in ("Text", "Button")} & set(TITLES)
                check(len(names) == 1, f"queue row at {bounds(line)} has ambiguous identity: {names}")
                identities.append((names.pop(), bounds(line), (x, y, r, min(b, y + 20))))
            return sorted(identities, key=lambda row: row[1][1])
        return self.stale_only("visible queue rows", read_rows)

    def queue(self, expected):
        observed = []
        def visible_prefix():
            nonlocal observed
            observed = [label for label, _, _ in self.queue_rows()]
            return observed[:len(expected)] == list(expected)
        try:
            wait("visible first occurrences in native queue", visible_prefix, 6)
        finally:
            self.event(queue_expected=list(expected), queue_observed=observed)

    def queue_tail(self, expected):
        def wheel(delta):
            viewport = self.stale_only("queue wheel target", self.queue_viewport)
            check(viewport, "missing live queue viewport for wheel")
            x, y = center(bounds(viewport))
            self.window.mcp.call("dispatch_pointer_scroll", {"windowHandle": self.window.handle,
                                 "position": {"x": x, "y": y}, "deltaX": 0, "deltaY": delta})
        def visible_tail():
            rows = self.queue_rows()[-4:]
            return rows if [row[0] for row in rows] == list(expected) else None
        try:
            wheel(-20000)
            rows = wait("native wheel reveals the four final queue titles", visible_tail, 4)
            check(all(a[2][3] <= b[2][1] for a, b in zip(rows, rows[1:])), "queue tail title bounds overlap")
            wheel(-20000)
            settled = wait("native queue remains at the end", visible_tail, 3)
            check(all(delta(a[1], b[1]) <= 1 for a, b in zip(rows, settled)), "extra wheel moved the queue tail")
            self.event(queue_tail=settled)
            self.frame("offline-header-queue-native-tail")
        finally:
            wheel(20000)

    def playback(self, title, pcm):
        self.open_detail(title)
        self.window.absent("Open Now Playing", roles=("Button",))
        self.clicked("header Add to queue", lambda: self.header_button("Add to queue"))
        self.window.click("Play queue")
        with verify(self.failures, "idle native queue shows ordered duplicate prefix and wheel reaches tail"):
            self.queue([TITLES[i] for i in ORDER[:5]])
            self.frame("offline-header-queue-ordered-duplicates")
            self.queue_tail([TITLES[i] for i in ORDER[-4:]])
        self.window.absent("Open Now Playing", roles=("Button",))
        self.window.click("Close play queue")
        # Activate the second matching Refrain with the ordinary row layout,
        # by keyboard after using a normal press to focus its face.
        self.clicked("focus second Refrain", lambda: self.song_face(4, ROW_TITLE))
        baseline = pcm.stat().st_size if pcm is not None else None
        self.window.key(RETURN)
        dock = player_bar(self.window)
        self.window.find("Open Now Playing", roles=("Button",), within=dock)
        self.window.find("Open album for " + name_filter(ROW_TITLE), roles=("Button",), within=dock)
        if pcm is not None:
            with verify(self.failures, "local Refrain decoder reaches the private monitor"):
                hz = FREQUENCIES[ROW_TITLE]
                wait("offline PCM matches the 330 Hz second Refrain", lambda: heard_tone(pcm, hz=hz, start=baseline), 6)
                self.event(pcm_witness_hz=hz, sample_bytes_from=baseline, sample_bytes_through=pcm.stat().st_size)
        self.window.click("Play queue")
        self.queue([ROW_TITLE, TITLES[3], TITLES[2]])
        self.frame("offline-row-play-second-duplicate")
        self.window.click("Close play queue")
        self.scroll(20000)
        self.window.title(title)
        self.clicked("header Play", lambda: self.header_button("Play"))
        self.window.find("Open album for " + name_filter(TITLES[0]), roles=("Button",), within=dock)
        self.window.click("Play queue")
        self.queue([TITLES[i] for i in ORDER[:5]])
        self.frame("offline-header-play-ordered-duplicates")
        self.window.click("Close play queue")

    def playlist_card_play(self):
        self.nav("Playlists")
        open_label = "Open " + name_filter(RULES)
        # Hover the native face by geometry; the ordinary Play control may
        # appear over the leading artwork instead of occupying a fixed column.
        x, y = center(bounds(self.find(open_label)))
        self.window.mcp.call("move_pointer", {"windowHandle": self.window.handle, "position": {"x": x, "y": y}})
        self.clicked("Play partial Rules from its card", lambda: self.find("Play " + name_filter(RULES)))
        self.find(open_label)  # Play must not open detail.
        self.window.click("Play queue")
        self.queue([TITLES[i] for i in [*ORDER[:4], ORDER[5]]])
        self.frame("offline-playlists-card-play")
        self.window.click("Close play queue")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--desktop", required=True, type=Path)
    parser.add_argument("--capture-dir", required=True, type=Path)
    parser.add_argument("--pcm", action="store_true", help="also witness actual local decoder output on an owned monitor")
    parser.add_argument("--wire-port", type=int, default=0, help="owned fixture port (default: ephemeral)")
    parser.add_argument("--mcp-port", type=int, default=0, help="owned Slint MCP port (default: ephemeral)")
    parser.add_argument("--in-place-fixture", action="store_true", help="binary is the fixture example; exercise its private mode intent")
    parser.add_argument("--frames-first", action="store_true", help="short run: capture Playlists and partial Rules before deeper input checks")
    parser.add_argument("--scratch-dir", type=Path, default=Path("/tmp"),
                        help="parent for temporary private HOME/XDG/store; use /dev/shm in a FUSE worker")
    args = parser.parse_args()
    scratch = args.scratch_dir.resolve(strict=True)
    check(scratch in (Path("/tmp").resolve(), Path("/dev/shm").resolve()), "choose /tmp or /dev/shm for the isolated profile")
    check(all(0 <= port <= 65535 for port in (args.wire_port, args.mcp_port))
          and (not args.wire_port or args.wire_port != args.mcp_port), "ports must be distinct or ephemeral")
    binary = args.desktop.resolve(strict=True)
    magick = shutil.which("magick") or shutil.which("convert")
    check(magick, "ImageMagick is needed for rendered ink and focus measurements")
    output = args.capture_dir.resolve()
    allowed = Path.home() / "repos/spofify-desktop-temporary"
    check(output.is_relative_to(Path("/tmp")) or output.is_relative_to(allowed.resolve()),
          "use a new capture directory under /tmp or ~/repos/spofify-desktop-temporary")
    check(output.name.startswith("offline_catalog_parity"), "use a task-specific offline_catalog_parity* capture directory")
    output.mkdir(mode=0o700)
    (output / "binary.sha256").write_text(sha256(binary.read_bytes()).hexdigest() + "\n")
    print(f"Native evidence: {output}", flush=True)
    wire = Wire(output, args.wire_port)
    failures = []
    with (TemporaryDirectory(prefix="spofify-offline-catalog-", dir=scratch) as name,
          TemporaryDirectory(prefix="spofify-offline-binary-", dir="/tmp") as binary_dir):
        root = Path(name)
        # /dev/shm can be noexec outside the worker. Keep all private profile
        # directories beneath scratch, but launch the owned /tmp binary copy.
        private_binary = Path(binary_dir) / "spofify-desktop"
        shutil.copy2(binary, private_binary)
        private_binary.chmod(0o700)
        env = private_env(root)
        no_bus = "unix:path=" + env["XDG_RUNTIME_DIR"] + "/no-bus"
        env.update(DBUS_SESSION_BUS_ADDRESS=no_bus, DBUS_SYSTEM_BUS_ADDRESS=no_bus)
        try:
            with (private_pcm(root, env) if args.pcm else nullcontext(None)) as pcm, wire.serve() as ready:
                if args.in_place_fixture:
                    env["SPOFIFY_OFFLINE_PARITY_WIRE_ORIGIN"] = ready["origin"]
                with desktop(private_binary, root, env, multisize=True, mcp_port=args.mcp_port) as window:
                    failures = journey(window, output, magick, wire, ready, args, root, pcm)
        finally:
            log = root / "desktop.log"
            if log.exists():
                shutil.copyfile(log, output / "desktop.log")
    check(not failures, "; ".join(failures))
    print(f"PASS offline catalog rendered and wire parity: {output}", flush=True)


def journey(window, output, magick, wire, ready, args, root, pcm):
    resize(window, MODE)
    check(window.window_properties()["scaleFactor"] == 1, "expected one scale-1 display")
    window.find("Sign in")
    window.fill("Server( origin| URL| address)?", ready["origin"])
    window.fill("Username", ready["username"])
    window.fill("Password", ready["password"])
    window.click("Sign in")
    window.section("History")
    review = Review(window, output, magick, wire)
    failures = review.failures
    # The wide shell may start with its Queue lane open. Keep comparison
    # captures in one layout and make later queue clicks genuine transitions.
    close_queue = next((e for e in window.elements() if window.matches(e, "Close play queue")
                        and e.get("accessibleRole") == "Button" and e.get("accessibleEnabled")), None)
    if close_queue:
        window.click_element(close_queue)
    review.nav("Playlists")
    with verify(failures, f"online save {MANUAL}"):
        review.pin(MANUAL)
    if args.frames_first:
        captures = {}
        for mode in ("online", "offline"):
            if mode == "offline":
                review.go_offline("first")
            with verify(failures, f"{mode} Playlists"):
                captures[mode, "Playlists"] = review.list_page(mode)
            with verify(failures, f"{mode} partial Rules"):
                captures[mode, "partial Rules"] = review.detail_top(mode + "-partial-rules", RULES)
        for label in ("Playlists", "partial Rules"):
            if all((mode, label) in captures for mode in ("online", "offline")):
                with verify(failures, label):
                    review.compare(label, captures["online", label], captures["offline", label])
        return failures
    online = {}
    for tag, run in (("list", lambda: review.list_page("online")),
                     ("manual", lambda: review.detail("online-manual", MANUAL)),
                     ("rules", lambda: review.detail("online-rules", RULES)),
                     ("search", lambda: review.search("online"))):
        with verify(failures, "online " + tag):
            online[tag] = run()
        review.nav("Playlists")
    review.nav(SAVED)
    with verify(failures, "online Saved detail"):
        online["saved"] = review.detail("online-saved", MANUAL)
    with verify(failures, "online Search title opens complete album"):
        review.album_from_search(offline=False)
    review.go_offline("first")
    with verify(failures, "offline Search title opens verified partial album"):
        review.album_from_search(offline=True)
    for tag, run in (("list", lambda: review.list_page("offline")),
                     ("manual", lambda: review.detail("offline-manual", MANUAL)),
                     ("rules", lambda: review.detail("offline-rules", RULES)),
                     ("search", lambda: review.search("offline")),
                     ("saved", lambda: (review.nav(SAVED), review.detail("offline-saved", MANUAL))[1])):
        with verify(failures, "offline " + tag):
            offline = run()
            if tag in online:
                if tag == "search":
                    review.compare(tag, online[tag][0], offline[0])
                    check(online[tag][1] == offline[1], f"mode-specific Search identity {online[tag][1]!r} → {offline[1]!r}")
                else:
                    review.compare(tag, online[tag], offline)
        review.nav(SAVED if tag == "saved" else "Playlists")
    with verify(failures, "ordinary offline Play and ordered duplicate queue"):
        review.nav("Playlists")
        review.playback(MANUAL, pcm)
    with verify(failures, "offline ordinary playlist card Play"):
        review.playlist_card_play()
    with verify(failures, "cancel explicit reconnect"):
        review.reconnect("cancel", success=False)
    with verify(failures, "explicit reconnect succeeds"):
        review.reconnect("return", success=True)
    with verify(failures, "repeat physical mode transition"):
        review.go_offline("repeat")
        review.list_page("repeat-offline")
        review.reconnect("repeat-return", success=True)
        review.list_page("repeat-online")
    if args.in_place_fixture:
        for title in (MANUAL, None):
            with verify(failures, f"in-place mode change of {title or 'Search'}"):
                review.in_place(root, title)
            if wire.phase == "offline":
                with verify(failures, f"reconnect after in-place mode change of {title or 'Search'}"):
                    review.reconnect("in-place-return-" + ("manual" if title else "search"), success=True)
    return failures


def interrupted(*_):
    raise KeyboardInterrupt


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    try:
        main()
    except (Failure, OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        sys.exit(f"FAIL: {error or type(error).__name__}")
