#!/usr/bin/env python3
r"""Native Search bulk-add regression against an isolated synthetic wire peer.

Build with SLINT_EMIT_DEBUG_INFO=1 and --features slint/mcp. Run with
worker-local TCP approvals for localhost:39281 (fixture) and :39282 (Slint):
  python3 -B scripts/acceptance/search_bulk_add.py --desktop /path/to/debug/spofify-desktop \
    --capture-dir .tmp/search-bulk-add/bulk-native

No installed app, Go instance, real account, or media. JSONL traces contain
only synthetic IDs, exact legacy write envelopes, and fixture publications.
"""
import argparse
from hashlib import file_digest
from pathlib import Path
import re
import shutil
import sys
from tempfile import TemporaryDirectory
from time import sleep

sys.dont_write_bytecode = True
from dock_device_picker import CLOSE, open_panel, selected_row, trigger
from fixture import Failure, check, private_env
from manual_add import Review, TYPICAL, aligned, capture_directory, login, midpoint
from manual_add_wire import LONG, TARGET_ID
from native import ESCAPE, RETURN, TAB, assert_disjoint, bounds, contains, desktop, wait
from search_bulk_add_wire import (ALBUM, ALBUM_TRACKS, ARTIST, ARTIST_ONLY, DIRECT, FOREIGN, QUERY,
                                  RESULTS, SEED, TARGET_LENGTH, WINDOW, BulkWire)
from settings import resize

CANCEL = "Cancel bulk add"
STARTING = "Preparing tracks…"
SHIFT = "\x10"


class BulkView(Review):
    def entity_add(self, entity, *, pending=False):
        verb = "Adding" if pending else "Add"
        subject = "album " if entity["kind"] == "album" else "songs by "
        return self.require(f"{verb} {subject}{entity['name']} to {LONG}", "Button")

    def position(self, entity):
        action, menu = self.entity_add(entity), self.row_menu(entity)
        viewport = self.part("catalog-scroll")
        check(contains(viewport, action) and contains(viewport, menu), "source actions are clipped")
        assert_disjoint([("collection add", action), ("overflow", menu)])
        box, more = bounds(action), bounds(menu)
        aligned(midpoint(box)[1], midpoint(more)[1], "collection add / overflow centers")
        aligned(midpoint(more)[0] - midpoint(box)[0], 40, "collection / overflow slot spacing")
        return box

    def progress_nodes(self):
        panel = next((n for n in self.nodes() if any(t.get("typeName") == "SearchAddProgress"
                      for t in n.get("typeNamesAndIds", ()))), None)
        return self.window.subtree(panel) if panel else []

    def progress(self, label):
        def visible():
            nodes = self.progress_nodes()
            text = self.node(label, "Text", nodes=nodes)
            return (nodes, text) if text else None

        nodes, control = wait(label, visible, 20)
        panel = nodes[0]
        check(contains(panel, control), "bulk progress text is outside its panel")
        cancel = self.node(CANCEL, "Button", nodes=nodes)
        if cancel is not None and cancel.get("accessibleEnabled", False):
            check(contains(panel, cancel), "bulk cancel button is outside progress")
            assert_disjoint([("progress", control), ("cancel", cancel)])
            aligned(midpoint(bounds(control))[1], midpoint(bounds(cancel))[1], "bulk progress / Cancel centers")
        self.record("bulk progress", text=label, panel=bounds(panel), status=bounds(control),
                    cancel=bounds(cancel) if cancel else None)
        return bounds(panel)

    def bulk_skip(self, skip):
        before = self.settled_menu_boxes()
        self.popup(opened_from=LONG)
        self.set_skip(skip)
        self.park()
        self.capture(f"skip-{'on' if skip else 'off'}")
        self.window.key(ESCAPE)
        check(self.settled_menu_boxes() == before and self.member(SEED), "skip setting changed Search rows")

    def start(self, entity, *, keyboard=False):
        slot = self.position(entity)
        control = self.entity_add(entity)
        check(control.get("accessibleEnabled"), "expected enabled collection action")
        if keyboard:
            self.window.click_element(self.row_menu(entity))
            self.require("Hide actions for " + entity["name"], "Button")
            self.window.key(ESCAPE)
            self.window.shortcut(SHIFT, TAB)
            wait("source action shows keyboard focus", lambda: self.gold(self.entity_add(entity)), 3)
            self.window.key(RETURN)
        else:
            self.window.click_element(control)
        return slot


