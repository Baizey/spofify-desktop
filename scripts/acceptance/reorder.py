#!/usr/bin/env python3
"""Native queue and playlist reorder review against disposable synthetic wire data.

From the worktree root, after coordinating the inspector build:
  SLINT_EMIT_DEBUG_INFO=1 cargo build --offline --locked --features slint/mcp
  python3 -B scripts/acceptance/reorder.py target/debug/spofify-desktop
  # Or add --delayed-echo to test order continuity across a 1.9 s readback.

Run on a host that permits worker-local IPv4 loopback. The default output is
.tmp/reorder-review/; on reruns use --output reorder-review-2 (must be new).
The runner sets SPOFIFY_INSPECT_REORDER_LINE=1 only for the line-cue runs.
Requires Cage with the wlr virtual-pointer and output-management globals, and
wlr-randr. Use a Wayland-only Cage or an isolated /tmp if the compositor was
built with Xwayland (which can otherwise create system X11 socket files).
Uses a private 1280x720 headless output, null audio sink, synthetic account and
two IPv4 loopback listeners (MCP and account wire). This is not Go or audio evidence.
"""
import argparse
from base64 import b64decode
from contextlib import contextmanager
import json
import os
from pathlib import Path
import re
import shutil
import socket
import struct
import subprocess
import sys
from tempfile import TemporaryDirectory
from threading import Lock
from time import monotonic, sleep
from unittest.mock import patch

sys.dont_write_bytecode = True
from fixture import Failure, check, private_env
from native import DOWN, ESCAPE, WIDE, bounds, contains, desktop, element_id, element_type, wait
import wire_ui


COUNT = 128
CURRENT = 1  # 000 played, 001 current, 002..127 upcoming
PLAYLIST = "Synthetic Reorder Review"
PLAYLIST_ID = "fixture-order"
ROW = re.compile(r"\bFixture row (\d{3})\b")
DUPLICATE = 3  # Both occurrences 003 and 005 have exactly the same metadata.
READBACK_DELAY = .14  # Match the offline native fixture's synthetic readback lag.


def queue_key(n):
    return 1000 + n


def entry_id(n):
    return f"fixture-entry-{n:03d}"


def track(n):
    n = DUPLICATE if n == 5 else n
    return dict(wire_ui.TRACK, id=f"fixture-track-{n:03d}", name=f"Fixture row {n:03d}")


def moved(rows, key, after, *, id_field, fixed):
    keys = [row[id_field] for row in rows]
    check(key in keys[fixed:] and (after is None or after in keys[fixed:]) and key != after,
          "move escaped the movable range")
    result = rows.copy()
    row = result.pop(keys.index(key))
    dest = fixed if after is None else next(i + 1 for i, r in enumerate(result) if r[id_field] == after)
    result.insert(dest, row)
    return result


