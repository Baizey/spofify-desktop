#!/usr/bin/env python3
r"""Actual Desktop quick-add review on private Cage/Slint and synthetic wire.

Build: SLINT_EMIT_DEBUG_INFO=1 cargo build --offline --locked --features slint/mcp
Run with worker-local TCP approvals for localhost:39281 and localhost:39282:
  python3 -B scripts/acceptance/quick_add_concurrency.py --desktop target/debug/spofify-desktop \
    --capture-dir .tmp/search-bulk-add/<new-run>

No Go server or installed state. The synthetic peer independently gates publication
of playlist occurrences; this is not evidence of Go write execution or attribution.
"""

import argparse
from contextlib import contextmanager
from hashlib import file_digest
from pathlib import Path
import re
import shutil
import sys
from tempfile import TemporaryDirectory
from threading import RLock
from time import monotonic, sleep
from unittest.mock import patch

sys.dont_write_bytecode = True
from fixture import Failure, check, private_env
from manual_add import Review, TYPICAL, aligned, capture_directory, login, midpoint
from manual_add_wire import LONG, NORMAL, QUERY, TARGET_ID, Wire, entry
from native import ESCAPE, RETURN, TAB, assert_disjoint, bounds, contains, desktop, wait
from settings import resize
import wire_ui as base

FIRST, SECOND, THIRD = NORMAL[5:8]
PLAN = FIRST, SECOND, THIRD, SECOND  # Final write is the explicitly requested ⋯ repeat.
SHIFT = "\x10"
COMPACT = (480, TYPICAL[1])  # The row (not just the shell) must use its compact branch.
PROBE_SAMPLE = 9  # Optional catch-up is due at 8s; it must not create success.
OBSERVE_SECONDS, AMBIGUITY_BUDGET = 30, 38


class ConcurrentWire(Wire):
    def __init__(self, output, port):
        super().__init__(output, port)
        self.publication_lock = RLock()
        self.peer = None
        self.held = {}
        self.published = set()

    def ingress(self, peer, notice):
        # Never retain arbitrary traffic. Record only an exact planned legacy
        # envelope, and fail extra/reordered writes even if a later UI assertion passes.
        with self.publication_lock:
            ordinal = len(self.held) + 1
            if ordinal > len(PLAN):
                self.record("unexpected-extra-add")
                raise Failure("unexpected extra addEntry")
            track = PLAN[ordinal - 1]
            command = dict(type="addEntry", playlistId=TARGET_ID, trackId=track["id"])
            if notice != dict(type="personal", command=command) or peer is not self.peer:
                self.record("unexpected-add", ordinal=ordinal)
                raise Failure(f"unexpected addEntry #{ordinal}")
            self.held[ordinal] = track
            self.record("add", ordinal=ordinal, command=command)

    def release(self, ordinal):
        with self.publication_lock:
            check(ordinal in self.held and ordinal not in self.published, f"cannot release add #{ordinal}")
            summary, rows, _ = self.playlists[TARGET_ID]
            track = self.held[ordinal]
            added = entry(TARGET_ID, len(rows), track)
            rows.append(added)
            summary.update(trackCount=len(rows), availableCount=len(rows), revision=summary["revision"] + 1)
            self.peer.personal(change=dict(playlist=summary, entries=dict(added=added)))
            self.published.add(ordinal)
            self.record("add-confirmed", ordinal=ordinal, track=track["id"], occurrence=added["id"],
                        revision=summary["revision"])

    @contextmanager
    def serve(self):
        # Keep the existing HTTP/search/auth and playlist-read behavior. Replace
        # just its single-write socket gate at fixture composition, not in app code.
        wire_fixture, read_message = base.wire_fixture, base.AccountWire.message
        wire = self

        @contextmanager
        def fixture_with_concurrent_reader():
            class Account(base.AccountWire):
                def frame(self, opcode, payload):
                    # No interleaving between main-thread occurrence publication
                    # and routine responses to the native client's socket reader.
                    with wire.publication_lock:
                        super().frame(opcode, payload)

                def playlist(self, command):
                    with wire.publication_lock:
                        super().playlist(command)

                def initial(self):
                    with wire.publication_lock:
                        if wire.peer is not None:
                            wire.record("unexpected-new-snapshot")
                            raise Failure("unexpected reconnect or repair")
                        wire.peer = self
                        super().initial()

                def message(self):
                    while True:
                        notice = read_message(self)
                        if notice.get("type") != "personal" or notice.get("command", {}).get("type") != "addEntry":
                            return notice
                        wire.ingress(self, notice)

            with patch.object(base, "AccountWire", Account), wire_fixture() as ready:
                yield ready

        with patch.object(base, "wire_fixture", fixture_with_concurrent_reader), super().serve() as ready:
            yield ready