def no_new_writes(wire, action, expected):
    # More than two UI frames, while the fixture has sent the real player
    # snapshot (and in a separate test the private Initial) but not all proof.
    sleep(.35)
    check(len(wire.seen("add", serial=action.serial)) == expected and not wire.seen("violation"),
          "unexpected additional write crossed a held boundary")


def checkpoint(wire, action, ordinal):
    row = wait(f"writer checkpoint #{ordinal}", lambda: wire.seen("sync", serial=action.serial, window=ordinal), 8)[0]
    check(row["writes"] == WINDOW, "large collection checkpoint came before a full window")
    wait("player snapshot sent before private Initial",
         lambda: wire.seen("player-snapshot-only", serial=action.serial, window=ordinal), 3)


def no_retry(view):
    view.window.absent(r"Retry.*", roles=("Button",))
    cancel = view.node(CANCEL, "Button", nodes=view.progress_nodes())
    check(cancel is None or not cancel.get("accessibleEnabled", False), "completed operation still offers cancel")


def finished(view, wire, action, text, capture):
    view.progress(text)
    wait("non-playing writer retired", lambda: wire.writer is None, 6)
    check(wire.main is not None and len(wire.seen("socket-open", role="main")) == 1,
          "bulk write replaced or disconnected the original account socket")
    view.expect_query(QUERY)
    view.accepted_results(len(RESULTS))
    view.no_pagers()
    no_retry(view)
    view.park()
    view.capture(capture)
    wire.finish(action)