@contextmanager
def account(output, readback_delay=READBACK_DELAY):
    summary = dict(wire_ui.SUMMARY, id=PLAYLIST_ID, name=PLAYLIST, description="",
                   trackCount=COUNT, availableCount=COUNT)
    entries = [dict(id=entry_id(i), position=i, addedAt=wire_ui.DATE, track=track(i)) for i in range(COUNT)]
    queue = [dict(id=t["id"], name=(t["name"] + " — The Last Train Out of the City Before the First Snow Came"
                                   if i in (DUPLICATE, 5) else t["name"]),
                  artists=t["artists"], albumId=t["albumId"],
                  albumName=t["albumName"], duration=60, key=queue_key(i), order=i + 1, intent="low")
             for i in range(COUNT) for t in (track(i),)]
    cursor = dict(key=queue_key(CURRENT), chain="fixture-paused", visit=1, run=1,
                  elapsed=0, desired=False, playing=False)
    writes = []
    lock = Lock()

    class Account(wire_ui.AccountWire):
        def initial(self):
            self.snapshot["player"].update(queue=queue, cursor=cursor)
            super().initial()

        def message(self):
            while True:
                msg = super().message()
                kind = msg.get("type")
                command = msg.get("command", {})
                if kind == "personal" and command.get("type") == "syncPlaylist":
                    return msg
                if kind in ("presence", "sync", "hello", "release"):
                    return msg
                # Record the exact mutation the desktop emitted, before applying it.
                with lock:
                    check(len(writes) < 32, "mutation cap exceeded")
                    writes.append(msg)
                    with (output / "commands.jsonl").open("a") as log:
                        log.write(json.dumps(dict(monotonic=monotonic(), command=msg)) + "\n")
                if kind == "move":
                    check(set(msg) == {"type", "key", "after"}, "unexpected queue move fields")
                    queue[:] = moved(queue, msg["key"], msg["after"] or None, id_field="key", fixed=CURRENT + 1)
                    sleep(readback_delay)
                    self.note_readback()
                    self.send(dict(type="player", player=dict(source=self.connection,
                                   queue=dict(kind="order", keys=[row["key"] for row in queue]))))
                elif kind == "personal" and command.get("type") == "moveEntry":
                    check(set(command) == {"type", "playlistId", "entryId", "after"}
                          and command["playlistId"] == PLAYLIST_ID, "unexpected playlist move fields")
                    order = moved(entries, command["entryId"], command["after"] or None, id_field="id", fixed=0)
                    entries[:] = [dict(e, position=i) for i, e in enumerate(order)]
                    summary["revision"] += 1
                    sleep(readback_delay)
                    self.note_readback()
                    self.personal(change=dict(playlist=summary,
                                              entries=dict(moved=command["entryId"], after=command["after"])))
                else:
                    raise Failure(f"unexpected synthetic mutation type {kind!r}")

        def note_readback(self):
            with (output / "readback.jsonl").open("a") as log:
                log.write(json.dumps(dict(monotonic=monotonic(), commandCount=len(writes))) + "\n")

    def commands():
        with lock:
            return writes.copy()

    (output / "commands.jsonl").touch(exist_ok=False)
    with patch.object(wire_ui, "AccountWire", Account), patch.object(
            wire_ui, "PLAYLISTS", {PLAYLIST_ID: (summary, entries, None)}), wire_ui.wire_fixture() as ready:
        yield ready, commands


class VirtualPointer:
    """Minimal Wayland client, scoped to the socket of our own Cage instance.

    Slint MCP's drag_element always releases. This device holds the real button
    across screenshots and timers; Slint itself records the resulting events.
    Only the Wayland registry, sync and wlr virtual-pointer v1 are used.
    """
    MI = b"zwlr_virtual_pointer_manager_v1\0"
    BTN_LEFT = 0x110

    def __init__(self, sock, size):
        self.sock, self.size = sock, size
        self.offset = (0, 0)
        self.next_id = 3
        self.globals = {}
        self.send(1, 1, struct.pack("=I", 2))  # wl_display.get_registry
        self.sync()
        check(self.MI in self.globals, "Cage does not advertise wlr virtual pointer; use a prepared host")
        name = self.globals[self.MI]
        manager, self.pointer = self.new_id(), self.new_id()
        interface = self.MI.ljust((len(self.MI) + 3) & ~3, b"\0")
        self.send(2, 0, struct.pack("=II", name, len(self.MI)) + interface + struct.pack("=II", 1, manager))
        self.send(manager, 0, struct.pack("=II", 0, self.pointer))  # manager.create_virtual_pointer (default seat)
        self.sync()

    def new_id(self):
        result = self.next_id
        self.next_id += 1
        return result

    def send(self, sender, opcode, payload=b""):
        self.sock.sendall(struct.pack("=II", sender, (len(payload) + 8) << 16 | opcode) + payload)

    def read(self, count):
        data = bytearray()
        while len(data) < count:
            chunk = self.sock.recv(count - len(data))
            check(chunk, "owned Wayland socket closed")
            data.extend(chunk)
        return data

    def sync(self):
        callback = self.new_id()
        self.send(1, 0, struct.pack("=I", callback))
        for _ in range(128):
            sender, header = struct.unpack("=II", self.read(8))
            opcode, size = header & 0xffff, header >> 16
            check(size >= 8, "invalid Wayland frame")
            body = self.read(size - 8)
            if sender == 1 and opcode == 0:
                detail = bytes(body[12:]).rstrip(b"\0").decode(errors="replace")
                raise Failure(f"owned Wayland protocol error: {detail}")
            if sender == 2 and opcode == 0:
                name, length = struct.unpack_from("=II", body)
                self.globals[bytes(body[8:8 + length])] = name
            if sender == callback:
                return
        raise Failure("too many events waiting for owned Wayland sync")

    def timestamp(self):
        return int(monotonic() * 1000) & 0xffffffff

    def move(self, x, y):
        x, y = round(x + self.offset[0]), round(y + self.offset[1])
        w, h = self.size
        check(0 <= x < w and 0 <= y < h, f"virtual pointer outside the private output: {(x, y)}")
        self.send(self.pointer, 1, struct.pack("=IIIII", self.timestamp(), x, y, w, h))
        self.send(self.pointer, 4)
        self.sync()

    def button(self, pressed):
        self.send(self.pointer, 2, struct.pack("=III", self.timestamp(), self.BTN_LEFT, int(pressed)))
        self.send(self.pointer, 4)
        self.sync()