class ConcurrencyView(Review):
    def quick_control(self, track, state, *, nodes=None):
        nodes = self.nodes() if nodes is None else nodes
        title = track["name"]
        if state in ("ready", "pending"):
            return self.node(f"{'Add' if state == 'ready' else 'Adding'} {title} to {LONG}", "Button", nodes=nodes)
        if state == "member":
            return self.member(track, nodes=nodes)
        check(state == "ambiguous", f"unknown quick-add state {state}")
        return self.node(f"Add unconfirmed for {title} in {LONG}", "Button", nodes=nodes)

    def states(self, expected, slots, *, timeout=4):
        wide = self.window.dimensions()[0] == TYPICAL[0]

        def observed():
            nodes = self.nodes()
            actual = {}
            for track, state in expected:
                control = self.quick_control(track, state, nodes=nodes)
                if control is None:
                    return None
                name = track["name"]
                left, top, right, bottom = slots[name]
                width = 100 if state == "ambiguous" and wide else right - left
                check(bounds(control) == (right - width, top, right, bottom), f"{state} moved the {name} slot")
                check(control.get("accessibleEnabled", False) == (state == "ready"),
                      f"wrong enabled state for {state}: {name}")
                if state != "member":
                    check(not self.member(track, nodes=nodes), f"fabricated membership: {name}")
                actual[name] = control
            return actual
        return wait(", ".join(f"{track['name']}: {state}" for track, state in expected), observed, timeout)

    def ambiguity_ink(self, control):
        # The real 1280px render had no exact #dfbd78 pixels in the glyph:
        # the font raster blends them into the dark background. Threshold its
        # warm channel differences, away from any focus ring and text next door.
        x, y, right, bottom = bounds(control)
        rgb, w, h = self.rgb(self.frame(), (x + 5, y + 5, right - 5, bottom - 5))
        pixels = zip(rgb[0::3], rgb[1::3], rgb[2::3])
        gold = [(i % w, i // w) for i, (r, g, b) in enumerate(pixels)
                if r - b >= 45 and r - g >= 11 and g - b >= 23]
        check(len(gold) > 6, "unconfirmed slot has no visible gold ink")
        gx, gy = zip(*gold)
        wide = self.window.dimensions()[0] == TYPICAL[0]
        check((max(gx) - min(gx) >= 45 if wide else max(gx) - min(gx) <= 12),
              "unconfirmed slot did not paint expected text/glyph footprint")
        aligned((min(gx) + max(gx)) / 2, (w - 1) / 2, "ambiguity ink horizontal center", 5)
        aligned((min(gy) + max(gy)) / 2, (h - 1) / 2, "ambiguity ink vertical center", 5)


def start_scrolled(view):
    view.input_query(QUERY)
    view.popup()
    view.select_playlist_row(LONG)
    wait("selected target's actual seed member", lambda: view.member(NORMAL[4]), 8)
    toolbar = view.layout("toolbar before scroll", LONG)
    view.scroll(-255)
    wait("genuine nonzero Search scroll", lambda: NORMAL[0]["name"] not in view.visible_menus(), 4)
    return toolbar, view.settled_menu_boxes()


def focus_quick_add(view, track):
    # Exercise true keyboard focus through the unchanged adjacent overflow; no
    # direct model writes or accessible-default-action invocations.
    win = view.window
    win.click_element(view.row_menu(track))
    win.find(re.escape("Add to playlist · " + track["name"]))
    win.key(ESCAPE)
    win.shortcut(SHIFT, TAB)
    wait("keyboard focus on quick-add", lambda: view.gold(view.row_add(track)), 2)


def received(wire, ordinal):
    return wait(f"exact synthetic addEntry #{ordinal}", lambda: wire.seen("add", ordinal=ordinal), 2)[0]


def scenario(view, wire):
    win = view.window
    toolbar, boxes = start_scrolled(view)
    slots = {}
    for track in PLAN[:3]:
        control = view.row_add(track)
        more = view.row_menu(track)
        check(contains(view.part("catalog-scroll"), control) and contains(view.part("catalog-scroll"), more),
              f"expected a fully visible scrolled test row: {track['name']}")
        assert_disjoint([("quick add", control), ("overflow", more)])
        slot, overflow = bounds(control), bounds(more)
        aligned(midpoint(slot)[1], midpoint(overflow)[1], "adjacent control centers")
        aligned(midpoint(overflow)[0] - midpoint(slot)[0], 40, "stable overflow slot")
        slots[track["name"]] = slot

    def state(expected, *, timeout=4):
        return view.states(expected, slots, timeout=timeout)

    def captured(label):
        view.park()
        view.expect_query(QUERY)
        check(view.layout(label, LONG) == toolbar, f"sticky toolbar moved in {label}")
        check(view.menu_boxes() == boxes, f"overflow or Search scroll shifted in {label}")
        view.capture(label)

    captured("01-scrolled-ready")
    focus_quick_add(view, FIRST)
    win.key(RETURN)
    first = received(wire, 1)
    pending = state(((FIRST, "pending"), (SECOND, "ready"), (THIRD, "ready")), timeout=2)[FIRST["name"]]
    wait("focused pending quick-add still shows keyboard focus", lambda: view.gold(pending), 2)
    win.key(RETURN)  # A held keyboard repeat must not duplicate the first frame.
    win.click_element(view.row_add(SECOND))
    second = received(wire, 2)
    focus_quick_add(view, THIRD)
    win.key(RETURN)
    third = received(wire, 3)
    check(first["time"] < second["time"] < third["time"] and not wire.published,
          "distinct writes were not all received with the first held")
    state(tuple((track, "pending") for track in PLAN[:3]), timeout=2)
    captured("02-three-pending")

    wire.release(2)
    controls = state(((FIRST, "pending"), (SECOND, "member"), (THIRD, "pending")), timeout=2)
    wait("newest pending row retains focus across an earlier confirmation", lambda: view.gold(controls[THIRD["name"]]), 2)
    captured("03-second-confirmed-first-and-third-held")
    wire.release(3)
    controls = state(((FIRST, "pending"), (SECOND, "member"), (THIRD, "member")), timeout=2)
    wait("newest keyboard focus persists as the row becomes a disabled member",
         lambda: view.gold(controls[THIRD["name"]]), 2)
    captured("04-third-confirmed-first-still-held")
    win.key(RETURN)
    check(len(wire.seen("add")) == 3, "Enter on focused member repeated the write")
    win.key(TAB)
    wait("Tab advances from the disabled focused member to its overflow", lambda: view.gold(view.row_menu(THIRD)), 2)
    captured("04a-focus-moves-to-member-overflow")

    sleep(max(0, first["time"] + PROBE_SAMPLE - monotonic()))
    state(((FIRST, "pending"), (SECOND, "member"), (THIRD, "member")), timeout=2)
    check(len(wire.seen("add")) == 3, "catch-up interval retried addEntry")
    captured("04b-first-pending-after-catch-up-window")
    ambiguous = state(((FIRST, "ambiguous"), (SECOND, "member"), (THIRD, "member")),
                      timeout=max(1, first["time"] + AMBIGUITY_BUDGET - monotonic()))[FIRST["name"]]
    check(monotonic() - first["time"] >= OBSERVE_SECONDS - 1, "ambiguity published before the observation deadline")
    captured("05-first-unconfirmed")
    view.ambiguity_ink(ambiguous)
    check(not view.node("Couldn’t verify that the song is in the playlist."), "unexpected page-wide write banner")
    win.absent(r"Retry.*", roles=("Button",))
    assert_disjoint([("unconfirmed", ambiguous), ("overflow", view.row_menu(FIRST))])
    win.click_element(ambiguous)
    # The unresolved slot and both confirmed members stay disabled and
    # stationary even when clicked; only their separate overflow can repeat.
    for track in (SECOND, THIRD):
        win.click_element(view.member(track))
    state(((FIRST, "ambiguous"), (SECOND, "member"), (THIRD, "member")), timeout=2)
    view.expect_query(QUERY)
    check(view.menu_boxes() == boxes, "clicking a disabled slot moved Search")
    wire.release(1)
    wait("late authoritative first occurrence becomes membership", lambda: view.member(FIRST), 5)
    view.expect_query(QUERY)
    view.capture("07-first-late-membership")
    check(len(wire.seen("add")) == 3, "unexpected retransmission before intentional repeat")

    win.click_element(view.row_menu(SECOND))
    win.click(re.escape("Add to playlist · " + SECOND["name"]))
    win.title("Add to playlist")
    view.capture("08-explicit-repeat-picker")
    win.click_element(view.require("Add to " + LONG, "Button"))
    received(wire, 4)
    wire.release(4)
    win.title(LONG)
    original = wire.seen("add-confirmed", ordinal=2)[0]
    repeat = wire.seen("add-confirmed", ordinal=4)[0]
    check(original["track"] == repeat["track"] and original["occurrence"] != repeat["occurrence"],
          "explicit repeat failed to create a separate occurrence")
    view.capture("09-explicit-repeat-occurrence")
    sleep(.4)
    check(len(wire.seen("add")) == 4 and wire.published == {1, 2, 3, 4}, "wrong write/occurrence count")
    print("PASS distinct exact writes before first release; independent membership, row ambiguity, "
          "no repeat, stable slots/query/scroll and unchanged explicit ⋯ repeat", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--desktop", type=Path, required=True)
    parser.add_argument("--capture-dir", type=Path, required=True)
    parser.add_argument("--wire-port", type=int, default=39281)
    parser.add_argument("--mcp-port", type=int, default=39282)
    parser.add_argument("--compact", action="store_true", help="run at 480px and verify the ? glyph")
    parser.add_argument("--before", action="store_true", help="compare against the pre-change member accessibility name")
    args = parser.parse_args()
    check(all(1024 <= port <= 65535 for port in (args.wire_port, args.mcp_port)) and args.wire_port != args.mcp_port,
          "require two distinct unprivileged worker-local ports")
    binary = args.desktop.resolve(strict=True)
    magick = shutil.which("magick") or shutil.which("convert")
    check(magick, "ImageMagick required for native focus pixels")
    output = capture_directory(args.capture_dir)
    with binary.open("rb") as source:
        (output / "binary.sha256").write_text(file_digest(source, "sha256").hexdigest() + "\n")
    wire = ConcurrentWire(output, args.wire_port)
    with TemporaryDirectory(prefix="qac.", dir="/tmp") as temporary:
        root = Path(temporary)
        env = private_env(root)
        no_bus = "unix:path=" + env["XDG_RUNTIME_DIR"] + "/no-bus"
        env.update(DBUS_SESSION_BUS_ADDRESS=no_bus, DBUS_SYSTEM_BUS_ADDRESS=no_bus)
        with wire.serve() as ready, desktop(binary, root, env, multisize=True, mcp_port=args.mcp_port) as window:
            wire.preflight()
            resize(window, COMPACT if args.compact else TYPICAL)
            check(window.window_properties()["scaleFactor"] == 1, "require scale-1 native geometry")
            login(window, ready)
            wait("isolated synthetic account snapshot", lambda: wire.seen("snapshot"))
            view = ConcurrencyView(window, output, magick, before=args.before)
            try:
                scenario(view, wire)
            except (RuntimeError, OSError, ValueError):
                try:
                    view.capture("failure-last-visible-state")
                except (RuntimeError, OSError, ValueError) as error:
                    print(f"Could not capture failure state: {error}", file=sys.stderr)
                raise
    print(f"PASS native quick-add concurrency fixture: {output}", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError) as error:
        sys.exit(f"FAIL: {error}")