def success(view, wire):
    toolbar = view.layout("bulk search before start", LONG)
    view.bulk_skip(False)
    album = wire.arm(ALBUM, skip=False, gate_terminal=True, hold=(1,), hold_primary=(2,))
    scroll_past_album = view.viewport()[1] - bounds(view.row_menu(ALBUM))[3] - 4
    slot = view.start(ALBUM, keyboard=True)
    wait("terminal album page reached before edits",
         lambda: wire.seen("manifest-read", serial=album.serial, terminal=True), 10)
    check(len(wire.seen("manifest-read", serial=album.serial)) > WINDOW, "album was not substantially multi-page")
    pending = view.entity_add(ALBUM, pending=True)
    check(not pending.get("accessibleEnabled", False) and bounds(pending) == slot, "album pending shifted its slot")
    wait("pending album action retains keyboard focus", lambda: view.gold(view.entity_add(ALBUM, pending=True)), 3)
    view.progress(STARTING)
    view.park()
    view.capture("album-01-terminal-page-held")
    view.window.key(RETURN)  # Must not queue an overlapping source operation.
    no_new_writes(wire, album, 0)
    album.terminal.set()
    checkpoint(wire, album, 1)
    no_new_writes(wire, album, WINDOW)
    check(not wire.seen("private-initial", serial=album.serial, window=1), "private Initial was not held")
    view.progress(STARTING)
    view.capture("album-02-player-snapshot-not-a-checkpoint")
    check(wire.seen("roster", can_play=[True, False], owner="main"),
          "fixture did not deliver both connections and retain main ownership")
    name = wire.main.device["name"]
    view.window.click_element(trigger(view.window, name))
    panel = open_panel(view.window)
    selected_row(view.window, name)
    devices = view.window.subtree(panel)
    check(sum(e.get("accessibleLabel") == name for e in devices) == 1
          and not any((e.get("accessibleLabel") or "").startswith("Select output: ") for e in devices),
          "the auxiliary connection appeared as a duplicate playback device")
    view.park()
    view.capture("album-02b-one-playback-device-with-writer-connected")
    view.window.click(CLOSE)
    wire.release(album)

    checkpoint(wire, album, 2)
    wait("private Initial sent while main revision held",
         lambda: wire.seen("private-initial", serial=album.serial, window=2), 3)
    check(album.deferred_primary is not None, "main target revision was not held")
    no_new_writes(wire, album, 2 * WINDOW)
    # The primary-revision wait is longer than the writer's I/O timeout. Use
    # that boundary for slower live UI interactions, not the pending Initial.
    # The original account and scrolling remain live during the held writer.
    # Concurrent quick intents on the target are deliberately suppressed;
    # both direct and collection slots must become usable again after finish.
    view.scroll(scroll_past_album)
    wait("live scroll moved the first result offscreen without moving the toolbar",
         lambda: ALBUM["name"] not in view.visible_menus() and DIRECT["name"] in view.visible_menus(), 5)
    check(view.layout("bulk held while scrolled", LONG) == toolbar, "bulk overlay moved sticky toolbar")
    at_scroll = view.settled_menu_boxes()
    for subject in (DIRECT["name"], "songs by " + ARTIST["name"]):
        blocked = view.require(f"Quick add unavailable: {subject} to {LONG}", "Button")
        check(not blocked.get("accessibleEnabled", False) and contains(view.part("catalog-scroll"), blocked),
              "overlapping quick action was not present and disabled")
        view.window.click_element(blocked)
    view.park()
    view.capture("album-03-scrolled-held-revision-overlapping-actions-disabled")
    no_new_writes(wire, album, 2 * WINDOW)
    check(not wire.seen("direct-add"), "held album allowed an overlapping direct quick write")
    wire.release_primary(album)
    finished(view, wire, album, f"{len(ALBUM_TRACKS)} sent", "album-05-skip-off-complete")
    wire.expect_direct(DIRECT)
    view.window.click_element(view.row_add(DIRECT))
    wait("direct write used original main socket and exact target", lambda: wire.seen("direct-add"), 4)
    wait("direct occurrence visible as persistent member", lambda: view.member(DIRECT), 5)
    check(view.menu_boxes() == at_scroll and view.member(SEED), "confirmations removed or moved a scrolled result")
    view.park()
    view.capture("album-06-direct-quick-add-usable-after-bulk")
    rows = wire.playlists[TARGET_ID][1]
    check(sum(e["track"]["id"] == SEED["id"] for e in rows) == 2,
          "skip off did not append a new occurrence for a member beyond the first target part")
    view.scroll(2000)
    wait("Search scroll returns the album and artist", lambda: ARTIST["name"] in view.visible_menus(), 4)
    view.bulk_skip(True)
    artist = wire.arm(ARTIST, skip=True, exclude=(FOREIGN,), hold=(1,))
    view.start(ARTIST)
    checkpoint(wire, artist, 1)
    # This exact recording was absent at operation start but is independently
    # appended before its later window. DIRECT and ARTIST_ONLY[1] share an
    # exact display name, but distinct IDs; only DIRECT was already present.
    wire.foreign(FOREIGN)
    wait("foreign occurrence visibly reaches the primary target", lambda: view.member(FOREIGN), 3)
    view.park()
    view.capture("artist-01-external-membership-while-checkpoint-held")
    no_new_writes(wire, artist, WINDOW)
    wire.release(artist)
    count, skipped = len(artist.expected), len(artist.source) - len(artist.expected)
    finished(view, wire, artist, f"{count} sent · {skipped} skipped", "artist-02-skip-on-complete")
    check(len(wire.seen("manifest-read", serial=artist.serial)) > len(wire.seen("manifest-read", serial=album.serial)),
          "artist never traversed the full, larger source")
    ids = [e["track"]["id"] for e in wire.playlists[TARGET_ID][1]]
    check(ids.count(DIRECT["id"]) == ids.count(FOREIGN["id"]) == ids.count(ARTIST_ONLY[1]["id"]) == 1,
          "skip-on occurrence or same-title distinct ID count differed from the independent oracle")
    print("PASS large album and artist: full manifest before exact writes, bounded windows and both fences, "
          "skip off/on, live scrolling, no duplicate intents, main-socket direct add and foreign overlap", flush=True)

    tail = RESULTS[-1]
    view.scroll(-2000)
    menu = wait("last Search row reachable below bulk status", lambda: view.visible_menus().get(tail["name"]), 5)
    member = view.member(tail)
    check(member and contains(view.part("catalog-scroll"), member), "tail member is clipped or missing")
    panel = view.progress_nodes()[0]
    view.park()
    view.capture("artist-03-scroll-end-with-status")
    assert_disjoint([("bulk status", panel), ("last Search row's overflow", menu), ("last Search member", member)])
    view.scroll(2000)
    wait("return to top after checking the tail", lambda: ALBUM["name"] in view.visible_menus(), 4)