@contextmanager
def pointer(runtime, window, output):
    sockets = [p for p in runtime.glob("wayland-*") if p.is_socket()]
    check(len(sockets) == 1, "expected only the private Cage Wayland socket")
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(3)
        sock.connect(str(sockets[0]))
        device = VirtualPointer(sock, WIDE)
        # Derive the client-decoration offset using what Slint actually received.
        with recording(window, output, "pointer-calibration") as events:
            device.move(550, 300)
            sleep(.04)
            device.move(570, 320)
            sleep(.07)
        moves = [e["pointerMoved"]["position"] for e in events if "pointerMoved" in e]
        check(moves, "no Wayland motion delivered to the Slint recorder")
        device.offset = (570 - moves[-1]["x"], 320 - moves[-1]["y"])
        check(all(abs(d) <= 64 for d in device.offset), f"unexpected window origin: {device.offset}")
        yield device


@contextmanager
def recording(window, output, name):
    window.mcp.call("start_event_recording", {})
    events = []
    try:
        yield events
    finally:
        log = window.mcp.value("stop_event_recording", {})
        (output / (name + "-events.json")).write_text(json.dumps(log, indent=2) + "\n")
        check(not log.get("droppedCount") and not log.get("unknownEventCount"), f"incomplete event log: {name}")
        events.extend(e["event"] for e in log.get("events", []) if e["windowHandle"] == window.handle)


def center(element):
    x, y, r, b = bounds(element)
    return (x + r) / 2, (y + b) / 2