def safety(view, wire):
    # No need to flip the setting after success; this safety sequence is also
    # runnable by itself in a fresh account without the optimistic mutations.
    view.popup(opened_from=LONG)
    view.set_skip(False)
    view.window.key(ESCAPE)

    duplicate = wire.arm(ALBUM, skip=False, outcome="duplicate")
    view.start(ALBUM)
    view.progress("Couldn’t read the whole collection")
    check(len(wire.seen("manifest-read", serial=duplicate.serial)) >= 2, "duplicate was not reached across pages")
    no_new_writes(wire, duplicate, 0)
    no_retry(view)
    view.park()
    view.capture("safety-01-duplicate-rejected-before-writes")
    wire.finish(duplicate)

    cancel = wire.arm(ARTIST, skip=False, gate_terminal=True, outcome="cancel")
    view.start(ARTIST)
    wait("artist final response held during cancellation", lambda: wire.seen("manifest-read", serial=cancel.serial, terminal=True), 10)
    view.progress(STARTING)
    button = view.node(CANCEL, "Button", nodes=view.progress_nodes())
    check(button and button.get("accessibleEnabled"), "active bulk operation has no usable Cancel action")
    view.window.click_element(button)
    view.progress("Add cancelled")
    cancel.terminal.set()
    no_new_writes(wire, cancel, 0)
    no_retry(view)
    view.park()
    view.capture("safety-02-cancel-before-any-write")
    wire.finish(cancel)

    ambiguous = wire.arm(ALBUM, skip=False, hold=(2,), outcome="ambiguous")
    view.start(ALBUM)
    checkpoint(wire, ambiguous, 2)
    no_new_writes(wire, ambiguous, 2 * WINDOW)
    view.progress(f"Sent {WINDOW}…")
    view.park()
    view.capture("safety-03-second-window-started")
    wire.disconnect_writer(ambiguous)
    view.progress(f"{WINDOW} sent · {WINDOW} unconfirmed")
    wait("writer closed instead of replaying", lambda: wire.writer is None, 5)
    no_new_writes(wire, ambiguous, 2 * WINDOW)
    no_retry(view)
    view.expect_query(QUERY)
    view.accepted_results(len(RESULTS))
    check(view.member(SEED) and wire.main is not None, "writer loss cleared Search or retired main account")
    view.capture("safety-04-exactly-one-unconfirmed-window")
    wire.finish(ambiguous, complete=False)
    print("PASS malformed canonical duplicate, pre-write cancel, and ambiguous window without replay", flush=True)