class Review:
    def __init__(self, window, device, output, commands):
        self.window, self.device, self.output, self.commands = window, device, output, commands
        self.surface = ""
        self.dimensions = tuple(map(round, self.window.dimensions()))

    def frame(self, name, layout=None):
        stamp = monotonic()
        response = self.window.mcp.call("take_screenshot", {"windowHandle": self.window.handle})
        img = next((p for p in response["content"] if p["type"] == "image"), None)
        check(img and img.get("mimeType") == "image/png", "Slint did not return a native PNG")
        png = b64decode(img["data"], validate=True)
        check(png[:8] == b"\x89PNG\r\n\x1a\n" and png[12:16] == b"IHDR", "invalid native PNG")
        dimensions = struct.unpack_from("!II", png, 16)
        check(dimensions == self.dimensions, f"unexpected PNG size: {dimensions}")
        filename = f"{self.surface}-{name}.png"
        (self.output / filename).write_bytes(png)
        with (self.output / "frames.jsonl").open("a") as log:
            log.write(json.dumps(dict(monotonic=stamp, captured=monotonic(),
                                      image=filename, commandCount=len(self.commands()), layout=layout)) + "\n")

    def tree(self, handle):
        result = self.window.mcp.value("get_element_tree", {"elementHandle": handle, "maxElements": 1000})
        check(not result.get("debugInfoMissing"), "build with SLINT_EMIT_DEBUG_INFO=1")
        return result["elements"]

    def handle(self, id_):
        def find():
            found = self.window.mcp.value("find_elements_by_id", {
                "windowHandle": self.window.handle, "elementsId": f"AppWindow::{id_}"})
            handles = found.get("elementHandles", [])
            return handles[0] if len(handles) == 1 else None
        return wait(id_, find)

    def control(self, label):
        # The playlist header and its sticky strip show alternate Edit order
        # buttons. Search the content pane, not the queue's identically named button.
        pane = self.handle("queue-pane" if self.surface == "queue" else "main")
        return wait(f"enabled {self.surface} {label}", lambda: next(
            (e for e in self.tree(pane) if self.window.matches(e, label)
             and e.get("accessibleRole") == "Button" and e.get("visible", True)
             and e.get("accessibleEnabled")), None))

    def open(self, surface):
        self.surface = surface
        if surface == "playlist":
            self.window.click("Playlists")
            self.window.click(f"Open {PLAYLIST}")
        # At 1280 px AppWindow opens the wide queue by default.
        if surface == "queue":
            self.frame("00-normal")
        self.window.click_element(self.control("Edit order"))
        self.editor = self.handle(f"{surface}-order")
        self.frame("01-editor-open", self.layout())
        self.idle_alignment()
        if surface == "playlist":
            # The real playlist has a Back bar absent in the model-only preview.
            # Move one row down before the local drag scenarios so row 007's
            # whole delegate and its center clear the dock and autoscroll edge.
            viewport, _ = self.rows()
            _, top, _, bottom = bounds(self.row(6))
            self.scroll(viewport, top - bottom)
            self.row(7)
            self.frame("01a-drag-viewport", self.layout())

    def layout(self):
        elements = self.tree(self.editor)
        viewport, rows = self.rows(elements)
        return dict(viewport=bounds(viewport), rows=[dict(label=r["accessibleLabel"], box=bounds(r)) for r in rows],
                    children=[dict(types=e.get("typeNamesAndIds"), box=bounds(e), label=e.get("accessibleLabel"))
                              for e in elements if contains(viewport, e)])

    def rows(self, elements=None):
        elements = elements if elements is not None else self.tree(self.editor)
        viewport = element_id(elements, "list")
        check(viewport, "missing reorder viewport")
        rows = [e for e in elements if e.get("accessibleRole") == "ListItem"
                and ROW.search(e.get("accessibleLabel", "")) and contains(viewport, e, slack=0)]
        return viewport, sorted(rows, key=lambda r: center(r)[1])

    def row(self, row, occurrence=0):
        def find():
            candidates = [r for r in self.rows()[1] if number(r) == row]
            return candidates[occurrence] if len(candidates) > occurrence else None
        return wait(f"fully visible {self.surface} row {row:03d} occurrence {occurrence}", find, timeout=3)

    def grab_point(self, row):
        card = next((e for e in self.tree(self.editor) if element_type([e], "ReorderCard")
                     and contains(row, e)), None)
        check(card, f"missing reorder card for {row['accessibleLabel']}")
        # The grip glyph's center is 18 logical px inside the card's right edge.
        return bounds(card)[2] - 18, center(card)[1]

    def idle_alignment(self):
        elements = self.tree(self.editor)
        _, rows = self.rows(elements)
        cards = [e for e in elements if element_type([e], "ReorderCard")
                 and any(contains(r, e) for r in rows)]
        check(len(cards) >= 4, "not enough fully visible reorder cards for alignment evidence")
        lefts, rights = ([bounds(c)[i] for c in cards] for i in (0, 2))
        check(max(lefts) - min(lefts) <= 1 and max(rights) - min(rights) <= 1,
              f"{self.surface} static row edges drift: {lefts!r}, {rights!r}")

    def lifted(self):
        # ReorderList describes the real insertion point along with lift state.
        return [r for r in self.rows()[1] if r.get("accessibleDescription", "").startswith("Lifted,")]

    def expect_adjacent(self, row, after, occurrence=0):
        def adjacent():
            visible = [number(r) for r in self.rows()[1]]
            matches = [i for i, number_ in enumerate(visible) if number_ == row]
            return len(matches) > occurrence and matches[occurrence] > 0 and visible[matches[occurrence] - 1] == after
        wait(f"{self.surface} published row {row:03d} directly after {after:03d}", adjacent, timeout=3)

    def expect(self, before, command=None):
        if command is not None:
            wait("wire mutation", lambda: len(self.commands()) > before, timeout=3)
        sleep(.22)  # Observe late writes after cancel, release, and settle.
        expected = [] if command is None else [command]
        check(self.commands()[before:] == expected,
              f"{self.surface} wire expected {expected!r}, got {self.commands()[before:]!r}")

    def command(self, row, after):
        if self.surface == "queue":
            return dict(type="move", key=queue_key(row), after=queue_key(after) if after is not None else 0)
        return dict(type="personal", command=dict(type="moveEntry", playlistId=PLAYLIST_ID, entryId=entry_id(row),
                                                 after=entry_id(after) if after is not None else ""))

    @contextmanager
    def hold(self, point):
        self.device.move(*point)
        sleep(.025)
        self.device.button(True)
        try:
            yield
        finally:
            self.device.button(False)
            sleep(.035)

    def after(self, row, x):
        # A centered press and fresh row bounds address the same logical slot.
        self.device.move(x, center(row)[1])

    def stationary_no_write(self):
        # The offline fixture asserts the first *synchronous* pressed frame.
        # This pass proves the same hit targets, real stationary events, and
        # absence of any wire mutations both on a quick click and a held release.
        for site, held in (("title", False), ("left-edge", True)):
            start = self.row(DUPLICATE, 1)
            left, top, right, bottom = bounds(start)
            point = ((left + right) / 2, (top + bottom) / 2 - 9) if site == "title" else (
                left + 4, (top + bottom) / 2)
            before = len(self.commands())
            name = f"01c-stationary-{site}"
            with recording(self.window, self.output, f"{self.surface}-{name}") as events:
                with self.hold(point):
                    if held:
                        lifted = wait("stationary Queue row lift", lambda: self.lifted(), timeout=1)
                        check(len(lifted) == 1 and bounds(lifted[0]) == bounds(start),
                              "wrong native occurrence lifted from the left edge")
                        self.frame(name + "-held")
                        check(len(self.commands()) == before, "stationary lift emitted a mutation")
                self.expect(before)
                check(not self.lifted(), "stationary release left the Queue row lifted")
                self.expect_adjacent(DUPLICATE, 4, occurrence=1)
                self.frame(name + "-released", self.layout())
            pointer_trace(events, point, stationary=True)

    def fixed_current(self):
        before = len(self.commands())
        name = "01b-fixed-current"
        current = self.row(CURRENT)
        point = self.grab_point(current)
        with recording(self.window, self.output, f"{self.surface}-{name}") as events:
            with self.hold(point):
                sleep(.32)
                self.frame(name + "-held")
                check(not self.lifted(), "current queue row lifted under the pointer")
                self.after(self.row(4), point[0])
            self.expect(before)
            self.frame(name + "-settled", self.layout())
        pointer_trace(events, point)
        with recording(self.window, self.output, f"{self.surface}-{name}-keyboard") as events:
            self.window.click_element(current)
            self.window.key(" ")
            check(not self.lifted(), "current queue row lifted from the keyboard")
            self.window.key(" ")
            self.expect(before)
        key_trace(events, " ")

    def pointer_gesture(self, *, cancel=False):
        name = "02-pointer-cancel" if cancel else "03-pointer-duplicate"
        start = self.row(DUPLICATE, 1)
        point = self.grab_point(start)
        p2, p4, p6, p7 = (self.row(i) for i in (2, 4, 6, 7))
        before = len(self.commands())
        with recording(self.window, self.output, f"{self.surface}-{name}") as events:
            with self.hold(point):
                sleep(.055)
                self.frame(name + "-00-press")
                sleep(.28)
                self.frame(name + "-00b-lift")
                lifted = self.lifted()
                check(len(lifted) == 1 and bounds(lifted[0]) == bounds(start), "wrong native occurrence lifted")
                for i, row in enumerate((p6, p4, p6, p2, p7), 1):
                    self.after(row, point[0])
                    self.frame(f"{name}-{i:02d}-cross")
                sleep(.18)
                self.frame(name + "-06-held")
                check([r.get("accessibleDescription") for r in self.lifted()] == ["Lifted, after Fixture row 007"],
                      "the lifted occurrence did not expose the final target")
                self.expect(before)
                self.frame(name + "-06b-held-stationary")
                if cancel:
                    self.window.key(ESCAPE)
                    self.frame(name + "-07-escape-still-held")
                    check(not self.lifted(), "Escape left the pointer drag lifted")
            self.frame(name + "-08-release")
            self.expect(before, None if cancel else self.command(5, 7))
            self.expect_adjacent(DUPLICATE, 4 if cancel else 7, occurrence=1)
            self.frame(name + "-09-settled", self.layout())
        pointer_trace(events, point)
        if cancel:
            key_trace(events, ESCAPE)

    def delayed_echo(self):
        before = len(self.commands())
        start = self.row(DUPLICATE, 1)
        anchor = self.row(7)
        point = self.grab_point(start)
        expected = self.command(5, 7)
        with recording(self.window, self.output, f"{self.surface}-slow-echo") as events:
            with self.hold(point):
                sleep(.32)
                self.after(anchor, point[0])
                sleep(.17)
            wait("exactly one emitted move", lambda: self.commands()[before:] == [expected], timeout=1)
            self.frame("slow-echo-01-release")
            sleep(.53)  # Clear the former 400 ms snap-back threshold.
            self.window.key(" ")  # A pending drop cannot lift/place a second row.
            carried = element_id(self.tree(self.editor), "carried-card")
            check(carried and carried.get("visible", True), "preview snapped back before the delayed echo")
            self.frame("slow-echo-02-pending", {"carried": bounds(carried)})
            check(not (self.output / "readback.jsonl").exists(), "pending frame was not captured before echo")
            self.expect_adjacent(DUPLICATE, 7, occurrence=1)
            check(not element_id(self.tree(self.editor), "carried-card"), "preview survived the echo")
            check(abs(center(carried)[1] - center(self.row(DUPLICATE, 1))[1]) <= 2,
                  "the authoritative occurrence jumped to a different slot")
            self.frame("slow-echo-03-authoritative", self.layout())
            self.expect(before, expected)
        pointer_trace(events, point)
        key_trace(events, " ")

    def keyboard(self):
        before = len(self.commands())
        name = "04-keyboard"
        with recording(self.window, self.output, f"{self.surface}-{name}") as events:
            self.window.click_element(self.row(6))
            self.window.key(" ")
            self.frame(name + "-00-pickup")
            check([number(r) for r in self.lifted()] == [6], "keyboard did not lift the selected occurrence")
            self.window.key(DOWN)
            self.frame(name + "-01-move")
            self.window.key(ESCAPE)
            self.expect(before)
            self.frame(name + "-02-cancel")
            self.window.key(" ")
            self.window.key(DOWN)
            self.window.key(" ")
            self.expect(before, self.command(6, 7))
            self.expect_adjacent(6, 7)
            self.frame(name + "-03-settled", self.layout())
        for key in (" ", DOWN, ESCAPE):
            key_trace(events, key)

    def scroll(self, viewport, delta):
        x, y = center(viewport)
        self.window.mcp.call("dispatch_pointer_scroll", {"windowHandle": self.window.handle,
                             "position": {"x": x, "y": y}, "deltaY": delta})
        sleep(.15)

    def scroll_to_boundary(self):
        for _ in range(16):
            viewport, rows = self.rows()
            check(len(rows) >= 2, "fewer than two visible reorder rows")
            index = next((i for i, row in enumerate(rows) if number(row) == 98), None)
            if index is not None and 1 <= index <= len(rows) // 2:
                return rows[index]
            pitch = (center(rows[-1])[1] - center(rows[0])[1]) / (len(rows) - 1)
            desired_first = 96
            delta = max(-3200, min(3200, (number(rows[0]) - desired_first) * pitch))
            check(delta != 0, "cannot position old playlist boundary in the viewport")
            self.scroll(viewport, delta)
        raise Failure("could not scroll to the old 100-row boundary")

    def edge_gesture(self):
        name = "05-edge"
        start = self.scroll_to_boundary()
        viewport, initially_visible = self.rows()
        # A high-numbered row can already be visible at the initial edge.
        # Require a newly revealed anchor and actual viewport travel.
        minimum_anchor = max(105, max(map(number, initially_visible)) + 2)
        before = len(self.commands())
        point = self.grab_point(start)
        x = point[0]
        self.frame(name + "-00-before", self.layout())
        with recording(self.window, self.output, f"{self.surface}-{name}") as events:
            with self.hold(point):
                sleep(.32)
                self.device.move(x, bounds(viewport)[3] - 3)
                for i in range(30):
                    self.frame(f"{name}-{i + 1:02d}-held")
                    _, visible = self.rows()
                    candidates = [r for r in visible[1:-2] if number(r) >= minimum_anchor]
                    if candidates:
                        anchor = number(candidates[0])
                        break
                    sleep(.10)
                else:
                    raise Failure(f"stationary held pointer did not reveal new row {minimum_anchor:03d}")

                # A tree poll can take several edge timer ticks to return.
                # Park the pointer away from the edge first; only aim at the
                # requested occurrence after its position stops changing.
                self.device.move(x, center(viewport)[1])
                prior = None

                def stationary_rows():
                    nonlocal prior
                    _, rows = self.rows()
                    snapshot = [(number(r), bounds(r)) for r in rows]
                    unchanged = snapshot == prior
                    prior = snapshot
                    return rows if unchanged else None

                visible = wait("edge scrolling to stop", stationary_rows, timeout=3)
                fresh = [r for r in visible[1:-2] if number(r) >= minimum_anchor]
                check(fresh, "no newly revealed row in the stationary viewport interior")
                dest = next((r for r in fresh if number(r) == anchor), fresh[0])
                after = number(dest)
                self.after(dest, x)
                layout = self.layout()
                check(any(r["label"] == dest["accessibleLabel"] and r["box"] == bounds(dest) for r in layout["rows"]),
                      "drop target moved after leaving the edge")
                self.frame(name + "-32-target", layout)
                self.expect(before)
            self.expect(before, self.command(98, after))
            self.expect_adjacent(98, after)
            self.frame(name + "-33-settled", self.layout())
        pointer_trace(events, point)