def primary_disconnect(view, wire):
    # Run on a fresh account. An unresolved write in the previous case must
    # block starting another bulk intent; don't clear it to set up this test.
    view.popup(opened_from=LONG)
    view.set_skip(False)
    view.window.key(ESCAPE)
    primary_loss = wire.arm(ARTIST, skip=False, hold=(1,), outcome="ambiguous")
    view.start(ARTIST)
    checkpoint(wire, primary_loss, 1)
    old_main, old_writer = wire.main, wire.writer
    wire.disconnect_primary(primary_loss)
    wait("primary closes while writer is still held", lambda: wire.main is not old_main, 3)
    wait("primary loss promptly retires the non-playing writer", lambda: wire.writer is not old_writer, 2.5)
    no_new_writes(wire, primary_loss, WINDOW)
    check(not wire.seen("private-initial", serial=primary_loss.serial), "connection loss falsely checkpointed")
    view.expect_query(QUERY)
    view.park()
    view.capture("safety-05-primary-loss-retires-writer")
    wire.finish(primary_loss, complete=False)
    print("PASS primary loss retires private writer without checkpoint or replay", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--desktop", type=Path, required=True)
    parser.add_argument("--capture-dir", type=Path, required=True)
    parser.add_argument("--wire-port", type=int, default=39281)
    parser.add_argument("--mcp-port", type=int, default=39282)
    parser.add_argument("--width", type=int, choices=(480, 640, 1280), default=TYPICAL[0],
                        help="native client width; use 480 to exercise the narrow status lane")
    parser.add_argument("--case", choices=("all", "success", "safety", "primary-loss"), default="all",
                        help="all runs success and safety; primary-loss requires a separate disposable account")
    parser.add_argument("--legacy-member-name", action="store_true", help="capture an interim binary with the old target-inclusive member name")
    args = parser.parse_args()
    check(1024 <= args.wire_port <= 65535 and 1024 <= args.mcp_port <= 65535 and args.wire_port != args.mcp_port,
          "specify two distinct unprivileged worker-local ports")
    binary = args.desktop.resolve(strict=True)
    magick = shutil.which("magick") or shutil.which("convert")
    check(magick, "ImageMagick required for native focus pixels")
    output = capture_directory(args.capture_dir)
    with binary.open("rb") as file:
        (output / "binary.sha256").write_text(file_digest(file, "sha256").hexdigest() + "\n")
    wire = BulkWire(output, args.wire_port)
    with TemporaryDirectory(prefix="sba.", dir="/tmp") as temporary:
        root = Path(temporary)
        env = private_env(root)
        no_bus = "unix:path=" + env["XDG_RUNTIME_DIR"] + "/no-bus"
        env.update(DBUS_SESSION_BUS_ADDRESS=no_bus, DBUS_SYSTEM_BUS_ADDRESS=no_bus)
        try:
            with wire.serve() as ready, desktop(binary, root, env, multisize=True, mcp_port=args.mcp_port) as window:
                wire.preflight()
                resize(window, (args.width, TYPICAL[1]))
                check(window.window_properties()["scaleFactor"] == 1, "require scale-1 native geometry")
                login(window, ready)
                wait("dedicated synthetic account on main socket", lambda: wire.seen("socket-open", role="main"))
                view = BulkView(window, output, magick, legacy_member_name=args.legacy_member_name)
                try:
                    view.input_query(QUERY)
                    view.popup()
                    view.select_playlist_row(LONG)
                    wait("target member from the last of many multipart rows", lambda: view.member(SEED), 10)
                    final_part = (TARGET_LENGTH - 1) // 64 * 64
                    check(wire.seen("target-read", playlist=TARGET_ID, offset=final_part, role="main"),
                          "main socket did not reach the late seed occurrence")
                    view.accepted_results(len(RESULTS))
                    view.park()
                    view.capture("00-long-target-mixed-search")
                    if args.case in ("all", "success"):
                        success(view, wire)
                    if args.case in ("all", "safety"):
                        safety(view, wire)
                    if args.case == "primary-loss":
                        primary_disconnect(view, wire)
                except (RuntimeError, OSError, ValueError):
                    try:
                        view.capture("failure-last-visible-state")
                    except (RuntimeError, OSError, ValueError) as error:
                        print(f"Could not capture failure state: {error}", file=sys.stderr)
                    raise
        finally:
            log = root / "desktop.log"
            if log.exists():
                shutil.copyfile(log, output / "desktop.log")
    print(f"PASS native Search bulk-add fixture: {output}", flush=True)


if __name__ == "__main__":
    try:
        main()
    except (Failure, RuntimeError, OSError, ValueError) as error:
        sys.exit(f"FAIL: {error}")