def number(row):
    return int(ROW.search(row["accessibleLabel"])[1])


def pointer_trace(events, point, *, stationary=False):
    presses = [i for i, e in enumerate(events) if "pointerPressed" in e]
    releases = [i for i, e in enumerate(events) if "pointerReleased" in e]
    check(len(presses) == len(releases) == 1 and presses[0] < releases[0],
          "expected one native pointer press followed by one release")
    position = events[presses[0]]["pointerPressed"]["position"]
    check(all(abs(position[axis] - expected) <= 2 for axis, expected in zip(("x", "y"), point)),
          f"recorded native press missed the target: {position!r} vs {point!r}")
    motion = any("pointerMoved" in e for e in events[presses[0] + 1:releases[0]])
    check(motion != stationary, "unexpected native pointer motion between press and release")


def key_trace(events, key):
    check(any(e.get("keyPressed", {}).get("text") == key for e in events)
          and any(e.get("keyReleased", {}).get("text") == key for e in events), f"keyboard trace missing {key!r}")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("desktop", type=Path, help="prebuilt debug binary with the reorder editor and Slint MCP")
    parser.add_argument("--output", default="reorder-review", help="new directory name under worktree .tmp/")
    parser.add_argument("--delayed-echo", action="store_true", help="only verify slow readback")
    parser.add_argument("--surface", choices=("queue", "playlist"), help="only verify this named surface")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parents[2]
    check(Path.cwd() == repo, "run this script from the desktop worktree root")
    check(re.fullmatch(r"reorder-[a-zA-Z0-9_-]+", args.output), "use a reorder-* output directory name")
    base = repo / ".tmp"
    check(not base.is_symlink(), "refusing a symlinked .tmp directory")
    base.mkdir(mode=0o700, exist_ok=True)
    output = base / args.output
    output.mkdir(mode=0o700)
    binary = args.desktop.resolve(strict=True)
    print(f"Native evidence: {output}", flush=True)
    failures = []
    for variant in (("soft-slot",) if args.delayed_echo else ("soft-slot", "line-cue")):
        for surface in ((args.surface,) if args.surface else ("queue", "playlist")):
            captures = output / variant / surface
            captures.mkdir(mode=0o700, parents=True)
            try:
                with TemporaryDirectory(prefix="r-", dir=base) as tmp:
                    root = Path(tmp)
                    check(len(os.fsencode(root / "runtime" / "wayland-0")) < 108,
                          "worktree path is too long for Cage's Unix socket")
                    env = private_env(root)
                    # Do not use the ambient desktop's Secret Service, MPRIS, or PipeWire.
                    env["DBUS_SESSION_BUS_ADDRESS"] = f"unix:path={root / 'runtime' / 'no-bus'}"
                    if variant == "line-cue":
                        env["SPOFIFY_INSPECT_REORDER_LINE"] = "1"
                    try:
                        delay = 1.9 if args.delayed_echo else READBACK_DELAY
                        with account(captures, delay) as (ready, commands), desktop(binary, root, env, multisize=True) as window:
                            if window.window_properties()["size"]["width"] == WIDE[0]:
                                window.resize_output(WIDE)  # May already be Cage's default mode.
                            else:
                                window.resize(WIDE)
                            check(window.window_properties()["scaleFactor"] == 1, "expected a scale-1 output")
                            window.fill("Server( origin| URL| address)?", ready["origin"])
                            window.fill("Username", ready["username"])
                            window.fill("Password", ready["password"])
                            window.click("Sign in")
                            window.section("History")
                            with pointer(root / "runtime", window, captures) as device:
                                review = Review(window, device, captures, commands)
                                review.open(surface)
                                if args.delayed_echo:
                                    review.delayed_echo()
                                else:
                                    if surface == "queue":
                                        review.fixed_current()
                                        review.stationary_no_write()
                                    review.pointer_gesture(cancel=True)
                                    review.pointer_gesture()
                                    review.keyboard()
                                    review.edge_gesture()
                                window.click_element(review.control("Done"))
                                review.control("Edit order")
                                review.frame("06-done")
                    finally:
                        if (root / "desktop.log").exists():
                            shutil.copyfile(root / "desktop.log", captures / "desktop.log")
            except (RuntimeError, OSError, ValueError, subprocess.SubprocessError) as error:
                failures.append(f"{variant}/{surface}: {error}")
                print(f"FAIL {failures[-1]}", flush=True)
            else:
                print(f"PASS {variant}/{surface}: native events and wire commands", flush=True)
    check(not failures, f"{len(failures)} native scenario(s) failed; evidence in {output}")
    if not args.delayed_echo:
        print("Visual review still required: compare aligned edges, text baselines and grip centers across the two frame sequences.")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError, KeyboardInterrupt) as error:
        sys.exit(f"FAIL: {error or type(error).__name__}")
